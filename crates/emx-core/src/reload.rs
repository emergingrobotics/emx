//! External changes to visited files (DESIGN sections 1 and 3): what a
//! visited file's state can be, how an observed change is classified, and
//! the blocking work a reload needs. The functions that read files run on
//! the server's I/O workers (the test harness calls them synchronously);
//! the editor only applies their results, in `editor/reload.rs`.

use std::fs::{self, File};
use std::io;
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use emx_text::{FileIdentity, LineLookup, ReadError, Text};

use crate::buffer::FileBinding;
use crate::config::Config;
use crate::io::{Loaded, load_file};

/// How far, in lines either way, a reload looks for the line a window's
/// point or start was on before it falls back to the same line number.
pub const ANCHOR_SEARCH_LINES: u64 = 50;
/// Only this much of a very long line is compared when looking for it
/// after a reload: a log line that long is identified by its start.
const ANCHOR_LINE_COMPARE_BYTES: u64 = 64 * 1024;
const UTF8_CONTINUATION_MASK: u8 = 0b1100_0000;
const UTF8_CONTINUATION: u8 = 0b1000_0000;
/// A UTF-8 character has at most this many continuation bytes.
const UTF8_MAX_CONTINUATIONS: u64 = 3;

/// One look at a visited file on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    /// `None` when the file does not exist.
    pub identity: Option<FileIdentity>,
    /// xxh3 of the `append_tail_hash_bytes` before the end the buffer's
    /// version had, when the file is at least that long: unchanged means
    /// the file may only have been appended to.
    pub tail_hash: Option<u64>,
    /// xxh3 of the `append_tail_hash_bytes` before the file's current end.
    /// Settling compares it as well as the identity, which tolerates the
    /// coarse mtimes of network filesystems; after an append it is the
    /// buffer's new tail hash.
    pub end_hash: Option<u64>,
    pub at: Instant,
}

impl Observation {
    /// Two observations a settle window apart that agree mean the writer
    /// has finished.
    pub fn same_version(&self, other: &Observation) -> bool {
        self.identity == other.identity && self.end_hash == other.end_hash
    }
}

/// What the editor knows about the visited file beyond the version the
/// buffer holds.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ExternalState {
    /// The file is the version the buffer was read from or saved to.
    #[default]
    Clean,
    /// The file changed and the editor waits for it to stop changing.
    /// `previous` is the state to return to if it settles back where it
    /// was.
    Settling {
        first_seen: Instant,
        last: Observation,
        previous: Box<ExternalState>,
    },
    /// The buffer is modified and the file changed to `disk`: nothing is
    /// reloaded until the user chooses.
    Conflict {
        disk: FileIdentity,
    },
    Deleted,
    /// The watcher saw the file renamed to this path.
    Renamed(PathBuf),
    /// The user chose to keep the buffer over the `disk` version; saving
    /// over that version does not ask.
    KeepMine {
        disk: FileIdentity,
    },
}

impl ExternalState {
    /// The version last known to be on disk (`None`: no file), given the
    /// version the buffer holds.
    pub fn known_disk(&self, visited: Option<FileIdentity>) -> Option<FileIdentity> {
        match self {
            ExternalState::Clean => visited,
            ExternalState::Settling { previous, .. } => previous.known_disk(visited),
            ExternalState::Conflict { disk } | ExternalState::KeepMine { disk } => Some(*disk),
            ExternalState::Deleted | ExternalState::Renamed(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    Unchanged,
    /// The file is the version emx itself last saved.
    OwnSave,
    Appended {
        new_len: u64,
    },
    Replaced,
    Truncated,
    Deleted,
}

/// Classifies a settled observation against `old`, the version last known
/// to be on disk. An append is the same device and inode, a larger size,
/// and an unchanged hash of the bytes before the old end; anything else
/// that changed is a replacement or a truncation.
pub fn classify(binding: &FileBinding, old: &FileIdentity, obs: &Observation) -> Classification {
    let Some(identity) = obs.identity else {
        return Classification::Deleted;
    };
    if binding.own_save_identity == Some(identity) {
        return Classification::OwnSave;
    }
    if identity == *old {
        return Classification::Unchanged;
    }
    let same_inode = identity.dev == old.dev && identity.ino == old.ino;
    if same_inode && identity.size > old.size && obs.tail_hash == Some(binding.tail_hash) {
        return Classification::Appended {
            new_len: identity.size,
        };
    }
    if same_inode && identity.size < old.size {
        return Classification::Truncated;
    }
    Classification::Replaced
}

/// xxh3 of the last `tail_bytes` of `bytes`.
pub fn tail_hash_of(bytes: &[u8], tail_bytes: u64) -> u64 {
    let start = bytes
        .len()
        .saturating_sub(usize::try_from(tail_bytes).unwrap_or(usize::MAX));
    xxhash_rust::xxh3::xxh3_64(&bytes[start..])
}

/// xxh3 of the `tail_bytes` before the end of `text`.
pub fn text_tail_hash(text: &Text, tail_bytes: u64) -> Result<u64, ReadError> {
    let end = text.len();
    let mut bytes = Vec::new();
    text.read(end.saturating_sub(tail_bytes)..end, &mut bytes)?;
    Ok(xxhash_rust::xxh3::xxh3_64(&bytes))
}

/// xxh3 of the `tail_bytes` of `file` before offset `end`.
fn file_hash_before(file: &File, end: u64, tail_bytes: u64) -> io::Result<u64> {
    let start = end.saturating_sub(tail_bytes);
    let mut bytes = vec![0; usize::try_from(end - start).unwrap_or(usize::MAX)];
    file.read_exact_at(&mut bytes, start)?;
    Ok(xxhash_rust::xxh3::xxh3_64(&bytes))
}

fn describe(path: &Path, error: &io::Error) -> String {
    format!("{}: {}", path.display(), crate::io::strerror(error))
}

/// Looks at `path` for the reload state machine. `old_len` is the size of
/// the version the buffer holds, for the append check. A file that changes
/// while it is looked at gets no hashes, so it cannot look settled.
pub fn observe(path: &Path, old_len: Option<u64>, tail_bytes: u64) -> Result<Observation, String> {
    let at = Instant::now();
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Observation {
                identity: None,
                tail_hash: None,
                end_hash: None,
                at,
            });
        }
        Err(error) => return Err(describe(path, &error)),
    };
    let before = FileIdentity::of_file(&file).map_err(|error| describe(path, &error))?;
    let tail_hash = match old_len {
        Some(old_len) if old_len <= before.size => {
            file_hash_before(&file, old_len, tail_bytes).ok()
        }
        _ => None,
    };
    let end_hash = file_hash_before(&file, before.size, tail_bytes).ok();
    let after = FileIdentity::of_file(&file).map_err(|error| describe(path, &error))?;
    let stable = after == before;
    Ok(Observation {
        identity: Some(before),
        tail_hash: tail_hash.filter(|_| stable),
        end_hash: end_hash.filter(|_| stable),
        at,
    })
}

