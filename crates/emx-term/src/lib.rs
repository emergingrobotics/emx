#![forbid(unsafe_code)]

mod decoder;
mod screen;
mod terminal;

pub use decoder::{Decoder, InputEvent, MAX_PENDING_SEQUENCE, PASTE_CHUNK_MAX, Protocol};
pub use screen::{Screen, ScreenError, face_sgr};
pub use terminal::{
    InputSource, KEYBOARD_ENV, KITTY_FLAGS, ProbeResult, StdinSource, TermError, Terminal,
    keyboard_override, mouse_sequence, probe_protocol, protocol_sequence, restore_sequence,
    setup_sequence,
};
