//! In-place saves: the file is rewritten on its own inode, without extra
//! disk space, as a disk `memmove`. A buffer's original-file pieces always
//! appear in increasing file order, so pieces that move left can be copied
//! first in ascending order and pieces that move right afterwards in
//! descending order (each from its end) without one copy overwriting bytes
//! another still has to read. Pieces that do not move are not rewritten, so
//! a small edit near the end of a huge file writes little. Inserted text is
//! written last and the file is then cut to its new length.
//!
//! This is not crash-safe: a failure part way leaves the file partly
//! rewritten. It is used where a rename would lose hardlinks, ownership or
//! fails for lack of directory access, or when the user chooses it.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;

use emx_text::{BLOCK_SIZE, FileIdentity, Original, PieceSource, Text};

use crate::io::{SaveError, SaveSink, SinkFactory, file_sink, write_text};

/// A file created by an in-place save gets this mode less the umask.
const NEW_FILE_MODE: u32 = 0o666;

/// One step of an in-place save.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    /// Copy the file's bytes `from..from + len` to `to..to + len`.
    Shift { from: u64, to: u64, len: u64 },
    /// Write the buffer's bytes `at..at + len` at the same file offset.
    Write { at: u64, len: u64 },
}

/// The moves that turn a `file_len`-byte file whose content is the buffer's
/// original into the buffer's text: left shifts ascending, right shifts
/// descending, then writes of inserted text. Panics when original pieces
/// are out of file order or reach past `file_len`, which would make the
/// moves overwrite bytes still to be read.
pub fn plan_in_place(text: &Text, file_len: u64) -> Vec<Move> {
    let mut left = Vec::new();
    let mut right = Vec::new();
    let mut writes: Vec<Move> = Vec::new();
    let mut previous_end = 0;
    for (offset, piece) in text.pieces() {
        match piece.source {
            PieceSource::Original => {
                assert!(
                    piece.start >= previous_end,
                    "original pieces appear in increasing file order"
                );
                assert!(
                    piece.start + piece.len <= file_len,
                    "original pieces lie inside the {file_len}-byte file"
                );
                previous_end = piece.start + piece.len;
                let shift = Move::Shift {
                    from: piece.start,
                    to: offset,
                    len: piece.len,
                };
                if offset < piece.start {
                    left.push(shift);
                } else if offset > piece.start {
                    right.push(shift);
                }
            }
            PieceSource::Add => match writes.last_mut() {
                Some(Move::Write { at, len }) if *at + *len == offset => *len += piece.len,
                _ => writes.push(Move::Write {
                    at: offset,
                    len: piece.len,
                }),
            },
        }
    }
    right.reverse();
    left.into_iter().chain(right).chain(writes).collect()
}

/// Carries out `moves` on `target`, then sets its length to the text's and
/// syncs it. The file must still be the `expected` version (`None`: it must
/// not exist), since the moves read its bytes and nothing newer may be
/// overwritten.
pub fn save_in_place(
    target: &Path,
    text: &Text,
    moves: &[Move],
    expected: Option<FileIdentity>,
) -> Result<FileIdentity, SaveError> {
    save_in_place_with(target, text, moves, expected, &mut file_sink)
}

pub fn save_in_place_with(
    target: &Path,
    text: &Text,
    moves: &[Move],
    expected: Option<FileIdentity>,
    sinks: &mut SinkFactory,
) -> Result<FileIdentity, SaveError> {
    let write_error = |source| SaveError::Write {
        path: target.to_path_buf(),
        source,
    };
    let opened = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(expected.is_none())
        .mode(NEW_FILE_MODE)
        .open(target);
    let file = match opened {
        Ok(file) => file,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::AlreadyExists
            ) =>
        {
            return Err(SaveError::Changed(target.to_path_buf()));
        }
        Err(source) => {
            return Err(SaveError::Open {
                path: target.to_path_buf(),
                source,
            });
        }
    };
    let opened = FileIdentity::of_file(&file).map_err(write_error)?;
    if expected.is_some_and(|expected| expected != opened) {
        return Err(SaveError::Changed(target.to_path_buf()));
    }
    if let Original::File(cache) = &text.sources().original {
        let source = cache.identity();
        let same_inode = source.dev == opened.dev && source.ino == opened.ino;
        if same_inode && (source != opened || !writes_only_inserted_text(text, moves)) {
            return Err(SaveError::ReadsTarget(target.to_path_buf()));
        }
    }
    reserve(&file, text.len()).map_err(write_error)?;
    let mut sink = sinks(file.try_clone().map_err(write_error)?);
    rewrite(&file, sink.as_mut(), target, text, moves).map_err(|error| SaveError::PartlyWritten {
        error: Box::new(error),
        path: target.to_path_buf(),
    })
}

