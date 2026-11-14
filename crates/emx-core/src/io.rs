//! Reading and writing visited files. The functions here do blocking I/O:
//! the server runs them on worker threads and the test harness calls them
//! synchronously; the editor only plans saves and applies results.
//!
//! A save never reports success after a partial write and never damages
//! the original on failure: the default writes a temporary file in the same
//! directory, copies the original's mode, owner (as root) and extended
//! attributes (which include POSIX ACLs), syncs it and renames it over the
//! target. Where a rename would lose something (hardlinks, ownership, a
//! read-only directory) the file is rewritten in place instead (`inplace`).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use emx_text::{BlockCache, FileIdentity, LineIndex, Original, ReadError, Sources, Text};
use rustix::fs::{Access, AtFlags, CWD};
use xattr::FileExt;

use crate::autosave;
pub use crate::buffer::CodingMnemonic;
use crate::buffer::FileBinding;
use crate::config::Config;
use crate::inplace::{self, Move};

/// A file that changes between the stat before and after reading it is
/// read again, at most this many times in all.
const LOAD_ATTEMPTS: u32 = 3;
/// Linux `MAXSYMLINKS`: more links than this in a chain is a loop.
const SYMLINK_HOPS_MAX: u32 = 40;
/// Distinct temporary names tried before giving up.
const TEMP_NAME_ATTEMPTS: u32 = 100;
/// A temporary copy of an existing file is private until its mode is set.
const TEMP_PRIVATE_MODE: u32 = 0o600;
/// A new file gets this mode less the umask, as `open(2)` callers expect.
const NEW_FILE_MODE: u32 = 0o666;
const PERMISSION_BITS: u32 = 0o7777;
const SET_GROUP_ID: u32 = 0o2000;
const ROOT_UID: u32 = 0;
const HOME_ABBREVIATION: &str = "~";

static TEMP_SERIAL: AtomicU64 = AtomicU64::new(0);

/// A visited file read into a buffer's sources.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub sources: Arc<Sources>,
    /// `None` when the file does not exist yet.
    pub identity: Option<FileIdentity>,
    pub coding: CodingMnemonic,
    /// The original is read through the block cache and its line index
    /// still has to be built.
    pub needs_index: bool,
    /// Emacs `file-writable-p`: for a new file, whether its directory is.
    pub writable: bool,
    pub directory_exists: bool,
    /// The auto-save file is newer than the file (Emacs
    /// `file-newer-than-file-p`), so there may be work to recover.
    pub auto_save_newer: bool,
    /// xxh3 of the `append_tail_hash_bytes` before the end of the file.
    pub tail_hash: u64,
}

impl PartialEq for Loaded {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.sources, &other.sources)
            && self.identity == other.identity
            && self.coding == other.coding
            && self.needs_index == other.needs_index
            && self.writable == other.writable
            && self.directory_exists == other.directory_exists
            && self.auto_save_newer == other.auto_save_newer
            && self.tail_hash == other.tail_hash
    }
}

impl Eq for Loaded {}

/// Messages are Emacs's for the same failures.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("{} is a directory", abbreviate(.0))]
    IsDirectory(PathBuf),
    #[error("File is not readable: {}", .0.display())]
    NotReadable(PathBuf),
    #[error("Opening input file: {}, {}", strerror(.source), .path.display())]
    Open { path: PathBuf, source: io::Error },
    #[error("Read error: {}, {}", strerror(.source), .path.display())]
    Read { path: PathBuf, source: io::Error },
    #[error("{} kept changing while it was read", .0.display())]
    Unstable(PathBuf),
    #[error("not a regular file: {}", .0.display())]
    NotRegular(PathBuf),
}

