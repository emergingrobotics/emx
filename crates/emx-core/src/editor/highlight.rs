//! Syntax highlighting of the bundled languages (DESIGN sections 5 and
//! 7): which language a buffer is in, asking a worker for the faces of the
//! text windows show, and taking the answers that are still current.
//! Between answers the faces move with edits, so typing does not flicker.

use std::ops::{ControlFlow, Range};

use super::Editor;
use crate::buffer::BufferId;
use crate::event::{ClientId, Effect};
use crate::syntax::{HighlightJob, Language};
use crate::window::{Fetch, WindowId};
use emx_types::Face;

/// How far beyond the text a window shows highlighting reaches, so a
/// small scroll needs no new request.
pub(crate) const HIGHLIGHT_MARGIN_BYTES: u64 = 64 * 1024;
/// How much of a file's start is read for a `#!` line.
const SHEBANG_BYTES: u64 = 256;

/// What a buffer's highlighting covers and what is on its way.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct HighlightState {
    /// The change the ranges in `done` were highlighted at.
    pub generation: u64,
    pub done: Vec<Range<u64>>,
    pub in_flight: Vec<(u64, Range<u64>)>,
}

impl HighlightState {
    fn covers(&self, generation: u64, wanted: &Range<u64>) -> bool {
        let contains = |range: &Range<u64>| range.start <= wanted.start && wanted.end <= range.end;
        (self.generation == generation && self.done.iter().any(contains))
            || self
                .in_flight
                .iter()
                .any(|(asked, range)| *asked == generation && contains(range))
    }
}

/// The language a buffer is in, and for which file that was decided.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct LanguageState {
    pub language: Option<Language>,
    /// The file the language was decided for; `None` while undecided.
    pub decided_for: Option<Option<std::path::PathBuf>>,
}

impl Editor {
    /// Asks for the faces of what every window shows that is not
    /// highlighted for the current text yet.
    pub(super) fn request_highlights(&mut self) {
        let shown: Vec<(ClientId, WindowId)> = self
            .clients
            .iter()
            .flat_map(|(client, state)| state.frame.windows.keys().map(|window| (*client, window)))
            .collect();
        for (client, window) in shown {
            self.highlight_window(client, window);
        }
    }

    fn highlight_window(&mut self, client: ClientId, window: WindowId) {
        let buffer_id = self.window(client, window).buffer;
        self.decide_language(buffer_id);
        let max_file = self.config.highlight_max_file;
        let max_line = self.config.highlight_max_line;
        let buffer = &self.buffers[buffer_id];
        let Some(language) = buffer.language.language else {
            return;
        };
        if buffer.text.len() > max_file {
            return;
        }
        let Some(visible) = self.visible_range(client, window) else {
            return;
        };
        let generation = buffer.modified_tick();
        if buffer.highlight.covers(generation, &visible) {
            return;
        }
        let length = buffer.text.len();
        let range = visible.start.saturating_sub(HIGHLIGHT_MARGIN_BYTES)
            ..visible
                .end
                .saturating_add(HIGHLIGHT_MARGIN_BYTES)
                .min(length);
        let job = HighlightJob {
            buffer: buffer_id,
            language,
            generation,
            text: buffer.text.clone(),
            range: range.clone(),
            max_line,
        };
        self.buffers[buffer_id]
            .highlight
            .in_flight
            .push((generation, range));
        self.effects.push(Effect::Highlight(job));
    }

    /// The buffer positions a window shows now, from cached text only.
    fn visible_range(&self, client: ClientId, window_id: WindowId) -> Option<Range<u64>> {
        let window = self.window(client, window_id);
        let buffer = &self.buffers[window.buffer];
        let layout = self.window_layout(client, window_id, &buffer.text, Fetch::CachedOnly);
        let start = layout
            .resolve_start(
                self.window_start(client, window_id),
                &buffer.checkpoints,
                &buffer.text,
            )
            .ok()?;
        let (_, rows) = self.window_size(client, window_id);
        let walk = layout
            .walk_window(start, rows, &mut |_, _| ControlFlow::Continue(()))
            .ok()?;
        Some(start.line_start..walk.end.min(buffer.text.len()))
    }

    /// Decides the buffer's language from its file's name, or its `#!`
    /// line, once per file it visits; a change of language drops the old
    /// faces.
    fn decide_language(&mut self, buffer_id: BufferId) {
        let buffer = &self.buffers[buffer_id];
        if buffer.tree.is_some() || buffer.loading {
            return;
        }
        let path = buffer.file.as_ref().map(|file| file.path.clone());
        if buffer.language.decided_for.as_ref() == Some(&path) {
            return;
        }
        let language = match &path {
            None => None,
            Some(path) => Language::from_path(path).or_else(|| {
                let mut start = Vec::new();
                let end = buffer.text.len().min(SHEBANG_BYTES);
                buffer.text.read_cached(0..end, &mut start).ok()?;
                let first_line = start.split(|byte| *byte == b'\n').next()?;
                Language::from_shebang(first_line)
            }),
        };
        let settings = self.config.file_settings(path.as_deref());
        let checkpoints =
            (settings.tab_width != self.buffers[buffer_id].settings.tab_width).then(|| {
                emx_text::ColumnCheckpoints::new(
                    usize::try_from(settings.tab_width).expect("tab_width is validated small"),
                    self.config.column_checkpoint_interval,
                    self.config.long_line_threshold,
                )
            });
        let target = &mut self.buffers[buffer_id];
        target.settings = settings;
        if let Some(checkpoints) = checkpoints {
            target.checkpoints = checkpoints;
        }
        if target.language.language != language {
            target.faces.clear();
            target.highlight = HighlightState::default();
        }
        target.language = LanguageState {
            language,
            decided_for: Some(path),
        };
    }

    pub(super) fn highlighted(
        &mut self,
        buffer_id: BufferId,
        generation: u64,
        range: Range<u64>,
        spans: Vec<(Range<u64>, Face)>,
    ) {
        let Some(buffer) = self.buffers.get_mut(buffer_id) else {
            return;
        };
        // The worker drops jobs a newer one for the buffer supersedes, so
        // an answer settles every older request too.
        buffer.highlight.in_flight.retain(|(asked, asked_range)| {
            *asked > generation || (*asked == generation && *asked_range != range)
        });
        if buffer.language.language.is_none() || generation != buffer.modified_tick() {
            return;
        }
        if buffer.highlight.generation != generation {
            buffer.highlight.generation = generation;
            buffer.highlight.done.clear();
        }
        buffer.faces.replace_range(range.clone(), spans);
        buffer.highlight.done.push(range);
    }
}
