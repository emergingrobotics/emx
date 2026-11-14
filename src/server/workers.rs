//! Work the editor thread hands off (DESIGN section 8: never block the UI
//! thread): file reads and saves, auto-saves and cache prefetches on a pool
//! of `IO_THREADS` threads, one line indexer thread per file, and
//! directory listings for completion on `LISTING_THREADS` threads of their
//! own.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use emx_core::diff::{self, DiffJob};
use emx_core::io::{self, SaveFailure, SavePlan};
use emx_core::{
    AutoSaveJob, BufferId, ClientId, Config, Event, ListingId, PrefetchRequest, ReloadJob,
};
use emx_core::{autosave, reload};
use emx_text::{BlockCache, FileIdentity, LineIndex, Original, Sources, index_file};

use super::journal_writer::JournalProgress;

/// DESIGN section 8 sizes the I/O pool at four threads.
pub const IO_THREADS: usize = 4;
/// Threads that list directories for file-name completion. They are apart
/// from the I/O pool because a directory on a hung network filesystem can
/// block its reader forever: that may stall completion, never a save.
pub const LISTING_THREADS: usize = 4;
/// The indexer reports its progress every this many blocks.
pub const INDEX_PROGRESS_BLOCKS: u64 = 256;

/// Set in the environment, a save job panics on its I/O thread: the seam
/// tests use to check that a worker panic ends the server (`panic =
/// "abort"`) instead of leaving it wedged. Debug builds only.
#[cfg(debug_assertions)]
pub const WORKER_PANIC_ENV: &str = "EMX_TEST_WORKER_PANIC";

/// Where workers deliver the events their results become.
pub type Deliver = Arc<dyn Fn(Event) + Send + Sync>;

pub enum Job {
    Load {
        buffer: BufferId,
        path: PathBuf,
    },
    Save {
        buffer: BufferId,
        plan: SavePlan,
    },
    AutoSave(AutoSaveJob),
    RemoveAutoSave {
        file: PathBuf,
    },
    ReadAutoSave {
        buffer: BufferId,
        file: PathBuf,
    },
    Prefetch {
        request: PrefetchRequest,
        cache: Arc<BlockCache>,
    },
    Observe {
        buffer: BufferId,
        path: PathBuf,
        old_len: Option<u64>,
    },
    Reload(ReloadJob),
    ReadAppend {
        buffer: BufferId,
        path: PathBuf,
        old: FileIdentity,
        new: FileIdentity,
        old_tail_hash: u64,
    },
    Diff(DiffJob),
}

impl Job {
    /// Jobs on the same file are run in the order submitted, one at a
    /// time: an auto-save write and its removal must not race.
    fn key(&self) -> Option<PathBuf> {
        match self {
            Job::AutoSave(job) => Some(job.file.clone()),
            Job::RemoveAutoSave { file } | Job::ReadAutoSave { file, .. } => Some(file.clone()),
            Job::Load { .. }
            | Job::Save { .. }
            | Job::Prefetch { .. }
            | Job::Observe { .. }
            | Job::Reload(_)
            | Job::ReadAppend { .. }
            | Job::Diff(_) => None,
        }
    }
}

struct Queued {
    job: Job,
    key: Option<PathBuf>,
    /// The job starts only once journal operation `after_journal` is
    /// applied.
    after_journal: u64,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Queued>,
    busy: HashSet<PathBuf>,
    closed: bool,
}

pub struct Pool {
    shared: Arc<(Mutex<Queue>, Condvar)>,
    threads: Vec<JoinHandle<()>>,
}

impl Pool {
    pub fn new(config: Arc<Config>, journal: JournalProgress, deliver: Deliver) -> Pool {
        let shared = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
        let panic_on_save = worker_panic_requested();
        let threads = (0..IO_THREADS)
            .map(|index| {
                let shared = shared.clone();
                let config = config.clone();
                let journal = journal.clone();
                let deliver = deliver.clone();
                thread::Builder::new()
                    .name(format!("emx-io-{index}"))
                    .spawn(move || work(&shared, &config, &journal, &deliver, panic_on_save))
                    .expect("spawning an I/O thread")
            })
            .collect();
        Pool { shared, threads }
    }

    pub fn submit(&self, job: Job, after_journal: u64) {
        let (queue, ready) = &*self.shared;
        let key = job.key();
        queue.lock().expect("pool lock").jobs.push_back(Queued {
            job,
            key,
            after_journal,
        });
        ready.notify_all();
    }
}

impl Drop for Pool {
    /// Finishes the queued jobs, then stops the threads.
    fn drop(&mut self) {
        let (queue, ready) = &*self.shared;
        queue.lock().expect("pool lock").closed = true;
        ready.notify_all();
        for thread in self.threads.drain(..) {
            if thread.join().is_err() {
                eprintln!("emx server: an I/O thread panicked");
            }
        }
    }
}

