//! Drives `emx` on a pseudo-terminal the way a Kitty-protocol terminal
//! would: it answers the keyboard-protocol and device-attributes probes,
//! keeps the drawn screen in a `vt100` parser, and counts bytes received so
//! callers can time the paint that follows a keystroke. The child is killed
//! when the harness is dropped, so a wedged editor never outlives a run.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

pub const COLS: u16 = 80;
pub const ROWS: u16 = 24;
const SCROLLBACK: usize = 0;
const READ_SIZE: usize = 65536;
const POLL: Duration = Duration::from_millis(5);

const KITTY_FLAGS_QUERY: &[u8] = b"\x1b[?u";
const DEVICE_ATTRIBUTES_QUERY: &[u8] = b"\x1b[c";
/// Flags 5: disambiguate escape codes and report alternate keys.
const KITTY_FLAGS_REPLY: &[u8] = b"\x1b[?5u";
const DEVICE_ATTRIBUTES_REPLY: &[u8] = b"\x1b[?62c";

/// Keys as a Kitty terminal with flags 5 sends them, matching the encoding
/// the differential test harness uses (CSI code ; modifiers u, modifiers
/// being the bitmask plus one; ctrl is 4, meta is 2).
pub const CTRL_N: &[u8] = b"\x1b[110;5u";
pub const CTRL_V: &[u8] = b"\x1b[118;5u";
pub const CTRL_S: &[u8] = b"\x1b[115;5u";
pub const CTRL_G: &[u8] = b"\x1b[103;5u";
pub const META_GREATER: &[u8] = b"\x1b[62;3u";

struct Inner {
    parser: vt100::Parser,
    raw: Vec<u8>,
    received: usize,
    flags_query_from: usize,
    attributes_query_from: usize,
    closed: bool,
}

struct Shared {
    inner: Mutex<Inner>,
    changed: Condvar,
}

pub struct Pty {
    shared: Arc<Shared>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Box<dyn Child + Send + Sync>,
    pid: Option<u32>,
    _master: Box<dyn MasterPty + Send>,
}

impl Pty {
    pub fn spawn(emx: &Path, args: &[&str], env_dir: &Path) -> Result<Pty, String> {
        let state = env_dir.join("state");
        let config = env_dir.join("config");
        let home = env_dir.join("home");
        let socket = env_dir.join("socket");
        for directory in [&state, &config, &home, &socket] {
            std::fs::create_dir_all(directory)
                .map_err(|error| format!("creating {}: {error}", directory.display()))?;
        }

        let system = native_pty_system();
        let pair = system
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| format!("opening a pty: {error}"))?;

