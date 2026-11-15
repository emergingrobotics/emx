//! Runs `emx` on a pseudo-terminal the way a terminal would drive it: the
//! harness answers the keyboard protocol probe as a Kitty-protocol terminal
//! (or as a legacy one, or not at all), encodes keys the way that terminal
//! would, and keeps the screen the program draws in a `vt100` parser.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use emx_proto::{ClientKind, ClientMsg, LineReader, ServerMsg, WirePath, write_msg};
use emx_types::{Key, KeyCode, Mods, parse_kbd};
use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};

use super::{TestEnv, WAIT};

pub const COLS: u16 = 80;
pub const ROWS: u16 = 24;
const POLL: Duration = Duration::from_millis(10);
const READ_SIZE: usize = 4096;
const SCROLLBACK: usize = 0;

const KITTY_FLAGS_QUERY: &[u8] = b"\x1b[?u";
const DEVICE_ATTRIBUTES_QUERY: &[u8] = b"\x1b[c";
/// Flags 5: disambiguate escape codes and report alternate keys.
const KITTY_FLAGS_REPLY: &[u8] = b"\x1b[?5u";
const DEVICE_ATTRIBUTES_REPLY: &[u8] = b"\x1b[?62c";
pub const ENTER_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049h";
pub const LEAVE_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049l";
pub const PUSH_KITTY_FLAGS: &[u8] = b"\x1b[>5u";
pub const POP_KITTY_FLAGS: &[u8] = b"\x1b[<u";
pub const BRACKETED_PASTE_ON: &[u8] = b"\x1b[?2004h";
pub const BRACKETED_PASTE_OFF: &[u8] = b"\x1b[?2004l";
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

const KITTY_SHIFT: u32 = 1;
const KITTY_ALT: u32 = 2;
const KITTY_CTRL: u32 = 4;
const KITTY_SUPER: u32 = 8;
const KITTY_ESCAPE: u32 = 27;
const KITTY_ENTER: u32 = 13;
const KITTY_TAB: u32 = 9;
const KITTY_BACKSPACE: u32 = 127;

/// What the terminal answers when emx probes for the Kitty protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// A Kitty-protocol terminal: both queries are answered.
    Answer,
    /// A legacy terminal: only the device attributes query is answered.
    Legacy,
    /// A terminal that never answers either query, so also legacy.
    Silent,
}

impl Probe {
    fn kitty(self) -> bool {
        self == Probe::Answer
    }
}

struct Output {
    parser: vt100::Parser,
    raw: Vec<u8>,
    /// Where the next search for each query starts.
    flags_query_from: usize,
    attributes_query_from: usize,
    closed: bool,
}

/// `emx` on a pseudo-terminal; killed when dropped.
pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    output: Arc<Mutex<Output>>,
    child: Box<dyn Child + Send + Sync>,
    pid: u32,
    cols: u16,
    rows: u16,
    probe: Probe,
}

impl Pty {
    pub fn spawn(env: &TestEnv, args: &[&str]) -> Pty {
        Pty::spawn_with(env, args, Probe::Answer, &[])
    }

