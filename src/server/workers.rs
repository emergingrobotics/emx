//! Work the editor thread hands off (DESIGN section 8: never block the UI
//! thread): file reads and saves, auto-saves and cache prefetches on a pool
//! of `IO_THREADS` threads, one line indexer thread per file, and a thread
//! per directory listed for completion (`Listers`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use emx_core::completion::{Candidate, ListingError};
use emx_core::diff::{self, DiffJob};
use emx_core::io::{self, SaveFailure, SavePlan};
use emx_core::search_job::{SearchJob, run_search_job};
use emx_core::{
    AutoSaveJob, BufferId, ClientId, Config, Event, ListingId, PrefetchRequest, ReloadJob,
};
use emx_core::{autosave, reload};
use emx_text::{BlockCache, FileIdentity, LineIndex, Original, Sources, index_file};

use super::journal_writer::JournalProgress;

/// DESIGN section 8 sizes the I/O pool at four threads.
pub const IO_THREADS: usize = 4;
/// The most directory reads for completion outstanding at once. Each read
/// has a thread of its own, and a read on a hung network mount never
/// returns and keeps its thread, so they must be capped; typing a path
/// below a hung point asks for a new directory at every `/`, which this
/// leaves plenty of room for. A request over the cap fails at once and the
/// prompt says so, instead of queueing behind reads that may never end.
pub const LISTINGS_IN_FLIGHT_MAX: usize = 64;
/// The most searches (`Effect::StartSearch`) running at once. Each runs on a
/// thread of its own and a search reading a hung network mount never
/// returns, so they are capped; the editor runs one search per client at a
/// time and cancels the one it replaces, so normal use stays far below.
pub const SEARCHES_IN_FLIGHT_MAX: usize = 16;
const TOO_MANY_SEARCHES: &str = "Too many searches in progress";
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

/// Who waits for a directory's listing. A client awaits one listing at a
/// time, so a client's newer request for the same directory replaces its
/// older one: a list holds at most one entry per client.
type Requesters = Vec<(ClientId, ListingId)>;

/// How a directory is read; `io::list_directory` except in tests, which
/// stand in directories that answer late or never.
pub type ListFn = dyn Fn(&Path) -> Result<Vec<Candidate>, ListingError> + Send + Sync;

/// The directories being read, each with who waits for it. It holds at
/// most `cap` directories, and each of those at most one entry per
/// client, so it stays bounded even when reads never return.
type InFlight = Arc<Mutex<HashMap<PathBuf, Requesters>>>;

/// Lists directories for completion (`Effect::ListDirectory`), each
/// directory on a thread of its own: a directory on a hung mount blocks
/// only its own read, never a listing of another directory, a save or the
/// server's exit (the threads are never joined). Requests for a directory
/// already being read share that read.
pub struct Listers {
    in_flight: InFlight,
    deliver: Deliver,
    list: Arc<ListFn>,
    cap: usize,
}

impl Listers {
    pub fn new(deliver: Deliver) -> Listers {
        Listers::with(
            deliver,
            LISTINGS_IN_FLIGHT_MAX,
            Arc::new(io::list_directory),
        )
    }

    /// Listers that read with `list` and allow `cap` reads outstanding.
    pub fn with(deliver: Deliver, cap: usize, list: Arc<ListFn>) -> Listers {
        Listers {
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            deliver,
            list,
            cap,
        }
    }

    pub fn submit(&self, client: ClientId, id: ListingId, directory: PathBuf) {
        let mut in_flight = self.in_flight.lock().expect("listing lock");
        if let Some(requesters) = in_flight.get_mut(&directory) {
            requesters.retain(|(requester, _)| *requester != client);
            requesters.push((client, id));
            return;
        }
        if in_flight.len() >= self.cap {
            drop(in_flight);
            (self.deliver)(Event::DirectoryListed {
                client,
                id,
                result: Err(ListingError::Overloaded),
            });
            return;
        }
        in_flight.insert(directory.clone(), vec![(client, id)]);
        drop(in_flight);
        let shared = self.in_flight.clone();
        let deliver = self.deliver.clone();
        let list = self.list.clone();
        let reader = directory.clone();
        let spawned = thread::Builder::new()
            .name("emx-list".to_string())
            .spawn(move || {
                let result = list(&reader);
                finish_listing(&shared, &deliver, &reader, &result);
            });
        // Dropping the handle detaches the thread. A thread that cannot be
        // started is the same shortage the cap guards against.
        if spawned.is_err() {
            finish_listing(
                &self.in_flight,
                &self.deliver,
                &directory,
                &Err(ListingError::Overloaded),
            );
        }
    }
}

