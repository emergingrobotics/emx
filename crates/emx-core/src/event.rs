use std::time::Duration;

use emx_proto::{ClientKind, ServerMsg};
use emx_types::Key;

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
    Shutdown,
}
