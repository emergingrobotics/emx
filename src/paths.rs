//! Where the session's socket, journals, log and config live (DESIGN
//! section 2, privilege isolation).
//!
//! The socket directory is keyed by effective UID and never taken from
//! `XDG_RUNTIME_DIR`, which `sudo` may or may not keep: `sudo emx` must
//! neither attach to the user's daemon nor put a root daemon's socket in a
//! directory the user controls. Each lookup has a `_with` form that reads
//! the environment through a function, so tests need not change the
//! process environment.

use std::ffi::OsString;
use std::fs::{self, DirBuilder, Metadata};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

/// Overrides the socket directory; it must still pass the ownership check.
pub const SOCKET_DIR_ENV: &str = "EMX_SOCKET_DIR";
pub const RUN_USER: &str = "/run/user";
const RUN_USER_SUBDIRECTORY: &str = "emx";
const TMP_PREFIX: &str = "/tmp/emx-";
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PERMISSION_BITS: u32 = 0o7777;
const SOCKET_NAME: &str = "server.sock";
const LOCK_NAME: &str = "server.lock";
const LOG_NAME: &str = "server.log";
const APPLICATION: &str = "emx";
const CONFIG_NAME: &str = "config.toml";
const SERVER_JOURNALS: &str = "server";
pub const NO_SERVER_JOURNALS_PREFIX: &str = "noserver-";
const HOME: &str = "HOME";
const XDG_STATE_HOME: &str = "XDG_STATE_HOME";
const XDG_CONFIG_HOME: &str = "XDG_CONFIG_HOME";
const HOME_STATE: &str = ".local/state";
const HOME_CONFIG: &str = ".config";

#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("cannot create {}: {source}", .path.display())]
    Create { path: PathBuf, source: io::Error },
    #[error("cannot inspect {}: {source}", .path.display())]
    Inspect { path: PathBuf, source: io::Error },
    #[error("refusing {}: it is not a directory (a symlink is not followed)", .path.display())]
    NotDirectory { path: PathBuf },
    #[error(
        "refusing {}: owned by uid {owner} with mode {mode:04o}; it must be owned by uid {euid} with mode 0700",
        .path.display()
    )]
    Unsafe {
        path: PathBuf,
        owner: u32,
        mode: u32,
        euid: u32,
    },
    #[error(
        "cannot find a place for {what}: neither {variable} nor HOME is set to an absolute path"
    )]
    NoHome {
        what: &'static str,
        variable: &'static str,
    },
}

/// Which journal directory: the session server's, or one `--no-server`
/// process's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalMode {
    Server,
    NoServer { pid: u32 },
}

