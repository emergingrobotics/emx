//! The terminal client: `emx [FILES]` and `emx --wait FILE` attach this
//! terminal to the session server (starting it when none runs), forward
//! keys, pastes and resizes, and draw the frames the server sends.
//! `emx --no-server` runs the same client over an in-process link.
//!
//! One thread owns the terminal and reacts to `UiEvent`s; a thread reads
//! the terminal, a thread reads the server, and a thread turns signals
//! into events.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use anyhow::{Context, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use emx_core::KeyboardEncoding;
use emx_proto::{ClientKind, ClientMsg, LineReader, Request, ServerMsg, WirePath, write_msg};
use emx_term::{
    Decoder, InputEvent, KEYBOARD_ENV, Protocol, Screen, StdinSource, Terminal, keyboard_override,
    probe_protocol,
};
use emx_types::Grid;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGWINCH};
use signal_hook::iterator::Signals;

use crate::paths;
use crate::server::client_conn::{MAX_TERMINAL_COLS, MAX_TERMINAL_ROWS, peer_uid};

/// How long a terminal has to answer the keyboard protocol probe; one
/// that stays silent this long does not speak it.
pub const KITTY_PROBE_TIMEOUT_MS: u64 = 1000;
/// How long a legacy terminal's ESC waits for the rest of a key sequence
/// before it is the Escape key. Terminals send a sequence's bytes together,
/// so this only needs to cover a slow link.
pub const LEGACY_ESCAPE_TIMEOUT_MS: u64 = 50;
pub const CONNECTION_LOST: &str = "emx: server connection lost";
/// Debug builds panic after drawing the first frame when this is set, so
/// tests can check that a panicking client restores the terminal.
#[cfg(debug_assertions)]
pub const CLIENT_PANIC_ENV: &str = "EMX_TEST_CLIENT_PANIC";
const READ_SIZE: usize = 64 * 1024;
const BELL: &[u8] = b"\x07";
const FILE_POSITION_SEPARATOR: u8 = b':';
const EXIT_FAILURE: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientOptions {
    pub files: Vec<OsString>,
    /// Exit once every file is done with (`C-x #` or killing its buffer).
    pub wait: bool,
}

/// A file named on the command line, with the line and column of a
/// `FILE:LINE[:COL]` suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileArgument {
    pub path: PathBuf,
    pub line: Option<u64>,
    pub col: Option<u64>,
}

/// `FILE:LINE:COL` or `FILE:LINE` when the suffix is all numbers and the
/// whole argument names no existing file; anything else is a file name,
/// so a file whose name ends in `:12` can still be opened.
pub fn parse_file_argument(argument: &OsStr, exists: &dyn Fn(&Path) -> bool) -> FileArgument {
    let whole = || FileArgument {
        path: PathBuf::from(argument),
        line: None,
        col: None,
    };
    if exists(Path::new(argument)) {
        return whole();
    }
    let bytes = argument.as_bytes();
    let number = |part: &[u8]| -> Option<u64> {
        if part.is_empty() || !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(part).ok()?.parse().ok()
    };
    let split = |bytes: &[u8]| -> Option<(Vec<u8>, u64)> {
        let at = bytes
            .iter()
            .rposition(|byte| *byte == FILE_POSITION_SEPARATOR)?;
        let value = number(&bytes[at + 1..])?;
        (at > 0).then(|| (bytes[..at].to_vec(), value))
    };
    let path = |bytes: Vec<u8>| PathBuf::from(OsString::from_vec(bytes));
    match split(bytes) {
        Some((rest, last)) => match split(&rest) {
            Some((file, line)) => FileArgument {
                path: path(file),
                line: Some(line),
                col: Some(last),
            },
            None => FileArgument {
                path: path(rest),
                line: Some(last),
                col: None,
            },
        },
        None => whole(),
    }
}

pub fn parse_file_arguments(arguments: &[OsString]) -> Vec<FileArgument> {
    arguments
        .iter()
        .map(|argument| parse_file_argument(argument, &|path| path.exists()))
        .collect()
}

