//! The command-line tools (`emx open`, `emx list-buffers`, ...) run as
//! real processes against a real `emx server`, with a terminal client
//! speaking the wire protocol standing in for the user's terminal.

mod common;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::{TestEnv, WAIT};
use emx_proto::ClientKind;
use serde_json::{Value, json};

const POLL: Duration = Duration::from_millis(10);
/// How long a `--wait` open is watched to make sure it is still held.
const HELD_FOR: Duration = Duration::from_millis(500);
const NO_SERVER: &str = "emx: no server running\n";
const NO_CLIENT: &str = "emx: no emx client is attached; run 'emx FILE' in a terminal first\n";

/// A tool process; killed and reaped when dropped.
struct Tool {
    child: Child,
}

impl Tool {
    fn spawn(env: &TestEnv, args: &[&OsStr]) -> Tool {
        let child = env
            .emx()
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn emx");
        Tool { child }
    }

    fn running(&mut self) -> bool {
        self.child.try_wait().expect("try_wait").is_none()
    }

    /// Waits for the process to end and collects what it wrote.
    fn finish(mut self) -> Output {
        let deadline = Instant::now() + WAIT;
        while self.running() {
            assert!(Instant::now() < deadline, "the tool never exited");
            thread::sleep(POLL);
        }
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        if let Some(mut pipe) = self.child.stdout.take() {
            pipe.read_to_end(&mut stdout).expect("read stdout");
        }
        if let Some(mut pipe) = self.child.stderr.take() {
            pipe.read_to_end(&mut stderr).expect("read stderr");
        }
        Output {
            status: self.child.wait().expect("reap"),
            stdout,
            stderr,
        }
    }
}

impl Drop for Tool {
    fn drop(&mut self) {
        if self.running() {
            drop(self.child.kill());
            drop(self.child.wait());
        }
    }
}

fn os(text: &str) -> &OsStr {
    OsStr::new(text)
}

/// Runs `emx ARGS` to completion.
fn emx(env: &TestEnv, args: &[&str]) -> Output {
    let args: Vec<&OsStr> = args.iter().map(|arg| os(arg)).collect();
    Tool::spawn(env, &args).finish()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("utf-8 stdout")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "{}; stderr: {}",
        output.status,
        stderr(output)
    );
}

/// Exit 1 with exactly `message` on stderr and nothing on stdout.
fn assert_fails_with(output: &Output, message: &str) {
    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(output));
    assert_eq!(stderr(output), message);
    assert!(output.stdout.is_empty(), "stdout: {}", stdout(output));
}

fn buffers_as_json(env: &TestEnv) -> Vec<Value> {
    let output = emx(env, &["list-buffers", "--json"]);
    assert_success(&output);
    let text = stdout(&output);
    assert!(text.ends_with('\n'), "{text:?}");
    let value: Value = serde_json::from_str(&text).expect("list-buffers prints json");
    value.as_array().expect("a json array").clone()
}

fn buffer_named<'a>(buffers: &'a [Value], name: &str) -> &'a Value {
    buffers
        .iter()
        .find(|buffer| buffer["name"] == name)
        .unwrap_or_else(|| panic!("no buffer {name} in {buffers:?}"))
}

/// Lines `emx events` prints, read on a thread so a test can wait for
/// them with a deadline.
fn event_lines(pipe: ChildStdout) -> Receiver<String> {
    let (sender, lines) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(pipe).lines() {
            let Ok(line) = line else { return };
            if sender.send(line).is_err() {
                return;
            }
        }
    });
    lines
}

/// Waits for an event line whose event satisfies `want`; returns it.
fn wait_event(lines: &Receiver<String>, want: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + WAIT;
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        let Ok(line) = lines.recv_timeout(POLL) else {
            continue;
        };
        let value: Value = serde_json::from_str(&line).expect("each event is one json line");
        assert_eq!(
            value["v"],
            emx_proto::PROTOCOL_VERSION,
            "events carry the protocol version: {line}"
        );
        assert_eq!(value["type"], "event", "{line}");
        if want(&value["event"]) {
            return value;
        }
        seen.push(line);
    }
    panic!("no matching event; saw {seen:?}");
}

/// The subscription is in place once an event arrives: terminals attach
/// and leave until one is reported, as the tool subscribes on its own time.
fn wait_subscribed(env: &TestEnv, lines: &Receiver<String>) {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        let probe = env.connect(ClientKind::Tui);
        if let Ok(line) = lines.recv_timeout(HELD_FOR) {
            let value: Value = serde_json::from_str(&line).expect("one json line");
            assert_eq!(value["type"], "event", "{line}");
            probe.disconnect();
            return;
        }
        probe.disconnect();
    }
    panic!("emx events never subscribed");
}

