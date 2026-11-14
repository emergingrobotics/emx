//! Live reload end to end (DESIGN sections 1 and 3): the real `emx
//! server` watching real files in a local temporary directory, changed
//! underneath it the way agents, editors, `git` and log writers change
//! them. Waits are on conditions with generous deadlines; the only fixed
//! waits are the quiet periods that check nothing more happens.

mod common;

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use common::{ROWS, RawClient, TestEnv, WAIT};
use emx::server::watcher::NETWORK_FS_ENV;
use emx_proto::{ClientKind, EditorEvent, ExternalKind, Request, Response, ServerMsg, WirePath};
use serde_json::{Value, json};

const POLL: Duration = Duration::from_millis(10);
/// Long enough for a change to settle (two looks a settle window apart)
/// and be applied: after it, nothing else may happen.
const QUIET: Duration = Duration::from_millis(1500);
const BURST_WRITES: usize = 20;
/// The burst spans this long, as an agent streaming a file would.
const BURST_SPAN: Duration = Duration::from_millis(100);
/// The in-place writer pauses this long half way.
const WRITER_PAUSE: Duration = Duration::from_millis(50);
const MULTI_FILES: [&str; 5] = ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"];

/// A session with a terminal attached and a CLI client subscribed to
/// editor events.
struct Session {
    env: TestEnv,
    _server: common::ServerProcess,
    terminal: RawClient,
    events: Events,
}

impl Session {
    fn start() -> Session {
        Session::start_with(TestEnv::new(), &[])
    }

    fn start_with(env: TestEnv, variables: &[(&str, &str)]) -> Session {
        let server = env.start_server_with(variables);
        let mut terminal = env.connect(ClientKind::Tui);
        terminal.wait_text("*scratch*");
        let events = Events::subscribe(&env);
        Session {
            env,
            _server: server,
            terminal,
            events,
        }
    }

    /// Writes `content` to `name` and shows it in the terminal.
    fn open(&mut self, name: &str, content: &str) -> PathBuf {
        let path = self.env.file(name, content);
        self.terminal.open(&path);
        self.wait_modeline(name);
        path
    }

    /// Waits until the modeline names the shown buffer and it is read.
    fn wait_modeline(&mut self, name: &str) {
        self.terminal.wait_screen(|screen| {
            let modeline = &screen[usize::from(ROWS) - 2];
            modeline.contains(name) && !modeline.contains("Loading")
        });
    }

    fn wait_modeline_with(&mut self, text: &str) -> String {
        self.terminal
            .wait_screen(|screen| screen[usize::from(ROWS) - 2].contains(text))
            [usize::from(ROWS) - 2]
            .clone()
    }
}

/// `ExternalChange` events of a subscribed CLI client, kept as they come.
struct Events {
    client: RawClient,
    seen: Vec<(PathBuf, ExternalKind)>,
}

impl Events {
    fn subscribe(env: &TestEnv) -> Events {
        let mut client = env.connect(ClientKind::Cli);
        assert_eq!(client.call(Request::SubscribeEvents), Ok(Response::Ok));
        Events {
            client,
            seen: Vec::new(),
        }
    }

    fn take(&mut self) {
        for message in self.client.messages.drain(..) {
            if let ServerMsg::Event {
                event: EditorEvent::ExternalChange { path, kind },
                ..
            } = message
            {
                self.seen.push((path.into_path_buf(), kind));
            }
        }
    }

    /// Every external change reported for `path` so far.
    fn of(&mut self, path: &Path) -> Vec<ExternalKind> {
        self.take();
        self.seen
            .iter()
            .filter(|(seen, _)| seen == path)
            .map(|(_, kind)| kind.clone())
            .collect()
    }

    fn wait(&mut self, path: &Path, kind: &ExternalKind) {
        let deadline = Instant::now() + WAIT;
        while !self.of(path).contains(kind) {
            assert!(
                Instant::now() < deadline,
                "no {kind:?} for {}; saw {:?}",
                path.display(),
                self.seen
            );
            self.client.pump(POLL);
        }
    }

    /// Takes in what arrives for `limit`.
    fn quiet(&mut self, limit: Duration) {
        self.client.pump(limit);
        self.take();
    }
}

/// The text rows of the window, without trailing blanks and blank rows.
fn text_rows(screen: &[String]) -> Vec<String> {
    let mut rows: Vec<String> = screen[..usize::from(ROWS) - 2]
        .iter()
        .map(|row| row.trim_end().to_string())
        .collect();
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    rows
}

fn lines(text: &str) -> Vec<String> {
    text.lines().map(str::to_string).collect()
}

fn echo(screen: &[String]) -> &str {
    screen[usize::from(ROWS) - 1].trim_end()
}