/// Messages follow Emacs's `write-region` and `rename-file` errors.
#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error("Opening output file: {}, {}", strerror(.source), .path.display())]
    Open { path: PathBuf, source: io::Error },
    #[error("Write error: {}, {}", strerror(.source), .path.display())]
    Write { path: PathBuf, source: io::Error },
    #[error("Read error: {source}, {}", .path.display())]
    Read { path: PathBuf, source: ReadError },
    #[error("Preserving owner of {}: {}", .path.display(), strerror(.source))]
    Owner { path: PathBuf, source: io::Error },
    #[error("Preserving file modes of {}: {}", .path.display(), strerror(.source))]
    Mode { path: PathBuf, source: io::Error },
    #[error("Preserving extended attribute {name} of {}: {}", .path.display(), strerror(.source))]
    Attribute {
        path: PathBuf,
        name: String,
        source: io::Error,
    },
    #[error("Renaming: {}, {}, {}", strerror(.source), .from.display(), .to.display())]
    Rename {
        from: PathBuf,
        to: PathBuf,
        source: io::Error,
    },
    #[error("{} changed on disk while it was being saved; not saved", .0.display())]
    Changed(PathBuf),
    /// The plan would read the text it writes from the file it rewrites.
    #[error("{} would be rewritten from its own bytes; not saved", .0.display())]
    ReadsTarget(PathBuf),
    /// An in-place save failed after it began rewriting the file.
    #[error("{error}; {} is left partly rewritten", .path.display())]
    PartlyWritten {
        error: Box<SaveError>,
        path: PathBuf,
    },
    #[error("{error}; the temporary file {} could not be removed: {}", .temp.display(), strerror(.cleanup))]
    TempLeft {
        error: Box<SaveError>,
        temp: PathBuf,
        cleanup: io::Error,
    },
}

/// A failed save as the editor receives it from a worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveFailure {
    pub message: String,
    /// An in-place save failed after it began rewriting the file, so the
    /// file holds neither the old nor the new content.
    pub file_damaged: bool,
}

impl From<&SaveError> for SaveFailure {
    fn from(error: &SaveError) -> Self {
        SaveFailure {
            message: error.to_string(),
            file_damaged: matches!(error, SaveError::PartlyWritten { .. }),
        }
    }
}

/// How a save writes the target, decided by `plan_save`. `expected` is the
/// target's identity when the save was planned (and any overwrite of newer
/// content confirmed), `None` when it did not exist: a file that changed
/// since is never overwritten.
#[derive(Debug, Clone)]
pub enum SavePlan {
    /// Write a temporary file next to `target`, then rename it over.
    Rename {
        target: PathBuf,
        text: Text,
        expected: Option<FileIdentity>,
    },
    /// Rewrite `target` itself.
    InPlace {
        target: PathBuf,
        text: Text,
        moves: Vec<Move>,
        expected: Option<FileIdentity>,
    },
}

impl SavePlan {
    pub fn target(&self) -> &Path {
        match self {
            SavePlan::Rename { target, .. } | SavePlan::InPlace { target, .. } => target,
        }
    }

    pub fn text(&self) -> &Text {
        match self {
            SavePlan::Rename { text, .. } | SavePlan::InPlace { text, .. } => text,
        }
    }
}

impl PartialEq for SavePlan {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                SavePlan::Rename {
                    target,
                    text,
                    expected,
                },
                SavePlan::Rename {
                    target: other_target,
                    text: other_text,
                    expected: other_expected,
                },
            ) => target == other_target && text.same_root(other_text) && expected == other_expected,
            (
                SavePlan::InPlace {
                    target,
                    text,
                    moves,
                    expected,
                },
                SavePlan::InPlace {
                    target: other_target,
                    text: other_text,
                    moves: other_moves,
                    expected: other_expected,
                },
            ) => {
                target == other_target
                    && text.same_root(other_text)
                    && moves == other_moves
                    && expected == other_expected
            }
            _ => false,
        }
    }
}

impl Eq for SavePlan {}

/// What a save needs to know about the file it will write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetInfo {
    /// The visited path with symlinks resolved: a symlink's target is saved
    /// and the link kept.
    pub target: PathBuf,
    pub existing: Option<ExistingFile>,
    pub directory_exists: bool,
    pub directory_writable: bool,
    /// The group a newly created file in the directory would get.
    pub new_file_gid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExistingFile {
    pub identity: FileIdentity,
    pub nlink: u64,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    /// Emacs `file-writable-p`.
    pub writable: bool,
}