    pub fn spawn_with(
        env: &TestEnv,
        args: &[&str],
        probe: Probe,
        variables: &[(&str, &str)],
    ) -> Pty {
        let system = native_pty_system();
        let pair = system
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open a pty");
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_emx"));
        command.args(args);
        command.env("EMX_SOCKET_DIR", &env.socket_dir);
        command.env("XDG_STATE_HOME", &env.state_home);
        command.env("XDG_CONFIG_HOME", &env.config_home);
        command.env("HOME", &env.home);
        command.env("TERM", "xterm-kitty");
        command.env_remove("XDG_RUNTIME_DIR");
        for (name, value) in variables {
            command.env(name, value);
        }
        command.cwd(&env.work);
        let child = pair.slave.spawn_command(command).expect("spawn emx");
        // Only the child may hold the terminal side, so the reader sees the
        // end of output when it exits.
        drop(pair.slave);
        let pid = child.process_id().expect("a pid");
        let reader = pair.master.try_clone_reader().expect("pty reader");
        let writer = Arc::new(Mutex::new(pair.master.take_writer().expect("pty writer")));
        let output = Arc::new(Mutex::new(Output {
            parser: vt100::Parser::new(ROWS, COLS, SCROLLBACK),
            raw: Vec::new(),
            flags_query_from: 0,
            attributes_query_from: 0,
            closed: false,
        }));
        let reader_output = output.clone();
        let reader_writer = writer.clone();
        thread::spawn(move || read_loop(reader, &reader_output, &reader_writer, probe));
        Pty {
            master: pair.master,
            writer,
            output,
            child,
            pid,
            cols: COLS,
            rows: ROWS,
            probe,
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn write(&self, bytes: &[u8]) {
        let mut writer = self.writer.lock().expect("writer lock");
        writer.write_all(bytes).expect("write to the pty");
        writer.flush().expect("flush the pty");
    }

    /// Sends an Emacs `kbd` sequence, each key encoded as the terminal
    /// would: Kitty-encoded, or the legacy way.
    pub fn keys(&self, kbd: &str) {
        for key in parse_kbd(kbd).expect("valid kbd") {
            self.write(&if self.probe.kitty() {
                encode_key(key)
            } else {
                encode_legacy_key(key)
            });
        }
    }

    /// Types `text` as plain keys; it must not hold control characters.
    pub fn type_text(&self, text: &str) {
        assert!(!text.chars().any(char::is_control), "{text:?}");
        self.write(text.as_bytes());
    }

    /// A bracketed paste of `text`, as the terminal sends it.
    pub fn paste(&self, text: &str) {
        let mut bytes = PASTE_START.to_vec();
        bytes.extend_from_slice(text.as_bytes());
        bytes.extend_from_slice(PASTE_END);
        self.write(&bytes);
    }

    pub fn screen(&self) -> Vec<String> {
        let output = self.output.lock().expect("output lock");
        output.parser.screen().rows(0, self.cols).collect()
    }

    /// The selected window's modeline: the row above the echo area.
    pub fn modeline(&self) -> String {
        self.screen()[usize::from(self.rows) - 2].clone()
    }

    pub fn alternate_screen(&self) -> bool {
        self.output
            .lock()
            .expect("output lock")
            .parser
            .screen()
            .alternate_screen()
    }

    pub fn bracketed_paste(&self) -> bool {
        self.output
            .lock()
            .expect("output lock")
            .parser
            .screen()
            .bracketed_paste()
    }

    /// Everything the program wrote, escape sequences included.
    pub fn raw_output(&self) -> Vec<u8> {
        self.output.lock().expect("output lock").raw.clone()
    }

    pub fn raw_output_text(&self) -> String {
        String::from_utf8_lossy(&self.raw_output()).into_owned()
    }

    pub fn wait_screen(&self, what: &str, want: impl Fn(&[String]) -> bool) -> Vec<String> {
        let deadline = Instant::now() + WAIT;
        loop {
            let screen = self.screen();
            if want(&screen) {
                return screen;
            }
            assert!(
                Instant::now() < deadline,
                "waiting for {what}; screen:\n{}",
                screen.join("\n")
            );
            thread::sleep(POLL);
        }
    }

    /// Waits until some row contains `text`.
    pub fn wait_text(&self, text: &str) -> Vec<String> {
        self.wait_screen(text, |screen| screen.iter().any(|row| row.contains(text)))
    }

    pub fn wait_modeline(&self, want: impl Fn(&str) -> bool) -> String {
        let row = usize::from(self.rows) - 2;
        let screen = self.wait_screen("the modeline", |screen| want(&screen[row]));
        screen[row].clone()
    }

    /// Resizes the terminal; the program gets SIGWINCH.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.output
            .lock()
            .expect("output lock")
            .parser
            .set_size(rows, cols);
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize the pty");
        self.cols = cols;
        self.rows = rows;
    }

    /// The exit status, once the program exits and its output is read.
    pub fn wait_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + WAIT;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "emx never exited; screen:\n{}",
                self.screen().join("\n")
            );
            thread::sleep(POLL);
        };
        while !self.output.lock().expect("output lock").closed {
            assert!(Instant::now() < deadline, "the pty output never ended");
            thread::sleep(POLL);
        }
        status
    }

    /// `kill -9`, then reap.
    pub fn kill9(&mut self) {
        kill9(self.pid);
        self.child.wait().expect("reap emx");
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            kill9(self.pid);
            drop(self.child.wait());
        }
    }
}

