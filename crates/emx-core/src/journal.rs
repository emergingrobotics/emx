//! The edit journal (DESIGN section 2): every buffer edit is appended to a
//! per-buffer file so a crashed session server loses nothing.
//!
//! A journal file is `<journal_dir>/<id>.jnl`, where `id` is the buffer's
//! random 128-bit journal id in 32 lowercase hex digits. It starts with
//! `MAGIC`, followed by records, each framed as
//!
//! ```text
//! u32 LE payload length | u32 LE CRC-32 of (length bytes, payload) | payload
//! ```
//!
//! and each payload is a kind byte followed by little-endian fields:
//!
//! ```text
//! 1 Header          u8 has_path, [u32 path_len, path bytes (raw OS bytes)], name (UTF-8, rest)
//! 2 EpochSnapshot   content (rest)
//! 3 EpochIdentity   u64 dev, u64 ino, u64 size, i128 mtime_ns
//! 4 Insert          u64 at, bytes (rest)
//! 5 Delete          u64 at, u64 len
//! 6 Saved           u64 dev, u64 ino, u64 size, i128 mtime_ns
//! 7 Killed          (nothing)
//! ```
//!
//! A journal is a sequence of epochs; each starts with a base (a snapshot
//! of the text, or the identity of the file the text reads) followed by
//! byte-offset edits. Replay stops at the first frame that is cut short,
//! fails its CRC, or has length zero (every payload has a kind byte; a tail
//! of zeros is what power loss often leaves), which is what a write torn by
//! a crash leaves.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use emx_text::{FileIdentity, Original, PieceSource, Sources, Text};

use crate::buffer::{BufferId, Change, ChangeRecorder};
use crate::config::Config;
use crate::io;

/// Identifies the format and its version; a journal from a different
/// version is reported unreadable rather than misread.
const MAGIC: &[u8; 8] = b"EMXJNL\x00\x02";
const LENGTH_BYTES: usize = 4;
const CRC_BYTES: usize = 4;
const FRAME_HEADER_BYTES: usize = LENGTH_BYTES + CRC_BYTES;
/// Snapshots and inserts above this are written as several records, so a
/// payload length always fits its `u32` with room to spare.
const MAX_RECORD_DATA: usize = 1 << 30;
const DIRECTORY_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;
const GROUP_AND_OTHER_BITS: u32 = 0o077;
const JOURNAL_EXTENSION: &str = "jnl";
/// A restarted journal is written as `<id>.jnl.new` until it replaces the
/// old one; replay reads only `.jnl` files.
const REPLACEMENT_EXTENSION: &str = "new";
const ID_HEX_DIGITS: usize = 32;
const HEX_RADIX: u32 = 16;

const KIND_HEADER: u8 = 1;
const KIND_EPOCH_SNAPSHOT: u8 = 2;
const KIND_EPOCH_IDENTITY: u8 = 3;
const KIND_INSERT: u8 = 4;
const KIND_DELETE: u8 = 5;
const KIND_SAVED: u8 = 6;
const KIND_KILLED: u8 = 7;
const NO_PATH: u8 = 0;
const HAS_PATH: u8 = 1;

/// Why the edits of a journal whose base file changed are listed instead.
pub const CHANGED_ON_DISK: &str = "the file on disk changed after these edits were made";
/// Why the edits of a damaged buffer are listed.
pub const LEFT_PARTLY_REWRITTEN: &str = "an in-place save left the file partly rewritten";

/// A buffer's journal, named by a random 128-bit number so journals from
/// different sessions never collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JournalId(pub u128);

impl JournalId {
    /// Panics only if the kernel's random source fails, which Linux's
    /// `getrandom` does not do for a 16-byte request once booted.
    pub fn random() -> JournalId {
        let mut bytes = [0u8; 16];
        let mut filled = 0;
        while filled < bytes.len() {
            match rustix::rand::getrandom(
                &mut bytes[filled..],
                rustix::rand::GetRandomFlags::empty(),
            ) {
                Ok(count) => filled += count,
                Err(rustix::io::Errno::INTR) => {}
                Err(error) => panic!("the kernel random source failed: {error}"),
            }
        }
        JournalId(u128::from_le_bytes(bytes))
    }

    pub fn file_name(&self) -> String {
        format!(
            "{:0width$x}.{JOURNAL_EXTENSION}",
            self.0,
            width = ID_HEX_DIGITS
        )
    }

    /// The id a journal file is named for, or `None` for any other name.
    pub fn from_file_name(name: &OsStr) -> Option<JournalId> {
        let name = name.to_str()?;
        let hex = name.strip_suffix(&format!(".{JOURNAL_EXTENSION}"))?;
        let canonical = hex.len() == ID_HEX_DIGITS
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if !canonical {
            return None;
        }
        u128::from_str_radix(hex, HEX_RADIX).ok().map(JournalId)
    }
}

/// Which buffer a journal belongs to. Recorded again whenever the buffer
/// is renamed; the last one in the file applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub path: Option<PathBuf>,
    pub name: String,
}

#[derive(Debug, Clone)]
pub enum Record {
    Header(Header),
    /// The text is read when the record is written, off the editor thread;
    /// texts are immutable, so it is the text as of the epoch.
    EpochSnapshot {
        content: Text,
    },
    EpochIdentity {
        identity: FileIdentity,
    },
    Insert {
        at: u64,
        bytes: Vec<u8>,
    },
    Delete {
        at: u64,
        len: u64,
    },
    Saved {
        identity: FileIdentity,
    },
    Killed,
}

/// Snapshots compare by identity of the text, as `AutoSaveJob` does:
/// comparing contents could mean reading a file.
impl PartialEq for Record {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Record::Header(left), Record::Header(right)) => left == right,
            (Record::EpochSnapshot { content: left }, Record::EpochSnapshot { content: right }) => {
                left.same_root(right)
            }
            (
                Record::EpochIdentity { identity: left },
                Record::EpochIdentity { identity: right },
            )
            | (Record::Saved { identity: left }, Record::Saved { identity: right }) => {
                left == right
            }
            (
                Record::Insert {
                    at: left_at,
                    bytes: left_bytes,
                },
                Record::Insert {
                    at: right_at,
                    bytes: right_bytes,
                },
            ) => left_at == right_at && left_bytes == right_bytes,
            (
                Record::Delete {
                    at: left_at,
                    len: left_len,
                },
                Record::Delete {
                    at: right_at,
                    len: right_len,
                },
            ) => left_at == right_at && left_len == right_len,
            (Record::Killed, Record::Killed) => true,
            _ => false,
        }
    }
}

