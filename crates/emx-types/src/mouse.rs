use serde::{Deserialize, Serialize};

use crate::key::Mods;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
    /// No button: a release in a report that does not say which, or motion
    /// with nothing held.
    None,
}

impl MouseButton {
    pub fn is_wheel(self) -> bool {
        matches!(
            self,
            MouseButton::WheelUp
                | MouseButton::WheelDown
                | MouseButton::WheelLeft
                | MouseButton::WheelRight
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseKind {
    Press,
    Release,
    /// Motion with a button held.
    Drag,
    /// Motion with no button held; only reported in any-event tracking.
    Move,
}

/// A mouse report at a terminal cell, 0-based. `clicks` counts presses of
/// the same button on the same cell in quick succession (2 for a double
/// click); the client counts them, so the editor needs no clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MouseEvent {
    pub kind: MouseKind,
    pub button: MouseButton,
    pub col: u16,
    pub row: u16,
    pub mods: Mods,
    pub clicks: u8,
}
