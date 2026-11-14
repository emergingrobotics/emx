mod edit;
mod files;
mod kill;
mod mark;
mod misc;
mod r#move;
mod window;
mod word_syntax;

pub(crate) use kill::xterm_paste;
pub(crate) use misc::RecenterPosition;

use emx_text::ReadError;
use emx_types::Key;

use std::ops::Range;

use crate::buffer::{Buffer, BufferId, ChangeRecorder, EditError};
use crate::editor::Editor;
use crate::event::ClientId;
use crate::killring::KillRing;
use crate::marker::InsertionType;
use crate::undo::UndoError;
use crate::window::{ScreenLine, WindowId};

/// Emacs `user-error` from `region-beginning` and the region commands.
const NO_REGION: &str = "The mark is not set now, so there is no region";

/// Emacs `prefix-arg` in its raw forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrefixArg {
    #[default]
    None,
    /// `(N)`: from `C-u` (4), `C-u C-u` (16), ..., or `C-u` after `-` (-4).
    Raw(i64),
    Numeric(i64),
    /// `-`: from `M--` or `C-u -` with no digits yet.
    Negative,
}

impl PrefixArg {
    /// Emacs `prefix-numeric-value`.
    pub fn numeric_value(&self) -> i64 {
        match self {
            PrefixArg::None => 1,
            PrefixArg::Raw(value) | PrefixArg::Numeric(value) => *value,
            PrefixArg::Negative => -1,
        }
    }

