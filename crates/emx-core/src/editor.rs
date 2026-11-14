mod files;
mod reload;
mod requests;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use emx_proto::{ClientKind, ServerMsg, diff_grids};
use emx_text::{ReadError, Sources, Text};
use emx_types::{Grid, Key, KeyCode, Mods};
use slotmap::SlotMap;

use crate::buffer::{Buffer, BufferId, ChangeRecorder};
use crate::command::{self, CommandContext, CommandError, PrefixArg, RecenterPosition};
use crate::config::{Config, ConfigError};
use crate::event::{ClientId, Effect, Event, TimerId};
use crate::journal::{JournalId, JournalOutbox};
use crate::keymap::{Keymap, Lookup, describe_keys, universal_argument_binding};
use crate::killring::KillRing;
use crate::marker::{InsertionType, MarkerId};
use crate::minibuffer::{Histories, Minibuffer};
use crate::render::{PrefetchRequest, render};
use crate::undo::{UndoLimits, outer_limit_warning};
pub(crate) use files::Purpose;
use files::{AutoSaveRequest, ExitWait, SaveRequest};
use reload::ReloadRequest;
use requests::{GroupReply, OpenRequest, Reply};

use crate::window::{
    Fetch, Frame, Layout, Reader, ScreenLine, Window, WindowId, frame_layout, not_cached_block,
};

const SCRATCH_BUFFER: &str = "*scratch*";
/// The one terminal frame of `emx --no-server`, like `emacs -nw`'s.
const NO_SERVER_FRAME: u64 = 1;
/// A daemon's own frame is F1, so `emacsclient -t` frames start at F2.
const FIRST_SERVER_FRAME: u64 = 2;

/// How clients reach the editor, which decides how their terminal frames
/// are named on the modeline, as Emacs names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    /// `emx --no-server`: one terminal frame, F1, as `emacs -nw` has.
    NoServer,
    /// The session server: each terminal client is a frame like one from
    /// `emacsclient -t`, shown with `@` and numbered in attach order.
    Server,
}
const PASTE_COMMAND: &str = "xterm-paste";
/// How Emacs describes a bracketed-paste event in a key sequence.
const PASTE_EVENT_DESCRIPTION: &str = "<xterm-paste>";
/// The quit character: in Emacs it aborts any key sequence being read,
/// whatever the keymaps say.
const QUIT_KEY: Key = Key {
    code: KeyCode::Char('g'),
    mods: Mods {
        ctrl: true,
        meta: false,
        shift: false,
        super_: false,
    },
};

/// Emacs's per-keyboard command-loop state.
#[derive(Debug, Default)]
struct CommandLoopState {
    /// The key sequence read so far.
    pending: Vec<Key>,
    /// Emacs `prefix-arg`: the argument for the next command.
    prefix_arg: PrefixArg,
    /// `universal-argument-map` is active for the next key.
    universal_argument_map: bool,
    last_command: Option<&'static str>,
    /// The unfinished sequence is being echoed; once it is, every further
    /// key echoes at once, as in Emacs.
    echoing: bool,
    echo_timer: Option<TimerId>,
}

impl CommandLoopState {
    fn mid_command(&self) -> bool {
        !self.pending.is_empty() || self.prefix_arg != PrefixArg::None
    }

    /// The echo of an unfinished command: the argument, then the keys.
    fn echo_text(&self) -> String {
        let mut parts: Vec<String> = self.prefix_arg.description().into_iter().collect();
        if !self.pending.is_empty() {
            parts.push(describe_keys(&self.pending));
        }
        format!("{}-", parts.join(" "))
    }

    fn end_command(&mut self) {
        self.pending.clear();
        self.prefix_arg = PrefixArg::None;
        self.universal_argument_map = false;
        self.echoing = false;
        self.echo_timer = None;
    }
}

#[derive(Debug)]
struct Client {
    cols: u16,
    rows: u16,
    kind: ClientKind,
    /// The `F<n>` frame name; only terminal clients have frames.
    frame_number: Option<u64>,
    frame: Frame,
    /// The last grid sent, which the next frame is diffed against; `None`
    /// sends the next frame whole.
    grid: Option<Grid>,
    /// Emacs `recenter-last-op`, for `C-l` cycling.
    recenter_last_op: Option<RecenterPosition>,
    command_loop: CommandLoopState,
    /// The echo area: the last message, or the echo of keys being read.
    echo: String,
    /// The last input event ended in an error (Emacs `command-error`),
    /// which would also end a keyboard macro.
    signaled: bool,
    /// Emacs `temporary-goal-column` for this client's vertical motion.
    temporary_goal_column: u64,
    /// Keys go to the minibuffer while it reads something.
    minibuffer: Option<Minibuffer>,
    exit_wait: Option<ExitWait>,
    /// The chunks of a bracketed paste received so far.
    paste: Option<PendingPaste>,
    /// The text this client's last yank inserted. Emacs finds it between
    /// point and the mark, but the mark is the buffer's, shared by every
    /// client, so another client could move it before `M-y`.
    yank: Option<YankRange>,
    /// The cursor `emx events` last reported, and the timer that holds back
    /// the next report.
    cursor_reported: Option<(BufferId, u64)>,
    cursor_timer: Option<TimerId>,
}

#[derive(Debug, Default)]
struct PendingPaste {
    data: Vec<u8>,
    /// The paste outgrew `in_memory_max`; the rest is dropped until the
    /// last chunk.
    overflowed: bool,
}