fn work(
    shared: &(Mutex<Queue>, Condvar),
    config: &Config,
    journal: &JournalProgress,
    deliver: &Deliver,
    panic_on_save: bool,
) {
    let (queue, ready) = shared;
    loop {
        let queued = {
            let mut state = queue.lock().expect("pool lock");
            loop {
                let runnable = state.jobs.iter().position(|queued| {
                    queued
                        .key
                        .as_ref()
                        .is_none_or(|key| !state.busy.contains(key))
                });
                if let Some(index) = runnable {
                    let queued = state.jobs.remove(index).expect("index found above");
                    if let Some(key) = &queued.key {
                        state.busy.insert(key.clone());
                    }
                    break queued;
                }
                if state.closed && state.jobs.is_empty() {
                    return;
                }
                state = ready.wait(state).expect("pool lock");
            }
        };
        journal.wait_for(queued.after_journal);
        panic_seam(panic_on_save, &queued.job);
        if let Some(event) = run(queued.job, config) {
            deliver(event);
        }
        if let Some(key) = queued.key {
            queue.lock().expect("pool lock").busy.remove(&key);
            ready.notify_all();
        }
    }
}

#[cfg(debug_assertions)]
fn worker_panic_requested() -> bool {
    std::env::var_os(WORKER_PANIC_ENV).is_some()
}

#[cfg(debug_assertions)]
fn panic_seam(panic_on_save: bool, job: &Job) {
    if panic_on_save && matches!(job, Job::Save { .. }) {
        panic!("{WORKER_PANIC_ENV} is set: an I/O thread panics on a save");
    }
}

#[cfg(not(debug_assertions))]
fn panic_seam(_panic_on_save: bool, _job: &Job) {}

/// Release builds carry no test seam.
#[cfg(not(debug_assertions))]
fn worker_panic_requested() -> bool {
    false
}

fn run(job: Job, config: &Config) -> Option<Event> {
    match job {
        Job::Load { buffer, path } => Some(Event::FileLoaded {
            buffer,
            result: io::load_file(&path, config).map_err(|error| error.to_string()),
        }),
        Job::Save { buffer, plan } => Some(Event::SaveFinished {
            buffer,
            result: io::save_file(&plan, config.append_tail_hash_bytes)
                .map_err(|error| SaveFailure::from(&error)),
        }),
        Job::Observe {
            buffer,
            path,
            old_len,
        } => Some(Event::Observed {
            buffer,
            result: reload::observe(&path, old_len, config.append_tail_hash_bytes),
        }),
        Job::Reload(job) => Some(Event::Reloaded {
            buffer: job.buffer,
            result: reload::reload_file(&job.path, config, &job.previous, &job.anchors),
        }),
        Job::ReadAppend {
            buffer,
            path,
            old,
            new,
            old_tail_hash,
        } => Some(Event::AppendRead {
            buffer,
            result: reload::read_appended(
                &path,
                old,
                new,
                old_tail_hash,
                config.append_tail_hash_bytes,
            ),
        }),
        Job::Diff(job) => Some(Event::DiffReady {
            buffer: job.buffer,
            requester: job.requester,
            result: diff::run_diff(&job),
        }),
        Job::AutoSave(job) => Some(Event::AutoSaved {
            buffer: job.buffer,
            result: autosave::write_auto_save(&job.file, &job.text)
                .map_err(|error| error.to_string()),
        }),
        Job::RemoveAutoSave { file } => {
            // No event reports it; a stale auto-save file is only offered
            // for recovery later.
            if let Err(error) = autosave::remove_auto_save(&file) {
                eprintln!(
                    "emx server: removing the auto-save file of {}: {error}",
                    file.display()
                );
            }
            None
        }
        Job::ReadAutoSave { buffer, file } => Some(Event::AutoSaveRead {
            buffer,
            result: autosave::read_auto_save(&file).map_err(|error| {
                format!(
                    "Opening input file: {}, {}",
                    io::strerror(&error),
                    autosave::auto_save_path(&file).display()
                )
            }),
        }),
        Job::Prefetch { request, cache } => Some(Event::BlockReady {
            buffer: request.buffer,
            block: request.block,
            result: cache
                .get(request.block)
                .map(|_| ())
                .map_err(|error| error.to_string()),
        }),
    }
}

/// One read per block in flight: redisplay asks for a block after every
/// event until it is cached. A block that failed is not asked for again
/// until the next input, so a read error cannot become a busy loop.
#[derive(Debug, Default)]
pub struct PrefetchTracker {
    in_flight: HashSet<(BufferId, u64)>,
    failed: HashSet<(BufferId, u64)>,
}

impl PrefetchTracker {
    /// Whether to start a read for `request`.
    pub fn start(&mut self, request: PrefetchRequest) -> bool {
        let key = (request.buffer, request.block);
        if self.in_flight.contains(&key) || self.failed.contains(&key) {
            return false;
        }
        self.in_flight.insert(key);
        true
    }

