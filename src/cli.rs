//! The command-line tools of the socket API (DESIGN section 2): `emx open`,
//! `list-buffers`, `reload`, `goto`, `save-all`, `events`, `diff-external`
//! and `apply-patch`. Each connects to the running server as a CLI client,
//! sends one request and reports its answer. They never start a server:
//! a script asking a session that does not exist is told so.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, bail};
use emx_proto::{
    BufferInfo, ClientKind, ClientMsg, LineReader, Request, Response, ServerMsg, WirePath,
    write_msg,
};

use crate::client::{self, FileArgument, parse_file_argument};

const NO_SERVER_RUNNING: &str = "no server running";
const CONNECTION_LOST: &str = "server connection lost";
const GOTO_USAGE: &str = "goto needs [FILE:]LINE[:COL]";
const POSITION_SEPARATOR: u8 = b':';
const MODIFIED_FLAG: char = '*';
const UNMODIFIED_FLAG: char = ' ';
/// Caret notation (`^J`) shows a control character as the character
/// this far above it, as Emacs does.
const CARET_OFFSET: u8 = 0x40;
const DELETE: char = '\x7f';
const DELETE_CARET: &str = "^?";

/// One command-line tool, with its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tool {
    /// `emx open FILE[:LINE[:COL]] [--wait]`
    Open {
        file: OsString,
        wait: bool,
    },
    /// `emx list-buffers [--json]`
    ListBuffers {
        json: bool,
    },
    /// `emx reload [FILE]`
    Reload {
        file: Option<OsString>,
    },
    /// `emx goto [FILE:]LINE[:COL]`
    Goto {
        target: OsString,
    },
    SaveAll,
    /// `emx events`: JSON lines on stdout until interrupted.
    Events,
    DiffExternal {
        file: OsString,
    },
    /// `emx apply-patch FILE`, the patch on stdin.
    ApplyPatch {
        file: OsString,
    },
}

/// Where `emx goto` moves point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GotoTarget {
    pub path: Option<PathBuf>,
    pub line: u64,
    pub col: Option<u64>,
}

/// `LINE` or `LINE:COL` moves point in the buffer the most recently active
/// terminal shows; anything else is `FILE:LINE[:COL]`, read as `emx open`
/// reads it, and must name a line.
pub fn parse_goto_target(argument: &OsStr, exists: &dyn Fn(&Path) -> bool) -> Option<GotoTarget> {
    if let Some((line, col)) = bare_position(argument.as_bytes()) {
        return Some(GotoTarget {
            path: None,
            line,
            col,
        });
    }
    let FileArgument { path, line, col } = parse_file_argument(argument, exists);
    line.map(|line| GotoTarget {
        path: Some(path),
        line,
        col,
    })
}

fn bare_position(bytes: &[u8]) -> Option<(u64, Option<u64>)> {
    let mut parts = bytes.split(|byte| *byte == POSITION_SEPARATOR);
    let line = number(parts.next()?)?;
    let col = match parts.next() {
        Some(part) => Some(number(part)?),
        None => None,
    };
    parts.next().is_none().then_some((line, col))
}

fn number(part: &[u8]) -> Option<u64> {
    if part.is_empty() || !part.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(part).ok()?.parse().ok()
}

pub fn run(tool: Tool) -> anyhow::Result<ExitCode> {
    // Arguments are checked before connecting, so a usage error is
    // reported the same with or without a server.
    let request = request_for(&tool)?;
    let mut connection = Connection::open()?;
    let response = connection.call(request)?;
    let mut stdout = io::stdout().lock();
    match (&tool, response) {
        (Tool::ListBuffers { json }, Response::Buffers(buffers)) => {
            print_buffers(&mut stdout, &buffers, *json).context("writing the buffer list")?;
        }
        (Tool::Events, Response::Ok) => return stream_events(&mut connection, &mut stdout),
        (Tool::DiffExternal { .. }, Response::Diff(diff)) => {
            stdout
                .write_all(diff.as_bytes())
                .and_then(|()| stdout.flush())
                .context("writing the diff")?;
        }
        (_, Response::Ok) => {}
        (_, other) => bail!("the server answered {other:?}"),
    }
    Ok(ExitCode::SUCCESS)
}

