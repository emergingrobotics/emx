//! Where external changes come from (DESIGN section 3): a watcher thread
//! with one inotify watch (`notify`) on the directory of each visited
//! file, shared by every file in it, and a poller thread for files on
//! network and FUSE filesystems, where inotify misses changes, and for
//! files whose directory could not be watched. Both only report that a
//! file may have changed (`Event::FileChanged`, `Event::FileRenamed`); the
//! editor looks at it and waits for it to settle. Nothing here runs on the
//! editor thread.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use emx_core::reload::{self, Observation};
use emx_core::{Config, Event};
use emx_text::FileIdentity;
use notify::event::{AccessKind, AccessMode, ModifyKind, RenameMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use super::workers::Deliver;

/// Changes to watched files are passed on at most this often: a writer
/// making thousands of small writes a second would otherwise have the
/// editor handle (and redisplay after) each one. Well under the settle
/// window, so it adds no visible delay.
pub const WATCH_COALESCE_MS: u64 = 20;

/// `statfs` `f_type` of the filesystems whose changes inotify does not see
/// (made on another machine, or by a FUSE daemon): NFS, SMB, CIFS, SMB2,
/// FUSE (sshfs and the like) and 9P.
pub const NFS_SUPER_MAGIC: u32 = 0x6969;
pub const SMB_SUPER_MAGIC: u32 = 0x517B;
pub const CIFS_SUPER_MAGIC: u32 = 0xFF53_4D42;
pub const SMB2_SUPER_MAGIC: u32 = 0xFE53_4D42;
pub const FUSE_SUPER_MAGIC: u32 = 0x6573_5546;
pub const V9FS_MAGIC: u32 = 0x0102_1997;
const NETWORK_MAGICS: [u32; 6] = [
    NFS_SUPER_MAGIC,
    SMB_SUPER_MAGIC,
    CIFS_SUPER_MAGIC,
    SMB2_SUPER_MAGIC,
    FUSE_SUPER_MAGIC,
    V9FS_MAGIC,
];

/// Set to a directory in the environment, the files under it are treated
/// as on a network filesystem: the seam the polling tests use, as no test
/// machine can be relied on to have one. Debug builds only.
#[cfg(debug_assertions)]
pub const NETWORK_FS_ENV: &str = "EMX_TEST_NETWORK_FS";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    /// inotify sees every change.
    Local,
    /// Changes are polled for.
    Network,
}

impl FsKind {
    pub fn of_magic(magic: u32) -> FsKind {
        if NETWORK_MAGICS.contains(&magic) {
            FsKind::Network
        } else {
            FsKind::Local
        }
    }
}

/// The kind of filesystem `directory` is on. One that cannot be looked at
/// is taken as local: watching it then fails and it is polled.
pub fn fs_kind(directory: &Path) -> FsKind {
    match rustix::fs::statfs(directory) {
        // `f_type` is a signed word on some targets; the magic numbers are
        // its low 32 bits.
        Ok(stat) => FsKind::of_magic(stat.f_type as u32),
        Err(_) => FsKind::Local,
    }
}

#[cfg(debug_assertions)]
fn forced_network() -> Option<PathBuf> {
    std::env::var_os(NETWORK_FS_ENV).map(PathBuf::from)
}

/// Release builds carry no test seam.
#[cfg(not(debug_assertions))]
fn forced_network() -> Option<PathBuf> {
    None
}

enum Command {
    Watch(PathBuf),
    Unwatch(PathBuf),
    Notify(notify::Result<notify::Event>),
    Stop,
}

/// The watcher and poller threads; stopped when dropped.
pub struct Watcher {
    commands: Sender<Command>,
    poller: Arc<Poller>,
}

impl Watcher {
    pub fn spawn(config: &Config, deliver: Deliver) -> std::io::Result<Watcher> {
        let (commands, receiver) = mpsc::channel();
        let poller = Arc::new(Poller::new(
            Duration::from_millis(config.poll_interval_ms),
            config.append_tail_hash_bytes,
        ));
        let notify_commands = commands.clone();
        let inotify = notify::recommended_watcher(move |result| {
            // The watcher thread is gone only when the server is exiting.
            drop(notify_commands.send(Command::Notify(result)));
        });
        let state = WatchState::new(
            inotify,
            poller.clone(),
            deliver.clone(),
            Duration::from_millis(config.poll_interval_ms),
        );
        thread::Builder::new()
            .name("emx-watch".to_string())
            .spawn(move || watch_loop(state, &receiver))?;
        let polling = poller.clone();
        thread::Builder::new()
            .name("emx-poll".to_string())
            .spawn(move || polling.run(&deliver))?;
        Ok(Watcher { commands, poller })
    }

