//! Keyboard macro recording and replay for the command loop. Recording is
//! driven from `Editor::handle_key`, which appends each key read while a
//! macro is being defined; the commands in `command::macro` start and end
//! the definition and ask for replay.

use emx_types::Key;

use super::Editor;
use crate::command::{CommandError, PrefixArg};
use crate::event::ClientId;

const ALREADY_DEFINING: &str = "Already defining kbd macro";
const NO_MACRO: &str = "No kbd macro has been defined";
const KMACRO_END_AND_CALL: &str = "kmacro-end-and-call-macro";

impl Editor {
    /// Emacs `kmacro-start-macro`: begin recording the keys read next.
    pub(crate) fn start_kbd_macro(&mut self, client_id: ClientId) -> Result<(), CommandError> {
        if self.client(client_id).macro_defining.is_some() {
            return Err(CommandError::User(ALREADY_DEFINING.to_string()));
        }
        let client = self.client_mut(client_id);
        client.macro_defining = Some(Vec::new());
        client.macro_repeat = None;
        Ok(())
    }

    /// Emacs `kmacro-end-macro`. Under `execute-kbd-macro` (as the test
    /// harness drives it) ending with no definition in progress is a no-op,
    /// not an error.
    pub(crate) fn end_kbd_macro(&mut self, client_id: ClientId) -> Result<(), CommandError> {
        self.end_kbd_macro_if_defining(client_id);
        Ok(())
    }

    /// Ends the definition if one is in progress, without error otherwise;
    /// used by `kmacro-end-and-call-macro`.
    pub(crate) fn end_kbd_macro_if_defining(&mut self, client_id: ClientId) {
        if self.client(client_id).macro_defining.is_some() {
            self.finish_macro_definition(client_id);
        }
    }

    /// Finishes the current definition, dropping the keys that invoked the
    /// ending command (its full key sequence is the pending one).
    fn finish_macro_definition(&mut self, client_id: ClientId) {
        let invoking = self.client(client_id).command_loop.pending.len();
        let client = self.client_mut(client_id);
        let mut keys = client
            .macro_defining
            .take()
            .expect("a definition is in progress");
        let kept = keys.len().saturating_sub(invoking);
        keys.truncate(kept);
        self.last_kbd_macro = Some(keys);
    }

    /// Runs the last keyboard macro `count` times, then arms `key` to repeat
    /// it, as Emacs's transient map does after `C-x e`.
    pub(crate) fn call_last_macro(&mut self, client_id: ClientId, count: i64, key: Option<Key>) {
        let Some(keys) = self.last_kbd_macro.clone() else {
            self.signal(
                client_id,
                Some(KMACRO_END_AND_CALL),
                &CommandError::User(NO_MACRO.to_string()),
            );
            return;
        };
        if count > 0 && !keys.is_empty() {
            self.replay_macro(client_id, &keys, count);
        }
        if self.clients.contains_key(&client_id) && !self.client(client_id).signaled {
            self.client_mut(client_id).macro_repeat = key;
        }
    }

    /// The repeat key `C-x e` armed was pressed: run the macro again, honoring
    /// a prefix argument as the repeat count.
    pub(crate) fn repeat_last_macro(&mut self, client_id: ClientId, key: Key) {
        let count = {
            let state = &mut self.client_mut(client_id).command_loop;
            let count = state.prefix_arg.numeric_value();
            state.prefix_arg = PrefixArg::None;
            count
        };
        self.call_last_macro(client_id, count, Some(key));
    }

    /// Feeds the macro's keys back through the command loop `count` times,
    /// stopping at the first command that signals, as `execute-kbd-macro`
    /// does.
    fn replay_macro(&mut self, client_id: ClientId, keys: &[Key], count: i64) {
        self.client_mut(client_id).macro_executing += 1;
        'replay: for _ in 0..count {
            for &key in keys {
                self.handle_key(client_id, key);
                if !self.clients.contains_key(&client_id) {
                    return;
                }
                if self.client(client_id).signaled {
                    break 'replay;
                }
            }
        }
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.macro_executing = client.macro_executing.saturating_sub(1);
        }
    }

    /// Discards any macro definition and repeat key, as `keyboard-quit` does.
    pub(crate) fn abort_keyboard_macro(&mut self, client_id: ClientId) {
        let client = self.client_mut(client_id);
        client.macro_defining = None;
        client.macro_repeat = None;
    }
}