/// The bytes an append added to a file whose buffer holds them in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendedBytes {
    /// The version the bytes complete.
    pub identity: FileIdentity,
    pub bytes: Vec<u8>,
    /// The tail hash of that version.
    pub end_hash: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppendRead {
    Read(AppendedBytes),
    /// The file is no longer the settled version, or no longer an append
    /// of the buffer's: it has to be observed and classified again.
    Changed,
}

/// Reads only the bytes `new` appended to `old`, after checking again that
/// the file is `new` and the bytes before the old end still hash to
/// `old_tail_hash`.
pub fn read_appended(
    path: &Path,
    old: FileIdentity,
    new: FileIdentity,
    old_tail_hash: u64,
    tail_bytes: u64,
) -> Result<AppendRead, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(AppendRead::Changed),
        Err(error) => return Err(describe(path, &error)),
    };
    let stat = |file: &File| FileIdentity::of_file(file).map_err(|error| describe(path, &error));
    if stat(&file)? != new || new.size < old.size {
        return Ok(AppendRead::Changed);
    }
    let read_error = |error: io::Error| describe(path, &error);
    if file_hash_before(&file, old.size, tail_bytes).map_err(read_error)? != old_tail_hash {
        return Ok(AppendRead::Changed);
    }
    let mut bytes = vec![
        0;
        usize::try_from(new.size - old.size).map_err(|_| {
            format!(
                "{}: the appended part does not fit in memory",
                path.display()
            )
        })?
    ];
    file.read_exact_at(&mut bytes, old.size)
        .map_err(read_error)?;
    let end_hash = file_hash_before(&file, new.size, tail_bytes).map_err(read_error)?;
    if stat(&file)? != new {
        return Ok(AppendRead::Changed);
    }
    Ok(AppendRead::Read(AppendedBytes {
        identity: new,
        bytes,
        end_hash,
    }))
}

/// A file read again to replace its buffer's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reloaded {
    pub loaded: Loaded,
    /// Where each anchor (a window's point or start in the previous text)
    /// goes in the new text, in the order given.
    pub anchors: Vec<u64>,
    /// The start of the line holding the first byte that differs, for
    /// follow-edit mode; `None` when the texts are equal or could not be
    /// compared.
    pub first_change: Option<u64>,
}

/// Reads `path` again for a reload and maps the anchors of `previous` onto
/// the new text by line content.
pub fn reload_file(
    path: &Path,
    config: &Config,
    previous: &Text,
    anchors: &[u64],
) -> Result<Reloaded, String> {
    let loaded = load_file(path, config).map_err(|error| error.to_string())?;
    let new = Text::from_original(loaded.sources.clone());
    let anchors = anchors
        .iter()
        .map(|&position| {
            // Text that can no longer be read (its file was rewritten in
            // place) keeps the byte offset.
            map_anchor(previous, &new, position).unwrap_or_else(|_| position.min(new.len()))
        })
        .collect();
    let first_change = first_difference(previous, &new)
        .ok()
        .flatten()
        .and_then(|offset| line_start_at(&new, offset).ok());
    Ok(Reloaded {
        loaded,
        anchors,
        first_change,
    })
}

