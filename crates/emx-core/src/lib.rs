#![forbid(unsafe_code)]

mod buffer;
pub mod config;
mod marker;
mod undo;

pub use buffer::{Buffer, BufferId, Change, ChangeRecorder, EditError, FileBinding};
pub use config::{Config, ConfigError, KeyboardEncoding};
pub use marker::{InsertionType, MarkerId};
pub use undo::{UndoEntry, UndoError, UndoLimits, UndoList, UndoOutcome, outer_limit_warning};
