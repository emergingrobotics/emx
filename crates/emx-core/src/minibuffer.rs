//! The minibuffer: one line of input read in the echo area with its own
//! keys, per-kind history, and the answer prompts `y-or-n-p`,
//! `yes-or-no-p`, single-key choices and `read-multiple-choice`'s long
//! form. What happens with the answer is the `Purpose`, which the editor
//! carries out when the minibuffer exits.
//!
//! The input is bytes, not a string: a file name completed into it need
//! not be UTF-8, and it must reach the file system unchanged.
//!
//! A prompt that completes (`completion`) gets `fido-vertical-mode`'s keys:
//! `RET` takes the selected candidate (entering it when it is a
//! directory), `C-j`/`M-j` take the input as typed, `C-n`/`C-p`, fido's
//! `C-s`/`C-r` and the arrows move the selection, and `DEL` after a `/`
//! goes up a directory.

use std::collections::HashMap;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;

use emx_types::{Key, KeyCode, Mods};

use crate::completion::{Candidate, Completer, LISTING, ListingError};
use crate::editor::Purpose;
use crate::event::ListingId;
use crate::io::expand_file_name;
use crate::killring::KillRing;

/// Emacs `history-length`.
const HISTORY_LENGTH: usize = 100;
const YES: &str = "yes";
const NO: &str = "no";
const BEGINNING_OF_HISTORY: &str = " [Beginning of history; no preceding item]";
const END_OF_HISTORY: &str = " [End of history; no default available]";
const END_OF_DEFAULTS: &str = " [End of defaults; no next item]";
const NO_MATCH: &str = " [No match]";
const PLEASE_ANSWER_Y_OR_N: &str = " [Please answer y or n]";
const PLEASE_ANSWER_YES_OR_NO: &str = "Please answer yes or no.";
const DIRECTORY_SEPARATOR: u8 = b'/';
const UTF8_MAX_LEN: usize = 4;
/// Added to the listing status while `RET` waits for the listing, so a
/// directory that never answers does not leave `RET` silently doing
/// nothing: `C-j` takes the input without waiting.
const RET_WAITS: &str = "; RET waits for it, C-j takes the input as typed";
const HOME_DIRECTORY_INPUT: &[u8] = b"~/";

/// The history lists Emacs keeps separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum HistoryKind {
    FileName,
    BufferName,
    YesOrNo,
    MultipleChoice,
    /// Emacs `extended-command-history`.
    Command,
    /// `read-from-kill-ring-history`, which is the kill ring itself.
    KillRing,
}

/// Every history, newest entry first. Shared by all clients, as one Emacs
/// process's histories are.
#[derive(Debug, Default)]
pub(crate) struct Histories {
    lists: HashMap<HistoryKind, Vec<Vec<u8>>>,
}

impl Histories {
    pub(crate) fn list(&self, kind: HistoryKind) -> &[Vec<u8>] {
        self.lists.get(&kind).map_or(&[], Vec::as_slice)
    }

    /// Emacs `add-to-history`: empty input and a repeat of the newest
    /// entry are not added.
    fn add(&mut self, kind: HistoryKind, entry: &[u8]) {
        let list = self.lists.entry(kind).or_default();
        if entry.is_empty() || list.first().is_some_and(|newest| newest == entry) {
            return;
        }
        list.insert(0, entry.to_vec());
        list.truncate(HISTORY_LENGTH);
    }
}

/// A message shown with the minibuffer until the next key or timeout
/// (Emacs `minibuffer-message`, or a message over the prompt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Overlay {
    /// Shown after the input, as `minibuffer-message` does.
    Append(String),
    /// Shown instead of the prompt and input.
    Replace(String),
}

/// One accepted key of a single-key prompt and the answer it gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyChoice {
    pub key: Key,
    pub answer: char,
}