/// Runs searches (`Effect::StartSearch`), each on a thread of its own, so a
/// search over a large file never holds up the editor, a save or another
/// search, and one stuck on a hung mount holds only its own thread (the
/// threads are never joined). The editor cancels a search it no longer
/// wants through the job's flag; a cancelled search reports nothing.
pub struct Searchers {
    in_flight: Arc<AtomicUsize>,
    deliver: Deliver,
    cap: usize,
}

impl Searchers {
    pub fn new(deliver: Deliver) -> Searchers {
        Searchers::with(deliver, SEARCHES_IN_FLIGHT_MAX)
    }

    pub fn with(deliver: Deliver, cap: usize) -> Searchers {
        Searchers {
            in_flight: Arc::new(AtomicUsize::new(0)),
            deliver,
            cap,
        }
    }

    pub fn submit(&self, job: SearchJob) {
        if self.in_flight.fetch_add(1, Ordering::AcqRel) >= self.cap {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            self.refuse(job.client, job.id);
            return;
        }
        let (client, id) = (job.client, job.id);
        let in_flight = self.in_flight.clone();
        let deliver = self.deliver.clone();
        let spawned = thread::Builder::new()
            .name("emx-search".to_string())
            .spawn(move || {
                let event = run_search_job(&job);
                in_flight.fetch_sub(1, Ordering::AcqRel);
                if !job.cancel.load(Ordering::Relaxed) {
                    deliver(event);
                }
            });
        // A thread that cannot be started is the shortage the cap guards
        // against; the editor still waits, so it is told.
        if spawned.is_err() {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            self.refuse(client, id);
        }
    }

    fn refuse(&self, client: ClientId, id: emx_core::SearchId) {
        (self.deliver)(Event::SearchResult {
            client,
            id,
            result: Err(TOO_MANY_SEARCHES.to_string()),
        });
    }
}