impl Eq for Record {}

/// What the journal writer is asked to do, emitted as `Effect::Journal`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalOp {
    /// Create the journal file, or reopen an existing one, and append the
    /// header; for an open journal, append the header again.
    Open {
        buffer: JournalId,
        header: Header,
    },
    Record {
        buffer: JournalId,
        record: Record,
    },
    /// Delete the journal file: its buffer was saved, killed or discarded.
    Close {
        buffer: JournalId,
    },
}

/// One journaled edit, applied to the text the edits before it left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalEdit {
    Insert { at: u64, bytes: Vec<u8> },
    Delete { at: u64, len: u64 },
}

impl JournalEdit {
    fn to_record(&self) -> Record {
        match self {
            JournalEdit::Insert { at, bytes } => Record::Insert {
                at: *at,
                bytes: bytes.clone(),
            },
            JournalEdit::Delete { at, len } => Record::Delete { at: *at, len: *len },
        }
    }
}

/// One journal file as replay found it.
#[derive(Debug, Clone)]
pub enum RecoveredBuffer {
    /// The buffer as it was at the last complete record. `identity` is the
    /// base file's when the base was the file and it still matches disk.
    Rebuilt {
        journal: JournalId,
        name: String,
        path: Option<PathBuf>,
        identity: Option<FileIdentity>,
        text: Text,
    },
    /// The base file changed, so the text cannot be rebuilt; the edits are
    /// kept so no typed text is lost.
    EditsOnly {
        journal: JournalId,
        name: String,
        path: Option<PathBuf>,
        edits: Vec<JournalEdit>,
    },
    /// Nothing to recover: the buffer was saved or killed, or no epoch was
    /// completed, but the file was not deleted yet.
    Finished { journal: JournalId },
    /// The file is not a journal this version can read; it is left alone.
    Unreadable { file: PathBuf, reason: String },
}

impl PartialEq for RecoveredBuffer {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                RecoveredBuffer::Rebuilt {
                    journal,
                    name,
                    path,
                    identity,
                    text,
                },
                RecoveredBuffer::Rebuilt {
                    journal: other_journal,
                    name: other_name,
                    path: other_path,
                    identity: other_identity,
                    text: other_text,
                },
            ) => {
                journal == other_journal
                    && name == other_name
                    && path == other_path
                    && identity == other_identity
                    && text.same_root(other_text)
            }
            (
                RecoveredBuffer::EditsOnly {
                    journal,
                    name,
                    path,
                    edits,
                },
                RecoveredBuffer::EditsOnly {
                    journal: other_journal,
                    name: other_name,
                    path: other_path,
                    edits: other_edits,
                },
            ) => {
                journal == other_journal
                    && name == other_name
                    && path == other_path
                    && edits == other_edits
            }
            (
                RecoveredBuffer::Finished { journal },
                RecoveredBuffer::Finished {
                    journal: other_journal,
                },
            ) => journal == other_journal,
            (
                RecoveredBuffer::Unreadable { file, reason },
                RecoveredBuffer::Unreadable {
                    file: other_file,
                    reason: other_reason,
                },
            ) => file == other_file && reason == other_reason,
            _ => false,
        }
    }
}

impl Eq for RecoveredBuffer {}

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("reading journal directory {}: {source}", .path.display())]
    ReadDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("writing journal {}: {source}", .path.display())]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("journal {} is not open", .journal.file_name())]
    NotOpen { journal: JournalId },
    /// An earlier write to this journal failed: what follows would leave a
    /// gap, so records are refused until a new epoch restarts it.
    #[error("journal {} lost a write; waiting for a new epoch", .journal.file_name())]
    Poisoned { journal: JournalId },
}

/// The name of the read-only buffer that lists a buffer's edits.
pub fn recovered_buffer_name(name: &str) -> String {
    format!("*recovered: {name}*")
}

/// The text of a `*recovered: NAME*` buffer: every edit with its offset,
/// and inserted text verbatim after a line giving its length, so it can be
/// copied back exactly.
pub fn render_edits(name: &str, reason: &str, edits: &[JournalEdit]) -> Vec<u8> {
    let mut out = format!(
        "Edits to {name} that could not be applied: {reason}.\n\
         Each edit applies to the text the edits before it left; offsets and \
         lengths are in bytes.\n\n"
    )
    .into_bytes();
    for edit in edits {
        match edit {
            JournalEdit::Insert { at, bytes } => {
                out.extend_from_slice(
                    format!("insert at {at}, {} bytes:\n", bytes.len()).as_bytes(),
                );
                out.extend_from_slice(bytes);
                out.push(b'\n');
            }
            JournalEdit::Delete { at, len } => {
                out.extend_from_slice(format!("delete at {at}, {len} bytes\n").as_bytes());
            }
        }
    }
    out
}

// ---- Encoding ------------------------------------------------------------

fn invalid_input(message: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
}

fn push_length(out: &mut Vec<u8>, length: usize) -> std::io::Result<()> {
    let length = u32::try_from(length)
        .map_err(|_| invalid_input(format!("journal field of {length} bytes is too long")))?;
    out.extend_from_slice(&length.to_le_bytes());
    Ok(())
}

fn push_identity(out: &mut Vec<u8>, identity: &FileIdentity) {
    out.extend_from_slice(&identity.dev.to_le_bytes());
    out.extend_from_slice(&identity.ino.to_le_bytes());
    out.extend_from_slice(&identity.size.to_le_bytes());
    out.extend_from_slice(&identity.mtime_ns.to_le_bytes());
}

/// The CRC covers the length too, so a corrupt length is caught instead of
/// framing the rest of the file wrongly.
fn frame_crc(length: &[u8], payload: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(length);
    hasher.update(payload);
    hasher.finalize()
}

