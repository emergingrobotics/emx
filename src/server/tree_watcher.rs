//! Watches the directories the file panels show (DESIGN section 3: "the
//! directory panel also updates live"), with an inotify instance of its
//! own so the visited-file watcher's bookkeeping stays separate. Changes to
//! a directory's entries are coalesced and reported as
//! `Event::TreeChanged`. Directories on network filesystems are not
//! watched; the panel lists them again when asked (`g`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use emx_core::Event;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use super::watcher::{FsKind, fs_kind};
use super::workers::Deliver;

/// Changes to a directory are reported at most this often: a build
/// writing thousands of files would otherwise have every panel listed
/// again for each.
pub const TREE_COALESCE_MS: u64 = 100;

enum Command {
    Watch(PathBuf),
    Unwatch(PathBuf),
    Notify(notify::Result<notify::Event>),
    Stop,
}

/// The tree watcher thread; stopped when dropped.
pub struct TreeWatcher {
    commands: Sender<Command>,
}

impl TreeWatcher {
    pub fn spawn(deliver: Deliver) -> std::io::Result<TreeWatcher> {
        let (commands, receiver) = mpsc::channel();
        let notify_commands = commands.clone();
        // Without inotify the panels are only listed again on request.
        let inotify = notify::recommended_watcher(move |result| {
            drop(notify_commands.send(Command::Notify(result)));
        })
        .ok();
        thread::Builder::new()
            .name("emx-tree-watch".to_string())
            .spawn(move || tree_watch_loop(inotify, &receiver, &deliver))?;
        Ok(TreeWatcher { commands })
    }

    pub fn watch(&self, dir: PathBuf) {
        // The thread is gone only when the server is exiting.
        drop(self.commands.send(Command::Watch(dir)));
    }

    pub fn unwatch(&self, dir: PathBuf) {
        drop(self.commands.send(Command::Unwatch(dir)));
    }
}

impl Drop for TreeWatcher {
    fn drop(&mut self) {
        drop(self.commands.send(Command::Stop));
    }
}

fn tree_watch_loop(
    mut inotify: Option<RecommendedWatcher>,
    commands: &Receiver<Command>,
    deliver: &Deliver,
) {
    // How many panels watch each directory.
    let mut watched: BTreeMap<PathBuf, usize> = BTreeMap::new();
    let mut pending: BTreeSet<PathBuf> = BTreeSet::new();
    let mut report_at: Option<Instant> = None;
    loop {
        let command = match report_at {
            Some(at) => match commands.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(command) => command,
                Err(RecvTimeoutError::Timeout) => {
                    for dir in std::mem::take(&mut pending) {
                        deliver(Event::TreeChanged { dir });
                    }
                    report_at = None;
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            },
            None => match commands.recv() {
                Ok(command) => command,
                Err(_) => return,
            },
        };
        match command {
            Command::Watch(dir) => {
                let count = watched.entry(dir.clone()).or_insert(0);
                *count += 1;
                if *count == 1
                    && fs_kind(&dir) == FsKind::Local
                    && let Some(watcher) = &mut inotify
                    && let Err(error) = watcher.watch(&dir, RecursiveMode::NonRecursive)
                {
                    eprintln!("emx server: cannot watch {}: {error}", dir.display());
                }
            }
            Command::Unwatch(dir) => {
                let Some(count) = watched.get_mut(&dir) else {
                    continue;
                };
                *count -= 1;
                if *count == 0 {
                    watched.remove(&dir);
                    if let Some(watcher) = &mut inotify {
                        drop(watcher.unwatch(&dir));
                    }
                }
            }
            Command::Notify(Ok(event)) => {
                if matches!(event.kind, EventKind::Access(_)) {
                    continue;
                }
                for path in &event.paths {
                    for dir in [Some(path.as_path()), path.parent()].into_iter().flatten() {
                        if watched.contains_key(dir) {
                            pending.insert(dir.to_path_buf());
                        }
                    }
                }
                if !pending.is_empty() && report_at.is_none() {
                    report_at = Some(Instant::now() + Duration::from_millis(TREE_COALESCE_MS));
                }
            }
            Command::Notify(Err(error)) => {
                eprintln!("emx server: tree watcher: {error}");
                pending.extend(watched.keys().cloned());
                report_at.get_or_insert_with(|| {
                    Instant::now() + Duration::from_millis(TREE_COALESCE_MS)
                });
            }
            Command::Stop => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use super::*;

    fn is_local(dir: &Path) -> bool {
        fs_kind(dir) == FsKind::Local
    }

    #[test]
    fn changes_to_a_watched_directory_are_reported_once_coalesced() {
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().to_path_buf();
        assert!(is_local(&dir));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let watcher = TreeWatcher::spawn(Arc::new(move |event| {
            recorder.lock().unwrap().push(event);
        }))
        .unwrap();
        watcher.watch(dir.clone());
        thread::sleep(Duration::from_millis(100));
        for name in ["a", "b", "c"] {
            fs::write(dir.join(name), "").unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while seen.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "no change reported");
            thread::sleep(Duration::from_millis(20));
        }
        thread::sleep(Duration::from_millis(3 * TREE_COALESCE_MS));
        assert_eq!(
            *seen.lock().unwrap(),
            vec![Event::TreeChanged { dir: dir.clone() }]
        );
        watcher.unwatch(dir.clone());
        thread::sleep(Duration::from_millis(100));
        fs::write(dir.join("d"), "").unwrap();
        thread::sleep(Duration::from_millis(3 * TREE_COALESCE_MS));
        assert_eq!(seen.lock().unwrap().len(), 1, "no longer watched");
    }
}
