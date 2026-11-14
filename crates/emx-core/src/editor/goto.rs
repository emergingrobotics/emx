//! The `goto-line` minibuffer read and its answer.

use super::{Editor, Purpose};
use crate::command::{self, CommandError};
use crate::event::ClientId;
use crate::minibuffer::{HistoryKind, Mode};

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
                    let point = ctx.point();
                    command::push_mark(ctx, point, true, false);
                    let text = ctx.buffer().text.clone();
                    ctx.set_point(command::line_number_position(&text, line)?);
                    Ok(())
                });
            }
            Err(_) => self.fail(client_id, &CommandError::User(INVALID_NUMBER.to_string())),
        }
    }
}