/// Runs the terminal client of the session server.
pub fn run(options: ClientOptions) -> anyhow::Result<ExitCode> {
    let files = parse_file_arguments(&options.files);
    let keyboard = crate::server::load_config()?.keyboard_encoding;
    let socket = socket_path()?;
    // Connect before taking over the terminal, so a server that cannot
    // start is reported on a usable terminal.
    let stream = connect_or_start(&socket)?;
    let (sender, events) = mpsc::channel();
    let end = run_tui(
        move |sender: &Sender<UiEvent>| -> anyhow::Result<Box<dyn Link>> {
            Ok(Box::new(SocketLink::start(stream, socket, sender.clone())?))
        },
        sender,
        events,
        &files,
        options.wait,
        keyboard,
    )?;
    Ok(end.report())
}

pub(crate) fn socket_path() -> anyhow::Result<PathBuf> {
    let euid = rustix::process::geteuid().as_raw();
    Ok(paths::socket_path(&paths::socket_dir(euid)?))
}

/// Connects to the server, starting one when none listens.
fn connect_or_start(socket: &Path) -> anyhow::Result<UnixStream> {
    let stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(error) if no_server_listens(&error) => {
            start_server()?;
            UnixStream::connect(socket)
                .with_context(|| format!("connecting to {}", socket.display()))?
        }
        Err(error) => {
            return Err(error).with_context(|| format!("connecting to {}", socket.display()));
        }
    };
    verify_server(socket, &stream)?;
    Ok(stream)
}