fn push_frame(out: &mut Vec<u8>, payload: &[u8]) -> std::io::Result<()> {
    let start = out.len();
    push_length(out, payload.len())?;
    let crc = frame_crc(&out[start..], payload);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

fn insert_payload(at: u64, bytes: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(1 + 8 + bytes.len());
    payload.push(KIND_INSERT);
    payload.extend_from_slice(&at.to_le_bytes());
    payload.extend_from_slice(bytes);
    payload
}

/// The frames for one record; snapshots and inserts above
/// `MAX_RECORD_DATA` become a first record and inserts that continue it.
fn encode(record: &Record) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    match record {
        Record::Header(header) => {
            let mut payload = vec![KIND_HEADER];
            match &header.path {
                Some(path) => {
                    let bytes = path.as_os_str().as_bytes();
                    payload.push(HAS_PATH);
                    push_length(&mut payload, bytes.len())?;
                    payload.extend_from_slice(bytes);
                }
                None => payload.push(NO_PATH),
            }
            payload.extend_from_slice(header.name.as_bytes());
            push_frame(&mut out, &payload)?;
        }
        Record::EpochSnapshot { content } => {
            let length = content.len();
            let chunk = MAX_RECORD_DATA as u64;
            let first = length.min(chunk);
            let mut payload = vec![KIND_EPOCH_SNAPSHOT];
            content
                .read(0..first, &mut payload)
                .map_err(std::io::Error::other)?;
            push_frame(&mut out, &payload)?;
            let mut start = first;
            while start < length {
                let end = (start + chunk).min(length);
                let mut bytes = Vec::new();
                content
                    .read(start..end, &mut bytes)
                    .map_err(std::io::Error::other)?;
                push_frame(&mut out, &insert_payload(start, &bytes))?;
                start = end;
            }
        }
        Record::EpochIdentity { identity } => {
            let mut payload = vec![KIND_EPOCH_IDENTITY];
            push_identity(&mut payload, identity);
            push_frame(&mut out, &payload)?;
        }
        Record::Insert { at, bytes } => {
            let mut offset = *at;
            for chunk in bytes.chunks(MAX_RECORD_DATA) {
                push_frame(&mut out, &insert_payload(offset, chunk))?;
                offset += chunk.len() as u64;
            }
        }
        Record::Delete { at, len } => {
            let mut payload = vec![KIND_DELETE];
            payload.extend_from_slice(&at.to_le_bytes());
            payload.extend_from_slice(&len.to_le_bytes());
            push_frame(&mut out, &payload)?;
        }
        Record::Saved { identity } => {
            let mut payload = vec![KIND_SAVED];
            push_identity(&mut payload, identity);
            push_frame(&mut out, &payload)?;
        }
        Record::Killed => push_frame(&mut out, &[KIND_KILLED])?,
    }
    Ok(out)
}

// ---- Decoding ------------------------------------------------------------

/// The CRC-valid payloads of a journal, in order, and the offset just past
/// the last of them. Reading stops at a frame cut short, failing its CRC,
/// or of length zero.
/// A file that is not a journal at all is an error.
fn scan(bytes: &[u8]) -> Result<(Vec<&[u8]>, usize), String> {
    if bytes.len() < MAGIC.len() {
        // A crash while the magic itself was written.
        return if MAGIC.starts_with(bytes) {
            Ok((Vec::new(), 0))
        } else {
            Err("not an emx journal".to_string())
        };
    }
    if &bytes[..MAGIC.len()] != MAGIC {
        return Err("not an emx journal of this version".to_string());
    }
    let mut payloads = Vec::new();
    let mut position = MAGIC.len();
    while bytes.len() - position >= FRAME_HEADER_BYTES {
        let length_bytes = &bytes[position..position + LENGTH_BYTES];
        let length =
            u32::from_le_bytes(length_bytes.try_into().expect("slice of LENGTH_BYTES")) as usize;
        let crc = u32::from_le_bytes(
            bytes[position + LENGTH_BYTES..position + FRAME_HEADER_BYTES]
                .try_into()
                .expect("slice of CRC_BYTES"),
        );
        let start = position + FRAME_HEADER_BYTES;
        if length == 0 || length > bytes.len() - start {
            break;
        }
        let payload = &bytes[start..start + length];
        if frame_crc(length_bytes, payload) != crc {
            break;
        }
        payloads.push(payload);
        position = start + length;
    }
    Ok((payloads, position))
}

struct Fields<'a> {
    bytes: &'a [u8],
}

impl<'a> Fields<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        if self.bytes.len() < count {
            return Err("record is shorter than its fields".to_string());
        }
        let (head, rest) = self.bytes.split_at(count);
        self.bytes = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("four bytes taken"),
        ))
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("eight bytes taken"),
        ))
    }

    fn i128(&mut self) -> Result<i128, String> {
        Ok(i128::from_le_bytes(
            self.take(16)?.try_into().expect("sixteen bytes taken"),
        ))
    }

    fn identity(&mut self) -> Result<FileIdentity, String> {
        Ok(FileIdentity {
            dev: self.u64()?,
            ino: self.u64()?,
            size: self.u64()?,
            mtime_ns: self.i128()?,
        })
    }

    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.bytes)
    }

    fn finish(&self) -> Result<(), String> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err("record is longer than its fields".to_string())
        }
    }
}

fn decode(payload: &[u8]) -> Result<Record, String> {
    let mut fields = Fields { bytes: payload };
    let record = match fields.u8()? {
        KIND_HEADER => {
            let path = match fields.u8()? {
                NO_PATH => None,
                HAS_PATH => {
                    let length = fields.u32()? as usize;
                    Some(PathBuf::from(OsStr::from_bytes(fields.take(length)?)))
                }
                other => return Err(format!("header has path tag {other}")),
            };
            let name = String::from_utf8(fields.rest().to_vec())
                .map_err(|_| "buffer name is not UTF-8".to_string())?;
            Record::Header(Header { path, name })
        }
        KIND_EPOCH_SNAPSHOT => Record::EpochSnapshot {
            content: Text::from_original(Sources::memory(fields.rest().into())),
        },
        KIND_EPOCH_IDENTITY => Record::EpochIdentity {
            identity: fields.identity()?,
        },
        KIND_INSERT => Record::Insert {
            at: fields.u64()?,
            bytes: fields.rest().to_vec(),
        },
        KIND_DELETE => Record::Delete {
            at: fields.u64()?,
            len: fields.u64()?,
        },
        KIND_SAVED => Record::Saved {
            identity: fields.identity()?,
        },
        KIND_KILLED => Record::Killed,
        other => return Err(format!("unknown record kind {other}")),
    };
    fields.finish()?;
    Ok(record)
}

// ---- Writing -------------------------------------------------------------

#[derive(Debug)]
struct OpenJournal {
    file: File,
    /// The length of the complete records; a failed append is cut back to
    /// it so later records are not hidden behind a torn one.
    length: u64,
    /// The last append failed and may have left part of a frame.
    torn: bool,
    /// Records go to this new file, which replaces the journal once its
    /// epoch is durable (see `start_afresh`).
    replacing: Option<PathBuf>,
}

/// How a journal file's data is made durable; `fdatasync` except where a
/// test injects failures.
pub type SyncData = Box<dyn FnMut(&File) -> std::io::Result<()> + Send>;

struct SyncHook(SyncData);

