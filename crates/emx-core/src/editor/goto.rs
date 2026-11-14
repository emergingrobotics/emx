//! The `goto-line` minibuffer read and its answer, and the background line
//! indexer's effect on line numbers and a `goto-line` that outran the index.

use super::{Editor, PendingGoto, Purpose};
use crate::buffer::{BufferId, IndexingProgress};
use crate::command::{self, CommandError};
use crate::event::{ClientId, Effect};
use crate::minibuffer::{HistoryKind, Mode};
use crate::window::WindowId;

const GOTO_LINE_PROMPT: &str = "Goto line: ";
const INVALID_NUMBER: &str = "Invalid number";

impl Editor {
    pub(crate) fn read_goto_line(&mut self, client_id: ClientId) {
        let mode = Mode::Line {
            history: HistoryKind::GotoLine,
            defaults: Vec::new(),
            require: None,
            allow_empty: false,
        };
        self.read_from_minibuffer(
            client_id,
            GOTO_LINE_PROMPT.to_string(),
            Vec::new(),
            mode,
            Purpose::GotoLine,
        );
    }

    pub(super) fn goto_line_answered(&mut self, client_id: ClientId, input: Vec<u8>) {
        match String::from_utf8_lossy(&input).trim().parse::<i64>() {
            Ok(line) => {
                self.run_command(client_id, "goto-line", None, move |ctx| {
                    command::goto_line_to(ctx, line)
                });
            }
            Err(_) => self.fail(client_id, &CommandError::User(INVALID_NUMBER.to_string())),
        }
    }

    /// Resets any recorded indexing progress for `buffer` and asks for its
    /// line index to be built. Every `Effect::StartIndex` goes through here so
    /// a re-indexed buffer (a reload, a save that rewrote the file) does not
    /// show stale progress from the previous scan.
    pub(super) fn start_indexing(&mut self, buffer: BufferId) {
        if let Some(target) = self.buffers.get_mut(buffer) {
            target.indexing = None;
        }
        self.effects.push(Effect::StartIndex { buffer });
    }

    /// Records a line indexer's progress: the piece newline counts are
    /// refreshed so a point within the newly counted prefix gets a real line
    /// number, the per-buffer progress snapshot is updated for the modeline,
    /// and any `goto-line` waiting on this buffer is retried.
    pub(super) fn index_progress(
        &mut self,
        buffer: BufferId,
        indexed_blocks: u64,
        total_blocks: u64,
        complete: bool,
    ) {
        let Some(target) = self.buffers.get_mut(buffer) else {
            return;
        };
        // Refreshing fills piece newline counts that the index now knows. A
        // partly covered edge block that is not cached fails; the next
        // progress report tries again, so the error is dropped here.
        if let Ok(refreshed) = target.text.refresh_newlines() {
            target.text = refreshed;
        }
        target.indexing = if complete {
            None
        } else {
            Some(IndexingProgress {
                indexed_blocks,
                total_blocks,
            })
        };
        self.resolve_pending_gotos(buffer);
    }

    /// Enters the pending state for a `goto-line` whose target line lies
    /// beyond the counted prefix: the client waits, with the echo area
    /// showing how far the indexer has reached.
    pub(crate) fn enter_pending_goto(
        &mut self,
        client: ClientId,
        buffer: BufferId,
        window: WindowId,
        line: i64,
    ) {
        self.client_mut(client).pending_goto = Some(PendingGoto {
            buffer,
            window,
            line,
        });
        let message = self.pending_goto_message(buffer, line);
        self.set_echo(client, message);
    }

    /// `C-g` while a `goto-line` waits for the indexer: the jump is abandoned.
    /// The indexer keeps running, since its line counts still serve the
    /// modeline and any later `goto-line`.
    pub(crate) fn cancel_pending_goto(&mut self, client: ClientId) {
        self.client_mut(client).pending_goto = None;
    }

    fn resolve_pending_gotos(&mut self, buffer: BufferId) {
        let clients: Vec<ClientId> = self
            .clients
            .iter()
            .filter(|(_, client)| {
                client
                    .pending_goto
                    .is_some_and(|goto| goto.buffer == buffer)
            })
            .map(|(id, _)| *id)
            .collect();
        for client in clients {
            self.resolve_pending_goto(client);
        }
    }

    fn resolve_pending_goto(&mut self, client: ClientId) {
        let Some(goto) = self.client(client).pending_goto else {
            return;
        };
        // The window may have gone, or moved to another buffer, while the
        // jump waited; then there is nowhere to land and the wait ends.
        let window_lands = self
            .client(client)
            .frame
            .windows
            .get(goto.window)
            .is_some_and(|window| window.buffer == goto.buffer);
        if !window_lands {
            self.client_mut(client).pending_goto = None;
            return;
        }
        let text = self.buffers[goto.buffer].text.clone();
        match command::goto_line_offset(&text, goto.line) {
            Ok(Some(offset)) => {
                self.client_mut(client).pending_goto = None;
                self.set_window_point(client, goto.window, offset);
                self.set_echo(client, String::new());
            }
            Ok(None) => {
                let message = self.pending_goto_message(goto.buffer, goto.line);
                self.set_echo(client, message);
            }
            // A block needed to place the line was not readable this time;
            // the next progress report retries.
            Err(_) => {}
        }
    }

    fn pending_goto_message(&self, buffer: BufferId, line: i64) -> String {
        let percent = self
            .buffers
            .get(buffer)
            .and_then(|target| target.indexing)
            .map_or(0, IndexingProgress::percent);
        format!("Indexing to line {line}... {percent}%")
    }
}
