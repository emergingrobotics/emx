//! The session server (DESIGN section 2): one editor shared by every
//! client of this user, its buffers journaled so a crash loses nothing.
//!
//! Threads:
//! - the editor thread (this module) owns the `Editor`; it takes inputs
//!   from one channel, runs `Editor::handle`, and carries out the effects;
//! - an accept thread admits connections and starts a reader and a writer
//!   thread for each (`client_conn`);
//! - the journal thread applies journal operations in order and syncs in
//!   batches (`journal_writer`);
//! - a pool of I/O threads loads, saves, auto-saves and prefetches, and an
//!   indexer thread per large file counts lines (`workers`);
//! - a signal thread turns SIGTERM and SIGINT into a clean exit.

pub mod client_conn;
pub mod journal_writer;
pub mod local;
pub mod workers;

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use emx_core::journal::{JournalWriter, replay_dir};
use emx_core::{ClientId, Config, Editor, Effect, Event, Session, TimerId};
use emx_proto::{ClientKind, ServerMsg};
use rustix::fs::FlockOperation;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::paths::{self, JournalMode};
use client_conn::{Delivery, Outbox};
use journal_writer::{JournalBackend, JournalFailure, JournalThread};
use workers::{Deliver, Indexers, Job, Pool, PrefetchTracker, prefetch_cache};

/// A server that finds the lock held retries for this long, so one
/// started while the previous one is exiting still comes up.
pub const LOCK_WAIT_MS: u64 = 2000;
const LOCK_RETRY_MS: u64 = 20;
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
const EXIT_SHUTDOWN: &str = "the emx server is exiting";
const EXIT_TERMINATED: &str = "the emx server was terminated";

/// Everything the editor thread reacts to.
pub enum Input {
    Attach {
        client: ClientId,
        kind: ClientKind,
        cols: u16,
        rows: u16,
        outbox: Arc<Outbox>,
    },
    /// A key, paste, resize, request or redraw from an attached client.
    Client(ClientId, Event),
    Detach(ClientId),
    /// The client broke the protocol; it is told why and disconnected.
    ProtocolError(ClientId, String),
    /// The client's outbox drained after frames were dropped.
    Resync(ClientId),
    /// A worker's result.
    Worker(Event),
    JournalFailed(JournalFailure),
    /// SIGTERM or SIGINT.
    Terminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerOptions {
    /// Start a server in the background (as clients do) and return once it
    /// listens.
    pub detach: bool,
    /// Lead a new session, away from any terminal; how `--detach` starts
    /// the server it leaves running.
    pub new_session: bool,
}

/// How long `--detach` waits for the server it started to listen.
pub const DETACH_WAIT_MS: u64 = 2000;
const DETACH_POLL_MS: u64 = 20;

/// Runs the session server until it exits; with `detach`, starts one in
/// the background instead.
pub fn run(options: ServerOptions) -> anyhow::Result<()> {
    if options.detach {
        return start_detached();
    }
    if options.new_session {
        // A child spawned normally is never a process-group leader, so
        // this succeeds; the terminal's hangup then cannot reach the server.
        rustix::process::setsid().context("starting a new session")?;
    }
    let config = load_config()?;
    let euid = rustix::process::geteuid().as_raw();
    let socket_dir = paths::socket_dir(euid)?;
    let _lock = acquire_lock(&paths::lock_path(&socket_dir))?;
    let socket_path = paths::socket_path(&socket_dir);
    // Only the lock holder may remove a socket: a server holding it is
    // alive, so a socket left without one is stale.
    match fs::remove_file(&socket_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("removing the stale socket {}", socket_path.display()));
        }
    }
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("listening on {}", socket_path.display()))?;
    let journal_dir = paths::journal_dir(JournalMode::Server)?;
    let writer = JournalWriter::new(&journal_dir)
        .with_context(|| format!("opening the journal directory {}", journal_dir.display()))?;
    let recovered = replay_dir(&journal_dir, &config)?;
    let (inputs, receiver) = mpsc::channel();
    start_signal_thread(inputs.clone())?;
    start_accept_thread(listener, euid, inputs.clone())?;
    let mut server = Server::new(config, Session::Server, Box::new(writer), inputs, receiver)?;
    server.handle(Event::Recovered(recovered));
    let outcome = server.run();
    if let Err(error) = fs::remove_file(&socket_path)
        && error.kind() != io::ErrorKind::NotFound
    {
        eprintln!("emx server: removing {}: {error}", socket_path.display());
    }
    outcome
}

