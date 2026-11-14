//! The terminal client and `--no-server` on a real pseudo-terminal, driven
//! as a Kitty-protocol terminal drives them.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use common::pty::{
    BRACKETED_PASTE_OFF, ENTER_ALTERNATE_SCREEN, LEAVE_ALTERNATE_SCREEN, POP_KITTY_FLAGS,
    PUSH_KITTY_FLAGS, Probe, Pty, SessionReaper, alive, find, rfind, server_pid, session_servers,
};
use common::{TestEnv, WAIT};
use emx::server::LOCK_WAIT_MS;
use emx_core::Config;
use emx_core::journal::{RecoveredBuffer, replay_dir};
use emx_proto::{ClientMsg, LineReader, RowUpdate, ServerMsg, write_msg};
use emx_types::{Cell, Face};

const POLL: Duration = Duration::from_millis(20);
const UNSUPPORTED: &str = "emx: this terminal does not support the Kitty keyboard protocol";
const CONNECTION_LOST: &str = "emx: server connection lost";
const RACERS: usize = 4;

fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(POLL);
    }
}

fn wait_file(path: &Path, content: &str) {
    wait_until(&format!("{} to hold {content:?}", path.display()), || {
        fs::read_to_string(path).is_ok_and(|text| text == content)
    });
}

/// The `noserver-*` journal directories in the state directory.
fn noserver_dirs(env: &TestEnv) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(env.state_home.join("emx")) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("noserver-"))
        })
        .collect();
    dirs.sort();
    dirs
}

/// Whether a replay of `journal` rebuilds `path` with `text`.
fn journaled(journal: &Path, path: &Path, text: &str) -> bool {
    let Ok(recovered) = replay_dir(journal, &Config::default()) else {
        return false;
    };
    recovered.iter().any(|buffer| match buffer {
        RecoveredBuffer::Rebuilt {
            path: Some(journaled),
            text: content,
            ..
        } if journaled == path => {
            let mut bytes = Vec::new();
            content
                .read(0..content.len(), &mut bytes)
                .is_ok_and(|()| bytes == text.as_bytes())
        }
        _ => false,
    })
}

/// Everything written after the last time the program set the terminal up.
fn output_after_setup(pty: &Pty) -> Vec<u8> {
    let raw = pty.raw_output();
    let setup = rfind(&raw, ENTER_ALTERNATE_SCREEN).expect("the terminal was set up");
    raw[setup..].to_vec()
}

fn assert_restored(pty: &Pty) {
    let tail = output_after_setup(pty);
    let text = String::from_utf8_lossy(&tail);
    let push = find(&tail, PUSH_KITTY_FLAGS).expect("flags pushed");
    let pop =
        rfind(&tail, POP_KITTY_FLAGS).unwrap_or_else(|| panic!("flags never popped: {text:?}"));
    assert!(pop > push, "{text:?}");
    let paste_off =
        rfind(&tail, BRACKETED_PASTE_OFF).unwrap_or_else(|| panic!("paste left on: {text:?}"));
    let leave = rfind(&tail, LEAVE_ALTERNATE_SCREEN)
        .unwrap_or_else(|| panic!("alternate screen not left: {text:?}"));
    assert!(
        pop < leave && paste_off < leave,
        "flags pop on the alternate screen: {text:?}"
    );
    assert!(!pty.alternate_screen(), "back on the main screen");
    assert!(!pty.bracketed_paste(), "bracketed paste off");
}

#[test]
fn type_and_save_noserver() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    let path = env.file("f.txt", "");
    let mut pty = Pty::spawn(&env, &["--no-server", "f.txt"]);
    pty.wait_modeline(|modeline| modeline.contains("f.txt"));
    pty.type_text("hello");
    pty.wait_text("hello");
    pty.keys("C-x C-s");
    wait_file(&path, "hello");
    pty.wait_text("Wrote ");
    pty.keys("C-x C-c");
    let status = pty.wait_exit();
    assert!(status.success(), "{status}");
    assert!(session_servers(&env).is_empty(), "no server was started");
    assert!(
        noserver_dirs(&env).is_empty(),
        "a clean exit leaves no journal directory: {:?}",
        noserver_dirs(&env)
    );
}

