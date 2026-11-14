//! Shared by the server tests: a private environment (socket, state and
//! config directories in a temporary directory, never the user's), the
//! real `emx server` process, killed when dropped, and a client that
//! speaks the wire protocol directly and keeps the screen its frames draw.

// Each test file uses a different part of this module; the rest would
// be reported as dead code in that test crate.
#![allow(dead_code)]

pub mod pty;

use std::fs;
use std::io::{BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use emx_proto::{
    ClientKind, ClientMsg, LineReader, Request, Response, ServerMsg, WireError, WirePath, write_msg,
};
use emx_types::{Key, KeyCode, Mods, parse_kbd};

/// Generous: the tests run in parallel on loaded machines.
pub const WAIT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(10);
pub const COLS: u16 = 80;
pub const ROWS: u16 = 24;

/// Directories for one test's session.
pub struct TestEnv {
    root: tempfile::TempDir,
    pub socket_dir: PathBuf,
    pub state_home: PathBuf,
    pub config_home: PathBuf,
    pub home: PathBuf,
    /// Where test files go, and the clients' working directory.
    pub work: PathBuf,
}

impl TestEnv {
    pub fn new() -> TestEnv {
        let root = tempfile::tempdir().expect("tempdir");
        let path = |name: &str| {
            let directory = root.path().join(name);
            fs::create_dir_all(&directory).expect("create test directory");
            directory
        };
        let state_home = path("state");
        let config_home = path("config");
        let home = path("home");
        let work = path("work");
        TestEnv {
            socket_dir: root.path().join("sockets"),
            state_home,
            config_home,
            home,
            work,
            root,
        }
    }

    /// A session whose config file holds `toml`.
    pub fn with_config(toml: &str) -> TestEnv {
        let env = TestEnv::new();
        let directory = env.config_home.join("emx");
        fs::create_dir_all(&directory).expect("config directory");
        fs::write(directory.join("config.toml"), toml).expect("config file");
        env
    }

    pub fn socket_path(&self) -> PathBuf {
        self.socket_dir.join("server.sock")
    }

    pub fn journal_dir(&self) -> PathBuf {
        self.state_home.join("emx").join("server")
    }

    /// `emx` with this session's environment.
    pub fn emx(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_emx"));
        command
            .env("EMX_SOCKET_DIR", &self.socket_dir)
            .env("XDG_STATE_HOME", &self.state_home)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("HOME", &self.home)
            .env_remove("XDG_RUNTIME_DIR")
            .current_dir(&self.work);
        command
    }

    /// Starts `emx server` and waits until it accepts connections.
    pub fn start_server(&self) -> ServerProcess {
        self.start_server_with(&[])
    }

    /// `start_server` with extra variables set for the server.
    pub fn start_server_with(&self, variables: &[(&str, &str)]) -> ServerProcess {
        let stderr = fs::File::create(self.root.path().join("server-stderr.log"))
            .expect("server stderr file");
        let mut command = self.emx();
        for (name, value) in variables {
            command.env(name, value);
        }
        let child = command
            .arg("server")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .expect("spawn emx server");
        let server = ServerProcess { child };
        self.wait_for_socket(&server);
        server
    }

    pub fn wait_for_socket(&self, server: &ServerProcess) {
        let deadline = Instant::now() + WAIT;
        while UnixStream::connect(self.socket_path()).is_err() {
            assert!(
                Instant::now() < deadline,
                "the server never listened; stderr:\n{}",
                self.server_stderr()
            );
            assert!(
                server.is_running_pid(),
                "the server exited: {}",
                self.server_stderr()
            );
            thread::sleep(POLL);
        }
    }

    pub fn server_stderr(&self) -> String {
        fs::read_to_string(self.root.path().join("server-stderr.log")).unwrap_or_default()
    }

    /// Writes `content` to `name` in the work directory.
    pub fn file(&self, name: &str, content: &str) -> PathBuf {
        let path = self.work.join(name);
        fs::write(&path, content).expect("write test file");
        path
    }

    pub fn connect(&self, kind: ClientKind) -> RawClient {
        RawClient::connect(&self.socket_path(), &self.work, kind)
    }
}

/// The `emx server` process; killed and reaped when dropped.
pub struct ServerProcess {
    child: Child,
}

impl ServerProcess {
    pub fn adopt(child: Child) -> ServerProcess {
        ServerProcess { child }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    fn is_running_pid(&self) -> bool {
        Path::new(&format!("/proc/{}", self.child.id())).exists()
    }

    /// `kill -9`, then reap.
    pub fn kill9(&mut self) {
        self.child.kill().expect("kill -9 the server");
        self.child.wait().expect("reap the server");
    }

    /// The exit status, if the server exits within `limit`.
    pub fn wait_exit(&mut self, limit: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(POLL);
        }
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            // Already gone is fine; a test may have killed it.
            drop(self.child.kill());
            drop(self.child.wait());
        }
    }
}

