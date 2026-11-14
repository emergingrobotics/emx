//! The session server end to end: the real `emx server` process, driven
//! over its socket by clients that speak the wire protocol.

mod common;

use std::fs;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use common::pty::SessionReaper;
use common::{ServerProcess, TestEnv, WAIT};
use emx::server::client_conn::{AdmitError, Outbox, admit, peer_uid};
use emx::server::journal_writer::{JOURNAL_IDLE_SYNC_MS, JournalBackend, JournalThread};
use emx::server::workers::WORKER_PANIC_ENV;
use emx::server::{Input, Server, Step};
use emx_core::config::DAEMON_GRACE_MS;
use emx_core::journal::{
    JournalError, JournalOp, JournalWriter, RecoveredBuffer, SyncFailure, replay_dir,
};
use emx_core::{ClientId, Config, Event, Session};
use emx_proto::{ClientKind, Request, Response, ServerMsg, WirePath};
use emx_types::parse_kbd;

const GRACE_SLACK_MS: u64 = 1000;
const STEP: Duration = Duration::from_millis(5);
const FRESH_JOURNAL_MAX: u64 = 120;

fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn signal(server: &ServerProcess, signal: rustix::process::Signal) {
    let pid = rustix::process::Pid::from_raw(server.pid() as i32).expect("a pid");
    rustix::process::kill_process(pid, signal).expect("signal the server");
}

/// `emx server --detach` from `starter`: it returns once the server
/// listens, and the server it leaves is checked to be detached.
fn detach_and_attach(env: &TestEnv, mut starter: std::process::Command) {
    let mut child = starter
        .args(["server", "--detach"])
        .spawn()
        .expect("spawn emx server --detach");
    let starter_pid = child.id();
    let status = child.wait().expect("wait for the starter");
    assert!(status.success(), "{status}");
    let mut client = env.connect(ClientKind::Tui);
    let server = client.server_pid;
    assert_ne!(server, starter_pid, "the starter re-spawns the server");
    let screen = client.wait_text("*scratch*");
    assert!(
        screen[screen.len() - 2].contains(":@---  F2  *scratch*"),
        "{screen:?}"
    );
    // setsid: the server leads its own session, away from the terminal.
    let stat = fs::read_to_string(format!("/proc/{server}/stat")).expect("stat");
    let fields: Vec<&str> = stat
        .rsplit(')')
        .next()
        .expect("fields")
        .split_whitespace()
        .collect();
    assert_eq!(fields[3], server.to_string(), "session id is the pid");
    let log = env.state_home.join("emx").join("server.log");
    assert!(log.exists(), "stderr goes to the log");
    let descriptor = |number: u32| {
        fs::read_link(format!("/proc/{server}/fd/{number}")).expect("server descriptor")
    };
    assert_eq!(descriptor(0), Path::new("/dev/null"));
    assert_eq!(descriptor(1), Path::new("/dev/null"));
    assert_eq!(descriptor(2), log);
    let mode = fs::metadata(&env.socket_dir)
        .expect("socket dir")
        .permissions();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode) & 0o7777,
        0o700
    );
}

#[test]
fn autostart_and_attach() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    detach_and_attach(&env, env.emx());
}

#[test]
fn detach_works_from_a_process_group_leader() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    // What an interactive shell does to every job: `setsid` would fail in
    // this process, so the server must be a new child.
    let mut starter = env.emx();
    std::os::unix::process::CommandExt::process_group(&mut starter, 0);
    detach_and_attach(&env, starter);
}

#[test]
fn detach_reattach_keeps_unsaved_edits() {
    let env = TestEnv::new();
    let path = env.file("notes.txt", "base\n");
    let _server = env.start_server();
    let mut first = env.connect(ClientKind::Tui);
    first.open(&path);
    first.type_text("edit ");
    first.wait_text("edit base");
    first.disconnect();
    let mut second = env.connect(ClientKind::Tui);
    second.open(&path);
    let screen = second.wait_text("edit base");
    assert!(
        screen[screen.len() - 2].starts_with("-UU-:@**-  F3  notes.txt"),
        "{screen:?}"
    );
    assert_eq!(fs::read_to_string(&path).expect("read"), "base\n");
}