#[test]
fn open_wait_released_by_c_x_hash() {
    let env = TestEnv::new();
    env.file("COMMIT_EDITMSG", "first line\n");
    let _server = env.start_server();
    let mut terminal = env.connect(ClientKind::Tui);
    terminal.wait_text("*scratch*");
    let mut waiting = Tool::spawn(&env, &[os("open"), os("--wait"), os("COMMIT_EDITMSG")]);
    terminal.wait_text("first line");
    assert!(terminal.modeline().contains("COMMIT_EDITMSG"));
    thread::sleep(HELD_FOR);
    assert!(waiting.running(), "held until the user is done");
    terminal.keys("C-x #");
    let output = waiting.finish();
    assert_success(&output);
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
    terminal.wait_screen(|screen| !screen.iter().any(|row| row.contains("first line")));
}

#[test]
fn open_places_point_and_needs_a_terminal() {
    let env = TestEnv::new();
    env.file("lines.txt", "one\ntwo\nthree\n");
    let _server = env.start_server();
    assert_fails_with(&emx(&env, &["open", "lines.txt"]), NO_CLIENT);
    let mut terminal = env.connect(ClientKind::Tui);
    terminal.wait_text("*scratch*");
    let output = emx(&env, &["open", "lines.txt:3:2"]);
    assert_success(&output);
    terminal.wait_text("three");
    terminal.wait_cursor((1, 2));
}

#[test]
fn list_buffers_json() {
    let env = TestEnv::new();
    let clean = env.file("clean.txt", "abc\n");
    let edited = env.file("edited.txt", "xyz\n");
    let _server = env.start_server();
    let mut terminal = env.connect(ClientKind::Tui);
    terminal.open(&clean);
    terminal.open(&edited);
    terminal.type_text("12");
    terminal.wait_text("12xyz");
    let buffers = buffers_as_json(&env);
    assert_eq!(
        buffer_named(&buffers, "edited.txt"),
        &json!({
            "name": "edited.txt",
            "path": edited.to_str().expect("utf-8 path"),
            "modified": true,
            "size": 6,
        })
    );
    assert_eq!(
        buffer_named(&buffers, "clean.txt"),
        &json!({
            "name": "clean.txt",
            "path": clean.to_str().expect("utf-8 path"),
            "modified": false,
            "size": 4,
        })
    );
    assert_eq!(buffer_named(&buffers, "*scratch*")["path"], Value::Null);
    assert_eq!(buffers[0]["name"], "edited.txt", "most recent first");

    let output = emx(&env, &["list-buffers"]);
    assert_success(&output);
    let table = stdout(&output);
    let row = |name: &str| {
        table
            .lines()
            .find(|line| line.contains(name))
            .unwrap_or_else(|| panic!("no row for {name} in\n{table}"))
            .to_string()
    };
    assert!(row("edited.txt").starts_with('*'), "{table}");
    assert!(row("clean.txt").starts_with(' '), "{table}");
    assert!(
        row("edited.txt").contains(edited.to_str().expect("utf-8")),
        "{table}"
    );
}