/// A connect error that means no server is running: no socket, or a
/// socket left behind by a server that is gone.
pub(crate) fn no_server_listens(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

/// The server must run as this user (DESIGN section 2, privilege
/// isolation).
pub(crate) fn verify_server(socket: &Path, stream: &UnixStream) -> anyhow::Result<()> {
    let euid = rustix::process::geteuid().as_raw();
    let peer = peer_uid(stream)
        .with_context(|| format!("reading the credentials of {}", socket.display()))?;
    check_server_uid(socket, peer, euid)
}

fn check_server_uid(socket: &Path, peer: u32, euid: u32) -> anyhow::Result<()> {
    if peer != euid {
        bail!(
            "refusing the server at {}: it runs as uid {peer}, not uid {euid}",
            socket.display()
        );
    }
    Ok(())
}

/// `emx server --detach`, which returns once a server listens.
fn start_server() -> anyhow::Result<()> {
    let program = std::env::current_exe().context("finding the emx program")?;
    let output = Command::new(program)
        .args(["server", "--detach"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("starting the emx server")?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        let message = message.trim();
        bail!("{}", message.strip_prefix("emx: ").unwrap_or(message));
    }
    Ok(())
}

/// What the terminal thread reacts to.
pub enum UiEvent {
    /// Bytes read from the terminal.
    Input(Vec<u8>),
    /// The terminal cannot be read any more (hung up).
    InputClosed,
    /// A message from connection number `generation`.
    Server { generation: u64, message: ServerMsg },
    /// Connection number `generation` ended, with what broke it.
    ServerClosed {
        generation: u64,
        error: Option<String>,
    },
    /// SIGWINCH.
    Resize,
    /// SIGTERM, SIGINT or SIGHUP.
    Terminate,
}

/// The client's connection to an editor: a socket, or `--no-server`'s
/// in-process link.
pub trait Link {
    fn send(&mut self, message: ClientMsg) -> io::Result<()>;
    /// Which connection the current one is; events from older ones are
    /// ignored.
    fn generation(&self) -> u64;
    /// A new connection, after one ended before the first frame (the
    /// server was exiting as the client connected); `None` when this link
    /// cannot make one.
    fn reconnect(&mut self) -> Option<anyhow::Result<()>>;
    /// The client is going away on a signal or a lost terminal. True when
    /// the editor answers with `Exit`, which the client then waits for.
    fn terminate(&mut self) -> bool;
}

struct SocketLink {
    stream: UnixStream,
    socket: PathBuf,
    generation: u64,
    sender: Sender<UiEvent>,
}

impl SocketLink {
    fn start(
        stream: UnixStream,
        socket: PathBuf,
        sender: Sender<UiEvent>,
    ) -> anyhow::Result<SocketLink> {
        let link = SocketLink {
            stream,
            socket,
            generation: 0,
            sender,
        };
        link.start_reader()?;
        Ok(link)
    }

    fn start_reader(&self) -> anyhow::Result<()> {
        let stream = self.stream.try_clone().context("reading from the server")?;
        let sender = self.sender.clone();
        let generation = self.generation;
        thread::Builder::new()
            .name("emx-server-reader".to_string())
            .spawn(move || read_server(stream, generation, &sender))
            .context("starting the server reader")?;
        Ok(())
    }
}

impl Link for SocketLink {
    fn send(&mut self, message: ClientMsg) -> io::Result<()> {
        write_msg(&mut self.stream, &message)
    }

    fn generation(&self) -> u64 {
        self.generation
    }

    fn reconnect(&mut self) -> Option<anyhow::Result<()>> {
        Some(connect_or_start(&self.socket).and_then(|stream| {
            self.stream = stream;
            self.generation += 1;
            self.start_reader()
        }))
    }

    fn terminate(&mut self) -> bool {
        false
    }
}

fn read_server(stream: UnixStream, generation: u64, sender: &Sender<UiEvent>) {
    let mut reader = LineReader::new(stream);
    loop {
        let event = match reader.read_msg::<ServerMsg>() {
            Ok(Some(message)) => UiEvent::Server {
                generation,
                message,
            },
            Ok(None) => UiEvent::ServerClosed {
                generation,
                error: None,
            },
            Err(error) => UiEvent::ServerClosed {
                generation,
                error: Some(error.to_string()),
            },
        };
        let closed = matches!(event, UiEvent::ServerClosed { .. });
        if sender.send(event).is_err() || closed {
            return;
        }
    }
}

/// How the client ends: the exit code, and a line for stderr once the
/// terminal is restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct End {
    pub code: u8,
    pub message: Option<String>,
}

impl End {
    fn success() -> End {
        End {
            code: 0,
            message: None,
        }
    }

    fn failure(message: impl Into<String>) -> End {
        End {
            code: EXIT_FAILURE,
            message: Some(message.into()),
        }
    }

    fn quiet_failure() -> End {
        End {
            code: EXIT_FAILURE,
            message: None,
        }
    }

    /// Prints the message, if any, on stderr; the exit code.
    pub fn report(self) -> ExitCode {
        if let Some(message) = self.message {
            eprintln!("{message}");
        }
        ExitCode::from(self.code)
    }
}

/// Takes over the terminal, probes it, connects with `connect`, and runs
/// the client until it ends. The terminal is restored when this returns;
/// the caller reports the `End`.
pub fn run_tui(
    connect: impl FnOnce(&Sender<UiEvent>) -> anyhow::Result<Box<dyn Link>>,
    sender: Sender<UiEvent>,
    events: Receiver<UiEvent>,
    files: &[FileArgument],
    wait: bool,
    keyboard: KeyboardEncoding,
) -> anyhow::Result<End> {
    // Registered before the terminal is taken over: from here on SIGTERM
    // and SIGWINCH queue until the client handles them, instead of killing
    // it with the terminal raw or being lost during a slow startup.
    let signals =
        Signals::new([SIGWINCH, SIGTERM, SIGINT, SIGHUP]).context("installing signal handlers")?;
    let mut terminal = Terminal::enter()?;
    let outcome = session(
        &mut terminal,
        signals,
        connect,
        sender,
        &events,
        Startup {
            files,
            wait,
            keyboard,
        },
    );
    drop(terminal);
    outcome
}

/// What the client starts with: the files to open, whether to wait for
/// them, and the configured keyboard encoding.
struct Startup<'a> {
    files: &'a [FileArgument],
    wait: bool,
    keyboard: KeyboardEncoding,
}

