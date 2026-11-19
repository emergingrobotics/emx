mod buffer_menu;
mod completing;
mod files;
mod goto;
mod highlight;
mod kmacro;
mod messages;
mod mouse;
mod occurring;
mod panel;
mod rectangles;
mod registers;
mod reload;
mod replacing;
mod requests;
mod searching;
mod windows;

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use emx_proto::{ClientKind, ServerMsg, diff_grids};
use emx_text::{ColumnCheckpoints, ReadError, Sources, Text};
use emx_types::{Face, Grid, Key, KeyCode, Mods};
use slotmap::SlotMap;

use crate::buffer::{Buffer, BufferId, ChangeRecorder};
use crate::command::{
    self, CommandContext, CommandError, DabbrevState, PrefixArg, RecenterPosition,
};
use crate::config::{Config, ConfigError};
use crate::event::{ClientId, Effect, Event, TimerId};
use crate::isearch::{CaseFold, Isearch, QueuedInput};
use crate::journal::{JournalId, JournalOutbox};
use crate::keymap::{Keymap, Lookup, describe_keys, universal_argument_binding};
use crate::killring::KillRing;
use crate::marker::{InsertionType, MarkerId};
use crate::minibuffer::{Histories, Minibuffer};
use crate::render::{PrefetchRequest, render};
use crate::undo::{UndoLimits, outer_limit_warning};
pub(crate) use files::Purpose;
use files::{AutoSaveRequest, ExitWait, SaveFlow, SaveRequest};
pub(crate) use highlight::{HighlightState, LanguageState};
pub(crate) use panel::TreeData;
use registers::{RegisterRead, RegisterValue};
use reload::ReloadRequest;
use replacing::QueryReplace;
use requests::{GroupReply, OpenRequest, Reply};
use searching::SearchWait;

use crate::window::{Fetch, Frame, ScreenLine, Window, WindowArea, WindowId, not_cached_block};

const SCRATCH_BUFFER: &str = "*scratch*";
/// The one terminal frame of `emx --no-server`, like `emacs -nw`'s.
const NO_SERVER_FRAME: u64 = 1;
/// A daemon's own frame is F1, so `emacsclient -t` frames start at F2.
const FIRST_SERVER_FRAME: u64 = 2;
/// Emacs numbers every window it makes, from 1; `emacs -nw`'s own window
/// is 1 and its minibuffer window 2.
const FIRST_WINDOW_NUMBER: u64 = 1;
/// A daemon's own frame took windows 1 and 2.
const FIRST_SERVER_WINDOW_NUMBER: u64 = 3;
/// Each terminal frame makes its root window and its minibuffer window.
const WINDOWS_PER_FRAME: u64 = 2;

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
/// `suspend-frame` from a client with no terminal to give back.
const NOT_SUSPENDABLE: &str = "This client cannot be suspended";
const COPIED_NOTICE: &str = "Copied to clipboard";
/// An echo-area notice delay meaning it stays until the next input, as
/// `clipboard_notice_ms = 0` asks.
const NO_EXPIRY: u64 = 0;
const QUIT_KEY: Key = command::QUIT_KEY;

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
    /// Rows the minibuffer's candidate list takes from the windows.
    candidate_rows: u16,
    isearch: Option<Isearch>,
    query_replace: Option<QueryReplace>,
    /// A search running on a worker for this client.
    search_wait: Option<SearchWait>,
    /// The keyboard macro being defined, as the input keys read so far
    /// (Emacs `defining-kbd-macro`).
    macro_defining: Option<Vec<Key>>,
    /// Depth of keyboard-macro execution; recording and the repeat key pause
    /// while it is above zero (Emacs `executing-kbd-macro`).
    macro_executing: u32,
    /// The key that repeats the last macro after `C-x e`, as Emacs installs
    /// a transient map on the last event of `kmacro-end-and-call-macro`.
    macro_repeat: Option<Key>,
    /// A register name being read for `C-x r ...`.
    register_read: Option<RegisterRead>,
    /// Cycling state for successive `dabbrev-expand` (`M-/`) presses.
    dabbrev: Option<DabbrevState>,
    /// A `goto-line` waiting for the background indexer to reach a line that
    /// lies beyond the counted prefix.
    pending_goto: Option<PendingGoto>,
    /// The mouse drag the last press started, until its release.
    mouse_drag: Option<mouse::MouseDrag>,
    /// A notice in the echo area drawn in its own face.
    echo_flash: Option<EchoFlash>,
    /// A `project-find-file` waiting for the project's files.
    quick_open: Option<panel::QuickOpen>,
}

