//! The journal thread: applies `Effect::Journal` operations in the order
//! the editor emitted them and syncs in batches (DESIGN section 2: every
//! `journal_fsync_ms`, which also limits SD-card wear).
//!
//! Workers that must not start before earlier journal operations are
//! applied (a save may rewrite the bytes a snapshot record still has to
//! read) wait on `JournalProgress` for the sequence number of the last
//! operation submitted before them.

use std::collections::HashMap;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use emx_core::journal::{JournalError, JournalId, JournalOp, JournalWriter, Record, SyncFailure};

/// After reporting a journal's failure, further failures of the same
/// journal wait this long before they are reported again, so a disk that
/// keeps failing does not keep the editor restarting the journal in a loop.
pub const JOURNAL_RETRY_MS: u64 = 1000;

/// What the journal thread writes to; a `JournalWriter` except in tests.
pub trait JournalBackend: Send {
    fn apply(&mut self, op: JournalOp) -> Result<(), JournalError>;
    fn sync(&mut self) -> Vec<SyncFailure>;
}

impl JournalBackend for JournalWriter {
    fn apply(&mut self, op: JournalOp) -> Result<(), JournalError> {
        JournalWriter::apply(self, op)
    }

    fn sync(&mut self) -> Vec<SyncFailure> {
        JournalWriter::sync(self)
    }
}

/// A journal lost a write or a sync; the editor restarts it
/// (`Editor::journal_write_failed`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalFailure {
    pub journal: JournalId,
    pub reason: String,
}

/// The sequence number of the last operation the journal thread finished.
#[derive(Debug, Clone, Default)]
pub struct JournalProgress {
    applied: Arc<(Mutex<u64>, Condvar)>,
}

impl JournalProgress {
    pub fn applied(&self) -> u64 {
        *self.applied.0.lock().expect("journal progress lock")
    }

    fn advance(&self, sequence: u64) {
        let (applied, changed) = &*self.applied;
        *applied.lock().expect("journal progress lock") = sequence;
        changed.notify_all();
    }

    /// Blocks until operation `sequence` (and every one before it) is
    /// applied.
    pub fn wait_for(&self, sequence: u64) {
        let (applied, changed) = &*self.applied;
        let mut current = applied.lock().expect("journal progress lock");
        while *current < sequence {
            current = changed.wait(current).expect("journal progress lock");
        }
    }
}

enum Command {
    Apply(u64, JournalOp),
    /// Sync now and acknowledge.
    Flush(mpsc::Sender<()>),
}

pub struct JournalThread {
    commands: mpsc::Sender<Command>,
    progress: JournalProgress,
    submitted: u64,
    handle: Option<JoinHandle<()>>,
}

impl JournalThread {
    /// `report` is called on the journal thread for each failure to
    /// report.
    pub fn spawn(
        backend: Box<dyn JournalBackend>,
        fsync_interval: Duration,
        report: Box<dyn Fn(JournalFailure) + Send>,
    ) -> JournalThread {
        let (commands, receiver) = mpsc::channel();
        let progress = JournalProgress::default();
        let state = Batcher {
            backend,
            fsync_interval,
            progress: progress.clone(),
            report,
            unsynced_since: None,
            reported_at: HashMap::new(),
            held: HashMap::new(),
        };
        let handle = thread::Builder::new()
            .name("emx-journal".to_string())
            .spawn(move || state.run(receiver))
            .expect("spawning the journal thread");
        JournalThread {
            commands,
            progress,
            submitted: 0,
            handle: Some(handle),
        }
    }

    /// Queues `op`; returns its sequence number.
    pub fn submit(&mut self, op: JournalOp) -> u64 {
        self.submitted += 1;
        self.commands
            .send(Command::Apply(self.submitted, op))
            .expect("the journal thread runs until the server drops it");
        self.submitted
    }

    /// The sequence number of the last operation submitted.
    pub fn submitted(&self) -> u64 {
        self.submitted
    }

    pub fn progress(&self) -> JournalProgress {
        self.progress.clone()
    }

    /// Applies everything submitted and syncs it before returning.
    pub fn flush(&self) {
        let (acknowledge, acknowledged) = mpsc::channel();
        self.commands
            .send(Command::Flush(acknowledge))
            .expect("the journal thread runs until the server drops it");
        acknowledged
            .recv()
            .expect("the journal thread acknowledges a flush");
    }
}

impl Drop for JournalThread {
    /// Closing the channel makes the thread sync once more and stop.
    fn drop(&mut self) {
        let (closed, _) = mpsc::channel();
        drop(std::mem::replace(&mut self.commands, closed));
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            eprintln!("emx server: the journal thread panicked");
        }
    }
}

