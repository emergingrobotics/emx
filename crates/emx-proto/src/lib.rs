#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use emx_types::{Cell, Grid, Key, MouseEvent};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const PROTOCOL_VERSION: u32 = 2;
pub const MAX_WIRE_MESSAGE: usize = 16 * 1024 * 1024;

const VERSION_FIELD: &str = "v";

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
    #[error("malformed json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("protocol version {got} not supported, expected {want}")]
    Version { got: u64, want: u32 },
    #[error("wire message exceeds {limit} bytes")]
    Oversized { limit: usize },
    #[error("connection ended in the middle of a message")]
    Truncated,
    #[error("invalid message: {0}")]
    Invalid(String),
}

/// Paths are arbitrary bytes on Linux, so non-UTF-8 paths need a lossless encoding.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WirePath {
    Text(String),
    Bytes(Vec<u8>),
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum WirePathRepr {
    Text(String),
    Bytes { bytes_b64: String },
}

impl From<&Path> for WirePath {
    fn from(path: &Path) -> WirePath {
        let bytes = path.as_os_str().as_bytes();
        match std::str::from_utf8(bytes) {
            Ok(text) => WirePath::Text(text.to_string()),
            Err(_) => WirePath::Bytes(bytes.to_vec()),
        }
    }
}

impl WirePath {
    pub fn into_path_buf(self) -> PathBuf {
        match self {
            WirePath::Text(text) => PathBuf::from(text),
            WirePath::Bytes(bytes) => PathBuf::from(OsString::from_vec(bytes)),
        }
    }
}

