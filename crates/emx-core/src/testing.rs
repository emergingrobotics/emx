//! Headless harness: one attached client driven through `Editor::handle`,
//! with effects processed synchronously. Timers fire only when a test asks.

use std::collections::VecDeque;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use emx_proto::{ClientKind, ServerMsg};
use emx_text::{Sources, Text};
use emx_types::{Grid, Key, KeyCode, Mods, parse_kbd};

use crate::autosave;
use crate::buffer::{Buffer, BufferId};
use crate::config::Config;
use crate::editor::{Editor, Session};
use crate::event::{ClientId, Effect, Event, TimerId};
use crate::io::{self, SaveFailure, SavePlan, SaveSink, SinkFactory};
use crate::marker::InsertionType;
use crate::render::{PrefetchRequest, render};

pub const HARNESS_COLS: u16 = 80;
pub const HARNESS_ROWS: u16 = 24;
const HARNESS_CLIENT: ClientId = ClientId(1);
const POINT_MARK: char = '|';
const MARK_MARK: char = '^';

/// A sink that writes `remaining` bytes, then fails with ENOSPC, as a full
/// disk does part way through a save.
pub struct FailAfter {
    file: File,
    remaining: u64,
}

impl SaveSink for FailAfter {
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        let allowed = usize::try_from(self.remaining)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        std::io::Write::write_all(&mut self.file, &buf[..allowed])?;
        self.remaining -= allowed as u64;
        if allowed < buf.len() {
            return Err(std::io::Error::from_raw_os_error(
                rustix::io::Errno::NOSPC.raw_os_error(),
            ));
        }
        Ok(())
    }

    fn sync(&mut self) -> std::io::Result<()> {
        self.file.sync_all()
    }

    fn seek(&mut self, offset: u64) -> std::io::Result<()> {
        std::io::Seek::seek(&mut self.file, std::io::SeekFrom::Start(offset)).map(|_| ())
    }
}

/// Saves write `bytes` bytes, then fail with ENOSPC.
pub fn fail_after(bytes: u64) -> Box<SinkFactory> {
    Box::new(move |file| -> Box<dyn SaveSink> {
        Box::new(FailAfter {
            file,
            remaining: bytes,
        })
    })
}

struct Sinks(Box<SinkFactory>);

impl std::fmt::Debug for Sinks {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Sinks")
    }
}

#[derive(Debug)]
pub struct Harness {
    editor: Editor,
    client: ClientId,
    sent: Vec<ServerMsg>,
    timers: Vec<TimerId>,
    prefetches: Vec<PrefetchRequest>,
    shutdown: bool,
    index_requests: Vec<BufferId>,
    hold_saves: bool,
    held_saves: Vec<(BufferId, SavePlan)>,
    sinks: Option<Sinks>,
    /// Auto-save files that could not be deleted.
    io_errors: Vec<String>,
}

impl Default for Harness {
    fn default() -> Self {
        Harness::new()
    }
}

impl Harness {
    /// An 80x24 TUI client looking at an empty `*scratch*`.
    pub fn new() -> Harness {
        Harness::with_config(Config::default())
    }

    /// A `--no-server` editor. Panics when the config's key overrides are
    /// invalid.
    pub fn with_config(config: Config) -> Harness {
        Harness::with_session(config, Session::NoServer)
    }

    /// Panics when the config's key overrides are invalid.
    pub fn with_session(config: Config, session: Session) -> Harness {
        let editor = Editor::with_session(config, session).expect("test config is valid");
        let mut harness = Harness {
            editor,
            client: HARNESS_CLIENT,
            sent: Vec::new(),
            timers: Vec::new(),
            prefetches: Vec::new(),
            shutdown: false,
            index_requests: Vec::new(),
            hold_saves: false,
            held_saves: Vec::new(),
            sinks: None,
            io_errors: Vec::new(),
        };
        harness.handle(Event::ClientAttached {
            client: HARNESS_CLIENT,
            cols: HARNESS_COLS,
            rows: HARNESS_ROWS,
            kind: ClientKind::Tui,
        });
        harness
    }

    /// `text` marks point with `|` and optionally the mark with `^`.
    pub fn with_text(text: &str) -> Harness {
        let mut harness = Harness::new();
        harness.set_text(text);
        harness
    }