#[derive(Debug, Clone, Copy)]
struct YankRange {
    buffer: BufferId,
    start: MarkerId,
    end: MarkerId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerPurpose {
    EchoKeystrokes(ClientId),
    /// The daemon exits once no client is attached and nothing is unsaved,
    /// after a grace period that lets a client reattach.
    DaemonGrace,
    /// Clears a message shown over the client's minibuffer, if it is still
    /// the one with this serial.
    MinibufferMessage(ClientId, u64),
    /// Emacs `auto-save-timeout`.
    AutoSaveIdle,
    /// The client's cursor may be reported to `emx events` again.
    CursorEvent(ClientId),
    /// A settling file is looked at again.
    Settle(BufferId),
}

/// How a command finished, for the command loop's bookkeeping.
enum Completion {
    /// A prefix-argument command ran: the argument and `last-command`
    /// carry over to the next command.
    KeepPrefixState(PrefixArg),
    /// Any other command. Emacs `universal-argument-more` after digits sets
    /// `prefix-arg` without `universal-argument--mode`; the argument still
    /// carries over.
    Ran {
        this_command: Option<&'static str>,
        next_prefix_arg: PrefixArg,
    },
}

/// The editor core: a deterministic state machine that consumes `Event`s,
/// mutates buffers and per-client state, and emits `Effect`s.
#[derive(Debug)]
pub struct Editor {
    config: Config,
    keymap: Keymap,
    buffers: SlotMap<BufferId, Buffer>,
    scratch: BufferId,
    clients: BTreeMap<ClientId, Client>,
    kill_ring: KillRing,
    journal_outbox: JournalOutbox,
    effects: Vec<Effect>,
    timers: HashMap<TimerId, TimerPurpose>,
    next_timer: u64,
    /// The one grace timer that may shut the daemon down; earlier ones were
    /// superseded by a later detach or cancelled by an attach.
    grace_timer: Option<TimerId>,
    session: Session,
    next_frame_number: u64,
    histories: Histories,
    /// Emacs `default-directory` for buffers that visit no file.
    default_directory: PathBuf,
    /// Buffers, most recently shown first.
    recency: Vec<BufferId>,
    /// Who asked for each file being read, saved or recovered.
    pending_loads: HashMap<BufferId, ClientId>,
    pending_saves: HashMap<BufferId, SaveRequest>,
    pending_reads: HashMap<BufferId, ClientId>,
    auto_saves: HashMap<BufferId, AutoSaveRequest>,
    /// Commands that changed text since the last auto-save, toward Emacs
    /// `auto-save-interval`, and in all, to tell whether the idle timer saw
    /// any.
    edits_since_auto_save: u64,
    edits_total: u64,
    auto_save_timer: Option<(TimerId, u64)>,
    /// Clients streaming `emx events`.
    subscribers: BTreeSet<ClientId>,
    /// Terminal clients, most recently active first: where a CLI `open`
    /// shows its file.
    terminal_recency: Vec<ClientId>,
    /// Open requests waiting for their file to be read.
    pending_opens: HashMap<BufferId, Vec<OpenRequest>>,
    /// `--wait` requests, released by `C-x #` or by killing the buffer.
    waiting: HashMap<BufferId, Vec<Reply>>,
    /// Buffers created for a `--wait` request, killed when it is done, as
    /// Emacs `server-kill-new-buffers` does.
    created_for_wait: BTreeSet<BufferId>,
    /// Buffers being read again for a reload request.
    reverting: BTreeSet<BufferId>,
    reload_replies: Vec<GroupReply>,
    save_replies: Vec<GroupReply>,
    /// Observations in flight, and whether another was asked for meanwhile.
    observing: HashMap<BufferId, bool>,
    /// The settle timer each settling buffer waits for.
    settle_timers: HashMap<BufferId, TimerId>,
    /// External changes that came while the buffer was busy, looked into
    /// once it is not.
    reload_recheck: BTreeSet<BufferId>,
    reloading: HashMap<BufferId, ReloadRequest>,
    /// Appends being read, with the text they extend.
    appending: HashMap<BufferId, Text>,
    /// Confirmed `revert-buffer`s waiting for the file to settle, and who
    /// asked.
    revert_pending: HashMap<BufferId, Option<ClientId>>,
    /// Follow-edit mode: a reload moves point to the first changed line.
    follow_edit: bool,
}

impl Editor {
    /// An editor for `--no-server` use. Fails when the `[keys]` overrides
    /// are invalid.
    pub fn new(config: Config) -> Result<Editor, ConfigError> {
        Editor::with_session(config, Session::NoServer)
    }

    /// Fails when the `[keys]` overrides are invalid.
    pub fn with_session(config: Config, session: Session) -> Result<Editor, ConfigError> {
        let mut keymap = Keymap::emacs_default();
        keymap.apply_overrides(&config.keys)?;
        let kill_ring_max =
            usize::try_from(config.kill_ring_max).expect("kill_ring_max fits in usize");
        let mut editor = Editor {
            keymap,
            buffers: SlotMap::with_key(),
            scratch: BufferId::default(),
            clients: BTreeMap::new(),
            kill_ring: KillRing::new(kill_ring_max),
            journal_outbox: JournalOutbox::new(config.journal_snapshot_max),
            effects: Vec::new(),
            timers: HashMap::new(),
            next_timer: 0,
            grace_timer: None,
            session,
            next_frame_number: FIRST_SERVER_FRAME,
            histories: Histories::default(),
            // A working directory that no longer exists leaves only the
            // root to offer as a starting point in file prompts.
            default_directory: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            recency: Vec::new(),
            pending_loads: HashMap::new(),
            pending_saves: HashMap::new(),
            pending_reads: HashMap::new(),
            auto_saves: HashMap::new(),
            edits_since_auto_save: 0,
            edits_total: 0,
            auto_save_timer: None,
            subscribers: BTreeSet::new(),
            terminal_recency: Vec::new(),
            pending_opens: HashMap::new(),
            waiting: HashMap::new(),
            created_for_wait: BTreeSet::new(),
            reverting: BTreeSet::new(),
            reload_replies: Vec::new(),
            save_replies: Vec::new(),
            observing: HashMap::new(),
            settle_timers: HashMap::new(),
            reload_recheck: BTreeSet::new(),
            reloading: HashMap::new(),
            appending: HashMap::new(),
            revert_pending: HashMap::new(),
            follow_edit: false,
            config,
        };
        editor.scratch = editor.create_buffer(SCRATCH_BUFFER);
        editor.track_journal(editor.scratch);
        Ok(editor)
    }

    fn create_buffer(&mut self, name: &str) -> BufferId {
        let limits = UndoLimits::from(&self.config);
        let text = Text::empty(Sources::memory(Arc::from(Vec::new())));
        let id = self
            .buffers
            .insert_with_key(|id| Buffer::new(id, name.to_string(), text, limits));
        // Creating a buffer is a command's change: it gets its boundary.
        self.buffers[id].undo.boundary(0);
        self.recency.push(id);
        id
    }