    /// Emacs `universal-argument--description`, the echo of a pending
    /// argument: `C-u 5` even when it was typed as `M-5`.
    pub fn description(&self) -> Option<String> {
        const RAW_STEP: i64 = 4;
        match self {
            PrefixArg::None => None,
            PrefixArg::Negative => Some("C-u -".to_string()),
            PrefixArg::Numeric(value) => Some(format!("C-u {value}")),
            PrefixArg::Raw(original) => {
                let mut value = *original;
                let mut repeats = String::new();
                while value > RAW_STEP && value % RAW_STEP == 0 {
                    repeats.push_str(" C-u");
                    value /= RAW_STEP;
                }
                if value == RAW_STEP {
                    Some(format!("C-u{repeats}"))
                } else {
                    Some(format!("C-u ({original})"))
                }
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("Quit")]
    Quit,
    #[error("Beginning of buffer")]
    BeginningOfBuffer,
    #[error("End of buffer")]
    EndOfBuffer,
    #[error("Buffer is read-only: #<buffer {0}>")]
    BufferReadOnly(String),
    #[error("{0}")]
    Edit(#[from] EditError),
    #[error("{0}")]
    Read(#[from] ReadError),
    #[error("{0}")]
    User(String),
}

pub struct CommandSpec {
    pub name: &'static str,
    pub run: fn(&mut CommandContext) -> Result<(), CommandError>,
}

static COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "forward-char",
        run: r#move::forward_char,
    },
    CommandSpec {
        name: "backward-char",
        run: r#move::backward_char,
    },
    CommandSpec {
        name: "next-line",
        run: r#move::next_line,
    },
    CommandSpec {
        name: "previous-line",
        run: r#move::previous_line,
    },
    CommandSpec {
        name: "move-beginning-of-line",
        run: r#move::move_beginning_of_line,
    },
    CommandSpec {
        name: "move-end-of-line",
        run: r#move::move_end_of_line,
    },
    CommandSpec {
        name: "forward-word",
        run: r#move::forward_word,
    },
    CommandSpec {
        name: "backward-word",
        run: r#move::backward_word,
    },
    CommandSpec {
        name: "beginning-of-buffer",
        run: r#move::beginning_of_buffer,
    },
    CommandSpec {
        name: "end-of-buffer",
        run: r#move::end_of_buffer,
    },
    CommandSpec {
        name: "self-insert-command",
        run: edit::self_insert_command,
    },
    CommandSpec {
        name: "newline",
        run: edit::newline,
    },
    CommandSpec {
        name: "delete-char",
        run: edit::delete_char,
    },
    CommandSpec {
        name: "delete-backward-char",
        run: edit::delete_backward_char,
    },
    CommandSpec {
        name: "undo",
        run: edit::undo,
    },
    CommandSpec {
        name: "undo-redo",
        run: edit::undo_redo,
    },
    CommandSpec {
        name: "kill-line",
        run: kill::kill_line,
    },
    CommandSpec {
        name: "kill-word",
        run: kill::kill_word,
    },
    CommandSpec {
        name: "kill-region",
        run: kill::kill_region_command,
    },
    CommandSpec {
        name: "kill-ring-save",
        run: kill::kill_ring_save,
    },
    CommandSpec {
        name: "yank",
        run: kill::yank,
    },
    CommandSpec {
        name: "yank-pop",
        run: kill::yank_pop,
    },
    CommandSpec {
        name: "set-mark-command",
        run: mark::set_mark_command,
    },
    CommandSpec {
        name: "exchange-point-and-mark",
        run: mark::exchange_point_and_mark,
    },
    CommandSpec {
        name: "universal-argument",
        run: misc::universal_argument,
    },
    CommandSpec {
        name: "universal-argument-more",
        run: misc::universal_argument_more,
    },
    CommandSpec {
        name: "digit-argument",
        run: misc::digit_argument,
    },
    CommandSpec {
        name: "negative-argument",
        run: misc::negative_argument,
    },
    CommandSpec {
        name: "keyboard-quit",
        run: misc::keyboard_quit,
    },
    CommandSpec {
        name: "scroll-up-command",
        run: misc::scroll_up_command,
    },
    CommandSpec {
        name: "scroll-down-command",
        run: misc::scroll_down_command,
    },
    CommandSpec {
        name: "recenter-top-bottom",
        run: misc::recenter_top_bottom,
    },
    CommandSpec {
        name: "find-file",
        run: files::find_file,
    },
    CommandSpec {
        name: "save-buffer",
        run: files::save_buffer,
    },
    CommandSpec {
        name: "write-file",
        run: files::write_file,
    },
    CommandSpec {
        name: "save-some-buffers",
        run: files::save_some_buffers,
    },
    CommandSpec {
        name: "kill-buffer",
        run: files::kill_buffer,
    },
    CommandSpec {
        name: "save-buffers-kill-terminal",
        run: files::save_buffers_kill_terminal,
    },
    CommandSpec {
        name: "server-edit",
        run: files::server_edit,
    },
    CommandSpec {
        name: "revert-buffer",
        run: files::revert_buffer,
    },
    CommandSpec {
        name: "keep-mine",
        run: files::keep_mine,
    },
    CommandSpec {
        name: "diff-external",
        run: files::diff_external,
    },
    CommandSpec {
        name: "follow-edit-mode",
        run: files::follow_edit_mode,
    },
    CommandSpec {
        name: "recover-this-file",
        run: files::recover_this_file,
    },
    CommandSpec {
        name: "read-only-mode",
        run: files::read_only_mode,
    },
    CommandSpec {
        name: "split-window-below",
        run: window::split_window_below,
    },
    CommandSpec {
        name: "split-window-right",
        run: window::split_window_right,
    },
    CommandSpec {
        name: "delete-window",
        run: window::delete_window,
    },
    CommandSpec {
        name: "delete-other-windows",
        run: window::delete_other_windows,
    },
    CommandSpec {
        name: "other-window",
        run: window::other_window,
    },
    CommandSpec {
        name: "switch-to-buffer",
        run: window::switch_to_buffer,
    },
    CommandSpec {
        name: "list-buffers",
        run: window::list_buffers,
    },
    CommandSpec {
        name: "Buffer-menu-this-window",
        run: window::buffer_menu_this_window,
    },
];

pub fn lookup(name: &str) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|spec| spec.name == name)
}

/// What a running command sees: the editor, the client and window it runs
/// in, and Emacs's command-loop variables.
pub struct CommandContext<'a> {
    editor: &'a mut Editor,
    pub client: ClientId,
    pub window: WindowId,
    pub buffer: BufferId,
    /// Emacs `current-prefix-arg`.
    pub prefix_arg: PrefixArg,
    pub last_command: Option<&'static str>,
    pub this_command: &'static str,
    /// Emacs `last-command-event`; `None` for a paste.
    pub key: Option<Key>,
    pub(crate) next_prefix_arg: PrefixArg,
    pub(crate) keep_prefix_state: bool,
    /// Emacs `deactivate-mark`: the command loop deactivates the mark after
    /// the command. Buffer changes request it too.
    pub(crate) deactivate_mark: bool,
}

