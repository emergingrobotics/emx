use std::path::PathBuf;
use std::time::Duration;

use emx_proto::{ClientKind, Request, ServerMsg};
use emx_text::{FileIdentity, Text};
use emx_types::Key;

use crate::buffer::BufferId;
use crate::diff::{DiffJob, DiffRequester};
use crate::io::{Loaded, SaveFailure, SavePlan, SavedFile};
use crate::journal::{JournalOp, RecoveredBuffer};
use crate::reload::{AppendRead, Observation, Reloaded};
use crate::render::PrefetchRequest;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClientId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TimerId(pub u64);

/// Input to the editor state machine. The server (or the test harness)
/// turns sockets, worker results and timers into these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    ClientAttached {
        client: ClientId,
        cols: u16,
        rows: u16,
        kind: ClientKind,
    },
    ClientDetached(ClientId),
    Key(ClientId, Key),
    /// A chunk of a bracketed paste. The chunks are collected until the
    /// `last` one, then inserted as one change, never looked up as keys.
    Paste {
        client: ClientId,
        data: Vec<u8>,
        last: bool,
    },
    Resize(ClientId, u16, u16),
    /// A socket API request; answered with `ServerMsg::Response` carrying
    /// the same id.
    Request(ClientId, u64, Request),
    /// The client could not use its last frame: send the next one whole.
    Redraw(ClientId),
    /// A timer requested by `Effect::SetTimer` expired. Timers are never
    /// cancelled, so the editor ignores ones that no longer apply.
    Timer(TimerId),
    /// The result of an `Effect::Load`, as `io::load_file` returned it.
    FileLoaded {
        buffer: BufferId,
        result: Result<Loaded, String>,
    },
    /// The result of an `Effect::Save`: the version of the file written.
    SaveFinished {
        buffer: BufferId,
        result: Result<SavedFile, SaveFailure>,
    },
    /// The watcher (or poller) saw something happen to `path`.
    FileChanged {
        path: PathBuf,
    },
    /// The watcher saw `from` renamed to `to`.
    FileRenamed {
        from: PathBuf,
        to: PathBuf,
    },
    /// The result of an `Effect::Observe`, as `reload::observe` returned it.
    Observed {
        buffer: BufferId,
        result: Result<Observation, String>,
    },
    /// The result of an `Effect::Reload`, as `reload::reload_file` returned
    /// it.
    Reloaded {
        buffer: BufferId,
        result: Result<Reloaded, String>,
    },
    /// The result of an `Effect::ReadAppend`, as `reload::read_appended`
    /// returned it.
    AppendRead {
        buffer: BufferId,
        result: Result<AppendRead, String>,
    },
    /// The result of an `Effect::Diff`, as `diff::run_diff` returned it.
    DiffReady {
        buffer: BufferId,
        requester: DiffRequester,
        result: Result<String, String>,
    },
    /// The result of an `Effect::AutoSave`.
    AutoSaved {
        buffer: BufferId,
        result: Result<(), String>,
    },
    /// The content of an auto-save file asked for by `Effect::ReadAutoSave`.
    AutoSaveRead {
        buffer: BufferId,
        result: Result<Vec<u8>, String>,
    },
    /// What `journal::replay_dir` rebuilt from the previous session's
    /// journals, delivered once at startup.
    Recovered(Vec<RecoveredBuffer>),
    /// A block asked for by `Effect::Prefetch` was read into the cache (or
    /// could not be), so the frames that needed it can be drawn.
    BlockReady {
        buffer: BufferId,
        block: u64,
        result: Result<(), String>,
    },
    /// The line indexer of an `Effect::StartIndex` has counted the lines of
    /// `indexed_blocks` of `total_blocks` blocks.
    IndexProgress {
        buffer: BufferId,
        indexed_blocks: u64,
        total_blocks: u64,
    },
    /// The line indexer stopped: done, or the file could not be read.
    IndexFinished {
        buffer: BufferId,
        result: Result<(), String>,
    },
}

/// Output of the editor state machine, carried out by the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Send(ClientId, ServerMsg),
    /// Deliver `Event::Timer(id)` once `after` has elapsed.
    SetTimer {
        id: TimerId,
        after: Duration,
    },
    /// Read a cache block that redisplay needed (from the buffer's
    /// `Editor::buffer_sources`), then deliver `Event::BlockReady` so the
    /// frame is rendered again. Repeated after every event until the block
    /// is cached, so the server keeps one read per block in flight.
    Prefetch(PrefetchRequest),
    /// Read `path` with `io::load_file` and deliver `Event::FileLoaded`.
    Load {
        buffer: BufferId,
        path: PathBuf,
    },
    /// Carry out the plan with `io::save` and deliver `Event::SaveFinished`.
    /// The buffer stays read-only until then.
    Save {
        buffer: BufferId,
        plan: SavePlan,
    },
    /// Build the line index of a buffer whose original is read through the
    /// block cache (`Editor::buffer_sources`), reporting
    /// `Event::IndexProgress` and `Event::IndexFinished`.
    StartIndex {
        buffer: BufferId,
    },
    /// Write `autosave::write_auto_save(file, text)` and deliver
    /// `Event::AutoSaved`.
    AutoSave(AutoSaveJob),
    /// Delete the auto-save file of `file` (`autosave::remove_auto_save`).
    RemoveAutoSave {
        file: PathBuf,
    },
    /// Read the auto-save file of `file` and deliver `Event::AutoSaveRead`.
    ReadAutoSave {
        buffer: BufferId,
        file: PathBuf,
    },
    /// Look at the visited file with `reload::observe` and deliver
    /// `Event::Observed`.
    Observe {
        buffer: BufferId,
        path: PathBuf,
        old_len: Option<u64>,
    },
    /// Read the file again with `reload::reload_file` and deliver
    /// `Event::Reloaded`.
    Reload(ReloadJob),
    /// Read the bytes an append added with `reload::read_appended` and
    /// deliver `Event::AppendRead`.
    ReadAppend {
        buffer: BufferId,
        path: PathBuf,
        old: FileIdentity,
        new: FileIdentity,
        old_tail_hash: u64,
    },
    /// Diff with `diff::run_diff` and deliver `Event::DiffReady`.
    Diff(DiffJob),
    /// Apply the operation with `JournalWriter::apply`. Journal effects
    /// must be applied in order, before the effects that follow them are
    /// carried out: a snapshot reads the buffer text, which a later save
    /// may rewrite.
    Journal(JournalOp),
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct AutoSaveJob {
    pub buffer: BufferId,
    /// The visited file; the auto-save file is named after it.
    pub file: PathBuf,
    pub text: Text,
}

impl PartialEq for AutoSaveJob {
    fn eq(&self, other: &Self) -> bool {
        self.buffer == other.buffer && self.file == other.file && self.text.same_root(&other.text)
    }
}

impl Eq for AutoSaveJob {}

#[derive(Debug, Clone)]
pub struct ReloadJob {
    pub buffer: BufferId,
    pub path: PathBuf,
    /// The text being replaced, whose anchors are mapped onto the new one.
    pub previous: Text,
    /// Window points and starts in `previous`.
    pub anchors: Vec<u64>,
}

impl PartialEq for ReloadJob {
    fn eq(&self, other: &Self) -> bool {
        self.buffer == other.buffer
            && self.path == other.path
            && self.previous.same_root(&other.previous)
            && self.anchors == other.anchors
    }
}

impl Eq for ReloadJob {}