    pub fn handle(&mut self, event: Event) {
        self.journal_outbox.begin_event();
        match event {
            Event::ClientAttached {
                client,
                cols,
                rows,
                kind,
            } => self.attach(client, cols, rows, kind),
            Event::ClientDetached(client) => self.detach(client),
            Event::Key(client, key) => {
                self.note_terminal_activity(client);
                self.handle_key(client, key.canonical());
            }
            Event::Paste { client, data, last } => {
                self.note_terminal_activity(client);
                self.paste_chunk(client, data, last);
            }
            Event::Resize(client, cols, rows) => {
                let client = self.client_mut(client);
                client.cols = cols;
                client.rows = rows;
            }
            Event::Request(client, id, request) => self.handle_request(client, id, request),
            Event::Redraw(client) => self.request_full_redraw(client),
            Event::BlockReady { buffer, result, .. } => self.block_ready(buffer, result),
            Event::IndexProgress { .. } => {}
            Event::IndexFinished { buffer, result } => self.index_finished(buffer, result),
            Event::Timer(id) => self.handle_timer(id),
            Event::FileLoaded { buffer, result } => self.file_loaded(buffer, result),
            Event::SaveFinished { buffer, result } => self.save_finished(buffer, result),
            Event::AutoSaved { buffer, result } => self.auto_saved(buffer, result),
            Event::AutoSaveRead { buffer, result } => self.auto_save_read(buffer, result),
            Event::Recovered(buffers) => self.recovered(buffers),
            Event::FileChanged { path } => self.file_changed(&path),
            Event::FileRenamed { from, to } => self.file_renamed(&from, &to),
            Event::Observed { buffer, result } => self.observed(buffer, result),
            Event::Reloaded { buffer, result } => self.reloaded(buffer, result),
            Event::AppendRead { buffer, result } => self.append_read(buffer, result),
            Event::DiffReady {
                requester, result, ..
            } => self.diff_ready(requester, result),
        }
        self.redisplay();
        self.report_cursors();
        self.flush_journal();
    }

    /// Emacs redisplay after every input event: every window is scrolled to
    /// show its point, then each terminal client gets the rows of its frame
    /// that changed. Other clients' edits can move any window's point.
    fn redisplay(&mut self) {
        let client_ids: Vec<ClientId> = self.clients.keys().copied().collect();
        for client_id in &client_ids {
            self.keep_points_visible(*client_id);
        }
        for client_id in client_ids {
            if self.client(client_id).kind != ClientKind::Tui {
                continue;
            }
            let (grid, prefetch) = render(self, client_id);
            self.effects
                .extend(prefetch.into_iter().map(Effect::Prefetch));
            let client = self.client_mut(client_id);
            if client.grid.as_ref() != Some(&grid) {
                let message = diff_grids(client.grid.as_ref(), &grid);
                client.grid = Some(grid);
                self.effects.push(Effect::Send(client_id, message));
            }
        }
    }

    /// Recenters each window of the client whose point is off screen.
    /// Like rendering, this never blocks on I/O: a window whose text is not
    /// cached keeps its start until the prefetched block arrives and the
    /// next event redisplays.
    fn keep_points_visible(&mut self, client_id: ClientId) {
        let window_ids: Vec<WindowId> = self.client(client_id).frame.windows.keys().collect();
        for window_id in window_ids {
            if let Err(error) = self.keep_point_visible(client_id, window_id) {
                let buffer = self.window(client_id, window_id).buffer;
                self.effects.push(Effect::Prefetch(PrefetchRequest {
                    buffer,
                    block: not_cached_block(error),
                }));
            }
        }
    }

    fn keep_point_visible(
        &mut self,
        client_id: ClientId,
        window_id: WindowId,
    ) -> Result<(), ReadError> {
        let (cols, height) = self.window_size(client_id, window_id);
        let window = *self.window(client_id, window_id);
        let buffer = &self.buffers[window.buffer];
        let text = buffer.text.clone();
        let point = buffer.marker_pos(window.point);
        let start = self.window_start(client_id, window_id);
        let layout = Layout::new(
            Reader::new(&text, Fetch::CachedOnly),
            cols,
            self.tab_width(),
        );
        if let Some(start) = layout.keep_point_visible(start, height, point)? {
            self.set_window_start(client_id, window_id, start);
        }
        Ok(())
    }

    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    /// Ends the session. Unsaved text is dropped only with the user's
    /// consent or when no file buffer has any, so the journals go too.
    pub(super) fn shutdown(&mut self) {
        self.journal_outbox.close_all();
        self.flush_journal();
        self.effects.push(Effect::Shutdown);
    }

    /// Moves the journal operations recorded so far into the effects.
    pub(super) fn flush_journal(&mut self) {
        let ops = self.journal_outbox.take_ops();
        self.effects.extend(ops.into_iter().map(Effect::Journal));
    }

    /// The buffer named `name`.
    pub fn find_buffer(&self, name: &str) -> Option<&Buffer> {
        self.buffers.values().find(|buffer| buffer.name == name)
    }

    /// A buffer visiting a file has edits not yet saved. Like Emacs on exit,
    /// buffers without a file do not count, except ones holding recovered
    /// text that exists nowhere else.
    pub fn has_unsaved(&self) -> bool {
        self.buffers
            .values()
            .any(|buffer| (buffer.file.is_some() || buffer.recovered) && buffer.is_modified())
    }