pub fn load_config() -> anyhow::Result<Config> {
    let path = paths::config_path()?;
    let exists = fs::exists(&path)
        .with_context(|| format!("checking for the config file {}", path.display()))?;
    Ok(Config::load(exists.then_some(path.as_path()))?)
}

/// Starts `emx server --new-session` with no terminal (stdin and stdout
/// on /dev/null, stderr appended to the log in the state directory) and
/// waits until a server listens. Re-spawning rather than detaching in
/// place works from an interactive shell too, where this process leads
/// its process group and so cannot call `setsid`. Racing starters are
/// safe: the server lock admits one, and every starter sees it listen.
fn start_detached() -> anyhow::Result<()> {
    let euid = rustix::process::geteuid().as_raw();
    let socket_path = paths::socket_path(&paths::socket_dir(euid)?);
    let state = paths::state_dir()?;
    DirBuilder::new()
        .recursive(true)
        .mode(PRIVATE_DIRECTORY_MODE)
        .create(&state)
        .with_context(|| format!("creating {}", state.display()))?;
    let log_path = paths::server_log_path()?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(PRIVATE_FILE_MODE)
        .open(&log_path)
        .with_context(|| format!("opening the log {}", log_path.display()))?;
    let program = std::env::current_exe().context("finding the emx program")?;
    let mut child = Command::new(program)
        .args(["server", "--new-session"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .context("starting the emx server")?;
    let deadline = Instant::now() + Duration::from_millis(DETACH_WAIT_MS);
    loop {
        if UnixStream::connect(&socket_path).is_ok() {
            return Ok(());
        }
        let exited = child
            .try_wait()
            .context("checking on the emx server")?
            .is_some();
        if exited || Instant::now() >= deadline {
            bail!("the emx server did not start; see {}", log_path.display());
        }
        thread::sleep(Duration::from_millis(DETACH_POLL_MS));
    }
}

/// One server per user: the lock is held for the server's lifetime.
fn acquire_lock(path: &Path) -> anyhow::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(PRIVATE_FILE_MODE)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
        .with_context(|| format!("opening the lock {}", path.display()))?;
    let deadline = Instant::now() + Duration::from_millis(LOCK_WAIT_MS);
    loop {
        match rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(file),
            Err(rustix::io::Errno::WOULDBLOCK) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(LOCK_RETRY_MS));
            }
            Err(rustix::io::Errno::WOULDBLOCK) => {
                bail!(
                    "another emx server is running (it holds {})",
                    path.display()
                )
            }
            Err(error) => {
                return Err(io::Error::from(error))
                    .with_context(|| format!("locking {}", path.display()));
            }
        }
    }
}

/// SIGTERM and SIGINT end the server cleanly; SIGHUP is ignored, so a
/// server started from a terminal outlives it.
fn start_signal_thread(inputs: Sender<Input>) -> anyhow::Result<()> {
    let mut signals =
        Signals::new([SIGTERM, SIGINT, SIGHUP]).context("installing signal handlers")?;
    thread::Builder::new()
        .name("emx-signals".to_string())
        .spawn(move || {
            for signal in signals.forever() {
                if signal != SIGHUP && inputs.send(Input::Terminate).is_err() {
                    return;
                }
            }
        })
        .context("starting the signal thread")?;
    Ok(())
}

fn start_accept_thread(
    listener: UnixListener,
    euid: u32,
    inputs: Sender<Input>,
) -> anyhow::Result<()> {
    thread::Builder::new()
        .name("emx-accept".to_string())
        .spawn(move || accept_loop(&listener, euid, &inputs))
        .context("starting the accept thread")?;
    Ok(())
}

