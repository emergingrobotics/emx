mod edit;
mod misc;
mod r#move;

use emx_text::ReadError;
use emx_types::Key;

use crate::buffer::{Buffer, BufferId, ChangeRecorder, EditError};
use crate::editor::Editor;
use crate::event::ClientId;
use crate::window::WindowId;

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

static COMMANDS: [CommandSpec; 8] = [
    CommandSpec {
        name: "forward-char",
        run: r#move::forward_char,
    },
    CommandSpec {
        name: "backward-char",
        run: r#move::backward_char,
    },
    CommandSpec {
        name: "self-insert-command",
        run: edit::self_insert_command,
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