fn finish_listing(
    in_flight: &InFlight,
    deliver: &Deliver,
    directory: &Path,
    result: &Result<Vec<Candidate>, ListingError>,
) {
    let requesters = in_flight
        .lock()
        .expect("listing lock")
        .remove(directory)
        .unwrap_or_default();
    for (client, id) in requesters {
        deliver(Event::DirectoryListed {
            client,
            id,
            result: result.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, Receiver};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    use emx_core::completion::Candidate;

    use super::*;

    const WAIT: Duration = Duration::from_secs(10);
    const HUNG: &str = "/hung";

    /// A gate that holds reads below `HUNG` (and every read, when `all`) until
    /// opened, counting reads.
    #[derive(Default)]
    struct Gate {
        open: Mutex<bool>,
        opened: Condvar,
        reads: Mutex<Vec<PathBuf>>,
    }

    impl Gate {
        fn open(&self) {
            *self.open.lock().expect("gate") = true;
            self.opened.notify_all();
        }

        fn wait(&self) {
            let mut open = self.open.lock().expect("gate");
            while !*open {
                open = self.opened.wait(open).expect("gate");
            }
        }
    }

    fn listers(cap: usize, gate: &Arc<Gate>, all: bool) -> (Listers, Receiver<Event>) {
        let (sender, receiver) = mpsc::channel();
        let sender = Mutex::new(sender);
        let deliver: Deliver = Arc::new(move |event| {
            sender
                .lock()
                .expect("sender")
                .send(event)
                .expect("receiver");
        });
        let gate = gate.clone();
        let list: Arc<ListFn> = Arc::new(move |directory: &Path| {
            gate.reads
                .lock()
                .expect("reads")
                .push(directory.to_path_buf());
            if all || directory.starts_with(HUNG) {
                gate.wait();
            }
            Ok(vec![Candidate::new(
                directory.as_os_str().as_encoded_bytes(),
            )])
        });
        (Listers::with(deliver, cap, list), receiver)
    }

    fn listed(event: Event) -> (ClientId, ListingId, Result<Vec<Candidate>, ListingError>) {
        match event {
            Event::DirectoryListed { client, id, result } => (client, id, result),
            other => panic!("unexpected event {other:?}"),
        }
    }

    #[test]
    fn requests_for_one_directory_share_one_read() {
        let gate = Arc::new(Gate::default());
        let (listers, events) = listers(LISTINGS_IN_FLIGHT_MAX, &gate, true);
        listers.submit(ClientId(1), ListingId(1), PathBuf::from("/a"));
        listers.submit(ClientId(2), ListingId(2), PathBuf::from("/a"));
        // The first client moved on: only its newest request is answered.
        listers.submit(ClientId(1), ListingId(3), PathBuf::from("/a"));
        gate.open();
        let first = listed(events.recv_timeout(WAIT).expect("listed"));
        let second = listed(events.recv_timeout(WAIT).expect("listed"));
        let mut answered = vec![(first.0, first.1), (second.0, second.1)];
        answered.sort();
        assert_eq!(
            answered,
            [(ClientId(1), ListingId(3)), (ClientId(2), ListingId(2))]
        );
        assert_eq!(first.2, second.2);
        assert!(events.recv_timeout(Duration::from_millis(100)).is_err());
        assert_eq!(*gate.reads.lock().expect("reads"), [PathBuf::from("/a")]);
    }

    /// More hung directories than a fixed pool would have threads (typing
    /// a path below a hung mount asks for one at every `/`).
    const HUNG_DIRECTORIES: u64 = 8;

    #[test]
    fn hung_directories_do_not_hold_up_another() {
        let gate = Arc::new(Gate::default());
        let (listers, events) = listers(LISTINGS_IN_FLIGHT_MAX, &gate, false);
        let mut hung = PathBuf::from(HUNG);
        for _ in 0..HUNG_DIRECTORIES {
            hung.push("below");
            listers.submit(ClientId(1), ListingId(1), hung.clone());
        }
        listers.submit(ClientId(2), ListingId(2), PathBuf::from("/fast"));
        let (client, id, result) = listed(events.recv_timeout(WAIT).expect("listed"));
        assert_eq!((client, id), (ClientId(2), ListingId(2)));
        assert_eq!(result.expect("read").len(), 1);
        gate.open();
        for _ in 0..HUNG_DIRECTORIES {
            let (client, _, _) = listed(events.recv_timeout(WAIT).expect("listed"));
            assert_eq!(client, ClientId(1));
        }
    }

    #[test]
    fn requests_over_the_cap_fail_at_once() {
        let gate = Arc::new(Gate::default());
        let (listers, events) = listers(2, &gate, true);
        listers.submit(ClientId(1), ListingId(1), PathBuf::from("/one"));
        listers.submit(ClientId(1), ListingId(2), PathBuf::from("/two"));
        listers.submit(ClientId(1), ListingId(3), PathBuf::from("/three"));
        let (_, id, result) = listed(events.recv_timeout(WAIT).expect("refused"));
        assert_eq!(id, ListingId(3));
        assert_eq!(result, Err(ListingError::Overloaded));
        // A directory already being read is no new read: it is not refused.
        listers.submit(ClientId(2), ListingId(4), PathBuf::from("/one"));
        assert!(events.recv_timeout(Duration::from_millis(100)).is_err());
        gate.open();
        let mut answered: Vec<ListingId> = (0..3)
            .map(|_| listed(events.recv_timeout(WAIT).expect("listed")).1)
            .collect();
        answered.sort();
        assert_eq!(answered, [ListingId(1), ListingId(2), ListingId(4)]);
        // With the reads done there is room again.
        listers.submit(ClientId(1), ListingId(5), PathBuf::from("/three"));
        let (_, id, result) = listed(events.recv_timeout(WAIT).expect("listed"));
        assert_eq!(id, ListingId(5));
        assert!(result.is_ok());
    }

    #[test]
    fn finished_listings_leave_nothing_behind() {
        let gate = Arc::new(Gate::default());
        let (listers, events) = listers(LISTINGS_IN_FLIGHT_MAX, &gate, false);
        listers.submit(ClientId(1), ListingId(1), PathBuf::from("/a"));
        assert!(listed(events.recv_timeout(WAIT).expect("listed")).2.is_ok());
        let in_flight = Arc::downgrade(&listers.in_flight);
        drop(listers);
        // The reading thread lets go of the state once it has delivered.
        let deadline = Instant::now() + WAIT;
        while in_flight.upgrade().is_some() {
            assert!(
                Instant::now() < deadline,
                "a listing thread was left running"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
