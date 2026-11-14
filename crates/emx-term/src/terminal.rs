//! Terminal setup, restore and the Kitty keyboard protocol probe.
//!
//! The escape sequences are built by pure functions so they can be tested;
//! the code that touches the TTY (raw mode, writing stdout, polling stdin) is
//! kept as small as possible because it cannot run without one.

use std::io::{self, Read, Stdin, Write};
use std::os::fd::AsFd;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec};

use crate::kitty::{InputEvent, KittyDecoder};

/// Progressive enhancement flags pushed on entry: 1 "disambiguate escape
/// codes" | 4 "report alternate keys", so alt+shift+5 arrives with its
/// shifted key and decodes as M-% rather than M-S-5.
pub const KITTY_FLAGS: u32 = 5;

pub const UNSUPPORTED_TERMINAL_MESSAGE: &str = "emx: this terminal does not support the Kitty keyboard protocol. Supported in this release: kitty, WezTerm, Ghostty, foot.";

const KITTY_FLAGS_QUERY: &[u8] = b"\x1b[?u";
const PRIMARY_DEVICE_ATTRIBUTES_QUERY: &[u8] = b"\x1b[c";
const ENTER_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049h";
const LEAVE_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049l";
const BRACKETED_PASTE_ON: &[u8] = b"\x1b[?2004h";
const BRACKETED_PASTE_OFF: &[u8] = b"\x1b[?2004l";
const POP_KITTY_FLAGS: &[u8] = b"\x1b[<u";
const RESET_ATTRIBUTES: &[u8] = b"\x1b[0m";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
const PROBE_READ_SIZE: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum TermError {
    #[error("terminal i/o: {0}")]
    Io(#[from] io::Error),
    #[error("terminal input closed during the keyboard protocol probe")]
    InputClosed,
    #[error("a Terminal is already active")]
    AlreadyActive,
}

/// Terminal input that can wait for readability with a timeout, which a
/// plain `Read` on a TTY cannot.
pub trait InputSource: Read {
    /// Returns false when `timeout` passes with nothing to read.
    fn wait_readable(&mut self, timeout: Duration) -> io::Result<bool>;
}

/// Reads the stdin file descriptor directly. `Stdin`'s own buffer is
/// bypassed because bytes held there would be invisible to `poll`.
pub struct StdinSource {
    stdin: Stdin,
}

impl StdinSource {
    pub fn new() -> StdinSource {
        StdinSource { stdin: io::stdin() }
    }
}

impl Default for StdinSource {
    fn default() -> StdinSource {
        StdinSource::new()
    }
}

impl Read for StdinSource {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        Ok(rustix::io::read(self.stdin.as_fd(), buffer)?)
    }
}

impl InputSource for StdinSource {
    fn wait_readable(&mut self, timeout: Duration) -> io::Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timespec = Timespec::try_from(remaining).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "poll timeout out of range")
            })?;
            let mut descriptors = [PollFd::new(&self.stdin, PollFlags::IN)];
            match rustix::event::poll(&mut descriptors, Some(&timespec)) {
                Ok(ready) => return Ok(ready > 0),
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
    pub supported: bool,
    /// Bytes read during the probe that were not probe replies (keys typed
    /// meanwhile, a partial sequence), in arrival order, for the caller's
    /// decoder.
    pub leftover: Vec<u8>,
}