#[test]
fn two_clients_same_buffer_points_shift() {
    let env = TestEnv::new();
    let path = env.file("shared.txt", "abc\n");
    let _server = env.start_server();
    let mut first = env.connect(ClientKind::Tui);
    let mut second = env.connect(ClientKind::Tui);
    first.open(&path);
    second.open(&path);
    second.keys("C-e");
    second.wait_cursor((3, 0));
    first.type_text("XY");
    first.wait_cursor((2, 0));
    second.wait_text("XYabc");
    second.wait_cursor((5, 0));
}

#[test]
fn kill_buffer_shown_in_other_client() {
    let env = TestEnv::new();
    let path = env.file("doomed.txt", "doomed\n");
    let _server = env.start_server();
    let mut first = env.connect(ClientKind::Tui);
    let mut second = env.connect(ClientKind::Tui);
    first.open(&path);
    second.open(&path);
    second.wait_text("doomed.txt");
    first.keys("C-x k RET");
    second.wait_screen(|screen| {
        let modeline = &screen[screen.len() - 2];
        modeline.contains("*scratch*") && !screen.iter().any(|row| row.contains("doomed"))
    });
}

#[test]
fn server_exits_when_idle_and_clean() {
    let env = TestEnv::new();
    let mut server = env.start_server();
    let client = env.connect(ClientKind::Tui);
    client.disconnect();
    let started = Instant::now();
    let status = server
        .wait_exit(Duration::from_millis(DAEMON_GRACE_MS + GRACE_SLACK_MS))
        .unwrap_or_else(|| panic!("still running after {:?}", started.elapsed()));
    assert!(status.success(), "{status:?}: {}", env.server_stderr());
    assert!(!env.socket_path().exists(), "the socket is removed");
}

#[test]
fn server_exits_when_nobody_ever_attaches() {
    let env = TestEnv::new();
    let mut server = env.start_server();
    assert!(
        server
            .wait_exit(Duration::from_millis(DAEMON_GRACE_MS + GRACE_SLACK_MS))
            .is_some()
    );
}

#[test]
fn server_stays_with_unsaved() {
    let env = TestEnv::new();
    let path = env.file("unsaved.txt", "x\n");
    let mut server = env.start_server();
    let mut client = env.connect(ClientKind::Tui);
    client.open(&path);
    client.type_text("changed ");
    client.wait_text("changed x");
    client.disconnect();
    assert!(
        server
            .wait_exit(Duration::from_millis(DAEMON_GRACE_MS + GRACE_SLACK_MS))
            .is_none(),
        "unsaved edits keep the server"
    );
    let mut again = env.connect(ClientKind::Tui);
    again.open(&path);
    again.wait_text("changed x");
}

#[test]
fn quitting_a_client_detaches_it_and_keeps_the_session() {
    let env = TestEnv::new();
    let path = env.file("kept.txt", "kept\n");
    let mut server = env.start_server();
    let mut client = env.connect(ClientKind::Tui);
    client.open(&path);
    client.type_text("more ");
    client.wait_text("more kept");
    client.keys("C-x C-c");
    client.wait_text("Save file");
    client.keys("n");
    assert!(client.closed_within(WAIT), "the server lets the client go");
    assert!(server.wait_exit(Duration::from_millis(100)).is_none());
    let mut again = env.connect(ClientKind::Tui);
    again.open(&path);
    again.wait_text("more kept");
}

#[test]
fn a_second_server_is_refused() {
    let env = TestEnv::new();
    let _server = env.start_server();
    let output = env.emx().arg("server").output().expect("second server");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("another emx server is running"), "{stderr}");
    env.connect(ClientKind::Tui).wait_text("*scratch*");
}