#[test]
fn unsupported_terminal_exits_with_message() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    env.file("f.txt", "text\n");
    for args in [&["--no-server", "f.txt"][..], &["f.txt"][..]] {
        let mut pty = Pty::spawn_with(&env, args, Probe::Silent, &[]);
        let status = pty.wait_exit();
        assert_eq!(status.exit_code(), 1, "{args:?}: {status}");
        let output = pty.raw_output_text();
        assert!(output.contains(UNSUPPORTED), "{args:?}: {output:?}");
        assert!(!pty.alternate_screen(), "{args:?}: terminal restored");
        assert!(!pty.bracketed_paste(), "{args:?}: terminal restored");
    }
}

#[test]
fn modeline_shows_modified() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    env.file("f.txt", "text\n");
    let pty = Pty::spawn(&env, &["--no-server", "f.txt"]);
    let clean = pty.wait_modeline(|modeline| modeline.contains("f.txt"));
    assert!(clean.contains(":---  F1  f.txt"), "{clean:?}");
    pty.type_text("x");
    let modified = pty.wait_modeline(|modeline| modeline.contains("**"));
    assert!(modified.contains(":**-  F1  f.txt"), "{modified:?}");
}

#[test]
fn server_mode_tui_attach_type_save() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    let path = env.file("f.txt", "");
    let mut pty = Pty::spawn(&env, &["f.txt"]);
    let modeline = pty.wait_modeline(|modeline| modeline.contains("f.txt"));
    assert!(modeline.contains(":@---  F2  f.txt"), "{modeline:?}");
    assert_eq!(
        session_servers(&env).len(),
        1,
        "the client started a server"
    );
    pty.type_text("hello");
    pty.wait_modeline(|modeline| modeline.contains("@**"));
    pty.keys("C-x C-s");
    wait_file(&path, "hello");
    pty.keys("C-x C-c");
    let status = pty.wait_exit();
    assert!(status.success(), "{status}");
    assert_restored(&pty);
}

#[test]
fn detach_reattach_tui_keeps_unsaved() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    let path = env.file("f.txt", "base\n");
    let mut first = Pty::spawn(&env, &["f.txt"]);
    first.wait_modeline(|modeline| modeline.contains("f.txt"));
    first.type_text("unsaved ");
    first.wait_text("unsaved base");
    first.kill9();
    let second = Pty::spawn(&env, &["f.txt"]);
    second.wait_text("unsaved base");
    let modeline = second.wait_modeline(|modeline| modeline.contains("f.txt"));
    assert!(modeline.contains(":@**-  F3  f.txt"), "{modeline:?}");
    assert_eq!(fs::read_to_string(&path).expect("file"), "base\n");
    assert_eq!(session_servers(&env).len(), 1, "one server throughout");
}

#[test]
fn terminal_restored_on_exit() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    env.file("f.txt", "text\n");
    let mut pty = Pty::spawn(&env, &["--no-server", "f.txt"]);
    pty.wait_modeline(|modeline| modeline.contains("f.txt"));
    assert!(pty.alternate_screen());
    assert!(pty.bracketed_paste());
    pty.keys("C-x C-c");
    let status = pty.wait_exit();
    assert!(status.success(), "{status}");
    assert_restored(&pty);
}

#[test]
fn terminal_restored_on_client_panic() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    env.file("f.txt", "text\n");
    let mut pty = Pty::spawn_with(
        &env,
        &["f.txt"],
        Probe::Answer,
        &[("EMX_TEST_CLIENT_PANIC", "1")],
    );
    let status = pty.wait_exit();
    assert!(!status.success(), "{status}");
    assert!(status.signal().is_some(), "aborted: {status}");
    assert_restored(&pty);
    let output = pty.raw_output_text();
    assert!(output.contains("EMX_TEST_CLIENT_PANIC"), "{output:?}");
    // The server outlives its client.
    let server = server_pid(&env.socket_path(), &env.work).expect("server still listening");
    assert!(alive(server));
}

#[test]
fn resize_redraws() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    env.file("f.txt", "text\n");
    for args in [&["--no-server", "f.txt"][..], &["f.txt"][..]] {
        let mut pty = Pty::spawn(&env, args);
        pty.wait_modeline(|modeline| modeline.contains("f.txt"));
        pty.resize(100, 30);
        let modeline = pty.wait_modeline(|modeline| modeline.contains("f.txt"));
        assert!(
            modeline.trim_end().len() > 80,
            "{args:?}: the modeline fills the wider terminal: {modeline:?}"
        );
        let screen = pty.screen();
        assert!(
            !screen[22].contains("f.txt"),
            "{args:?}: the old modeline row is redrawn: {screen:?}"
        );
        pty.resize(60, 10);
        pty.wait_modeline(|modeline| modeline.contains("f.txt"));
        pty.type_text("z");
        pty.wait_text("ztext");
    }
}