fn read_loop(
    mut reader: Box<dyn Read + Send>,
    output: &Mutex<Output>,
    writer: &Mutex<Box<dyn Write + Send>>,
    probe: Probe,
) {
    let mut buffer = [0; READ_SIZE];
    let kitty = probe.kitty();
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) | Err(_) => {
                output.lock().expect("output lock").closed = true;
                return;
            }
            Ok(count) => count,
        };
        let replies = {
            let mut output = output.lock().expect("output lock");
            output.parser.process(&buffer[..count]);
            output.raw.extend_from_slice(&buffer[..count]);
            output.replies(kitty)
        };
        if probe != Probe::Silent && !replies.is_empty() {
            let mut writer = writer.lock().expect("writer lock");
            // The program may already be gone.
            drop(writer.write_all(&replies).and_then(|()| writer.flush()));
        }
    }
}

impl Output {
    /// Replies to the queries that arrived since the last call, in order;
    /// a legacy terminal answers only the device attributes query.
    fn replies(&mut self, kitty: bool) -> Vec<u8> {
        let mut found = Vec::new();
        for (query, reply, from) in [
            (
                KITTY_FLAGS_QUERY,
                if kitty { KITTY_FLAGS_REPLY } else { b"" },
                &mut self.flags_query_from,
            ),
            (
                DEVICE_ATTRIBUTES_QUERY,
                DEVICE_ATTRIBUTES_REPLY,
                &mut self.attributes_query_from,
            ),
        ] {
            while let Some(position) = find(&self.raw[*from..], query) {
                found.push((*from + position, reply));
                *from += position + query.len();
            }
            // A query cut off at the end of what was read so far.
            *from = (*from).max(self.raw.len().saturating_sub(query.len() - 1));
        }
        found.sort_by_key(|(position, _)| *position);
        found
            .into_iter()
            .flat_map(|(_, reply)| reply.to_vec())
            .collect()
    }
}

pub fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

pub fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

/// A key as a Kitty terminal with flags 5 sends it: text keys as UTF-8,
/// Enter, Tab and Backspace as legacy bytes, everything else modified or
/// ambiguous as `CSI code ; modifiers u` or its legacy form.
pub fn encode_key(key: Key) -> Vec<u8> {
    let modifiers = kitty_modifiers(key.mods);
    let csi_u = |code: u32| {
        if modifiers == 0 {
            format!("\x1b[{code}u").into_bytes()
        } else {
            format!("\x1b[{code};{}u", modifiers + 1).into_bytes()
        }
    };
    let letter = |final_byte: char| {
        if modifiers == 0 {
            format!("\x1b[{final_byte}").into_bytes()
        } else {
            format!("\x1b[1;{}{final_byte}", modifiers + 1).into_bytes()
        }
    };
    let tilde = |number: u32| {
        if modifiers == 0 {
            format!("\x1b[{number}~").into_bytes()
        } else {
            format!("\x1b[{number};{}~", modifiers + 1).into_bytes()
        }
    };
    match key.code {
        KeyCode::Char(character) if modifiers == 0 => character.to_string().into_bytes(),
        KeyCode::Char(character) => csi_u(u32::from(character)),
        KeyCode::Enter if modifiers == 0 => b"\r".to_vec(),
        KeyCode::Enter => csi_u(KITTY_ENTER),
        KeyCode::Tab if modifiers == 0 => b"\t".to_vec(),
        KeyCode::Tab => csi_u(KITTY_TAB),
        KeyCode::Backspace if modifiers == 0 => vec![0x7f],
        KeyCode::Backspace => csi_u(KITTY_BACKSPACE),
        KeyCode::Escape => csi_u(KITTY_ESCAPE),
        KeyCode::Up => letter('A'),
        KeyCode::Down => letter('B'),
        KeyCode::Right => letter('C'),
        KeyCode::Left => letter('D'),
        KeyCode::Home => letter('H'),
        KeyCode::End => letter('F'),
        KeyCode::Insert => tilde(2),
        KeyCode::Delete => tilde(3),
        KeyCode::PageUp => tilde(5),
        KeyCode::PageDown => tilde(6),
        KeyCode::F(number) => {
            let tilde_number = match number {
                1 => 11,
                2 => 12,
                3 => 13,
                4 => 14,
                5 => 15,
                6..=10 => u32::from(number) + 11,
                11 | 12 => u32::from(number) + 12,
                other => panic!("the harness does not encode F{other}"),
            };
            tilde(tilde_number)
        }
    }
}