#[test]
fn stale_socket_is_replaced() {
    let env = TestEnv::new();
    fs::create_dir(&env.socket_dir).expect("socket dir");
    fs::set_permissions(
        &env.socket_dir,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("chmod");
    drop(UnixListener::bind(env.socket_path()).expect("stale socket"));
    assert!(env.socket_path().exists());
    let _server = env.start_server();
    env.connect(ClientKind::Tui).wait_text("*scratch*");
}

#[test]
fn unsafe_socket_dir_is_refused() {
    let env = TestEnv::new();
    fs::create_dir(&env.socket_dir).expect("socket dir");
    fs::set_permissions(
        &env.socket_dir,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .expect("chmod");
    let output = env.emx().arg("server").output().expect("server");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("mode 0755"), "{stderr}");
}

#[test]
fn sigterm_flushes_the_journal_and_exits() {
    let env = TestEnv::new();
    let path = env.file("term.txt", "t\n");
    let mut server = env.start_server();
    let mut client = env.connect(ClientKind::Tui);
    client.open(&path);
    client.type_text("abc");
    client.wait_text("abct");
    signal(&server, rustix::process::Signal::TERM);
    let status = server.wait_exit(WAIT).expect("exits on SIGTERM");
    assert!(status.success(), "{status:?}");
    assert!(!env.socket_path().exists());
    let recovered = replay_dir(&env.journal_dir(), &Config::default()).expect("replay");
    assert_eq!(recovered.len(), 1, "the unsaved buffer stays journaled");
}

#[test]
fn sighup_is_ignored() {
    let env = TestEnv::new();
    let mut server = env.start_server();
    let mut client = env.connect(ClientKind::Tui);
    signal(&server, rustix::process::Signal::HUP);
    assert!(server.wait_exit(Duration::from_millis(200)).is_none());
    client.keys("a");
    client.wait_text("a");
}

#[test]
fn cli_open_shows_the_file_in_the_terminal() {
    let env = TestEnv::new();
    let path = env.file("agent.rs", "fn main() {}\n");
    let _server = env.start_server();
    let mut terminal = env.connect(ClientKind::Tui);
    terminal.wait_text("*scratch*");
    let mut cli = env.connect(ClientKind::Cli);
    let relative = Request::Open {
        path: WirePath::Text("./agent.rs".to_string()),
        line: None,
        col: None,
        wait: false,
    };
    assert_eq!(cli.call(relative), Ok(Response::Ok));
    terminal.wait_text("fn main() {}");
    assert!(terminal.modeline().contains("agent.rs"));
    drop(path);
}

#[test]
fn paste_chunks_accumulate() {
    let env = TestEnv::new();
    let _server = env.start_server();
    let mut client = env.connect(ClientKind::Tui);
    client.paste_chunk(b"first ", false);
    client.paste_chunk(b"second ", false);
    client.keys("C-g");
    client.paste_chunk(b"third", true);
    client.wait_text("first second third");
    client.keys("C-/");
    client.wait_screen(|screen| !screen.iter().any(|row| row.contains("first")));
}

#[test]
fn yank_pop_per_client() {
    let env = TestEnv::new();
    let _server = env.start_server();
    let mut first = env.connect(ClientKind::Tui);
    let mut second = env.connect(ClientKind::Tui);
    first.type_text("one");
    first.keys("C-a C-k");
    first.type_text("two");
    first.keys("C-a C-k C-y");
    second.wait_text("two");
    second.keys("C-e");
    second.type_text("X");
    second.keys("C-SPC");
    second.wait_text("twoX");
    first.keys("M-y");
    first.wait_text("oneX");
    second.wait_text("oneX");
}

/// A client can send key objects no terminal produces; none may bring the
/// daemon down.
#[test]
fn ctrl_char_key_never_panics() {
    let env = TestEnv::new();
    let mut server = env.start_server();
    let mut client = env.connect(ClientKind::Tui);
    client.type_text("ab");
    client.wait_text("ab");
    // A key that is not valid `kbd` breaks the protocol and only ends
    // that connection.
    for code in (0u32..0x20).chain([0x7f]) {
        for prefix in ["", "M-", "C-", "S-"] {
            let key = format!("{prefix}{}", char::from_u32(code).expect("ascii"));
            let line = serde_json::json!({ "type": "key", "key": key, "v": 1 }).to_string();
            if client.try_send_raw(&line).is_err() {
                client = env.connect(ClientKind::Tui);
            }
        }
    }
    drop(client);
    let mut probe = env.connect(ClientKind::Tui);
    probe.keys("C-e");
    probe.type_text("!");
    probe.wait_text("!");
    assert!(server.wait_exit(Duration::from_millis(10)).is_none());
}

#[test]
fn malformed_messages_disconnect_only_that_client() {
    let env = TestEnv::new();
    let mut server = env.start_server();
    let mut bad = env.connect(ClientKind::Tui);
    let mut good = env.connect(ClientKind::Tui);
    bad.send_raw("{\"type\":\"resize\",\"cols\":60000,\"rows\":60000,\"v\":1}");
    assert!(bad.closed_within(WAIT));
    good.keys("z");
    good.wait_text("z");
    assert!(server.wait_exit(Duration::from_millis(10)).is_none());
}

#[test]
fn peer_uid_checked() {
    let (server_side, _client_side) = UnixStream::pair().expect("socket pair");
    let refused = admit(&server_side, euid(), &|_| Ok(euid() + 1));
    assert!(
        matches!(refused, Err(AdmitError::ForeignPeer { peer, euid: owner }) if peer == euid() + 1 && owner == euid()),
        "{refused:?}"
    );
    assert_eq!(peer_uid(&server_side).expect("SO_PEERCRED"), euid());
    assert!(admit(&server_side, euid(), &peer_uid).is_ok());
}

// ---- The editor thread with injected journal backends ------------------------

/// Applies to a real writer, slowly, counting what it applied.
struct SlowBackend {
    writer: JournalWriter,
    delay: Duration,
    applied: Arc<AtomicU64>,
}

impl JournalBackend for SlowBackend {
    fn apply(&mut self, op: JournalOp) -> Result<(), JournalError> {
        thread::sleep(self.delay);
        let result = self.writer.apply(op);
        self.applied.fetch_add(1, Ordering::SeqCst);
        result
    }

    fn sync(&mut self) -> Vec<SyncFailure> {
        self.writer.sync()
    }
}

struct Harness {
    server: Server,
    outbox: Arc<Outbox>,
    client: ClientId,
    /// What the client was sent, as a writer would have sent it.
    sent: Vec<ServerMsg>,
}

impl Harness {
    fn new(config: Config, backend: Box<dyn JournalBackend>) -> Harness {
        let (inputs, receiver) = mpsc::channel();
        let mut server =
            Server::new(config, Session::Server, backend, inputs, receiver).expect("server");
        server.handle(Event::Recovered(Vec::new()));
        let outbox = Arc::new(Outbox::default());
        let client = ClientId(1);
        server.input(Input::Attach {
            client,
            kind: ClientKind::Tui,
            cols: 80,
            rows: 24,
            outbox: outbox.clone(),
        });
        Harness {
            server,
            outbox,
            client,
            sent: Vec::new(),
        }
    }

    fn event(&mut self, event: Event) {
        self.server.input(Input::Client(self.client, event));
        self.collect();
    }

    /// Takes the client's messages, resyncing after dropped frames as the
    /// writer thread does.
    fn collect(&mut self) {
        loop {
            let (messages, resync) = self.outbox.take_queued();
            self.sent.extend(messages);
            if !resync {
                return;
            }
            self.server.input(Input::Resync(self.client));
        }
    }

    fn step(&mut self) {
        assert_eq!(self.server.step(Some(STEP)).expect("step"), Step::Continue);
        self.collect();
    }

    /// The rows the frames sent so far drew, as text.
    fn screen(&self) -> Vec<String> {
        let mut rows: Vec<String> = Vec::new();
        for message in &self.sent {
            let ServerMsg::Frame {
                full,
                rows: count,
                rows_changed,
                ..
            } = message
            else {
                continue;
            };
            if *full {
                rows = vec![String::new(); usize::from(*count)];
            }
            for update in rows_changed {
                rows[usize::from(update.row)] = update
                    .cells
                    .iter()
                    .filter(|cell| cell.width > 0)
                    .map(|cell| cell.text.as_str())
                    .collect();
            }
        }
        rows
    }

    fn shows(&self, text: &str) -> bool {
        self.screen().iter().any(|row| row.contains(text))
    }

    fn keys(&mut self, kbd: &str) {
        for key in parse_kbd(kbd).expect("kbd") {
            self.event(Event::Key(self.client, key));
        }
    }

    fn open(&mut self, path: &Path) {
        self.event(Event::Request(
            self.client,
            1,
            Request::Open {
                path: WirePath::from(path),
                line: None,
                col: None,
                wait: false,
            },
        ));
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        self.step_until(|harness| {
            harness
                .server
                .editor()
                .find_buffer(&name)
                .is_some_and(|buffer| !buffer.loading)
        });
    }

    fn step_until(&mut self, done: impl Fn(&Harness) -> bool) {
        let deadline = Instant::now() + WAIT;
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "the server never got there; screen:\n{}",
                self.screen().join("\n")
            );
            self.step();
        }
    }
}