#[test]
fn bracketed_paste_inserts_and_yanks() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    for args in [&["--no-server", "f.txt"][..], &["f.txt"][..]] {
        let path = env.file("f.txt", "");
        let mut pty = Pty::spawn(&env, args);
        pty.wait_modeline(|modeline| modeline.contains("f.txt"));
        pty.paste("one\r\ntwo");
        pty.wait_text("two");
        pty.keys("RET C-y");
        pty.wait_screen("the yank", |screen| {
            screen.iter().filter(|row| row.starts_with("two")).count() == 2
        });
        pty.keys("C-x C-s");
        wait_file(&path, "one\ntwo\none\ntwo");
        pty.keys("C-x C-c");
        assert!(pty.wait_exit().success(), "{args:?}");
    }
}

#[test]
fn wait_released_by_c_x_hash() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    let path = env.file("msg.txt", "");
    let mut pty = Pty::spawn(&env, &["--wait", "msg.txt"]);
    pty.wait_modeline(|modeline| modeline.contains("msg.txt"));
    pty.type_text("commit message");
    pty.wait_text("commit message");
    thread::sleep(POLL);
    assert!(alive(pty.pid()), "still waiting");
    pty.keys("C-x #");
    pty.wait_text("Save file");
    pty.keys("y");
    let status = pty.wait_exit();
    assert!(status.success(), "{status}");
    assert_eq!(fs::read_to_string(&path).expect("file"), "commit message");
    assert_restored(&pty);
}

#[test]
fn autostart_race_single_server() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    env.file("f.txt", "text\n");
    let starters: Vec<_> = (0..RACERS)
        .map(|_| {
            env.emx()
                .args(["server", "--detach"])
                .spawn()
                .expect("spawn emx server --detach")
        })
        .collect();
    let clients: Vec<Pty> = (0..2).map(|_| Pty::spawn(&env, &["f.txt"])).collect();
    for mut starter in starters {
        let status = starter.wait().expect("wait for the starter");
        assert!(status.success(), "every starter finds a server: {status}");
    }
    for client in &clients {
        client.wait_modeline(|modeline| modeline.contains("f.txt"));
    }
    let server = server_pid(&env.socket_path(), &env.work).expect("a server");
    // Losers give up once they have waited for the lock.
    thread::sleep(Duration::from_millis(LOCK_WAIT_MS) + Duration::from_secs(1));
    assert_eq!(session_servers(&env), vec![server], "exactly one server");
    let mut frames: Vec<&str> = clients
        .iter()
        .map(|client| {
            let modeline = client.modeline();
            ["F2", "F3"]
                .into_iter()
                .find(|frame| modeline.contains(&format!(":@---  {frame}  f.txt")))
                .unwrap_or_else(|| panic!("{modeline:?}"))
        })
        .collect();
    frames.sort();
    assert_eq!(
        frames,
        vec!["F2", "F3"],
        "both clients share the one server"
    );
}

#[test]
fn noserver_crash_recovered_by_next_noserver() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    let path = env.file("f.txt", "base\n");
    let mut first = Pty::spawn(&env, &["--no-server", "f.txt"]);
    first.wait_modeline(|modeline| modeline.contains("f.txt"));
    first.type_text("unsaved ");
    first.wait_text("unsaved base");
    let crashed_dir = env
        .state_home
        .join("emx")
        .join(format!("noserver-{}", first.pid()));
    wait_until("the edit to be journaled", || {
        journaled(&crashed_dir, &path, "unsaved base\n")
    });
    first.kill9();
    let mut second = Pty::spawn(&env, &["--no-server", "f.txt"]);
    second.wait_text("unsaved base");
    let modeline = second.wait_modeline(|modeline| modeline.contains("f.txt"));
    assert!(modeline.contains(":**-  F1  f.txt"), "{modeline:?}");
    assert_eq!(fs::read_to_string(&path).expect("file"), "base\n");
    let own_dir = env
        .state_home
        .join("emx")
        .join(format!("noserver-{}", second.pid()));
    assert_eq!(
        noserver_dirs(&env),
        vec![own_dir.clone()],
        "the crashed session's journals moved into this one"
    );
    assert!(journaled(&own_dir, &path, "unsaved base\n"));
    second.keys("C-x C-c");
    second.wait_text("Save file");
    second.keys("n");
    second.wait_text("exit anyway?");
    second.type_text("yes");
    second.keys("RET");
    assert!(second.wait_exit().success());
    assert!(noserver_dirs(&env).is_empty(), "{:?}", noserver_dirs(&env));
    assert_eq!(fs::read_to_string(&path).expect("file"), "base\n");
}