/// A notice shown in the echo area in its own face. The face applies only
/// while the echo area still shows the notice's text, so any later message
/// is drawn plainly without every writer of the echo area clearing this.
#[derive(Debug)]
struct EchoFlash {
    text: String,
    face: Face,
    /// Clears the notice; `None` leaves it to the next input.
    timer: Option<TimerId>,
}

/// A `goto-line` whose target line is not yet indexed: the jump completes
/// once the indexer counts past it, or is dropped by `C-g`.
#[derive(Debug, Clone, Copy)]
struct PendingGoto {
    buffer: BufferId,
    window: WindowId,
    /// The 1-based line asked for.
    line: i64,
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
    /// Clears the client's echo-area notice, if this is still its timer.
    EchoFlash(ClientId),
    /// Emacs `auto-save-timeout`.
    AutoSaveIdle,
    /// The client's cursor may be reported to `emx events` again.
    CursorEvent(ClientId),
    /// A settling file is looked at again.
    Settle(BufferId),
    /// The automatic reloads of one settle round are summarized.
    ReloadSummary,
    /// A client's file panel lists changed directories again.
    TreeRefresh(ClientId),
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
    /// The next Emacs window sequence number; every client's windows
    /// share the count, as one Emacs process's do.
    next_window_number: u64,
    /// Names the next `Effect::ListDirectory`.
    next_listing: u64,
    /// Emacs `window_select_count`, the clock of `window-use-time`.
    window_select_count: u64,
    /// `Buffer-menu-mode-map`, active in Buffer Menu buffers.
    buffer_menu_keymap: Keymap,
    /// Saves waiting for a look at a target changed under the buffer's
    /// file-backed text, and who asked.
    save_verify: HashMap<BufferId, (ClientId, SaveFlow)>,
    /// `isearch-mode-map` and `occur-mode-map`.
    isearch_keymap: Keymap,
    occur_keymap: Keymap,
    /// The file panel's map.
    tree_keymap: Keymap,
    /// Names the next `Effect::StartSearch`.
    next_search: u64,
    /// Emacs `search-ring` and `regexp-search-ring`, newest first.
    search_ring: VecDeque<Vec<u8>>,
    regexp_search_ring: VecDeque<Vec<u8>>,
    /// The case folding the last isearch ended with, which the next one's
    /// `C-s C-s` restores (`isearch-last-case-fold-search`).
    isearch_case_fold: CaseFold,
    /// Emacs `query-replace-history`, `query-replace-defaults` and
    /// `occur-collect-regexp-history`, newest first.
    query_replace_history: Vec<Vec<u8>>,
    query_replace_defaults: Vec<(Vec<u8>, Vec<u8>)>,
    occur_collect_history: Vec<Vec<u8>>,
    /// Visited paths the server watches (`Effect::Watch`).
    watched: BTreeSet<PathBuf>,
    /// Watched paths that are polled instead.
    polled_paths: BTreeSet<PathBuf>,
    /// The settle round of automatic reloads being summarized: the names
    /// reloaded so far, its timer, and how often it was extended.
    reload_round: Option<reload::ReloadRound>,
    /// Emacs registers, keyed by their name character.
    registers: HashMap<char, RegisterValue>,
    /// Emacs `last-kbd-macro`, the keys of the most recent macro.
    last_kbd_macro: Option<Vec<Key>>,
    /// Emacs `killed-rectangle`, one line of the rectangle per entry.
    killed_rectangle: Option<Vec<Vec<u8>>>,
    /// Emacs `xterm-mouse-mode`: terminal clients report the mouse.
    mouse: bool,
    /// Each client's file panel buffer.
    trees: BTreeMap<ClientId, BufferId>,
    /// The width the next panel opens with, its border included: the
    /// config's, until a panel is resized.
    panel_width: u16,
    /// The kill ring generation last sent to a terminal's clipboard.
    clipboard_generation: u64,
    /// The current event made a copy that is announced once it reaches the
    /// clipboard.
    clipboard_notice: bool,
    /// The terminal client whose input the current event is.
    input_client: Option<ClientId>,
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
        // The working directory seeds file prompts. A failure to read it (the
        // directory was removed, or is unreadable) is an environment fault
        // surfaced at startup, not silently masked by rooting prompts at "/".
        let default_directory =
            std::env::current_dir().map_err(|source| ConfigError::WorkingDirectory { source })?;
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
            default_directory,
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
            next_listing: 0,
            next_window_number: match session {
                Session::NoServer => FIRST_WINDOW_NUMBER,
                Session::Server => FIRST_SERVER_WINDOW_NUMBER,
            },
            window_select_count: 0,
            buffer_menu_keymap: Keymap::buffer_menu(),
            isearch_keymap: Keymap::isearch(),
            occur_keymap: Keymap::occur(),
            tree_keymap: Keymap::tree(),
            next_search: 0,
            search_ring: VecDeque::new(),
            regexp_search_ring: VecDeque::new(),
            isearch_case_fold: CaseFold::Smart,
            query_replace_history: Vec::new(),
            query_replace_defaults: Vec::new(),
            occur_collect_history: Vec::new(),
            save_verify: HashMap::new(),
            watched: BTreeSet::new(),
            polled_paths: BTreeSet::new(),
            reload_round: None,
            registers: HashMap::new(),
            last_kbd_macro: None,
            killed_rectangle: None,
            mouse: config.mouse,
            trees: BTreeMap::new(),
            panel_width: u16::try_from(config.panel_width).expect("panel_width is validated small"),
            clipboard_generation: 0,
            clipboard_notice: false,
            input_client: None,
            config,
        };
        editor.scratch = editor.create_buffer(SCRATCH_BUFFER);
        editor.track_journal(editor.scratch);
        editor.create_messages_buffer();
        Ok(editor)
    }

    fn create_buffer(&mut self, name: &str) -> BufferId {
        let limits = UndoLimits::from(&self.config);
        let text = Text::empty(Sources::memory(Arc::from(Vec::new())));
        let checkpoints = self.new_checkpoints();
        let id = self
            .buffers
            .insert_with_key(|id| Buffer::new(id, name.to_string(), text, limits, checkpoints));
        self.buffers[id].settings = self.config.file_settings(None);
        // Creating a buffer is a command's change: it gets its boundary.
        self.buffers[id].undo.boundary(0);
        self.recency.push(id);
        id
    }

    /// A fresh column-checkpoint cache sized by the current configuration.
    pub(crate) fn new_checkpoints(&self) -> ColumnCheckpoints {
        ColumnCheckpoints::new(
            self.tab_width(),
            self.config.column_checkpoint_interval,
            self.config.long_line_threshold,
        )
    }

    pub fn handle(&mut self, event: Event) {
        self.journal_outbox.begin_event();
        self.input_client = match &event {
            Event::Key(client, _) | Event::Mouse(client, _) => Some(*client),
            Event::Paste { client, .. } => Some(*client),
            _ => None,
        };
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
            Event::Mouse(client, event) => {
                self.note_terminal_activity(client);
                self.handle_mouse(client, event);
            }
            Event::Tree { client, outcome } => {
                if self.clients.contains_key(&client) {
                    self.tree_result(client, outcome);
                }
            }
            Event::TreeChanged { dir } => self.tree_changed(&dir),
            Event::Highlighted {
                buffer,
                generation,
                range,
                spans,
            } => self.highlighted(buffer, generation, range, spans),
            Event::Paste { client, data, last } => {
                self.note_terminal_activity(client);
                self.paste_chunk(client, data, last);
            }
            Event::Resize(client_id, cols, rows) => {
                let client = self.client_mut(client_id);
                client.cols = cols;
                client.rows = rows;
                client.candidate_rows = 0;
                client.frame.resize(cols, rows);
                self.fit_frame(client_id);
            }
            Event::Request(client, id, request) => self.handle_request(client, id, request),
            Event::Redraw(client) => self.request_full_redraw(client),
            Event::BlockReady { buffer, result, .. } => self.block_ready(buffer, result),
            Event::IndexProgress {
                buffer,
                indexed_blocks,
                total_blocks,
                complete,
            } => self.index_progress(buffer, indexed_blocks, total_blocks, complete),
            Event::IndexFinished { buffer, result } => self.index_finished(buffer, result),
            Event::SaveProgress {
                buffer,
                written_bytes,
                total_bytes,
            } => self.save_progress(buffer, written_bytes, total_bytes),
            Event::Timer(id) => self.handle_timer(id),
            Event::FileLoaded { buffer, result } => self.file_loaded(buffer, result),
            Event::SaveFinished { buffer, result } => {
                self.save_finished(buffer, result);
                self.files_changed_for_trees();
            }
            Event::AutoSaved { buffer, result } => self.auto_saved(buffer, result),
            Event::AutoSaveRead { buffer, result } => self.auto_save_read(buffer, result),
            Event::Recovered(buffers) => self.recovered(buffers),
            Event::FileChanged { path } => self.file_changed(&path),
            Event::FileRenamed { from, to } => self.file_renamed(&from, &to),
            Event::FilePolled { path, notice } => self.file_polled(path, notice),
            Event::FileWatched { path } => {
                self.polled_paths.remove(&path);
            }
            Event::Observed { buffer, result } => self.observed(buffer, result),
            Event::Reloaded { buffer, result } => {
                self.reloaded(buffer, result);
                self.files_changed_for_trees();
            }
            Event::AppendRead { buffer, result } => self.append_read(buffer, result),
            Event::DiffReady {
                requester, result, ..
            } => self.diff_ready(requester, result),
            Event::DirectoryListed { client, id, result } => {
                self.directory_listed(client, id, result);
            }
            Event::SearchResult { client, id, result } => self.search_result(client, id, result),
        }
        self.send_clipboard();
        self.sync_watches();
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
            self.fit_frame(*client_id);
            self.keep_points_visible(*client_id);
        }
        self.request_highlights();
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
        self.refit_window_start(client_id, window_id)?;
        self.auto_hscroll(client_id, window_id)?;
        let (_, height) = self.window_size(client_id, window_id);
        let window = *self.window(client_id, window_id);
        let buffer = &self.buffers[window.buffer];
        let text = buffer.text.clone();
        let point = buffer.marker_pos(window.point);
        let start = self.window_start(client_id, window_id);
        let layout = self.window_layout(client_id, window_id, &text, Fetch::CachedOnly);
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
        self.show_echo_to(&clients, message);
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
        // A terminal frame numbers its root and minibuffer windows; other
        // clients have no Emacs frame and their window is never named.
        let number = if kind == ClientKind::Tui {
            let number = self.next_window_number;
            self.next_window_number += WINDOWS_PER_FRAME;
            number
        } else {
            0
        };
        let buffer = &mut self.buffers[buffer_id];
        let window = Window {
            buffer: buffer_id,
            point: buffer.create_marker_without_undo(0, InsertionType::StayBefore),
            start: buffer.create_marker_without_undo(0, InsertionType::StayBefore),
            start_row_skip: 0,
            start_cols: cols,
            hscroll: 0,
            number,
            use_time: 0,
        };
        self.clients.insert(
            client,
            Client {
                cols,
                rows,
                kind,
                frame_number,
                frame: Frame::new(window, cols, rows),
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
                candidate_rows: 0,
                isearch: None,
                query_replace: None,
                search_wait: None,
                macro_defining: None,
                macro_executing: 0,
                macro_repeat: None,
                register_read: None,
                dabbrev: None,
                pending_goto: None,
                mouse_drag: None,
                echo_flash: None,
                quick_open: None,
            },
        );
        if kind == ClientKind::Tui && self.mouse {
            self.effects
                .push(Effect::Send(client, ServerMsg::Modes { mouse: true }));
        }
        if kind == ClientKind::Tui {
            let selected = self.selected_window_id(client);
            self.bump_use_time(client, selected);
            self.note_terminal_activity(client);
            self.client_attached_event(client);
        }
    }

    fn detach(&mut self, client: ClientId) {
        self.cancel_search(client);
        if let Some(panel) = self.frame(client).panel()
            && self.frame(client).selected == panel.window
        {
            let main = self.main_window(client);
            self.client_mut(client).frame.selected = main;
        }
        if let Some(buffer) = self.trees.remove(&client) {
            self.forget_tree(buffer);
            if let Some((window, history)) = self.client_mut(client).frame.close_panel() {
                self.discard_window(window, history);
            }
            self.kill_buffer(buffer);
        }
        let mut removed = self
            .clients
            .remove(&client)
            .expect("the server detaches only attached clients");
        if let Some(isearch) = removed.isearch.take() {
            self.isearch_detach(isearch);
        }
        if let Some(session) = removed.query_replace.take() {
            self.replace_detach(session);
        }
        for (id, window) in &removed.frame.windows {
            let buffer = &mut self.buffers[window.buffer];
            buffer.delete_marker(window.point);
            buffer.delete_marker(window.start);
            for entry in removed.frame.prev_buffers.get(id).into_iter().flatten() {
                let buffer = &mut self.buffers[entry.buffer];
                buffer.delete_marker(entry.start);
                buffer.delete_marker(entry.point);
            }
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

    /// A new kill goes to the system clipboard of the terminal whose input
    /// made it (DESIGN section 7: kill ring to clipboard only). A copy asked
    /// to be announced flashes a notice, or warns when it is too large to
    /// send.
    fn send_clipboard(&mut self) {
        let notice = std::mem::take(&mut self.clipboard_notice);
        let generation = self.kill_ring.generation();
        if generation == self.clipboard_generation {
            return;
        }
        self.clipboard_generation = generation;
        if !self.config.osc52_clipboard {
            return;
        }
        let Some(client) = self.input_client else {
            return;
        };
        let Some(text) = self.kill_ring.newest() else {
            return;
        };
        if self
            .clients
            .get(&client)
            .is_none_or(|state| state.kind != ClientKind::Tui)
        {
            return;
        }
        let length = text.len() as u64;
        let limit = self.config.osc52_max_bytes;
        if length > limit {
            if notice {
                let message = format!(
                    "Not copied to clipboard: {length} bytes is over osc52_max_bytes ({limit})"
                );
                self.flash_echo(client, message, Face::Warning, NO_EXPIRY);
            }
            return;
        }
        let data_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, text);
        self.effects
            .push(Effect::Send(client, ServerMsg::Clipboard { data_b64 }));
        if notice {
            let delay = self.config.clipboard_notice_ms;
            self.flash_echo(client, COPIED_NOTICE.to_string(), Face::Success, delay);
        }
    }

    pub(crate) fn request_clipboard_notice(&mut self) {
        self.clipboard_notice = true;
    }

    /// Shows `text` in the echo area in `face`, cleared after `delay_ms`
    /// unless that is `NO_EXPIRY`, which leaves it to the next input.
    fn flash_echo(&mut self, client_id: ClientId, text: String, face: Face, delay_ms: u64) {
        self.set_echo(client_id, text.clone());
        let timer = (delay_ms != NO_EXPIRY)
            .then(|| self.set_timer(TimerPurpose::EchoFlash(client_id), delay_ms));
        self.client_mut(client_id).echo_flash = Some(EchoFlash { text, face, timer });
    }

    fn echo_flash_expired(&mut self, client_id: ClientId, id: TimerId) {
        let Some(client) = self.clients.get_mut(&client_id) else {
            return;
        };
        let Some(flash) = &client.echo_flash else {
            return;
        };
        if flash.timer != Some(id) {
            return;
        }
        if client.echo == flash.text {
            client.echo.clear();
        }
        client.echo_flash = None;
    }

    /// The face the echo area's message is drawn in.
    pub(crate) fn echo_face(&self, client_id: ClientId) -> Face {
        let client = self.client(client_id);
        match &client.echo_flash {
            Some(flash) if client.minibuffer.is_none() && client.echo == flash.text => flash.face,
            _ => Face::Default,
        }
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
            TimerPurpose::EchoFlash(client) => self.echo_flash_expired(client, id),
            TimerPurpose::AutoSaveIdle => self.auto_save_idle(id),
            TimerPurpose::CursorEvent(client) => self.cursor_timer_expired(client, id),
            TimerPurpose::Settle(buffer) => self.settle_timer(buffer, id),
            TimerPurpose::ReloadSummary => self.reload_summary_timer(id),
            TimerPurpose::TreeRefresh(client) => self.tree_refresh_timer(client),
        }
    }

    fn handle_key(&mut self, client_id: ClientId, key: Key) {
        // A key typed while isearch or query-replace waits for a search on
        // a worker waits too, so it acts on the search's result.
        if self.hold_input_for_search(client_id, QueuedInput::Key(key)) {
            return;
        }
        self.buffer_viewed(client_id);
        let client = self.client_mut(client_id);
        // Reading an input event wipes the echo area.
        client.echo.clear();
        client.signaled = false;
        // Every key read while a macro is being defined joins it, as Emacs
        // records them; the start and end commands trim their own keys.
        if client.macro_executing == 0
            && let Some(recording) = client.macro_defining.as_mut()
        {
            recording.push(key);
        }
        // A pending register-name read (`C-x r ...`) takes this key.
        if self.client(client_id).register_read.is_some() {
            self.read_register_key(client_id, key);
            return;
        }
        if self.client(client_id).minibuffer.is_some() {
            self.minibuffer_key(client_id, key);
            return;
        }
        if self.query_replace_active(client_id) {
            self.query_replace_key(client_id, key);
            return;
        }
        // The transient map `C-x e` installs: its last event repeats the
        // macro until some other key is read.
        if self.client(client_id).macro_executing == 0
            && let Some(repeat_key) = self.client(client_id).macro_repeat
        {
            let ready = {
                let state = &self.client(client_id).command_loop;
                state.pending.is_empty() && !state.universal_argument_map
            };
            if key == repeat_key && ready {
                self.repeat_last_macro(client_id, key);
                return;
            }
            self.client_mut(client_id).macro_repeat = None;
        }
        let client = self.client_mut(client_id);
        let state = &mut client.command_loop;
        if !state.pending.is_empty() && key == QUIT_KEY {
            self.abort_input(client_id);
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
        if self.isearch_active(client_id) {
            self.isearch_key(client_id, key);
            return;
        }
        self.client_mut(client_id).command_loop.pending.push(key);
        self.dispatch_pending(client_id, key);
    }

    /// Looks up the key sequence read so far, ending in `key`, and runs it.
    fn dispatch_pending(&mut self, client_id: ClientId, key: Key) {
        let pending = self.client(client_id).command_loop.pending.clone();
        match self.lookup_keys(client_id, &pending) {
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
            match self.lookup_keys(client_id, &translated) {
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
        let message = format!("{keys} is undefined");
        self.log_message(&message);
        let client = self.client_mut(client_id);
        client.echo = message;
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
        if self.hold_input_for_search(client_id, QueuedInput::Paste(bytes.clone())) {
            return;
        }
        let client = self.client_mut(client_id);
        client.echo.clear();
        client.signaled = false;
        if client.minibuffer.is_some() {
            self.minibuffer_paste(client_id, &bytes);
            return;
        }
        if self.query_replace_active(client_id) {
            // A paste is not an answer: it ends query-replace and inserts.
            self.query_replace_paste(client_id);
        }
        if self.isearch_active(client_id) && self.client(client_id).command_loop.pending.is_empty()
        {
            self.isearch_paste(client_id, bytes);
            return;
        }
        let client = self.client_mut(client_id);
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
        let macro_call = context.macro_call;
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
        // `kmacro-end-and-call-macro` runs the macro only once its own
        // command loop bookkeeping is done, so the replayed keys start from
        // a clean command state.
        if let Some(count) = macro_call
            && !self.client(client_id).signaled
        {
            self.call_last_macro(client_id, count, key);
        }
    }

    /// Emacs `keyboard-quit` and a `C-g` that aborts a partial key sequence:
    /// deactivate the mark and discard any macro definition, repeat key and
    /// register read in progress.
    fn abort_input(&mut self, client_id: ClientId) {
        let buffer = self.selected_buffer(client_id);
        self.buffers[buffer].mark_active = false;
        let client = self.client_mut(client_id);
        client.macro_defining = None;
        client.macro_repeat = None;
        client.register_read = None;
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
        let message = client.echo.clone();
        client.command_loop.end_command();
        client.command_loop.last_command = this_command;
        self.log_message(&message);
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

    /// The tab width the buffer's text is laid out with.
    pub(crate) fn buffer_tab_width(&self, buffer: BufferId) -> usize {
        usize::try_from(self.buffers[buffer].settings.tab_width)
            .expect("tab_width is validated small")
    }

    pub(crate) fn tab_width(&self) -> usize {
        usize::try_from(self.config.tab_width).expect("tab_width is validated small")
    }

    /// A window's text columns and rows.
    pub(crate) fn window_size(&self, client_id: ClientId, window: WindowId) -> (u16, u16) {
        let area = self.window_area(client_id, window);
        (area.cols, area.text_rows)
    }

    /// Where the window draws: its place on the frame, less its border,
    /// modeline and any header line its buffer has.
    pub(crate) fn window_area(&self, client_id: ClientId, window: WindowId) -> WindowArea {
        let frame = &self.client(client_id).frame;
        let buffer = &self.buffers[frame.windows[window].buffer];
        frame
            .window_box(window)
            .area(frame.width(), buffer.menu.is_some())
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
        let (cols, _) = self.window_size(client_id, window_id);
        let window = &mut self.client_mut(client_id).frame.windows[window_id];
        // Only a line of over four billion rows could overflow; its window
        // then starts at the last row that can be recorded.
        window.start_row_skip = u32::try_from(start.row).unwrap_or(u32::MAX);
        window.start_cols = cols;
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

    /// Emacs `message`: shown in the client's echo area and logged in
    /// `*Messages*`.
    pub(crate) fn set_echo(&mut self, client_id: ClientId, text: String) {
        self.log_message(&text);
        self.client_mut(client_id).echo = text;
    }

    /// One message for several clients' echo areas, logged once.
    pub(crate) fn show_echo_to(&mut self, clients: &[ClientId], text: String) {
        self.log_message(&text);
        for client in clients {
            self.client_mut(*client).echo = text.clone();
        }
    }

    pub(crate) fn echo(&self, client_id: ClientId) -> &str {
        &self.client(client_id).echo
    }

    pub(crate) fn signaled(&self, client_id: ClientId) -> bool {
        self.client(client_id).signaled
    }

    /// Only terminals can be suspended; other clients are told so.
    pub(crate) fn suspend_client(&mut self, client_id: ClientId) {
        if self.client(client_id).kind == ClientKind::Tui {
            self.effects
                .push(Effect::Send(client_id, ServerMsg::Suspend));
        } else {
            self.set_echo(client_id, NOT_SUSPENDABLE.to_string());
        }
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

    /// Removes and returns the client's `dabbrev-expand` cycling state.
    pub(crate) fn take_dabbrev(&mut self, client_id: ClientId) -> Option<DabbrevState> {
        self.client_mut(client_id).dabbrev.take()
    }

    pub(crate) fn set_dabbrev(&mut self, client_id: ClientId, state: DabbrevState) {
        self.client_mut(client_id).dabbrev = Some(state);
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
