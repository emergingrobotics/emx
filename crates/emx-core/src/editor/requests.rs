//! The socket API (DESIGN section 2): requests from `emx open`, `emx
//! reload`, `emx events` and the rest, `--wait` and `C-x #`, and the
//! events streamed to subscribers. Requests that need file I/O are
//! answered when its result comes back.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use emx_proto::{
    BufferInfo, ClientKind, EditorEvent, ExternalKind, Request, Response, ServerMsg, WireError,
    WirePath, error_code,
};
use emx_text::{LineLookup, ReadError, Text};

use super::files::{Purpose, SaveFlow};
use super::reload::ReloadFailure;
use super::{Editor, TimerPurpose};
use crate::buffer::BufferId;
use crate::diff::DiffRequester;
use crate::event::{ClientId, Effect, TimerId};

const NO_CLIENT_MESSAGE: &str = "no emx client is attached; run 'emx FILE' in a terminal first";
const BUFFER_MODIFIED_MESSAGE: &str = "buffer modified";
const NO_EXTERNAL_CHANGE_MESSAGE: &str = "no external change recorded";
const NO_SERVER_BUFFERS: &str = "No server editing buffers exist";
const KILLED_WHILE_LOADING: &str = "the buffer was killed before its file was read";
/// How far back from point a cursor event looks for the line start before
/// it reports the column as unknown (0).
const UNKNOWN_COLUMN: u64 = 0;
const UTF8_CONTINUATION_MASK: u8 = 0b1100_0000;
const UTF8_CONTINUATION: u8 = 0b1000_0000;

/// Where to send the answer to a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Reply {
    client: ClientId,
    id: u64,
}

/// An open request waiting for its file to be read.
#[derive(Debug)]
pub(super) struct OpenRequest {
    reply: Reply,
    /// The terminal client the file is shown in.
    terminal: ClientId,
    line: Option<u64>,
    col: Option<u64>,
    wait: bool,
    /// The buffer was created for this request.
    created: bool,
}

/// A request covering several buffers (reload of every buffer, save-all),
/// answered when the last of them is done.
#[derive(Debug)]
pub(super) struct GroupReply {
    reply: Reply,
    remaining: BTreeSet<BufferId>,
    failures: Vec<WireError>,
}

fn wire_error(code: &str, message: impl Into<String>) -> WireError {
    WireError {
        code: code.to_string(),
        message: message.into(),
    }
}

fn wire_path(path: &Path) -> WirePath {
    WirePath::from(path)
}

impl Editor {
    pub(super) fn handle_request(&mut self, client: ClientId, id: u64, request: Request) {
        let reply = Reply { client, id };
        match request {
            Request::Open {
                path,
                line,
                col,
                wait,
            } => self.request_open(reply, path.into_path_buf(), line, col, wait),
            Request::ListBuffers => {
                let buffers = self.buffer_list();
                self.reply(reply, Ok(Response::Buffers(buffers)));
            }
            Request::Reload { path } => {
                self.request_reload(reply, path.map(WirePath::into_path_buf))
            }
            Request::Goto { path, line, col } => match path {
                Some(path) => {
                    self.request_open(reply, path.into_path_buf(), Some(line), col, false)
                }
                None => self.request_goto(reply, line, col),
            },
            Request::SaveAll => self.request_save_all(reply),
            Request::SubscribeEvents => {
                self.subscribe(client);
                self.reply(reply, Ok(Response::Ok));
            }
            Request::DiffExternal { path } => {
                self.request_diff_external(reply, &path.into_path_buf());
            }
        }
    }

    /// Answers are dropped for a client that has gone.
    fn reply(&mut self, reply: Reply, result: Result<Response, WireError>) {
        if self.clients.contains_key(&reply.client) {
            self.effects.push(Effect::Send(
                reply.client,
                ServerMsg::Response {
                    id: reply.id,
                    result,
                },
            ));
        }
    }

    /// A terminal client typed or pasted, or attached.
    pub(super) fn note_terminal_activity(&mut self, client: ClientId) {
        if self.client(client).kind != ClientKind::Tui {
            return;
        }
        self.terminal_recency.retain(|id| *id != client);
        self.terminal_recency.insert(0, client);
    }

    /// The terminal a request shows its buffer in: the requester's own, or
    /// for a CLI client the most recently active one.
    fn request_terminal(&self, client: ClientId) -> Result<ClientId, WireError> {
        if self.client(client).kind == ClientKind::Tui {
            return Ok(client);
        }
        self.terminal_recency
            .first()
            .copied()
            .ok_or_else(|| wire_error(error_code::NO_CLIENT, NO_CLIENT_MESSAGE))
    }