#[derive(Debug, Clone)]
pub(crate) enum Mode {
    /// `read-from-minibuffer`; `require` lists the only acceptable inputs
    /// (completion's require-match), and empty input means the default.
    Line {
        history: HistoryKind,
        defaults: Vec<Vec<u8>>,
        require: Option<Vec<Vec<u8>>>,
        allow_empty: bool,
    },
    YOrN,
    YesOrNo,
    /// One key answers; others show `invalid`, and `help` shows the help.
    Keys {
        choices: Vec<KeyChoice>,
        invalid: Option<Overlay>,
        help: Option<(Key, String)>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Answer {
    Line(Vec<u8>),
    Yes(bool),
    Key(char),
}

pub(crate) enum Outcome {
    Continue,
    /// Continue, ringing the bell.
    Ding,
    Exit(Answer),
    Quit,
}

/// The editing command a key runs in the minibuffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Insert(char),
    BeginningOfLine,
    EndOfLine,
    ForwardChar,
    BackwardChar,
    DeleteBackwardChar,
    DeleteChar,
    KillLine,
    Yank,
    PreviousHistory,
    NextHistory,
    Exit,
    Quit,
    /// `icomplete-fido-ret`.
    Accept,
    /// `icomplete-force-complete`.
    ForceComplete,
    NextCandidate,
    PreviousCandidate,
    FirstCandidate,
    LastCandidate,
}

const fn key(code: KeyCode, ctrl: bool, meta: bool) -> Key {
    Key {
        code,
        mods: Mods {
            ctrl,
            meta,
            shift: false,
            super_: false,
        },
    }
}

pub(crate) const fn plain(character: char) -> Key {
    key(KeyCode::Char(character), false, false)
}

pub(crate) const fn control(character: char) -> Key {
    key(KeyCode::Char(character), true, false)
}

pub(crate) const RETURN: Key = key(KeyCode::Enter, false, false);
pub(crate) const BACKSPACE: Key = key(KeyCode::Backspace, false, false);
pub(crate) const ESCAPE: Key = key(KeyCode::Escape, false, false);

/// `icomplete-fido-mode-map` and `icomplete-vertical-mode-minibuffer-map`
/// over the line-editing keys.
fn completion_command(key: Key) -> Option<Command> {
    let Mods {
        ctrl, meta, super_, ..
    } = key.mods;
    match (key.code, ctrl, meta, super_) {
        (KeyCode::Enter, false, false, false) => Some(Command::Accept),
        (KeyCode::Tab, false, false, false) | (KeyCode::Tab, false, true, false) => {
            Some(Command::ForceComplete)
        }
        (KeyCode::Char('j'), false, true, false) => Some(Command::Exit),
        (KeyCode::Char('n' | 's'), true, false, false) | (KeyCode::Down, false, false, false) => {
            Some(Command::NextCandidate)
        }
        (KeyCode::Char('p' | 'r'), true, false, false) | (KeyCode::Up, false, false, false) => {
            Some(Command::PreviousCandidate)
        }
        (KeyCode::Char('<'), false, true, false) => Some(Command::FirstCandidate),
        (KeyCode::Char('>'), false, true, false) => Some(Command::LastCandidate),
        _ => minibuffer_command(key),
    }
}

/// `minibuffer-local-map` and the global editing keys it inherits.
fn minibuffer_command(key: Key) -> Option<Command> {
    let Mods {
        ctrl, meta, super_, ..
    } = key.mods;
    match (key.code, ctrl, meta, super_) {
        (KeyCode::Char(character), false, false, false) => Some(Command::Insert(character)),
        (KeyCode::Char('a'), true, false, false) | (KeyCode::Home, false, false, false) => {
            Some(Command::BeginningOfLine)
        }
        (KeyCode::Char('e'), true, false, false) | (KeyCode::End, false, false, false) => {
            Some(Command::EndOfLine)
        }
        (KeyCode::Char('f'), true, false, false) | (KeyCode::Right, false, false, false) => {
            Some(Command::ForwardChar)
        }
        (KeyCode::Char('b'), true, false, false) | (KeyCode::Left, false, false, false) => {
            Some(Command::BackwardChar)
        }
        (KeyCode::Backspace, false, false, false) => Some(Command::DeleteBackwardChar),
        (KeyCode::Char('d'), true, false, false) | (KeyCode::Delete, false, false, false) => {
            Some(Command::DeleteChar)
        }
        (KeyCode::Char('k'), true, false, false) => Some(Command::KillLine),
        (KeyCode::Char('y'), true, false, false) => Some(Command::Yank),
        (KeyCode::Char('p'), false, true, false) | (KeyCode::Up, false, false, false) => {
            Some(Command::PreviousHistory)
        }
        (KeyCode::Char('n'), false, true, false) | (KeyCode::Down, false, false, false) => {
            Some(Command::NextHistory)
        }
        (KeyCode::Enter, false, false, false) | (KeyCode::Char('j'), true, false, false) => {
            Some(Command::Exit)
        }
        (KeyCode::Char('g'), true, false, false) => Some(Command::Quit),
        _ => None,
    }
}

#[derive(Debug)]
pub(crate) struct Minibuffer {
    pub prompt: String,
    pub input: Vec<u8>,
    /// Byte offset of the cursor in `input`, on a character boundary (an
    /// invalid byte is a character of its own).
    pub cursor: usize,
    pub mode: Mode,
    pub purpose: Purpose,
    /// 0 is the input being typed, n > 0 the nth newest history entry, and
    /// n < 0 the -nth default (Emacs "future history").
    history_position: i64,
    typed_input: Vec<u8>,
    pub overlay: Option<Overlay>,
    /// Identifies the overlay a timer was set to clear.
    pub overlay_serial: u64,
    pub completion: Option<Completer>,
    /// A history list of the prompt's own that input is never added to
    /// (`read-from-kill-ring` reads over the kill ring itself).
    own_history: Option<Vec<Vec<u8>>>,
    /// `RET` came while the directory listing was on its way: it takes the
    /// selected candidate once the listing arrives.
    accept_when_listed: bool,
}

impl Minibuffer {
    pub(crate) fn new(prompt: String, initial: Vec<u8>, mode: Mode, purpose: Purpose) -> Self {
        Minibuffer {
            cursor: initial.len(),
            prompt,
            input: initial,
            mode,
            purpose,
            history_position: 0,
            typed_input: Vec::new(),
            overlay: None,
            overlay_serial: 0,
            completion: None,
            own_history: None,
            accept_when_listed: false,
        }
    }