fn session(
    terminal: &mut Terminal,
    signals: Signals,
    connect: impl FnOnce(&Sender<UiEvent>) -> anyhow::Result<Box<dyn Link>>,
    sender: Sender<UiEvent>,
    events: &Receiver<UiEvent>,
    startup: Startup,
) -> anyhow::Result<End> {
    let Startup {
        files,
        wait,
        keyboard,
    } = startup;
    let mut input = StdinSource::new();
    let configured = match keyboard {
        KeyboardEncoding::Auto => None,
        KeyboardEncoding::Kitty => Some(Protocol::Kitty),
        KeyboardEncoding::Legacy => Some(Protocol::Legacy),
    };
    let overridden = keyboard_override(std::env::var_os(KEYBOARD_ENV).as_deref())?;
    let (protocol, leftover) = match overridden.or(configured) {
        Some(protocol) => (protocol, Vec::new()),
        None => {
            let probe = probe_protocol(
                &mut input,
                &mut io::stdout(),
                Duration::from_millis(KITTY_PROBE_TIMEOUT_MS),
            )?;
            (probe.protocol, probe.leftover)
        }
    };
    terminal.set_protocol(protocol)?;
    let mut decoder = Decoder::new(protocol);
    let typed_during_probe = decoder.feed(&leftover);
    startup_delay_seam();
    let link = connect(&sender)?;
    let (cols, rows) = clamp_size(terminal.size()?);
    let cwd = std::env::current_dir().context("finding the current directory")?;
    let mut client = Client {
        link,
        decoder,
        display: Display::new(cols, rows),
        cwd,
        files: files.to_vec(),
        wait,
        waiting: BTreeSet::new(),
        drawn: false,
        reconnected: false,
        terminating: false,
        panic_after_first_frame: panic_seam_requested(),
    };
    if let Some(end) = client.greet() {
        return Ok(end);
    }
    for event in typed_during_probe {
        if let Some(end) = client.input_event(event) {
            return Ok(end);
        }
    }
    start_input_thread(input, sender.clone())?;
    start_signal_thread(signals, sender)?;
    loop {
        let event = if client.decoder.waiting_for_escape() {
            match events.recv_timeout(Duration::from_millis(LEGACY_ESCAPE_TIMEOUT_MS)) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(end) = client.escape_timeout() {
                        return Ok(end);
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("every source of client events closed")
                }
            }
        } else {
            events
                .recv()
                .context("every source of client events closed")?
        };
        if let Some(end) = client.handle(event, terminal)? {
            return Ok(end);
        }
    }
}

#[cfg(debug_assertions)]
fn panic_seam_requested() -> bool {
    std::env::var_os(CLIENT_PANIC_ENV).is_some()
}

#[cfg(not(debug_assertions))]
fn panic_seam_requested() -> bool {
    false
}

#[cfg(debug_assertions)]
fn panic_seam(requested: bool) {
    assert!(
        !requested,
        "{CLIENT_PANIC_ENV}: the client panics after its first frame"
    );
}

#[cfg(not(debug_assertions))]
fn panic_seam(_requested: bool) {}

/// Debug builds wait this many milliseconds between the probe and
/// connecting, so tests can signal a client that is still starting.
#[cfg(debug_assertions)]
pub const STARTUP_DELAY_ENV: &str = "EMX_TEST_STARTUP_DELAY_MS";