    /// The journal writer failed to write `journal` (Task 13's journal thread
    /// reports it): the writer refuses that journal's records until a new
    /// epoch, so one starts from the buffer's current text, and every client
    /// is told its edits were briefly unprotected.
    pub fn journal_write_failed(&mut self, journal: JournalId, reason: &str) {
        let Some(buffer) = self.journal_outbox.buffer_of(journal) else {
            return;
        };
        let target = &self.buffers[buffer];
        let message = format!(
            "Journal write failed for {}; edits are unprotected until it recovers: {reason}",
            target.name
        );
        // A damaged buffer's text reads a corrupt file; a snapshot of it
        // would only replace good journaled edits with garbage.
        if !target.damaged {
            let text = target.text.clone();
            self.journal_outbox.restart(buffer, text);
        }
        let clients: Vec<ClientId> = self.clients.keys().copied().collect();
        for client in clients {
            self.set_echo(client, message.clone());
        }
        self.redisplay();
        self.flush_journal();
    }

    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// Terminal size in columns and rows.
    pub fn client_size(&self, client: ClientId) -> (u16, u16) {
        let client = self.client(client);
        (client.cols, client.rows)
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Every client kind gets a frame; CLI clients simply never send keys
    /// and are never sent frames.
    fn attach(&mut self, client: ClientId, cols: u16, rows: u16, kind: ClientKind) {
        assert!(
            !self.clients.contains_key(&client),
            "the server attaches each client id once"
        );
        self.grace_timer = None;
        let frame_number = match (kind, self.session) {
            (ClientKind::Cli, _) => None,
            (ClientKind::Tui, Session::NoServer) => Some(NO_SERVER_FRAME),
            (ClientKind::Tui, Session::Server) => {
                let number = self.next_frame_number;
                self.next_frame_number += 1;
                Some(number)
            }
        };
        let buffer_id = if self.buffers.contains_key(self.scratch) {
            self.scratch
        } else {
            self.other_buffer(self.scratch)
        };
        let buffer = &mut self.buffers[buffer_id];
        let window = Window {
            buffer: buffer_id,
            point: buffer.create_marker_without_undo(0, InsertionType::StayBefore),
            start: buffer.create_marker_without_undo(0, InsertionType::StayBefore),
            start_row_skip: 0,
            hscroll: 0,
        };
        self.clients.insert(
            client,
            Client {
                cols,
                rows,
                kind,
                frame_number,
                frame: Frame::new(window),
                grid: None,
                recenter_last_op: None,
                command_loop: CommandLoopState::default(),
                echo: String::new(),
                signaled: false,
                temporary_goal_column: 0,
                minibuffer: None,
                exit_wait: None,
                paste: None,
                yank: None,
                cursor_reported: None,
                cursor_timer: None,
            },
        );
        if kind == ClientKind::Tui {
            self.note_terminal_activity(client);
            self.client_attached_event(client);
        }
    }

    fn detach(&mut self, client: ClientId) {
        let removed = self
            .clients
            .remove(&client)
            .expect("the server detaches only attached clients");
        for window in removed.frame.windows.values() {
            let buffer = &mut self.buffers[window.buffer];
            buffer.delete_marker(window.point);
            buffer.delete_marker(window.start);
        }
        if let Some(yank) = removed.yank
            && let Some(buffer) = self.buffers.get_mut(yank.buffer)
        {
            buffer.delete_marker(yank.start);
            buffer.delete_marker(yank.end);
        }
        self.client_detached(client, removed.kind);
        self.grace_timer = None;
        self.maybe_arm_grace();
    }

    fn set_timer(&mut self, purpose: TimerPurpose, after_ms: u64) -> TimerId {
        let id = TimerId(self.next_timer);
        self.next_timer += 1;
        self.timers.insert(id, purpose);
        self.effects.push(Effect::SetTimer {
            id,
            after: Duration::from_millis(after_ms),
        });
        id
    }

    fn handle_timer(&mut self, id: TimerId) {
        let purpose = self
            .timers
            .remove(&id)
            .expect("timer events come only for timers the editor set");
        match purpose {
            TimerPurpose::EchoKeystrokes(client_id) => {
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return;
                };
                let state = &mut client.command_loop;
                if state.echo_timer == Some(id) && state.mid_command() {
                    state.echo_timer = None;
                    state.echoing = true;
                    client.echo = state.echo_text();
                }
            }
            TimerPurpose::DaemonGrace => {
                if self.grace_timer == Some(id) {
                    self.grace_timer = None;
                    if self.clients.is_empty() && !self.has_unsaved() {
                        self.shutdown();
                    }
                }
            }
            TimerPurpose::MinibufferMessage(client, serial) => {
                self.minibuffer_message_expired(client, serial);
            }
            TimerPurpose::AutoSaveIdle => self.auto_save_idle(id),
            TimerPurpose::CursorEvent(client) => self.cursor_timer_expired(client, id),
            TimerPurpose::Settle(buffer) => self.settle_timer(buffer, id),
        }
    }

    fn handle_key(&mut self, client_id: ClientId, key: Key) {
        self.buffer_viewed(client_id);
        let client = self.client_mut(client_id);
        // Reading an input event wipes the echo area.
        client.echo.clear();
        client.signaled = false;
        if client.minibuffer.is_some() {
            self.minibuffer_key(client_id, key);
            return;
        }
        let state = &mut client.command_loop;
        if !state.pending.is_empty() && key == QUIT_KEY {
            self.signal(client_id, None, &CommandError::Quit);
            return;
        }
        if state.pending.is_empty() && state.universal_argument_map {
            state.universal_argument_map = false;
            if let Some(name) = universal_argument_binding(key, &state.prefix_arg) {
                self.run_key_command(client_id, name, key);
                return;
            }
        }
        state.pending.push(key);
        let pending = state.pending.clone();
        match self.keymap.lookup(&pending) {
            Lookup::Command(name) => self.run_key_command(client_id, name, key),
            Lookup::Prefix => self.continue_sequence(client_id),
            Lookup::Undefined => self.shift_translate_or_undefined(client_id),
        }
    }

    /// Emacs falls back to the unshifted key when a shifted one is unbound
    /// (`M-F` runs `M-f`); if that is unbound too, the original keys are
    /// reported.
    fn shift_translate_or_undefined(&mut self, client_id: ClientId) {
        let state = &mut self.client_mut(client_id).command_loop;
        let last = *state.pending.last().expect("a key was just pushed");
        if let Some(unshifted) = unshift(last) {
            let mut translated = state.pending.clone();
            *translated.last_mut().expect("non-empty") = unshifted;
            match self.keymap.lookup(&translated) {
                Lookup::Command(name) => {
                    self.client_mut(client_id).command_loop.pending = translated;
                    self.run_key_command(client_id, name, unshifted);
                    return;
                }
                Lookup::Prefix => {
                    self.client_mut(client_id).command_loop.pending = translated;
                    self.continue_sequence(client_id);
                    return;
                }
                Lookup::Undefined => {}
            }
        }
        let keys = describe_keys(&self.client_mut(client_id).command_loop.pending);
        self.undefined(client_id, keys);
    }