    /// Completes with `completer`, starting from the initial input.
    pub(crate) fn with_completion(mut self, mut completer: Completer) -> Self {
        completer.start(&self.input);
        self.completion = Some(completer);
        self
    }

    /// Reads history from `entries` (newest first) instead of the shared
    /// lists, starting at `position`, and adds nothing to it.
    pub(crate) fn with_own_history(mut self, entries: Vec<Vec<u8>>, position: i64) -> Self {
        self.own_history = Some(entries);
        self.history_position = position;
        self
    }

    /// The echo-area line and the byte offset of the cursor in it.
    pub(crate) fn display(&self) -> (Vec<u8>, usize) {
        match &self.overlay {
            Some(Overlay::Replace(message)) => (message.as_bytes().to_vec(), message.len()),
            overlay => {
                let mut line = self.prompt.as_bytes().to_vec();
                line.extend_from_slice(&self.input);
                let cursor = self.prompt.len() + self.cursor;
                match overlay {
                    Some(Overlay::Append(message)) => line.extend_from_slice(message.as_bytes()),
                    _ => {
                        let waiting = self.accept_when_listed
                            && self.completion.as_ref().is_some_and(Completer::pending);
                        let status = if waiting {
                            Some(format!(" [{LISTING}{RET_WAITS}]"))
                        } else {
                            self.completion.as_ref().and_then(Completer::status)
                        };
                        if let Some(status) = status {
                            line.extend_from_slice(status.as_bytes());
                        }
                    }
                }
                (line, cursor)
            }
        }
    }

    fn set_overlay(&mut self, overlay: Overlay) {
        self.overlay = Some(overlay);
        self.overlay_serial += 1;
    }

    fn set_input(&mut self, input: Vec<u8>) {
        self.cursor = input.len();
        self.input = input;
    }