        let mut command = CommandBuilder::new(emx);
        command.args(args);
        command.env("TERM", "xterm-kitty");
        command.env("XDG_STATE_HOME", &state);
        command.env("XDG_CONFIG_HOME", &config);
        command.env("HOME", &home);
        command.env("EMX_SOCKET_DIR", &socket);
        command.env_remove("XDG_RUNTIME_DIR");
        command.cwd(env_dir);

        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| format!("spawning emx: {error}"))?;
        // Only the child may hold the terminal side, so the reader sees the
        // end of output when it exits.
        drop(pair.slave);
        let pid = child.process_id();
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| format!("cloning the pty reader: {error}"))?;
        let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(
            pair.master
                .take_writer()
                .map_err(|error| format!("taking the pty writer: {error}"))?,
        ));
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                parser: vt100::Parser::new(ROWS, COLS, SCROLLBACK),
                raw: Vec::new(),
                received: 0,
                flags_query_from: 0,
                attributes_query_from: 0,
                closed: false,
            }),
            changed: Condvar::new(),
        });
        let reader_shared = shared.clone();
        let reader_writer = writer.clone();
        thread::spawn(move || read_loop(reader, &reader_shared, &reader_writer));

        Ok(Pty {
            shared,
            writer,
            child,
            pid,
            _master: pair.master,
        })
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn write(&self, bytes: &[u8]) {
        if let Ok(mut writer) = self.writer.lock() {
            // The editor may already be gone; a failed write is not fatal to
            // the benchmark, which reports a timeout if no paint follows.
            drop(writer.write_all(bytes).and_then(|()| writer.flush()));
        }
    }

    pub fn received(&self) -> usize {
        self.shared.inner.lock().expect("output lock").received
    }

    /// Waits until the editor has written more than `from` bytes in total,
    /// returning how long that took from `start`, or `None` on the deadline
    /// or if the editor exited without painting.
    pub fn wait_received_beyond(
        &self,
        from: usize,
        start: Instant,
        deadline: Instant,
    ) -> Option<Duration> {
        let mut inner = self.shared.inner.lock().expect("output lock");
        loop {
            if inner.received > from {
                return Some(start.elapsed());
            }
            if inner.closed {
                return None;
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let timeout = (deadline - now).min(POLL);
            let (next, _) = self
                .shared
                .changed
                .wait_timeout(inner, timeout)
                .expect("condvar wait");
            inner = next;
        }
    }

    /// Waits until the drawn screen satisfies `predicate`, returning the time
    /// from `start`, or `None` on the deadline.
    pub fn wait_screen(
        &self,
        start: Instant,
        deadline: Instant,
        predicate: impl Fn(&[String]) -> bool,
    ) -> Option<Duration> {
        let mut inner = self.shared.inner.lock().expect("output lock");
        loop {
            let rows: Vec<String> = inner.parser.screen().rows(0, COLS).collect();
            if predicate(&rows) {
                return Some(start.elapsed());
            }
            if inner.closed {
                return None;
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let timeout = (deadline - now).min(POLL);
            let (next, _) = self
                .shared
                .changed
                .wait_timeout(inner, timeout)
                .expect("condvar wait");
            inner = next;
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            drop(self.child.kill());
            drop(self.child.wait());
        }
    }
}

fn read_loop(
    mut reader: Box<dyn Read + Send>,
    shared: &Shared,
    writer: &Mutex<Box<dyn Write + Send>>,
) {
    let mut buffer = [0u8; READ_SIZE];
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) | Err(_) => {
                let mut inner = shared.inner.lock().expect("output lock");
                inner.closed = true;
                shared.changed.notify_all();
                return;
            }
            Ok(count) => count,
        };
        let replies = {
            let mut inner = shared.inner.lock().expect("output lock");
            inner.parser.process(&buffer[..count]);
            inner.raw.extend_from_slice(&buffer[..count]);
            inner.received += count;
            let replies = inner.replies();
            shared.changed.notify_all();
            replies
        };
        if !replies.is_empty()
            && let Ok(mut writer) = writer.lock()
        {
            drop(writer.write_all(&replies).and_then(|()| writer.flush()));
        }
    }
}

impl Inner {
    /// Replies to the probe queries that arrived since the last call.
    fn replies(&mut self) -> Vec<u8> {
        let mut found: Vec<(usize, &[u8])> = Vec::new();
        let raw = self.raw.clone();
        for (query, reply, from) in [
            (
                KITTY_FLAGS_QUERY,
                KITTY_FLAGS_REPLY,
                &mut self.flags_query_from,
            ),
            (
                DEVICE_ATTRIBUTES_QUERY,
                DEVICE_ATTRIBUTES_REPLY,
                &mut self.attributes_query_from,
            ),
        ] {
            while let Some(position) = find(&raw[*from..], query) {
                found.push((*from + position, reply));
                *from += position + query.len();
            }
            *from = (*from).max(raw.len().saturating_sub(query.len().saturating_sub(1)));
        }
        found.sort_by_key(|(position, _)| *position);
        found
            .into_iter()
            .flat_map(|(_, reply)| reply.to_vec())
            .collect()
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
