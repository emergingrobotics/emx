use std::path::PathBuf;
use std::time::Duration;

use emx_proto::{ClientKind, ServerMsg};
use emx_text::{FileIdentity, Text};
use emx_types::Key;

use crate::buffer::BufferId;
use crate::io::{Loaded, SaveFailure, SavePlan};
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
    /// Bracketed paste: inserted as one change, never looked up as keys.
    Paste(ClientId, Vec<u8>),
    Resize(ClientId, u16, u16),
    /// A timer requested by `Effect::SetTimer` expired. Timers are never
    /// cancelled, so the editor ignores ones that no longer apply.
    Timer(TimerId),
    /// The result of an `Effect::Load`, as `io::load_file` returned it.
    FileLoaded {
        buffer: BufferId,
        result: Result<Loaded, String>,
    },
    /// The result of an `Effect::Save`: the identity of the file written.
    SaveFinished {
        buffer: BufferId,
        result: Result<FileIdentity, SaveFailure>,
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
    /// Read a cache block that redisplay needed, then deliver an event so
    /// the frame is rendered again. Repeated after every event until the
    /// block is cached, so the server keeps one read per block in flight.
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
    /// block cache.
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