    /// Emacs `undefined`: ring the bell, report the keys (without the
    /// prefix-argument keys), and drop the prefix argument.
    fn undefined(&mut self, client_id: ClientId, keys: String) {
        self.ring_bell(client_id);
        let client = self.client_mut(client_id);
        client.echo = format!("{keys} is undefined");
        client.command_loop.end_command();
        client.command_loop.last_command = None;
        self.finish_command(client_id, self.journal_outbox.len());
    }

    fn continue_sequence(&mut self, client_id: ClientId) {
        self.update_echo(client_id);
    }

    /// Echo an unfinished command now if echoing has started, otherwise
    /// after `echo_keystrokes_ms` without input.
    fn update_echo(&mut self, client_id: ClientId) {
        let delay = self.config.echo_keystrokes_ms;
        let client = self.client_mut(client_id);
        let state = &client.command_loop;
        if !state.mid_command() {
            return;
        }
        if state.echoing {
            client.echo = state.echo_text();
        } else if delay > 0 {
            let id = self.set_timer(TimerPurpose::EchoKeystrokes(client_id), delay);
            self.client_mut(client_id).command_loop.echo_timer = Some(id);
        }
    }

    fn run_key_command(&mut self, client_id: ClientId, name: &'static str, key: Key) {
        let spec = command::lookup(name).expect("keymaps bind only registered commands");
        self.run_command(client_id, name, Some(key), spec.run);
    }

    /// Collects the chunks of a bracketed paste, then pastes once. A paste
    /// larger than a buffer may hold in memory is dropped whole: half of it
    /// would be worse than none.
    fn paste_chunk(&mut self, client_id: ClientId, data: Vec<u8>, last: bool) {
        let limit = self.config.in_memory_max;
        let pending = self.client_mut(client_id).paste.get_or_insert_default();
        if !pending.overflowed {
            if (pending.data.len() + data.len()) as u64 > limit {
                pending.overflowed = true;
                pending.data = Vec::new();
            } else {
                pending.data.extend_from_slice(&data);
            }
        }
        if !last {
            return;
        }
        let pending = self
            .client_mut(client_id)
            .paste
            .take()
            .expect("filled above");
        if pending.overflowed {
            let error = CommandError::User(format!(
                "Paste discarded: larger than {limit} bytes (in_memory_max)"
            ));
            self.signal(client_id, None, &error);
            return;
        }
        self.handle_paste(client_id, pending.data);
    }

    fn handle_paste(&mut self, client_id: ClientId, bytes: Vec<u8>) {
        let client = self.client_mut(client_id);
        client.echo.clear();
        client.signaled = false;
        if client.minibuffer.is_some() {
            self.minibuffer_paste(client_id, &bytes);
            return;
        }
        if !client.command_loop.pending.is_empty() {
            let keys = describe_keys(&client.command_loop.pending);
            self.undefined(client_id, format!("{keys} {PASTE_EVENT_DESCRIPTION}"));
            return;
        }
        self.run_command(client_id, PASTE_COMMAND, None, |ctx| {
            command::xterm_paste(ctx, &bytes)
        });
    }

    fn run_command(
        &mut self,
        client_id: ClientId,
        name: &'static str,
        key: Option<Key>,
        run: impl FnOnce(&mut CommandContext) -> Result<(), CommandError>,
    ) {
        let journal_start = self.journal_outbox.len();
        let state = &mut self.client_mut(client_id).command_loop;
        let prefix_arg = std::mem::take(&mut state.prefix_arg);
        let last_command = state.last_command;
        let mut context = CommandContext::new(self, client_id, name, key, prefix_arg, last_command);
        let result = run(&mut context);
        // Commands may rename themselves, as Emacs commands set
        // `this-command` (a kill becomes `kill-region`).
        let this_command = context.this_command;
        let deactivate_mark = context.deactivate_mark;
        let completion = if context.keep_prefix_state {
            Completion::KeepPrefixState(context.next_prefix_arg)
        } else {
            Completion::Ran {
                this_command: Some(this_command),
                next_prefix_arg: context.next_prefix_arg,
            }
        };
        match result {
            Ok(()) => {
                // Transient Mark mode: any buffer change sets Emacs's
                // `deactivate-mark`, which the command loop then honors.
                if deactivate_mark || self.journal_outbox.len() > journal_start {
                    let buffer = self.selected_buffer(client_id);
                    self.buffers[buffer].mark_active = false;
                }
                self.complete(client_id, completion);
            }
            Err(error) => self.signal(client_id, Some(this_command), &error),
        }
        self.finish_command(client_id, journal_start);
    }

    fn complete(&mut self, client_id: ClientId, completion: Completion) {
        let state = &mut self.client_mut(client_id).command_loop;
        match completion {
            Completion::KeepPrefixState(next_prefix_arg) => {
                state.pending.clear();
                state.prefix_arg = next_prefix_arg;
                state.universal_argument_map = true;
                self.update_echo(client_id);
            }
            Completion::Ran {
                this_command,
                next_prefix_arg,
            } => {
                state.end_command();
                // Emacs command_loop_1 updates last-command only when the
                // command left prefix-arg nil.
                if next_prefix_arg == PrefixArg::None {
                    state.last_command = this_command;
                } else {
                    state.prefix_arg = next_prefix_arg;
                    self.update_echo(client_id);
                }
            }
        }
    }

    /// An error ends the command: Emacs's `command-error` shows the message,
    /// rings the bell and drops the prefix argument. `last-command` becomes
    /// the failed command (nil when the key sequence itself was quit).
    fn signal(
        &mut self,
        client_id: ClientId,
        this_command: Option<&'static str>,
        error: &CommandError,
    ) {
        self.ring_bell(client_id);
        let client = self.client_mut(client_id);
        client.echo = error.to_string();
        client.signaled = true;
        client.command_loop.end_command();
        client.command_loop.last_command = this_command;
        if this_command.is_none() {
            self.finish_command(client_id, self.journal_outbox.len());
        }
    }