/// A client speaking the wire protocol. Frames update `screen`; other
/// messages are kept for the test to look at.
pub struct RawClient {
    writer: BufWriter<UnixStream>,
    stream: UnixStream,
    incoming: Receiver<ServerMsg>,
    rows: Vec<Vec<emx_types::Cell>>,
    pub cursor: Option<(u16, u16)>,
    /// Messages other than frames, not yet looked at.
    pub messages: Vec<ServerMsg>,
    pub frames: usize,
    next_id: u64,
    pub server_pid: u32,
}

impl RawClient {
    pub fn connect(socket: &Path, cwd: &Path, kind: ClientKind) -> RawClient {
        let stream = UnixStream::connect(socket).expect("connect to the server");
        let reader = stream.try_clone().expect("clone stream");
        let (sender, incoming) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = LineReader::new(reader);
            while let Ok(Some(message)) = reader.read_msg::<ServerMsg>() {
                if sender.send(message).is_err() {
                    return;
                }
            }
        });
        let mut client = RawClient {
            writer: BufWriter::new(stream.try_clone().expect("clone stream")),
            stream,
            incoming,
            rows: Vec::new(),
            cursor: None,
            messages: Vec::new(),
            frames: 0,
            next_id: 1,
            server_pid: 0,
        };
        client.send(&ClientMsg::Hello {
            kind,
            cwd: WirePath::from(cwd),
            cols: COLS,
            rows: ROWS,
            wait_for: None,
        });
        let welcome = client.wait_message(|message| matches!(message, ServerMsg::Welcome { .. }));
        let ServerMsg::Welcome { server_pid } = welcome else {
            unreachable!("matched above");
        };
        client.server_pid = server_pid;
        client
    }

    pub fn send(&mut self, message: &ClientMsg) {
        write_msg(&mut self.writer, message).expect("send to the server");
    }

    /// Sends one line as it is, for messages the types cannot express.
    pub fn send_raw(&mut self, line: &str) {
        self.try_send_raw(line).expect("send to the server");
    }

    /// Like `send_raw`, for a connection the server may have closed.
    pub fn try_send_raw(&mut self, line: &str) -> std::io::Result<()> {
        self.writer
            .write_all(format!("{line}\n").as_bytes())
            .and_then(|()| self.writer.flush())
    }

    /// Waits until the cursor is at (`col`, `row`).
    pub fn wait_cursor(&mut self, cursor: (u16, u16)) {
        let deadline = Instant::now() + WAIT;
        while self.cursor != Some(cursor) {
            assert!(
                Instant::now() < deadline,
                "cursor at {:?}, not {cursor:?}; screen:\n{}",
                self.cursor,
                self.screen().join("\n")
            );
            if let Some(message) = self.receive(POLL)
                && !matches!(message, ServerMsg::Frame { .. })
            {
                self.messages.push(message);
            }
        }
    }

    /// Sends an Emacs `kbd` sequence.
    pub fn keys(&mut self, kbd: &str) {
        for key in parse_kbd(kbd).expect("valid kbd") {
            self.send(&ClientMsg::Key { key });
        }
    }

    pub fn key(&mut self, key: Key) {
        self.send(&ClientMsg::Key { key });
    }

    pub fn type_text(&mut self, text: &str) {
        for character in text.chars() {
            self.key(Key {
                code: KeyCode::Char(character),
                mods: Mods::default(),
            });
        }
    }

    pub fn paste_chunk(&mut self, data: &[u8], last: bool) {
        self.send(&ClientMsg::Paste {
            data_b64: STANDARD.encode(data),
            r#final: last,
        });
    }

    pub fn request(&mut self, request: Request) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&ClientMsg::Request { id, req: request });
        id
    }

    /// Sends `request` and waits for its answer.
    pub fn call(&mut self, request: Request) -> Result<Response, WireError> {
        let id = self.request(request);
        self.response(id)
    }

    pub fn response(&mut self, id: u64) -> Result<Response, WireError> {
        let message = self.wait_message(
            |message| matches!(message, ServerMsg::Response { id: answered, .. } if *answered == id),
        );
        let ServerMsg::Response { result, .. } = message else {
            unreachable!("matched above");
        };
        result
    }

    /// Opens `path` and waits until it is open.
    pub fn open(&mut self, path: &Path) {
        let result = self.call(Request::Open {
            path: WirePath::from(path),
            line: None,
            col: None,
            wait: false,
        });
        assert_eq!(result, Ok(Response::Ok), "opening {}", path.display());
    }

    /// Takes one incoming message, applying frames to the screen.
    fn receive(&mut self, limit: Duration) -> Option<ServerMsg> {
        match self.incoming.recv_timeout(limit) {
            Ok(message) => {
                if let ServerMsg::Frame {
                    full,
                    cols,
                    rows,
                    rows_changed,
                    cursor,
                } = &message
                {
                    message
                        .validate(COLS)
                        .expect("the server sends valid frames");
                    if *full {
                        self.rows = vec![Vec::new(); usize::from(*rows)];
                    }
                    assert_eq!((*cols, self.rows.len()), (COLS, usize::from(ROWS)));
                    for update in rows_changed {
                        self.rows[usize::from(update.row)] = update.cells.clone();
                    }
                    self.cursor = *cursor;
                    self.frames += 1;
                }
                Some(message)
            }
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => None,
        }
    }

    /// Waits for a message matching `want`, keeping the others.
    pub fn wait_message(&mut self, want: impl Fn(&ServerMsg) -> bool) -> ServerMsg {
        if let Some(index) = self.messages.iter().position(&want) {
            return self.messages.remove(index);
        }
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            let Some(message) = self.receive(POLL) else {
                continue;
            };
            if want(&message) {
                return message;
            }
            if !matches!(message, ServerMsg::Frame { .. }) {
                self.messages.push(message);
            }
        }
        panic!(
            "no matching message; kept {:?}; screen:\n{}",
            self.messages,
            self.screen().join("\n")
        );
    }

    /// Whether the connection was closed by the server within `limit`.
    pub fn closed_within(&mut self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            match self.incoming.recv_timeout(POLL) {
                Ok(message) => {
                    if !matches!(message, ServerMsg::Frame { .. }) {
                        self.messages.push(message);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return true,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        false
    }

    /// Every row as text.
    pub fn screen(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|cells| {
                cells
                    .iter()
                    .filter(|cell| cell.width > 0)
                    .map(|cell| cell.text.as_str())
                    .collect()
            })
            .collect()
    }

    pub fn modeline(&self) -> String {
        let screen = self.screen();
        screen
            .get(screen.len().saturating_sub(2))
            .cloned()
            .unwrap_or_default()
    }

    /// Waits until the screen satisfies `want`; returns it.
    pub fn wait_screen(&mut self, want: impl Fn(&[String]) -> bool) -> Vec<String> {
        let deadline = Instant::now() + WAIT;
        loop {
            let screen = self.screen();
            if want(&screen) {
                return screen;
            }
            assert!(
                Instant::now() < deadline,
                "screen never matched:\n{}",
                screen.join("\n")
            );
            if let Some(message) = self.receive(POLL)
                && !matches!(message, ServerMsg::Frame { .. })
            {
                self.messages.push(message);
            }
        }
    }

    /// Waits until some row contains `text`.
    pub fn wait_text(&mut self, text: &str) -> Vec<String> {
        self.wait_screen(|screen| screen.iter().any(|row| row.contains(text)))
    }

    /// Hangs up, as a closed terminal does.
    pub fn disconnect(self) {
        self.stream
            .shutdown(std::net::Shutdown::Both)
            .expect("shutdown");
    }
}