    /// Replaces the fresh `*scratch*` text, unmodified and with an empty undo
    /// list, as if the buffer had been opened with it. Panics unless `text`
    /// has exactly one `|` and at most one `^`, or if anything was typed.
    pub fn set_text(&mut self, text: &str) {
        let mut content = String::with_capacity(text.len());
        let mut point = None;
        let mut mark = None;
        for character in text.chars() {
            let position = Some(content.len() as u64);
            match character {
                POINT_MARK => {
                    assert!(point.is_none(), "one | marks point");
                    point = position;
                }
                MARK_MARK => {
                    assert!(mark.is_none(), "at most one ^ marks the mark");
                    mark = position;
                }
                other => content.push(other),
            }
        }
        let point = point.expect("text has a | marking point");
        let buffer = self.buffer_mut();
        assert!(
            buffer.text.is_empty() && buffer.undo.entries().is_empty(),
            "set_text runs on a fresh harness"
        );
        buffer.text = Text::from_original(Sources::memory(Arc::from(content.as_bytes())));
        if let Some(mark) = mark {
            buffer.mark = Some(buffer.create_marker(mark, InsertionType::StayBefore));
        }
        let window = self.editor.selected_window_id(self.client);
        self.editor.set_window_point(self.client, window, point);
        self.buffer_mut().undo.boundary(point);
    }

    /// Feeds an Emacs `kbd` sequence key by key.
    pub fn keys(&mut self, kbd: &str) {
        for key in parse_kbd(kbd).expect("test key sequence is valid kbd") {
            self.key(key);
        }
    }

    /// Feeds one key as a terminal would deliver it, uncanonicalized.
    pub fn key(&mut self, key: Key) {
        self.handle(Event::Key(self.client, key));
    }

    pub fn paste(&mut self, bytes: &[u8]) {
        self.handle(Event::Paste(self.client, bytes.to_vec()));
    }

    /// Feeds each character of `text` as a plain key, as typing it would.
    pub fn type_text(&mut self, text: &str) {
        for character in text.chars() {
            self.key(Key {
                code: KeyCode::Char(character),
                mods: Mods::default(),
            });
        }
    }

    /// Handles the event, then carries out its effects the way the server's
    /// workers do, feeding their results back in until none are left.
    pub fn handle(&mut self, event: Event) {
        let mut events = VecDeque::from([event]);
        while let Some(event) = events.pop_front() {
            self.editor.handle(event);
            for effect in self.editor.take_effects() {
                if let Some(result) = self.run_effect(effect) {
                    events.push_back(result);
                }
            }
        }
    }

    fn run_effect(&mut self, effect: Effect) -> Option<Event> {
        match effect {
            Effect::Send(_, message) => self.sent.push(message),
            Effect::SetTimer { id, .. } => self.timers.push(id),
            Effect::Prefetch(request) => self.prefetches.push(request),
            Effect::Shutdown => self.shutdown = true,
            Effect::StartIndex { buffer } => self.index_requests.push(buffer),
            Effect::Load { buffer, path } => {
                let result =
                    io::load_file(&path, self.editor.config()).map_err(|error| error.to_string());
                return Some(Event::FileLoaded { buffer, result });
            }
            Effect::Save { buffer, plan } => {
                if self.hold_saves {
                    self.held_saves.push((buffer, plan));
                } else {
                    return Some(self.save(buffer, &plan));
                }
            }
            Effect::AutoSave(job) => {
                let result = autosave::write_auto_save(&job.file, &job.text)
                    .map_err(|error| error.to_string());
                return Some(Event::AutoSaved {
                    buffer: job.buffer,
                    result,
                });
            }
            Effect::RemoveAutoSave { file } => {
                if let Err(error) = autosave::remove_auto_save(&file) {
                    self.io_errors.push(format!("{}: {error}", file.display()));
                }
            }
            Effect::ReadAutoSave { buffer, file } => {
                let result = autosave::read_auto_save(&file).map_err(|error| {
                    format!(
                        "Opening input file: {}, {}",
                        io::strerror(&error),
                        autosave::auto_save_path(&file).display()
                    )
                });
                return Some(Event::AutoSaveRead { buffer, result });
            }
        }
        None
    }

    fn save(&mut self, buffer: BufferId, plan: &SavePlan) -> Event {
        let result = match self.sinks.as_mut() {
            Some(Sinks(sinks)) => io::save_with(plan, sinks.as_mut()),
            None => io::save(plan),
        };
        Event::SaveFinished {
            buffer,
            result: result.map_err(|error| SaveFailure::from(&error)),
        }
    }

    /// Saves write through sinks from `sinks`, for injecting failures.
    pub fn set_save_sink(&mut self, sinks: Box<SinkFactory>) {
        self.sinks = Some(Sinks(sinks));
    }

    pub fn clear_save_sink(&mut self) {
        self.sinks = None;
    }