    /// Emacs `undo-auto--add-boundary` after every command, on every buffer
    /// the command changed and on the selected buffer (whose boundary point
    /// is Emacs's `point_before_last_command_or_undo`).
    fn finish_command(&mut self, client_id: ClientId, journal_start: usize) {
        let mut buffers: BTreeSet<BufferId> =
            self.journal_outbox.buffers_since(journal_start).collect();
        if !buffers.is_empty() {
            self.note_edit(client_id);
        }
        let selected = self.client(client_id).frame.selected_window().buffer;
        buffers.insert(selected);
        for buffer_id in buffers {
            let point = self.boundary_point(client_id, buffer_id);
            let buffer = &mut self.buffers[buffer_id];
            buffer.undo.boundary(point);
            if buffer.undo.take_outer_limit_warning() {
                let warning = outer_limit_warning(&buffer.name);
                self.set_echo(client_id, warning);
            }
        }
    }

    /// Point for a buffer's undo boundary: the acting client's selected
    /// window if it shows the buffer, else any window showing it. A buffer
    /// in no window has no point to restore; Emacs records none for a
    /// buffer that was not current, and 0 never differs from where a first
    /// change in a fresh buffer starts.
    fn boundary_point(&self, client_id: ClientId, buffer_id: BufferId) -> u64 {
        let acting = self.client(client_id);
        let selected = acting.frame.selected_window();
        let window = std::iter::once(selected)
            .chain(acting.frame.windows.values())
            .chain(
                self.clients
                    .values()
                    .flat_map(|client| client.frame.windows.values()),
            )
            .find(|window| window.buffer == buffer_id);
        window.map_or(0, |window| self.buffers[buffer_id].marker_pos(window.point))
    }

    fn client(&self, client_id: ClientId) -> &Client {
        self.clients
            .get(&client_id)
            .expect("events name only attached clients")
    }

    fn client_mut(&mut self, client_id: ClientId) -> &mut Client {
        self.clients
            .get_mut(&client_id)
            .expect("events name only attached clients")
    }

    pub(crate) fn selected_window_id(&self, client_id: ClientId) -> WindowId {
        self.client(client_id).frame.selected
    }

    pub(crate) fn window(&self, client_id: ClientId, window: WindowId) -> &Window {
        &self.client(client_id).frame.windows[window]
    }

    pub(crate) fn window_point(&self, client_id: ClientId, window: WindowId) -> u64 {
        let window = self.window(client_id, window);
        self.buffers[window.buffer].marker_pos(window.point)
    }

    pub(crate) fn set_window_point(&mut self, client_id: ClientId, window: WindowId, pos: u64) {
        let window = *self.window(client_id, window);
        self.buffers[window.buffer].set_marker(window.point, pos);
    }

    pub(crate) fn frame(&self, client_id: ClientId) -> &Frame {
        &self.client(client_id).frame
    }

    /// The client's frame number, and whether its modeline marks it as a
    /// client frame (Emacs `mode-line-client`). Only terminal clients have
    /// frames to render.
    pub(crate) fn frame_name(&self, client_id: ClientId) -> (u64, bool) {
        let number = self
            .client(client_id)
            .frame_number
            .expect("only terminal clients are rendered");
        (number, self.session == Session::Server)
    }

    pub(crate) fn tab_width(&self) -> usize {
        usize::try_from(self.config.tab_width).expect("tab_width is validated small")
    }

    /// A window's columns and text rows. The one window fills the frame
    /// above the modeline and echo area.
    pub(crate) fn window_size(&self, client_id: ClientId, _window: WindowId) -> (u16, u16) {
        let client = self.client(client_id);
        (client.cols, frame_layout(client.rows).text_rows)
    }

    pub(crate) fn window_start(&self, client_id: ClientId, window: WindowId) -> ScreenLine {
        let window = self.window(client_id, window);
        ScreenLine {
            line_start: self.buffers[window.buffer].marker_pos(window.start),
            row: u64::from(window.start_row_skip),
        }
    }

    pub(crate) fn set_window_start(
        &mut self,
        client_id: ClientId,
        window_id: WindowId,
        start: ScreenLine,
    ) {
        let window = &mut self.client_mut(client_id).frame.windows[window_id];
        // Only a line of over four billion rows could overflow; its window
        // then starts at the last row that can be recorded.
        window.start_row_skip = u32::try_from(start.row).unwrap_or(u32::MAX);
        let window = *window;
        self.buffers[window.buffer].set_marker(window.start, start.line_start);
    }

    /// The next frame sent to the client is whole, as after Emacs
    /// `redraw-frame`.
    pub(crate) fn request_full_redraw(&mut self, client_id: ClientId) {
        self.client_mut(client_id).grid = None;
    }

    pub(crate) fn recenter_last_op(&self, client_id: ClientId) -> Option<RecenterPosition> {
        self.client(client_id).recenter_last_op
    }

    pub(crate) fn set_recenter_last_op(&mut self, client_id: ClientId, op: RecenterPosition) {
        self.client_mut(client_id).recenter_last_op = Some(op);
    }

    pub(crate) fn buffer(&self, id: BufferId) -> &Buffer {
        &self.buffers[id]
    }

    pub(crate) fn buffer_mut(&mut self, id: BufferId) -> &mut Buffer {
        &mut self.buffers[id]
    }

    pub(crate) fn buffer_and_recorder(
        &mut self,
        id: BufferId,
    ) -> (&mut Buffer, &mut dyn ChangeRecorder) {
        (&mut self.buffers[id], &mut self.journal_outbox)
    }

    pub(crate) fn set_echo(&mut self, client_id: ClientId, text: String) {
        self.client_mut(client_id).echo = text;
    }

    pub(crate) fn echo(&self, client_id: ClientId) -> &str {
        &self.client(client_id).echo
    }

    pub(crate) fn signaled(&self, client_id: ClientId) -> bool {
        self.client(client_id).signaled
    }

    /// Only terminals have a bell.
    pub(crate) fn ring_bell(&mut self, client_id: ClientId) {
        if self.client(client_id).kind == ClientKind::Tui {
            self.effects.push(Effect::Send(client_id, ServerMsg::Bell));
        }
    }