    /// The completion's listing `id` arrived. When `RET` was waiting for
    /// it, returns what `RET` does now.
    pub(crate) fn listing_arrived(
        &mut self,
        id: ListingId,
        result: Result<Vec<Candidate>, ListingError>,
        histories: &mut Histories,
    ) -> Option<Outcome> {
        let completion = self.completion.as_mut()?;
        if !completion.listing_arrived(id, result, &self.input) || !self.accept_when_listed {
            return None;
        }
        self.accept_when_listed = false;
        let before = self.input.clone();
        let outcome = self.accept(histories);
        // Keys update completion in `line_key`; this `RET` came with the
        // listing, so entering a directory has to be followed here.
        if self.input != before
            && let Some(completion) = self.completion.as_mut()
        {
            completion.update(&self.input);
        }
        Some(outcome)
    }

    /// Handles one key; on `Exit` the answer has been added to history.
    pub(crate) fn handle_key(
        &mut self,
        key: Key,
        histories: &mut Histories,
        kill_ring: &mut KillRing,
    ) -> Outcome {
        self.overlay = None;
        // A key typed while `RET` waits for the listing means the user went
        // on; only a `RET` again waits again.
        self.accept_when_listed = false;
        match &self.mode {
            Mode::YOrN => self.y_or_n_key(key),
            Mode::Keys {
                choices,
                invalid,
                help,
            } => {
                if key == control('g') {
                    return Outcome::Quit;
                }
                if let Some(choice) = choices.iter().find(|choice| choice.key == key) {
                    return Outcome::Exit(Answer::Key(choice.answer));
                }
                let overlay = match help {
                    Some((help_key, text)) if *help_key == key => {
                        Some(Overlay::Replace(text.clone()))
                    }
                    _ => invalid.clone(),
                };
                match overlay {
                    Some(overlay) => {
                        self.set_overlay(overlay);
                        Outcome::Ding
                    }
                    None => Outcome::Ding,
                }
            }
            Mode::Line { .. } | Mode::YesOrNo => self.line_key(key, histories, kill_ring),
        }
    }

    /// `y-or-n-p-map` over `query-replace-map`.
    fn y_or_n_key(&mut self, key: Key) -> Outcome {
        match key {
            key if key == plain('y') || key == plain('Y') || key == plain(' ') => {
                Outcome::Exit(Answer::Yes(true))
            }
            key if key == plain('n') || key == plain('N') || key == BACKSPACE => {
                Outcome::Exit(Answer::Yes(false))
            }
            key if key == control('g') || key == ESCAPE => Outcome::Quit,
            _ => {
                self.input.clear();
                self.cursor = 0;
                self.set_overlay(Overlay::Append(PLEASE_ANSWER_Y_OR_N.to_string()));
                Outcome::Ding
            }
        }
    }

    fn line_key(
        &mut self,
        key: Key,
        histories: &mut Histories,
        kill_ring: &mut KillRing,
    ) -> Outcome {
        let command = if self.completion.is_some() {
            completion_command(key)
        } else {
            minibuffer_command(key)
        };
        let Some(command) = command else {
            return Outcome::Ding;
        };
        let before = self.input.clone();
        let outcome = self.run(command, histories, kill_ring);
        if self.input != before
            && let Some(completion) = self.completion.as_mut()
        {
            completion.update(&self.input);
        }
        outcome
    }

