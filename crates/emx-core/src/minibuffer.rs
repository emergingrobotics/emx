//! The minibuffer: one line of input read in the echo area with its own
//! keys, per-kind history, and the answer prompts `y-or-n-p`,
//! `yes-or-no-p`, single-key choices and `read-multiple-choice`'s long
//! form. What happens with the answer is the `Purpose`, which the editor
//! carries out when the minibuffer exits.
//!
//! Keys are looked up with `minibuffer_command`, so completion (a later
//! addition) adds its keys there and its candidates to the display.

use std::collections::HashMap;

use emx_types::{Key, KeyCode, Mods};

use crate::editor::Purpose;
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

/// The history lists Emacs keeps separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum HistoryKind {
    FileName,
    BufferName,
    YesOrNo,
    MultipleChoice,
}

/// Every history, newest entry first. Shared by all clients, as one Emacs
/// process's histories are.
#[derive(Debug, Default)]
pub(crate) struct Histories {
    lists: HashMap<HistoryKind, Vec<String>>,
}

impl Histories {
    fn list(&self, kind: HistoryKind) -> &[String] {
        self.lists.get(&kind).map_or(&[], Vec::as_slice)
    }

    /// Emacs `add-to-history`: empty input and a repeat of the newest
    /// entry are not added.
    fn add(&mut self, kind: HistoryKind, entry: &str) {
        let list = self.lists.entry(kind).or_default();
        if entry.is_empty() || list.first().is_some_and(|newest| newest == entry) {
            return;
        }
        list.insert(0, entry.to_string());
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
        defaults: Vec<String>,
        require: Option<Vec<String>>,
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
    Line(String),
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
        (KeyCode::Enter, false, false, false) => Some(Command::Exit),
        (KeyCode::Char('g'), true, false, false) => Some(Command::Quit),
        _ => None,
    }
}

#[derive(Debug)]
pub(crate) struct Minibuffer {
    pub prompt: String,
    pub input: String,
    /// Byte offset of the cursor in `input`, on a character boundary.
    pub cursor: usize,
    pub mode: Mode,
    pub purpose: Purpose,
    /// 0 is the input being typed, n > 0 the nth newest history entry, and
    /// n < 0 the -nth default (Emacs "future history").
    history_position: i64,
    typed_input: String,
    pub overlay: Option<Overlay>,
    /// Identifies the overlay a timer was set to clear.
    pub overlay_serial: u64,
}

impl Minibuffer {
    pub(crate) fn new(prompt: String, initial: String, mode: Mode, purpose: Purpose) -> Self {
        Minibuffer {
            cursor: initial.len(),
            prompt,
            input: initial,
            mode,
            purpose,
            history_position: 0,
            typed_input: String::new(),
            overlay: None,
            overlay_serial: 0,
        }
    }

    /// The echo-area line and the byte offset of the cursor in it.
    pub(crate) fn display(&self) -> (String, usize) {
        match &self.overlay {
            Some(Overlay::Replace(message)) => (message.clone(), message.len()),
            overlay => {
                let mut line = format!("{}{}", self.prompt, self.input);
                let cursor = self.prompt.len() + self.cursor;
                if let Some(Overlay::Append(message)) = overlay {
                    line.push_str(message);
                }
                (line, cursor)
            }
        }
    }

    fn set_overlay(&mut self, overlay: Overlay) {
        self.overlay = Some(overlay);
        self.overlay_serial += 1;
    }

    fn set_input(&mut self, input: String) {
        self.cursor = input.len();
        self.input = input;
    }

    /// Handles one key; on `Exit` the answer has been added to history.
    pub(crate) fn handle_key(
        &mut self,
        key: Key,
        histories: &mut Histories,
        kill_ring: &mut KillRing,
    ) -> Outcome {
        self.overlay = None;
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
        let Some(command) = minibuffer_command(key) else {
            return Outcome::Ding;
        };
        match command {
            Command::Insert(character) => {
                self.input.insert(self.cursor, character);
                self.cursor += character.len_utf8();
            }
            Command::BeginningOfLine => self.cursor = 0,
            Command::EndOfLine => self.cursor = self.input.len(),
            Command::ForwardChar => match self.input[self.cursor..].chars().next() {
                Some(character) => self.cursor += character.len_utf8(),
                None => return Outcome::Ding,
            },
            Command::BackwardChar => match self.input[..self.cursor].chars().next_back() {
                Some(character) => self.cursor -= character.len_utf8(),
                None => return Outcome::Ding,
            },
            Command::DeleteBackwardChar => match self.input[..self.cursor].chars().next_back() {
                Some(character) => {
                    self.cursor -= character.len_utf8();
                    self.input.remove(self.cursor);
                }
                None => return Outcome::Ding,
            },
            Command::DeleteChar => {
                if self.cursor == self.input.len() {
                    return Outcome::Ding;
                }
                self.input.remove(self.cursor);
            }
            Command::KillLine => {
                let killed = self.input.split_off(self.cursor);
                kill_ring.kill_new(killed.into_bytes());
            }
            Command::Yank => {
                let Ok(text) = kill_ring.current_kill(0) else {
                    return Outcome::Ding;
                };
                let text = String::from_utf8_lossy(text).into_owned();
                self.input.insert_str(self.cursor, &text);
                self.cursor += text.len();
            }
            Command::PreviousHistory => self.move_in_history(1, histories),
            Command::NextHistory => self.move_in_history(-1, histories),
            Command::Exit => return self.exit(histories),
            Command::Quit => return Outcome::Quit,
        }
        Outcome::Continue
    }

    fn history_kind(&self) -> HistoryKind {
        match &self.mode {
            Mode::Line { history, .. } => *history,
            Mode::YesOrNo => HistoryKind::YesOrNo,
            Mode::YOrN | Mode::Keys { .. } => unreachable!("only line reads have history"),
        }
    }

    fn defaults(&self) -> &[String] {
        match &self.mode {
            Mode::Line { defaults, .. } => defaults,
            _ => &[],
        }
    }

    /// Emacs `previous-history-element` (step 1) and
    /// `next-history-element` (step -1).
    fn move_in_history(&mut self, step: i64, histories: &Histories) {
        let history = histories.list(self.history_kind());
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
                histories.add(*history, &self.input);
                Outcome::Exit(Answer::Line(self.input.clone()))
            }
            Mode::YesOrNo => {
                let answer = self.input.to_lowercase();
                histories.add(HistoryKind::YesOrNo, &self.input);
                match answer.as_str() {
                    YES => Outcome::Exit(Answer::Yes(true)),
                    NO => Outcome::Exit(Answer::Yes(false)),
                    _ => {
                        self.set_input(String::new());
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