    // ---- open, goto and --wait ---------------------------------------------

    fn request_open(
        &mut self,
        reply: Reply,
        path: PathBuf,
        line: Option<u64>,
        col: Option<u64>,
        wait: bool,
    ) {
        let terminal = match self.request_terminal(reply.client) {
            Ok(terminal) => terminal,
            Err(error) => return self.reply(reply, Err(error)),
        };
        // The server resolves names against the client's directory.
        if !path.is_absolute() {
            return self.reply(
                reply,
                Err(wire_error(
                    error_code::OPEN_FAILED,
                    format!("{} is not an absolute file name", path.display()),
                )),
            );
        }
        let created = self.buffer_visiting(&path).is_none();
        self.visit_file(terminal, path);
        let buffer = self.selected_buffer(terminal);
        let request = OpenRequest {
            reply,
            terminal,
            line,
            col,
            wait,
            created,
        };
        if self.buffers[buffer].loading {
            self.pending_opens.entry(buffer).or_default().push(request);
        } else {
            self.finish_open(buffer, request);
        }
    }

    fn buffer_visiting(&self, path: &Path) -> Option<BufferId> {
        self.buffers
            .iter()
            .find(|(_, buffer)| buffer.file.as_ref().is_some_and(|file| file.path == path))
            .map(|(id, _)| id)
    }

    /// The buffer is ready: go to the line and column, then answer, or
    /// hold the answer until the buffer is done (`--wait`).
    fn finish_open(&mut self, buffer: BufferId, request: OpenRequest) {
        if let Some(line) = request.line
            && let Err(error) = self.goto_in(request.terminal, buffer, line, request.col)
        {
            return self.reply(request.reply, Err(error));
        }
        if request.wait {
            self.waiting.entry(buffer).or_default().push(request.reply);
            if request.created {
                self.created_for_wait.insert(buffer);
            }
        } else {
            self.reply(request.reply, Ok(Response::Ok));
        }
    }

    /// The file of every open request for `buffer` could not be read.
    pub(super) fn fail_pending_opens(&mut self, buffer: BufferId, message: &str) {
        for request in self.pending_opens.remove(&buffer).unwrap_or_default() {
            self.reply(
                request.reply,
                Err(wire_error(error_code::OPEN_FAILED, message)),
            );
        }
    }

    fn finish_pending_opens(&mut self, buffer: BufferId) {
        for request in self.pending_opens.remove(&buffer).unwrap_or_default() {
            self.finish_open(buffer, request);
        }
    }

    fn request_goto(&mut self, reply: Reply, line: u64, col: Option<u64>) {
        let result = self.request_terminal(reply.client).and_then(|terminal| {
            let buffer = self.selected_buffer(terminal);
            self.goto_in(terminal, buffer, line, col)
        });
        self.reply(reply, result.map(|()| Response::Ok));
    }

    /// Moves the terminal's point to 1-based `line` and `col` (counted in
    /// characters) in `buffer`, if its selected window still shows it. A
    /// line past the end goes to the end, a column past the line's end to
    /// the line's end, as `emacsclient +LINE:COL` does.
    fn goto_in(
        &mut self,
        terminal: ClientId,
        buffer: BufferId,
        line: u64,
        col: Option<u64>,
    ) -> Result<(), WireError> {
        if !self.clients.contains_key(&terminal) || self.selected_buffer(terminal) != buffer {
            return Ok(());
        }
        let text = self.buffers[buffer].text.clone();
        let failed = |error: ReadError| wire_error(error_code::GOTO_FAILED, error.to_string());
        let start = match text.line_start(line.saturating_sub(1)).map_err(failed)? {
            LineLookup::Found(start) => start,
            LineLookup::PastEnd => text.len(),
            LineLookup::Unknown => {
                return Err(wire_error(
                    error_code::GOTO_FAILED,
                    format!("line {line} is not indexed yet"),
                ));
            }
        };
        let position = match col {
            Some(col) => advance_characters(&text, start, col.saturating_sub(1)).map_err(failed)?,
            None => start,
        };
        let window = self.selected_window_id(terminal);
        self.set_window_point(terminal, window, position);
        Ok(())
    }