#[test]
fn journal_ops_applied_before_save() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("saved.txt");
    fs::write(&path, "body\n").expect("fixture");
    let applied = Arc::new(AtomicU64::new(0));
    let backend = SlowBackend {
        writer: JournalWriter::new(&directory.path().join("journal")).expect("writer"),
        delay: Duration::from_millis(20),
        applied: applied.clone(),
    };
    let mut harness = Harness::new(Config::default(), Box::new(backend));
    harness.open(&path);
    for character in "0123456789".chars() {
        harness.keys(&character.to_string());
    }
    harness.keys("C-x C-s");
    // The save may start only after every journal operation before it.
    let deadline = Instant::now() + WAIT;
    while fs::read_to_string(&path).expect("read") == "body\n" {
        assert!(Instant::now() < deadline, "the save never ran");
        thread::sleep(Duration::from_millis(1));
    }
    let at_save = applied.load(Ordering::SeqCst);
    thread::sleep(Duration::from_millis(200));
    assert_eq!(
        applied.load(Ordering::SeqCst),
        at_save,
        "no journal operation emitted before the save was still pending"
    );
    assert!(at_save >= 12, "open, epoch and ten inserts: {at_save}");
    assert_eq!(fs::read_to_string(&path).expect("read"), "0123456789body\n");
}