impl<'a> CommandContext<'a> {
    pub(crate) fn new(
        editor: &'a mut Editor,
        client: ClientId,
        this_command: &'static str,
        key: Option<Key>,
        prefix_arg: PrefixArg,
        last_command: Option<&'static str>,
    ) -> CommandContext<'a> {
        let window = editor.selected_window_id(client);
        let buffer = editor.window(client, window).buffer;
        CommandContext {
            editor,
            client,
            window,
            buffer,
            prefix_arg,
            last_command,
            this_command,
            key,
            next_prefix_arg: PrefixArg::None,
            keep_prefix_state: false,
            deactivate_mark: false,
        }
    }

    pub fn editor(&mut self) -> &mut Editor {
        self.editor
    }

    pub fn buffer(&self) -> &Buffer {
        self.editor.buffer(self.buffer)
    }

    pub fn buffer_mut(&mut self) -> &mut Buffer {
        self.editor.buffer_mut(self.buffer)
    }

    /// The buffer with the journal recorder every edit must be given.
    pub fn buffer_and_recorder(&mut self) -> (&mut Buffer, &mut dyn ChangeRecorder) {
        self.editor.buffer_and_recorder(self.buffer)
    }

    pub fn point(&self) -> u64 {
        self.editor.window_point(self.client, self.window)
    }

    pub fn set_point(&mut self, pos: u64) {
        self.editor.set_window_point(self.client, self.window, pos);
    }

    /// Inserts at point and leaves point after the text.
    pub fn insert(&mut self, bytes: &[u8]) -> Result<(), CommandError> {
        let point = self.point();
        let (buffer, recorder) = self.buffer_and_recorder();
        match buffer.insert(point, bytes, recorder) {
            Ok(()) => {}
            Err(EditError::ReadOnly) => {
                return Err(CommandError::BufferReadOnly(buffer.name.clone()));
            }
            Err(error) => return Err(error.into()),
        }
        self.set_point(point + bytes.len() as u64);
        Ok(())
    }

    /// Emacs `message`: shows text in this client's echo area.
    pub fn message(&mut self, text: impl Into<String>) {
        self.editor.set_echo(self.client, text.into());
    }

    /// Emacs `error`: return the result to signal it. The command loop
    /// shows the message and rings the bell.
    pub fn error(&self, text: impl Into<String>) -> CommandError {
        CommandError::User(text.into())
    }

    /// Emacs `ding` without a message.
    pub fn ding(&mut self) {
        self.editor.ring_bell(self.client);
    }

    /// Emacs `(setq prefix-arg ARG)`: the argument the next command gets.
    pub fn set_prefix_arg(&mut self, arg: PrefixArg) {
        self.next_prefix_arg = arg;
    }

    /// Emacs `universal-argument--mode`: keep `last-command` as it was and
    /// read the next key in `universal-argument-map`.
    pub fn universal_argument_mode(&mut self) {
        self.keep_prefix_state = true;
    }

    /// The command's `last-command` is `name`.
    pub fn last_command_is(&self, name: &str) -> bool {
        self.last_command == Some(name)
    }

    /// Deletes `range` with Emacs PT at `point`, which decides where undo
    /// leaves point. Point moves like a marker.
    pub fn delete_with_point(
        &mut self,
        range: Range<u64>,
        point: u64,
    ) -> Result<Vec<u8>, CommandError> {
        let (buffer, recorder) = self.buffer_and_recorder();
        buffer
            .delete_with_point(range, point, recorder)
            .map_err(|error| read_only_error(buffer, error))
    }

    /// Emacs `barf-if-buffer-read-only`.
    pub fn check_editable(&self) -> Result<(), CommandError> {
        let buffer = self.buffer();
        buffer
            .check_editable()
            .map_err(|error| read_only_error(buffer, error))
    }

    /// Emacs `(mark t)`: the mark whether or not it is active.
    pub fn mark(&self) -> Option<u64> {
        let buffer = self.buffer();
        buffer.mark.map(|mark| buffer.marker_pos(mark))
    }

    /// Emacs `set-marker` on `mark-marker`, without activating it.
    pub fn set_mark_position(&mut self, pos: u64) {
        let buffer = self.buffer_mut();
        match buffer.mark {
            Some(mark) => buffer.set_marker(mark, pos),
            None => buffer.mark = Some(buffer.create_marker(pos, InsertionType::StayBefore)),
        }
    }

    /// Emacs `region-active-p` (Transient Mark mode is always on).
    pub fn region_active(&self) -> bool {
        self.buffer().mark_active && self.mark().is_some()
    }

    /// Emacs `use-region-p`: an active, non-empty region.
    pub fn use_region(&self) -> bool {
        self.region_active() && self.mark() != Some(self.point())
    }

    /// Emacs `activate-mark`; does nothing without a mark.
    pub fn activate_mark(&mut self) {
        if self.mark().is_some() {
            self.buffer_mut().mark_active = true;
        }
    }

    /// Emacs `deactivate-mark` called directly.
    pub fn deactivate_mark_now(&mut self) {
        self.buffer_mut().mark_active = false;
    }

    /// Emacs `(setq deactivate-mark t)`.
    pub fn request_deactivate_mark(&mut self) {
        self.deactivate_mark = true;
    }

    /// The region from mark to point as Emacs's interactive spec reads it:
    /// `(mark)` first, then `(point)`. Signals without a mark.
    pub fn mark_and_point(&self) -> Result<(u64, u64), CommandError> {
        let mark = self.mark().ok_or_else(|| self.error(NO_REGION))?;
        Ok((mark, self.point()))
    }

    /// `region-beginning` to `region-end`.
    pub fn region(&self) -> Result<Range<u64>, CommandError> {
        let (mark, point) = self.mark_and_point()?;
        Ok(mark.min(point)..mark.max(point))
    }

    /// Remembers what a yank inserted, for `M-y`, per client.
    pub(crate) fn record_yank(&mut self, start: u64, end: u64) {
        self.editor.set_yank_range(self.client, start, end);
    }

    /// What this client's last yank inserted in this buffer.
    pub(crate) fn yank_range(&self) -> Option<(u64, u64)> {
        self.editor.yank_range(self.client)
    }

    pub fn kill_ring_mut(&mut self) -> &mut KillRing {
        self.editor.kill_ring_mut()
    }

    /// The window's columns and text rows.
    pub(crate) fn window_size(&self) -> (u16, u16) {
        self.editor.window_size(self.client, self.window)
    }

    pub(crate) fn window_start(&self) -> ScreenLine {
        self.editor.window_start(self.client, self.window)
    }

    pub(crate) fn set_window_start(&mut self, start: ScreenLine) {
        self.editor
            .set_window_start(self.client, self.window, start);
    }

    /// Emacs `redraw-frame` for this client's terminal.
    pub(crate) fn request_full_redraw(&mut self) {
        self.editor.request_full_redraw(self.client);
    }

    pub(crate) fn recenter_last_op(&self) -> Option<RecenterPosition> {
        self.editor.recenter_last_op(self.client)
    }

    pub(crate) fn set_recenter_last_op(&mut self, op: RecenterPosition) {
        self.editor.set_recenter_last_op(self.client, op);
    }

    /// Emacs `temporary-goal-column`, kept per client.
    pub fn temporary_goal_column(&self) -> u64 {
        self.editor.temporary_goal_column(self.client)
    }

    pub fn set_temporary_goal_column(&mut self, column: u64) {
        self.editor.set_temporary_goal_column(self.client, column);
    }
}

