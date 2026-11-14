//! Completing reads: the candidate list a prompt shows above the echo area
//! (taking its rows from the windows, as Emacs grows the minibuffer
//! window), the directory listings file-name completion waits for, and the
//! commands whose prompts complete: `M-x` and `yank-from-kill-ring`.

use super::{Editor, Purpose};
use crate::command::{self, CommandError, PrefixArg};
use crate::completion::{Completer, Kind, ListSource};
use crate::event::{ClientId, ListingId};
use crate::killring::KillRingError;
use crate::minibuffer::{HistoryKind, Minibuffer, Mode};

/// `read-extended-command`'s prompt after any prefix-argument part.
const EXECUTE_EXTENDED_COMMAND_PROMPT: &str = "M-x ";
const YANK_FROM_KILL_RING_PROMPT: &str = "Yank from kill-ring: ";
/// `yank-from-kill-ring` runs as part of `yank-pop` (`M-y`).
const YANK_POP: &str = "yank-pop";
/// The prefix argument `C-u` alone gives, which `M-x` shows as `C-u`.
const PLAIN_UNIVERSAL_ARGUMENT: i64 = 4;

/// The prefix-argument part of the `M-x` prompt, as
/// `read-extended-command-1` writes it.
fn prefix_prompt(prefix_arg: PrefixArg) -> String {
    match prefix_arg {
        PrefixArg::None => String::new(),
        PrefixArg::Negative => "- ".to_string(),
        PrefixArg::Raw(PLAIN_UNIVERSAL_ARGUMENT) => "C-u ".to_string(),
        PrefixArg::Raw(value) | PrefixArg::Numeric(value) => format!("{value} "),
    }
}

impl Editor {
    pub(super) fn next_listing_id(&mut self) -> ListingId {
        let id = ListingId(self.next_listing);
        self.next_listing += 1;
        id
    }

    /// How many candidates a prompt lists at most.
    pub(super) fn completion_rows(&self) -> usize {
        usize::try_from(self.config.completion_rows).expect("completion_rows is validated small")
    }

    /// Buffer-name completion: most recently shown first, the current
    /// buffer last, `default` on top until something is typed.
    pub(super) fn buffer_completer(
        &self,
        client: ClientId,
        default: &str,
        require_match: bool,
    ) -> Completer {
        let current = &self.buffers[self.selected_buffer(client)].name;
        let names = self
            .recency
            .iter()
            .map(|buffer| self.buffers[*buffer].name.as_str());
        let source = ListSource::buffers(names, current);
        Completer::listed(
            Kind::Buffer,
            &source,
            Some(default.as_bytes().to_vec()),
            require_match,
            self.completion_rows(),
        )
    }

    /// The rows the client's candidate list wants: one per match up to
    /// `completion_rows`, and no more than the windows can give up.
    fn wanted_candidate_rows(&self, client_id: ClientId) -> u16 {
        let client = self.client(client_id);
        let Some(completion) = client
            .minibuffer
            .as_ref()
            .and_then(|minibuffer| minibuffer.completion.as_ref())
        else {
            return 0;
        };
        let wanted = completion.match_count().min(self.completion_rows());
        u16::try_from(wanted)
            .unwrap_or(u16::MAX)
            .min(client.frame.spare_rows(client.rows))
    }

    /// Gives the windows the rows the candidate list does not use, as
    /// Emacs resizes the windows above a growing or shrinking minibuffer.
    /// The windows keep their shares, so they come back to the same sizes.
    pub(super) fn fit_frame(&mut self, client_id: ClientId) {
        let rows = self.wanted_candidate_rows(client_id);
        let client = self.client_mut(client_id);
        if client.candidate_rows != rows {
            client.candidate_rows = rows;
            client.frame.resize(client.cols, client.rows - rows);
        }
    }

    /// The candidate lines drawn above the echo area, top first: what each
    /// shows, and whether it is the selected one.
    pub(crate) fn candidate_lines(&self, client_id: ClientId) -> Vec<(Vec<u8>, bool)> {
        let client = self.client(client_id);
        let Some(completion) = client
            .minibuffer
            .as_ref()
            .and_then(|minibuffer| minibuffer.completion.as_ref())
        else {
            return Vec::new();
        };
        let (shown, selected) = completion.view(usize::from(client.candidate_rows));
        let columns = usize::from(client.cols);
        shown
            .into_iter()
            .enumerate()
            .map(|(line, candidate)| {
                (
                    completion.display(candidate, columns),
                    selected == Some(line),
                )
            })
            .collect()
    }

    /// The selected candidate's index among the matches and how many
    /// there are, while the client's prompt completes.
    pub(crate) fn completion_selection(&self, client_id: ClientId) -> Option<(usize, usize)> {
        let completion = self
            .client(client_id)
            .minibuffer
            .as_ref()?
            .completion
            .as_ref()?;
        Some((completion.selected_index(), completion.match_count()))
    }