#[test]
fn live_noserver_journals_are_left_alone() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    let path = env.file("f.txt", "base\n");
    let first = Pty::spawn(&env, &["--no-server", "f.txt"]);
    first.wait_modeline(|modeline| modeline.contains("f.txt"));
    first.type_text("mine ");
    let first_dir = env
        .state_home
        .join("emx")
        .join(format!("noserver-{}", first.pid()));
    wait_until("the edit to be journaled", || {
        journaled(&first_dir, &path, "mine base\n")
    });
    let second = Pty::spawn(&env, &["--no-server", "f.txt"]);
    let modeline = second.wait_modeline(|modeline| modeline.contains("f.txt"));
    assert!(modeline.contains(":---  F1  f.txt"), "{modeline:?}");
    let screen = second.wait_text("base");
    assert!(
        !screen.iter().any(|row| row.contains("mine base")),
        "{screen:?}"
    );
    assert!(
        journaled(&first_dir, &path, "mine base\n"),
        "still the first's"
    );
    assert_eq!(noserver_dirs(&env).len(), 2);
}

#[test]
fn server_lost_restores_and_reports() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    env.file("f.txt", "text\n");
    let mut pty = Pty::spawn(&env, &["f.txt"]);
    pty.wait_modeline(|modeline| modeline.contains("f.txt"));
    let server = server_pid(&env.socket_path(), &env.work).expect("a server");
    common::pty::kill9(server);
    let status = pty.wait_exit();
    assert_eq!(status.exit_code(), 1, "{status}");
    assert_restored(&pty);
    let output = pty.raw_output_text();
    assert!(output.contains(CONNECTION_LOST), "{output:?}");
}

#[test]
fn file_line_col_argument_places_point() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    env.file("f.txt", "one\ntwo\nthree\n");
    let pty = Pty::spawn(&env, &["--no-server", "f.txt:3:2"]);
    pty.wait_modeline(|modeline| modeline.contains("f.txt") && modeline.contains("L3"));
    pty.type_text("X");
    pty.wait_text("tXhree");
}

/// Listens on the session's socket as a server that is exiting: each of
/// the first `hangups` connections is closed before the welcome. After
/// the last, the socket is removed when `remove` is set, as an exiting
/// server removes it.
fn exiting_server(env: &TestEnv, hangups: usize, remove: bool) -> thread::JoinHandle<()> {
    fs::create_dir_all(&env.socket_dir).expect("socket dir");
    fs::set_permissions(&env.socket_dir, fs::Permissions::from_mode(0o700)).expect("mode");
    let socket = env.socket_path();
    let listener = UnixListener::bind(&socket).expect("bind the socket");
    thread::spawn(move || {
        let mut listener = Some(listener);
        for hangup in 1..=hangups {
            let (stream, _) = listener
                .as_ref()
                .expect("listening until the last hangup")
                .accept()
                .expect("accept");
            // Gone before the hangup, so the retry finds no server.
            if hangup == hangups && remove {
                fs::remove_file(&socket).expect("remove the socket");
                listener = None;
            }
            stream.shutdown(std::net::Shutdown::Both).expect("hang up");
        }
    })
}

#[test]
fn connection_closed_before_welcome_is_retried_once() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    env.file("f.txt", "text\n");
    let exiting = exiting_server(&env, 1, true);
    let pty = Pty::spawn(&env, &["f.txt"]);
    exiting.join().expect("the exiting server");
    let modeline = pty.wait_modeline(|modeline| modeline.contains("f.txt"));
    assert!(modeline.contains(":@---  F2  f.txt"), "{modeline:?}");
    assert_eq!(session_servers(&env).len(), 1, "a new server was started");
}

#[test]
fn a_second_early_close_is_a_lost_connection() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    env.file("f.txt", "text\n");
    let exiting = exiting_server(&env, 2, false);
    let mut pty = Pty::spawn(&env, &["f.txt"]);
    let status = pty.wait_exit();
    exiting.join().expect("the exiting server");
    assert_eq!(status.exit_code(), 1, "{status}");
    assert_restored(&pty);
    let output = pty.raw_output_text();
    assert!(output.contains(CONNECTION_LOST), "{output:?}");
}

