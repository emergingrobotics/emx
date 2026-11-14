#![forbid(unsafe_code)]

mod kitty;
mod screen;
mod terminal;

pub use kitty::{InputEvent, KittyDecoder, MAX_PENDING_SEQUENCE, PASTE_CHUNK_MAX};
pub use screen::{Screen, ScreenError, face_sgr};
pub use terminal::{
    InputSource, KITTY_FLAGS, ProbeResult, StdinSource, TermError, Terminal,
    UNSUPPORTED_TERMINAL_MESSAGE, probe_kitty, restore_sequence, setup_sequence,
};
