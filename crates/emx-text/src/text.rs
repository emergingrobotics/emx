use std::ops::Range;
use std::sync::Arc;

use crate::block_cache::BLOCK_SIZE;
use crate::line_index::NthNewline;
use crate::piece_tree::{
    self, Piece, PieceSource, Pieces, PiecesBackward, Tree, last_piece, leaf, merge,
    replace_last_piece, split, summary,
};
use crate::source::{Original, ReadError, Sources};

const BLOCK_BYTES: u64 = BLOCK_SIZE as u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineLookup {
    Found(u64),
    PastEnd,
    /// A region that must be scanned to answer is not indexed yet.
    Unknown,
}

/// A contiguous run of buffer bytes. Original-file chunks share the cached
/// block or in-memory original instead of copying it.
#[derive(Debug, Clone)]
pub struct Chunk {
    /// Buffer offset of the first byte.
    pub start: u64,
    data: Arc<[u8]>,
    range: Range<usize>,
}

impl Chunk {
    pub fn bytes(&self) -> &[u8] {
        &self.data[self.range.clone()]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fetch {
    Io,
    CachedOnly,
}

/// Outcome of looking for the newline with a given ordinal in part of a text.
enum Nth {
    Found(u64),
    NotFound { count: u64 },
    Unknown,
}

/// Persistent text: every edit returns a new `Text` sharing unchanged
/// structure with the old one, so snapshots for undo, saving and rendering
/// cost nothing to keep.
#[derive(Clone)]
pub struct Text {
    root: Tree,
    sources: Arc<Sources>,
}

impl std::fmt::Debug for Text {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Text")
            .field("len", &self.len())
            .field("pieces", &self.piece_count())
            .field("newlines", &summary(&self.root).newlines)
            .finish()
    }
}

fn is_file_backed(sources: &Sources, piece: &Piece) -> bool {
    piece.source == PieceSource::Original && matches!(sources.original, Original::File(_))
}

fn memory_range(piece: &Piece, within: Range<u64>) -> Range<usize> {
    let start = usize::try_from(piece.start + within.start).expect("memory offset fits");
    let end = usize::try_from(piece.start + within.end).expect("memory offset fits");
    start..end
}

/// Newlines in `piece`, or `None` while its original-file region is unindexed.
fn count_piece(sources: &Sources, piece: &Piece) -> Result<Option<u64>, ReadError> {
    match (piece.source, &sources.original) {
        (PieceSource::Add, _) => Ok(Some(sources.add.count_newlines(piece.start, piece.len))),
        (PieceSource::Original, Original::Memory(bytes)) => {
            let range = memory_range(piece, 0..piece.len);
            Ok(Some(
                memchr::memchr_iter(b'\n', &bytes[range]).count() as u64
            ))
        }
        (PieceSource::Original, Original::File(cache)) => sources
            .index
            .count_range(cache, piece.start..piece.start + piece.len),
    }
}

/// End (piece-relative) of the segment starting at `offset`: file-backed
/// segments stop at block boundaries so each maps onto one cached block.
fn segment_end(sources: &Sources, piece: &Piece, offset: u64, limit: u64) -> u64 {
    if !is_file_backed(sources, piece) {
        return limit;
    }
    let block_end = ((piece.start + offset) / BLOCK_BYTES + 1) * BLOCK_BYTES;
    limit.min(block_end - piece.start)
}

/// Start (piece-relative) of the segment ending at `end`.
fn segment_start(sources: &Sources, piece: &Piece, end: u64) -> u64 {
    if !is_file_backed(sources, piece) {
        return 0;
    }
    let block_start = (piece.start + end - 1) / BLOCK_BYTES * BLOCK_BYTES;
    block_start.saturating_sub(piece.start)
}

/// Reads piece bytes `within`, which must not cross a block boundary for
/// file-backed pieces.
fn read_segment(
    sources: &Sources,
    piece: &Piece,
    within: Range<u64>,
    fetch: Fetch,
) -> Result<(Arc<[u8]>, Range<usize>), ReadError> {
    let length = usize::try_from(within.end - within.start).expect("segment fits in memory");
    match (piece.source, &sources.original) {
        (PieceSource::Add, _) => {
            let mut copy = Vec::with_capacity(length);
            sources
                .add
                .read_into(piece.start + within.start, length, &mut copy);
            Ok((Arc::from(copy), 0..length))
        }
        (PieceSource::Original, Original::Memory(bytes)) => {
            Ok((bytes.clone(), memory_range(piece, within)))
        }
        (PieceSource::Original, Original::File(cache)) => {
            let file_offset = piece.start + within.start;
            let block = file_offset / BLOCK_BYTES;
            let data = match fetch {
                Fetch::Io => cache.get(block)?,
                Fetch::CachedOnly => cache
                    .try_get_cached(block)
                    .ok_or(ReadError::NotCached { block })?,
            };
            let start = (file_offset - block * BLOCK_BYTES) as usize;
            assert!(
                start + length <= data.len(),
                "piece bytes lie inside block {block}"
            );
            Ok((data, start..start + length))
        }
    }
}

/// Iterates the chunks of a buffer range in order. After an error it yields
/// nothing more.
pub struct ChunkIter {
    sources: Arc<Sources>,
    pieces: Pieces,
    current: Option<(u64, Piece)>,
    position: u64,
    end: u64,
    fetch: Fetch,
}

impl Iterator for ChunkIter {
    type Item = Result<Chunk, ReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.position >= self.end {
            return None;
        }
        let (piece_offset, piece) = match self.current {
            Some((offset, piece)) if self.position < offset + piece.len => (offset, piece),
            _ => {
                let next = self.pieces.next().expect("pieces cover the whole text");
                self.current = Some(next);
                next
            }
        };
        let offset = self.position - piece_offset;
        let limit = (self.end - piece_offset).min(piece.len);
        let end = segment_end(&self.sources, &piece, offset, limit);
        match read_segment(&self.sources, &piece, offset..end, self.fetch) {
            Ok((data, range)) => {
                let chunk = Chunk {
                    start: self.position,
                    data,
                    range,
                };
                self.position += end - offset;
                Some(Ok(chunk))
            }
            Err(error) => {
                self.position = self.end;
                Some(Err(error))
            }
        }
    }
}