impl std::fmt::Debug for SyncHook {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SyncHook")
    }
}

/// Data that could not be made durable: a journal's, or (`journal` is
/// `None`) the directory's entries.
#[derive(Debug)]
pub struct SyncFailure {
    pub journal: Option<JournalId>,
    pub error: JournalError,
}

/// Applies journal operations to files. It holds no threads: the session
/// server's journal thread owns it and decides when to sync.
#[derive(Debug)]
pub struct JournalWriter {
    directory: PathBuf,
    journals: HashMap<JournalId, OpenJournal>,
    dirty: BTreeSet<JournalId>,
    directory_dirty: bool,
    /// Journals whose last write failed; see `JournalError::Poisoned`.
    poisoned: BTreeSet<JournalId>,
    /// Journals whose sync failed. After a failed `fsync` Linux may drop
    /// the unwritten pages and report success later, so nothing in the
    /// file is trusted: the next header starts it over.
    afresh: BTreeSet<JournalId>,
    sync_data: SyncHook,
}

impl JournalWriter {
    /// Creates `directory` (mode 0700) if needed. An existing directory must
    /// belong to this user and be closed to everyone else: journals hold
    /// unsaved text.
    pub fn new(directory: &Path) -> std::io::Result<JournalWriter> {
        JournalWriter::with_sync(directory, Box::new(|file: &File| file.sync_data()))
    }

    /// A writer that makes journal data durable with `sync_data`, the seam
    /// tests use to make `fsync` fail.
    pub fn with_sync(directory: &Path, sync_data: SyncData) -> std::io::Result<JournalWriter> {
        prepare_directory(directory)?;
        Ok(JournalWriter {
            directory: directory.to_path_buf(),
            journals: HashMap::new(),
            dirty: BTreeSet::new(),
            directory_dirty: false,
            poisoned: BTreeSet::new(),
            afresh: BTreeSet::new(),
            sync_data: SyncHook(sync_data),
        })
    }

    /// A failure poisons the buffer's journal: its later edit records are
    /// refused, never written after a gap, until the editor starts a new
    /// epoch (`Editor::journal_write_failed`). A header may still be
    /// written; a new epoch clears the poison.
    pub fn apply(&mut self, op: JournalOp) -> Result<(), JournalError> {
        match op {
            JournalOp::Open { buffer, header } => self.guarded(buffer, |writer| {
                if !writer.journals.contains_key(&buffer) {
                    let journal = writer.open_journal(buffer)?;
                    writer.journals.insert(buffer, journal);
                }
                let replacing = writer
                    .journals
                    .get(&buffer)
                    .is_some_and(|journal| journal.replacing.is_some());
                if writer.afresh.contains(&buffer) || replacing {
                    writer.start_afresh(buffer)?;
                }
                writer.append(buffer, &Record::Header(header))
            }),
            JournalOp::Record { buffer, record } => {
                let epoch = matches!(
                    record,
                    Record::EpochSnapshot { .. } | Record::EpochIdentity { .. }
                );
                if self.poisoned.contains(&buffer) && !epoch {
                    return Err(JournalError::Poisoned { journal: buffer });
                }
                self.guarded(buffer, |writer| {
                    writer.append(buffer, &record)?;
                    if epoch {
                        writer.finish_replacement(buffer)?;
                    }
                    Ok(())
                })?;
                if epoch {
                    self.poisoned.remove(&buffer);
                }
                Ok(())
            }
            JournalOp::Close { buffer } => {
                if let Some(replacing) = self
                    .journals
                    .remove(&buffer)
                    .and_then(|journal| journal.replacing)
                {
                    fs::remove_file(&replacing).map_err(|source| JournalError::Write {
                        path: replacing.clone(),
                        source,
                    })?;
                }
                self.dirty.remove(&buffer);
                self.poisoned.remove(&buffer);
                self.afresh.remove(&buffer);
                let path = self.journal_path(buffer);
                fs::remove_file(&path).map_err(|source| JournalError::Write { path, source })?;
                self.directory_dirty = true;
                Ok(())
            }
        }
    }

    /// Makes everything applied so far durable: the journals written to,
    /// and the directory entries created or removed. Fails with the first
    /// failure `sync` reports.
    pub fn flush_and_sync(&mut self) -> Result<(), JournalError> {
        match self.sync().into_iter().next() {
            Some(failure) => Err(failure.error),
            None => Ok(()),
        }
    }

    /// Syncs every journal written to and the directory, and reports each
    /// failure. What failed stays to be synced again. A journal whose sync
    /// failed is poisoned until its buffer starts it afresh
    /// (`Editor::journal_write_failed` emits the header and new epoch).
    pub fn sync(&mut self) -> Vec<SyncFailure> {
        let mut failures = Vec::new();
        for id in self.dirty.clone() {
            let Some(journal) = self.journals.get(&id) else {
                self.dirty.remove(&id);
                continue;
            };
            match (self.sync_data.0)(&journal.file) {
                Ok(()) => {
                    self.dirty.remove(&id);
                }
                Err(source) => {
                    self.poisoned.insert(id);
                    self.afresh.insert(id);
                    failures.push(SyncFailure {
                        journal: Some(id),
                        error: JournalError::Write {
                            path: self.journal_path(id),
                            source,
                        },
                    });
                }
            }
        }
        if self.directory_dirty {
            match File::open(&self.directory).and_then(|directory| directory.sync_all()) {
                Ok(()) => self.directory_dirty = false,
                Err(source) => failures.push(SyncFailure {
                    journal: None,
                    error: JournalError::Write {
                        path: self.directory.clone(),
                        source,
                    },
                }),
            }
        }
        failures
    }

