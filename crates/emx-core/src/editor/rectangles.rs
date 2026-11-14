//! The killed-rectangle store and the `string-rectangle` minibuffer read.
//! The rectangle geometry and edits live in `command::rect`.

use super::{Editor, Purpose};
use crate::command;
use crate::event::ClientId;
use crate::minibuffer::{HistoryKind, Mode};

const STRING_RECTANGLE_PROMPT: &str = "String rectangle: ";

impl Editor {
    pub(crate) fn killed_rectangle(&self) -> Option<Vec<Vec<u8>>> {
        self.killed_rectangle.clone()
    }

    pub(crate) fn set_killed_rectangle(&mut self, rectangle: Vec<Vec<u8>>) {
        self.killed_rectangle = Some(rectangle);
    }

    /// `C-x r t`: read the replacement string for the rectangle between
    /// `start` and `end`.
    pub(crate) fn read_string_rectangle(&mut self, client_id: ClientId, start: u64, end: u64) {
        let mode = Mode::Line {
            history: HistoryKind::StringRectangle,
            defaults: Vec::new(),
            require: None,
            allow_empty: true,
        };
        self.read_from_minibuffer(
            client_id,
            STRING_RECTANGLE_PROMPT.to_string(),
            Vec::new(),
            mode,
            Purpose::StringRectangle { start, end },
        );
    }

    pub(super) fn string_rectangle_answered(
        &mut self,
        client_id: ClientId,
        start: u64,
        end: u64,
        string: Vec<u8>,
    ) {
        self.run_command(client_id, "string-rectangle", None, move |ctx| {
            command::apply_string_rectangle(ctx, start, end, &string)
        });
    }
}