    /// Emacs `server-edit` (`C-x #`): the selected buffer is done. A
    /// modified buffer is offered for saving first.
    pub(crate) fn server_edit_command(&mut self, client: ClientId) {
        if self.waiting.is_empty() {
            self.set_echo(client, NO_SERVER_BUFFERS.to_string());
            return;
        }
        let buffer = self.selected_buffer(client);
        if !self.waiting.contains_key(&buffer) {
            if let Some(next) = self.next_server_buffer(None) {
                self.show_in_selected_window(client, next);
            }
            return;
        }
        let target = &self.buffers[buffer];
        if let Some(file) = target.file.as_ref().filter(|_| target.is_modified()) {
            let question = format!("Save file {}? ", file.path.display());
            self.y_or_n_p(client, question, Purpose::ServerDone { buffer });
            return;
        }
        self.server_buffer_done(client, buffer);
    }

    /// Emacs `server-buffer-done`: releases the clients waiting for the
    /// buffer, kills it if it was created for them and is unmodified, and
    /// shows the next buffer a client waits for.
    pub(super) fn server_buffer_done(&mut self, client: ClientId, buffer: BufferId) {
        for reply in self.waiting.remove(&buffer).unwrap_or_default() {
            self.reply(reply, Ok(Response::Ok));
        }
        let created = self.created_for_wait.remove(&buffer);
        let unmodified = self
            .buffers
            .get(buffer)
            .is_some_and(|target| !target.is_modified());
        if created && unmodified {
            self.kill_buffer(buffer);
        }
        if !self.clients.contains_key(&client) {
            return;
        }
        if let Some(next) = self.next_server_buffer(Some(buffer)) {
            self.show_in_selected_window(client, next);
        } else if self.selected_buffer(client) == buffer {
            let other = self.other_buffer(buffer);
            self.show_in_selected_window(client, other);
        }
    }

    /// The most recently shown buffer a client waits for.
    fn next_server_buffer(&self, excluding: Option<BufferId>) -> Option<BufferId> {
        self.recency
            .iter()
            .copied()
            .find(|id| Some(*id) != excluding && self.waiting.contains_key(id))
    }

    // ---- list-buffers ------------------------------------------------------

    fn buffer_list(&self) -> Vec<BufferInfo> {
        self.recency
            .iter()
            .map(|id| {
                let buffer = &self.buffers[*id];
                BufferInfo {
                    name: buffer.name.clone(),
                    path: buffer.file.as_ref().map(|file| wire_path(&file.path)),
                    modified: buffer.is_modified(),
                    size: buffer.text.len(),
                }
            })
            .collect()
    }

    // ---- reload --------------------------------------------------------------

    /// `emx reload`: an unmodified buffer is read again from disk through
    /// the one reload path; a modified one is left alone and reported. Without a file, every
    /// file buffer is reloaded.
    fn request_reload(&mut self, reply: Reply, path: Option<PathBuf>) {
        let mut failures = Vec::new();
        let candidates: Vec<BufferId> = match &path {
            Some(path) => match self.buffer_visiting(path) {
                Some(buffer) => vec![buffer],
                None => {
                    return self.reply(
                        reply,
                        Err(wire_error(
                            error_code::NO_BUFFER,
                            format!("no buffer visits {}", path.display()),
                        )),
                    );
                }
            },
            None => self
                .recency
                .iter()
                .copied()
                .filter(|id| self.buffers[*id].file.is_some())
                .collect(),
        };
        let single = path.is_some();
        let mut remaining = BTreeSet::new();
        for buffer in candidates {
            let target = &self.buffers[buffer];
            let file = target.file.as_ref().expect("candidates visit files");
            let refusal = if target.is_modified() {
                Some((error_code::BUFFER_MODIFIED, BUFFER_MODIFIED_MESSAGE))
            } else if target.loading || target.damaged || file.saving {
                Some((error_code::RELOAD_FAILED, "buffer is busy"))
            } else {
                None
            };
            if let Some((code, message)) = refusal {
                let message = if single {
                    message.to_string()
                } else {
                    format!("{}: {message}", target.name)
                };
                failures.push(wire_error(code, message));
                continue;
            }
            if self.reverting.insert(buffer) {
                self.request_reload_of(buffer);
            }
            remaining.insert(buffer);
        }
        let group = GroupReply {
            reply,
            remaining,
            failures,
        };
        self.start_group(group, GroupKind::Reload);
    }