    /// Keeps save effects instead of running them, so the state while a
    /// save is in flight can be inspected.
    pub fn hold_saves(&mut self) {
        self.hold_saves = true;
    }

    pub fn held_saves(&self) -> Vec<SavePlan> {
        self.held_saves
            .iter()
            .map(|(_, plan)| plan.clone())
            .collect()
    }

    /// Runs the held saves and stops holding.
    pub fn release_saves(&mut self) {
        self.hold_saves = false;
        for (buffer, plan) in std::mem::take(&mut self.held_saves) {
            let finished = self.save(buffer, &plan);
            self.handle(finished);
        }
    }

    /// Buffers whose line index was asked for, since the last call.
    pub fn take_index_requests(&mut self) -> Vec<BufferId> {
        std::mem::take(&mut self.index_requests)
    }

    /// Failures of effects that report no event, such as deleting an
    /// auto-save file.
    pub fn io_errors(&self) -> &[String] {
        &self.io_errors
    }

    pub fn set_default_directory(&mut self, directory: &Path) {
        self.editor.set_default_directory(directory);
    }

    /// The echo area's whole line: the message, or the minibuffer's
    /// prompt, input and any message over it.
    pub fn echo_line(&self) -> String {
        self.editor.echo_area(self.client).0
    }

    /// Fires the timers pending now, oldest first, as if their delays passed
    /// with no input.
    pub fn fire_timers(&mut self) {
        for id in std::mem::take(&mut self.timers) {
            self.handle(Event::Timer(id));
        }
    }

    /// Messages sent to clients since the last call.
    pub fn take_sent(&mut self) -> Vec<ServerMsg> {
        std::mem::take(&mut self.sent)
    }

    /// Cache blocks the renderer asked for since the last call.
    pub fn take_prefetches(&mut self) -> Vec<PrefetchRequest> {
        std::mem::take(&mut self.prefetches)
    }

    /// The terminal was resized to `cols` by `rows`.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.handle(Event::Resize(self.client, cols, rows));
    }

    /// The client's frame as it is rendered now.
    pub fn grid(&self) -> Grid {
        render(&self.editor, self.client).0
    }

    /// Each row of the frame as text, the second halves of wide characters
    /// left out.
    pub fn screen(&self) -> Vec<String> {
        let grid = self.grid();
        (0..grid.rows)
            .map(|row| {
                grid.row(row)
                    .iter()
                    .filter(|cell| cell.width > 0)
                    .map(|cell| cell.text.as_str())
                    .collect()
            })
            .collect()
    }

    /// The cursor as (column, row), or `None` when point is not shown.
    pub fn cursor(&self) -> Option<(u16, u16)> {
        self.grid().cursor
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown
    }

    pub fn client(&self) -> ClientId {
        self.client
    }

    pub fn editor(&self) -> &Editor {
        &self.editor
    }

    pub fn editor_mut(&mut self) -> &mut Editor {
        &mut self.editor
    }

    /// The selected window's buffer.
    pub fn buffer(&self) -> &Buffer {
        self.editor.buffer(self.editor.selected_buffer(self.client))
    }

    pub fn buffer_mut(&mut self) -> &mut Buffer {
        let id = self.editor.selected_buffer(self.client);
        self.editor.buffer_mut(id)
    }

    /// The buffer text; invalid UTF-8 is replaced.
    pub fn text(&self) -> String {
        let text = &self.buffer().text;
        let mut bytes = Vec::new();
        text.read(0..text.len(), &mut bytes)
            .expect("harness buffers are in memory");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    pub fn point(&self) -> u64 {
        let window = self.editor.selected_window_id(self.client);
        self.editor.window_point(self.client, window)
    }

    pub fn mark(&self) -> Option<u64> {
        let buffer = self.buffer();
        buffer.mark.map(|mark| buffer.marker_pos(mark))
    }

    /// Emacs `mark-active`.
    pub fn mark_active(&self) -> bool {
        self.buffer().mark_active
    }

    /// The last key or paste ended in an error, as one would end a keyboard
    /// macro; the echo area holds its message.
    pub fn signaled(&self) -> bool {
        self.editor.signaled(self.client)
    }

    /// Newest first; invalid UTF-8 is replaced.
    pub fn kill_ring(&self) -> Vec<String> {
        self.editor
            .kill_ring()
            .entries()
            .map(|entry| String::from_utf8_lossy(entry).into_owned())
            .collect()
    }

    pub fn echo(&self) -> String {
        self.editor.echo(self.client).to_string()
    }
}