/// A failed edit as Emacs reports it: read-only names the buffer.
fn read_only_error(buffer: &Buffer, error: EditError) -> CommandError {
    match error {
        EditError::ReadOnly => CommandError::BufferReadOnly(buffer.name.clone()),
        other => other.into(),
    }
}

/// An undo failure as Emacs reports it.
fn undo_error(buffer: &Buffer, error: UndoError) -> CommandError {
    match error {
        UndoError::Edit(edit) => read_only_error(buffer, edit),
        other => CommandError::User(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn description_matches_universal_argument_description() {
        // (universal-argument--description) in Emacs 29.3 for each prefix-arg.
        for (arg, description) in [
            (PrefixArg::Raw(4), Some("C-u")),
            (PrefixArg::Raw(16), Some("C-u C-u")),
            (PrefixArg::Raw(64), Some("C-u C-u C-u")),
            (PrefixArg::Raw(-4), Some("C-u (-4)")),
            (PrefixArg::Raw(12), Some("C-u (12)")),
            (PrefixArg::Raw(48), Some("C-u (48)")),
            (PrefixArg::Negative, Some("C-u -")),
            (PrefixArg::Numeric(5), Some("C-u 5")),
            (PrefixArg::Numeric(-5), Some("C-u -5")),
            (PrefixArg::Numeric(0), Some("C-u 0")),
            (PrefixArg::None, None),
        ] {
            assert_eq!(arg.description().as_deref(), description, "{arg:?}");
        }
    }

    #[test]
    fn numeric_value_matches_prefix_numeric_value() {
        assert_eq!(PrefixArg::None.numeric_value(), 1);
        assert_eq!(PrefixArg::Raw(16).numeric_value(), 16);
        assert_eq!(PrefixArg::Negative.numeric_value(), -1);
        assert_eq!(PrefixArg::Numeric(-7).numeric_value(), -7);
    }

    #[test]
    fn command_names_are_unique() {
        for (index, spec) in COMMANDS.iter().enumerate() {
            assert!(
                COMMANDS[index + 1..]
                    .iter()
                    .all(|other| other.name != spec.name),
                "{} registered twice",
                spec.name
            );
        }
    }
}
