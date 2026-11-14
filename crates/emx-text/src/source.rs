use std::io;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::block_cache::BlockCache;
use crate::line_index::LineIndex;

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
    #[error("file truncated: expected {expected_len} bytes, file now has {actual_len}")]
    Truncated { expected_len: u64, actual_len: u64 },
    #[error("the file was rewritten on disk; the text read from its earlier version is gone")]
    Retired,
    #[error("block {block} is not cached")]
    NotCached { block: u64 },
}

/// Append-only store for inserted bytes. Pieces reference it by offset, so
/// bytes are never moved or removed once appended.
#[derive(Debug, Default)]
pub struct AddStore {
    bytes: RwLock<Vec<u8>>,
}

impl AddStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Returns the offset at which `bytes` now start.
    pub fn append(&self, bytes: &[u8]) -> u64 {
        let mut store = self.bytes.write();
        let offset = store.len() as u64;
        store.extend_from_slice(bytes);
        offset
    }

    pub fn len(&self) -> u64 {
        self.bytes.read().len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.read().is_empty()
    }

    /// Panics when the range was never appended: pieces only reference
    /// bytes that `append` returned offsets for.
    pub fn read_into(&self, off: u64, len: usize, out: &mut Vec<u8>) {
        let store = self.bytes.read();
        let start = usize::try_from(off).expect("add store offset fits in memory");
        out.extend_from_slice(&store[start..start + len]);
    }

    pub(crate) fn count_newlines(&self, off: u64, len: u64) -> u64 {
        let store = self.bytes.read();
        let start = usize::try_from(off).expect("add store offset fits in memory");
        let end = start + usize::try_from(len).expect("add piece length fits in memory");
        memchr::memchr_iter(b'\n', &store[start..end]).count() as u64
    }

    pub(crate) fn nth_newline(&self, off: u64, len: u64, n: u64) -> Option<u64> {
        let store = self.bytes.read();
        let start = usize::try_from(off).expect("add store offset fits in memory");
        let end = start + usize::try_from(len).expect("add piece length fits in memory");
        let skip = usize::try_from(n).ok()?;
        memchr::memchr_iter(b'\n', &store[start..end])
            .nth(skip)
            .map(|position| position as u64)
    }
}

/// The bytes a buffer was opened from: read fully into memory for small files,
/// or read on demand through a block cache for large ones.
#[derive(Debug, Clone)]
pub enum Original {
    Memory(Arc<[u8]>),
    File(Arc<BlockCache>),
}

impl Original {
    pub fn len(&self) -> u64 {
        match self {
            Original::Memory(bytes) => bytes.len() as u64,
            Original::File(cache) => cache.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug)]
pub struct Sources {
    pub original: Original,
    pub add: Arc<AddStore>,
    pub index: Arc<LineIndex>,
}

impl Sources {
    pub fn memory(content: Arc<[u8]>) -> Arc<Self> {
        let index = LineIndex::fully_indexed(&content);
        Arc::new(Self {
            original: Original::Memory(content),
            add: AddStore::new(),
            index,
        })
    }

    /// Panics when `index` was built for a different length than `cache`.
    pub fn file(cache: Arc<BlockCache>, index: Arc<LineIndex>) -> Arc<Self> {
        assert_eq!(
            index.file_len(),
            cache.len(),
            "line index and block cache describe the same file length"
        );
        Arc::new(Self {
            original: Original::File(cache),
            add: AddStore::new(),
            index,
        })
    }
}