    fn run(
        &mut self,
        command: Command,
        histories: &mut Histories,
        kill_ring: &mut KillRing,
    ) -> Outcome {
        match command {
            Command::Insert(character) => {
                let mut encoded = [0; 4];
                let encoded = character.encode_utf8(&mut encoded).as_bytes();
                self.input
                    .splice(self.cursor..self.cursor, encoded.iter().copied());
                self.cursor += encoded.len();
            }
            Command::BeginningOfLine => self.cursor = 0,
            Command::EndOfLine => self.cursor = self.input.len(),
            Command::ForwardChar => match next_char_length(&self.input, self.cursor) {
                Some(length) => self.cursor += length,
                None => return Outcome::Ding,
            },
            Command::BackwardChar => match previous_char_length(&self.input, self.cursor) {
                Some(length) => self.cursor -= length,
                None => return Outcome::Ding,
            },
            Command::DeleteBackwardChar => {
                if self.after_directory_separator() {
                    self.up_directory();
                    return Outcome::Continue;
                }
                match previous_char_length(&self.input, self.cursor) {
                    Some(length) => {
                        self.cursor -= length;
                        self.input.drain(self.cursor..self.cursor + length);
                    }
                    None => return Outcome::Ding,
                }
            }
            Command::DeleteChar => match next_char_length(&self.input, self.cursor) {
                Some(length) => {
                    self.input.drain(self.cursor..self.cursor + length);
                }
                None => return Outcome::Ding,
            },
            Command::KillLine => {
                let killed = self.input.split_off(self.cursor);
                kill_ring.kill_new(killed);
            }
            Command::Yank => {
                let Ok(text) = kill_ring.current_kill(0) else {
                    return Outcome::Ding;
                };
                let text = text.to_vec();
                self.input
                    .splice(self.cursor..self.cursor, text.iter().copied());
                self.cursor += text.len();
            }
            Command::PreviousHistory => self.move_in_history(1, histories),
            Command::NextHistory => self.move_in_history(-1, histories),
            Command::Exit => return self.exit(histories),
            Command::Quit => return Outcome::Quit,
            Command::Accept => return self.accept(histories),
            Command::ForceComplete => return self.force_complete(),
            Command::NextCandidate => self.completer().select_next(),
            Command::PreviousCandidate => self.completer().select_previous(),
            Command::FirstCandidate => self.completer().select_first(),
            Command::LastCandidate => self.completer().select_last(),
        }
        Outcome::Continue
    }

    fn completer(&mut self) -> &mut Completer {
        self.completion
            .as_mut()
            .expect("completion keys are looked up only while completing")
    }

    /// `DEL` after a `/` in file-name input goes up a directory
    /// (`icomplete-fido-backward-updir`).
    fn after_directory_separator(&self) -> bool {
        self.completion
            .as_ref()
            .is_some_and(Completer::completes_files)
            && self.input[..self.cursor].ends_with(&[DIRECTORY_SEPARATOR])
    }

    /// `icomplete-fido-backward-updir`: `~/` becomes the home directory
    /// spelled out, then everything after the `/` before the one at point
    /// is deleted.
    fn up_directory(&mut self) {
        if self.input == HOME_DIRECTORY_INPUT {
            let mut home = expand_file_name(HOME_DIRECTORY_INPUT, Path::new("/"))
                .into_os_string()
                .into_vec();
            if !home.ends_with(&[DIRECTORY_SEPARATOR]) {
                home.push(DIRECTORY_SEPARATOR);
            }
            self.set_input(home);
        }
        let before_slash = self.cursor - 1;
        if let Some(previous) = self.input[..before_slash]
            .iter()
            .rposition(|byte| *byte == DIRECTORY_SEPARATOR)
        {
            self.input.truncate(previous + 1);
            self.cursor = self.cursor.min(self.input.len());
        }
    }

    /// `icomplete-fido-ret`: take the selected candidate and exit, or enter
    /// it when it is a directory; with no candidate, exit with the input.
    fn accept(&mut self, histories: &mut Histories) -> Outcome {
        let Some(completion) = self.completion.as_ref() else {
            return self.exit(histories);
        };
        if completion.pending() {
            self.accept_when_listed = true;
            return Outcome::Continue;
        }
        let Some(candidate) = completion.selected() else {
            return self.exit(histories);
        };
        let completed = completion.completed_input(&self.input, candidate);
        let enters = completion.completes_files() && candidate.is_directory();
        self.set_input(completed);
        if enters {
            return Outcome::Continue;
        }
        self.exit(histories)
    }

    /// `icomplete-force-complete`: the selected candidate becomes the input.
    fn force_complete(&mut self) -> Outcome {
        let completion = self
            .completion
            .as_ref()
            .expect("completion keys are looked up only while completing");
        let Some(candidate) = completion.selected() else {
            return Outcome::Ding;
        };
        let completed = completion.completed_input(&self.input, candidate);
        self.set_input(completed);
        Outcome::Continue
    }

