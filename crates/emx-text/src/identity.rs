use std::fs::{self, File, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

const NANOSECONDS_PER_SECOND: i128 = 1_000_000_000;

/// What distinguishes one on-disk file version from another: a rename over
/// the path changes `ino`, an in-place rewrite changes `size` or `mtime_ns`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i128,
}

impl FileIdentity {
    pub fn of_file(file: &File) -> io::Result<Self> {
        Ok(Self::of_metadata(&file.metadata()?))
    }

    pub fn of_path(path: &Path) -> io::Result<Self> {
        Ok(Self::of_metadata(&fs::metadata(path)?))
    }

    fn of_metadata(metadata: &Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            size: metadata.size(),
            mtime_ns: i128::from(metadata.mtime()) * NANOSECONDS_PER_SECOND
                + i128::from(metadata.mtime_nsec()),
        }
    }
}