#[test]
fn goto_moves_point() {
    let env = TestEnv::new();
    let path = env.file("target.txt", "alpha\nbeta\ngamma\n");
    env.file("other.txt", "first\nsecond\n");
    let _server = env.start_server();
    let mut terminal = env.connect(ClientKind::Tui);
    terminal.open(&path);
    terminal.wait_text("gamma");
    assert_success(&emx(&env, &["goto", "3:4"]));
    terminal.wait_cursor((3, 2));
    assert_success(&emx(&env, &["goto", "2"]));
    terminal.wait_cursor((0, 1));
    assert_success(&emx(&env, &["goto", "other.txt:2:3"]));
    terminal.wait_text("second");
    terminal.wait_cursor((2, 1));
    assert!(terminal.modeline().contains("other.txt"));
    let output = emx(&env, &["goto", "other.txt"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).starts_with("emx: goto needs [FILE:]LINE[:COL]"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn save_all_writes() {
    let env = TestEnv::new();
    let first = env.file("first.txt", "1\n");
    let second = env.file("second.txt", "2\n");
    let _server = env.start_server();
    let mut terminal = env.connect(ClientKind::Tui);
    terminal.open(&first);
    terminal.type_text("a");
    terminal.wait_text("a1");
    terminal.open(&second);
    terminal.type_text("b");
    terminal.wait_text("b2");
    assert_success(&emx(&env, &["save-all"]));
    assert_eq!(fs::read_to_string(&first).expect("first"), "a1\n");
    assert_eq!(fs::read_to_string(&second).expect("second"), "b2\n");
    let buffers = buffers_as_json(&env);
    assert_eq!(buffer_named(&buffers, "first.txt")["modified"], false);
    assert_eq!(buffer_named(&buffers, "second.txt")["modified"], false);
}

#[test]
fn save_all_reports_a_save_that_would_ask() {
    let env = TestEnv::new();
    let path = env.file("agent.txt", "old\n");
    let _server = env.start_server();
    let mut terminal = env.connect(ClientKind::Tui);
    terminal.open(&path);
    terminal.type_text("user ");
    terminal.wait_text("user old");
    // An agent rewrites the file: saving over it needs the user's say.
    thread::sleep(Duration::from_millis(20));
    fs::write(&path, "agent\n").expect("agent write");
    let output = emx(&env, &["save-all"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).starts_with("emx: agent.txt: "),
        "{}",
        stderr(&output)
    );
    assert_eq!(fs::read_to_string(&path).expect("file"), "agent\n");
}

#[test]
fn reload_rereads_an_unmodified_buffer() {
    let env = TestEnv::new();
    let path = env.file("log.txt", "before\n");
    let _server = env.start_server();
    let mut terminal = env.connect(ClientKind::Tui);
    terminal.open(&path);
    terminal.wait_text("before");
    fs::write(&path, "after\n").expect("rewrite");
    assert_success(&emx(&env, &["reload", "log.txt"]));
    terminal.wait_text("after");
    fs::write(&path, "again\n").expect("rewrite");
    assert_success(&emx(&env, &["reload"]));
    terminal.wait_text("again");
    terminal.type_text("x");
    terminal.wait_text("xagain");
    assert_fails_with(&emx(&env, &["reload", "log.txt"]), "emx: buffer modified\n");
}

#[test]
fn diff_external_without_a_change_errors() {
    let env = TestEnv::new();
    let path = env.file("quiet.txt", "q\n");
    let _server = env.start_server();
    let mut terminal = env.connect(ClientKind::Tui);
    terminal.open(&path);
    assert_fails_with(
        &emx(&env, &["diff-external", "quiet.txt"]),
        "emx: no external change recorded\n",
    );
}

#[test]
fn events_stream_reports_open_and_save() {
    let env = TestEnv::new();
    let path = env.file("watched.txt", "w\n");
    let _server = env.start_server();
    let mut terminal = env.connect(ClientKind::Tui);
    terminal.wait_text("*scratch*");
    let mut events = Tool::spawn(&env, &[os("events")]);
    let lines = event_lines(events.child.stdout.take().expect("piped stdout"));
    wait_subscribed(&env, &lines);
    let expected_path = json!(path.to_str().expect("utf-8 path"));
    terminal.open(&path);
    wait_event(&lines, |event| {
        event["BufferOpened"]["path"] == expected_path
            && event["BufferOpened"]["name"] == "watched.txt"
    });
    terminal.type_text("x");
    terminal.keys("C-x C-s");
    wait_event(&lines, |event| {
        event["BufferSaved"]["path"] == expected_path
    });
    assert_eq!(fs::read_to_string(&path).expect("saved"), "xw\n");
    assert!(events.running(), "events stream until interrupted");
}

#[test]
fn cli_open_path_with_newline_and_invalid_utf8() {
    let env = TestEnv::new();
    let name: OsString = OsString::from_vec(b"odd\nname\xff.txt".to_vec());
    let path: PathBuf = env.work.join(&name);
    fs::write(&path, "strange file\n").expect("fixture");
    let _server = env.start_server();
    let mut terminal = env.connect(ClientKind::Tui);
    terminal.wait_text("*scratch*");
    let output = Tool::spawn(&env, &[os("open"), name.as_os_str()]).finish();
    assert_success(&output);
    terminal.wait_text("strange file");
    assert!(
        terminal.modeline().contains("odd^Jname"),
        "{}",
        terminal.modeline()
    );
    let buffers = buffers_as_json(&env);
    let opened = buffers
        .iter()
        .find(|buffer| buffer["path"].get("bytes_b64").is_some())
        .unwrap_or_else(|| panic!("no byte path in {buffers:?}"));
    let bytes = STANDARD
        .decode(opened["path"]["bytes_b64"].as_str().expect("base64 text"))
        .expect("valid base64");
    assert_eq!(bytes, path.as_os_str().as_bytes());
    assert_eq!(Path::new(OsStr::from_bytes(&bytes)), path);

    let table = stdout(&emx(&env, &["list-buffers"]));
    assert!(
        table.lines().any(|line| line.contains("odd^Jname")),
        "control characters are shown, not printed: {table}"
    );
}

#[test]
fn cli_without_server_errors() {
    let env = TestEnv::new();
    env.file("f.txt", "f\n");
    let commands: [&[&str]; 9] = [
        &["open", "f.txt"],
        &["open", "--wait", "f.txt"],
        &["list-buffers"],
        &["list-buffers", "--json"],
        &["reload"],
        &["goto", "1"],
        &["save-all"],
        &["events"],
        &["diff-external", "f.txt"],
    ];
    for args in commands {
        assert_fails_with(&emx(&env, args), NO_SERVER);
    }
    assert!(
        !env.socket_path().exists(),
        "the command-line tools never start a server"
    );
}