    pub fn watch(&self, path: PathBuf) {
        // The watcher thread is gone only when the server is exiting.
        drop(self.commands.send(Command::Watch(path)));
    }

    pub fn unwatch(&self, path: PathBuf) {
        drop(self.commands.send(Command::Unwatch(path)));
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        drop(self.commands.send(Command::Stop));
        self.poller.stop();
    }
}

/// How the files of one directory are watched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Inotify,
    Polled,
}

/// One watched directory: the visited files in it, which keep it watched.
struct Directory {
    files: BTreeSet<OsString>,
    mode: Mode,
    /// Polled only until inotify can watch it (again): it did not exist,
    /// or was deleted or moved away, or watching it failed.
    retry: bool,
}

struct WatchState {
    /// `None` when creating it failed: every file is polled.
    inotify: Option<RecommendedWatcher>,
    /// Why creating it failed, until that is told.
    inotify_error: Option<String>,
    directories: HashMap<PathBuf, Directory>,
    poller: Arc<Poller>,
    deliver: Deliver,
    forced_network: Option<PathBuf>,
    /// Files changed since the last report, reported together at
    /// `report_at`.
    pending: BTreeSet<PathBuf>,
    report_at: Option<Instant>,
    /// When directories polled for want of a watch are tried again.
    retry_at: Option<Instant>,
    retry_interval: Duration,
}