/// Everything after the first byte is written: a failure here leaves the
/// file partly rewritten.
fn rewrite(
    file: &File,
    sink: &mut dyn SaveSink,
    target: &Path,
    text: &Text,
    moves: &[Move],
) -> Result<FileIdentity, SaveError> {
    let write_error = |source| SaveError::Write {
        path: target.to_path_buf(),
        source,
    };
    for step in moves {
        match *step {
            Move::Shift { from, to, len } => shift(file, sink, from, to, len, target)?,
            Move::Write { at, len } => {
                sink.seek(at).map_err(write_error)?;
                write_text(sink, text, at..at + len, target)?;
            }
        }
    }
    file.set_len(text.len()).map_err(write_error)?;
    sink.sync().map_err(write_error)?;
    FileIdentity::of_file(file).map_err(write_error)
}

/// No `Write` covers bytes of an original piece, which would be read from
/// the file while it is rewritten.
fn writes_only_inserted_text(text: &Text, moves: &[Move]) -> bool {
    let originals: Vec<(u64, u64)> = text
        .pieces()
        .filter(|(_, piece)| piece.source == PieceSource::Original)
        .map(|(offset, piece)| (offset, offset + piece.len))
        .collect();
    moves.iter().all(|step| match *step {
        Move::Write { at, len } => originals
            .iter()
            .all(|&(start, end)| end <= at || at + len <= start),
        Move::Shift { .. } => true,
    })
}

/// Allocates the space a growing file needs before any byte moves, so a
/// full disk fails the save before the file is touched. Filesystems that
/// cannot preallocate are written without it.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn reserve(file: &File, new_len: u64) -> io::Result<()> {
    let current = file.metadata()?.len();
    if new_len <= current {
        return Ok(());
    }
    match rustix::fs::fallocate(
        file,
        rustix::fs::FallocateFlags::KEEP_SIZE,
        current,
        new_len - current,
    ) {
        Ok(()) => Ok(()),
        Err(errno)
            if errno == rustix::io::Errno::OPNOTSUPP || errno == rustix::io::Errno::NOSYS =>
        {
            Ok(())
        }
        Err(errno) => Err(errno.into()),
    }
}

/// Without `fallocate` (macOS), a growing file is written without
/// preallocation; a full disk then fails the save part way, which the
/// in-place path already reports.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn reserve(_file: &File, _new_len: u64) -> io::Result<()> {
    Ok(())
}

/// Copies `len` bytes within the file in `BLOCK_SIZE` chunks: front to back
/// when moving left, back to front when moving right, so an overlapping
/// copy never reads bytes it already overwrote. Reads bypass the block cache.
fn shift(
    file: &File,
    sink: &mut dyn SaveSink,
    from: u64,
    to: u64,
    len: u64,
    target: &Path,
) -> Result<(), SaveError> {
    let write_error = |source| SaveError::Write {
        path: target.to_path_buf(),
        source,
    };
    let block = BLOCK_SIZE as u64;
    let mut buffer = vec![0u8; BLOCK_SIZE];
    let chunk_count = len.div_ceil(block);
    for index in 0..chunk_count {
        let chunk = if to < from {
            index
        } else {
            chunk_count - 1 - index
        };
        let offset = chunk * block;
        let size = (len - offset).min(block);
        let bytes = &mut buffer[..usize::try_from(size).expect("a chunk is at most one block")];
        file.read_exact_at(bytes, from + offset)
            .map_err(|source| SaveError::Read {
                path: target.to_path_buf(),
                source: source.into(),
            })?;
        sink.seek(to + offset).map_err(write_error)?;
        sink.write_all(bytes).map_err(write_error)?;
    }
    Ok(())
}