/// Where writes go. Tests substitute one that fails part way through.
pub trait SaveSink {
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()>;
    fn sync(&mut self) -> io::Result<()>;
    /// Moves the write position; in-place saves write at offsets.
    fn seek(&mut self, offset: u64) -> io::Result<()>;
}

/// Wraps a file handle as a `SaveSink`.
pub type SinkFactory = dyn FnMut(File) -> Box<dyn SaveSink>;

pub struct FileSink {
    file: File,
}

impl FileSink {
    pub fn new(file: File) -> FileSink {
        FileSink { file }
    }
}

impl SaveSink for FileSink {
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        Write::write_all(&mut self.file, buf)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }

    fn seek(&mut self, offset: u64) -> io::Result<()> {
        Seek::seek(&mut self.file, SeekFrom::Start(offset)).map(|_| ())
    }
}

pub fn file_sink(file: File) -> Box<dyn SaveSink> {
    Box::new(FileSink { file })
}

/// Reads `path` for a new buffer. Files up to `in_memory_max` are read
/// whole, so the buffer never depends on the file staying intact; larger
/// ones are read on demand through the block cache, and their line index is
/// built afterwards. A missing file is a new, empty buffer.
pub fn load_file(path: &Path, config: &Config) -> Result<Loaded, LoadError> {
    let auto_save_newer = autosave::auto_save_newer(path);
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(new_file(path, auto_save_newer));
        }
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            return Err(LoadError::NotReadable(path.to_path_buf()));
        }
        Err(source) => {
            return Err(LoadError::Open {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if metadata.is_dir() {
        return Err(LoadError::IsDirectory(path.to_path_buf()));
    }
    // FIFOs, devices and sockets never end or block; Emacs's
    // `insert-file-contents` refuses them the same way.
    if !metadata.is_file() {
        return Err(LoadError::NotRegular(path.to_path_buf()));
    }
    let writable = access_writable(path);
    let (sources, identity, coding, needs_index) = if metadata.len() <= config.in_memory_max {
        let (bytes, identity) = read_whole(path)?;
        let coding = if bytes.is_ascii() {
            CodingMnemonic::Undecided
        } else {
            CodingMnemonic::Utf8
        };
        (Sources::memory(Arc::from(bytes)), identity, coding, false)
    } else {
        let capacity = usize::try_from(config.block_cache_bytes).unwrap_or(usize::MAX);
        let cache = BlockCache::open(path, capacity).map_err(|source| open_error(path, source))?;
        let identity = cache.identity();
        let index = LineIndex::new(cache.len());
        // Telling ASCII from UTF-8 would mean reading the whole file.
        (
            Sources::file(cache, index),
            identity,
            CodingMnemonic::Utf8,
            true,
        )
    };
    let tail_hash = crate::reload::text_tail_hash(
        &Text::from_original(sources.clone()),
        config.append_tail_hash_bytes,
    )
    .map_err(|source| LoadError::Read {
        path: path.to_path_buf(),
        source: io::Error::other(source.to_string()),
    })?;
    Ok(Loaded {
        tail_hash,
        sources,
        identity: Some(identity),
        coding,
        needs_index,
        writable,
        directory_exists: true,
        auto_save_newer,
    })
}

fn new_file(path: &Path, auto_save_newer: bool) -> Loaded {
    let directory = parent_directory(path);
    let directory_exists = directory.is_dir();
    Loaded {
        sources: Sources::memory(Arc::from(Vec::new())),
        identity: None,
        coding: CodingMnemonic::Utf8,
        needs_index: false,
        writable: directory_exists && access_writable(directory),
        directory_exists,
        auto_save_newer,
        tail_hash: crate::reload::tail_hash_of(&[], 0),
    }
}

fn open_error(path: &Path, source: io::Error) -> LoadError {
    if source.kind() == io::ErrorKind::PermissionDenied {
        LoadError::NotReadable(path.to_path_buf())
    } else {
        LoadError::Open {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Reads the file whole for a new buffer. One that shrank or was replaced
/// while it was read is read again.
fn read_whole(path: &Path) -> Result<(Vec<u8>, FileIdentity), LoadError> {
    let read_error = |source| LoadError::Read {
        path: path.to_path_buf(),
        source,
    };
    for _ in 0..LOAD_ATTEMPTS {
        let file = File::open(path).map_err(|source| open_error(path, source))?;
        if !file.metadata().map_err(read_error)?.is_file() {
            return Err(LoadError::NotRegular(path.to_path_buf()));
        }
        let before = FileIdentity::of_file(&file).map_err(read_error)?;
        if let Some(bytes) = read_stat_size(&file, before).map_err(read_error)? {
            return Ok((bytes, before));
        }
    }
    Err(LoadError::Unstable(path.to_path_buf()))
}

/// The first `before.size` bytes, never more: a file that grew while it
/// was read keeps the bytes it had when stat'ed, and what was appended is
/// newer than the buffer, as any later change is. `None` when the file
/// shrank or was replaced in the meantime.
fn read_stat_size(file: &File, before: FileIdentity) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::with_capacity(usize::try_from(before.size).unwrap_or(0));
    io::Read::read_to_end(&mut io::Read::take(file, before.size), &mut bytes)?;
    let after = FileIdentity::of_file(file)?;
    let complete = bytes.len() as u64 == before.size;
    let appended_to =
        after.dev == before.dev && after.ino == before.ino && after.size > before.size;
    Ok((complete && (after == before || appended_to)).then_some(bytes))
}

/// Emacs `file-writable-p`, with the effective ids.
pub(crate) fn access_writable(path: &Path) -> bool {
    rustix::fs::accessat(CWD, path, Access::WRITE_OK, AtFlags::EACCESS).is_ok()
}

pub(crate) fn parent_directory(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("/"),
    }
}

/// Follows `path` while it is a symlink. A link to a missing file resolves
/// to that missing file, which a save then creates.
pub fn resolve_symlinks(path: &Path) -> io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..SYMLINK_HOPS_MAX {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let link = fs::read_link(&current)?;
                current = if link.is_absolute() {
                    link
                } else {
                    parent_directory(&current).join(link)
                };
            }
            Ok(_) => return Ok(current),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(current),
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::from_raw_os_error(
        rustix::io::Errno::LOOP.raw_os_error(),
    ))
}

/// Stats the file a save of `path` would write.
pub fn probe_target(path: &Path) -> io::Result<TargetInfo> {
    let target = resolve_symlinks(path)?;
    let directory = parent_directory(&target);
    let (directory_exists, directory_writable, new_file_gid) = match fs::metadata(directory) {
        Ok(metadata) => {
            let gid = if metadata.mode() & SET_GROUP_ID != 0 {
                metadata.gid()
            } else {
                rustix::process::getegid().as_raw()
            };
            (true, access_writable(directory), gid)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            (false, false, rustix::process::getegid().as_raw())
        }
        Err(error) => return Err(error),
    };
    let existing = match fs::metadata(&target) {
        Ok(metadata) => Some(ExistingFile {
            identity: FileIdentity::of_path(&target)?,
            nlink: metadata.nlink(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            mode: metadata.mode() & PERMISSION_BITS,
            writable: access_writable(&target),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    Ok(TargetInfo {
        target,
        existing,
        directory_exists,
        directory_writable,
        new_file_gid,
    })
}

/// Rename-based saves are crash-safe but replace the inode, so they are
/// used only when nothing is lost by that: the file has one link, a new
/// file would get the same owner and group (or root can set them), and the
/// directory is writable (Emacs `backup-by-copying-when-linked` and
/// `-when-mismatch`). Otherwise the file is written in place.
pub fn plan_save(
    binding: &FileBinding,
    text: &Text,
    info: &TargetInfo,
    euid: u32,
) -> Result<SavePlan, PlanError> {
    if in_place_reason(info, euid).is_some() {
        in_place_plan(binding, text, info)
    } else {
        rename_plan(binding, text, info)
    }
}

/// Why a rename would lose something, so the save must be in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InPlaceReason {
    Hardlinked,
    UnwritableDirectory,
    OtherOwner,
}

pub fn in_place_reason(info: &TargetInfo, euid: u32) -> Option<InPlaceReason> {
    if !info.directory_writable {
        return Some(InPlaceReason::UnwritableDirectory);
    }
    let file = info.existing.as_ref()?;
    if file.nlink > 1 {
        return Some(InPlaceReason::Hardlinked);
    }
    let cannot_keep_owner = euid != ROOT_UID && (file.uid != euid || file.gid != info.new_file_gid);
    cannot_keep_owner.then_some(InPlaceReason::OtherOwner)
}

/// A save that would write garbage is refused before it starts.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// The buffer reads its unchanged text from the target file, which was
    /// changed in place since: those bytes are no longer the buffer's.
    #[error("{0} changed on disk under its buffer; revert or write elsewhere")]
    ChangedUnderBuffer(String),
}

/// Whether the buffer's original is the target inode, and if so whether
/// that inode still holds what the buffer was read from (or last saved).
fn original_on_target(text: &Text, info: &TargetInfo) -> Option<bool> {
    let Original::File(cache) = &text.sources().original else {
        return None;
    };
    let disk = info.existing.as_ref()?.identity;
    let opened = cache.identity();
    (opened.dev == disk.dev && opened.ino == disk.ino).then_some(opened == disk)
}

fn changed_under_buffer(binding: &FileBinding) -> PlanError {
    let name = binding
        .path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| binding.path.display().to_string());
    PlanError::ChangedUnderBuffer(name)
}

/// A rename reads the buffer's text, which may come from the target file;
/// that file must still be what the buffer read.
pub fn rename_plan(
    binding: &FileBinding,
    text: &Text,
    info: &TargetInfo,
) -> Result<SavePlan, PlanError> {
    if original_on_target(text, info) == Some(false) {
        return Err(changed_under_buffer(binding));
    }
    Ok(SavePlan::Rename {
        target: info.target.clone(),
        text: text.clone(),
        expected: info.existing.map(|file| file.identity),
    })
}

/// An in-place save of `text` into the target. When the buffer's original
/// is the target inode, unchanged pieces are moved on disk (and skipped
/// when they do not move) and only inserted text is written: a write never
/// reads from the file it overwrites. Otherwise every byte comes from the
/// buffer. A target changed in place under its buffer is refused.
pub fn in_place_plan(
    binding: &FileBinding,
    text: &Text,
    info: &TargetInfo,
) -> Result<SavePlan, PlanError> {
    let moves = match (original_on_target(text, info), &text.sources().original) {
        (Some(false), _) => return Err(changed_under_buffer(binding)),
        (Some(true), Original::File(cache)) => inplace::plan_in_place(text, cache.len()),
        _ if text.is_empty() => Vec::new(),
        _ => vec![Move::Write {
            at: 0,
            len: text.len(),
        }],
    };
    Ok(SavePlan::InPlace {
        target: info.target.clone(),
        text: text.clone(),
        moves,
        expected: info.existing.map(|file| file.identity),
    })
}

/// The version a save wrote, as the editor receives it from a worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavedFile {
    pub identity: FileIdentity,
    /// xxh3 of the `append_tail_hash_bytes` before the end of what was
    /// written, so a later append to the saved file is recognized.
    pub tail_hash: u64,
}

/// Carries out a plan as a worker does: hashes the tail of the text, then
/// writes it. The tail is read first because an in-place save rewrites
/// the file the text may read from.
pub fn save_file(plan: &SavePlan, tail_bytes: u64) -> Result<SavedFile, SaveError> {
    save_file_with(plan, tail_bytes, &mut file_sink)
}

/// Like `save_file`, with every write going through a sink made by `sinks`.
pub fn save_file_with(
    plan: &SavePlan,
    tail_bytes: u64,
    sinks: &mut SinkFactory,
) -> Result<SavedFile, SaveError> {
    let tail_hash = crate::reload::text_tail_hash(plan.text(), tail_bytes).map_err(|source| {
        SaveError::Read {
            path: plan.target().to_path_buf(),
            source,
        }
    })?;
    let identity = save_with(plan, sinks)?;
    Ok(SavedFile {
        identity,
        tail_hash,
    })
}

/// Carries out a plan, writing through real files.
pub fn save(plan: &SavePlan) -> Result<FileIdentity, SaveError> {
    save_with(plan, &mut file_sink)
}

/// Like `save`, with every write going through a sink made by `sinks`.
pub fn save_with(plan: &SavePlan, sinks: &mut SinkFactory) -> Result<FileIdentity, SaveError> {
    match plan {
        SavePlan::Rename {
            target,
            text,
            expected,
        } => save_rename_with(target, text, *expected, sinks),
        SavePlan::InPlace {
            target,
            text,
            moves,
            expected,
        } => inplace::save_in_place_with(target, text, moves, *expected, sinks),
    }
}

pub fn save_rename(
    target: &Path,
    text: &Text,
    expected: Option<FileIdentity>,
) -> Result<FileIdentity, SaveError> {
    save_rename_with(target, text, expected, &mut file_sink)
}

/// Writes `text` to a temporary file beside `target`, gives it the
/// original's mode, owner and extended attributes, syncs it, renames it over
/// `target` and syncs the directory. Before the rename any failure removes
/// the temporary file and leaves `target` untouched, as does finding that
/// `target` is no longer the `expected` version.
pub fn save_rename_with(
    target: &Path,
    text: &Text,
    expected: Option<FileIdentity>,
    sinks: &mut SinkFactory,
) -> Result<FileIdentity, SaveError> {
    let directory = parent_directory(target);
    let original = match fs::metadata(target) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(SaveError::Open {
                path: target.to_path_buf(),
                source,
            });
        }
    };
    let create_mode = if original.is_some() {
        TEMP_PRIVATE_MODE
    } else {
        NEW_FILE_MODE
    };
    let (temp_path, temp_file) =
        create_temp(directory, target, create_mode).map_err(|source| SaveError::Open {
            path: target.to_path_buf(),
            source,
        })?;
    let written = write_temp(&temp_file, target, text, original.as_ref(), sinks)
        .and_then(|()| check_unchanged(target, expected));
    if let Err(error) = written {
        return Err(match fs::remove_file(&temp_path) {
            Ok(()) => error,
            Err(cleanup) => SaveError::TempLeft {
                error: Box::new(error),
                temp: temp_path,
                cleanup,
            },
        });
    }
    if let Err(source) = fs::rename(&temp_path, target) {
        let error = SaveError::Rename {
            from: temp_path.clone(),
            to: target.to_path_buf(),
            source,
        };
        return Err(match fs::remove_file(&temp_path) {
            Ok(()) => error,
            Err(cleanup) => SaveError::TempLeft {
                error: Box::new(error),
                temp: temp_path,
                cleanup,
            },
        });
    }
    let write_error = |source| SaveError::Write {
        path: target.to_path_buf(),
        source,
    };
    // The new content is in place now; an unsynced directory could still
    // lose the rename in a crash, so that is a failed save too.
    File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(write_error)?;
    FileIdentity::of_file(&temp_file).map_err(write_error)
}

