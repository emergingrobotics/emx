//! `emx` inside real terminal multiplexers: a detached tmux session and a
//! detached GNU screen session each run it, keys go in the way a user's
//! terminal would send them through the multiplexer, and the screen is
//! read back from the multiplexer itself. Both programs are required, as
//! Emacs is for the differential tests.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use common::pty::SessionReaper;
use common::{TestEnv, WAIT};

const POLL: Duration = Duration::from_millis(50);
const SESSION: &str = "emx";
/// Room for the escape timeout between a lone ESC and the next key.
const AFTER_ESCAPE: Duration = Duration::from_millis(300);

fn run(command: &mut Command) -> Output {
    let output = command.output().unwrap_or_else(|error| {
        panic!("running {command:?}: {error} (tmux and screen are required for these tests)")
    });
    assert!(
        output.status.success(),
        "{command:?}: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// The shell command that runs `emx` with the test session's environment.
fn emx_command(env: &TestEnv, args: &[&str]) -> String {
    let quote = |path: &Path| format!("'{}'", path.display());
    format!(
        "cd {} && exec env -u XDG_RUNTIME_DIR EMX_SOCKET_DIR={} XDG_STATE_HOME={} XDG_CONFIG_HOME={} HOME={} {} {}",
        quote(&env.work),
        quote(&env.socket_dir),
        quote(&env.state_home),
        quote(&env.config_home),
        quote(&env.home),
        quote(Path::new(env!("CARGO_BIN_EXE_emx"))),
        args.join(" ")
    )
}

/// A multiplexer session running `emx`.
trait Multiplexer {
    /// Sends bytes as if typed at the user's terminal.
    fn send(&self, bytes: &[u8]);
    fn screen(&self) -> String;
    fn running(&self) -> bool;
}

struct Tmux {
    socket: PathBuf,
}

impl Tmux {
    fn start(env: &TestEnv, scratch: &Path, args: &[&str]) -> Tmux {
        let tmux = Tmux {
            socket: scratch.join("tmux.sock"),
        };
        run(tmux
            .command()
            .args(["new-session", "-d", "-s", SESSION, "-x", "80", "-y", "24"])
            .arg(emx_command(env, args)));
        tmux
    }

    fn command(&self) -> Command {
        let mut command = Command::new("tmux");
        command
            .arg("-S")
            .arg(&self.socket)
            .args(["-f", "/dev/null"]);
        command
    }
}

impl Multiplexer for Tmux {
    fn send(&self, bytes: &[u8]) {
        let mut command = self.command();
        command.args(["send-keys", "-t", SESSION, "-H"]);
        for byte in bytes {
            command.arg(format!("{byte:02x}"));
        }
        run(&mut command);
    }

    fn screen(&self) -> String {
        let output = run(self.command().args(["capture-pane", "-p", "-t", SESSION]));
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn running(&self) -> bool {
        self.command()
            .args(["has-session", "-t", SESSION])
            .output()
            .is_ok_and(|output| output.status.success())
    }
}

impl Drop for Tmux {
    fn drop(&mut self) {
        drop(self.command().arg("kill-server").output());
    }
}

struct Screen {
    directory: PathBuf,
    hardcopy: PathBuf,
}

impl Screen {
    fn start(env: &TestEnv, scratch: &Path, args: &[&str]) -> Screen {
        let directory = scratch.join("screen");
        fs::create_dir(&directory).expect("screen directory");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).expect("chmod");
        let screen = Screen {
            directory,
            hardcopy: scratch.join("hardcopy"),
        };
        run(screen
            .command()
            .args(["-d", "-m", "-S", SESSION, "-T", "screen", "sh", "-c"])
            .arg(emx_command(env, args)));
        screen
    }

    fn command(&self) -> Command {
        let mut command = Command::new("screen");
        command
            .env("SCREENDIR", &self.directory)
            .env("SCREENRC", "/dev/null")
            .env_remove("STY");
        command
    }
}

impl Multiplexer for Screen {
    fn send(&self, bytes: &[u8]) {
        let text = String::from_utf8(bytes.to_vec()).expect("screen stuff takes text");
        run(self
            .command()
            .args(["-S", SESSION, "-p", "0", "-X", "stuff"])
            .arg(text));
    }

    fn screen(&self) -> String {
        drop(fs::remove_file(&self.hardcopy));
        let copied = self
            .command()
            .args(["-S", SESSION, "-p", "0", "-X", "hardcopy"])
            .arg(&self.hardcopy)
            .output()
            .is_ok_and(|output| output.status.success());
        if !copied {
            return String::new();
        }
        fs::read_to_string(&self.hardcopy).unwrap_or_default()
    }

    fn running(&self) -> bool {
        self.command()
            .args(["-S", SESSION, "-Q", "select", "."])
            .output()
            .is_ok_and(|output| output.status.success())
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        drop(self.command().args(["-S", SESSION, "-X", "quit"]).output());
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(POLL);
    }
}

fn wait_screen(session: &dyn Multiplexer, what: &str, want: impl Fn(&str) -> bool) {
    let deadline = Instant::now() + WAIT;
    loop {
        let screen = session.screen();
        if want(&screen) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; screen:\n{screen}"
        );
        thread::sleep(POLL);
    }
}

/// Opens a file, moves with Meta and Ctrl keys, edits, saves and quits:
/// the keys a user types, as bytes their terminal sends.
fn edit_save_quit(session: &dyn Multiplexer, path: &Path) {
    wait_screen(session, "the file shown", |screen| {
        screen.contains("f.txt") && screen.contains("one") && !screen.contains("Loading")
    });
    session.send(b"\x1b>");
    session.send(b"end");
    session.send(b"\x1b<");
    session.send(b"start ");
    session.send(b"\x05\x1bb\x1bu");
    wait_screen(session, "the edits", |screen| screen.contains("start ONE"));
    session.send(b"\x1b");
    thread::sleep(AFTER_ESCAPE);
    session.send(b"<");
    session.send(b"\x18\x13");
    wait_until("the save", || {
        fs::read_to_string(path).is_ok_and(|text| text == "start ONE\ntwo\nend")
    });
    wait_screen(session, "the save message", |screen| {
        screen.contains("Wrote ")
    });
    session.send(b"\x18\x03");
    wait_until("emx to exit", || !session.running());
}

#[test]
fn tmux_noserver() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    let scratch = tempfile::tempdir().expect("tempdir");
    let path = env.file("f.txt", "one\ntwo\n");
    let tmux = Tmux::start(&env, scratch.path(), &["--no-server", "f.txt"]);
    edit_save_quit(&tmux, &path);
}

#[test]
fn tmux_server() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    let scratch = tempfile::tempdir().expect("tempdir");
    let path = env.file("f.txt", "one\ntwo\n");
    let tmux = Tmux::start(&env, scratch.path(), &["f.txt"]);
    edit_save_quit(&tmux, &path);
}

#[test]
fn screen_noserver() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    let scratch = tempfile::tempdir().expect("tempdir");
    let path = env.file("f.txt", "one\ntwo\n");
    let screen = Screen::start(&env, scratch.path(), &["--no-server", "f.txt"]);
    edit_save_quit(&screen, &path);
}

#[test]
fn screen_server() {
    let env = TestEnv::new();
    let _reaper = SessionReaper::new(&env);
    let scratch = tempfile::tempdir().expect("tempdir");
    let path = env.file("f.txt", "one\ntwo\n");
    let screen = Screen::start(&env, scratch.path(), &["f.txt"]);
    edit_save_quit(&screen, &path);
}