    /// A reload for a request finished; `outcome` says why it was not
    /// applied.
    pub(super) fn reload_request_done(
        &mut self,
        buffer: BufferId,
        outcome: Result<(), ReloadFailure>,
    ) {
        let outcome = outcome.map_err(|failure| match failure {
            ReloadFailure::Modified => {
                let name = self
                    .buffers
                    .get(buffer)
                    .map(|target| target.name.clone())
                    .unwrap_or_default();
                wire_error(
                    error_code::BUFFER_MODIFIED,
                    format!("{name}: {BUFFER_MODIFIED_MESSAGE}"),
                )
            }
            ReloadFailure::Failed(message) => wire_error(error_code::RELOAD_FAILED, message),
        });
        self.group_buffer_done(GroupKind::Reload, buffer, outcome);
    }

    // ---- diff-external -------------------------------------------------------

    fn request_diff_external(&mut self, reply: Reply, path: &Path) {
        let Some(buffer) = self.buffer_visiting(path) else {
            return self.reply(
                reply,
                Err(wire_error(
                    error_code::NO_BUFFER,
                    format!("no buffer visits {}", path.display()),
                )),
            );
        };
        match self.diff_job(buffer, DiffRequester::Request(reply.client, reply.id)) {
            Ok(job) => self.effects.push(Effect::Diff(job)),
            Err(_) => self.reply(
                reply,
                Err(wire_error(
                    error_code::NO_EXTERNAL_CHANGE,
                    NO_EXTERNAL_CHANGE_MESSAGE,
                )),
            ),
        }
    }

    pub(super) fn diff_request_done(
        &mut self,
        client: ClientId,
        id: u64,
        result: Result<String, String>,
    ) {
        let result = result
            .map(Response::Diff)
            .map_err(|message| wire_error(error_code::DIFF_FAILED, message));
        self.reply(Reply { client, id }, result);
    }

    // ---- save-all ------------------------------------------------------------

    /// Saves every modified file buffer, never asking: a save that would
    /// ask (the file changed on disk, is write-protected, ...) is reported
    /// instead, so an agent never overwrites a file the user did not
    /// confirm.
    fn request_save_all(&mut self, reply: Reply) {
        let candidates: Vec<BufferId> = self
            .recency
            .iter()
            .copied()
            .filter(|id| {
                let target = &self.buffers[*id];
                target.file.is_some() && target.is_modified()
            })
            .collect();
        let mut failures = Vec::new();
        let mut remaining = BTreeSet::new();
        for buffer in candidates {
            let name = self.buffers[buffer].name.clone();
            match self.continue_save(reply.client, SaveFlow::unattended(buffer)) {
                Ok(()) if self.pending_saves.contains_key(&buffer) => {
                    remaining.insert(buffer);
                }
                Ok(()) => {}
                Err(error) => failures.push(wire_error(
                    error_code::SAVE_FAILED,
                    format!("{name}: {error}"),
                )),
            }
        }
        let group = GroupReply {
            reply,
            remaining,
            failures,
        };
        self.start_group(group, GroupKind::Save);
    }

    /// A save finished; `outcome` names the file on failure.
    pub(super) fn save_completed(&mut self, buffer: BufferId, outcome: Result<(), String>) {
        let outcome = outcome.map_err(|message| wire_error(error_code::SAVE_FAILED, message));
        self.group_buffer_done(GroupKind::Save, buffer, outcome);
    }

    // ---- requests over several buffers ---------------------------------------

    fn groups(&mut self, kind: GroupKind) -> &mut Vec<GroupReply> {
        match kind {
            GroupKind::Reload => &mut self.reload_replies,
            GroupKind::Save => &mut self.save_replies,
        }
    }

    fn start_group(&mut self, group: GroupReply, kind: GroupKind) {
        if group.remaining.is_empty() {
            self.answer_group(group);
        } else {
            self.groups(kind).push(group);
        }
    }

    fn group_buffer_done(
        &mut self,
        kind: GroupKind,
        buffer: BufferId,
        outcome: Result<(), WireError>,
    ) {
        let groups = self.groups(kind);
        let mut finished = Vec::new();
        let mut index = 0;
        while index < groups.len() {
            let group = &mut groups[index];
            if group.remaining.remove(&buffer) {
                if let Err(error) = &outcome {
                    group.failures.push(error.clone());
                }
                if group.remaining.is_empty() {
                    finished.push(groups.remove(index));
                    continue;
                }
            }
            index += 1;
        }
        for group in finished {
            self.answer_group(group);
        }
    }