#[test]
fn noserver_hangup_keeps_unsaved_for_the_next() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    let path = env.file("f.txt", "base\n");
    let mut first = Pty::spawn(&env, &["--no-server", "f.txt"]);
    first.wait_modeline(|modeline| modeline.contains("f.txt"));
    first.type_text("kept ");
    first.wait_text("kept base");
    let pid = rustix::process::Pid::from_raw(first.pid() as i32).expect("a pid");
    rustix::process::kill_process(pid, rustix::process::Signal::HUP).expect("hang up");
    let status = first.wait_exit();
    assert_eq!(status.exit_code(), 1, "{status}");
    assert_restored(&first);
    let first_dir = env
        .state_home
        .join("emx")
        .join(format!("noserver-{}", first.pid()));
    assert!(
        journaled(&first_dir, &path, "kept base\n"),
        "the journal is kept, synced"
    );
    let second = Pty::spawn(&env, &["--no-server", "f.txt"]);
    second.wait_text("kept base");
    let modeline = second.wait_modeline(|modeline| modeline.contains("f.txt"));
    assert!(modeline.contains(":**-  F1  f.txt"), "{modeline:?}");
    assert!(!first_dir.exists(), "adopted and removed");
}

/// A whole frame of `cols` x `rows` with `text` on its first row.
fn whole_frame(cols: u16, rows: u16, text: &str) -> ServerMsg {
    let mut first: Vec<Cell> = text
        .chars()
        .map(|character| Cell {
            text: character.to_string(),
            width: 1,
            face: Face::Default,
        })
        .collect();
    first.resize(usize::from(cols), Cell::blank());
    ServerMsg::Frame {
        full: true,
        cols,
        rows,
        rows_changed: (0..rows)
            .map(|row| RowUpdate {
                row,
                cells: if row == 0 {
                    first.clone()
                } else {
                    vec![Cell::blank(); usize::from(cols)]
                },
            })
            .collect(),
        cursor: Some((0, 0)),
    }
}

/// The client is driven by a fake server that sends frames the client
/// must refuse: one for another size, then rows outside the terminal.
#[test]
fn unusable_frames_are_discarded_and_a_redraw_requested() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    fs::create_dir_all(&env.socket_dir).expect("socket dir");
    fs::set_permissions(&env.socket_dir, fs::Permissions::from_mode(0o700)).expect("mode");
    let listener = UnixListener::bind(env.socket_path()).expect("bind the socket");
    let mut pty = Pty::spawn(&env, &[]);
    let (stream, _) = listener.accept().expect("accept the client");
    stream.set_read_timeout(Some(WAIT)).expect("read timeout");
    let mut reader = LineReader::new(stream.try_clone().expect("clone"));
    let mut writer = stream.try_clone().expect("clone");
    let hello = reader
        .read_msg::<ClientMsg>()
        .expect("read")
        .expect("hello");
    let ClientMsg::Hello { cols, rows, .. } = hello else {
        panic!("{hello:?}");
    };
    write_msg(&mut writer, &ServerMsg::Welcome { server_pid: 1 }).expect("welcome");
    write_msg(&mut writer, &whole_frame(cols, rows, "first")).expect("frame");
    pty.wait_text("first");
    write_msg(&mut writer, &whole_frame(cols - 1, rows, "wrong size")).expect("frame");
    assert_eq!(
        reader.read_msg::<ClientMsg>().expect("read"),
        Some(ClientMsg::Redraw)
    );
    // Refused too, but the redraw asked for is already on its way.
    write_msg(&mut writer, &whole_frame(cols, rows + 1, "also wrong")).expect("frame");
    write_msg(&mut writer, &whole_frame(cols, rows, "redrawn")).expect("frame");
    pty.wait_text("redrawn");
    let outside = ServerMsg::Frame {
        full: false,
        cols,
        rows,
        rows_changed: vec![RowUpdate {
            row: rows,
            cells: vec![Cell::blank(); usize::from(cols)],
        }],
        cursor: None,
    };
    write_msg(&mut writer, &outside).expect("frame");
    assert_eq!(
        reader.read_msg::<ClientMsg>().expect("read"),
        Some(ClientMsg::Redraw)
    );
    let screen = pty.screen();
    assert!(
        screen[0].starts_with("redrawn") && !screen.iter().any(|row| row.contains("wrong")),
        "{screen:?}"
    );
    // Reading stays open: what the client sent before it saw the hangup
    // is checked below.
    stream.shutdown(std::net::Shutdown::Write).expect("hang up");
    assert_eq!(pty.wait_exit().exit_code(), 1);
    while let Ok(Some(message)) = reader.read_msg::<ClientMsg>() {
        assert_ne!(message, ClientMsg::Redraw, "one redraw per unusable run");
    }
}
