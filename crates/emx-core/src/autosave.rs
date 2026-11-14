//! Emacs auto-save files: `#NAME#` beside the visited file, written from
//! time to time while a buffer has unsaved changes, deleted by a save of
//! the buffer, and read back by `recover-this-file`.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use emx_text::Text;

use crate::io::{FileSink, SaveError, create_temp, parent_directory, write_text};

/// Owner read and write are added to the visited file's mode, as Emacs
/// does, so the owner can always recover.
const OWNER_READ_WRITE: u32 = 0o600;
const PERMISSION_BITS: u32 = 0o7777;
/// Mode of the auto-save file of a file that does not exist yet.
const PRIVATE_MODE: u32 = 0o600;

/// Emacs `make-auto-save-file-name` for a visited file.
pub fn auto_save_path(file: &Path) -> PathBuf {
    let name = file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    parent_directory(file).join(format!("#{name}#"))
}

/// Emacs `(file-newer-than-file-p AUTO-SAVE FILE)`: the auto-save file
/// exists and the file does not, or is older.
pub fn auto_save_newer(file: &Path) -> bool {
    let Ok(auto_save) = fs::metadata(auto_save_path(file)) else {
        return false;
    };
    match fs::metadata(file) {
        Ok(visited) => {
            (auto_save.mtime(), auto_save.mtime_nsec()) > (visited.mtime(), visited.mtime_nsec())
        }
        Err(_) => true,
    }
}

/// Writes `text` as the auto-save file of `file`. A temporary file renamed
/// into place means a crash never leaves a half-written auto-save.
pub fn write_auto_save(file: &Path, text: &Text) -> Result<(), SaveError> {
    let path = auto_save_path(file);
    let mode = match fs::metadata(file) {
        Ok(metadata) => (metadata.mode() & PERMISSION_BITS) | OWNER_READ_WRITE,
        Err(_) => PRIVATE_MODE,
    };
    let open_error = |source| SaveError::Open {
        path: path.clone(),
        source,
    };
    let (temp_path, temp) =
        create_temp(parent_directory(&path), &path, PRIVATE_MODE).map_err(open_error)?;
    let written = (|| {
        let write_error = |source| SaveError::Write {
            path: path.clone(),
            source,
        };
        temp.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(write_error)?;
        let mut sink = FileSink::new(temp.try_clone().map_err(write_error)?);
        write_text(&mut sink, text, 0..text.len(), &path)?;
        fs::rename(&temp_path, &path).map_err(write_error)
    })();
    let Err(error) = written else {
        return Ok(());
    };
    match fs::remove_file(&temp_path) {
        Ok(()) => Err(error),
        Err(cleanup) => Err(SaveError::TempLeft {
            error: Box::new(error),
            temp: temp_path,
            cleanup,
        }),
    }
}

/// Deletes the auto-save file of `file`; one that is already gone is fine.
pub fn remove_auto_save(file: &Path) -> io::Result<()> {
    match fs::remove_file(auto_save_path(file)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

pub fn read_auto_save(file: &Path) -> io::Result<Vec<u8>> {
    fs::read(auto_save_path(file))
}
