use std::io;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::RwLock;
use rustix::fs::Advice;

use crate::block_cache::{BLOCK_SIZE, BlockCache, block_length};
use crate::source::ReadError;

const BLOCK_BYTES: u64 = BLOCK_SIZE as u64;
/// A block holds at most `BLOCK_SIZE` newlines, so `u32::MAX` never collides
/// with a real count.
const UNKNOWN: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NthNewline {
    /// File offset of the requested newline.
    Found(u64),
    /// The range holds only `count` newlines.
    NotInRange { count: u64 },
    /// An unindexed block was reached before the newline was found.
    Unknown,
}

/// Where the bytes of an original's blocks come from when a block only partly
/// inside a range has to be scanned.
#[derive(Clone, Copy)]
pub(crate) enum BlockReader<'source> {
    Cache(&'source BlockCache),
    Memory(&'source Arc<[u8]>),
}

impl BlockReader<'_> {
    /// The block's bytes and the offset of the block inside them.
    fn read(&self, block: u64, file_len: u64) -> Result<(Arc<[u8]>, usize), ReadError> {
        let (data, offset) = match self {
            BlockReader::Cache(cache) => (cache.get(block)?, 0),
            BlockReader::Memory(bytes) => ((*bytes).clone(), block_slot(block) * BLOCK_SIZE),
        };
        assert!(
            offset as u64 + block_length(block, file_len) <= data.len() as u64,
            "block {block} holds as many bytes as the index expects"
        );
        Ok((data, offset))
    }
}

/// Fenwick (binary indexed) tree of non-negative values, giving prefix sums
/// and "first index whose prefix exceeds a target" in O(log n).
#[derive(Debug)]
struct Fenwick {
    tree: Vec<u64>,
}

impl Fenwick {
    fn from_values(values: impl Iterator<Item = u64>) -> Self {
        let mut tree = vec![0];
        tree.extend(values);
        let size = tree.len() - 1;
        for index in 1..=size {
            let parent = index + (index & index.wrapping_neg());
            if parent <= size {
                tree[parent] += tree[index];
            }
        }
        Self { tree }
    }

    fn size(&self) -> usize {
        self.tree.len() - 1
    }

    fn add(&mut self, position: usize, delta: i64) {
        let mut index = position + 1;
        while index <= self.size() {
            self.tree[index] = self.tree[index].wrapping_add_signed(delta);
            index += index & index.wrapping_neg();
        }
    }

    /// Sum of values `[0, end)`.
    fn prefix(&self, end: usize) -> u64 {
        let mut index = end;
        let mut total = 0;
        while index > 0 {
            total += self.tree[index];
            index -= index & index.wrapping_neg();
        }
        total
    }

    fn range_sum(&self, range: Range<usize>) -> u64 {
        self.prefix(range.end) - self.prefix(range.start)
    }

    /// Smallest position whose inclusive prefix sum exceeds `target`, or
    /// `size()` when none does.
    fn first_exceeding(&self, target: u64) -> usize {
        let mut position = 0;
        let mut remaining = target;
        let mut step = self.size().checked_next_power_of_two().unwrap_or(0);
        while step > 0 {
            let next = position + step;
            if next <= self.size() && self.tree[next] <= remaining {
                position = next;
                remaining -= self.tree[next];
            }
            step /= 2;
        }
        position
    }
}

#[derive(Debug)]
struct IndexState {
    file_len: u64,
    counts: Vec<u32>,
    /// Newlines per block, with unknown blocks counted as zero.
    newline_sums: Fenwick,
    /// One per unknown block, so "is any block in this range unknown" is O(log n).
    unknown_blocks: Fenwick,
    indexed_prefix: u64,
}

fn known_value(count: u32) -> u64 {
    if count == UNKNOWN {
        0
    } else {
        u64::from(count)
    }
}

fn unknown_value(count: u32) -> u64 {
    u64::from(count == UNKNOWN)
}

impl IndexState {
    fn with_counts(file_len: u64, counts: Vec<u32>) -> Self {
        let mut state = Self {
            file_len,
            newline_sums: Fenwick::from_values(counts.iter().map(|&count| known_value(count))),
            unknown_blocks: Fenwick::from_values(counts.iter().map(|&count| unknown_value(count))),
            counts,
            indexed_prefix: 0,
        };
        state.advance_prefix();
        state
    }