fn watch_loop(mut state: WatchState, commands: &Receiver<Command>) {
    loop {
        let deadline = match (state.report_at, state.retry_at) {
            (Some(report), Some(retry)) => Some(report.min(retry)),
            (report, retry) => report.or(retry),
        };
        let command = match deadline {
            Some(deadline) => {
                match commands.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(command) => command,
                    Err(RecvTimeoutError::Timeout) => {
                        state.timers_due(Instant::now());
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            None => match commands.recv() {
                Ok(command) => command,
                Err(_) => return,
            },
        };
        match command {
            Command::Watch(path) => state.watch(path),
            Command::Unwatch(path) => state.unwatch(&path),
            Command::Notify(Ok(event)) => state.notified(&event),
            Command::Notify(Err(error)) => {
                // Events may have been lost: every watched file is looked
                // at again.
                eprintln!("emx server: file watcher: {error}");
                state.changed_all();
            }
            Command::Stop => return,
        }
    }
}

/// A watch error for the echo area, without the paths it names: the
/// message names the directory already.
fn describe(error: &notify::Error) -> String {
    match &error.kind {
        notify::ErrorKind::Io(error) => emx_core::io::strerror(error),
        notify::ErrorKind::PathNotFound => "No such file or directory".to_string(),
        notify::ErrorKind::MaxFilesWatch => {
            "the limit on inotify watches is reached; see fs.inotify.max_user_watches".to_string()
        }
        notify::ErrorKind::Generic(message) => message.clone(),
        notify::ErrorKind::WatchNotFound | notify::ErrorKind::InvalidConfig(_) => error.to_string(),
    }
}

/// A visited path split into the directory watched and the name in it.
fn split(path: &Path) -> Option<(PathBuf, OsString)> {
    Some((
        path.parent()?.to_path_buf(),
        path.file_name()?.to_os_string(),
    ))
}

impl WatchState {
    fn new(
        inotify: notify::Result<RecommendedWatcher>,
        poller: Arc<Poller>,
        deliver: Deliver,
        retry_interval: Duration,
    ) -> WatchState {
        let (inotify, inotify_error) = match inotify {
            Ok(watcher) => (Some(watcher), None),
            Err(error) => (None, Some(describe(&error))),
        };
        WatchState {
            inotify,
            inotify_error,
            directories: HashMap::new(),
            poller,
            deliver,
            forced_network: forced_network(),
            pending: BTreeSet::new(),
            report_at: None,
            retry_at: None,
            retry_interval,
        }
    }

    fn kind(&self, directory: &Path) -> FsKind {
        if self
            .forced_network
            .as_ref()
            .is_some_and(|forced| directory.starts_with(forced))
        {
            return FsKind::Network;
        }
        fs_kind(directory)
    }

    fn watch(&mut self, path: PathBuf) {
        let Some((directory, name)) = split(&path) else {
            eprintln!("emx server: cannot watch {}: no directory", path.display());
            return;
        };
        let (mode, notice) = match self.directories.get_mut(&directory) {
            Some(watched) => {
                watched.files.insert(name);
                (watched.mode, None)
            }
            None => {
                let (mode, notice, retry) = self.start_watching(&directory);
                self.directories.insert(
                    directory,
                    Directory {
                        files: BTreeSet::from([name]),
                        mode,
                        retry,
                    },
                );
                if retry {
                    self.schedule_retry();
                }
                (mode, notice)
            }
        };
        match mode {
            Mode::Polled => {
                self.poller.add(path.clone());
                (self.deliver)(Event::FilePolled { path, notice });
            }
            // Whatever changed between the file being read and the watch
            // being in place is looked for now.
            Mode::Inotify => (self.deliver)(Event::FileChanged { path }),
        }
    }

    /// Watches `directory` with inotify, or says why it is polled instead
    /// and whether to try again later.
    fn start_watching(&mut self, directory: &Path) -> (Mode, Option<String>, bool) {
        if self.kind(directory) == FsKind::Network {
            return (Mode::Polled, None, false);
        }
        let Some(watcher) = &mut self.inotify else {
            // Told once, when the first file is polled for it.
            let notice = self.inotify_error.take().map(|reason| {
                format!("Polling visited files: cannot watch for changes ({reason})")
            });
            return (Mode::Polled, notice, false);
        };
        match watcher.watch(directory, RecursiveMode::NonRecursive) {
            Ok(()) => (Mode::Inotify, None, false),
            Err(error) => (
                Mode::Polled,
                Some(format!(
                    "Polling files in {}: cannot watch it ({})",
                    directory.display(),
                    describe(&error)
                )),
                true,
            ),
        }
    }

    fn schedule_retry(&mut self) {
        self.retry_at
            .get_or_insert_with(|| Instant::now() + self.retry_interval);
    }

    fn timers_due(&mut self, now: Instant) {
        if self.report_at.is_some_and(|at| at <= now) {
            self.report();
        }
        if self.retry_at.is_some_and(|at| at <= now) {
            self.retry_at = None;
            self.retry_watches(None);
        }
    }

    /// The watched `directory` was deleted or moved away: inotify has
    /// dropped its watch, or would go on reporting the moved directory
    /// under the old name. Its files are polled (a missing file is seen
    /// as deleted) until the directory can be watched again.
    fn lose(&mut self, directory: &Path) {
        let Some(watched) = self.directories.get_mut(directory) else {
            return;
        };
        if watched.mode != Mode::Inotify {
            return;
        }
        watched.mode = Mode::Polled;
        watched.retry = true;
        let paths: Vec<PathBuf> = watched
            .files
            .iter()
            .map(|name| directory.join(name))
            .collect();
        // A deleted directory's watch is gone already; this ends a moved
        // one's.
        if let Some(watcher) = &mut self.inotify {
            drop(watcher.unwatch(directory));
        }
        let mut notice = Some(format!(
            "Polling files in {}: it was deleted or moved",
            directory.display()
        ));
        for path in paths {
            self.pending.remove(&path);
            self.poller.add(path.clone());
            (self.deliver)(Event::FilePolled {
                path,
                notice: notice.take(),
            });
        }
        self.schedule_retry();
    }

    /// Tries inotify again on the directories waiting for it (only
    /// `only`, when given); those it watches now stop being polled.
    fn retry_watches(&mut self, only: Option<&Path>) {
        let waiting: Vec<PathBuf> = self
            .directories
            .iter()
            .filter(|(directory, watched)| {
                watched.retry && only.is_none_or(|only| only == directory.as_path())
            })
            .map(|(directory, _)| directory.clone())
            .collect();
        for directory in waiting {
            let Some(watcher) = &mut self.inotify else {
                return;
            };
            if watcher
                .watch(&directory, RecursiveMode::NonRecursive)
                .is_err()
            {
                continue;
            }
            let watched = self.directories.get_mut(&directory).expect("listed above");
            watched.mode = Mode::Inotify;
            watched.retry = false;
            let paths: Vec<PathBuf> = watched
                .files
                .iter()
                .map(|name| directory.join(name))
                .collect();
            for path in paths {
                self.poller.remove(&path);
                (self.deliver)(Event::FileWatched { path: path.clone() });
                // What changed while it was polled is looked for now.
                self.changed(&path);
            }
        }
        if self.directories.values().any(|watched| watched.retry) {
            self.schedule_retry();
        }
    }

    fn unwatch(&mut self, path: &Path) {
        let Some((directory, name)) = split(path) else {
            return;
        };
        let Some(watched) = self.directories.get_mut(&directory) else {
            return;
        };
        watched.files.remove(&name);
        if watched.mode == Mode::Polled {
            self.poller.remove(path);
        }
        if !watched.files.is_empty() {
            return;
        }
        let mode = watched.mode;
        self.directories.remove(&directory);
        if mode == Mode::Inotify
            && let Some(watcher) = &mut self.inotify
            && let Err(error) = watcher.unwatch(&directory)
        {
            // The directory may be gone, which ended its watch already.
            eprintln!("emx server: unwatching {}: {error}", directory.display());
        }
    }

    /// Whether `path` is a visited file whose directory inotify watches.
    fn watched(&self, path: &Path) -> bool {
        split(path).is_some_and(|(directory, name)| {
            self.directories.get(&directory).is_some_and(|watched| {
                watched.mode == Mode::Inotify && watched.files.contains(&name)
            })
        })
    }

    fn changed(&mut self, path: &Path) {
        if self.watched(path) {
            self.pending.insert(path.to_path_buf());
            self.report_at
                .get_or_insert_with(|| Instant::now() + Duration::from_millis(WATCH_COALESCE_MS));
        }
    }

    fn changed_all(&mut self) {
        let paths: Vec<PathBuf> = self
            .directories
            .iter()
            .filter(|(_, watched)| watched.mode == Mode::Inotify)
            .flat_map(|(directory, watched)| watched.files.iter().map(|name| directory.join(name)))
            .collect();
        for path in paths {
            self.changed(&path);
        }
    }

    /// Passes on the changes collected since the last report.
    fn report(&mut self) {
        self.report_at = None;
        for path in std::mem::take(&mut self.pending) {
            (self.deliver)(Event::FileChanged { path });
        }
    }

    fn notified(&mut self, event: &notify::Event) {
        if event.need_rescan() {
            self.changed_all();
            return;
        }
        // An event on a watched directory itself (its own watch's
        // delete-self or move-self, or its parent's watch) means it is
        // gone or replaced; a directory waiting to be watched that
        // appears is watched at once.
        match event.kind {
            EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_)) => {
                for path in &event.paths {
                    self.lose(path);
                }
            }
            _ => {}
        }
        if matches!(
            event.kind,
            EventKind::Create(_)
                | EventKind::Modify(ModifyKind::Name(RenameMode::To | RenameMode::Both))
        ) {
            for path in &event.paths {
                if self
                    .directories
                    .get(path)
                    .is_some_and(|watched| watched.retry)
                {
                    self.retry_watches(Some(path));
                }
            }
        }
        match event.kind {
            // Reading a file (as the editor itself does to look at it)
            // changes nothing.
            EventKind::Access(AccessKind::Close(AccessMode::Write)) => {}
            EventKind::Access(_) => return,
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
                if let [from, to] = event.paths.as_slice() {
                    self.renamed(from, to);
                    return;
                }
            }
            _ => {}
        }
        for path in &event.paths {
            self.changed(path);
        }
    }

    /// `from` was renamed to `to`. A visited `from` that is still gone
    /// (nothing was put in its place yet) moved; one already replaced is
    /// only changed.
    fn renamed(&mut self, from: &Path, to: &Path) {
        if self.watched(from) {
            // Changes seen before the rename are reported before it.
            self.report();
            let replaced = std::fs::symlink_metadata(from).is_ok();
            if replaced || from == to {
                self.changed(from);
            } else {
                (self.deliver)(Event::FileRenamed {
                    from: from.to_path_buf(),
                    to: to.to_path_buf(),
                });
            }
        }
        self.changed(to);
    }
}