#[test]
fn fsync_failure_restarts_journal() {
    let directory = tempfile::tempdir().expect("tempdir");
    let journal = directory.path().join("journal");
    let fail = Arc::new(AtomicBool::new(false));
    let failing = fail.clone();
    let writer = JournalWriter::with_sync(
        &journal,
        Box::new(move |file: &fs::File| {
            if failing.swap(false, Ordering::SeqCst) {
                return Err(std::io::Error::from_raw_os_error(5));
            }
            file.sync_data()
        }),
    )
    .expect("writer");
    let config = Config {
        journal_fsync_ms: 10,
        ..Config::default()
    };
    let mut harness = Harness::new(config.clone(), Box::new(writer));
    harness.keys("a b c");
    // Let the first batch sync cleanly, then fail the next one.
    thread::sleep(Duration::from_millis(50));
    fail.store(true, Ordering::SeqCst);
    for character in "0123456789".chars() {
        harness.keys(&character.to_string());
    }
    let journal_file = || {
        fs::read_dir(&journal)
            .expect("journal dir")
            .next()
            .expect("one journal")
            .expect("entry")
            .path()
    };
    // The failure arrives as an input; the editor restarts the journal.
    let deadline = Instant::now() + WAIT;
    while fail.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "no sync ran");
        harness.step();
    }
    harness.step_until(|harness| harness.shows("Journal write failed for *scratch*"));
    // A fresh journal holds the magic, a header and one snapshot, far less
    // than the header, epoch and thirteen insert records before it.
    let deadline = Instant::now() + WAIT;
    while fs::metadata(journal_file()).expect("journal").len() >= FRESH_JOURNAL_MAX {
        assert!(Instant::now() < deadline, "the journal never restarted");
        harness.step();
    }
    harness.keys("z");
    // Let the journal thread apply and sync the last edit.
    thread::sleep(Duration::from_millis(100));
    let recovered = replay_dir(&journal, &config).expect("replay");
    let text = match &recovered[..] {
        [RecoveredBuffer::Rebuilt { text, .. }] => {
            let mut bytes = Vec::new();
            text.read(0..text.len(), &mut bytes).expect("read");
            String::from_utf8(bytes).expect("utf-8")
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(text, "abc0123456789z");
}

#[test]
fn prefetch_deduped() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("big.log");
    let line = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde\n";
    fs::write(&path, line.repeat(4096)).expect("fixture");
    let config = Config {
        in_memory_max: 1024,
        ..Config::default()
    };
    let journal = tempfile::tempdir().expect("tempdir");
    let writer = JournalWriter::new(&journal.path().join("journal")).expect("writer");
    let mut harness = Harness::new(config, Box::new(writer));
    harness.open(&path);
    for size in 0..20 {
        harness.event(Event::Resize(harness.client, 80, 20 + size % 2));
    }
    assert_eq!(
        harness.server.prefetch_reads(),
        1,
        "one read for the first block, however often redisplay asks"
    );
    harness.step_until(|harness| harness.shows("0123456789abcdef"));
    assert_eq!(harness.server.prefetch_reads(), 1);
}