fn request_for(tool: &Tool) -> anyhow::Result<Request> {
    let wire = |path: &Path| WirePath::from(path);
    Ok(match tool {
        Tool::Open { file, wait } => {
            let FileArgument { path, line, col } = parse_file_argument(file, &|path| path.exists());
            Request::Open {
                path: wire(&path),
                line,
                col,
                wait: *wait,
            }
        }
        Tool::ListBuffers { .. } => Request::ListBuffers,
        Tool::Reload { file } => Request::Reload {
            path: file.as_deref().map(|file| wire(Path::new(file))),
        },
        Tool::Goto { target } => {
            let Some(GotoTarget { path, line, col }) =
                parse_goto_target(target, &|path| path.exists())
            else {
                bail!("{GOTO_USAGE}, not {}", Path::new(target).display());
            };
            Request::Goto {
                path: path.as_deref().map(wire),
                line,
                col,
            }
        }
        Tool::SaveAll => Request::SaveAll,
        Tool::Events => Request::SubscribeEvents,
        Tool::DiffExternal { file } => Request::DiffExternal {
            path: wire(Path::new(file)),
        },
        Tool::ApplyPatch { file } => {
            let mut patch = Vec::new();
            io::stdin()
                .read_to_end(&mut patch)
                .context("reading the patch from stdin")?;
            Request::ApplyPatch {
                path: wire(Path::new(file)),
                patch_b64: base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    patch,
                ),
            }
        }
    })
}

/// A CLI client's connection to the running server.
struct Connection {
    writer: UnixStream,
    reader: LineReader<UnixStream>,
}

/// Every tool sends one request.
const REQUEST_ID: u64 = 1;

impl Connection {
    fn open() -> anyhow::Result<Connection> {
        let socket = client::socket_path()?;
        let stream = match UnixStream::connect(&socket) {
            Ok(stream) => stream,
            Err(error) if client::no_server_listens(&error) => bail!(NO_SERVER_RUNNING),
            Err(error) => {
                return Err(error).with_context(|| format!("connecting to {}", socket.display()));
            }
        };
        client::verify_server(&socket, &stream)?;
        let reader = stream.try_clone().context("reading from the server")?;
        let mut connection = Connection {
            writer: stream,
            reader: LineReader::new(reader),
        };
        let cwd = std::env::current_dir().context("finding the current directory")?;
        // A CLI client draws nothing, so it has no size.
        connection.send(&ClientMsg::Hello {
            kind: ClientKind::Cli,
            cwd: WirePath::from(cwd.as_path()),
            cols: 0,
            rows: 0,
            wait_for: None,
        })?;
        match connection.receive()? {
            ServerMsg::Welcome { .. } => Ok(connection),
            other => bail!("the server greeted with {other:?}"),
        }
    }

    fn send(&mut self, message: &ClientMsg) -> anyhow::Result<()> {
        write_msg(&mut self.writer, message).context(CONNECTION_LOST)
    }

    /// The next message; the server ending the connection is an error.
    fn receive(&mut self) -> anyhow::Result<ServerMsg> {
        match self.reader.read_msg::<ServerMsg>() {
            Ok(Some(ServerMsg::Exit { reason })) if reason.is_empty() => bail!(CONNECTION_LOST),
            Ok(Some(ServerMsg::Exit { reason })) => bail!(reason),
            Ok(Some(message)) => Ok(message),
            Ok(None) => bail!(CONNECTION_LOST),
            Err(error) => Err(error).context(CONNECTION_LOST),
        }
    }

    /// Sends `request` and waits for its answer; an error answer is
    /// reported as its message.
    fn call(&mut self, request: Request) -> anyhow::Result<Response> {
        self.send(&ClientMsg::Request {
            id: REQUEST_ID,
            req: request,
        })?;
        loop {
            if let ServerMsg::Response { id, result } = self.receive()?
                && id == REQUEST_ID
            {
                return result.map_err(|error| anyhow::anyhow!(error.message));
            }
        }
    }
}

/// Prints each event as the wire line it came in, protocol version
/// included, so scripts can tell when its shape changes.
fn stream_events(connection: &mut Connection, out: &mut impl Write) -> anyhow::Result<ExitCode> {
    loop {
        let message = connection.receive()?;
        if !matches!(message, ServerMsg::Event { .. }) {
            continue;
        }
        match write_msg(out, &message) {
            Ok(()) => {}
            // The reader went away (`emx events | head`): the stream is done.
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                return Ok(ExitCode::SUCCESS);
            }
            Err(error) => return Err(error).context("writing an event"),
        }
    }
}