    fn count(&self, block: u64) -> Option<u32> {
        let count = self.counts[block_slot(block)];
        (count != UNKNOWN).then_some(count)
    }

    fn set_count(&mut self, block: u64, count: u32) {
        let slot = block_slot(block);
        let old = self.counts[slot];
        self.counts[slot] = count;
        let newline_delta = known_value(count) as i64 - known_value(old) as i64;
        let unknown_delta = unknown_value(count) as i64 - unknown_value(old) as i64;
        self.newline_sums.add(slot, newline_delta);
        self.unknown_blocks.add(slot, unknown_delta);
    }

    fn advance_prefix(&mut self) {
        while self.indexed_prefix < self.counts.len() as u64
            && self.counts[block_slot(self.indexed_prefix)] != UNKNOWN
        {
            self.indexed_prefix += 1;
        }
    }

    /// First unknown block in `range`, or `range.end` when all are known.
    fn first_unknown(&self, range: Range<usize>) -> usize {
        if self.unknown_blocks.range_sum(range.clone()) == 0 {
            return range.end;
        }
        let before = self.unknown_blocks.prefix(range.start);
        self.unknown_blocks.first_exceeding(before)
    }

    fn span(&self, block: u64, range: &Range<u64>) -> BlockSpan {
        let block_start = block * BLOCK_BYTES;
        let block_end = block_start + block_length(block, self.file_len);
        let start = range.start.max(block_start);
        let end = range.end.min(block_end);
        BlockSpan {
            block,
            start,
            within: (start - block_start) as usize..(end - block_start) as usize,
            covers_block: start == block_start && end == block_end,
        }
    }

    fn span_count(&self, reader: BlockReader<'_>, span: &BlockSpan) -> Result<u64, ReadError> {
        let count = self.count(span.block).expect("span block is indexed");
        if span.covers_block {
            return Ok(u64::from(count));
        }
        let (data, offset) = reader.read(span.block, self.file_len)?;
        Ok(count_newlines(span_bytes(&data, offset, span)))
    }

    /// Looks for newline ordinal `remaining` inside `span`; on a miss returns
    /// how many newlines the span holds.
    fn span_nth(
        &self,
        reader: BlockReader<'_>,
        span: &BlockSpan,
        remaining: u64,
    ) -> Result<Result<u64, u64>, ReadError> {
        let count = u64::from(self.count(span.block).expect("span block is indexed"));
        if span.covers_block && remaining >= count {
            return Ok(Err(count));
        }
        let (data, offset) = reader.read(span.block, self.file_len)?;
        let bytes = span_bytes(&data, offset, span);
        let skip = usize::try_from(remaining).expect("ordinal fits in memory");
        Ok(match memchr::memchr_iter(b'\n', bytes).nth(skip) {
            Some(position) => Ok(span.start + position as u64),
            None => Err(count_newlines(bytes)),
        })
    }
}

/// Newline count per block of the original file, filled in lazily by a
/// background indexer so huge files open before they are fully scanned.
#[derive(Debug)]
pub struct LineIndex {
    state: RwLock<IndexState>,
}

fn block_slot(block: u64) -> usize {
    usize::try_from(block).expect("block number fits in memory")
}

fn blocks_for(file_len: u64) -> u64 {
    file_len.div_ceil(BLOCK_BYTES)
}

fn count_newlines(bytes: &[u8]) -> u64 {
    memchr::memchr_iter(b'\n', bytes).count() as u64
}

/// One block's share of a file range: `within` is relative to the block start.
struct BlockSpan {
    block: u64,
    start: u64,
    within: Range<usize>,
    covers_block: bool,
}

fn span_bytes<'data>(data: &'data [u8], offset: usize, span: &BlockSpan) -> &'data [u8] {
    &data[offset + span.within.start..offset + span.within.end]
}

fn assert_range_inside(range: &Range<u64>, file_len: u64) {
    assert!(
        range.start <= range.end && range.end <= file_len,
        "range {range:?} lies inside the {file_len}-byte file"
    );
}