/// What the poller last saw of a file: its identity and the hash before
/// its end, which also catches a rewrite within a coarse network mtime;
/// or why it could not be looked at.
type Fingerprint = Result<(Option<FileIdentity>, Option<u64>), String>;

fn fingerprint(observation: Result<Observation, String>) -> Fingerprint {
    observation.map(|observation| (observation.identity, observation.end_hash))
}

#[derive(Default)]
struct PollState {
    /// Polled files and what was seen of each; `None` until first looked
    /// at.
    files: HashMap<PathBuf, Option<Fingerprint>>,
    stopped: bool,
}

/// Looks at every polled file every `interval`, on a thread of its own:
/// a hung network filesystem holds up only the polling.
struct Poller {
    state: Mutex<PollState>,
    wake: Condvar,
    interval: Duration,
    tail_bytes: u64,
}

impl Poller {
    fn new(interval: Duration, tail_bytes: u64) -> Poller {
        Poller {
            state: Mutex::new(PollState::default()),
            wake: Condvar::new(),
            interval,
            tail_bytes,
        }
    }

    fn add(&self, path: PathBuf) {
        self.state
            .lock()
            .expect("poll lock")
            .files
            .insert(path, None);
        self.wake.notify_all();
    }

    fn remove(&self, path: &Path) {
        self.state.lock().expect("poll lock").files.remove(path);
    }