    pub fn finished(&mut self, buffer: BufferId, block: u64, ok: bool) {
        self.in_flight.remove(&(buffer, block));
        if !ok {
            self.failed.insert((buffer, block));
        }
    }

    /// New input: failed blocks may be tried again.
    pub fn input(&mut self) {
        self.failed.clear();
    }
}

/// The cache a prefetch for a buffer reads, if its text reads a file.
pub fn prefetch_cache(sources: &Sources) -> Option<Arc<BlockCache>> {
    match &sources.original {
        Original::File(cache) => Some(cache.clone()),
        Original::Memory(_) => None,
    }
}

/// Indexer threads, one per buffer; a new one for the same buffer cancels
/// the old.
#[derive(Default)]
pub struct Indexers {
    running: HashMap<BufferId, Arc<AtomicBool>>,
}

impl Indexers {
    pub fn start(&mut self, buffer: BufferId, sources: &Sources, deliver: Deliver) {
        let Original::File(cache) = &sources.original else {
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        if let Some(previous) = self.running.insert(buffer, cancel.clone()) {
            previous.store(true, Ordering::Relaxed);
        }
        let cache = cache.clone();
        let index = sources.index.clone();
        let spawned = thread::Builder::new()
            .name("emx-index".to_string())
            .spawn(move || index_lines(buffer, &cache, &index, &cancel, &deliver));
        if let Err(error) = spawned {
            eprintln!("emx server: cannot start a line indexer: {error}");
        }
    }

    pub fn finished(&mut self, buffer: BufferId) {
        self.running.remove(&buffer);
    }

    /// Stops every indexer (the server is exiting).
    pub fn cancel_all(&mut self) {
        for (_, cancel) in self.running.drain() {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

fn index_lines(
    buffer: BufferId,
    cache: &BlockCache,
    index: &LineIndex,
    cancel: &AtomicBool,
    deliver: &Deliver,
) {
    let total_blocks = index.block_count();
    let mut progress = |indexed_blocks: u64| {
        if indexed_blocks.is_multiple_of(INDEX_PROGRESS_BLOCKS) {
            deliver(Event::IndexProgress {
                buffer,
                indexed_blocks,
                total_blocks,
            });
        }
    };
    let result = index_file(cache, index, cancel, &mut progress).map_err(|error| error.to_string());
    deliver(Event::IndexFinished { buffer, result });
}

/// Who asked for a directory's listing: every request for a directory
/// already queued or being read waits for that one read.
type Requesters = Vec<(ClientId, ListingId)>;

#[derive(Default)]
struct ListingQueue {
    waiting: VecDeque<PathBuf>,
    requesters: HashMap<PathBuf, Requesters>,
    closed: bool,
}

/// Lists directories for completion (`Effect::ListDirectory`). The threads
/// are never joined: one stuck reading a hung mount must not keep the
/// server from exiting.
pub struct Listers {
    shared: Arc<(Mutex<ListingQueue>, Condvar)>,
}

impl Listers {
    pub fn new(deliver: Deliver) -> Listers {
        let shared = Arc::new((Mutex::new(ListingQueue::default()), Condvar::new()));
        for index in 0..LISTING_THREADS {
            let shared = shared.clone();
            let deliver = deliver.clone();
            // Dropping the handle detaches the thread.
            thread::Builder::new()
                .name(format!("emx-list-{index}"))
                .spawn(move || list_directories(&shared, &deliver))
                .expect("spawning a directory lister");
        }
        Listers { shared }
    }

    pub fn submit(&self, client: ClientId, id: ListingId, directory: PathBuf) {
        let (queue, ready) = &*self.shared;
        let mut state = queue.lock().expect("listing lock");
        match state.requesters.get_mut(&directory) {
            Some(requesters) => requesters.push((client, id)),
            None => {
                state
                    .requesters
                    .insert(directory.clone(), vec![(client, id)]);
                state.waiting.push_back(directory);
                ready.notify_one();
            }
        }
    }
}

impl Drop for Listers {
    fn drop(&mut self) {
        let (queue, ready) = &*self.shared;
        queue.lock().expect("listing lock").closed = true;
        ready.notify_all();
    }
}

fn list_directories(shared: &(Mutex<ListingQueue>, Condvar), deliver: &Deliver) {
    let (queue, ready) = shared;
    loop {
        let directory = {
            let mut state = queue.lock().expect("listing lock");
            loop {
                if state.closed {
                    return;
                }
                if let Some(directory) = state.waiting.pop_front() {
                    break directory;
                }
                state = ready.wait(state).expect("listing lock");
            }
        };
        let result = io::list_directory(&directory);
        let requesters = queue
            .lock()
            .expect("listing lock")
            .requesters
            .remove(&directory)
            .unwrap_or_default();
        for (client, id) in requesters {
            deliver(Event::DirectoryListed {
                client,
                id,
                result: result.clone(),
            });
        }
    }
}