    /// Starts a journal whose sync failed over in a new file beside it.
    /// The old file keeps its records, durable or not, until the new one
    /// holds a header and an epoch on disk (`finish_replacement`): a crash
    /// in between replays the old one.
    fn start_afresh(&mut self, id: JournalId) -> Result<(), JournalError> {
        let temporary = self
            .directory
            .join(format!("{}.{REPLACEMENT_EXTENSION}", id.file_name()));
        let write_error = |source| JournalError::Write {
            path: temporary.clone(),
            source,
        };
        match fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(write_error(error)),
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&temporary)
            .map_err(write_error)?;
        file.write_all(MAGIC).map_err(write_error)?;
        let journal = self
            .journals
            .get_mut(&id)
            .ok_or(JournalError::NotOpen { journal: id })?;
        journal.file = file;
        journal.length = MAGIC.len() as u64;
        journal.torn = false;
        journal.replacing = Some(temporary);
        self.afresh.remove(&id);
        self.dirty.insert(id);
        Ok(())
    }

    /// The new file of a restarted journal holds its epoch: make it durable,
    /// then rename it over the old journal and make the rename durable.
    fn finish_replacement(&mut self, id: JournalId) -> Result<(), JournalError> {
        let path = self.journal_path(id);
        let Some(journal) = self.journals.get_mut(&id) else {
            return Ok(());
        };
        let Some(temporary) = journal.replacing.clone() else {
            return Ok(());
        };
        (self.sync_data.0)(&journal.file).map_err(|source| JournalError::Write {
            path: temporary.clone(),
            source,
        })?;
        fs::rename(&temporary, &path).map_err(|source| JournalError::Write {
            path: path.clone(),
            source,
        })?;
        journal.replacing = None;
        File::open(&self.directory)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| JournalError::Write {
                path: self.directory.clone(),
                source,
            })
    }

    fn journal_path(&self, id: JournalId) -> PathBuf {
        self.directory.join(id.file_name())
    }

    /// Runs one write to journal `id`; a failure poisons it.
    fn guarded(
        &mut self,
        id: JournalId,
        write: impl FnOnce(&mut Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let result = write(self);
        if result.is_err() {
            self.poisoned.insert(id);
        }
        result
    }

    /// Opens the journal for appending. An existing one (a recovered
    /// buffer's) keeps its complete records and loses any torn tail.
    fn open_journal(&mut self, id: JournalId) -> Result<OpenJournal, JournalError> {
        let path = self.journal_path(id);
        self.open_journal_file(id, &path)
            .map_err(|source| JournalError::Write { path, source })
    }

    fn open_journal_file(&mut self, id: JournalId, path: &Path) -> std::io::Result<OpenJournal> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(FILE_MODE)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(path)?;
        let mut existing = Vec::new();
        file.read_to_end(&mut existing)?;
        let (_, end) = scan(&existing).map_err(|reason| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{}: {reason}", path.display()),
            )
        })?;
        let mut length = end as u64;
        file.set_len(length)?;
        file.seek(SeekFrom::Start(length))?;
        if length == 0 {
            file.write_all(MAGIC)?;
            length = MAGIC.len() as u64;
        }
        self.directory_dirty = true;
        self.dirty.insert(id);
        Ok(OpenJournal {
            replacing: None,
            file,
            length,
            torn: false,
        })
    }

    /// Appends after the last complete record. A write that failed part way
    /// is cut back first, so nothing is hidden behind a torn frame.
    fn append(&mut self, id: JournalId, record: &Record) -> Result<(), JournalError> {
        let path = self.journal_path(id);
        let write_error = |source| JournalError::Write {
            path: path.clone(),
            source,
        };
        let journal = self
            .journals
            .get_mut(&id)
            .ok_or(JournalError::NotOpen { journal: id })?;
        let frames = encode(record).map_err(write_error)?;
        if journal.torn {
            journal
                .file
                .set_len(journal.length)
                .and_then(|()| journal.file.seek(SeekFrom::Start(journal.length)))
                .map_err(write_error)?;
            journal.torn = false;
        }
        if let Err(source) = journal.file.write_all(&frames) {
            journal.torn = true;
            return Err(write_error(source));
        }
        journal.length += frames.len() as u64;
        self.dirty.insert(id);
        Ok(())
    }
}

fn prepare_directory(directory: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(directory) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => DirBuilder::new()
            .recursive(true)
            .mode(DIRECTORY_MODE)
            .create(directory)?,
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(directory)?;
    let refuse = |why: &str| {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("journal directory {} {why}", directory.display()),
        ))
    };
    if !metadata.is_dir() {
        return refuse("is not a directory");
    }
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return refuse("belongs to another user");
    }
    if metadata.mode() & GROUP_AND_OTHER_BITS != 0 {
        return refuse("is open to other users; it must be mode 0700");
    }
    Ok(())
}

// ---- Replay --------------------------------------------------------------

/// Rebuilds every buffer journaled in `directory`, in file name order. A
/// missing directory has no journals. A journal that cannot be read is
/// reported, never skipped.
pub fn replay_dir(directory: &Path, config: &Config) -> Result<Vec<RecoveredBuffer>, JournalError> {
    let read_error = |source| JournalError::ReadDirectory {
        path: directory.to_path_buf(),
        source,
    };
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(read_error(error)),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry.map_err(read_error)?.path();
        if path.extension() == Some(OsStr::new(JOURNAL_EXTENSION)) {
            files.push(path);
        }
    }
    files.sort();
    Ok(files.iter().map(|file| replay_file(file, config)).collect())
}

fn replay_file(file: &Path, config: &Config) -> RecoveredBuffer {
    let unreadable = |reason: String| RecoveredBuffer::Unreadable {
        file: file.to_path_buf(),
        reason,
    };
    let Some(journal) = file.file_name().and_then(JournalId::from_file_name) else {
        return unreadable("its name is not a journal id".to_string());
    };
    let bytes = match fs::read(file) {
        Ok(bytes) => bytes,
        Err(error) => return unreadable(io::strerror(&error)),
    };
    rebuild(journal, &bytes, config).unwrap_or_else(unreadable)
}

enum Base {
    Snapshot(Text),
    Identity(FileIdentity),
}

fn rebuild(journal: JournalId, bytes: &[u8], config: &Config) -> Result<RecoveredBuffer, String> {
    let (payloads, _) = scan(bytes)?;
    let mut header: Option<Header> = None;
    let mut base: Option<Base> = None;
    let mut edits = Vec::new();
    let mut finished = false;
    for payload in payloads {
        let record = decode(payload)?;
        if header.is_none() && !matches!(record, Record::Header(_)) {
            return Err("a record comes before the header".to_string());
        }
        match record {
            Record::Header(next) => header = Some(next),
            Record::EpochSnapshot { content } => {
                base = Some(Base::Snapshot(content));
                edits.clear();
                finished = false;
            }
            Record::EpochIdentity { identity } => {
                base = Some(Base::Identity(identity));
                edits.clear();
                finished = false;
            }
            Record::Insert { at, bytes } => {
                if base.is_none() || finished {
                    return Err("an edit comes before its epoch".to_string());
                }
                edits.push(JournalEdit::Insert { at, bytes });
            }
            Record::Delete { at, len } => {
                if base.is_none() || finished {
                    return Err("an edit comes before its epoch".to_string());
                }
                edits.push(JournalEdit::Delete { at, len });
            }
            Record::Saved { .. } | Record::Killed => finished = true,
        }
    }
    let (Some(header), Some(base), false) = (header, base, finished) else {
        return Ok(RecoveredBuffer::Finished { journal });
    };
    match base {
        Base::Snapshot(text) => Ok(RecoveredBuffer::Rebuilt {
            journal,
            name: header.name,
            path: header.path,
            identity: None,
            text: apply_edits(text, &edits)?,
        }),
        Base::Identity(identity) => {
            let path = header
                .path
                .ok_or_else(|| "a file identity base for a buffer without a file".to_string())?;
            match io::load_file(&path, config) {
                Ok(loaded) if loaded.identity == Some(identity) => Ok(RecoveredBuffer::Rebuilt {
                    journal,
                    name: header.name,
                    path: Some(path),
                    identity: Some(identity),
                    text: apply_edits(Text::from_original(loaded.sources), &edits)?,
                }),
                _ => Ok(RecoveredBuffer::EditsOnly {
                    journal,
                    name: header.name,
                    path: Some(path),
                    edits,
                }),
            }
        }
    }
}

