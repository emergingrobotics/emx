//! `emx --no-server`: the same editor and terminal client in one process,
//! joined by an in-process link instead of a socket (DESIGN section 2).
//!
//! Edits are journaled in `noserver-<pid>/` under the state directory,
//! which this process locks for as long as it runs. A process that died
//! without exiting leaves its directory unlocked; the next `--no-server`
//! moves those journals into its own directory and recovers them, so a
//! crash loses nothing here either.

use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::panic;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use anyhow::{Context, bail};
use emx_core::journal::{JournalWriter, RecoveredBuffer, replay_dir};
use emx_core::{Config, Event, Session};
use emx_proto::ClientMsg;
use rustix::fs::{CWD, FlockOperation, RenameFlags};

use crate::client::{self, Link, UiEvent};
use crate::paths::{self, JournalMode, NO_SERVER_JOURNALS_PREFIX};
use crate::server::local::LocalLink;
use crate::server::{self, Input, Server};

const LOCK_NAME: &str = "lock";
/// Where `--no-server` sessions write their diagnostics, in the state
/// directory.
pub const LOG_NAME: &str = "noserver.log";
const JOURNAL_EXTENSION: &str = "jnl";
/// A journal being replaced; the `.jnl` it would replace is the valid one.
const REPLACEMENT_SUFFIX: &str = ".jnl.new";
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
/// Claiming the directory is retried while another process recovering a
/// crashed session removes it under us; each retry means it made progress.
const CLAIM_ATTEMPTS: usize = 16;

pub fn run(files: &[OsString]) -> anyhow::Result<ExitCode> {
    let files = client::parse_file_arguments(files);
    let terminal_stderr = divert_stderr()?;
    let (sender, events) = mpsc::channel();
    let mut started = None;
    let outcome = client::run_tui(
        |sender: &Sender<UiEvent>| -> anyhow::Result<Box<dyn Link>> {
            let (link, editor) = start(sender)?;
            started = Some(editor);
            Ok(Box::new(link))
        },
        sender,
        events,
        &files,
        false,
    );
    let finished = match started {
        Some(editor) => editor.finish(),
        None => Ok(()),
    };
    rustix::stdio::dup2_stderr(&terminal_stderr).context("restoring stderr")?;
    let code = outcome?.report();
    finished?;
    Ok(code)
}

/// The editor's diagnostics (a failed journal sync, an auto-save that
/// could not be removed) are written to stderr, which here is the terminal
/// the client draws on by diffs: a line printed there would stay on screen.
/// For the session stderr goes to `noserver.log` in the state directory;
/// the terminal's stderr is returned for the exit message, and a panic
/// puts it back so its message reaches the user.
fn divert_stderr() -> anyhow::Result<Arc<OwnedFd>> {
    let state = paths::state_dir()?;
    create_private_dir(&state)?;
    let log_path = state.join(LOG_NAME);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(PRIVATE_FILE_MODE)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(&log_path)
        .with_context(|| format!("opening the log {}", log_path.display()))?;
    let terminal = Arc::new(
        io::stderr()
            .as_fd()
            .try_clone_to_owned()
            .context("keeping the terminal's stderr")?,
    );
    let panic_stderr = terminal.clone();
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let restored = rustix::stdio::dup2_stderr(&*panic_stderr);
        previous(info);
        if let Err(error) = restored {
            eprintln!("emx: could not restore stderr for the panic message: {error}");
        }
    }));
    rustix::stdio::dup2_stderr(&log).context("sending stderr to the log")?;
    Ok(terminal)
}

fn create_private_dir(path: &Path) -> anyhow::Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(PRIVATE_DIRECTORY_MODE)
        .create(path)
        .with_context(|| format!("creating {}", path.display()))
}

/// A file or empty directory another process already removed is as good
/// as removed.
fn removed(result: io::Result<()>, path: &Path) -> anyhow::Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

/// The editor thread and the journal directory it writes.
struct EditorThread {
    inputs: Sender<Input>,
    thread: JoinHandle<anyhow::Result<()>>,
    journals: JournalDir,
}

impl EditorThread {
    /// Waits for the editor to stop (asking it to, if the client ended
    /// without its `Exit`), then removes the journal directory unless
    /// journals were kept in it.
    fn finish(self) -> anyhow::Result<()> {
        // Already stopped is fine: the editor ended the session itself.
        drop(self.inputs.send(Input::Terminate));
        let outcome = match self.thread.join() {
            Ok(outcome) => outcome,
            Err(_) => bail!("the editor thread panicked"),
        };
        self.journals.release()?;
        outcome
    }
}

