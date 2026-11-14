//! One connection: its reader thread turns `ClientMsg`s into editor
//! events, its writer thread sends the `ServerMsg`s queued in its outbox.
//!
//! Frames are bounded: at most `FRAME_QUEUE_DEPTH` wait in the outbox. A
//! frame that does not fit is dropped and so is every later one until the
//! outbox drains; then the writer asks for a resync and the editor sends
//! the next frame whole, so a slow terminal never sees a diff against a
//! frame it did not get.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{self, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use emx_core::config::FRAME_QUEUE_DEPTH;
use emx_core::{ClientId, Event};
use emx_proto::{
    ClientKind, ClientMsg, LineReader, ProtoError, Request, ServerMsg, WirePath, write_msg,
};

use super::Input;

/// Terminal sizes beyond these are refused: a whole frame of that many
/// cells must still fit in one wire message (`MAX_WIRE_MESSAGE`).
pub const MAX_TERMINAL_COLS: u16 = 1000;
pub const MAX_TERMINAL_ROWS: u16 = 250;
/// Messages other than frames (answers, events) queued for a client that
/// does not read them; past this the client is disconnected.
pub const OUTBOX_MESSAGE_LIMIT: usize = 65_536;

#[derive(Debug, thiserror::Error)]
pub enum AdmitError {
    #[error("cannot read the peer's credentials: {0}")]
    Credentials(io::Error),
    #[error("refused a connection from uid {peer}; this server belongs to uid {euid}")]
    ForeignPeer { peer: u32, euid: u32 },
}

/// The peer's uid as `SO_PEERCRED` reports it.
pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    rustix::net::sockopt::socket_peercred(stream)
        .map(|credentials| credentials.uid.as_raw())
        .map_err(io::Error::from)
}

/// Only the server's own user may attach (DESIGN section 2). `peer_uid` is
/// the seam tests use to present another uid.
pub fn admit(
    stream: &UnixStream,
    euid: u32,
    peer_uid: &dyn Fn(&UnixStream) -> io::Result<u32>,
) -> Result<(), AdmitError> {
    let peer = peer_uid(stream).map_err(AdmitError::Credentials)?;
    if peer != euid {
        return Err(AdmitError::ForeignPeer { peer, euid });
    }
    Ok(())
}

/// What happened to a message given to `Outbox::push`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Queued,
    /// The frame queue is full; the frame was dropped.
    FrameDropped,
    /// The client stopped reading; it must be disconnected.
    Overflow,
}

#[derive(Debug, Default)]
struct OutboxState {
    queue: VecDeque<ServerMsg>,
    frames: usize,
    closed: bool,
    /// A frame was dropped: ask for a resync once the queue drains.
    resync: bool,
}

/// The messages waiting for a client's writer thread.
#[derive(Debug, Default)]
pub struct Outbox {
    state: Mutex<OutboxState>,
    ready: Condvar,
}

/// What the writer thread takes from the outbox.
enum Next {
    Message(ServerMsg),
    /// The queue drained after frames were dropped.
    Resync,
    Closed,
}

impl Outbox {
    pub fn push(&self, message: ServerMsg) -> Delivery {
        let mut state = self.state.lock().expect("outbox lock");
        if state.closed {
            return Delivery::Queued;
        }
        if matches!(message, ServerMsg::Frame { .. }) {
            if state.frames >= FRAME_QUEUE_DEPTH as usize {
                state.resync = true;
                return Delivery::FrameDropped;
            }
            state.frames += 1;
        } else if state.queue.len() >= OUTBOX_MESSAGE_LIMIT {
            return Delivery::Overflow;
        }
        state.queue.push_back(message);
        self.ready.notify_one();
        Delivery::Queued
    }

    /// The writer sends what is queued, then ends the connection.
    pub fn close(&self) {
        self.state.lock().expect("outbox lock").closed = true;
        self.ready.notify_one();
    }

