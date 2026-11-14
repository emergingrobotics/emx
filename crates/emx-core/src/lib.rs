#![forbid(unsafe_code)]

mod buffer;
pub mod command;
pub mod config;
mod editor;
mod event;
mod keymap;
mod killring;
mod marker;
mod render;
pub mod testing;
mod undo;
mod window;

pub use buffer::{Buffer, BufferId, Change, ChangeRecorder, EditError, FileBinding};
pub use command::{CommandContext, CommandError, CommandSpec, PrefixArg};
pub use config::{Config, ConfigError, InvalidSetting, KeyboardEncoding};
pub use editor::{Editor, JournalOutbox, Session};
pub use event::{ClientId, Effect, Event, TimerId};
pub use keymap::{Binding, Keymap, Lookup, describe_keys};
pub use killring::{KillRing, KillRingError};
pub use marker::{InsertionType, MarkerId};
pub use render::{PrefetchRequest, render};
pub use undo::{UndoEntry, UndoError, UndoLimits, UndoList, UndoOutcome, outer_limit_warning};
pub use window::{Frame, Window, WindowId};