/// The target is still the version the save was planned against.
fn check_unchanged(target: &Path, expected: Option<FileIdentity>) -> Result<(), SaveError> {
    let current = match FileIdentity::of_path(target) {
        Ok(identity) => Some(identity),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(SaveError::Open {
                path: target.to_path_buf(),
                source,
            });
        }
    };
    if current == expected {
        Ok(())
    } else {
        Err(SaveError::Changed(target.to_path_buf()))
    }
}

fn write_temp(
    temp: &File,
    target: &Path,
    text: &Text,
    original: Option<&fs::Metadata>,
    sinks: &mut SinkFactory,
) -> Result<(), SaveError> {
    let write_error = |source| SaveError::Write {
        path: target.to_path_buf(),
        source,
    };
    let mut sink = sinks(temp.try_clone().map_err(write_error)?);
    write_text(sink.as_mut(), text, 0..text.len(), target)?;
    if let Some(original) = original {
        preserve_metadata(temp, target, original)?;
    }
    sink.sync().map_err(write_error)
}

pub(crate) fn write_text(
    sink: &mut dyn SaveSink,
    text: &Text,
    range: std::ops::Range<u64>,
    target: &Path,
) -> Result<(), SaveError> {
    for chunk in text.chunks(range) {
        let chunk = chunk.map_err(|source| SaveError::Read {
            path: target.to_path_buf(),
            source,
        })?;
        sink.write_all(chunk.bytes())
            .map_err(|source| SaveError::Write {
                path: target.to_path_buf(),
                source,
            })?;
    }
    Ok(())
}

