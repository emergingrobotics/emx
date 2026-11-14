//! Where the session's socket, journals and config live, and the checks
//! that keep another user out of them (DESIGN section 2).

use std::cell::RefCell;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use emx::paths::{
    self, JournalMode, PathError, check_dir, config_path_with, journal_dir_with,
    resolve_socket_dir, socket_dir_with, state_dir_with,
};

const PRIVATE: u32 = 0o700;
const OPEN: u32 = 0o755;
const PERMISSION_BITS: u32 = 0o7777;

fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// An environment holding only `variables`.
fn environment(variables: &[(&str, &Path)]) -> impl Fn(&str) -> Option<OsString> + use<> {
    let variables: Vec<(String, OsString)> = variables
        .iter()
        .map(|(name, value)| (name.to_string(), value.as_os_str().to_owned()))
        .collect();
    move |name| {
        variables
            .iter()
            .find(|(variable, _)| variable == name)
            .map(|(_, value)| value.clone())
    }
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("stat").permissions().mode() & PERMISSION_BITS
}

#[test]
fn socket_dir_rejects_wrong_mode() {
    let directory = tempfile::tempdir().expect("tempdir");
    let sockets = directory.path().join("sockets");
    fs::create_dir(&sockets).expect("mkdir");
    fs::set_permissions(&sockets, fs::Permissions::from_mode(OPEN)).expect("chmod");
    let env = environment(&[(paths::SOCKET_DIR_ENV, &sockets)]);
    let error = socket_dir_with(&env, euid(), directory.path()).expect_err("0755 is refused");
    let message = error.to_string();
    assert!(matches!(error, PathError::Unsafe { .. }), "{error:?}");
    assert!(
        message.contains(&sockets.display().to_string()),
        "{message}"
    );
    assert!(message.contains("0755"), "{message}");
    assert!(message.contains(&format!("uid {}", euid())), "{message}");
}

#[test]
fn socket_dir_rejects_foreign_owner() {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(PRIVATE)).expect("chmod");
    let metadata = fs::symlink_metadata(directory.path()).expect("stat");
    let other = metadata.uid() + 1;
    let error = check_dir(directory.path(), &metadata, other).expect_err("owner differs");
    assert!(
        matches!(error, PathError::Unsafe { owner, euid, .. } if owner == metadata.uid() && euid == other),
        "{error:?}"
    );
    assert!(check_dir(directory.path(), &metadata, metadata.uid()).is_ok());
}

#[test]
fn socket_dir_rejects_a_symlink() {
    let directory = tempfile::tempdir().expect("tempdir");
    let target = directory.path().join("target");
    fs::create_dir(&target).expect("mkdir");
    fs::set_permissions(&target, fs::Permissions::from_mode(PRIVATE)).expect("chmod");
    let link = directory.path().join("link");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let env = environment(&[(paths::SOCKET_DIR_ENV, &link)]);
    let error = socket_dir_with(&env, euid(), directory.path()).expect_err("a symlink is refused");
    assert!(matches!(error, PathError::NotDirectory { .. }), "{error:?}");
}

#[test]
fn socket_dir_override_is_created_private() {
    let directory = tempfile::tempdir().expect("tempdir");
    let sockets = directory.path().join("new");
    let env = environment(&[(paths::SOCKET_DIR_ENV, &sockets)]);
    let found = socket_dir_with(&env, euid(), directory.path()).expect("created");
    assert_eq!(found, sockets);
    assert_eq!(mode(&sockets), PRIVATE);
}

#[test]
fn xdg_runtime_dir_ignored() {
    let directory = tempfile::tempdir().expect("tempdir");
    let runtime = directory.path().join("xdg-runtime");
    fs::create_dir(&runtime).expect("mkdir");
    let run_user = directory.path().join("run-user");
    fs::create_dir_all(run_user.join(euid().to_string())).expect("mkdir");
    let asked = RefCell::new(Vec::new());
    let base = environment(&[("XDG_RUNTIME_DIR", &runtime)]);
    let env = |name: &str| {
        asked.borrow_mut().push(name.to_string());
        base(name)
    };
    let found = socket_dir_with(&env, euid(), &run_user).expect("socket dir");
    assert_eq!(found, run_user.join(euid().to_string()).join("emx"));
    assert_eq!(mode(&found), PRIVATE);
    assert!(
        !asked.borrow().iter().any(|name| name == "XDG_RUNTIME_DIR"),
        "asked for {:?}",
        asked.borrow()
    );
}

#[test]
fn socket_dir_falls_back_to_tmp_without_run_user() {
    let directory = tempfile::tempdir().expect("tempdir");
    let env = environment(&[]);
    assert_eq!(
        resolve_socket_dir(&env, 4242, directory.path()),
        PathBuf::from("/tmp/emx-4242")
    );
    fs::create_dir(directory.path().join("4242")).expect("mkdir");
    assert_eq!(
        resolve_socket_dir(&env, 4242, directory.path()),
        directory.path().join("4242").join("emx")
    );
}

#[test]
fn state_and_config_follow_xdg_then_home() {
    let home = Path::new("/home/someone");
    let state = Path::new("/xdg/state");
    let config = Path::new("/xdg/config");
    let with_xdg = environment(&[
        ("HOME", home),
        ("XDG_STATE_HOME", state),
        ("XDG_CONFIG_HOME", config),
    ]);
    assert_eq!(state_dir_with(&with_xdg).expect("state"), state.join("emx"));
    assert_eq!(
        config_path_with(&with_xdg).expect("config"),
        config.join("emx").join("config.toml")
    );
    assert_eq!(
        journal_dir_with(&with_xdg, JournalMode::Server).expect("journal"),
        state.join("emx").join("server")
    );
    assert_eq!(
        journal_dir_with(&with_xdg, JournalMode::NoServer { pid: 77 }).expect("journal"),
        state.join("emx").join("noserver-77")
    );
    let relative = Path::new("relative");
    let home_only = environment(&[("HOME", home), ("XDG_STATE_HOME", relative)]);
    assert_eq!(
        state_dir_with(&home_only).expect("state"),
        home.join(".local/state/emx"),
        "a relative XDG directory is ignored, as the spec says"
    );
    assert_eq!(
        config_path_with(&home_only).expect("config"),
        home.join(".config/emx/config.toml")
    );
    let nothing = environment(&[]);
    assert!(matches!(
        state_dir_with(&nothing),
        Err(PathError::NoHome { .. })
    ));
}