    /// Remembers the text a yank just inserted, between `start` and `end`
    /// of the client's selected buffer, for a following `M-y`.
    pub(crate) fn set_yank_range(&mut self, client_id: ClientId, start: u64, end: u64) {
        let buffer_id = self.selected_buffer(client_id);
        if let Some(old) = self.client_mut(client_id).yank.take()
            && let Some(buffer) = self.buffers.get_mut(old.buffer)
        {
            buffer.delete_marker(old.start);
            buffer.delete_marker(old.end);
        }
        let buffer = &mut self.buffers[buffer_id];
        // Text another client inserts at either edge is not the yank's.
        let range = YankRange {
            buffer: buffer_id,
            start: buffer.create_marker_without_undo(start, InsertionType::AdvanceAfter),
            end: buffer.create_marker_without_undo(end, InsertionType::StayBefore),
        };
        self.client_mut(client_id).yank = Some(range);
    }

    /// The text the client's last yank inserted, if it is in the selected
    /// buffer. Insertions by others at its start can leave the start past
    /// the end; the range is then empty at its end.
    pub(crate) fn yank_range(&self, client_id: ClientId) -> Option<(u64, u64)> {
        let range = self.client(client_id).yank?;
        if range.buffer != self.selected_buffer(client_id) {
            return None;
        }
        let buffer = &self.buffers[range.buffer];
        let end = buffer.marker_pos(range.end);
        Some((buffer.marker_pos(range.start).min(end), end))
    }

    /// The sources a buffer's text reads, for the server's prefetch and
    /// indexing workers; `None` once the buffer is killed.
    pub fn buffer_sources(&self, buffer: BufferId) -> Option<Arc<Sources>> {
        self.buffers
            .get(buffer)
            .map(|buffer| buffer.text.sources().clone())
    }

    pub(crate) fn kill_ring(&self) -> &KillRing {
        &self.kill_ring
    }

    pub(crate) fn kill_ring_mut(&mut self) -> &mut KillRing {
        &mut self.kill_ring
    }

    pub(crate) fn temporary_goal_column(&self, client_id: ClientId) -> u64 {
        self.client(client_id).temporary_goal_column
    }

    pub(crate) fn set_temporary_goal_column(&mut self, client_id: ClientId, column: u64) {
        self.client_mut(client_id).temporary_goal_column = column;
    }