/// Owner first (changing it clears set-id bits), then extended attributes
/// (POSIX ACLs among them), then the mode.
fn preserve_metadata(temp: &File, target: &Path, original: &fs::Metadata) -> Result<(), SaveError> {
    if rustix::process::geteuid().as_raw() == ROOT_UID {
        std::os::unix::fs::fchown(temp, Some(original.uid()), Some(original.gid())).map_err(
            |source| SaveError::Owner {
                path: target.to_path_buf(),
                source,
            },
        )?;
    }
    copy_xattrs(temp, target)?;
    temp.set_permissions(fs::Permissions::from_mode(
        original.mode() & PERMISSION_BITS,
    ))
    .map_err(|source| SaveError::Mode {
        path: target.to_path_buf(),
        source,
    })
}

/// Makes the temporary file's extended attributes exactly the target's: a
/// directory's default ACL may have given it one the original lacks.
fn copy_xattrs(temp: &File, target: &Path) -> Result<(), SaveError> {
    let attribute_error = |name: &std::ffi::OsStr, source| SaveError::Attribute {
        path: target.to_path_buf(),
        name: name.to_string_lossy().into_owned(),
        source,
    };
    let names: Vec<std::ffi::OsString> = xattr::list(target)
        .map_err(|source| attribute_error(std::ffi::OsStr::new("(list)"), source))?
        .collect();
    let inherited: Vec<std::ffi::OsString> = temp
        .list_xattr()
        .map_err(|source| attribute_error(std::ffi::OsStr::new("(list)"), source))?
        .collect();
    for name in inherited.iter().filter(|name| !names.contains(name)) {
        temp.remove_xattr(name)
            .map_err(|source| attribute_error(name, source))?;
    }
    for name in &names {
        let value = xattr::get(target, name).map_err(|source| attribute_error(name, source))?;
        // Removed between listing and reading: nothing to copy.
        let Some(value) = value else {
            continue;
        };
        temp.set_xattr(name, &value)
            .map_err(|source| attribute_error(name, source))?;
    }
    Ok(())
}