/// A key as a legacy terminal sends it, with Alt sending an ESC prefix and
/// xterm's `modifyOtherKeys` at level 1 for Ctrl keys without a byte.
pub fn encode_legacy_key(key: Key) -> Vec<u8> {
    if key.mods.meta {
        let mut bytes = vec![0x1b];
        bytes.extend(encode_legacy_key(Key {
            code: key.code,
            mods: Mods {
                meta: false,
                ..key.mods
            },
        }));
        return bytes;
    }
    let modifiers = kitty_modifiers(key.mods);
    let letter = |final_byte: char| {
        if modifiers == 0 {
            format!("\x1b[{final_byte}").into_bytes()
        } else {
            format!("\x1b[1;{}{final_byte}", modifiers + 1).into_bytes()
        }
    };
    let tilde = |number: u32| {
        if modifiers == 0 {
            format!("\x1b[{number}~").into_bytes()
        } else {
            format!("\x1b[{number};{}~", modifiers + 1).into_bytes()
        }
    };
    match key.code {
        KeyCode::Char(character) if modifiers == 0 => character.to_string().into_bytes(),
        KeyCode::Char(' ') if key.mods.ctrl && !key.mods.shift => vec![0],
        KeyCode::Char(character @ ('a'..='z' | '@' | '[' | '\\' | ']' | '^' | '_'))
            if key.mods.ctrl && !key.mods.shift =>
        {
            vec![(character.to_ascii_uppercase() as u8) & 0x1f]
        }
        KeyCode::Char(character) => {
            format!("\x1b[27;{};{}~", modifiers + 1, u32::from(character)).into_bytes()
        }
        KeyCode::Enter if modifiers == 0 => b"\r".to_vec(),
        KeyCode::Tab if modifiers == 0 => b"\t".to_vec(),
        KeyCode::Tab if key.mods == SHIFT_ONLY => b"\x1b[Z".to_vec(),
        KeyCode::Backspace if modifiers == 0 => vec![0x7f],
        KeyCode::Escape if modifiers == 0 => vec![0x1b],
        KeyCode::Enter => format!("\x1b[27;{};13~", modifiers + 1).into_bytes(),
        KeyCode::Tab => format!("\x1b[27;{};9~", modifiers + 1).into_bytes(),
        KeyCode::Backspace => format!("\x1b[27;{};127~", modifiers + 1).into_bytes(),
        KeyCode::Escape => format!("\x1b[27;{};27~", modifiers + 1).into_bytes(),
        KeyCode::Up => letter('A'),
        KeyCode::Down => letter('B'),
        KeyCode::Right => letter('C'),
        KeyCode::Left => letter('D'),
        KeyCode::Home => letter('H'),
        KeyCode::End => letter('F'),
        KeyCode::Insert => tilde(2),
        KeyCode::Delete => tilde(3),
        KeyCode::PageUp => tilde(5),
        KeyCode::PageDown => tilde(6),
        KeyCode::F(number @ 1..=4) if modifiers == 0 => {
            format!("\x1bO{}", char::from(b'P' + number - 1)).into_bytes()
        }
        KeyCode::F(number @ 1..=4) => letter(char::from(b'P' + number - 1)),
        KeyCode::F(number) => {
            let tilde_number = match number {
                5 => 15,
                6..=10 => u32::from(number) + 11,
                11 | 12 => u32::from(number) + 12,
                other => panic!("the harness does not encode F{other}"),
            };
            tilde(tilde_number)
        }
    }
}

