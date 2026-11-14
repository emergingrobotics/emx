//! Headless harness: one attached client driven through `Editor::handle`,
//! with effects processed synchronously. Timers fire only when a test asks.

use std::collections::{BTreeSet, VecDeque};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use emx_proto::{ClientKind, Request, ServerMsg};
use emx_text::{Sources, Text};
use emx_types::{Grid, Key, KeyCode, Mods, parse_kbd};

use crate::autosave;
use crate::buffer::{Buffer, BufferId};
use crate::completion::{Candidate, ListingError};
use crate::config::Config;
use crate::diff;
use crate::editor::{Editor, Session};
use crate::event::{ClientId, Effect, Event, ListingId, ReloadJob, TimerId};
use crate::io::{self, SaveFailure, SavePlan, SaveSink, SinkFactory};
use crate::journal::{JournalError, JournalOp, JournalWriter, RecoveredBuffer, replay_dir};
use crate::marker::InsertionType;
use crate::reload;
use crate::render::{PrefetchRequest, render};
use crate::search_job::{SearchJob, run_search_job};

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
    hold_loads: bool,
    held_loads: Vec<(BufferId, PathBuf)>,
    held_reloads: Vec<ReloadJob>,
    hold_listings: bool,
    held_listings: Vec<(ClientId, ListingId, PathBuf)>,
    hold_searches: bool,
    held_searches: Vec<SearchJob>,
    sinks: Option<Sinks>,
    /// Auto-save files that could not be deleted.
    io_errors: Vec<String>,
    journal_ops: Vec<JournalOp>,
    /// The paths the editor asked to be watched and has not let go.
    watched: BTreeSet<PathBuf>,
    /// Every message sent, to whom.
    messages: Vec<(ClientId, ServerMsg)>,
    next_client: u64,
    next_request: u64,
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
            hold_loads: false,
            held_loads: Vec::new(),
            held_reloads: Vec::new(),
            hold_listings: false,
            held_listings: Vec::new(),
            hold_searches: false,
            held_searches: Vec::new(),
            sinks: None,
            io_errors: Vec::new(),
            journal_ops: Vec::new(),
            watched: BTreeSet::new(),
            messages: Vec::new(),
            next_client: HARNESS_CLIENT.0 + 1,
            next_request: 1,
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
        self.handle(Event::Paste {
            client: self.client,
            data: bytes.to_vec(),
            last: true,
        });
    }

    /// Attaches another 80x24 client and returns its id.
    pub fn attach(&mut self, kind: ClientKind) -> ClientId {
        let client = ClientId(self.next_client);
        self.next_client += 1;
        self.handle(Event::ClientAttached {
            client,
            cols: HARNESS_COLS,
            rows: HARNESS_ROWS,
            kind,
        });
        client
    }

    pub fn detach(&mut self, client: ClientId) {
        self.handle(Event::ClientDetached(client));
    }

    /// Feeds an Emacs `kbd` sequence from `client`.
    pub fn keys_as(&mut self, client: ClientId, kbd: &str) {
        for key in parse_kbd(kbd).expect("test key sequence is valid kbd") {
            self.handle(Event::Key(client, key));
        }
    }

    /// Feeds each character of `text` from `client`.
    pub fn type_as(&mut self, client: ClientId, text: &str) {
        for character in text.chars() {
            let key = Key {
                code: KeyCode::Char(character),
                mods: Mods::default(),
            };
            self.handle(Event::Key(client, key));
        }
    }

    /// One chunk of a bracketed paste from `client`.
    pub fn paste_chunk(&mut self, client: ClientId, bytes: &[u8], last: bool) {
        self.handle(Event::Paste {
            client,
            data: bytes.to_vec(),
            last,
        });
    }

    /// Sends a socket API request from `client`; returns its id.
    pub fn request(&mut self, client: ClientId, request: Request) -> u64 {
        let id = self.next_request;
        self.next_request += 1;
        self.handle(Event::Request(client, id, request));
        id
    }

    /// Messages sent to `client` since the last call for it.
    pub fn take_messages(&mut self, client: ClientId) -> Vec<ServerMsg> {
        let (taken, kept) = std::mem::take(&mut self.messages)
            .into_iter()
            .partition(|(to, _)| *to == client);
        self.messages = kept;
        taken.into_iter().map(|(_, message)| message).collect()
    }

    /// Point in `client`'s selected window.
    pub fn point_of(&self, client: ClientId) -> u64 {
        let window = self.editor.selected_window_id(client);
        self.editor.window_point(client, window)
    }

    /// `client`'s frame as text, as `screen` gives the harness client's.
    pub fn screen_of(&self, client: ClientId) -> Vec<String> {
        let grid = render(&self.editor, client).0;
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

    /// `client`'s echo area message.
    pub fn echo_of(&self, client: ClientId) -> String {
        self.editor.echo(client).to_string()
    }

    /// `client`'s echo area line; invalid UTF-8 is replaced.
    pub fn echo_line_of(&self, client: ClientId) -> String {
        String::from_utf8_lossy(&self.editor.echo_area(client).0).into_owned()
    }

    /// The echo area line as bytes, file names that are not UTF-8 intact.
    pub fn echo_line_bytes(&self) -> Vec<u8> {
        self.editor.echo_area(self.client).0
    }

    /// The completion candidates listed above `client`'s prompt, top first,
    /// as shown; invalid UTF-8 is replaced.
    pub fn candidates_of(&self, client: ClientId) -> Vec<String> {
        self.editor
            .candidate_lines(client)
            .into_iter()
            .map(|(text, _)| String::from_utf8_lossy(&text).into_owned())
            .collect()
    }

    /// The candidates listed above the prompt, top first.
    pub fn candidates(&self) -> Vec<String> {
        self.candidates_of(self.client)
    }

    /// The selected candidate as shown, if one is listed.
    pub fn selected_candidate(&self) -> Option<String> {
        self.editor
            .candidate_lines(self.client)
            .into_iter()
            .find(|(_, selected)| *selected)
            .map(|(text, _)| String::from_utf8_lossy(&text).into_owned())
    }

    /// The selected candidate's position among all matches, and how many
    /// match, while the prompt completes.
    pub fn completion_selection(&self) -> Option<(usize, usize)> {
        self.editor.completion_selection(self.client)
    }

    /// Keeps directory listings instead of running them, as a slow (or
    /// hung) file system would.
    pub fn hold_listings(&mut self) {
        self.hold_listings = true;
    }

    /// The directories asked to be listed and not yet answered.
    pub fn held_listings(&self) -> Vec<PathBuf> {
        self.held_listings
            .iter()
            .map(|(_, _, directory)| directory.clone())
            .collect()
    }

    /// Runs the held listings, oldest first, and stops holding.
    pub fn release_listings(&mut self) {
        self.hold_listings = false;
        for (client, id, directory) in std::mem::take(&mut self.held_listings) {
            self.handle(Event::DirectoryListed {
                client,
                id,
                result: io::list_directory(&directory),
            });
        }
    }

    /// Answers every held listing with `result` instead of reading, as a
    /// server that refused or failed the reads would.
    pub fn answer_held_listings(&mut self, result: Result<Vec<Candidate>, ListingError>) {
        self.hold_listings = false;
        for (client, id, _) in std::mem::take(&mut self.held_listings) {
            self.handle(Event::DirectoryListed {
                client,
                id,
                result: result.clone(),
            });
        }
    }

    /// Answers only the newest held listing, as a fast directory would
    /// after a slow one.
    pub fn release_newest_listing(&mut self) {
        if let Some((client, id, directory)) = self.held_listings.pop() {
            self.handle(Event::DirectoryListed {
                client,
                id,
                result: io::list_directory(&directory),
            });
        }
    }

    /// The text of the buffer named `name`; invalid UTF-8 is replaced.
    pub fn buffer_text(&self, name: &str) -> Option<String> {
        let buffer = self.editor.find_buffer(name)?;
        let mut bytes = Vec::new();
        buffer
            .text
            .read(0..buffer.text.len(), &mut bytes)
            .expect("harness buffers are readable");
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Panics when no buffer is named `name`.
    pub fn buffer_modified(&self, name: &str) -> bool {
        self.editor
            .find_buffer(name)
            .expect("the buffer exists")
            .is_modified()
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
            Effect::Send(client, message) => {
                if client == self.client {
                    self.sent.push(message.clone());
                }
                self.messages.push((client, message));
            }
            Effect::SetTimer { id, .. } => self.timers.push(id),
            Effect::Prefetch(request) => self.prefetches.push(request),
            Effect::Shutdown => self.shutdown = true,
            Effect::Journal(op) => self.journal_ops.push(op),
            Effect::StartIndex { buffer } => self.index_requests.push(buffer),
            Effect::Load { buffer, path } if self.hold_loads => {
                self.held_loads.push((buffer, path));
            }
            Effect::Load { buffer, path } => return Some(self.load(buffer, &path)),
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
            Effect::Observe {
                buffer,
                path,
                old_len,
            } => {
                let tail_bytes = self.editor.config().append_tail_hash_bytes;
                return Some(Event::Observed {
                    buffer,
                    result: reload::observe(&path, old_len, tail_bytes),
                });
            }
            Effect::Reload(job) if self.hold_loads => self.held_reloads.push(job),
            Effect::Reload(job) => {
                let result = reload::reload_file(
                    &job.path,
                    self.editor.config(),
                    &job.previous,
                    &job.anchors,
                );
                return Some(Event::Reloaded {
                    buffer: job.buffer,
                    result,
                });
            }
            Effect::ReadAppend {
                buffer,
                path,
                old,
                new,
                old_tail_hash,
            } => {
                let tail_bytes = self.editor.config().append_tail_hash_bytes;
                return Some(Event::AppendRead {
                    buffer,
                    result: reload::read_appended(&path, old, new, old_tail_hash, tail_bytes),
                });
            }
            Effect::Watch { path } => {
                assert!(self.watched.insert(path), "a path is watched once");
            }
            Effect::Unwatch { path } => {
                assert!(self.watched.remove(&path), "only watched paths are let go");
            }
            Effect::ListDirectory {
                client,
                id,
                directory,
            } => {
                if self.hold_listings {
                    self.held_listings.push((client, id, directory));
                } else {
                    return Some(Event::DirectoryListed {
                        client,
                        id,
                        result: io::list_directory(&directory),
                    });
                }
            }
            Effect::StartSearch(job) => {
                if self.hold_searches {
                    self.held_searches.push(job);
                } else {
                    return Some(run_search_job(&job));
                }
            }
            Effect::Diff(job) => {
                return Some(Event::DiffReady {
                    buffer: job.buffer,
                    requester: job.requester,
                    result: diff::run_diff(&job),
                });
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

    fn load(&self, buffer: BufferId, path: &Path) -> Event {
        let result = io::load_file(path, self.editor.config()).map_err(|error| error.to_string());
        Event::FileLoaded { buffer, result }
    }

    fn save(&mut self, buffer: BufferId, plan: &SavePlan) -> Event {
        let tail_bytes = self.editor.config().append_tail_hash_bytes;
        let result = match self.sinks.as_mut() {
            Some(Sinks(sinks)) => io::save_file_with(plan, tail_bytes, sinks.as_mut()),
            None => io::save_file(plan, tail_bytes),
        };
        Event::SaveFinished {
            buffer,
            result: result.map_err(|error| SaveFailure::from(&error)),
        }
    }

    /// Searches started on a worker wait until `release_searches`, so
    /// tests can see the editor while a search runs.
    pub fn hold_searches(&mut self) {
        self.hold_searches = true;
    }

    /// The searches held so far: whether each was cancelled.
    pub fn held_searches(&self) -> Vec<bool> {
        self.held_searches
            .iter()
            .map(|job| job.cancel.load(std::sync::atomic::Ordering::Relaxed))
            .collect()
    }

    /// Runs the held searches, oldest first, and stops holding new ones.
    pub fn release_searches(&mut self) {
        self.hold_searches = false;
        for job in std::mem::take(&mut self.held_searches) {
            self.handle(run_search_job(&job));
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

    /// Keeps file reads (visits and reloads) instead of running them, so
    /// the state while a file is being read can be inspected.
    pub fn hold_loads(&mut self) {
        self.hold_loads = true;
    }

    /// Runs the held reads and stops holding.
    pub fn release_loads(&mut self) {
        self.hold_loads = false;
        for (buffer, path) in std::mem::take(&mut self.held_loads) {
            let loaded = self.load(buffer, &path);
            self.handle(loaded);
        }
        for job in std::mem::take(&mut self.held_reloads) {
            let result =
                reload::reload_file(&job.path, self.editor.config(), &job.previous, &job.anchors);
            self.handle(Event::Reloaded {
                buffer: job.buffer,
                result,
            });
        }
    }

    /// Buffers whose line index was asked for, since the last call.
    pub fn take_index_requests(&mut self) -> Vec<BufferId> {
        std::mem::take(&mut self.index_requests)
    }

    /// The journal thread reports that writing `journal` failed.
    pub fn journal_write_failed(&mut self, journal: crate::journal::JournalId, reason: &str) {
        self.editor.journal_write_failed(journal, reason);
        for effect in self.editor.take_effects() {
            if let Some(result) = self.run_effect(effect) {
                self.handle(result);
            }
        }
    }

    /// The paths being watched.
    pub fn watched(&self) -> &BTreeSet<PathBuf> {
        &self.watched
    }

    /// Journal operations the editor emitted since the last call.
    pub fn journal_ops(&mut self) -> Vec<JournalOp> {
        std::mem::take(&mut self.journal_ops)
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
        String::from_utf8_lossy(&self.editor.echo_area(self.client).0).into_owned()
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

/// Applies `ops` in order and syncs, as the server's journal thread does
/// with each batch.
pub fn apply_journal(writer: &mut JournalWriter, ops: Vec<JournalOp>) -> Result<(), JournalError> {
    for op in ops {
        writer.apply(op)?;
    }
    writer.flush_and_sync()
}

/// Applies `ops` with a new `JournalWriter` over `directory`, as a session
/// server does from its start.
pub fn write_journal(directory: &Path, ops: Vec<JournalOp>) -> Result<(), JournalError> {
    let mut writer = JournalWriter::new(directory).map_err(|source| JournalError::Write {
        path: directory.to_path_buf(),
        source,
    })?;
    apply_journal(&mut writer, ops)
}

/// Writes `ops`, then replays the directory as a restarted server would:
/// edit, crash, replay, compare.
pub fn write_and_replay(
    directory: &Path,
    ops: Vec<JournalOp>,
    config: &Config,
) -> Result<Vec<RecoveredBuffer>, JournalError> {
    write_journal(directory, ops)?;
    replay_dir(directory, config)
}