/// A new file in `directory` named after `target`, created exclusively.
pub(crate) fn create_temp(
    directory: &Path,
    target: &Path,
    mode: u32,
) -> io::Result<(PathBuf, File)> {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let process = std::process::id();
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let serial = TEMP_SERIAL.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(".{name}.emx-{process}-{serial}"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("no free temporary name in {}", directory.display()),
    ))
}

/// The C library's message for an OS error, as Emacs prints it.
pub fn strerror(error: &io::Error) -> String {
    let text = error.to_string();
    match error.raw_os_error() {
        Some(code) => text
            .strip_suffix(&format!(" (os error {code})"))
            .unwrap_or(&text)
            .to_string(),
        None => text,
    }
}

/// Emacs `abbreviate-file-name`: the home directory shows as `~`.
pub fn abbreviate(path: &Path) -> String {
    let text = path.to_string_lossy().into_owned();
    let Some(home) = std::env::var_os("HOME") else {
        return text;
    };
    let home = home.to_string_lossy().trim_end_matches('/').to_string();
    if home.is_empty() {
        return text;
    }
    if text == home {
        return HOME_ABBREVIATION.to_string();
    }
    match text.strip_prefix(&home) {
        Some(rest) if rest.starts_with('/') => format!("{HOME_ABBREVIATION}{rest}"),
        _ => text,
    }
}