    fn stop(&self) {
        self.state.lock().expect("poll lock").stopped = true;
        self.wake.notify_all();
    }

    fn run(&self, deliver: &Deliver) {
        let mut next_round = Instant::now() + self.interval;
        loop {
            let due: Vec<(PathBuf, bool)> = {
                let mut state = self.state.lock().expect("poll lock");
                loop {
                    if state.stopped {
                        return;
                    }
                    let round = Instant::now() >= next_round;
                    let due: Vec<(PathBuf, bool)> = state
                        .files
                        .iter()
                        .filter(|(_, seen)| round || seen.is_none())
                        .map(|(path, seen)| (path.clone(), seen.is_none()))
                        .collect();
                    if round {
                        next_round = Instant::now() + self.interval;
                    }
                    if !due.is_empty() {
                        break due;
                    }
                    let wait = next_round.saturating_duration_since(Instant::now());
                    state = self.wake.wait_timeout(state, wait).expect("poll lock").0;
                }
            };
            for (path, first) in due {
                let seen = fingerprint(reload::observe(&path, None, self.tail_bytes));
                let mut state = self.state.lock().expect("poll lock");
                // Let go of while it was looked at.
                let Some(previous) = state.files.get_mut(&path) else {
                    continue;
                };
                let changed = previous.as_ref() != Some(&seen);
                *previous = Some(seen);
                drop(state);
                // The first look catches up on what changed since the file
                // was read; the editor finds nothing if nothing did.
                if first || changed {
                    deliver(Event::FileChanged { path });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_magics_are_polled() {
        for magic in NETWORK_MAGICS {
            assert_eq!(FsKind::of_magic(magic), FsKind::Network, "{magic:#x}");
        }
        const EXT4_SUPER_MAGIC: u32 = 0xEF53;
        const TMPFS_MAGIC: u32 = 0x0102_1994;
        assert_eq!(FsKind::of_magic(EXT4_SUPER_MAGIC), FsKind::Local);
        assert_eq!(FsKind::of_magic(TMPFS_MAGIC), FsKind::Local);
    }

    /// Long enough that no retry or poll happens on its own in a test.
    const NEVER: Duration = Duration::from_secs(3600);
    const TAIL_BYTES: u64 = 64;
    const EVENT_WAIT: Duration = Duration::from_secs(10);

    struct Fixture {
        state: WatchState,
        notifications: Receiver<Command>,
        delivered: Arc<Mutex<Vec<Event>>>,
    }

    impl Fixture {
        fn new() -> Fixture {
            let (commands, notifications) = mpsc::channel();
            let inotify = notify::recommended_watcher(move |result| {
                drop(commands.send(Command::Notify(result)));
            });
            let delivered = Arc::new(Mutex::new(Vec::new()));
            let sink = delivered.clone();
            let deliver: Deliver = Arc::new(move |event| sink.lock().expect("lock").push(event));
            let poller = Arc::new(Poller::new(NEVER, TAIL_BYTES));
            Fixture {
                state: WatchState::new(inotify, poller, deliver, NEVER),
                notifications,
                delivered,
            }
        }

        fn mode(&self, directory: &Path) -> Option<Mode> {
            self.state
                .directories
                .get(directory)
                .map(|watched| watched.mode)
        }

        /// Feeds notifications to the state until `done` holds.
        fn feed_until(&mut self, done: impl Fn(&WatchState) -> bool) {
            let deadline = Instant::now() + EVENT_WAIT;
            while !done(&self.state) {
                let left = deadline.saturating_duration_since(Instant::now());
                match self.notifications.recv_timeout(left) {
                    Ok(Command::Notify(Ok(event))) => self.state.notified(&event),
                    Ok(_) => {}
                    Err(error) => panic!("no notification came: {error}"),
                }
            }
        }

        fn delivered(&self) -> Vec<Event> {
            std::mem::take(&mut *self.delivered.lock().expect("lock"))
        }
    }

    #[test]
    fn one_watch_per_directory_until_its_last_file_goes() {
        let directory = tempfile::tempdir().expect("tempdir");
        let a = directory.path().join("a.txt");
        let b = directory.path().join("b.txt");
        std::fs::write(&a, "a").expect("write");
        std::fs::write(&b, "b").expect("write");
        let mut fixture = Fixture::new();
        fixture.state.watch(a.clone());
        fixture.state.watch(b.clone());
        assert_eq!(fixture.state.directories.len(), 1);
        assert_eq!(fixture.mode(directory.path()), Some(Mode::Inotify));
        fixture.state.unwatch(&a);
        assert_eq!(fixture.mode(directory.path()), Some(Mode::Inotify));
        // The watch is still in place for b.txt.
        std::fs::write(&b, "b changed").expect("write");
        fixture.feed_until(|state| state.pending.contains(&b));
        fixture.state.unwatch(&b);
        assert!(fixture.state.directories.is_empty());
        let inotify = fixture.state.inotify.as_mut().expect("inotify");
        assert!(
            inotify.unwatch(directory.path()).is_err(),
            "the directory's watch was removed with its last file"
        );
    }

    #[test]
    fn deleted_directory_is_polled_then_watched_again() {
        let root = tempfile::tempdir().expect("tempdir");
        let directory = root.path().join("gen");
        std::fs::create_dir(&directory).expect("mkdir");
        let path = directory.join("out.txt");
        std::fs::write(&path, "1").expect("write");
        let mut fixture = Fixture::new();
        fixture.state.watch(path.clone());
        assert_eq!(fixture.mode(&directory), Some(Mode::Inotify));
        fixture.delivered();
        std::fs::remove_dir_all(&directory).expect("rm -rf");
        fixture.feed_until(|state| {
            state
                .directories
                .get(&directory)
                .is_some_and(|watched| watched.mode == Mode::Polled && watched.retry)
        });
        let polled: Vec<Event> = fixture
            .delivered()
            .into_iter()
            .filter(|event| matches!(event, Event::FilePolled { .. }))
            .collect();
        assert_eq!(
            polled,
            vec![Event::FilePolled {
                path: path.clone(),
                notice: Some(format!(
                    "Polling files in {}: it was deleted or moved",
                    directory.display()
                )),
            }]
        );
        assert!(fixture.state.retry_at.is_some());
        // Still missing: it stays polled.
        fixture.state.retry_watches(None);
        assert_eq!(fixture.mode(&directory), Some(Mode::Polled));
        std::fs::create_dir(&directory).expect("mkdir again");
        fixture.state.retry_watches(None);
        assert_eq!(fixture.mode(&directory), Some(Mode::Inotify));
        assert!(
            fixture
                .delivered()
                .contains(&Event::FileWatched { path: path.clone() })
        );
        std::fs::write(&path, "2").expect("regenerate");
        fixture.feed_until(|state| state.pending.contains(&path));
    }

    #[test]
    fn local_temp_dir_is_watched() {
        let directory = tempfile::tempdir().expect("tempdir");
        assert_eq!(fs_kind(directory.path()), FsKind::Local);
    }
}