impl Serialize for WirePath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            WirePath::Text(text) => WirePathRepr::Text(text.clone()),
            WirePath::Bytes(bytes) => WirePathRepr::Bytes {
                bytes_b64: STANDARD.encode(bytes),
            },
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for WirePath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<WirePath, D::Error> {
        match WirePathRepr::deserialize(deserializer)? {
            WirePathRepr::Text(text) => Ok(WirePath::Text(text)),
            WirePathRepr::Bytes { bytes_b64 } => STANDARD
                .decode(bytes_b64)
                .map(WirePath::Bytes)
                .map_err(serde::de::Error::custom),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientKind {
    Tui,
    Cli,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        kind: ClientKind,
        cwd: WirePath,
        cols: u16,
        rows: u16,
        wait_for: Option<Vec<WirePath>>,
    },
    Key {
        key: Key,
    },
    /// A mouse report; sent only while the server asks for mouse reporting.
    Mouse {
        event: MouseEvent,
    },
    /// One chunk of a bracketed paste; the editor inserts the whole paste
    /// once the chunk with `final` set arrives.
    Paste {
        data_b64: String,
        r#final: bool,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    Request {
        id: u64,
        req: Request,
    },
    /// The client could not use a frame (it failed validation or did not
    /// match the terminal): the next frame is sent whole.
    Redraw,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Open {
        path: WirePath,
        line: Option<u64>,
        col: Option<u64>,
        wait: bool,
    },
    ListBuffers,
    Reload {
        path: Option<WirePath>,
    },
    Goto {
        path: Option<WirePath>,
        line: u64,
        col: Option<u64>,
    },
    SaveAll,
    SubscribeEvents,
    DiffExternal {
        path: WirePath,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowUpdate {
    pub row: u16,
    pub cells: Vec<Cell>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BufferInfo {
    pub name: String,
    pub path: Option<WirePath>,
    pub modified: bool,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Buffers(Vec<BufferInfo>),
    Diff(String),
}

/// The `code` of a `WireError`, for scripts to match on.
pub mod error_code {
    /// A request that shows a buffer came from a CLI client while no
    /// terminal client is attached.
    pub const NO_CLIENT: &str = "no_client";
    /// No buffer visits the file named.
    pub const NO_BUFFER: &str = "no_buffer";
    pub const BUFFER_MODIFIED: &str = "buffer_modified";
    pub const OPEN_FAILED: &str = "open_failed";
    pub const RELOAD_FAILED: &str = "reload_failed";
    pub const SAVE_FAILED: &str = "save_failed";
    pub const GOTO_FAILED: &str = "goto_failed";
    pub const NO_EXTERNAL_CHANGE: &str = "no_external_change";
    /// `diff-external` could not read or diff the texts.
    pub const DIFF_FAILED: &str = "diff_failed";
    /// The connection broke the protocol; the server closes it.
    pub const PROTOCOL: &str = "protocol";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExternalKind {
    Reloaded,
    Appended,
    Conflict,
    Deleted,
    Renamed { to: WirePath },
    Truncated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditorEvent {
    BufferOpened {
        path: Option<WirePath>,
        name: String,
    },
    BufferKilled {
        path: Option<WirePath>,
        name: String,
    },
    BufferSaved {
        path: Option<WirePath>,
        name: String,
    },
    ExternalChange {
        path: WirePath,
        kind: ExternalKind,
    },
    CursorMoved {
        client: u64,
        path: Option<WirePath>,
        line: Option<u64>,
        col: u64,
        offset: u64,
    },
    ClientAttached {
        client: u64,
    },
    ClientDetached {
        client: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome {
        server_pid: u32,
    },
    Frame {
        full: bool,
        cols: u16,
        rows: u16,
        rows_changed: Vec<RowUpdate>,
        cursor: Option<(u16, u16)>,
    },
    Bell,
    /// The terminal modes the client sets: mouse reporting on or off. Sent
    /// to a terminal client after `Welcome` and whenever they change.
    Modes {
        mouse: bool,
    },
    Response {
        id: u64,
        result: Result<Response, WireError>,
    },
    Event {
        event: EditorEvent,
    },
    Exit {
        reason: String,
    },
}

impl ServerMsg {
    /// Frames come from the network, so row geometry is checked before a client indexes a grid with it.
    pub fn validate(&self, cols: u16) -> Result<(), ProtoError> {
        let ServerMsg::Frame {
            rows, rows_changed, ..
        } = self
        else {
            return Ok(());
        };
        for update in rows_changed {
            if update.row >= *rows {
                return Err(ProtoError::Invalid(format!(
                    "row update {} outside {} rows",
                    update.row, rows
                )));
            }
            if update.cells.len() > usize::from(cols) {
                return Err(ProtoError::Invalid(format!(
                    "row update {} has {} cells, wider than {} columns",
                    update.row,
                    update.cells.len(),
                    cols
                )));
            }
        }
        Ok(())
    }
}

pub fn diff_grids(old: Option<&Grid>, new: &Grid) -> ServerMsg {
    let comparable = old.filter(|previous| previous.cols == new.cols && previous.rows == new.rows);
    let rows_changed = (0..new.rows)
        .filter(|row| comparable.is_none_or(|previous| previous.row(*row) != new.row(*row)))
        .map(|row| RowUpdate {
            row,
            cells: new.row(row).to_vec(),
        })
        .collect();
    ServerMsg::Frame {
        full: comparable.is_none(),
        cols: new.cols,
        rows: new.rows,
        rows_changed,
        cursor: new.cursor,
    }
}

pub fn write_msg<W: Write, T: Serialize>(writer: &mut W, message: &T) -> io::Result<()> {
    let mut value = serde_json::to_value(message).map_err(io::Error::other)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| io::Error::other("wire message must serialize to a json object"))?;
    object.insert(VERSION_FIELD.to_string(), PROTOCOL_VERSION.into());
    let mut line = serde_json::to_vec(&value).map_err(io::Error::other)?;
    if line.len() > MAX_WIRE_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("wire message exceeds {MAX_WIRE_MESSAGE} bytes"),
        ));
    }
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()
}

pub struct LineReader<R> {
    reader: BufReader<R>,
    limit: usize,
}

impl<R: Read> LineReader<R> {
    pub fn new(reader: R) -> LineReader<R> {
        LineReader::with_limit(reader, MAX_WIRE_MESSAGE)
    }

    pub fn with_limit(reader: R, limit: usize) -> LineReader<R> {
        LineReader {
            reader: BufReader::new(reader),
            limit,
        }
    }

    /// Returns `None` on a clean end of stream between messages.
    pub fn read_msg<T: DeserializeOwned>(&mut self) -> Result<Option<T>, ProtoError> {
        let mut line = Vec::new();
        // One byte past the limit for the newline, one more to detect overflow.
        let allowance = (self.limit as u64).saturating_add(2);
        let read = (&mut self.reader)
            .take(allowance)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(None);
        }
        let terminated = line.last() == Some(&b'\n');
        if terminated {
            line.pop();
        }
        if line.len() > self.limit {
            return Err(ProtoError::Oversized { limit: self.limit });
        }
        if !terminated {
            return Err(ProtoError::Truncated);
        }
        let mut value: serde_json::Value = serde_json::from_slice(&line)?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| ProtoError::Invalid("wire message is not a json object".into()))?;
        let version = object
            .remove(VERSION_FIELD)
            .and_then(|field| field.as_u64())
            .ok_or_else(|| ProtoError::Invalid("missing or non-numeric version field".into()))?;
        if version != u64::from(PROTOCOL_VERSION) {
            return Err(ProtoError::Version {
                got: version,
                want: PROTOCOL_VERSION,
            });
        }
        Ok(Some(serde_json::from_value(value)?))
    }
}

#[cfg(test)]
mod tests;