    /// One failure is reported as it is; several under the first one's
    /// code, their messages joined.
    fn answer_group(&mut self, group: GroupReply) {
        let mut failures = group.failures.into_iter();
        let result = match failures.next() {
            None => Ok(Response::Ok),
            Some(first) => {
                let mut message = first.message;
                for other in failures {
                    message.push_str("; ");
                    message.push_str(&other.message);
                }
                Err(WireError {
                    code: first.code,
                    message,
                })
            }
        };
        self.reply(group.reply, result);
    }

    // ---- events ----------------------------------------------------------------

    fn subscribe(&mut self, client: ClientId) {
        // Moves are reported from here on, not where cursors already are.
        let terminals: Vec<ClientId> = self.terminal_recency.clone();
        for terminal in terminals {
            let current = self.cursor_of(terminal);
            self.client_mut(terminal).cursor_reported = Some(current);
        }
        self.subscribers.insert(client);
    }

    fn emit(&mut self, event: EditorEvent) {
        for subscriber in &self.subscribers {
            self.effects.push(Effect::Send(
                *subscriber,
                ServerMsg::Event {
                    event: event.clone(),
                },
            ));
        }
    }

    fn buffer_event_fields(&self, buffer: BufferId) -> (Option<WirePath>, String) {
        let target = &self.buffers[buffer];
        (
            target.file.as_ref().map(|file| wire_path(&file.path)),
            target.name.clone(),
        )
    }

    /// A visited file changed on disk and the editor acted on it.
    pub(super) fn external_change_event(&mut self, buffer: BufferId, kind: ExternalKind) {
        let Some(path) = self.buffers[buffer]
            .file
            .as_ref()
            .map(|file| wire_path(&file.path))
        else {
            return;
        };
        self.emit(EditorEvent::ExternalChange { path, kind });
    }

    /// A visited file was read into its buffer.
    pub(super) fn buffer_opened(&mut self, buffer: BufferId) {
        let (path, name) = self.buffer_event_fields(buffer);
        self.emit(EditorEvent::BufferOpened { path, name });
        self.finish_pending_opens(buffer);
    }

    pub(super) fn buffer_saved(&mut self, buffer: BufferId) {
        let (path, name) = self.buffer_event_fields(buffer);
        self.emit(EditorEvent::BufferSaved { path, name });
    }

    /// The buffer is about to be killed: clients waiting for it are
    /// released, as killing a server buffer does in Emacs. Opens still
    /// waiting for its file are answered too, or they would never be: a
    /// `--wait` is released, a plain open fails as its file was never shown.
    pub(super) fn buffer_killed(&mut self, buffer: BufferId) {
        for reply in self.waiting.remove(&buffer).unwrap_or_default() {
            self.reply(reply, Ok(Response::Ok));
        }
        for request in self.pending_opens.remove(&buffer).unwrap_or_default() {
            let result = if request.wait {
                Ok(Response::Ok)
            } else {
                Err(wire_error(error_code::OPEN_FAILED, KILLED_WHILE_LOADING))
            };
            self.reply(request.reply, result);
        }
        self.created_for_wait.remove(&buffer);
        // A buffer whose file could not be read was never opened.
        if !self.buffers[buffer].loading {
            let (path, name) = self.buffer_event_fields(buffer);
            self.emit(EditorEvent::BufferKilled { path, name });
        }
    }

    pub(super) fn client_attached_event(&mut self, client: ClientId) {
        self.emit(EditorEvent::ClientAttached { client: client.0 });
    }

    /// Forgets what the client asked for; its answers have nowhere to go.
    pub(super) fn client_detached(&mut self, client: ClientId, kind: ClientKind) {
        self.subscribers.remove(&client);
        self.terminal_recency.retain(|id| *id != client);
        for replies in self.waiting.values_mut() {
            replies.retain(|reply| reply.client != client);
        }
        self.waiting.retain(|_, replies| !replies.is_empty());
        let waited: BTreeSet<BufferId> = self.waiting.keys().copied().collect();
        self.created_for_wait
            .retain(|buffer| waited.contains(buffer));
        if kind == ClientKind::Tui {
            self.emit(EditorEvent::ClientDetached { client: client.0 });
        }
    }

    fn cursor_of(&self, client: ClientId) -> (BufferId, u64) {
        let window = self.selected_window_id(client);
        (
            self.window(client, window).buffer,
            self.window_point(client, window),
        )
    }

