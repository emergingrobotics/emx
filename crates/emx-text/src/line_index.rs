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

#[derive(Debug)]
struct IndexState {
    file_len: u64,
    counts: Vec<u32>,
    indexed_prefix: u64,
}

impl IndexState {
    fn count(&self, block: u64) -> Option<u32> {
        let count = self.counts[block_slot(block)];
        (count != UNKNOWN).then_some(count)
    }

    fn advance_prefix(&mut self) {
        while self.indexed_prefix < self.counts.len() as u64
            && self.counts[block_slot(self.indexed_prefix)] != UNKNOWN
        {
            self.indexed_prefix += 1;
        }
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

/// One block's share of a file range: offsets are relative to the block start.
struct BlockSpan {
    block: u64,
    start: u64,
    within: Range<usize>,
    covers_block: bool,
}

fn block_spans(range: &Range<u64>, file_len: u64) -> impl Iterator<Item = BlockSpan> {
    let first = range.start / BLOCK_BYTES;
    let end_block = if range.is_empty() {
        first
    } else {
        (range.end - 1) / BLOCK_BYTES + 1
    };
    let range = range.clone();
    (first..end_block).map(move |block| {
        let block_start = block * BLOCK_BYTES;
        let block_end = block_start + block_length(block, file_len);
        let start = range.start.max(block_start);
        let end = range.end.min(block_end);
        BlockSpan {
            block,
            start,
            within: (start - block_start) as usize..(end - block_start) as usize,
            covers_block: start == block_start && end == block_end,
        }
    })
}

impl LineIndex {
    pub fn new(file_len: u64) -> Arc<Self> {
        Arc::new(Self {
            state: RwLock::new(IndexState {
                file_len,
                counts: vec![UNKNOWN; block_slot(blocks_for(file_len))],
                indexed_prefix: 0,
            }),
        })
    }

    pub fn fully_indexed(content: &[u8]) -> Arc<Self> {
        let counts: Vec<u32> = content
            .chunks(BLOCK_SIZE)
            .map(|block| count_newlines(block) as u32)
            .collect();
        let indexed_prefix = counts.len() as u64;
        Arc::new(Self {
            state: RwLock::new(IndexState {
                file_len: content.len() as u64,
                counts,
                indexed_prefix,
            }),
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
        state.counts[block_slot(block)] = newlines;
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
        if !state.file_len.is_multiple_of(BLOCK_BYTES) {
            let partial_block = state.file_len / BLOCK_BYTES;
            state.counts[block_slot(partial_block)] = UNKNOWN;
            state.indexed_prefix = state.indexed_prefix.min(partial_block);
        }
        state
            .counts
            .resize(block_slot(blocks_for(new_len)), UNKNOWN);
        state.file_len = new_len;
        state.advance_prefix();
    }

    /// Newlines in the whole file, or `None` until every block is indexed.
    pub(crate) fn total_newlines(&self) -> Option<u64> {
        let state = self.state.read();
        if state.indexed_prefix != state.counts.len() as u64 {
            return None;
        }
        Some(state.counts.iter().map(|&count| u64::from(count)).sum())
    }

    /// Newlines in the file range, or `None` while any block it touches is
    /// unindexed. Blocks only partly inside the range are read through `cache`.
    pub fn count_range(
        &self,
        cache: &BlockCache,
        range: Range<u64>,
    ) -> Result<Option<u64>, ReadError> {
        let mut total = 0;
        let mut partial_spans = Vec::new();
        {
            let state = self.state.read();
            assert_range_inside(&range, state.file_len);
            for span in block_spans(&range, state.file_len) {
                let Some(count) = state.count(span.block) else {
                    return Ok(None);
                };
                if span.covers_block {
                    total += u64::from(count);
                } else {
                    partial_spans.push(span);
                }
            }
        }
        for span in partial_spans {
            let data = cache.get(span.block)?;
            total += count_newlines(span_bytes(&data, &span));
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
        let file_len = self.file_len();
        assert_range_inside(&range, file_len);
        let mut remaining = n;
        let mut seen = 0;
        for span in block_spans(&range, file_len) {
            let Some(count) = self.get_block(span.block) else {
                return Ok(NthNewline::Unknown);
            };
            let count = u64::from(count);
            if span.covers_block && remaining >= count {
                remaining -= count;
                seen += count;
                continue;
            }
            let data = cache.get(span.block)?;
            let bytes = span_bytes(&data, &span);
            let skip = usize::try_from(remaining).expect("remaining is below a block count");
            if let Some(position) = memchr::memchr_iter(b'\n', bytes).nth(skip) {
                return Ok(NthNewline::Found(span.start + position as u64));
            }
            let span_count = count_newlines(bytes);
            remaining -= span_count;
            seen += span_count;
        }
        Ok(NthNewline::NotInRange { count: seen })
    }
}

fn assert_range_inside(range: &Range<u64>, file_len: u64) {
    assert!(
        range.start <= range.end && range.end <= file_len,
        "range {range:?} lies inside the {file_len}-byte file"
    );
}

fn span_bytes<'data>(data: &'data [u8], span: &BlockSpan) -> &'data [u8] {
    assert!(
        span.within.end <= data.len(),
        "cached block {} is as long as the index expects",
        span.block
    );
    &data[span.within.clone()]
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