    /// Takes everything queued at once, as a writer would send it, and
    /// whether a resync is due; for driving a `Server` without sockets.
    pub fn take_queued(&self) -> (Vec<ServerMsg>, bool) {
        let mut state = self.state.lock().expect("outbox lock");
        state.frames = 0;
        let resync = std::mem::take(&mut state.resync);
        (state.queue.drain(..).collect(), resync)
    }

    fn next(&self) -> Next {
        let mut state = self.state.lock().expect("outbox lock");
        loop {
            if let Some(message) = state.queue.pop_front() {
                if matches!(message, ServerMsg::Frame { .. }) {
                    state.frames -= 1;
                }
                return Next::Message(message);
            }
            if state.closed {
                return Next::Closed;
            }
            if state.resync {
                state.resync = false;
                return Next::Resync;
            }
            state = self.ready.wait(state).expect("outbox lock");
        }
    }
}

/// Serves one admitted connection: reads `Hello`, starts the writer, then
/// forwards the client's messages until it disconnects.
pub fn serve(stream: UnixStream, client: ClientId, inputs: Sender<Input>) {
    let mut reader = LineReader::new(&stream);
    let hello = match read_hello(&mut reader) {
        Ok(hello) => hello,
        Err(reason) => {
            refuse(&stream, &reason);
            return;
        }
    };
    let writer_stream = match stream.try_clone() {
        Ok(writer_stream) => writer_stream,
        Err(error) => {
            eprintln!("emx server: cannot serve a client: {error}");
            return;
        }
    };
    let outbox = Arc::new(Outbox::default());
    outbox.push(ServerMsg::Welcome {
        server_pid: std::process::id(),
    });
    let writer_outbox = outbox.clone();
    let writer_inputs = inputs.clone();
    let spawned = thread::Builder::new()
        .name(format!("emx-write-{}", client.0))
        .spawn(move || write_loop(writer_stream, client, &writer_outbox, &writer_inputs));
    if let Err(error) = spawned {
        eprintln!("emx server: cannot start a client writer: {error}");
        return;
    }
    let attached = inputs.send(Input::Attach {
        client,
        kind: hello.kind,
        cols: hello.cols,
        rows: hello.rows,
        outbox,
    });
    if attached.is_err() {
        return;
    }
    loop {
        let input = match reader.read_msg::<ClientMsg>() {
            Ok(Some(message)) => match to_event(client, message, &hello.cwd) {
                Ok(event) => Input::Client(client, event),
                Err(reason) => Input::ProtocolError(client, reason),
            },
            Ok(None) => Input::Detach(client),
            Err(ProtoError::Io(error)) if error.kind() == io::ErrorKind::ConnectionReset => {
                Input::Detach(client)
            }
            Err(error) => Input::ProtocolError(client, error.to_string()),
        };
        let last = !matches!(input, Input::Client(..));
        if inputs.send(input).is_err() || last {
            return;
        }
    }
}

struct Hello {
    kind: ClientKind,
    cwd: PathBuf,
    cols: u16,
    rows: u16,
}

fn read_hello(reader: &mut LineReader<&UnixStream>) -> Result<Hello, String> {
    let message = reader
        .read_msg::<ClientMsg>()
        .map_err(|error| error.to_string())?;
    let Some(ClientMsg::Hello {
        kind,
        cwd,
        cols,
        rows,
        wait_for,
    }) = message
    else {
        return Err("the first message must be hello".to_string());
    };
    if wait_for.is_some() {
        return Err("hello wait_for is not supported; open files with wait instead".to_string());
    }
    let cwd = cwd.into_path_buf();
    if !cwd.is_absolute() {
        return Err(format!("cwd {} is not absolute", cwd.display()));
    }
    check_size(cols, rows)?;
    Ok(Hello {
        kind,
        cwd,
        cols,
        rows,
    })
}