    pub(crate) fn selected_buffer(&self, client_id: ClientId) -> BufferId {
        self.client(client_id).frame.selected_window().buffer
    }
}

/// The key Emacs's shift translation tries: shift removed, or an uppercase
/// letter downcased.
fn unshift(key: Key) -> Option<Key> {
    if key.mods.shift {
        return Some(Key {
            code: key.code,
            mods: Mods {
                shift: false,
                ..key.mods
            },
        });
    }
    let KeyCode::Char(character) = key.code else {
        return None;
    };
    let mut lower = character.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(lowercase), None) if lowercase != character => Some(Key {
            code: KeyCode::Char(lowercase),
            mods: key.mods,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use emx_types::parse_kbd;

    use emx_proto::ClientKind;

    use super::*;
    use crate::buffer::FileBinding;

    const FIRST: ClientId = ClientId(1);
    const SECOND: ClientId = ClientId(2);

    fn attach(editor: &mut Editor, client: ClientId) {
        editor.handle(Event::ClientAttached {
            client,
            cols: 80,
            rows: 24,
            kind: ClientKind::Tui,
        });
    }

    fn type_keys(editor: &mut Editor, client: ClientId, keys: &str) {
        for key in parse_kbd(keys).expect("valid kbd") {
            editor.handle(Event::Key(client, key));
        }
    }

    fn point(editor: &Editor, client: ClientId) -> u64 {
        editor.window_point(client, editor.selected_window_id(client))
    }

    fn timers(effects: &[Effect]) -> Vec<(TimerId, Duration)> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::SetTimer { id, after } => Some((*id, *after)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn clients_share_buffers_but_keep_their_own_point() {
        let mut editor = Editor::new(Config::default()).expect("valid config");
        attach(&mut editor, FIRST);
        attach(&mut editor, SECOND);
        type_keys(&mut editor, FIRST, "a b c");
        assert_eq!(point(&editor, FIRST), 3);
        assert_eq!(
            point(&editor, SECOND),
            0,
            "insertion at its point leaves it before"
        );
        type_keys(&mut editor, SECOND, "C-f C-f x");
        assert_eq!(point(&editor, SECOND), 3);
        assert_eq!(point(&editor, FIRST), 4, "insertion before it moves it");
        assert_eq!(editor.client_count(), 2);
        assert_eq!(editor.echo(SECOND), "");
    }

    #[test]
    fn each_client_has_its_own_command_loop() {
        let mut editor = Editor::new(Config::default()).expect("valid config");
        attach(&mut editor, FIRST);
        attach(&mut editor, SECOND);
        type_keys(&mut editor, FIRST, "C-u");
        type_keys(&mut editor, SECOND, "a");
        type_keys(&mut editor, FIRST, "b");
        let buffer = editor.buffer(editor.selected_buffer(FIRST));
        let mut text = Vec::new();
        buffer
            .text
            .read(0..buffer.text.len(), &mut text)
            .expect("memory read");
        assert_eq!(text, b"bbbba");
    }

    #[test]
    fn journal_records_every_change() {
        use crate::journal::{JournalOp, Record};

        let mut editor = Editor::new(Config::default()).expect("valid config");
        attach(&mut editor, FIRST);
        type_keys(&mut editor, FIRST, "a C-u 2 b");
        editor.handle(Event::Paste {
            client: FIRST,
            data: b"xy".to_vec(),
            last: true,
        });
        let records: Vec<Record> = editor
            .take_effects()
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::Journal(JournalOp::Record { record, .. }) => Some(record),
                _ => None,
            })
            .collect();
        let inserted: Vec<(u64, Vec<u8>)> = records
            .into_iter()
            .filter_map(|record| match record {
                Record::Insert { at, bytes } => Some((at, bytes)),
                Record::EpochSnapshot { content } => {
                    assert!(content.is_empty(), "the base is the empty buffer");
                    None
                }
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            inserted,
            vec![(0, b"a".to_vec()), (1, b"bb".to_vec()), (3, b"xy".to_vec())]
        );
        assert!(
            !editor
                .take_effects()
                .iter()
                .any(|effect| matches!(effect, Effect::Journal(_)))
        );
    }

    #[test]
    fn last_client_detaching_starts_the_grace_period() {
        let mut editor = Editor::new(Config::default()).expect("valid config");
        attach(&mut editor, FIRST);
        attach(&mut editor, SECOND);
        editor.handle(Event::ClientDetached(FIRST));
        assert!(timers(&editor.take_effects()).is_empty());
        editor.handle(Event::ClientDetached(SECOND));
        let pending = timers(&editor.take_effects());
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1, Duration::from_millis(3000));
        editor.handle(Event::Timer(pending[0].0));
        assert_eq!(editor.take_effects(), vec![Effect::Shutdown]);
    }

    #[test]
    fn reattach_within_grace_keeps_the_server() {
        let mut editor = Editor::new(Config::default()).expect("valid config");
        attach(&mut editor, FIRST);
        editor.handle(Event::ClientDetached(FIRST));
        let pending = timers(&editor.take_effects());
        attach(&mut editor, SECOND);
        // Discard the new client's first frame.
        editor.take_effects();
        editor.handle(Event::Timer(pending[0].0));
        assert!(editor.take_effects().is_empty());
    }

    #[test]
    fn stale_grace_timer_does_not_shut_down() {
        let mut editor = Editor::new(Config::default()).expect("valid config");
        attach(&mut editor, FIRST);
        editor.handle(Event::ClientDetached(FIRST));
        let first = timers(&editor.take_effects());
        attach(&mut editor, SECOND);
        editor.handle(Event::ClientDetached(SECOND));
        let second = timers(&editor.take_effects());
        editor.handle(Event::Timer(first[0].0));
        assert!(editor.take_effects().is_empty(), "superseded timer ignored");
        editor.handle(Event::Timer(second[0].0));
        assert_eq!(editor.take_effects(), vec![Effect::Shutdown]);
    }

    #[test]
    fn unsaved_file_buffer_keeps_the_server() {
        let mut editor = Editor::new(Config::default()).expect("valid config");
        attach(&mut editor, FIRST);
        let scratch = editor.selected_buffer(FIRST);
        editor.buffer_mut(scratch).file = Some(FileBinding::new("/tmp/file".into(), None));
        type_keys(&mut editor, FIRST, "a");
        assert!(editor.has_unsaved());
        // The edit armed the auto-save idle timer; only detach's count.
        editor.take_effects();
        editor.handle(Event::ClientDetached(FIRST));
        assert!(timers(&editor.take_effects()).is_empty());
    }

    #[test]
    fn modified_buffer_without_file_is_not_unsaved() {
        let mut editor = Editor::new(Config::default()).expect("valid config");
        attach(&mut editor, FIRST);
        type_keys(&mut editor, FIRST, "a");
        assert!(!editor.has_unsaved());
    }

    #[test]
    fn detach_removes_window_markers() {
        let mut editor = Editor::new(Config::default()).expect("valid config");
        attach(&mut editor, FIRST);
        let window = *editor.window(FIRST, editor.selected_window_id(FIRST));
        editor.handle(Event::ClientDetached(FIRST));
        assert_eq!(
            editor.buffer(window.buffer).marker_position(window.point),
            None
        );
        assert_eq!(editor.client_count(), 0);
    }

    #[test]
    fn resize_updates_client_geometry() {
        let mut editor = Editor::new(Config::default()).expect("valid config");
        attach(&mut editor, FIRST);
        editor.handle(Event::Resize(FIRST, 100, 40));
        assert_eq!(editor.client_size(FIRST), (100, 40));
    }

    #[test]
    fn redisplay_waits_for_uncached_text_instead_of_reading_it() {
        use std::sync::atomic::AtomicBool;

        use emx_text::{BLOCK_SIZE, BlockCache, LineIndex, index_file};

        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("big.txt");
        let mut content = Vec::new();
        while content.len() < 2 * BLOCK_SIZE + 100 {
            content.extend_from_slice(b"0123456789abcdef\n");
        }
        std::fs::write(&path, &content).expect("write fixture");
        let cache = BlockCache::open(&path, 3 * BLOCK_SIZE).expect("open cache");
        let index = LineIndex::new(cache.len());
        index_file(&cache, &index, &AtomicBool::new(false), &mut |_| {}).expect("index");
        let text = Text::from_original(Sources::file(cache, index));

        let mut editor = Editor::new(Config::default()).expect("valid config");
        attach(&mut editor, FIRST);
        let buffer = editor.selected_buffer(FIRST);
        editor.buffer_mut(buffer).text = text.clone();
        let window = editor.selected_window_id(FIRST);
        let end = text.len();
        // The window shows the cached top of the file; point jumps to the
        // uncached end without a command reading it.
        text.read(0..1, &mut Vec::new()).expect("read block 0");
        editor.set_window_point(FIRST, window, end);
        editor.take_effects();
        editor.handle(Event::Resize(FIRST, 80, 24));
        let prefetches: Vec<PrefetchRequest> = editor
            .take_effects()
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::Prefetch(request) => Some(request),
                _ => None,
            })
            .collect();
        assert!(!prefetches.is_empty());
        assert!(
            prefetches
                .iter()
                .all(|request| request.buffer == buffer && request.block > 0),
            "only uncached blocks are requested: {prefetches:?}"
        );
        assert_eq!(
            editor.window_start(FIRST, window).line_start,
            0,
            "start kept"
        );

        let mut warm = Vec::new();
        text.read(0..end, &mut warm).expect("read the whole file");
        editor.handle(Event::Resize(FIRST, 80, 24));
        let start = editor.window_start(FIRST, window).line_start;
        assert_eq!(start, end - 11 * 17, "point's row is in the middle");
        assert!(
            !editor
                .take_effects()
                .iter()
                .any(|effect| matches!(effect, Effect::Prefetch(_)))
        );
    }

    #[test]
    fn echo_timer_for_detached_client_is_ignored() {
        let mut editor = Editor::new(Config::default()).expect("valid config");
        attach(&mut editor, FIRST);
        attach(&mut editor, SECOND);
        type_keys(&mut editor, FIRST, "C-x");
        let pending = timers(&editor.take_effects());
        editor.handle(Event::ClientDetached(FIRST));
        editor.handle(Event::Timer(pending[0].0));
        assert!(editor.take_effects().is_empty());
    }
}