// ---- Fix round 1 ---------------------------------------------------------------

/// A panicking thread must not leave the daemon alive but wedged: with
/// `panic = "abort"` the process ends, the panic hook having run (it is
/// what restores a terminal), and the journal brings the edits back.
#[test]
fn worker_panic_ends_the_server_and_the_next_one_replays() {
    let env = TestEnv::new();
    let path = env.file("panic.txt", "on disk\n");
    let mut server = env.start_server_with(&[(WORKER_PANIC_ENV, "1")]);
    let mut client = env.connect(ClientKind::Tui);
    client.open(&path);
    client.type_text("unsaved ");
    client.wait_text("unsaved on disk");
    client.keys("C-x C-s");
    let status = server.wait_exit(WAIT).expect("the panic ends the server");
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(rustix::process::Signal::ABORT.as_raw()),
        "aborted; stderr:\n{}",
        env.server_stderr()
    );
    assert!(
        env.server_stderr()
            .contains("an I/O thread panics on a save"),
        "the panic hook ran before the abort:\n{}",
        env.server_stderr()
    );
    drop(client);
    assert_eq!(fs::read_to_string(&path).expect("read"), "on disk\n");
    let _restarted = env.start_server();
    let mut client = env.connect(ClientKind::Tui);
    client.open(&path);
    client.wait_text("unsaved on disk");
    assert!(client.modeline().contains("**"), "{}", client.modeline());
}

fn resident_bytes(pid: u32) -> u64 {
    const KIB: u64 = 1024;
    let status = fs::read_to_string(format!("/proc/{pid}/status")).expect("proc status");
    let line = status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .expect("VmRSS");
    let kib: u64 = line
        .split_whitespace()
        .nth(1)
        .expect("value")
        .parse()
        .expect("number");
    kib * KIB
}

