//! Work the editor thread hands off (DESIGN section 8: never block the UI
//! thread): file reads and saves, auto-saves and cache prefetches on a pool
//! of `IO_THREADS` threads, and one line indexer thread per file.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use emx_core::autosave;
use emx_core::io::{self, SaveFailure, SavePlan};
use emx_core::{AutoSaveJob, BufferId, Config, Event, PrefetchRequest};
use emx_text::{BlockCache, LineIndex, Original, Sources, index_file};

use super::journal_writer::JournalProgress;

/// DESIGN section 8 sizes the I/O pool at four threads.
pub const IO_THREADS: usize = 4;
/// The indexer reports its progress every this many blocks.
pub const INDEX_PROGRESS_BLOCKS: u64 = 256;

/// Set in the environment, a save job panics on its I/O thread: the seam
/// tests use to check that a worker panic ends the server (`panic =
/// "abort"`) instead of leaving it wedged.
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
}

impl Job {
    /// Jobs on the same file are run in the order submitted, one at a
    /// time: an auto-save write and its removal must not race.
    fn key(&self) -> Option<PathBuf> {
        match self {
            Job::AutoSave(job) => Some(job.file.clone()),
            Job::RemoveAutoSave { file } | Job::ReadAutoSave { file, .. } => Some(file.clone()),
            Job::Load { .. } | Job::Save { .. } | Job::Prefetch { .. } => None,
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
        let panic_on_save = std::env::var_os(WORKER_PANIC_ENV).is_some();
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
        if panic_on_save && matches!(queued.job, Job::Save { .. }) {
            panic!("{WORKER_PANIC_ENV} is set: an I/O thread panics on a save");
        }
        if let Some(event) = run(queued.job, config) {
            deliver(event);
        }
        if let Some(key) = queued.key {
            queue.lock().expect("pool lock").busy.remove(&key);
            ready.notify_all();
        }
    }
}

fn run(job: Job, config: &Config) -> Option<Event> {
    match job {
        Job::Load { buffer, path } => Some(Event::FileLoaded {
            buffer,
            result: io::load_file(&path, config).map_err(|error| error.to_string()),
        }),
        Job::Save { buffer, plan } => Some(Event::SaveFinished {
            buffer,
            result: io::save(&plan).map_err(|error| SaveFailure::from(&error)),
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
