use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use lru::LruCache;
use parking_lot::Mutex;

use crate::identity::FileIdentity;
use crate::source::ReadError;

pub const BLOCK_SIZE: usize = 65536;
const BLOCK_BYTES: u64 = BLOCK_SIZE as u64;

struct CachedBlocks {
    blocks: LruCache<u64, Arc<[u8]>>,
    bytes: usize,
}

/// Reads a file with `pread` rather than `mmap`, so another process
/// truncating or rewriting the file yields a short read (an error) instead of
/// undefined behaviour. The open descriptor keeps the original inode alive
/// across rename-replacement.
pub struct BlockCache {
    file: File,
    identity: Mutex<FileIdentity>,
    expected_len: AtomicU64,
    capacity_bytes: usize,
    cached: Mutex<CachedBlocks>,
}

impl std::fmt::Debug for BlockCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BlockCache")
            .field("identity", &self.identity())
            .field("expected_len", &self.len())
            .field("capacity_bytes", &self.capacity_bytes)
            .finish_non_exhaustive()
    }
}

impl BlockCache {
    /// `capacity_bytes` must hold at least one block, otherwise every read
    /// would evict the block it just loaded.
    pub fn open(path: &Path, capacity_bytes: usize) -> io::Result<Arc<Self>> {
        if capacity_bytes < BLOCK_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "block cache capacity {capacity_bytes} is smaller than one block ({BLOCK_SIZE})"
                ),
            ));
        }
        let file = File::open(path)?;
        let identity = FileIdentity::of_file(&file)?;
        Ok(Arc::new(Self {
            file,
            identity: Mutex::new(identity),
            expected_len: AtomicU64::new(identity.size),
            capacity_bytes,
            cached: Mutex::new(CachedBlocks {
                blocks: LruCache::unbounded(),
                bytes: 0,
            }),
        }))
    }

    pub fn len(&self) -> u64 {
        self.expected_len.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Identity of the file as it was when opened or last `rewritten`.
    pub fn identity(&self) -> FileIdentity {
        *self.identity.lock()
    }

    /// Records that the open file was rewritten in place and is now the
    /// `identity` version: every cached block is dropped.
    pub fn rewritten(&self, identity: FileIdentity) {
        *self.identity.lock() = identity;
        self.expected_len.store(identity.size, Ordering::Release);
        self.invalidate_from(0);
    }

    pub fn get(&self, block: u64) -> Result<Arc<[u8]>, ReadError> {
        if let Some(data) = self.try_get_cached(block) {
            return Ok(data);
        }
        let data: Arc<[u8]> = Arc::from(self.read_block_direct(block)?);
        self.insert(block, data.clone());
        Ok(data)
    }

    pub fn try_get_cached(&self, block: u64) -> Option<Arc<[u8]>> {
        self.cached.lock().blocks.get(&block).cloned()
    }

    /// Drops every cached block that holds bytes at or after `offset`.
    pub fn invalidate_from(&self, offset: u64) {
        let first_block = offset / BLOCK_BYTES;
        let mut cached = self.cached.lock();
        let stale: Vec<u64> = cached
            .blocks
            .iter()
            .map(|(block, _)| *block)
            .filter(|block| *block >= first_block)
            .collect();
        for block in stale {
            if let Some(data) = cached.blocks.pop(&block) {
                cached.bytes -= data.len();
            }
        }
    }

    /// Records that the file now has `new_len` bytes (an append was observed).
    /// The block that held the old end is dropped because it was cached short.
    pub fn set_len(&self, new_len: u64) {
        let old_len = self.expected_len.swap(new_len, Ordering::AcqRel);
        self.invalidate_from(old_len.min(new_len));
    }

    fn insert(&self, block: u64, data: Arc<[u8]>) {
        let mut cached = self.cached.lock();
        // A concurrent `set_len` may have changed this block's length after it
        // was read; caching it would serve short data until the next invalidation.
        if data.len() as u64 != block_length(block, self.len()) {
            return;
        }
        cached.bytes += data.len();
        if let Some(previous) = cached.blocks.put(block, data) {
            cached.bytes -= previous.len();
        }
        while cached.bytes > self.capacity_bytes {
            let (_, evicted) = cached
                .blocks
                .pop_lru()
                .expect("cache over capacity holds at least one block");
            cached.bytes -= evicted.len();
        }
    }

    /// Reads one block with `pread`, bypassing the cache. A read shorter than
    /// the expected length means the file was truncated underneath us.
    pub(crate) fn read_block_direct(&self, block: u64) -> Result<Vec<u8>, ReadError> {
        let expected_len = self.len();
        let start = block * BLOCK_BYTES;
        if start >= expected_len {
            return Err(ReadError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("block {block} is past the end of a {expected_len}-byte file"),
            )));
        }
        let wanted = (expected_len - start).min(BLOCK_BYTES) as usize;
        let mut buffer = vec![0u8; wanted];
        let mut filled = 0;
        while filled < wanted {
            match self
                .file
                .read_at(&mut buffer[filled..], start + filled as u64)
            {
                Ok(0) => {
                    let actual_len = self.file.metadata()?.len();
                    return Err(ReadError::Truncated {
                        expected_len,
                        actual_len,
                    });
                }
                Ok(read) => filled += read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(ReadError::Io(error)),
            }
        }
        Ok(buffer)
    }

    pub(crate) fn file(&self) -> &File {
        &self.file
    }
}

/// Number of bytes block `block` holds in a file of `file_len` bytes.
pub(crate) fn block_length(block: u64, file_len: u64) -> u64 {
    file_len
        .saturating_sub(block * BLOCK_BYTES)
        .min(BLOCK_BYTES)
}