impl LineIndex {
    pub fn new(file_len: u64) -> Arc<Self> {
        let counts = vec![UNKNOWN; block_slot(blocks_for(file_len))];
        Arc::new(Self {
            state: RwLock::new(IndexState::with_counts(file_len, counts)),
        })
    }

    pub fn fully_indexed(content: &[u8]) -> Arc<Self> {
        let counts: Vec<u32> = content
            .chunks(BLOCK_SIZE)
            .map(|block| count_newlines(block) as u32)
            .collect();
        Arc::new(Self {
            state: RwLock::new(IndexState::with_counts(content.len() as u64, counts)),
        })
    }

    pub fn file_len(&self) -> u64 {
        self.state.read().file_len
    }

    pub fn block_count(&self) -> u64 {
        self.state.read().counts.len() as u64
    }

    /// Panics when `block` is past the end or `newlines` exceeds a block's size.
    pub fn set_block(&self, block: u64, newlines: u32) {
        assert!(
            newlines as usize <= BLOCK_SIZE,
            "a block holds at most BLOCK_SIZE newlines"
        );
        let mut state = self.state.write();
        assert!(
            block < state.counts.len() as u64,
            "block {block} is inside the indexed file"
        );
        state.set_count(block, newlines);
        state.advance_prefix();
    }

    pub fn get_block(&self, block: u64) -> Option<u32> {
        let state = self.state.read();
        if block >= state.counts.len() as u64 {
            return None;
        }
        state.count(block)
    }

    /// Number of blocks indexed contiguously from the start of the file.
    pub fn indexed_prefix_blocks(&self) -> u64 {
        self.state.read().indexed_prefix
    }

    pub fn is_complete(&self) -> bool {
        let state = self.state.read();
        state.indexed_prefix == state.counts.len() as u64
    }

    /// Grows the index after an append. The old last block, if partial, gains
    /// bytes and so loses its count.
    pub fn extend(&self, new_len: u64) {
        let mut state = self.state.write();
        assert!(
            new_len >= state.file_len,
            "extend never shrinks the index ({new_len} < {})",
            state.file_len
        );
        let mut counts = std::mem::take(&mut state.counts);
        if !state.file_len.is_multiple_of(BLOCK_BYTES) {
            counts[block_slot(state.file_len / BLOCK_BYTES)] = UNKNOWN;
        }
        counts.resize(block_slot(blocks_for(new_len)), UNKNOWN);
        *state = IndexState::with_counts(new_len, counts);
    }

    /// Newlines in the whole file, or `None` until every block is indexed.
    pub(crate) fn total_newlines(&self) -> Option<u64> {
        let state = self.state.read();
        if state.indexed_prefix != state.counts.len() as u64 {
            return None;
        }
        Some(state.newline_sums.prefix(state.counts.len()))
    }

    /// Newlines in the file range, or `None` while any block it touches is
    /// unindexed. Blocks only partly inside the range are read through `cache`.
    pub fn count_range(
        &self,
        cache: &BlockCache,
        range: Range<u64>,
    ) -> Result<Option<u64>, ReadError> {
        self.count_range_in(BlockReader::Cache(cache), range)
    }

    /// O(log blocks) plus at most two partial-block scans.
    pub(crate) fn count_range_in(
        &self,
        reader: BlockReader<'_>,
        range: Range<u64>,
    ) -> Result<Option<u64>, ReadError> {
        let state = self.state.read();
        assert_range_inside(&range, state.file_len);
        if range.is_empty() {
            return Ok(Some(0));
        }
        let first = range.start / BLOCK_BYTES;
        let last = (range.end - 1) / BLOCK_BYTES;
        let middle = block_slot(first + 1)..block_slot(last.max(first + 1));
        if state.count(first).is_none()
            || state.count(last).is_none()
            || state.unknown_blocks.range_sum(middle.clone()) > 0
        {
            return Ok(None);
        }
        let mut total = state.span_count(reader, &state.span(first, &range))?;
        if last > first {
            total += state.newline_sums.range_sum(middle);
            total += state.span_count(reader, &state.span(last, &range))?;
        }
        Ok(Some(total))
    }