/// Write-temp-and-rename, as editors and `git` replace files.
fn atomic_replace(path: &Path, content: &str) {
    let temporary = path.with_extension("emx-test-tmp");
    fs::write(&temporary, content).expect("write the temporary file");
    fs::rename(&temporary, path).expect("rename over the file");
}

#[test]
fn agent_burst_writes_coalesce() {
    let mut session = Session::start();
    let path = session.open("agent.txt", "original\n");
    session.terminal.wait_text("original");
    let started = Instant::now();
    let pause = BURST_SPAN / u32::try_from(BURST_WRITES).expect("small");
    for write in 1..=BURST_WRITES {
        fs::write(&path, format!("burst write {write} of {BURST_WRITES}\n")).expect("write");
        thread::sleep(pause);
    }
    let span = started.elapsed();
    session
        .terminal
        .wait_text(&format!("burst write {BURST_WRITES} of {BURST_WRITES}"));
    session.events.wait(&path, &ExternalKind::Reloaded);
    session.events.quiet(QUIET);
    assert_eq!(
        session.events.of(&path),
        vec![ExternalKind::Reloaded],
        "one reload for the whole burst (it took {span:?})"
    );
    assert_eq!(
        text_rows(&session.terminal.screen()),
        lines(&format!("burst write {BURST_WRITES} of {BURST_WRITES}\n"))
    );
}

#[test]
fn in_place_writer_never_shows_partial() {
    let mut session = Session::start();
    let old = "old line one\nold line two\nold line three\n";
    let new = "new line one\nnew line two\nnew line three\nnew line four\nnew line five\n";
    let path = session.open("inplace.txt", old);
    session.terminal.wait_text("old line three");
    let (half, rest) = new.split_at(new.len() / 2);
    let (half, rest) = (half.to_string(), rest.to_string());
    let writer_path = path.clone();
    let writer = thread::spawn(move || {
        let mut file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&writer_path)
            .expect("open for an in-place rewrite");
        file.write_all(half.as_bytes())
            .expect("write the first half");
        file.flush().expect("flush");
        thread::sleep(WRITER_PAUSE);
        file.write_all(rest.as_bytes()).expect("write the rest");
    });
    let (old_rows, new_rows) = (lines(old), lines(new));
    let mut screens = 0;
    session.terminal.wait_screen_each(
        |screen| {
            screens += 1;
            let rows = text_rows(screen);
            assert!(
                rows == old_rows || rows == new_rows,
                "a partial file was shown:\n{}",
                screen.join("\n")
            );
        },
        |screen| text_rows(screen) == new_rows,
    );
    writer.join().expect("the writer finished");
    // Whatever else is drawn afterwards is checked as well.
    let deadline = Instant::now() + QUIET;
    while Instant::now() < deadline {
        session.terminal.pump(POLL);
        assert_eq!(text_rows(&session.terminal.screen()), new_rows);
    }
    assert!(screens > 0);
    session.events.quiet(Duration::ZERO);
    assert!(
        session
            .events
            .of(&path)
            .iter()
            .all(|kind| matches!(kind, ExternalKind::Reloaded | ExternalKind::Truncated)),
        "{:?}",
        session.events.seen
    );
}

#[test]
fn atomic_rename_save_reloads() {
    let mut session = Session::start();
    let path = session.open("renamed.txt", "before rename\n");
    session.terminal.wait_text("before rename");
    atomic_replace(&path, "after rename\n");
    session.terminal.wait_text("after rename");
    session.events.wait(&path, &ExternalKind::Reloaded);
    let modeline = session.terminal.modeline();
    assert!(!modeline.contains("**"), "unmodified: {modeline}");
    assert!(!modeline.contains("[MOVED"), "{modeline}");
    assert!(!modeline.contains("[DELETED]"), "{modeline}");
}

#[test]
fn append_follows_tail() {
    let mut session = Session::start();
    let path = session.open("tail.log", "line 1\nline 2\n");
    session.terminal.wait_text("line 2");
    session.terminal.keys("M->");
    session.terminal.wait_cursor((0, 2));
    let mut log = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open the log");
    log.write_all(b"line 3\n").expect("append");
    drop(log);
    session.terminal.wait_text("line 3");
    session.terminal.wait_cursor((0, 3));
    session.events.wait(&path, &ExternalKind::Appended);
    session.events.quiet(QUIET);
    assert_eq!(session.events.of(&path), vec![ExternalKind::Appended]);
    assert_eq!(
        text_rows(&session.terminal.screen()),
        lines("line 1\nline 2\nline 3\n")
    );
}