fn start(sender: &Sender<UiEvent>) -> anyhow::Result<(LocalClient, EditorThread)> {
    let config = server::load_config()?;
    let state = paths::state_dir()?;
    let journals = JournalDir::claim(&state, std::process::id())?;
    journals.recover_orphans(&state)?;
    let writer = JournalWriter::new(&journals.path)
        .with_context(|| format!("opening the journal directory {}", journals.path.display()))?;
    let recovered = replay_dir(&journals.path, &config)?;
    let (inputs, receiver) = mpsc::channel();
    let editor_inputs = inputs.clone();
    let editor_failed = sender.clone();
    let thread = thread::Builder::new()
        .name("emx-editor".to_string())
        .spawn(move || {
            let outcome = run_editor(config, writer, recovered, editor_inputs, receiver);
            if outcome.is_err() {
                // The client would otherwise wait for frames forever; the
                // error itself is reported once the terminal is restored.
                drop(editor_failed.send(UiEvent::ServerClosed {
                    generation: 0,
                    error: None,
                }));
            }
            outcome
        })
        .context("starting the editor thread")?;
    let deliver_to = sender.clone();
    let link = LocalLink::new(
        inputs.clone(),
        Box::new(move |message| {
            let event = match message {
                Some(message) => UiEvent::Server {
                    generation: 0,
                    message,
                },
                None => UiEvent::ServerClosed {
                    generation: 0,
                    error: None,
                },
            };
            // The client is gone only when it has ended.
            drop(deliver_to.send(event));
        }),
    )
    .context("connecting to the editor")?;
    Ok((
        LocalClient { link },
        EditorThread {
            inputs,
            thread,
            journals,
        },
    ))
}

fn run_editor(
    config: Config,
    writer: JournalWriter,
    recovered: Vec<RecoveredBuffer>,
    inputs: Sender<Input>,
    receiver: Receiver<Input>,
) -> anyhow::Result<()> {
    editor_failure_seam()?;
    let mut server = Server::new(
        config,
        Session::NoServer,
        Box::new(writer),
        inputs,
        receiver,
    )?;
    server.handle(Event::Recovered(recovered));
    server.run()
}

/// Debug builds fail the editor thread at startup when this is set, so
/// tests can check that the client does not wait for it forever.
#[cfg(debug_assertions)]
pub const EDITOR_FAILURE_ENV: &str = "EMX_TEST_EDITOR_FAILURE";

#[cfg(debug_assertions)]
fn editor_failure_seam() -> anyhow::Result<()> {
    if std::env::var_os(EDITOR_FAILURE_ENV).is_some() {
        bail!("{EDITOR_FAILURE_ENV} is set: the editor thread fails");
    }
    Ok(())
}

#[cfg(not(debug_assertions))]
fn editor_failure_seam() -> anyhow::Result<()> {
    Ok(())
}

struct LocalClient {
    link: LocalLink,
}

impl Link for LocalClient {
    fn send(&mut self, message: ClientMsg) -> io::Result<()> {
        self.link.send(message)
    }

    fn generation(&self) -> u64 {
        0
    }

    fn reconnect(&mut self) -> Option<anyhow::Result<()>> {
        None
    }

    fn terminate(&mut self) -> bool {
        self.link.terminate().is_ok()
    }
}

/// This process's journal directory, locked while it runs.
struct JournalDir {
    path: PathBuf,
    lock: File,
}