/// Asks for the Kitty keyboard flags, then for the primary device
/// attributes, which every terminal answers. A flags reply before the
/// attributes reply means the protocol is supported. A terminal that stays
/// silent until `timeout` is unsupported unless it already sent flags.
///
/// Requires raw mode, so call it after `Terminal::enter`: in cooked mode the
/// replies are echoed and line-buffered and may never reach `input`.
pub fn probe_kitty(
    input: &mut impl InputSource,
    output: &mut impl Write,
    timeout: Duration,
) -> Result<ProbeResult, TermError> {
    output.write_all(KITTY_FLAGS_QUERY)?;
    output.write_all(PRIMARY_DEVICE_ATTRIBUTES_QUERY)?;
    output.flush()?;
    let deadline = Instant::now() + timeout;
    let mut decoder = KittyDecoder::new();
    let mut raw = Vec::new();
    let mut leftover = Vec::new();
    let mut decoded_until = 0;
    let mut flags_seen = false;
    let mut attributes_seen = false;
    while !attributes_seen {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || !input.wait_readable(remaining)? {
            break;
        }
        let mut buffer = [0; PROBE_READ_SIZE];
        let count = match input.read(&mut buffer) {
            Ok(0) => return Err(TermError::InputClosed),
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        raw.extend_from_slice(&buffer[..count]);
        for decoded in decoder.feed_accounted(&buffer[..count]) {
            let span = decoded_until..decoded_until + decoded.consumed;
            decoded_until = span.end;
            match decoded.event {
                Some(InputEvent::KittyFlags(_)) if !attributes_seen => flags_seen = true,
                Some(InputEvent::PrimaryDeviceAttributes) if !attributes_seen => {
                    attributes_seen = true;
                }
                _ => leftover.extend_from_slice(&raw[span]),
            }
        }
    }
    leftover.extend_from_slice(&raw[decoded_until..]);
    Ok(ProbeResult {
        supported: flags_seen,
        leftover,
    })
}

/// Kitty keeps separate flag stacks for the main and alternate screens, so
/// the flags are pushed after switching screens and popped before leaving.
pub fn setup_sequence() -> Vec<u8> {
    let mut sequence = Vec::new();
    sequence.extend_from_slice(ENTER_ALTERNATE_SCREEN);
    sequence.extend_from_slice(BRACKETED_PASTE_ON);
    sequence.extend_from_slice(format!("\x1b[>{KITTY_FLAGS}u").as_bytes());
    sequence
}

pub fn restore_sequence() -> Vec<u8> {
    [
        POP_KITTY_FLAGS,
        BRACKETED_PASTE_OFF,
        RESET_ATTRIBUTES,
        SHOW_CURSOR,
        LEAVE_ALTERNATE_SCREEN,
    ]
    .concat()
}

/// True while a `Terminal` has the TTY set up; whoever flips it back to
/// false (drop or panic hook) is the one that restores.
static ACTIVE: AtomicBool = AtomicBool::new(false);
static PANIC_HOOK: Once = Once::new();

/// Raw mode, alternate screen, bracketed paste and Kitty flags for as long
/// as it lives. Dropping it, or a panic anywhere, restores the terminal
/// exactly once.
pub struct Terminal {
    _not_constructible_elsewhere: (),
}

impl Terminal {
    pub fn enter() -> Result<Terminal, TermError> {
        if ACTIVE.swap(true, Ordering::SeqCst) {
            return Err(TermError::AlreadyActive);
        }
        PANIC_HOOK.call_once(install_panic_hook);
        if let Err(error) = crossterm::terminal::enable_raw_mode() {
            ACTIVE.store(false, Ordering::SeqCst);
            return Err(error.into());
        }
        let terminal = Terminal {
            _not_constructible_elsewhere: (),
        };
        write_stdout(&setup_sequence())?;
        Ok(terminal)
    }

    /// Columns and rows.
    pub fn size(&self) -> Result<(u16, u16), TermError> {
        Ok(crossterm::terminal::size()?)
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<(), TermError> {
        Ok(write_stdout(bytes)?)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Err(error) = restore() {
            eprintln!("emx: failed to restore the terminal: {error}");
        }
    }
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Err(error) = restore() {
            eprintln!("emx: failed to restore the terminal: {error}");
        }
        previous(info);
    }));
}

fn restore() -> io::Result<()> {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return Ok(());
    }
    let written = write_stdout(&restore_sequence());
    let cooked = crossterm::terminal::disable_raw_mode();
    written.and(cooked)
}

fn write_stdout(bytes: &[u8]) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(bytes)?;
    stdout.flush()
}