    /// `execute-extended-command` (`M-x`): reads a command name, completing
    /// over every command, and runs it with the prefix argument `M-x` got.
    pub(crate) fn execute_extended_command_command(
        &mut self,
        client: ClientId,
        prefix_arg: PrefixArg,
        last_command: Option<&'static str>,
    ) {
        let names: Vec<Vec<u8>> = command::names()
            .map(|name| name.as_bytes().to_vec())
            .collect();
        let source =
            ListSource::commands(command::names(), self.histories.list(HistoryKind::Command));
        let mode = Mode::Line {
            history: HistoryKind::Command,
            defaults: Vec::new(),
            require: Some(names),
            allow_empty: false,
        };
        let completer =
            Completer::listed(Kind::Command, &source, None, true, self.completion_rows());
        let minibuffer = Minibuffer::new(
            format!(
                "{}{EXECUTE_EXTENDED_COMMAND_PROMPT}",
                prefix_prompt(prefix_arg)
            ),
            Vec::new(),
            mode,
            Purpose::ExecuteCommand {
                prefix_arg,
                last_command,
            },
        )
        .with_completion(completer);
        self.start_minibuffer(client, minibuffer);
    }

    /// Runs the command `M-x` read, as if its keys had been typed with the
    /// prefix argument, and with `last-command` what it was before `M-x`.
    pub(super) fn execute_named_command(
        &mut self,
        client: ClientId,
        name: &[u8],
        prefix_arg: PrefixArg,
        last_command: Option<&'static str>,
    ) -> Result<(), CommandError> {
        let name = String::from_utf8_lossy(name);
        let Some(spec) = command::lookup(&name) else {
            return Err(CommandError::User(format!(
                "\u{2018}{name}\u{2019} is not a valid command name"
            )));
        };
        let state = &mut self.client_mut(client).command_loop;
        state.prefix_arg = prefix_arg;
        state.last_command = last_command;
        self.run_command(client, spec.name, None, spec.run);
        Ok(())
    }

    /// Emacs 28 `yank-from-kill-ring` as `M-y` runs it after a command
    /// other than a yank: reads a kill, completing over the kill ring
    /// newest first; its history is the kill ring itself, starting where
    /// the yank pointer is (`read-from-kill-ring`).
    pub(crate) fn yank_from_kill_ring_command(
        &mut self,
        client: ClientId,
        prefix_arg: PrefixArg,
        last_command: Option<&'static str>,
    ) -> Result<(), CommandError> {
        if self.kill_ring.is_empty() {
            return Err(CommandError::User(KillRingError::Empty.to_string()));
        }
        let source = ListSource::kill_ring(self.kill_ring.entries());
        let entries: Vec<Vec<u8>> = self.kill_ring.entries().map(<[u8]>::to_vec).collect();
        let pointer = self.kill_ring.yank_pointer();
        let position = if pointer == 0 { 0 } else { pointer + 1 };
        let mode = Mode::Line {
            history: HistoryKind::KillRing,
            defaults: Vec::new(),
            require: None,
            allow_empty: true,
        };
        let completer =
            Completer::listed(Kind::KillRing, &source, None, false, self.completion_rows());
        let minibuffer = Minibuffer::new(
            YANK_FROM_KILL_RING_PROMPT.to_string(),
            Vec::new(),
            mode,
            Purpose::YankFromKillRing {
                prefix_arg,
                last_command,
            },
        )
        .with_completion(completer)
        .with_own_history(
            entries,
            i64::try_from(position).expect("the kill ring is bounded"),
        );
        self.start_minibuffer(client, minibuffer);
        Ok(())
    }

    /// Inserts the kill `yank-from-kill-ring` read, as one command.
    pub(super) fn yank_chosen_kill(
        &mut self,
        client: ClientId,
        text: Vec<u8>,
        prefix_arg: PrefixArg,
        last_command: Option<&'static str>,
    ) {
        let state = &mut self.client_mut(client).command_loop;
        state.prefix_arg = prefix_arg;
        state.last_command = last_command;
        self.run_command(client, YANK_POP, None, move |ctx| {
            command::yank_from_kill_ring(ctx, &text)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_prompt_follows_read_extended_command() {
        assert_eq!(prefix_prompt(PrefixArg::None), "");
        assert_eq!(prefix_prompt(PrefixArg::Raw(4)), "C-u ");
        assert_eq!(prefix_prompt(PrefixArg::Raw(16)), "16 ");
        assert_eq!(prefix_prompt(PrefixArg::Numeric(3)), "3 ");
        assert_eq!(prefix_prompt(PrefixArg::Negative), "- ");
    }
}