/// Journal contents are external input: an edit outside the text is
/// corruption, reported rather than applied.
fn apply_edits(mut text: Text, edits: &[JournalEdit]) -> Result<Text, String> {
    for edit in edits {
        text = match edit {
            JournalEdit::Insert { at, bytes } => {
                if *at > text.len() {
                    return Err(format!("insert at {at} is past the end ({})", text.len()));
                }
                text.insert(*at, bytes)
            }
            JournalEdit::Delete { at, len } => {
                let end = at.checked_add(*len).filter(|end| *end <= text.len());
                let Some(end) = end else {
                    return Err(format!(
                        "delete of {len} at {at} is past the end ({})",
                        text.len()
                    ));
                };
                text.delete(*at..end)
            }
        }
        .map_err(|error| error.to_string())?;
    }
    Ok(text)
}

/// Parses journal bytes the way replay does, without touching the
/// filesystem, so a fuzzer can drive the frame scanner, the record decoder
/// and the edit-application arithmetic on arbitrary input. The identity
/// base is not exercised here because rebuilding it reads the visited file;
/// the snapshot base covers the in-memory edit path. Externally supplied
/// bytes must never panic the parser, only return an error.
#[doc(hidden)]
pub fn parse_for_fuzz(bytes: &[u8]) -> Result<(), String> {
    let (payloads, _) = scan(bytes)?;
    let mut base: Option<Text> = None;
    let mut edits: Vec<JournalEdit> = Vec::new();
    for payload in payloads {
        match decode(payload)? {
            Record::Header(_) => {}
            Record::EpochSnapshot { content } => {
                base = Some(content);
                edits.clear();
            }
            Record::EpochIdentity { .. } => {
                base = None;
                edits.clear();
            }
            Record::Insert { at, bytes } => edits.push(JournalEdit::Insert { at, bytes }),
            Record::Delete { at, len } => edits.push(JournalEdit::Delete { at, len }),
            Record::Saved { .. } | Record::Killed => {}
        }
    }
    if let Some(text) = base {
        let _ = apply_edits(text, &edits)?;
    }
    Ok(())
}

// ---- The editor's side ---------------------------------------------------

/// A buffer's journal as the editor sees it. Nothing is written until the
/// buffer first changes: an unmodified buffer needs no journal, and opening
/// a large file must not copy it.
#[derive(Debug)]
struct BufferJournal {
    id: JournalId,
    header: Header,
    /// The text the current epoch starts from.
    base: Text,
    /// The file exists and holds the current epoch.
    open: bool,
    /// Edits since `base`, kept only when the base reads its file: only such
    /// a buffer can be damaged and need them listed.
    edits: Option<Vec<JournalEdit>>,
    /// Rescued or recovered text that exists nowhere else: ending the
    /// session leaves the journal for the next one.
    rescue: bool,
}

impl BufferJournal {
    fn open(&mut self, ops: &mut Vec<JournalOp>, snapshot_max: u64) {
        if self.open {
            return;
        }
        self.open = true;
        ops.push(JournalOp::Open {
            buffer: self.id,
            header: self.header.clone(),
        });
        self.start_epoch(ops, snapshot_max);
    }

    /// Emits the base of a new epoch for `self.base`: a snapshot, or the
    /// identity of the file it reads plus the edits that turn that file
    /// into the text.
    fn start_epoch(&mut self, ops: &mut Vec<JournalOp>, snapshot_max: u64) {
        let (record, edits) = match &self.base.sources().original {
            Original::File(cache) if self.base.len() > snapshot_max => (
                Record::EpochIdentity {
                    identity: cache.identity(),
                },
                edits_from_original(&self.base, cache.len()),
            ),
            _ => (
                Record::EpochSnapshot {
                    content: self.base.clone(),
                },
                Vec::new(),
            ),
        };
        ops.push(JournalOp::Record {
            buffer: self.id,
            record,
        });
        ops.extend(edits.iter().map(|edit| JournalOp::Record {
            buffer: self.id,
            record: edit.to_record(),
        }));
        self.edits = reads_file(&self.base).then_some(edits);
    }
}

fn reads_file(text: &Text) -> bool {
    matches!(text.sources().original, Original::File(_))
}

/// Edits that turn the original file (`original_len` bytes) into `text`.
fn edits_from_original(text: &Text, original_len: u64) -> Vec<JournalEdit> {
    let mut edits = Vec::new();
    // Bytes of the result built so far, and of the original consumed.
    let mut position = 0;
    let mut consumed = 0;
    for (_, piece) in text.pieces() {
        match piece.source {
            PieceSource::Original => {
                assert!(
                    piece.start >= consumed,
                    "original pieces stay in file order: edits only split and drop them"
                );
                if piece.start > consumed {
                    edits.push(JournalEdit::Delete {
                        at: position,
                        len: piece.start - consumed,
                    });
                }
                position += piece.len;
                consumed = piece.start + piece.len;
            }
            PieceSource::Add => {
                let mut bytes = Vec::new();
                let length = usize::try_from(piece.len).expect("inserted text fits in memory");
                text.sources()
                    .add
                    .read_into(piece.start, length, &mut bytes);
                edits.push(JournalEdit::Insert {
                    at: position,
                    bytes,
                });
                position += piece.len;
            }
        }
    }
    if consumed < original_len {
        edits.push(JournalEdit::Delete {
            at: position,
            len: original_len - consumed,
        });
    }
    edits
}