    /// Reports terminal cursors that moved, each at most once per
    /// `cursor_event_interval_ms`; a move inside the interval is reported
    /// when it ends.
    pub(super) fn report_cursors(&mut self) {
        if self.subscribers.is_empty() {
            return;
        }
        let terminals: Vec<ClientId> = self.terminal_recency.clone();
        for terminal in terminals {
            let current = self.cursor_of(terminal);
            let state = self.client(terminal);
            if state.cursor_reported == Some(current) || state.cursor_timer.is_some() {
                continue;
            }
            let event = self.cursor_event(terminal, current);
            self.emit(event);
            let timer = self.set_timer(
                TimerPurpose::CursorEvent(terminal),
                self.config.cursor_event_interval_ms,
            );
            let state = self.client_mut(terminal);
            state.cursor_reported = Some(current);
            state.cursor_timer = Some(timer);
        }
    }

    pub(super) fn cursor_timer_expired(&mut self, client: ClientId, id: TimerId) {
        if let Some(state) = self.clients.get_mut(&client)
            && state.cursor_timer == Some(id)
        {
            state.cursor_timer = None;
        }
    }

    /// Line and column (1-based, the column in characters) come from
    /// cached text only: an event never waits for the disk. What is not
    /// cached is reported unknown.
    fn cursor_event(&self, client: ClientId, (buffer, offset): (BufferId, u64)) -> EditorEvent {
        let target = &self.buffers[buffer];
        let line = target
            .text
            .line_of_cached(offset)
            .ok()
            .flatten()
            .map(|line| line + 1);
        let col = cached_column(&target.text, offset, self.config.long_line_threshold)
            .map_or(UNKNOWN_COLUMN, |column| column + 1);
        EditorEvent::CursorMoved {
            client: client.0,
            path: target.file.as_ref().map(|file| wire_path(&file.path)),
            line,
            col,
            offset,
        }
    }

    // ---- worker results without a request -------------------------------------

    /// A prefetched block arrived; the redisplay that follows draws it. A
    /// failed read is shown to the clients looking at the buffer.
    pub(super) fn block_ready(&mut self, buffer: BufferId, result: Result<(), String>) {
        let Err(message) = result else {
            return;
        };
        let Some(target) = self.buffers.get(buffer) else {
            return;
        };
        let text = format!("Reading {}: {message}", target.name);
        let viewers: Vec<ClientId> = self
            .clients
            .iter()
            .filter(|(_, state)| state.kind == ClientKind::Tui)
            .filter(|(_, state)| {
                state
                    .frame
                    .windows
                    .values()
                    .any(|window| window.buffer == buffer)
            })
            .map(|(id, _)| *id)
            .collect();
        self.show_echo_to(&viewers, text);
    }

    pub(super) fn index_finished(&mut self, buffer: BufferId, result: Result<(), String>) {
        let Err(message) = result else {
            return;
        };
        let Some(target) = self.buffers.get(buffer) else {
            return;
        };
        let text = format!("Counting the lines of {} failed: {message}", target.name);
        let terminals: Vec<ClientId> = self.terminal_recency.clone();
        self.show_echo_to(&terminals, text);
    }
}

#[derive(Debug, Clone, Copy)]
enum GroupKind {
    Reload,
    Save,
}

/// The position `count` characters after `start`, stopping at the end of
/// the line.
fn advance_characters(text: &Text, start: u64, count: u64) -> Result<u64, ReadError> {
    let mut position = start;
    let mut remaining = count;
    for chunk in text.chunks(start..text.len()) {
        let chunk = chunk?;
        for byte in chunk.bytes() {
            let starts_character = byte & UTF8_CONTINUATION_MASK != UTF8_CONTINUATION;
            if starts_character {
                if remaining == 0 || *byte == b'\n' {
                    return Ok(position);
                }
                remaining -= 1;
            }
            position += 1;
        }
    }
    Ok(position)
}

/// Characters between the start of `offset`'s line and `offset`, from
/// cached text, or `None` when that needs a read or the line start is more
/// than `limit` bytes back.
fn cached_column(text: &Text, offset: u64, limit: u64) -> Option<u64> {
    let from = offset.saturating_sub(limit);
    let mut bytes = Vec::new();
    text.read_cached(from..offset, &mut bytes).ok()?;
    let line = match bytes.iter().rposition(|byte| *byte == b'\n') {
        Some(newline) => &bytes[newline + 1..],
        None if from == 0 => &bytes[..],
        None => return None,
    };
    Some(
        line.iter()
            .filter(|byte| **byte & UTF8_CONTINUATION_MASK != UTF8_CONTINUATION)
            .count() as u64,
    )
}