fn accept_loop(listener: &UnixListener, euid: u32, inputs: &Sender<Input>) {
    let mut next_client = 1;
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("emx server: accepting a connection: {error}");
                continue;
            }
        };
        if let Err(error) = client_conn::admit(&stream, euid, &client_conn::peer_uid) {
            eprintln!("emx server: {error}");
            continue;
        }
        let client = ClientId(next_client);
        next_client += 1;
        let inputs = inputs.clone();
        let spawned = thread::Builder::new()
            .name(format!("emx-read-{}", client.0))
            .spawn(move || client_conn::serve(stream, client, inputs));
        if let Err(error) = spawned {
            eprintln!("emx server: cannot serve a client: {error}");
        }
    }
}

struct Connection {
    outbox: Arc<Outbox>,
    /// Frames are dropped until the client resyncs.
    awaiting_resync: bool,
}

/// The editor thread's state.
pub struct Server {
    editor: Editor,
    receiver: Receiver<Input>,
    connections: HashMap<ClientId, Connection>,
    /// Pending timers, soonest first; ties keep the order they were set.
    timers: BinaryHeap<Reverse<(Instant, u64, TimerId)>>,
    timer_serial: u64,
    journal: JournalThread,
    pool: Pool,
    prefetches: PrefetchTracker,
    indexers: Indexers,
    deliver: Deliver,
    /// Events the server itself raised while carrying out effects.
    raised: VecDeque<Event>,
    shutdown: bool,
    prefetch_reads: u64,
    session: Session,
}

/// Debug builds print a diagnostic on stderr for every key when this is
/// set, as the editor thread does when a journal or file operation fails.
#[cfg(debug_assertions)]
pub const DIAGNOSTIC_ENV: &str = "EMX_TEST_DIAGNOSTIC";

#[cfg(debug_assertions)]
fn diagnostic_seam(event: &Event) {
    if matches!(event, Event::Key(..)) && std::env::var_os(DIAGNOSTIC_ENV).is_some() {
        eprintln!("emx server: {DIAGNOSTIC_ENV}: a diagnostic for the key");
    }
}

#[cfg(not(debug_assertions))]
fn diagnostic_seam(_event: &Event) {}

/// Whether the server goes on after a `Server::step`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Continue,
    Exit,
}

impl Server {
    pub fn new(
        config: Config,
        session: Session,
        journal: Box<dyn JournalBackend>,
        inputs: Sender<Input>,
        receiver: Receiver<Input>,
    ) -> anyhow::Result<Server> {
        let fsync_interval = Duration::from_millis(config.journal_fsync_ms);
        let journal_inputs = inputs.clone();
        let journal = JournalThread::spawn(
            journal,
            fsync_interval,
            Box::new(move |failure| {
                // The editor thread is gone only when the server is exiting.
                drop(journal_inputs.send(Input::JournalFailed(failure)));
            }),
        );
        let worker_inputs = inputs.clone();
        let deliver: Deliver = Arc::new(move |event| {
            // The editor thread is gone only when the server is exiting.
            drop(worker_inputs.send(Input::Worker(event)));
        });
        let pool = Pool::new(
            Arc::new(config.clone()),
            journal.progress(),
            deliver.clone(),
        );
        let editor = Editor::with_session(config, session)?;
        Ok(Server {
            editor,
            receiver,
            connections: HashMap::new(),
            timers: BinaryHeap::new(),
            timer_serial: 0,
            journal,
            pool,
            prefetches: PrefetchTracker::default(),
            indexers: Indexers::default(),
            deliver,
            raised: VecDeque::new(),
            shutdown: false,
            prefetch_reads: 0,
            session,
        })
    }

    /// Serves inputs until the editor shuts down or a signal ends it.
    pub fn run(&mut self) -> anyhow::Result<()> {
        while self.step(None)? == Step::Continue {}
        Ok(())
    }