const SHIFT_ONLY: Mods = Mods {
    ctrl: false,
    meta: false,
    shift: true,
    super_: false,
};

fn kitty_modifiers(mods: Mods) -> u32 {
    let mut bits = 0;
    if mods.shift {
        bits |= KITTY_SHIFT;
    }
    if mods.meta {
        bits |= KITTY_ALT;
    }
    if mods.ctrl {
        bits |= KITTY_CTRL;
    }
    if mods.super_ {
        bits |= KITTY_SUPER;
    }
    bits
}

pub fn kill9(pid: u32) {
    let pid = rustix::process::Pid::from_raw(pid as i32).expect("a pid");
    match rustix::process::kill_process(pid, rustix::process::Signal::KILL) {
        // Already gone is fine.
        Ok(()) | Err(rustix::io::Errno::SRCH) => {}
        Err(error) => panic!("kill -9 {pid:?}: {error}"),
    }
}

/// The pid of the server listening on `socket`, from its welcome.
pub fn server_pid(socket: &Path, cwd: &Path) -> Option<u32> {
    let stream = UnixStream::connect(socket).ok()?;
    stream.set_read_timeout(Some(WAIT)).ok()?;
    let mut writer = stream.try_clone().ok()?;
    write_msg(
        &mut writer,
        &ClientMsg::Hello {
            kind: ClientKind::Cli,
            cwd: WirePath::from(cwd),
            cols: COLS,
            rows: ROWS,
            wait_for: None,
        },
    )
    .ok()?;
    match LineReader::new(&stream).read_msg::<ServerMsg>() {
        Ok(Some(ServerMsg::Welcome { server_pid })) => Some(server_pid),
        _ => None,
    }
}

/// Pids of the processes started with this session's socket directory:
/// every `emx` a test ran, and the servers they started.
pub fn session_processes(env: &TestEnv) -> Vec<u32> {
    processes_using(&env.socket_dir)
}

fn processes_using(socket_dir: &Path) -> Vec<u32> {
    let marker = format!("EMX_SOCKET_DIR={}", socket_dir.display()).into_bytes();
    let own = std::process::id();
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| *pid != own)
        .filter(|pid| {
            fs::read(format!("/proc/{pid}/environ")).is_ok_and(|environ| {
                environ
                    .split(|byte| *byte == 0)
                    .any(|variable| variable == marker)
            })
        })
        .collect()
}

/// Server processes of this session: `emx server` with any flags.
pub fn session_servers(env: &TestEnv) -> Vec<u32> {
    session_processes(env)
        .into_iter()
        .filter(|pid| alive(*pid))
        .filter(|pid| {
            fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| {
                cmdline
                    .split(|byte| *byte == 0)
                    .nth(1)
                    .is_some_and(|argument| argument == b"server")
            })
        })
        .collect()
}

/// Kills every process of the session when dropped, so a server a client
/// started in the background never outlives its test.
pub struct SessionReaper {
    socket_dir: PathBuf,
}

impl SessionReaper {
    pub fn new(env: &TestEnv) -> SessionReaper {
        SessionReaper {
            socket_dir: env.socket_dir.clone(),
        }
    }
}

impl Drop for SessionReaper {
    fn drop(&mut self) {
        for pid in processes_using(&self.socket_dir) {
            kill9(pid);
        }
    }
}

/// Whether process `pid` exists and has not exited.
pub fn alive(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit(')')
            .next()
            .and_then(|fields| fields.split_whitespace().next())
            .is_some_and(|state| state != "Z" && state != "X")
    })
}