impl JournalDir {
    /// Creates (or takes over, after a crashed process with the same pid)
    /// `noserver-<pid>/` and locks it. A process recovering crashed
    /// sessions may remove the directory between our creating and locking
    /// it, so the lock is trusted only while its file is still the one in
    /// the directory.
    fn claim(state: &Path, pid: u32) -> anyhow::Result<JournalDir> {
        create_private_dir(state)?;
        let path = paths::journal_dir(JournalMode::NoServer { pid })?;
        for _ in 0..CLAIM_ATTEMPTS {
            match DirBuilder::new().mode(PRIVATE_DIRECTORY_MODE).create(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("creating {}", path.display()));
                }
            }
            let lock_path = path.join(LOCK_NAME);
            let lock = match open_lock(&lock_path, true) {
                Ok(lock) => lock,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| format!("opening {}", lock_path.display()));
                }
            };
            rustix::fs::flock(&lock, FlockOperation::LockExclusive)
                .with_context(|| format!("locking {}", lock_path.display()))?;
            if still_linked(&lock, &lock_path)? {
                return Ok(JournalDir { path, lock });
            }
        }
        bail!(
            "could not lock the journal directory {}: it kept being removed",
            path.display()
        )
    }

    /// Moves the journals of every crashed `--no-server` session (its
    /// directory's lock is free) into this one, so replaying this
    /// directory recovers them, then removes the emptied directories. A
    /// directory still locked belongs to a running process and is left
    /// alone.
    fn recover_orphans(&self, state: &Path) -> anyhow::Result<()> {
        let entries =
            fs::read_dir(state).with_context(|| format!("reading {}", state.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", state.display()))?;
            let orphan = entry.path();
            let is_session = entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(NO_SERVER_JOURNALS_PREFIX));
            if !is_session || orphan == self.path {
                continue;
            }
            let real_directory = fs::symlink_metadata(&orphan).is_ok_and(|meta| meta.is_dir());
            if real_directory {
                self.adopt(&orphan)?;
            }
        }
        Ok(())
    }

    fn adopt(&self, orphan: &Path) -> anyhow::Result<()> {
        let lock_path = orphan.join(LOCK_NAME);
        let lock = match open_lock(&lock_path, false) {
            Ok(lock) => lock,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // Created by a process that died before locking it (it
                // holds no journals then), or being created right now: an
                // empty directory is removed, its creator retries.
                drop(fs::remove_dir(orphan));
                return Ok(());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("opening {}", lock_path.display()));
            }
        };
        match rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(rustix::io::Errno::WOULDBLOCK) => return Ok(()),
            Err(error) => {
                return Err(io::Error::from(error))
                    .with_context(|| format!("locking {}", lock_path.display()));
            }
        }
        if !still_linked(&lock, &lock_path)? {
            return Ok(());
        }
        let mut others = false;
        let entries =
            fs::read_dir(orphan).with_context(|| format!("reading {}", orphan.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", orphan.display()))?;
            let name = entry.file_name();
            let from = entry.path();
            if Path::new(&name).extension() == Some(OsStr::new(JOURNAL_EXTENSION)) {
                let to = self.path.join(&name);
                rustix::fs::renameat_with(CWD, &from, CWD, &to, RenameFlags::NOREPLACE)
                    .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
            } else if name
                .as_encoded_bytes()
                .ends_with(REPLACEMENT_SUFFIX.as_bytes())
            {
                fs::remove_file(&from).with_context(|| format!("removing {}", from.display()))?;
            } else if name != LOCK_NAME {
                others = true;
            }
        }
        // The moves are durable before the source can disappear.
        sync_directory(&self.path)?;
        sync_directory(orphan)?;
        if !others {
            removed(fs::remove_file(&lock_path), &lock_path)?;
            removed(fs::remove_dir(orphan), orphan)?;
        }
        Ok(())
    }

    /// At exit: a directory with no journals left is removed. One that
    /// keeps journals (text the user chose not to drop, or a journal that
    /// could not be read) keeps its lock file, so the next `--no-server`
    /// recovers it.
    fn release(self) -> anyhow::Result<()> {
        let entries =
            fs::read_dir(&self.path).with_context(|| format!("reading {}", self.path.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", self.path.display()))?;
            if entry.file_name() != LOCK_NAME {
                return Ok(());
            }
        }
        let lock_path = self.path.join(LOCK_NAME);
        removed(fs::remove_file(&lock_path), &lock_path)?;
        removed(fs::remove_dir(&self.path), &self.path)?;
        drop(self.lock);
        Ok(())
    }
}

fn open_lock(path: &Path, create: bool) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .mode(PRIVATE_FILE_MODE)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
}

/// Whether the locked `file` is still the file at `path`: a lock on a
/// file someone removed protects nothing.
fn still_linked(file: &File, path: &Path) -> anyhow::Result<bool> {
    let held = file
        .metadata()
        .with_context(|| format!("inspecting {}", path.display()))?;
    match fs::symlink_metadata(path) {
        Ok(current) => Ok(current.dev() == held.dev() && current.ino() == held.ino()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

fn sync_directory(path: &Path) -> anyhow::Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("syncing {}", path.display()))
}