#[test]
fn git_checkout_multi_file_single_summary() {
    let mut session = Session::start();
    let paths: Vec<PathBuf> = MULTI_FILES
        .iter()
        .map(|name| session.open(name, &format!("{name} on main\n")))
        .collect();
    // e.rs, opened last, is shown; the others are not.
    session.terminal.wait_text("e.rs on main");
    for (name, path) in MULTI_FILES.iter().zip(&paths) {
        atomic_replace(path, &format!("{name} on the branch\n"));
    }
    let summary = "Reloaded 5 files changed on disk: a.rs, b.rs, c.rs, d.rs, e.rs";
    let mut summaries = Vec::new();
    session.terminal.wait_screen_each(
        |screen| {
            let echo = echo(screen);
            if echo.starts_with("Reloaded") && summaries.last() != Some(&echo.to_string()) {
                summaries.push(echo.to_string());
            }
        },
        |screen| echo(screen) == summary,
    );
    session.terminal.wait_text("e.rs on the branch");
    for path in &paths {
        session.events.wait(path, &ExternalKind::Reloaded);
    }
    session.events.quiet(QUIET);
    for path in &paths {
        assert_eq!(
            session.events.of(path),
            vec![ExternalKind::Reloaded],
            "one event for {}",
            path.display()
        );
    }
    session.terminal.pump(Duration::ZERO);
    assert_eq!(summaries, vec![summary.to_string()], "one summary");
    // The shown buffer was seen changing; b.rs was not, and says so once
    // it is shown, until the user acts in it.
    assert!(!session.terminal.modeline().contains("[ext]"));
    session.terminal.open(&paths[1]);
    session.terminal.wait_text("b.rs on the branch");
    let modeline = session.wait_modeline_with("b.rs");
    assert!(modeline.contains("[ext]"), "{modeline}");
    session.terminal.keys("C-g");
    session
        .terminal
        .wait_screen(|screen| !screen[usize::from(ROWS) - 2].contains("[ext]"));
}

#[test]
fn own_save_no_reload_loop() {
    let mut session = Session::start();
    let path = session.open("own.txt", "content\n");
    session.terminal.wait_text("content");
    for round in 1..=2 {
        session.terminal.type_text("x");
        session.terminal.keys("C-x C-s");
        session
            .terminal
            .wait_screen(|screen| echo(screen).starts_with("Wrote"));
        session.terminal.pump(QUIET);
        let modeline = session.terminal.modeline();
        assert!(!modeline.contains("**"), "round {round}: {modeline}");
        assert!(
            !modeline.contains('['),
            "round {round}: no badge: {modeline}"
        );
    }
    session.events.quiet(Duration::ZERO);
    assert_eq!(session.events.of(&path), Vec::<ExternalKind>::new());
    assert_eq!(fs::read_to_string(&path).expect("saved"), "xxcontent\n");
    assert_eq!(text_rows(&session.terminal.screen()), lines("xxcontent\n"));
}

#[test]
fn conflict_badge_on_modified() {
    let mut session = Session::start();
    let path = session.open("mine.txt", "shared\n");
    session.terminal.wait_text("shared");
    session.terminal.type_text("my edit ");
    session.terminal.wait_text("my edit shared");
    atomic_replace(&path, "agent version\n");
    let modeline = session.wait_modeline_with("[CONFLICT]");
    assert!(modeline.contains("**"), "{modeline}");
    session
        .terminal
        .wait_screen(|screen| echo(screen).contains("mine.txt changed on disk"));
    session.events.wait(&path, &ExternalKind::Conflict);
    assert_eq!(
        text_rows(&session.terminal.screen()),
        lines("my edit shared\n"),
        "a modified buffer is never reloaded"
    );
    assert_eq!(fs::read_to_string(&path).expect("file"), "agent version\n");
}

#[test]
fn delete_then_recreate() {
    let mut session = Session::start();
    let path = session.open("phoenix.txt", "first life\n");
    session.terminal.wait_text("first life");
    fs::remove_file(&path).expect("delete");
    session.wait_modeline_with("[DELETED]");
    session.events.wait(&path, &ExternalKind::Deleted);
    assert_eq!(
        text_rows(&session.terminal.screen()),
        lines("first life\n"),
        "the buffer is kept"
    );
    fs::write(&path, "second life\n").expect("recreate");
    session.terminal.wait_text("second life");
    session
        .terminal
        .wait_screen(|screen| !screen[usize::from(ROWS) - 2].contains("[DELETED]"));
    session.events.wait(&path, &ExternalKind::Reloaded);
}

/// `emx events` as a process, its lines read on a thread.
struct EventsTool {
    child: Child,
    lines: Receiver<String>,
}