    /// Finds the newline with 0-based ordinal `n` inside the file range.
    pub fn nth_newline(
        &self,
        cache: &BlockCache,
        range: Range<u64>,
        n: u64,
    ) -> Result<NthNewline, ReadError> {
        self.nth_newline_in(BlockReader::Cache(cache), range, n)
    }

    /// O(log blocks) plus at most three block scans: the two edge blocks and
    /// the block the Fenwick search lands in.
    pub(crate) fn nth_newline_in(
        &self,
        reader: BlockReader<'_>,
        range: Range<u64>,
        n: u64,
    ) -> Result<NthNewline, ReadError> {
        let state = self.state.read();
        assert_range_inside(&range, state.file_len);
        if range.is_empty() {
            return Ok(NthNewline::NotInRange { count: 0 });
        }
        let first = range.start / BLOCK_BYTES;
        let last = (range.end - 1) / BLOCK_BYTES;
        let mut remaining = n;
        let mut seen = 0;

        if state.count(first).is_none() {
            return Ok(NthNewline::Unknown);
        }
        match state.span_nth(reader, &state.span(first, &range), remaining)? {
            Ok(offset) => return Ok(NthNewline::Found(offset)),
            Err(count) => {
                remaining -= count;
                seen += count;
            }
        }
        if last == first {
            return Ok(NthNewline::NotInRange { count: seen });
        }

        let middle_start = block_slot(first + 1);
        let middle_end = block_slot(last);
        let known_end = state.first_unknown(middle_start..middle_end);
        let known = state.newline_sums.range_sum(middle_start..known_end);
        if remaining < known {
            let target = state.newline_sums.prefix(middle_start) + remaining;
            let block = state.newline_sums.first_exceeding(target);
            let skipped = state.newline_sums.range_sum(middle_start..block);
            let span = state.span(block as u64, &range);
            return match state.span_nth(reader, &span, remaining - skipped)? {
                Ok(offset) => Ok(NthNewline::Found(offset)),
                Err(_) => unreachable!("prefix sums place newline {n} in block {block}"),
            };
        }
        remaining -= known;
        seen += known;
        if known_end < middle_end {
            return Ok(NthNewline::Unknown);
        }

        if state.count(last).is_none() {
            return Ok(NthNewline::Unknown);
        }
        match state.span_nth(reader, &state.span(last, &range), remaining)? {
            Ok(offset) => Ok(NthNewline::Found(offset)),
            Err(count) => Ok(NthNewline::NotInRange {
                count: seen + count,
            }),
        }
    }
}

/// Counts newlines in every unindexed block of `cache`'s file. Reads with
/// `pread` directly so a full scan does not evict the blocks being viewed.
/// Returns early, without error, when `cancel` is set.
pub fn index_file(
    cache: &BlockCache,
    index: &LineIndex,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64),
) -> Result<(), ReadError> {
    if index.file_len() != cache.len() {
        return Err(ReadError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "line index covers {} bytes but the block cache expects {}",
                index.file_len(),
                cache.len()
            ),
        )));
    }
    rustix::fs::fadvise(cache.file(), 0, None, Advice::Sequential).map_err(io::Error::from)?;
    let block_count = index.block_count();
    for block in 0..block_count {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        if index.get_block(block).is_none() {
            let data = cache.read_block_direct(block)?;
            index.set_block(block, count_newlines(&data) as u32);
        }
        progress(block + 1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fenwick_prefix_and_search_match_linear() {
        let values: Vec<u64> = (0..37u64).map(|value| (value * 7) % 5).collect();
        let mut fenwick = Fenwick::from_values(values.iter().copied());
        let mut values = values;
        fenwick.add(10, 3);
        values[10] += 3;
        fenwick.add(4, -(values[4] as i64));
        values[4] = 0;
        for end in 0..=values.len() {
            assert_eq!(fenwick.prefix(end), values[..end].iter().sum::<u64>());
        }
        let total: u64 = values.iter().sum();
        for target in 0..total + 2 {
            let expected = (0..values.len())
                .find(|&position| values[..=position].iter().sum::<u64>() > target)
                .unwrap_or(values.len());
            assert_eq!(fenwick.first_exceeding(target), expected, "target {target}");
        }
    }
}