#[cfg(debug_assertions)]
fn startup_delay_seam() {
    if let Some(milliseconds) = std::env::var(STARTUP_DELAY_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
    {
        thread::sleep(Duration::from_millis(milliseconds));
    }
}

#[cfg(not(debug_assertions))]
fn startup_delay_seam() {}

/// The server refuses sizes it cannot send whole frames for; a larger
/// terminal is drawn in its top-left corner. A zero size (a terminal that
/// does not report one) is drawn as one cell.
fn clamp_size((cols, rows): (u16, u16)) -> (u16, u16) {
    (
        cols.clamp(1, MAX_TERMINAL_COLS),
        rows.clamp(1, MAX_TERMINAL_ROWS),
    )
}

fn start_input_thread(mut input: StdinSource, sender: Sender<UiEvent>) -> anyhow::Result<()> {
    thread::Builder::new()
        .name("emx-terminal-reader".to_string())
        .spawn(move || {
            let mut buffer = vec![0; READ_SIZE];
            loop {
                let event = match input.read(&mut buffer) {
                    Ok(0) => UiEvent::InputClosed,
                    Ok(count) => UiEvent::Input(buffer[..count].to_vec()),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => UiEvent::InputClosed,
                };
                let closed = matches!(event, UiEvent::InputClosed);
                if sender.send(event).is_err() || closed {
                    return;
                }
            }
        })
        .context("starting the terminal reader")?;
    Ok(())
}

fn start_signal_thread(mut signals: Signals, sender: Sender<UiEvent>) -> anyhow::Result<()> {
    thread::Builder::new()
        .name("emx-client-signals".to_string())
        .spawn(move || {
            for signal in signals.forever() {
                let event = if signal == SIGWINCH {
                    UiEvent::Resize
                } else {
                    UiEvent::Terminate
                };
                if sender.send(event).is_err() {
                    return;
                }
            }
        })
        .context("starting the signal thread")?;
    Ok(())
}

struct Client {
    link: Box<dyn Link>,
    decoder: Decoder,
    display: Display,
    cwd: PathBuf,
    files: Vec<FileArgument>,
    wait: bool,
    /// Ids of `--wait` opens not answered yet.
    waiting: BTreeSet<u64>,
    /// A frame was drawn on the current connection.
    drawn: bool,
    reconnected: bool,
    /// The client asked the editor to stop and waits for its `Exit`.
    terminating: bool,
    panic_after_first_frame: bool,
}

impl Client {
    /// Says hello and opens the files; on a lost connection, the end.
    fn greet(&mut self) -> Option<End> {
        let (cols, rows) = self.display.size();
        let hello = ClientMsg::Hello {
            kind: ClientKind::Tui,
            cwd: WirePath::from(self.cwd.as_path()),
            cols,
            rows,
            wait_for: None,
        };
        if let Err(error) = self.link.send(hello) {
            return self.lost(Some(error.to_string()));
        }
        self.waiting.clear();
        let opens: Vec<ClientMsg> = self
            .files
            .iter()
            .zip(1..)
            .map(|(file, id)| ClientMsg::Request {
                id,
                req: Request::Open {
                    path: WirePath::from(file.path.as_path()),
                    line: file.line,
                    col: file.col,
                    wait: self.wait,
                },
            })
            .collect();
        for open in opens {
            if let ClientMsg::Request { id, .. } = &open
                && self.wait
            {
                self.waiting.insert(*id);
            }
            if let Err(error) = self.link.send(open) {
                return self.lost(Some(error.to_string()));
            }
        }
        None
    }

    fn handle(&mut self, event: UiEvent, terminal: &mut Terminal) -> anyhow::Result<Option<End>> {
        Ok(match event {
            UiEvent::Input(bytes) => {
                let events = self.decoder.feed(&bytes);
                events.into_iter().find_map(|event| self.input_event(event))
            }
            UiEvent::InputClosed | UiEvent::Terminate => self.stop(),
            UiEvent::Server {
                generation,
                message,
            } => {
                if generation != self.link.generation() {
                    return Ok(None);
                }
                self.server_message(message, terminal)?
            }
            UiEvent::ServerClosed { generation, error } => {
                if generation != self.link.generation() {
                    return Ok(None);
                }
                self.lost(error)
            }
            UiEvent::Resize => {
                let (cols, rows) = clamp_size(terminal.size()?);
                if self.display.resize(cols, rows) {
                    self.send(ClientMsg::Resize { cols, rows })
                } else {
                    None
                }
            }
        })
    }

    /// A legacy terminal's ESC was not followed by the rest of a sequence.
    fn escape_timeout(&mut self) -> Option<End> {
        let events = self.decoder.timeout();
        events.into_iter().find_map(|event| self.input_event(event))
    }

    fn input_event(&mut self, event: InputEvent) -> Option<End> {
        match event {
            InputEvent::Key(key) => self.send(ClientMsg::Key { key }),
            InputEvent::Paste { data, last } => self.send(ClientMsg::Paste {
                data_b64: STANDARD.encode(data),
                r#final: last,
            }),
            // Late probe replies, sequences no key maps to, and mouse
            // reports, which are never asked for yet.
            InputEvent::Mouse(_)
            | InputEvent::KittyFlags(_)
            | InputEvent::PrimaryDeviceAttributes
            | InputEvent::Unknown(_) => None,
        }
    }

    fn send(&mut self, message: ClientMsg) -> Option<End> {
        match self.link.send(message) {
            Ok(()) => None,
            Err(error) => self.lost(Some(error.to_string())),
        }
    }

    fn server_message(
        &mut self,
        message: ServerMsg,
        terminal: &mut Terminal,
    ) -> anyhow::Result<Option<End>> {
        Ok(match message {
            ServerMsg::Welcome { .. } | ServerMsg::Event { .. } => None,
            ServerMsg::Frame { .. } => match self.display.apply(&message) {
                FrameOutcome::Drawn(output) => {
                    terminal.write(&output)?;
                    self.drawn = true;
                    panic_seam(self.panic_after_first_frame);
                    None
                }
                FrameOutcome::Skipped => None,
                FrameOutcome::Unusable { request_redraw } => {
                    if request_redraw {
                        self.send(ClientMsg::Redraw)
                    } else {
                        None
                    }
                }
            },
            ServerMsg::Bell => {
                terminal.write(BELL)?;
                None
            }
            ServerMsg::Response { id, result } => {
                if !self.waiting.remove(&id) {
                    // Answers to plain opens: the editor shows its own
                    // errors in the echo area.
                    return Ok(None);
                }
                match result {
                    Err(error) => Some(End::failure(format!("emx: {}", error.message))),
                    Ok(_) if self.waiting.is_empty() => Some(End::success()),
                    Ok(_) => None,
                }
            }
            ServerMsg::Exit { reason } => Some(if self.terminating {
                End::quiet_failure()
            } else if reason.is_empty() {
                End::success()
            } else {
                End::failure(format!("emx: {reason}"))
            }),
        })
    }

    /// The connection ended. One that ends before the first frame is
    /// retried once: the server may have been exiting as it was reached.
    fn lost(&mut self, error: Option<String>) -> Option<End> {
        if !self.drawn && !self.reconnected && !self.terminating {
            self.reconnected = true;
            if let Some(reconnected) = self.link.reconnect() {
                return match reconnected {
                    Ok(()) => {
                        let (cols, rows) = self.display.size();
                        self.display = Display::new(cols, rows);
                        self.greet()
                    }
                    Err(error) => Some(End::failure(format!("{CONNECTION_LOST}: {error:#}"))),
                };
            }
        }
        if self.terminating {
            return Some(End::quiet_failure());
        }
        Some(End::failure(match error {
            Some(error) => format!("{CONNECTION_LOST}: {error}"),
            None => CONNECTION_LOST.to_string(),
        }))
    }

    /// A signal or a hung-up terminal: a server keeps the session; the
    /// in-process editor is asked to stop and its `Exit` awaited.
    fn stop(&mut self) -> Option<End> {
        if self.terminating {
            return None;
        }
        if self.link.terminate() {
            self.terminating = true;
            None
        } else {
            Some(End::quiet_failure())
        }
    }
}

/// What became of a frame.
#[derive(Debug, PartialEq, Eq)]
enum FrameOutcome {
    /// Terminal output that shows it.
    Drawn(Vec<u8>),
    /// A diff while a whole frame is awaited.
    Skipped,
    /// Malformed, or not for this terminal's size: discarded, and a whole
    /// frame is needed. `request_redraw` is false when one is already on
    /// its way.
    Unusable { request_redraw: bool },
}

/// What the terminal shows, kept as the grid the server's frames build.
struct Display {
    cols: u16,
    rows: u16,
    /// `None` until a whole frame arrives.
    grid: Option<Grid>,
    screen: Screen,
    /// A whole frame was asked for (or is due after a resize) and has not
    /// arrived.
    whole_frame_due: bool,
}

impl Display {
    /// The server's first frame for a client is always whole.
    fn new(cols: u16, rows: u16) -> Display {
        Display {
            cols,
            rows,
            grid: None,
            screen: Screen::new(),
            whole_frame_due: true,
        }
    }

    fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Returns whether the size changed; the server then sends a whole
    /// frame of the new size.
    fn resize(&mut self, cols: u16, rows: u16) -> bool {
        if (cols, rows) == (self.cols, self.rows) {
            return false;
        }
        self.cols = cols;
        self.rows = rows;
        self.grid = None;
        self.screen.invalidate();
        self.whole_frame_due = true;
        true
    }

    /// Frames come from the network: each is checked against this
    /// terminal before it touches the grid.
    fn apply(&mut self, message: &ServerMsg) -> FrameOutcome {
        let ServerMsg::Frame {
            full,
            cols,
            rows,
            rows_changed,
            cursor,
        } = message
        else {
            unreachable!("apply is given frames only");
        };
        if message.validate(self.cols).is_err()
            || (*cols, *rows) != (self.cols, self.rows)
            || rows_changed
                .iter()
                .any(|update| update.cells.len() != usize::from(self.cols))
        {
            return self.unusable();
        }
        if *full {
            self.grid = Some(Grid::new(self.cols, self.rows));
        }
        let Some(grid) = self.grid.as_mut() else {
            return FrameOutcome::Skipped;
        };
        let width = usize::from(self.cols);
        for update in rows_changed {
            let start = usize::from(update.row) * width;
            grid.cells[start..start + width].clone_from_slice(&update.cells);
        }
        grid.cursor = *cursor;
        let mut output = Vec::new();
        if self.screen.draw(grid, &mut output).is_err() {
            return self.unusable();
        }
        if *full {
            self.whole_frame_due = false;
        }
        FrameOutcome::Drawn(output)
    }

    fn unusable(&mut self) -> FrameOutcome {
        self.grid = None;
        self.screen.invalidate();
        let request_redraw = !self.whole_frame_due;
        self.whole_frame_due = true;
        FrameOutcome::Unusable { request_redraw }
    }
}

#[cfg(test)]
mod tests {
    use emx_proto::RowUpdate;
    use emx_types::{Cell, Face};

    use super::*;

    fn parse(argument: &str, existing: &[&str]) -> FileArgument {
        parse_file_argument(OsStr::new(argument), &|path| {
            existing.iter().any(|name| Path::new(name) == path)
        })
    }

    fn file(path: &str, line: Option<u64>, col: Option<u64>) -> FileArgument {
        FileArgument {
            path: PathBuf::from(path),
            line,
            col,
        }
    }

    #[test]
    fn file_line_and_column_suffixes() {
        assert_eq!(parse("a.txt", &[]), file("a.txt", None, None));
        assert_eq!(parse("a.txt:12", &[]), file("a.txt", Some(12), None));
        assert_eq!(parse("a.txt:12:3", &[]), file("a.txt", Some(12), Some(3)));
        assert_eq!(parse("a:b.txt:7", &[]), file("a:b.txt", Some(7), None));
    }

    #[test]
    fn a_suffix_that_is_not_numbers_is_part_of_the_name() {
        assert_eq!(parse("a.txt:x", &[]), file("a.txt:x", None, None));
        assert_eq!(parse("a.txt:", &[]), file("a.txt:", None, None));
        assert_eq!(parse("a.txt:+4", &[]), file("a.txt:+4", None, None));
        assert_eq!(parse("a.txt:x:4", &[]), file("a.txt:x", Some(4), None));
        assert_eq!(parse(":12", &[]), file(":12", None, None));
    }

    #[test]
    fn an_existing_file_named_like_a_position_is_opened_whole() {
        assert_eq!(
            parse("a.txt:12", &["a.txt:12"]),
            file("a.txt:12", None, None)
        );
        assert_eq!(
            parse("a.txt:12:3", &["a.txt:12"]),
            file("a.txt", Some(12), Some(3)),
            "only the whole argument decides"
        );
    }

    #[test]
    fn non_utf8_names_keep_their_bytes() {
        let argument = OsString::from_vec(b"caf\xe9.txt:2".to_vec());
        let parsed = parse_file_argument(&argument, &|_| false);
        assert_eq!(parsed.path.as_os_str().as_bytes(), b"caf\xe9.txt");
        assert_eq!(parsed.line, Some(2));
    }

    fn row(text: &str, cols: u16) -> Vec<Cell> {
        let mut cells: Vec<Cell> = text
            .chars()
            .map(|character| Cell {
                text: character.to_string(),
                width: 1,
                face: Face::Default,
            })
            .collect();
        cells.resize(usize::from(cols), Cell::blank());
        cells
    }

    fn frame(full: bool, cols: u16, rows: u16, updates: &[(u16, &str)]) -> ServerMsg {
        ServerMsg::Frame {
            full,
            cols,
            rows,
            rows_changed: updates
                .iter()
                .map(|(row_number, text)| RowUpdate {
                    row: *row_number,
                    cells: row(text, cols),
                })
                .collect(),
            cursor: Some((0, 0)),
        }
    }

    fn whole(cols: u16, rows: u16, text: &str) -> ServerMsg {
        let updates: Vec<(u16, &str)> = (0..rows).map(|row_number| (row_number, text)).collect();
        frame(true, cols, rows, &updates)
    }

    #[test]
    fn whole_frames_then_diffs_are_drawn() {
        let mut display = Display::new(4, 2);
        assert!(matches!(
            display.apply(&whole(4, 2, "ab")),
            FrameOutcome::Drawn(_)
        ));
        let FrameOutcome::Drawn(output) = display.apply(&frame(false, 4, 2, &[(1, "xy")])) else {
            panic!("a diff on a whole frame is drawn");
        };
        assert!(String::from_utf8_lossy(&output).contains("xy"));
    }

    #[test]
    fn a_frame_for_another_size_is_refused_and_a_redraw_asked_once() {
        let mut display = Display::new(4, 2);
        display.apply(&whole(4, 2, "ab"));
        assert_eq!(
            display.apply(&whole(5, 2, "ab")),
            FrameOutcome::Unusable {
                request_redraw: true
            }
        );
        assert_eq!(
            display.apply(&frame(false, 4, 2, &[(0, "zz")])),
            FrameOutcome::Skipped,
            "diffs wait for the whole frame"
        );
        assert_eq!(
            display.apply(&whole(4, 3, "ab")),
            FrameOutcome::Unusable {
                request_redraw: false
            },
            "a redraw is already on its way"
        );
        assert!(matches!(
            display.apply(&whole(4, 2, "ok")),
            FrameOutcome::Drawn(_)
        ));
    }

    #[test]
    fn malformed_frames_are_refused_never_drawn() {
        let mut display = Display::new(4, 2);
        display.apply(&whole(4, 2, "ab"));
        let outside = ServerMsg::Frame {
            full: false,
            cols: 4,
            rows: 2,
            rows_changed: vec![RowUpdate {
                row: 2,
                cells: row("", 4),
            }],
            cursor: None,
        };
        assert_eq!(
            display.apply(&outside),
            FrameOutcome::Unusable {
                request_redraw: true
            }
        );
        display.apply(&whole(4, 2, "ab"));
        let short = ServerMsg::Frame {
            full: false,
            cols: 4,
            rows: 2,
            rows_changed: vec![RowUpdate {
                row: 0,
                cells: row("ab", 2),
            }],
            cursor: None,
        };
        assert!(matches!(
            display.apply(&short),
            FrameOutcome::Unusable { .. }
        ));
        display.apply(&whole(4, 2, "ab"));
        let mut cells = row("", 4);
        cells[3].width = 2;
        let wide_in_last_column = ServerMsg::Frame {
            full: true,
            cols: 4,
            rows: 2,
            rows_changed: vec![
                RowUpdate { row: 0, cells },
                RowUpdate {
                    row: 1,
                    cells: row("", 4),
                },
            ],
            cursor: None,
        };
        assert!(matches!(
            display.apply(&wide_in_last_column),
            FrameOutcome::Unusable { .. }
        ));
        let cursor_outside = ServerMsg::Frame {
            full: true,
            cols: 4,
            rows: 2,
            rows_changed: vec![],
            cursor: Some((9, 9)),
        };
        assert!(matches!(
            display.apply(&cursor_outside),
            FrameOutcome::Unusable { .. }
        ));
    }

    #[test]
    fn a_resize_waits_for_a_whole_frame_of_the_new_size() {
        let mut display = Display::new(4, 2);
        display.apply(&whole(4, 2, "ab"));
        assert!(display.resize(6, 3));
        assert!(!display.resize(6, 3), "unchanged");
        assert_eq!(
            display.apply(&frame(false, 4, 2, &[(0, "late")])),
            FrameOutcome::Unusable {
                request_redraw: false
            },
            "a frame of the old size: the resize brings a whole one"
        );
        assert!(matches!(
            display.apply(&whole(6, 3, "new")),
            FrameOutcome::Drawn(_)
        ));
    }

    #[test]
    fn a_server_of_another_user_is_refused() {
        let socket = Path::new("/run/user/1000/emx/server.sock");
        assert!(check_server_uid(socket, 1000, 1000).is_ok());
        let error = check_server_uid(socket, 0, 1000).expect_err("refused");
        assert_eq!(
            error.to_string(),
            "refusing the server at /run/user/1000/emx/server.sock: it runs as uid 0, not uid 1000"
        );
    }

    #[test]
    fn sizes_are_clamped_to_what_the_server_accepts() {
        assert_eq!(clamp_size((0, 0)), (1, 1));
        assert_eq!(
            clamp_size((5000, 900)),
            (MAX_TERMINAL_COLS, MAX_TERMINAL_ROWS)
        );
        assert_eq!(clamp_size((80, 24)), (80, 24));
    }
}