fn process_environment(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

/// The socket directory for `euid`, created if missing and checked.
pub fn socket_dir(euid: u32) -> Result<PathBuf, PathError> {
    socket_dir_with(&process_environment, euid, Path::new(RUN_USER))
}

/// `socket_dir` with the environment read through `env` and `/run/user`
/// at `run_user`.
pub fn socket_dir_with(
    env: &dyn Fn(&str) -> Option<OsString>,
    euid: u32,
    run_user: &Path,
) -> Result<PathBuf, PathError> {
    let directory = resolve_socket_dir(env, euid, run_user);
    prepare_private_dir(&directory, euid)?;
    Ok(directory)
}

/// `EMX_SOCKET_DIR` if set, else `<run_user>/<euid>/emx` when
/// `<run_user>/<euid>` exists, else `/tmp/emx-<euid>`. Nothing is created.
pub fn resolve_socket_dir(
    env: &dyn Fn(&str) -> Option<OsString>,
    euid: u32,
    run_user: &Path,
) -> PathBuf {
    if let Some(directory) = env(SOCKET_DIR_ENV).filter(|value| !value.is_empty()) {
        return PathBuf::from(directory);
    }
    let user_runtime = run_user.join(euid.to_string());
    if user_runtime.is_dir() {
        user_runtime.join(RUN_USER_SUBDIRECTORY)
    } else {
        PathBuf::from(format!("{TMP_PREFIX}{euid}"))
    }
}

/// Creates `directory` with mode 0700 if it does not exist (its parent
/// must), then checks it without following a symlink.
pub fn prepare_private_dir(directory: &Path, euid: u32) -> Result<(), PathError> {
    match DirBuilder::new()
        .mode(PRIVATE_DIRECTORY_MODE)
        .create(directory)
    {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(source) => {
            return Err(PathError::Create {
                path: directory.to_path_buf(),
                source,
            });
        }
    }
    let metadata = fs::symlink_metadata(directory).map_err(|source| PathError::Inspect {
        path: directory.to_path_buf(),
        source,
    })?;
    check_dir(directory, &metadata, euid)
}

/// The directory (as `symlink_metadata` describes it) must be a real
/// directory owned by `euid` with mode exactly 0700.
pub fn check_dir(directory: &Path, metadata: &Metadata, euid: u32) -> Result<(), PathError> {
    if !metadata.is_dir() {
        return Err(PathError::NotDirectory {
            path: directory.to_path_buf(),
        });
    }
    let mode = metadata.mode() & PERMISSION_BITS;
    if metadata.uid() != euid || mode != PRIVATE_DIRECTORY_MODE {
        return Err(PathError::Unsafe {
            path: directory.to_path_buf(),
            owner: metadata.uid(),
            mode,
            euid,
        });
    }
    Ok(())
}

pub fn socket_path(socket_dir: &Path) -> PathBuf {
    socket_dir.join(SOCKET_NAME)
}

/// The lock held by the one running server.
pub fn lock_path(socket_dir: &Path) -> PathBuf {
    socket_dir.join(LOCK_NAME)
}

/// An absolute directory named by `variable`, else `HOME` joined with
/// `home_relative`. The XDG spec says relative values are ignored.
fn xdg_dir(
    env: &dyn Fn(&str) -> Option<OsString>,
    variable: &'static str,
    home_relative: &str,
    what: &'static str,
) -> Result<PathBuf, PathError> {
    let absolute = |name: &str| {
        env(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    if let Some(directory) = absolute(variable) {
        return Ok(directory);
    }
    absolute(HOME)
        .map(|home| home.join(home_relative))
        .ok_or(PathError::NoHome { what, variable })
}

/// `$XDG_STATE_HOME/emx`, or `~/.local/state/emx`.
pub fn state_dir() -> Result<PathBuf, PathError> {
    state_dir_with(&process_environment)
}

pub fn state_dir_with(env: &dyn Fn(&str) -> Option<OsString>) -> Result<PathBuf, PathError> {
    Ok(xdg_dir(env, XDG_STATE_HOME, HOME_STATE, "the state directory")?.join(APPLICATION))
}

/// Where the edit journals of `mode` live, under the state directory.
pub fn journal_dir(mode: JournalMode) -> Result<PathBuf, PathError> {
    journal_dir_with(&process_environment, mode)
}

pub fn journal_dir_with(
    env: &dyn Fn(&str) -> Option<OsString>,
    mode: JournalMode,
) -> Result<PathBuf, PathError> {
    let name = match mode {
        JournalMode::Server => SERVER_JOURNALS.to_string(),
        JournalMode::NoServer { pid } => format!("{NO_SERVER_JOURNALS_PREFIX}{pid}"),
    };
    Ok(state_dir_with(env)?.join(name))
}

/// The detached server's stderr.
pub fn server_log_path() -> Result<PathBuf, PathError> {
    Ok(state_dir()?.join(LOG_NAME))
}

/// `$XDG_CONFIG_HOME/emx/config.toml`, or `~/.config/emx/config.toml`.
pub fn config_path() -> Result<PathBuf, PathError> {
    config_path_with(&process_environment)
}

pub fn config_path_with(env: &dyn Fn(&str) -> Option<OsString>) -> Result<PathBuf, PathError> {
    Ok(
        xdg_dir(env, XDG_CONFIG_HOME, HOME_CONFIG, "the config file")?
            .join(APPLICATION)
            .join(CONFIG_NAME),
    )
}