    fn history_kind(&self) -> HistoryKind {
        match &self.mode {
            Mode::Line { history, .. } => *history,
            Mode::YesOrNo => HistoryKind::YesOrNo,
            Mode::YOrN | Mode::Keys { .. } => unreachable!("only line reads have history"),
        }
    }

    fn defaults(&self) -> &[Vec<u8>] {
        match &self.mode {
            Mode::Line { defaults, .. } => defaults,
            _ => &[],
        }
    }

    /// Emacs `previous-history-element` (step 1) and
    /// `next-history-element` (step -1).
    fn move_in_history(&mut self, step: i64, histories: &Histories) {
        let history = match &self.own_history {
            Some(entries) => entries.as_slice(),
            None => histories.list(self.history_kind()),
        };
        let target = self.history_position + step;
        let history_len = i64::try_from(history.len()).expect("history is bounded");
        let defaults_len = i64::try_from(self.defaults().len()).expect("defaults are few");
        if target > history_len {
            self.set_overlay(Overlay::Append(BEGINNING_OF_HISTORY.to_string()));
            return;
        }
        if target < -defaults_len {
            let message = if defaults_len == 0 {
                END_OF_HISTORY
            } else {
                END_OF_DEFAULTS
            };
            self.set_overlay(Overlay::Append(message.to_string()));
            return;
        }
        if self.history_position == 0 {
            self.typed_input = self.input.clone();
        }
        self.history_position = target;
        let input = match target {
            0 => self.typed_input.clone(),
            position if position > 0 => {
                history[usize::try_from(position - 1).expect("positive")].clone()
            }
            position => self.defaults()[usize::try_from(-position - 1).expect("negative")].clone(),
        };
        self.set_input(input);
    }

    fn exit(&mut self, histories: &mut Histories) -> Outcome {
        match &self.mode {
            Mode::Line {
                history,
                require,
                allow_empty,
                ..
            } => {
                let acceptable = match require {
                    _ if self.input.is_empty() => *allow_empty,
                    Some(candidates) => candidates.contains(&self.input),
                    None => true,
                };
                if !acceptable {
                    self.set_overlay(Overlay::Append(NO_MATCH.to_string()));
                    return Outcome::Continue;
                }
                if self.own_history.is_none() {
                    histories.add(*history, &self.input);
                }
                Outcome::Exit(Answer::Line(self.input.clone()))
            }
            Mode::YesOrNo => {
                let answer = String::from_utf8_lossy(&self.input).to_lowercase();
                histories.add(HistoryKind::YesOrNo, &self.input);
                match answer.as_str() {
                    YES => Outcome::Exit(Answer::Yes(true)),
                    NO => Outcome::Exit(Answer::Yes(false)),
                    _ => {
                        self.set_input(Vec::new());
                        self.history_position = 0;
                        self.set_overlay(Overlay::Replace(PLEASE_ANSWER_YES_OR_NO.to_string()));
                        Outcome::Ding
                    }
                }
            }
            Mode::YOrN | Mode::Keys { .. } => unreachable!("single-key prompts exit on keys"),
        }
    }
}

/// The length of the character at `at` (an invalid byte is one), if any.
fn next_char_length(bytes: &[u8], at: usize) -> Option<usize> {
    let rest = &bytes[at..];
    let chunk = rest.utf8_chunks().next()?;
    Some(chunk.valid().chars().next().map_or(1, char::len_utf8))
}

/// The length of the character ending at `at` (an invalid byte is one).
fn previous_char_length(bytes: &[u8], at: usize) -> Option<usize> {
    if at == 0 {
        return None;
    }
    let longest = at.min(UTF8_MAX_LEN);
    for length in (2..=longest).rev() {
        if std::str::from_utf8(&bytes[at - length..at]).is_ok_and(|text| text.chars().count() == 1)
        {
            return Some(length);
        }
    }
    Some(1)
}