    /// Fires the timers that are due, then waits for one input (no longer
    /// than `limit`, when given) and handles it.
    pub fn step(&mut self, limit: Option<Duration>) -> anyhow::Result<Step> {
        self.fire_due_timers();
        if self.shutdown {
            // `--no-server` shuts down only when its user exits: nothing to
            // explain.
            let reason = match self.session {
                Session::Server => EXIT_SHUTDOWN,
                Session::NoServer => "",
            };
            self.finish(reason);
            return Ok(Step::Exit);
        }
        let now = Instant::now();
        let deadline = match (self.next_deadline(), limit.map(|limit| now + limit)) {
            (Some(timer), Some(limit)) => Some(timer.min(limit)),
            (timer, limit) => timer.or(limit),
        };
        let input = match deadline {
            Some(deadline) => match self
                .receiver
                .recv_timeout(deadline.saturating_duration_since(now))
            {
                Ok(input) => input,
                Err(RecvTimeoutError::Timeout) => return Ok(Step::Continue),
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("every input of the editor thread closed")
                }
            },
            None => self
                .receiver
                .recv()
                .context("every input of the editor thread closed")?,
        };
        if matches!(input, Input::Terminate) {
            // Unsaved edits stay journaled for the next server.
            self.finish(EXIT_TERMINATED);
            return Ok(Step::Exit);
        }
        self.input(input);
        Ok(Step::Continue)
    }

    pub fn editor(&self) -> &Editor {
        &self.editor
    }

    /// Cache blocks read for redisplay so far.
    pub fn prefetch_reads(&self) -> u64 {
        self.prefetch_reads
    }

    /// Applies and syncs every journal operation, lets the clients go and
    /// stops the workers.
    fn finish(&mut self, reason: &str) {
        self.journal.flush();
        for (_, connection) in self.connections.drain() {
            connection.outbox.push(ServerMsg::Exit {
                reason: reason.to_string(),
            });
            connection.outbox.close();
        }
        self.indexers.cancel_all();
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.timers
            .peek()
            .map(|Reverse((deadline, _, _))| *deadline)
    }

    fn fire_due_timers(&mut self) {
        let now = Instant::now();
        while let Some(Reverse((deadline, _, id))) = self.timers.peek().copied() {
            if deadline > now || self.shutdown {
                return;
            }
            self.timers.pop();
            self.handle(Event::Timer(id));
        }
    }

    /// Handles one input as if it came from the channel.
    pub fn input(&mut self, input: Input) {
        match input {
            Input::Attach {
                client,
                kind,
                cols,
                rows,
                outbox,
            } => {
                self.prefetches.input();
                self.connections.insert(
                    client,
                    Connection {
                        outbox,
                        awaiting_resync: false,
                    },
                );
                self.handle(Event::ClientAttached {
                    client,
                    cols,
                    rows,
                    kind,
                });
            }
            Input::Client(client, event) => {
                diagnostic_seam(&event);
                if self.connections.contains_key(&client) {
                    self.prefetches.input();
                    self.handle(event);
                }
            }
            Input::Detach(client) => {
                if self.connections.remove(&client).is_some() {
                    self.handle(Event::ClientDetached(client));
                }
            }
            Input::ProtocolError(client, reason) => {
                eprintln!("emx server: client {}: {reason}", client.0);
                self.disconnect(client, &reason);
                self.handle_raised();
            }
            Input::Resync(client) => {
                if let Some(connection) = self.connections.get_mut(&client) {
                    connection.awaiting_resync = false;
                    self.handle(Event::Redraw(client));
                }
            }
            Input::Worker(event) => {
                match &event {
                    Event::BlockReady {
                        buffer,
                        block,
                        result,
                    } => self.prefetches.finished(*buffer, *block, result.is_ok()),
                    Event::IndexFinished { buffer, .. } => self.indexers.finished(*buffer),
                    _ => {}
                }
                self.handle(event);
            }
            Input::JournalFailed(failure) => {
                self.editor
                    .journal_write_failed(failure.journal, &failure.reason);
                let effects = self.editor.take_effects();
                self.dispatch(effects);
                self.handle_raised();
            }
            Input::Terminate => unreachable!("run handles Terminate"),
        }
    }

    /// Runs one event through the editor and carries out its effects.
    pub fn handle(&mut self, event: Event) {
        self.editor.handle(event);
        let effects = self.editor.take_effects();
        self.dispatch(effects);
        self.handle_raised();
    }

    fn handle_raised(&mut self) {
        while let Some(event) = self.raised.pop_front() {
            self.editor.handle(event);
            let effects = self.editor.take_effects();
            self.dispatch(effects);
        }
    }

    /// Carries out effects in order. Journal operations are queued first
    /// in line: every job that reads or writes a file waits for the journal
    /// operations emitted before it.
    fn dispatch(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Journal(op) => {
                    self.journal.submit(op);
                }
                Effect::Send(client, message) => self.send(client, message),
                Effect::SetTimer { id, after } => {
                    self.timer_serial += 1;
                    self.timers
                        .push(Reverse((Instant::now() + after, self.timer_serial, id)));
                }
                Effect::Prefetch(request) => {
                    let cache = self
                        .editor
                        .buffer_sources(request.buffer)
                        .and_then(|sources| prefetch_cache(&sources));
                    if let Some(cache) = cache
                        && self.prefetches.start(request)
                    {
                        self.prefetch_reads += 1;
                        self.submit(Job::Prefetch { request, cache });
                    }
                }
                Effect::Load { buffer, path } => self.submit(Job::Load { buffer, path }),
                Effect::Save { buffer, plan } => self.submit(Job::Save { buffer, plan }),
                Effect::StartIndex { buffer } => {
                    if let Some(sources) = self.editor.buffer_sources(buffer) {
                        self.indexers.start(buffer, &sources, self.deliver.clone());
                    }
                }
                Effect::AutoSave(job) => self.submit(Job::AutoSave(job)),
                Effect::RemoveAutoSave { file } => self.submit(Job::RemoveAutoSave { file }),
                Effect::ReadAutoSave { buffer, file } => {
                    self.submit(Job::ReadAutoSave { buffer, file });
                }
                Effect::Observe {
                    buffer,
                    path,
                    old_len,
                } => self.submit(Job::Observe {
                    buffer,
                    path,
                    old_len,
                }),
                Effect::Reload(job) => self.submit(Job::Reload(job)),
                Effect::ReadAppend {
                    buffer,
                    path,
                    old,
                    new,
                    old_tail_hash,
                } => self.submit(Job::ReadAppend {
                    buffer,
                    path,
                    old,
                    new,
                    old_tail_hash,
                }),
                Effect::Diff(job) => self.submit(Job::Diff(job)),
                Effect::Shutdown => self.shutdown = true,
            }
        }
    }

    /// Loads and saves wait for the journal operations emitted before them:
    /// a snapshot record reads text that a save may rewrite in place.
    fn submit(&self, job: Job) {
        let after_journal = match job {
            Job::Load { .. } | Job::Save { .. } | Job::Reload(_) | Job::ReadAppend { .. } => {
                self.journal.submitted()
            }
            _ => 0,
        };
        self.pool.submit(job, after_journal);
    }

    fn send(&mut self, client: ClientId, message: ServerMsg) {
        let Some(connection) = self.connections.get_mut(&client) else {
            return;
        };
        let frame = matches!(message, ServerMsg::Frame { .. });
        if frame && connection.awaiting_resync {
            return;
        }
        let exit = matches!(message, ServerMsg::Exit { .. });
        match connection.outbox.push(message) {
            Delivery::Queued => {}
            Delivery::FrameDropped => connection.awaiting_resync = true,
            Delivery::Overflow => {
                self.cut_off(client);
                return;
            }
        }
        if exit {
            // The editor let the client go (`C-x C-c`): end the connection
            // after what is queued and detach it now.
            connection.outbox.close();
            self.connections.remove(&client);
            self.raised.push_back(Event::ClientDetached(client));
        }
    }

    /// A client that stops reading is cut off at once: its backlog is
    /// dropped and its socket shut down, even mid-write.
    fn cut_off(&mut self, client: ClientId) {
        let Some(connection) = self.connections.remove(&client) else {
            return;
        };
        eprintln!(
            "emx server: client {} stopped reading; cutting it off",
            client.0
        );
        connection.outbox.cut_off();
        self.raised.push_back(Event::ClientDetached(client));
    }

    /// Tells the client why, then ends the connection.
    fn disconnect(&mut self, client: ClientId, reason: &str) {
        let Some(connection) = self.connections.remove(&client) else {
            return;
        };
        connection.outbox.push(ServerMsg::Exit {
            reason: reason.to_string(),
        });
        connection.outbox.close();
        self.raised.push_back(Event::ClientDetached(client));
    }
}