fn check_size(cols: u16, rows: u16) -> Result<(), String> {
    if cols > MAX_TERMINAL_COLS || rows > MAX_TERMINAL_ROWS {
        return Err(format!(
            "terminal size {cols}x{rows} exceeds {MAX_TERMINAL_COLS}x{MAX_TERMINAL_ROWS}"
        ));
    }
    Ok(())
}

/// Tells a client its connection is refused, best effort: it may be gone.
fn refuse(stream: &UnixStream, reason: &str) {
    eprintln!("emx server: refusing a client: {reason}");
    let mut writer = BufWriter::new(stream);
    if let Err(error) = write_msg(
        &mut writer,
        &ServerMsg::Exit {
            reason: reason.to_string(),
        },
    ) {
        eprintln!("emx server: could not tell the client why: {error}");
    }
}

/// A client message as an editor event; paths in requests are made
/// absolute against the client's directory.
fn to_event(client: ClientId, message: ClientMsg, cwd: &Path) -> Result<Event, String> {
    Ok(match message {
        ClientMsg::Hello { .. } => return Err("hello sent twice".to_string()),
        ClientMsg::Key { key } => Event::Key(client, key),
        ClientMsg::Paste { data_b64, r#final } => Event::Paste {
            client,
            data: STANDARD
                .decode(data_b64)
                .map_err(|error| format!("paste data is not base64: {error}"))?,
            last: r#final,
        },
        ClientMsg::Resize { cols, rows } => {
            check_size(cols, rows)?;
            Event::Resize(client, cols, rows)
        }
        ClientMsg::Request { id, req } => Event::Request(client, id, absolute_request(req, cwd)),
        ClientMsg::Redraw => Event::Redraw(client),
    })
}

fn absolute_request(request: Request, cwd: &Path) -> Request {
    let absolute =
        |path: WirePath| WirePath::from(absolute_path(&path.into_path_buf(), cwd).as_path());
    match request {
        Request::Open {
            path,
            line,
            col,
            wait,
        } => Request::Open {
            path: absolute(path),
            line,
            col,
            wait,
        },
        Request::Reload { path } => Request::Reload {
            path: path.map(absolute),
        },
        Request::Goto { path, line, col } => Request::Goto {
            path: path.map(absolute),
            line,
            col,
        },
        Request::DiffExternal { path } => Request::DiffExternal {
            path: absolute(path),
        },
        other @ (Request::ListBuffers | Request::SaveAll | Request::SubscribeEvents) => other,
    }
}

/// `path` relative to `cwd`, with `.` and `..` removed lexically, as Emacs
/// `expand-file-name` does: one file has one buffer whichever way it is
/// named. Works on raw bytes, so any file name survives.
pub fn absolute_path(path: &Path, cwd: &Path) -> PathBuf {
    let joined = cwd.join(path);
    let mut parts: Vec<OsString> = Vec::new();
    for component in joined.components() {
        match component {
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(part) => parts.push(part.to_owned()),
        }
    }
    let mut absolute = PathBuf::from("/");
    absolute.extend(parts);
    absolute
}

fn write_loop(stream: UnixStream, client: ClientId, outbox: &Outbox, inputs: &Sender<Input>) {
    let mut writer = BufWriter::new(&stream);
    loop {
        match outbox.next() {
            Next::Message(message) => {
                if let Err(error) = write_msg(&mut writer, &message) {
                    if error.kind() != io::ErrorKind::BrokenPipe {
                        eprintln!("emx server: writing to client {}: {error}", client.0);
                    }
                    break;
                }
            }
            Next::Resync => {
                if inputs.send(Input::Resync(client)).is_err() {
                    break;
                }
            }
            Next::Closed => break,
        }
    }
    outbox.close();
    // Ends the reader too, which reports the detach.
    if let Err(error) = stream.shutdown(std::net::Shutdown::Both)
        && error.kind() != io::ErrorKind::NotConnected
    {
        eprintln!("emx server: closing client {}: {error}", client.0);
    }
}
