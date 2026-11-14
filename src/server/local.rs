//! The in-process connection of `emx --no-server`: the terminal client
//! talks to an editor thread in the same process through channels instead
//! of a socket, with the same messages, outbox bounds and resyncs as a
//! socket client.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::thread;

use emx_core::ClientId;
use emx_proto::{ClientMsg, LineReader, ServerMsg};

use super::Input;
use super::client_conn::{Next, Outbox, check_size, to_event};

/// The one client of a `--no-server` editor.
pub const LOCAL_CLIENT: ClientId = ClientId(1);

/// Receives each message for the client, then `None` once the editor
/// let it go.
pub type Deliver = Box<dyn FnMut(Option<ServerMsg>) + Send>;

pub struct LocalLink {
    inputs: Sender<Input>,
    outbox: Arc<Outbox>,
    cwd: Option<PathBuf>,
}

impl LocalLink {
    /// A connection to the editor thread reading `inputs`; what the editor
    /// sends goes to `deliver` from a thread of its own.
    pub fn new(inputs: Sender<Input>, mut deliver: Deliver) -> io::Result<LocalLink> {
        let outbox = Arc::new(Outbox::default());
        let forward_outbox = outbox.clone();
        let forward_inputs = inputs.clone();
        thread::Builder::new()
            .name("emx-local".to_string())
            .spawn(move || {
                forward(&forward_outbox, &forward_inputs, &mut deliver);
                deliver(None);
            })?;
        Ok(LocalLink {
            inputs,
            outbox,
            cwd: None,
        })
    }

    /// Sends one message as a socket client would; fails once the editor
    /// thread is gone. Messages a socket server would refuse are refused
    /// here too.
    pub fn send(&mut self, message: ClientMsg) -> io::Result<()> {
        let input = match (message, &self.cwd) {
            (
                ClientMsg::Hello {
                    kind,
                    cwd,
                    cols,
                    rows,
                    wait_for: None,
                },
                None,
            ) => {
                check_size(cols, rows).map_err(io::Error::other)?;
                let cwd = cwd.into_path_buf();
                if !cwd.is_absolute() {
                    return Err(io::Error::other(format!(
                        "cwd {} is not absolute",
                        cwd.display()
                    )));
                }
                self.cwd = Some(cwd);
                self.outbox.push(ServerMsg::Welcome {
                    server_pid: std::process::id(),
                });
                Input::Attach {
                    client: LOCAL_CLIENT,
                    kind,
                    cols,
                    rows,
                    outbox: self.outbox.clone(),
                }
            }
            (message @ ClientMsg::Hello { .. }, _) => {
                return Err(io::Error::other(format!("refused {message:?}")));
            }
            (_, None) => return Err(io::Error::other("the first message must be hello")),
            (message, Some(cwd)) => match to_event(LOCAL_CLIENT, message, cwd) {
                Ok(event) => Input::Client(LOCAL_CLIENT, event),
                Err(reason) => Input::ProtocolError(LOCAL_CLIENT, reason),
            },
        };
        self.inputs
            .send(input)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the editor has stopped"))
    }

    /// Ends the session as SIGTERM ends a server: journals are synced and
    /// kept for the next session.
    pub fn terminate(&self) -> io::Result<()> {
        self.inputs
            .send(Input::Terminate)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the editor has stopped"))
    }
}

fn forward(outbox: &Outbox, inputs: &Sender<Input>, deliver: &mut Deliver) {
    loop {
        match outbox.next() {
            Next::Line(line) => {
                let message = LineReader::new(line.as_slice())
                    .read_msg::<ServerMsg>()
                    .expect("the outbox holds what write_msg encoded")
                    .expect("one message per line");
                deliver(Some(message));
            }
            Next::Resync => {
                if inputs.send(Input::Resync(LOCAL_CLIENT)).is_err() {
                    return;
                }
            }
            Next::Closed => return,
        }
    }
}
