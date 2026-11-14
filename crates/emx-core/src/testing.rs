//! Headless harness: one attached client driven through `Editor::handle`,
//! with effects processed synchronously. Timers fire only when a test asks.

use std::sync::Arc;

use emx_proto::{ClientKind, ServerMsg};
use emx_text::{Sources, Text};
use emx_types::{Grid, Key, parse_kbd};

use crate::buffer::Buffer;
use crate::config::Config;
use crate::editor::{Editor, Session};
use crate::event::{ClientId, Effect, Event, TimerId};
use crate::marker::InsertionType;
use crate::render::{PrefetchRequest, render};

pub const HARNESS_COLS: u16 = 80;
pub const HARNESS_ROWS: u16 = 24;
const HARNESS_CLIENT: ClientId = ClientId(1);
const POINT_MARK: char = '|';
const MARK_MARK: char = '^';

#[derive(Debug)]
pub struct Harness {
    editor: Editor,
    client: ClientId,
    sent: Vec<ServerMsg>,
    timers: Vec<TimerId>,
    prefetches: Vec<PrefetchRequest>,
    shutdown: bool,
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

    pub fn handle(&mut self, event: Event) {
        self.editor.handle(event);
        for effect in self.editor.take_effects() {
            match effect {
                Effect::Send(_, message) => self.sent.push(message),
                Effect::SetTimer { id, .. } => self.timers.push(id),
                Effect::Prefetch(request) => self.prefetches.push(request),
                Effect::Shutdown => self.shutdown = true,
            }
        }
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