struct Batcher {
    backend: Box<dyn JournalBackend>,
    fsync_interval: Duration,
    progress: JournalProgress,
    report: Box<dyn Fn(JournalFailure) + Send>,
    /// When the oldest operation not yet synced was applied.
    unsynced_since: Option<Instant>,
    /// When each journal's failure was last reported.
    reported_at: HashMap<JournalId, Instant>,
    /// Failures held back by `JOURNAL_RETRY_MS`.
    held: HashMap<JournalId, String>,
}

fn journal_of(op: &JournalOp) -> JournalId {
    match op {
        JournalOp::Open { buffer, .. }
        | JournalOp::Record { buffer, .. }
        | JournalOp::Close { buffer } => *buffer,
    }
}

fn starts_epoch(op: &JournalOp) -> bool {
    matches!(
        op,
        JournalOp::Record {
            record: Record::EpochSnapshot { .. } | Record::EpochIdentity { .. },
            ..
        }
    )
}

impl Batcher {
    fn run(mut self, commands: mpsc::Receiver<Command>) {
        loop {
            let command = match self.next_deadline() {
                Some(deadline) => {
                    let wait = deadline.saturating_duration_since(Instant::now());
                    match commands.recv_timeout(wait) {
                        Ok(command) => Some(command),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                None => match commands.recv() {
                    Ok(command) => Some(command),
                    Err(_) => break,
                },
            };
            match command {
                Some(Command::Apply(sequence, op)) => self.apply(sequence, op),
                Some(Command::Flush(acknowledge)) => {
                    self.sync();
                    // The server may have stopped waiting; nothing to do then.
                    let _ = acknowledge.send(());
                }
                None => {}
            }
            let now = Instant::now();
            if self
                .unsynced_since
                .is_some_and(|since| now >= since + self.fsync_interval)
            {
                self.sync();
            }
            self.report_held(now);
        }
        self.sync();
    }

    fn next_deadline(&self) -> Option<Instant> {
        let sync = self.unsynced_since.map(|since| since + self.fsync_interval);
        let retry = self
            .held
            .keys()
            .filter_map(|journal| self.reported_at.get(journal))
            .map(|reported| *reported + Duration::from_millis(JOURNAL_RETRY_MS))
            .min();
        match (sync, retry) {
            (Some(sync), Some(retry)) => Some(sync.min(retry)),
            (sync, retry) => sync.or(retry),
        }
    }

    fn apply(&mut self, sequence: u64, op: JournalOp) {
        let journal = journal_of(&op);
        let epoch = starts_epoch(&op);
        match self.backend.apply(op) {
            Ok(()) => {
                if epoch {
                    self.held.remove(&journal);
                }
            }
            // The failure that poisoned it was reported already.
            Err(JournalError::Poisoned { .. }) => {}
            Err(error) => self.failed(journal, error.to_string()),
        }
        self.progress.advance(sequence);
        self.unsynced_since.get_or_insert_with(Instant::now);
    }

    fn sync(&mut self) {
        let failures = self.backend.sync();
        self.unsynced_since = if failures.is_empty() {
            None
        } else {
            // What failed stays dirty; try again after another interval.
            Some(Instant::now())
        };
        for failure in failures {
            match failure.journal {
                Some(journal) => self.failed(journal, failure.error.to_string()),
                None => eprintln!("emx server: {}", failure.error),
            }
        }
    }

    fn failed(&mut self, journal: JournalId, reason: String) {
        eprintln!("emx server: {reason}");
        let now = Instant::now();
        let recent = self
            .reported_at
            .get(&journal)
            .is_some_and(|reported| now < *reported + Duration::from_millis(JOURNAL_RETRY_MS));
        if recent {
            self.held.insert(journal, reason);
            return;
        }
        self.reported_at.insert(journal, now);
        (self.report)(JournalFailure { journal, reason });
    }

    fn report_held(&mut self, now: Instant) {
        let due: Vec<JournalId> = self
            .held
            .keys()
            .copied()
            .filter(|journal| {
                self.reported_at.get(journal).is_none_or(|reported| {
                    now >= *reported + Duration::from_millis(JOURNAL_RETRY_MS)
                })
            })
            .collect();
        for journal in due {
            if let Some(reason) = self.held.remove(&journal) {
                self.reported_at.insert(journal, now);
                (self.report)(JournalFailure { journal, reason });
            }
        }
    }
}