fn print_buffers(out: &mut impl Write, buffers: &[BufferInfo], json: bool) -> io::Result<()> {
    if json {
        serde_json::to_writer(&mut *out, buffers).map_err(io::Error::other)?;
        writeln!(out)?;
        return out.flush();
    }
    let rows: Vec<(char, String, String, String)> = buffers
        .iter()
        .map(|buffer| {
            let flag = if buffer.modified {
                MODIFIED_FLAG
            } else {
                UNMODIFIED_FLAG
            };
            let path = buffer.path.clone().map_or_else(String::new, |path| {
                caret_notation(&path.into_path_buf().to_string_lossy())
            });
            (
                flag,
                caret_notation(&buffer.name),
                buffer.size.to_string(),
                path,
            )
        })
        .collect();
    let name_width = rows
        .iter()
        .map(|row| row.1.chars().count())
        .max()
        .unwrap_or_default();
    let size_width = rows.iter().map(|row| row.2.len()).max().unwrap_or_default();
    for (flag, name, size, path) in &rows {
        let line = format!("{flag} {name:<name_width$}  {size:>size_width$}  {path}");
        writeln!(out, "{}", line.trim_end())?;
    }
    out.flush()
}

/// A name with its control characters shown as `^J`, so one buffer is
/// one line.
fn caret_notation(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    for character in text.chars() {
        if character == DELETE {
            shown.push_str(DELETE_CARET);
        } else if character.is_ascii_control() {
            shown.push('^');
            shown.push(char::from(character as u8 + CARET_OFFSET));
        } else {
            shown.push(character);
        }
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exists_none(_: &Path) -> bool {
        false
    }

    #[test]
    fn goto_target_forms() {
        let parse = |text: &str| parse_goto_target(OsStr::new(text), &exists_none);
        assert_eq!(
            parse("12"),
            Some(GotoTarget {
                path: None,
                line: 12,
                col: None
            })
        );
        assert_eq!(
            parse("12:3"),
            Some(GotoTarget {
                path: None,
                line: 12,
                col: Some(3)
            })
        );
        assert_eq!(
            parse("a.rs:12"),
            Some(GotoTarget {
                path: Some(PathBuf::from("a.rs")),
                line: 12,
                col: None
            })
        );
        assert_eq!(
            parse("a.rs:12:3"),
            Some(GotoTarget {
                path: Some(PathBuf::from("a.rs")),
                line: 12,
                col: Some(3)
            })
        );
        assert_eq!(parse("a.rs"), None);
        assert_eq!(parse("1:2:3:"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn goto_target_existing_file_has_no_line() {
        let exists = |path: &Path| path == Path::new("notes:12");
        assert_eq!(parse_goto_target(OsStr::new("notes:12"), &exists), None);
    }

    #[test]
    fn caret_notation_shows_controls() {
        assert_eq!(caret_notation("a\nb\tc\x7f"), "a^Jb^Ic^?");
        assert_eq!(caret_notation("plain \u{e9}"), "plain \u{e9}");
    }

    #[test]
    fn buffer_table() {
        let buffers = vec![
            BufferInfo {
                name: "long-name.txt".to_string(),
                path: Some(WirePath::Text("/w/long-name.txt".to_string())),
                modified: true,
                size: 1234,
            },
            BufferInfo {
                name: "*scratch*".to_string(),
                path: None,
                modified: false,
                size: 0,
            },
        ];
        let mut out = Vec::new();
        print_buffers(&mut out, &buffers, false).expect("print");
        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            "* long-name.txt  1234  /w/long-name.txt\n  *scratch*         0\n"
        );
        let mut out = Vec::new();
        print_buffers(&mut out, &buffers, true).expect("print");
        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            "[{\"name\":\"long-name.txt\",\"path\":\"/w/long-name.txt\",\"modified\":true,\"size\":1234},\
             {\"name\":\"*scratch*\",\"path\":null,\"modified\":false,\"size\":0}]\n"
        );
    }
}