impl EventsTool {
    fn spawn(env: &TestEnv) -> EventsTool {
        let mut child = env
            .emx()
            .arg("events")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn emx events");
        let stdout = child.stdout.take().expect("piped stdout");
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        EventsTool { child, lines }
    }

    /// The tool subscribes on its own time: terminals attach and leave
    /// until one is reported.
    fn wait_subscribed(&self, env: &TestEnv) {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            let probe = env.connect(ClientKind::Tui);
            let reported = self.lines.recv_timeout(Duration::from_millis(500)).is_ok();
            probe.disconnect();
            if reported {
                return;
            }
        }
        panic!("emx events never subscribed");
    }

    fn wait(&self, want: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + WAIT;
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            let Ok(line) = self.lines.recv_timeout(POLL) else {
                continue;
            };
            let value: Value = serde_json::from_str(&line).expect("one json line per event");
            assert_eq!(value["v"], 1, "{line}");
            if want(&value["event"]) {
                return value;
            }
            seen.push(line);
        }
        panic!("no matching event; saw {seen:?}");
    }
}

impl Drop for EventsTool {
    fn drop(&mut self) {
        drop(self.child.kill());
        drop(self.child.wait());
    }
}

#[test]
fn events_stream_reports_external_change() {
    let mut session = Session::start();
    let tool = EventsTool::spawn(&session.env);
    tool.wait_subscribed(&session.env);
    let path = session.open("streamed.txt", "one\n");
    session.terminal.wait_text("one");
    atomic_replace(&path, "two\n");
    let expected = json!(path.to_str().expect("utf-8 path"));
    tool.wait(|event| {
        event["ExternalChange"]["path"] == expected && event["ExternalChange"]["kind"] == "Reloaded"
    });
    session.terminal.wait_text("two");
}

/// Runs `emx ARGS` to completion within the deadline.
fn run_emx(env: &TestEnv, args: &[&str]) -> std::process::Output {
    let mut child = env
        .emx()
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn emx");
    let deadline = Instant::now() + WAIT;
    while child.try_wait().expect("try_wait").is_none() {
        if Instant::now() >= deadline {
            drop(child.kill());
            panic!("emx {args:?} never exited");
        }
        thread::sleep(POLL);
    }
    child.wait_with_output().expect("collect the output")
}

#[test]
fn diff_external_cli_prints_diff() {
    let mut session = Session::start();
    let path = session.open("diffed.txt", "keep\nold middle\nkeep too\n");
    session.terminal.wait_text("old middle");
    atomic_replace(&path, "keep\nnew middle\nkeep too\n");
    session.terminal.wait_text("new middle");
    session.events.wait(&path, &ExternalKind::Reloaded);
    let output = run_emx(&session.env, &["diff-external", "diffed.txt"]);
    let stdout = String::from_utf8(output.stdout).expect("utf-8 diff");
    assert!(
        output.status.success(),
        "{}; stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("@@"), "{stdout}");
    assert!(stdout.contains("\n-old middle\n"), "{stdout}");
    assert!(stdout.contains("\n+new middle\n"), "{stdout}");
    assert!(stdout.contains(" keep\n"), "context lines: {stdout}");
}

#[test]
fn polling_used_when_fs_is_network() {
    let env = TestEnv::new();
    let work = env.work.to_str().expect("utf-8 path").to_string();
    let mut session = Session::start_with(env, &[(NETWORK_FS_ENV, &work)]);
    let path = session.open("remote.txt", "on the server\n");
    session.terminal.wait_text("on the server");
    let modeline = session.wait_modeline_with("[poll]");
    assert!(modeline.contains("remote.txt"), "{modeline}");
    fs::write(&path, "changed remotely\n").expect("rewrite");
    session.terminal.wait_text("changed remotely");
    session.events.wait(&path, &ExternalKind::Reloaded);
    assert!(session.terminal.modeline().contains("[poll]"));
}

#[test]
fn watch_error_degrades_to_polling() {
    let mut session = Session::start();
    let directory = session.env.work.join("not-yet");
    let path = directory.join("later.txt");
    let id = session.terminal.request(Request::Open {
        path: WirePath::from(path.as_path()),
        line: None,
        col: None,
        wait: false,
    });
    assert_eq!(session.terminal.response(id), Ok(Response::Ok));
    session.wait_modeline_with("[poll]");
    session
        .terminal
        .wait_screen(|screen| echo(screen).starts_with("Polling files in"));
    fs::create_dir(&directory).expect("create the directory");
    fs::write(&path, "appeared\n").expect("create the file");
    session.terminal.wait_text("appeared");
    session.events.wait(&path, &ExternalKind::Reloaded);
}