/// A subscriber that never reads is cut off once its backlog passes
/// `OUTBOX_BYTE_LIMIT`, even while its writer is blocked in a write.
#[test]
fn stalled_subscriber_is_cut_off() {
    use std::io::{Read, Write};

    const BUFFERS: usize = 100;
    const REQUESTS: usize = 2000;
    const DIRECTORY_LEVELS: usize = 18;
    const LEVEL_NAME: usize = 200;
    const MEMORY_BOUND: u64 = 100 * 1024 * 1024;

    let env = TestEnv::new();
    let mut deep = env.work.clone();
    for level in 0..DIRECTORY_LEVELS {
        let letter = char::from(b'a' + level as u8);
        deep.push(letter.to_string().repeat(LEVEL_NAME));
    }
    fs::create_dir_all(&deep).expect("deep directory");
    let server = env.start_server();
    let mut good = env.connect(ClientKind::Tui);
    for index in 0..BUFFERS {
        good.open(&deep.join(format!("f{index}")));
    }

    let mut stalled = UnixStream::connect(env.socket_path()).expect("connect");
    let mut lines = Vec::new();
    emx_proto::write_msg(
        &mut lines,
        &emx_proto::ClientMsg::Hello {
            kind: ClientKind::Cli,
            cwd: WirePath::from(env.work.as_path()),
            cols: 80,
            rows: 24,
            wait_for: None,
        },
    )
    .expect("encode");
    emx_proto::write_msg(
        &mut lines,
        &emx_proto::ClientMsg::Request {
            id: 0,
            req: Request::SubscribeEvents,
        },
    )
    .expect("encode");
    for id in 1..=REQUESTS as u64 {
        emx_proto::write_msg(
            &mut lines,
            &emx_proto::ClientMsg::Request {
                id,
                req: Request::ListBuffers,
            },
        )
        .expect("encode");
    }
    // The server stops reading once it cuts the client off, so the write
    // may fail part way.
    drop(stalled.write_all(&lines));
    // Not reading until the server has given up on the client.
    let deadline = Instant::now() + WAIT;
    while !env
        .server_stderr()
        .contains("stopped reading; cutting it off")
    {
        assert!(
            Instant::now() < deadline,
            "never cut off; stderr:\n{}",
            env.server_stderr()
        );
        thread::sleep(Duration::from_millis(10));
    }
    // The socket is shut down at once, even though the writer was blocked
    // in a write the client never read: the client's writes now fail.
    let mut ping = Vec::new();
    emx_proto::write_msg(
        &mut ping,
        &emx_proto::ClientMsg::Request {
            id: 0,
            req: Request::DiffExternal {
                path: WirePath::Text("/x".to_string()),
            },
        },
    )
    .expect("encode");
    let deadline = Instant::now() + WAIT;
    while stalled.write_all(&ping).is_ok() {
        assert!(Instant::now() < deadline, "the socket stays open");
        thread::sleep(Duration::from_millis(1));
    }
    good.keys("C-g");
    good.type_text("still here");
    good.wait_text("still here");
    stalled.set_read_timeout(Some(WAIT)).expect("read timeout");
    let mut received = Vec::new();
    stalled.read_to_end(&mut received).unwrap_or_else(|error| {
        panic!(
            "the server closes the connection: {error}; stderr:\n{}",
            env.server_stderr()
        )
    });
    assert!(
        resident_bytes(server.pid()) < MEMORY_BOUND,
        "the backlog stays bounded"
    );
    assert!(
        received.len() < REQUESTS * BUFFERS * LEVEL_NAME,
        "the backlog was dropped, not sent"
    );
    good.type_text("!");
    good.wait_text("still here!");
}

/// DESIGN section 2: "every journal_fsync_ms or on idle".
#[test]
fn quiet_after_an_edit_syncs_long_before_the_batch_interval() {
    struct CountingBackend {
        syncs: Arc<AtomicU64>,
    }

    impl JournalBackend for CountingBackend {
        fn apply(&mut self, _op: JournalOp) -> Result<(), JournalError> {
            Ok(())
        }

        fn sync(&mut self) -> Vec<SyncFailure> {
            self.syncs.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        }
    }

    const BATCH: Duration = Duration::from_secs(60);
    const IDLE_SLACK: u64 = 25;
    let syncs = Arc::new(AtomicU64::new(0));
    let mut journal = JournalThread::spawn(
        Box::new(CountingBackend {
            syncs: syncs.clone(),
        }),
        BATCH,
        Box::new(|_| {}),
    );
    journal.submit(JournalOp::Close {
        buffer: emx_core::journal::JournalId(1),
    });
    let started = Instant::now();
    while syncs.load(Ordering::SeqCst) == 0 {
        assert!(
            started.elapsed() < Duration::from_millis(JOURNAL_IDLE_SYNC_MS * IDLE_SLACK),
            "no idle sync"
        );
        thread::sleep(Duration::from_millis(1));
    }
}