/// Offset of the first byte where the texts differ (or where the shorter
/// ends), `None` when they are equal.
pub fn first_difference(left: &Text, right: &Text) -> Result<Option<u64>, ReadError> {
    let mut right_chunks = right.chunks(0..right.len());
    let mut pending: Vec<u8> = Vec::new();
    let mut pending_start = 0;
    let mut position = 0;
    for chunk in left.chunks(0..left.len()) {
        let chunk = chunk?;
        let mut bytes = chunk.bytes();
        while !bytes.is_empty() {
            if pending_start == pending.len() {
                let Some(next) = right_chunks.next() else {
                    return Ok(Some(position));
                };
                pending.clear();
                pending.extend_from_slice(next?.bytes());
                pending_start = 0;
            }
            let count = bytes.len().min(pending.len() - pending_start);
            let same = bytes[..count]
                .iter()
                .zip(&pending[pending_start..pending_start + count])
                .position(|(a, b)| a != b);
            if let Some(index) = same {
                return Ok(Some(position + index as u64));
            }
            bytes = &bytes[count..];
            pending_start += count;
            position += count as u64;
        }
    }
    Ok((left.len() != right.len()).then_some(position))
}

fn line_start_at(text: &Text, position: u64) -> Result<u64, ReadError> {
    let position = position.min(text.len());
    Ok(text
        .find_newline_backward(position)?
        .map_or(0, |newline| newline + 1))
}

fn line_range_at(text: &Text, start: u64) -> Result<Range<u64>, ReadError> {
    let end = text.find_newline_forward(start)?.unwrap_or(text.len());
    Ok(start..end)
}

/// The compared part of a line: lines are equal when their lengths and
/// first `ANCHOR_LINE_COMPARE_BYTES` are.
fn line_key(text: &Text, range: &Range<u64>) -> Result<(u64, Vec<u8>), ReadError> {
    let mut bytes = Vec::new();
    let end = range.end.min(range.start + ANCHOR_LINE_COMPARE_BYTES);
    text.read(range.start..end, &mut bytes)?;
    Ok((range.end - range.start, bytes))
}

/// The line of `new` that stands where the anchor's line stood: the same
/// line number when both texts know their line numbers, else the line at
/// the same byte offset.
fn center_line(old: &Text, new: &Text, old_start: u64) -> Result<u64, ReadError> {
    if let Some(number) = old.line_of(old_start)? {
        match new.line_start(number)? {
            LineLookup::Found(start) if start <= new.len() => return Ok(start),
            LineLookup::PastEnd => return line_start_at(new, new.len()),
            _ => {}
        }
    }
    line_start_at(new, old_start)
}

/// Where position `pos` of `old` goes in `new`: on the line with the same
/// text within `ANCHOR_SEARCH_LINES` lines of the old line number, nearest
/// first, else on the same line number (clamped); at the same column,
/// clamped to the line and kept off the middle of a UTF-8 character.
pub fn map_anchor(old: &Text, new: &Text, pos: u64) -> Result<u64, ReadError> {
    let pos = pos.min(old.len());
    let old_start = line_start_at(old, pos)?;
    let old_line = line_range_at(old, old_start)?;
    let column = pos - old_start;
    let key = line_key(old, &old_line)?;
    let center = line_range_at(new, center_line(old, new, old_start)?)?;
    let mut before = Vec::new();
    let mut cursor = center.start;
    while (before.len() as u64) < ANCHOR_SEARCH_LINES && cursor > 0 {
        let start = line_start_at(new, cursor - 1)?;
        before.push(start..cursor - 1);
        cursor = start;
    }
    let mut after = Vec::new();
    let mut cursor = center.end;
    while (after.len() as u64) < ANCHOR_SEARCH_LINES && cursor < new.len() {
        let line = line_range_at(new, cursor + 1)?;
        cursor = line.end;
        after.push(line);
    }
    let mut candidates = vec![center.clone()];
    for distance in 0..ANCHOR_SEARCH_LINES as usize {
        candidates.extend(before.get(distance).cloned());
        candidates.extend(after.get(distance).cloned());
    }
    let mut chosen = center;
    for candidate in candidates {
        if line_key(new, &candidate)? == key {
            chosen = candidate;
            break;
        }
    }
    let mut position = chosen.start + column.min(chosen.end - chosen.start);
    let mut steps = 0;
    while position > chosen.start
        && position < new.len()
        && steps < UTF8_MAX_CONTINUATIONS
        && new.byte_at(position)? & UTF8_CONTINUATION_MASK == UTF8_CONTINUATION
    {
        position -= 1;
        steps += 1;
    }
    Ok(position)
}

/// Reads the file whole for `diff-external`, refusing one larger than
/// `limit` bytes.
pub fn read_for_diff(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let metadata = fs::metadata(path).map_err(|error| describe(path, &error))?;
    if metadata.len() > limit {
        return Err(format!(
            "{} is too large to diff (over {limit} bytes)",
            path.display()
        ));
    }
    fs::read(path).map_err(|error| describe(path, &error))
}