/// Every change made to every buffer, turned into journal operations that
/// wait for the editor to emit them. It is a separate field so commands can
/// borrow a buffer and the recorder at the same time.
#[derive(Debug)]
pub struct JournalOutbox {
    snapshot_max: u64,
    /// The buffer of each change in the current event, for the command
    /// loop's bookkeeping.
    changed: Vec<BufferId>,
    ops: Vec<JournalOp>,
    journals: HashMap<BufferId, BufferJournal>,
}

impl JournalOutbox {
    pub fn new(snapshot_max: u64) -> JournalOutbox {
        JournalOutbox {
            snapshot_max,
            changed: Vec::new(),
            ops: Vec::new(),
            journals: HashMap::new(),
        }
    }

    /// Changes recorded in the current event.
    pub fn len(&self) -> usize {
        self.changed.len()
    }

    pub fn is_empty(&self) -> bool {
        self.changed.is_empty()
    }

    pub(crate) fn begin_event(&mut self) {
        self.changed.clear();
    }

    pub(crate) fn buffers_since(&self, start: usize) -> impl Iterator<Item = BufferId> + '_ {
        self.changed[start..].iter().copied()
    }

    pub(crate) fn take_ops(&mut self) -> Vec<JournalOp> {
        std::mem::take(&mut self.ops)
    }

    /// Starts journaling `buffer`, whose current text is `base`; the
    /// journal is written once the buffer changes.
    pub(crate) fn track(&mut self, buffer: BufferId, header: Header, base: Text) {
        self.track_as(buffer, JournalId::random(), header, base);
    }

    fn track_as(&mut self, buffer: BufferId, id: JournalId, header: Header, base: Text) {
        let edits = reads_file(&base).then(Vec::new);
        self.journals.insert(
            buffer,
            BufferJournal {
                id,
                header,
                base,
                open: false,
                edits,
                rescue: false,
            },
        );
    }

    /// The buffer holds rescued or recovered text: its journal outlives the
    /// session until the buffer is saved to a file or killed.
    pub(crate) fn mark_rescue(&mut self, buffer: BufferId) {
        if let Some(journal) = self.journals.get_mut(&buffer) {
            journal.rescue = true;
        }
    }

    /// The buffer whose journal is `id`.
    /// Whether the buffer's changes are journaled.
    pub(crate) fn tracks(&self, buffer: BufferId) -> bool {
        self.journals.contains_key(&buffer)
    }

    pub(crate) fn buffer_of(&self, id: JournalId) -> Option<BufferId> {
        self.journals
            .iter()
            .find(|(_, journal)| journal.id == id)
            .map(|(buffer, _)| *buffer)
    }

    /// The writer lost a write to the buffer's journal: start over from
    /// `text` with the header and a new epoch, which the writer accepts.
    pub(crate) fn restart(&mut self, buffer: BufferId, text: Text) {
        let Some(journal) = self.journals.get_mut(&buffer) else {
            return;
        };
        journal.base = text;
        journal.open = true;
        self.ops.push(JournalOp::Open {
            buffer: journal.id,
            header: journal.header.clone(),
        });
        journal.start_epoch(&mut self.ops, self.snapshot_max);
    }

    /// Journals `buffer` in the journal `id` from now on, writing its text
    /// at once: a recovered buffer's text exists nowhere else.
    pub(crate) fn adopt(&mut self, buffer: BufferId, id: JournalId, header: Header, base: Text) {
        self.track_as(buffer, id, header, base);
        self.open(buffer);
    }

    /// Writes the journal now if it is not written yet.
    pub(crate) fn open(&mut self, buffer: BufferId) {
        if let Some(journal) = self.journals.get_mut(&buffer) {
            journal.open(&mut self.ops, self.snapshot_max);
        }
    }

    /// The buffer's text is now `text` by other means than edits (a reload,
    /// its undo, or edits made while a save ran): a new epoch starts.
    pub(crate) fn rebase(&mut self, buffer: BufferId, text: Text) {
        let Some(journal) = self.journals.get_mut(&buffer) else {
            return;
        };
        journal.base = text;
        if journal.open {
            journal.start_epoch(&mut self.ops, self.snapshot_max);
        } else {
            journal.open(&mut self.ops, self.snapshot_max);
        }
    }

    /// The buffer was saved as `identity` and is unmodified with `base`:
    /// its journal is finished and deleted.
    pub(crate) fn saved(&mut self, buffer: BufferId, identity: FileIdentity, base: Text) {
        let Some(journal) = self.journals.get_mut(&buffer) else {
            return;
        };
        if journal.open {
            self.ops.push(JournalOp::Record {
                buffer: journal.id,
                record: Record::Saved { identity },
            });
            self.ops.push(JournalOp::Close { buffer: journal.id });
            journal.open = false;
        }
        journal.rescue = false;
        journal.edits = reads_file(&base).then(Vec::new);
        journal.base = base;
    }

    /// The buffer was reloaded from disk (the `identity` version) and is
    /// unmodified with `base`: like a save, nothing is left to recover, so
    /// any journal is finished. The change still counts as this event's.
    pub(crate) fn reloaded(&mut self, buffer: BufferId, identity: FileIdentity, base: Text) {
        self.changed.push(buffer);
        self.saved(buffer, identity, base);
    }

    /// The buffer was renamed or now visits another file.
    pub(crate) fn set_header(&mut self, buffer: BufferId, header: Header) {
        let Some(journal) = self.journals.get_mut(&buffer) else {
            return;
        };
        journal.header = header;
        if journal.open {
            self.ops.push(JournalOp::Open {
                buffer: journal.id,
                header: journal.header.clone(),
            });
        }
    }

    pub(crate) fn killed(&mut self, buffer: BufferId) {
        if let Some(journal) = self.journals.remove(&buffer)
            && journal.open
        {
            self.ops.push(JournalOp::Record {
                buffer: journal.id,
                record: Record::Killed,
            });
            self.ops.push(JournalOp::Close { buffer: journal.id });
        }
    }

    /// Deletes a journal no buffer uses, such as one replay found finished.
    pub(crate) fn discard(&mut self, id: JournalId) {
        self.ops.push(JournalOp::Close { buffer: id });
    }

    /// The session ends with the user's consent to drop unsaved text.
    /// Rescued text is never dropped this way: its journal stays for the
    /// next session.
    pub(crate) fn close_all(&mut self) {
        let mut open: Vec<JournalId> = self
            .journals
            .values_mut()
            .filter(|journal| journal.open && !journal.rescue)
            .map(|journal| {
                journal.open = false;
                journal.id
            })
            .collect();
        open.sort();
        self.ops
            .extend(open.into_iter().map(|buffer| JournalOp::Close { buffer }));
    }

    /// The buffer's journaled edits since its epoch base, when they are kept.
    pub(crate) fn edits(&self, buffer: BufferId) -> &[JournalEdit] {
        self.journals
            .get(&buffer)
            .and_then(|journal| journal.edits.as_deref())
            .unwrap_or(&[])
    }

    fn edit(&mut self, buffer: BufferId, edit: JournalEdit) {
        let Some(journal) = self.journals.get_mut(&buffer) else {
            return;
        };
        journal.open(&mut self.ops, self.snapshot_max);
        self.ops.push(JournalOp::Record {
            buffer: journal.id,
            record: edit.to_record(),
        });
        if let Some(edits) = journal.edits.as_mut() {
            edits.push(edit);
        }
    }
}

