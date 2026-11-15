#![forbid(unsafe_code)]

pub mod autosave;
mod buffer;
pub mod command;
pub mod completion;
pub mod config;
pub mod diff;
mod editor;
mod event;
pub mod inplace;
pub mod io;
mod isearch;
pub mod journal;
mod keymap;
mod killring;
mod marker;
mod minibuffer;
pub mod reload;
mod render;
pub mod search_job;
pub mod testing;
mod undo;
mod window;

pub use buffer::{
    Buffer, BufferId, Change, ChangeRecorder, CodingMnemonic, EditError, FileBinding,
    IndexingProgress, SaveProgress,
};
pub use command::{CommandContext, CommandError, CommandSpec, PrefixArg};
pub use config::{Config, ConfigError, InvalidSetting, KeyboardEncoding};
pub use editor::{Editor, Session};
pub use event::{AutoSaveJob, ClientId, Effect, Event, ListingId, ReloadJob, SearchId, TimerId};
pub use journal::JournalOutbox;
pub use keymap::{Binding, Keymap, Lookup, describe_keys};
pub use killring::{KillRing, KillRingError};
pub use marker::{InsertionType, MarkerId};
pub use render::{PrefetchRequest, render};
pub use undo::{UndoEntry, UndoError, UndoLimits, UndoList, UndoOutcome, outer_limit_warning};
pub use window::{Frame, PrevBuffer, Window, WindowId};