impl Text {
    pub fn empty(sources: Arc<Sources>) -> Text {
        Text {
            root: None,
            sources,
        }
    }

    /// The whole original as one piece.
    pub fn from_original(sources: Arc<Sources>) -> Text {
        let len = sources.original.len();
        if len == 0 {
            return Text::empty(sources);
        }
        let newlines = match &sources.original {
            Original::Memory(bytes) => Some(memchr::memchr_iter(b'\n', bytes).count() as u64),
            Original::File(_) => sources.index.total_newlines(),
        };
        let root = leaf(Piece {
            source: PieceSource::Original,
            start: 0,
            len,
            newlines,
        });
        Text { root, sources }
    }

    pub fn sources(&self) -> &Arc<Sources> {
        &self.sources
    }

    pub fn len(&self) -> u64 {
        summary(&self.root).bytes
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    pub fn piece_count(&self) -> u64 {
        summary(&self.root).pieces
    }

    /// Newline count plus one, or `None` while any piece is unindexed.
    pub fn line_count(&self) -> Option<u64> {
        summary(&self.root).newlines.map(|newlines| newlines + 1)
    }

    fn assert_range(&self, range: &Range<u64>) {
        assert!(
            range.start <= range.end && range.end <= self.len(),
            "range {range:?} lies inside the {}-byte text",
            self.len()
        );
    }

    fn split_at(&self, tree: &Tree, at: u64) -> Result<(Tree, Tree), ReadError> {
        let sources = &self.sources;
        split(tree, at, &mut |piece| count_piece(sources, piece))
    }

    /// Panics when `at` is past the end.
    pub fn insert(&self, at: u64, bytes: &[u8]) -> Result<Text, ReadError> {
        self.assert_range(&(at..at));
        if bytes.is_empty() {
            return Ok(self.clone());
        }
        let (head, tail) = self.split_at(&self.root, at)?;
        let length = bytes.len() as u64;
        let newlines = memchr::memchr_iter(b'\n', bytes).count() as u64;
        let add = &self.sources.add;
        let add_len_before = add.len();
        let offset = add.append(bytes);
        let head = match last_piece(&head) {
            // Typing appends to the add store right after the previous
            // keystroke's bytes, so extending that piece keeps the tree from
            // growing one node per character.
            Some(previous)
                if previous.source == PieceSource::Add
                    && previous.start + previous.len == add_len_before
                    && offset == add_len_before =>
            {
                let extended = Piece {
                    len: previous.len + length,
                    newlines: previous.newlines.map(|count| count + newlines),
                    ..previous
                };
                replace_last_piece(&head, extended)
            }
            _ => merge(
                &head,
                &leaf(Piece {
                    source: PieceSource::Add,
                    start: offset,
                    len: length,
                    newlines: Some(newlines),
                }),
            ),
        };
        Ok(Text {
            root: merge(&head, &tail),
            sources: self.sources.clone(),
        })
    }

    /// Panics when `range` is not inside the text.
    pub fn delete(&self, range: Range<u64>) -> Result<Text, ReadError> {
        self.assert_range(&range);
        if range.is_empty() {
            return Ok(self.clone());
        }
        let (head, rest) = self.split_at(&self.root, range.start)?;
        let (_, tail) = self.split_at(&rest, range.end - range.start)?;
        Ok(Text {
            root: merge(&head, &tail),
            sources: self.sources.clone(),
        })
    }

    fn chunks_with(&self, range: Range<u64>, fetch: Fetch) -> ChunkIter {
        self.assert_range(&range);
        ChunkIter {
            sources: self.sources.clone(),
            pieces: Pieces::starting_at(&self.root, range.start),
            current: None,
            position: range.start,
            end: range.end,
            fetch,
        }
    }

    pub fn chunks(&self, range: Range<u64>) -> ChunkIter {
        self.chunks_with(range, Fetch::Io)
    }

    fn read_with(
        &self,
        range: Range<u64>,
        out: &mut Vec<u8>,
        fetch: Fetch,
    ) -> Result<(), ReadError> {
        let original_len = out.len();
        for chunk in self.chunks_with(range, fetch) {
            match chunk {
                Ok(chunk) => out.extend_from_slice(chunk.bytes()),
                Err(error) => {
                    out.truncate(original_len);
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// Appends the bytes of `range` to `out`; on error `out` is left unchanged.
    pub fn read(&self, range: Range<u64>, out: &mut Vec<u8>) -> Result<(), ReadError> {
        self.read_with(range, out, Fetch::Io)
    }

    /// Like `read`, but fails with `NotCached` rather than reading the file,
    /// for callers (rendering) that must never block on I/O.
    pub fn read_cached(&self, range: Range<u64>, out: &mut Vec<u8>) -> Result<(), ReadError> {
        self.read_with(range, out, Fetch::CachedOnly)
    }

    /// Panics when `pos` is not before the end.
    pub fn byte_at(&self, pos: u64) -> Result<u8, ReadError> {
        assert!(pos < self.len(), "byte_at({pos}) is before the end");
        let chunk = self
            .chunks(pos..pos + 1)
            .next()
            .expect("a one-byte range yields a chunk")?;
        Ok(chunk.bytes()[0])
    }

    /// Offset of the first newline at or after `from`.
    pub fn find_newline_forward(&self, from: u64) -> Result<Option<u64>, ReadError> {
        self.assert_range(&(from..from));
        for (piece_offset, piece) in Pieces::starting_at(&self.root, from) {
            if piece.newlines == Some(0) {
                continue;
            }
            let mut offset = from.saturating_sub(piece_offset);
            while offset < piece.len {
                let end = segment_end(&self.sources, &piece, offset, piece.len);
                let (data, range) = read_segment(&self.sources, &piece, offset..end, Fetch::Io)?;
                if let Some(position) = memchr::memchr(b'\n', &data[range]) {
                    return Ok(Some(piece_offset + offset + position as u64));
                }
                offset = end;
            }
        }
        Ok(None)
    }

    /// Offset of the last newline strictly before `before`.
    pub fn find_newline_backward(&self, before: u64) -> Result<Option<u64>, ReadError> {
        self.assert_range(&(before..before));
        for (piece_offset, piece) in PiecesBackward::before(&self.root, before) {
            if piece.newlines == Some(0) {
                continue;
            }
            let mut end = (before - piece_offset).min(piece.len);
            while end > 0 {
                let start = segment_start(&self.sources, &piece, end);
                let (data, range) = read_segment(&self.sources, &piece, start..end, Fetch::Io)?;
                if let Some(position) = memchr::memrchr(b'\n', &data[range]) {
                    return Ok(Some(piece_offset + start + position as u64));
                }
                end = start;
            }
        }
        Ok(None)
    }

    /// Offset where 0-based `line` starts.
    pub fn line_start(&self, line: u64) -> Result<LineLookup, ReadError> {
        if line == 0 {
            return Ok(LineLookup::Found(0));
        }
        Ok(match self.nth_in_tree(&self.root, line - 1)? {
            Nth::Found(offset) => LineLookup::Found(offset + 1),
            Nth::NotFound { .. } => LineLookup::PastEnd,
            Nth::Unknown => LineLookup::Unknown,
        })
    }

    fn nth_in_tree(&self, tree: &Tree, n: u64) -> Result<Nth, ReadError> {
        let Some(node) = tree else {
            return Ok(Nth::NotFound { count: 0 });
        };
        if let Some(total) = node.summary.newlines
            && n >= total
        {
            return Ok(Nth::NotFound { count: total });
        }
        let mut remaining = n;
        let left_bytes = summary(&node.left).bytes;
        match self.nth_in_tree(&node.left, remaining)? {
            Nth::Found(offset) => return Ok(Nth::Found(offset)),
            Nth::NotFound { count } => remaining -= count,
            Nth::Unknown => return Ok(Nth::Unknown),
        }
        match self.nth_in_piece(&node.piece, remaining)? {
            Nth::Found(offset) => return Ok(Nth::Found(left_bytes + offset)),
            Nth::NotFound { count } => remaining -= count,
            Nth::Unknown => return Ok(Nth::Unknown),
        }
        let right_base = left_bytes + node.piece.len;
        Ok(match self.nth_in_tree(&node.right, remaining)? {
            Nth::Found(offset) => Nth::Found(right_base + offset),
            Nth::NotFound { count } => Nth::NotFound {
                count: n - remaining + count,
            },
            Nth::Unknown => Nth::Unknown,
        })
    }

    fn nth_in_piece(&self, piece: &Piece, n: u64) -> Result<Nth, ReadError> {
        if let Some(count) = piece.newlines
            && n >= count
        {
            return Ok(Nth::NotFound { count });
        }
        let found = match (piece.source, &self.sources.original) {
            (PieceSource::Add, _) => self.sources.add.nth_newline(piece.start, piece.len, n),
            (PieceSource::Original, Original::Memory(bytes)) => {
                let range = memory_range(piece, 0..piece.len);
                let skip = usize::try_from(n).expect("ordinal below a memory piece's count");
                memchr::memchr_iter(b'\n', &bytes[range])
                    .nth(skip)
                    .map(|position| position as u64)
            }
            (PieceSource::Original, Original::File(cache)) => {
                let file_range = piece.start..piece.start + piece.len;
                return Ok(
                    match self.sources.index.nth_newline(cache, file_range, n)? {
                        NthNewline::Found(file_offset) => Nth::Found(file_offset - piece.start),
                        NthNewline::NotInRange { count } => Nth::NotFound { count },
                        NthNewline::Unknown => Nth::Unknown,
                    },
                );
            }
        };
        let found = found.expect("memory and add pieces hold their counted newlines");
        Ok(Nth::Found(found))
    }

    /// Number of newlines strictly before `pos`, or `None` while unindexed.
    pub fn line_of(&self, pos: u64) -> Result<Option<u64>, ReadError> {
        self.assert_range(&(pos..pos));
        self.prefix_newlines(&self.root, pos)
    }

    fn prefix_newlines(&self, tree: &Tree, pos: u64) -> Result<Option<u64>, ReadError> {
        let Some(node) = tree else {
            return Ok(Some(0));
        };
        if pos == 0 {
            return Ok(Some(0));
        }
        if pos >= node.summary.bytes
            && let Some(total) = node.summary.newlines
        {
            return Ok(Some(total));
        }
        let left_bytes = summary(&node.left).bytes;
        if pos <= left_bytes {
            return self.prefix_newlines(&node.left, pos);
        }
        let Some(left) = self.prefix_newlines(&node.left, left_bytes)? else {
            return Ok(None);
        };
        let within = (pos - left_bytes).min(node.piece.len);
        let piece_count = match node.piece.newlines {
            Some(count) if within == node.piece.len => Some(count),
            _ => count_piece(
                &self.sources,
                &Piece {
                    len: within,
                    newlines: None,
                    ..node.piece
                },
            )?,
        };
        let Some(piece_count) = piece_count else {
            return Ok(None);
        };
        let right_base = left_bytes + node.piece.len;
        if pos <= right_base {
            return Ok(Some(left + piece_count));
        }
        Ok(self
            .prefix_newlines(&node.right, pos - right_base)?
            .map(|right| left + piece_count + right))
    }

    /// Fills unknown piece newline counts from the line index as it
    /// progresses. A piece whose count cannot be computed yet (unindexed, or
    /// its partial edge block unreadable) stays unknown: the read error itself
    /// surfaces through the read paths, and the next refresh retries.
    pub fn refresh_newlines(&self) -> Text {
        let sources = &self.sources;
        let root = piece_tree::refresh(&self.root, &mut |piece| {
            count_piece(sources, piece).ok().flatten()
        });
        Text {
            root,
            sources: self.sources.clone(),
        }
    }

    pub fn pieces(&self) -> impl Iterator<Item = (u64, Piece)> + use<> {
        Pieces::starting_at(&self.root, 0)
    }

    /// True when both texts are the same version (pointer equality of roots).
    pub fn same_root(&self, other: &Text) -> bool {
        match (&self.root, &other.root) {
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            (None, None) => true,
            _ => false,
        }
    }
}