impl ChangeRecorder for JournalOutbox {
    fn record(&mut self, buffer: BufferId, change: &Change) {
        self.changed.push(buffer);
        match change {
            Change::Insert { at, bytes } => self.edit(
                buffer,
                JournalEdit::Insert {
                    at: *at,
                    bytes: bytes.clone(),
                },
            ),
            Change::Delete { at, len } => {
                self.edit(buffer, JournalEdit::Delete { at: *at, len: *len })
            }
            Change::ReplaceAll { text } => self.rebase(buffer, text.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> FileIdentity {
        FileIdentity {
            dev: 1,
            ino: 2,
            size: 3,
            mtime_ns: -4,
        }
    }

    #[test]
    fn records_round_trip() {
        let records = vec![
            Record::Header(Header {
                path: Some(PathBuf::from(OsStr::from_bytes(b"/tmp/\xff name"))),
                name: "name<2>".to_string(),
            }),
            Record::Header(Header {
                path: None,
                name: "*scratch*".to_string(),
            }),
            Record::EpochIdentity {
                identity: identity(),
            },
            Record::Insert {
                at: 7,
                bytes: b"\x00\nbytes".to_vec(),
            },
            Record::Delete { at: 9, len: 10 },
            Record::Saved {
                identity: identity(),
            },
            Record::Killed,
        ];
        let mut bytes = MAGIC.to_vec();
        for record in &records {
            bytes.extend(encode(record).expect("encode"));
        }
        let (payloads, end) = scan(&bytes).expect("scan");
        assert_eq!(end, bytes.len());
        let decoded: Vec<Record> = payloads
            .into_iter()
            .map(|payload| decode(payload).expect("decode"))
            .collect();
        assert_eq!(decoded, records);
    }

    #[test]
    fn snapshot_round_trips_its_content() {
        let content = Text::from_original(Sources::memory(b"snap\nshot".to_vec().into()));
        let mut bytes = MAGIC.to_vec();
        bytes.extend(encode(&Record::EpochSnapshot { content }).expect("encode"));
        let (payloads, _) = scan(&bytes).expect("scan");
        let Record::EpochSnapshot { content } = decode(payloads[0]).expect("decode") else {
            panic!("not a snapshot");
        };
        let mut read = Vec::new();
        content.read(0..content.len(), &mut read).expect("read");
        assert_eq!(read, b"snap\nshot");
    }

    #[test]
    fn decode_rejects_malformed_payloads() {
        assert!(decode(&[]).is_err());
        assert!(decode(&[99]).is_err());
        assert!(decode(&[KIND_DELETE, 1, 2]).is_err());
        let mut long = vec![KIND_KILLED];
        long.push(0);
        assert!(decode(&long).is_err());
        assert!(decode(&[KIND_HEADER, 2]).is_err());
        assert!(decode(&[KIND_HEADER, NO_PATH, 0xff]).is_err());
    }

    #[test]
    fn scan_stops_at_a_torn_or_corrupt_frame() {
        let mut bytes = MAGIC.to_vec();
        bytes.extend(encode(&Record::Killed).expect("encode"));
        let good = bytes.len();
        bytes.extend(encode(&Record::Delete { at: 1, len: 2 }).expect("encode"));
        for cut in good..bytes.len() {
            assert_eq!(scan(&bytes[..cut]).expect("scan").1, good);
        }
        let mut corrupt = bytes.clone();
        corrupt[good + FRAME_HEADER_BYTES] ^= 1;
        assert_eq!(
            scan(&corrupt).expect("scan"),
            (vec![&[KIND_KILLED][..]], good)
        );
        assert_eq!(scan(&MAGIC[..3]).expect("torn magic").1, 0);
        assert!(scan(b"XYZ").is_err());
        assert!(scan(b"EMXJNL\x00\x01").is_err());
    }

    #[test]
    fn file_names_round_trip_and_reject_others() {
        let id = JournalId(0x0123_4567_89ab_cdef_0011_2233_4455_6677);
        let name = id.file_name();
        assert_eq!(name, "0123456789abcdef0011223344556677.jnl");
        assert_eq!(JournalId::from_file_name(OsStr::new(&name)), Some(id));
        for other in [
            "0123456789ABCDEF0011223344556677.jnl",
            "123.jnl",
            "+123456789abcdef0011223344556677.jnl",
            "0123456789abcdef0011223344556677.tmp",
        ] {
            assert_eq!(
                JournalId::from_file_name(OsStr::new(other)),
                None,
                "{other}"
            );
        }
    }

    #[test]
    fn edits_from_original_rebuild_the_text() {
        let original = Text::from_original(Sources::memory(b"0123456789".to_vec().into()));
        let text = original
            .delete(0..2)
            .and_then(|text| text.insert(3, b"abc"))
            .and_then(|text| text.delete(8..10))
            .and_then(|text| text.insert(8, b"Z"))
            .expect("edit");
        let edits = edits_from_original(&text, original.len());
        let rebuilt = apply_edits(original, &edits).expect("apply");
        let mut expected = Vec::new();
        text.read(0..text.len(), &mut expected).expect("read");
        let mut actual = Vec::new();
        rebuilt.read(0..rebuilt.len(), &mut actual).expect("read");
        assert_eq!(actual, expected);
    }

    #[test]
    fn edits_outside_the_text_are_corruption() {
        let text = Text::from_original(Sources::memory(b"abc".to_vec().into()));
        assert!(
            apply_edits(
                text.clone(),
                &[JournalEdit::Insert {
                    at: 4,
                    bytes: b"x".to_vec()
                }]
            )
            .is_err()
        );
        assert!(apply_edits(text.clone(), &[JournalEdit::Delete { at: 2, len: 2 }]).is_err());
        assert!(
            apply_edits(
                text,
                &[JournalEdit::Delete {
                    at: 1,
                    len: u64::MAX
                }]
            )
            .is_err()
        );
    }
}