/// Emacs `substitute-in-file-name` followed by `expand-file-name` for
/// minibuffer input: `//` or `/~` restart the name, `~` is the home
/// directory, relative names are relative to `default_directory`, and `.`
/// and `..` are removed. A trailing slash is kept.
pub fn expand_file_name(input: &str, default_directory: &Path) -> PathBuf {
    let substituted = substitute_restart(input);
    let home = std::env::var_os("HOME").map(|home| home.to_string_lossy().into_owned());
    let absolute = match (substituted, home) {
        ("~", Some(home)) => home,
        (name, Some(home)) if name.starts_with("~/") => format!("{home}{}", &name[1..]),
        (name, _) if name.starts_with('/') => name.to_string(),
        (name, _) => format!("{}/{name}", default_directory.display()),
    };
    let trailing_slash = absolute.ends_with('/') && absolute.len() > 1;
    let mut parts: Vec<&str> = Vec::new();
    for part in absolute.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let mut expanded = format!("/{}", parts.join("/"));
    if trailing_slash && expanded != "/" {
        expanded.push('/');
    }
    PathBuf::from(expanded)
}

fn substitute_restart(input: &str) -> &str {
    let bytes = input.as_bytes();
    let mut start = 0;
    for index in 1..bytes.len() {
        if bytes[index - 1] == b'/' && (bytes[index] == b'/' || bytes[index] == b'~') {
            start = index;
        }
    }
    &input[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_file_name_follows_emacs() {
        let base = Path::new("/work/dir");
        assert_eq!(
            expand_file_name("a.txt", base),
            Path::new("/work/dir/a.txt")
        );
        assert_eq!(
            expand_file_name("/work/dir/", base),
            Path::new("/work/dir/")
        );
        assert_eq!(
            expand_file_name("/work/dir//etc/passwd", base),
            Path::new("/etc/passwd")
        );
        assert_eq!(expand_file_name("../x", base), Path::new("/work/x"));
        assert_eq!(
            expand_file_name("./x/./y", base),
            Path::new("/work/dir/x/y")
        );
        assert_eq!(expand_file_name("/", base), Path::new("/"));
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            assert_eq!(expand_file_name("/a/b/~/f", base), home.join("f"));
            assert_eq!(abbreviate(&home.join("f")), "~/f");
        }
    }

    #[test]
    fn a_file_growing_while_read_keeps_its_stat_size() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("growing.log");
        fs::write(&path, b"0123456789").expect("write");
        let file = File::open(&path).expect("open");
        let before = FileIdentity::of_file(&file).expect("stat");
        let mut appender = OpenOptions::new().append(true).open(&path).expect("open");
        Write::write_all(&mut appender, b"appended").expect("append");
        assert_eq!(
            read_stat_size(&file, before).expect("read"),
            Some(b"0123456789".to_vec())
        );
        fs::write(&path, b"012").expect("shrink");
        let file = File::open(&path).expect("open");
        let stale = FileIdentity {
            size: 10,
            ..FileIdentity::of_file(&file).expect("stat")
        };
        assert_eq!(read_stat_size(&file, stale).expect("read"), None);
    }

    #[test]
    fn strerror_drops_the_code() {
        let error = io::Error::from_raw_os_error(rustix::io::Errno::NOSPC.raw_os_error());
        assert_eq!(strerror(&error), "No space left on device");
    }
}
