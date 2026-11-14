//! `query-replace` (`M-%`) and `query-replace-regexp` (`C-M-%`), ported
//! from Emacs replace.el's `perform-replace`: each match is shown and a key
//! says what to do with it. Each replacement made after a question is its
//! own undo group, as Emacs puts an undo boundary before every question.
//! The search for the next match runs on a worker for large buffers, like
//! isearch's.

use std::ops::Range;

use emx_text::Text;
use emx_text::search::{CompiledPattern, Match, Pattern, captures, match_at, search_backward};
use emx_types::{Key, KeyCode, Mods};

use super::Editor;
use super::Purpose;
use super::searching::{SearchOwner, SearchWait, isearch_pattern, match_of};
use crate::buffer::BufferId;
use crate::command::{self, CommandError, PrefixArg, char_after, char_before, is_word};
use crate::event::ClientId;
use crate::isearch::{MATCH_TRUNCATED, SEARCHING, describe, no_upper_case};
use crate::marker::{InsertionType, MarkerId};
use crate::minibuffer::{HistoryKind, Minibuffer, Mode};
use crate::search_job::{SearchOutcome, SearchRequest};
use crate::window::WindowId;

/// Emacs `query-replace-from-to-separator` on a terminal that can show it.
pub(crate) const FROM_TO_SEPARATOR: &str = " \u{2192} ";
/// Emacs `history-length`.
const HISTORY_LENGTH: usize = 100;
const QUERY_REPLACE: &str = "Query replace";
const HELP_BUFFER: &str = "*Help*";
const NO_PREVIOUS_MATCH: &str = "No previous match";
const NOTHING_TO_UNDO: &str = "Nothing to undo";
const NOT_SUPPORTED_HERE: &str = "Recursive editing is not supported in emx";
const INVALID_REPLACEMENT: &str = "Invalid use of \u{2018}\\\u{2019} in replacement text";
const NEWLINE_NOTE: &str = "Note: \u{2018}\\n\u{2019} here doesn\u{2019}t match a newline; to do that, type C-q C-j instead";
const TAB_NOTE: &str =
    "Note: \u{2018}\\t\u{2019} here doesn\u{2019}t match a tab; to do that, just type TAB";
/// `this-command` after a key that ends query-replace and runs as usual.
const MODE_EXITED: &str = "mode-exited";
/// Emacs `query-replace-help`, with the bindings it names spelled out.
const QUERY_REPLACE_HELP: &str = "Type SPC or y to replace one match, Delete or n to skip to next,
RET or q to exit, Period to replace one match and exit,
, to replace but not move point immediately,
C-r to enter recursive edit (C-M-c to get out again),
C-w to delete match and recursive edit,
C-l to clear the screen, redisplay, and offer same replacement again,
! to replace all remaining matches in this buffer with no more questions,
^ to move point back to previous match,
u to undo previous replacement,
U to undo all replacements,
e to edit the replacement string.
E to edit the replacement string with exact case.
In multi-buffer replacements type Y to replace all remaining
matches in all remaining buffers with no more questions,
N to skip to the next buffer without replacing remaining matches
in the current buffer.";

/// One part of a replacement: text, a group of the match (`\&` is group
/// 0), or the number of replacements so far (`\#`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Text(Vec<u8>),
    Group(usize),
    Count,
}

/// A replacement string as `replace-match` and
/// `query-replace-compile-replacement` read it; an invalid one fails when
/// the first replacement is made, as in Emacs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Template {
    source: Vec<u8>,
    pieces: Result<Vec<Piece>, String>,
}

impl Template {
    pub(crate) fn new(to: &[u8], regexp: bool) -> Template {
        let pieces = if regexp {
            parse_template(to)
        } else {
            Ok(vec![Piece::Text(to.to_vec())])
        };
        Template {
            source: to.to_vec(),
            pieces,
        }
    }

    fn literal(text: &[u8]) -> Template {
        Template {
            source: text.to_vec(),
            pieces: Ok(vec![Piece::Text(text.to_vec())]),
        }
    }

    /// The text replacing a match with these `groups`, after `count`
    /// replacements.
    fn expand(
        &self,
        text: &Text,
        groups: &[Option<Range<u64>>],
        count: u64,
    ) -> Result<Vec<u8>, CommandError> {
        self.expand_with(count, |group, expanded| {
            if let Some(Some(range)) = groups.get(group) {
                text.read(range.clone(), expanded)?;
            }
            Ok(())
        })
    }

    /// `expand` for a match whose groups' text is already read.
    pub(crate) fn expand_groups(
        &self,
        groups: &[Option<Vec<u8>>],
        count: u64,
    ) -> Result<Vec<u8>, CommandError> {
        self.expand_with(count, |group, expanded| {
            if let Some(Some(bytes)) = groups.get(group) {
                expanded.extend_from_slice(bytes);
            }
            Ok(())
        })
    }

    fn expand_with(
        &self,
        count: u64,
        mut group_text: impl FnMut(usize, &mut Vec<u8>) -> Result<(), CommandError>,
    ) -> Result<Vec<u8>, CommandError> {
        let pieces = self
            .pieces
            .as_ref()
            .map_err(|error| CommandError::User(error.clone()))?;
        let mut expanded = Vec::new();
        for piece in pieces {
            match piece {
                Piece::Text(bytes) => expanded.extend_from_slice(bytes),
                Piece::Count => expanded.extend_from_slice(count.to_string().as_bytes()),
                // A group that did not match, or does not exist, gives
                // nothing.
                Piece::Group(group) => group_text(*group, &mut expanded)?,
            }
        }
        Ok(expanded)
    }
}

fn parse_template(to: &[u8]) -> Result<Vec<Piece>, String> {
    let mut pieces = Vec::new();
    let mut text = Vec::new();
    let mut index = 0;
    while index < to.len() {
        let byte = to[index];
        if byte != b'\\' {
            text.push(byte);
            index += 1;
            continue;
        }
        let Some(next) = to.get(index + 1).copied() else {
            return Err(INVALID_REPLACEMENT.to_string());
        };
        index += 2;
        let piece = match next {
            b'&' => Piece::Group(0),
            b'1'..=b'9' => Piece::Group(usize::from(next - b'0')),
            b'#' => Piece::Count,
            b'\\' => {
                text.push(b'\\');
                continue;
            }
            b',' => return Err("Unsupported replacement construct `\\,` (Lisp expression)".into()),
            b'?' => return Err("Unsupported replacement construct `\\?`".into()),
            _ => return Err(INVALID_REPLACEMENT.to_string()),
        };
        if !text.is_empty() {
            pieces.push(Piece::Text(std::mem::take(&mut text)));
        }
        pieces.push(piece);
    }
    if !text.is_empty() {
        pieces.push(Piece::Text(text));
    }
    Ok(pieces)
}

/// How `replace-match` changes the case of a replacement for a match that
/// does not care about case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaseAction {
    Unchanged,
    AllCaps,
    CapitalizeInitials,
}

/// `Freplace_match`'s look at the matched text.
fn case_action(matched: &[u8]) -> CaseAction {
    let text = String::from_utf8_lossy(matched);
    let mut some_multiletter_word = false;
    let mut some_lowercase = false;
    let mut some_nonuppercase_initial = false;
    let mut some_uppercase = false;
    let mut previous = '\n';
    for character in text.chars() {
        let previous_is_word = is_word(u32::from(previous));
        let lower = character.to_lowercase().ne(std::iter::once(character));
        let is_lowercase = !lower && character.to_uppercase().ne(std::iter::once(character));
        if is_lowercase {
            some_lowercase = true;
            if previous_is_word {
                some_multiletter_word = true;
            } else {
                some_nonuppercase_initial = true;
            }
        } else if lower {
            some_uppercase = true;
            if previous_is_word {
                some_multiletter_word = true;
            }
        } else if !previous_is_word && is_word(u32::from(character)) {
            // A caseless word constituent starting a word counts as a
            // lower-case initial.
            some_nonuppercase_initial = true;
        }
        previous = character;
    }
    if !some_lowercase && some_multiletter_word {
        CaseAction::AllCaps
    } else if !some_nonuppercase_initial && some_multiletter_word {
        CaseAction::CapitalizeInitials
    } else if !some_nonuppercase_initial && some_uppercase {
        CaseAction::AllCaps
    } else {
        CaseAction::Unchanged
    }
}

fn change_case(text: &[u8], action: CaseAction) -> Vec<u8> {
    let mut changed = Vec::with_capacity(text.len());
    let mut previous_is_word = false;
    for chunk in text.utf8_chunks() {
        for character in chunk.valid().chars() {
            let word = is_word(u32::from(character));
            let mut encoded = [0; 4];
            let upcase = match action {
                CaseAction::Unchanged => false,
                CaseAction::AllCaps => true,
                CaseAction::CapitalizeInitials => word && !previous_is_word,
            };
            if upcase {
                for upper in character.to_uppercase() {
                    changed.extend_from_slice(upper.encode_utf8(&mut encoded).as_bytes());
                }
            } else {
                changed.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
            }
            previous_is_word = word;
        }
        changed.extend_from_slice(chunk.invalid());
        if !chunk.invalid().is_empty() {
            previous_is_word = true;
        }
    }
    changed
}

/// Emacs `word-search-regexp` (not lax): the words of `string` with any
/// non-word characters between them.
fn word_search_regexp(string: &str) -> String {
    if string.is_empty() {
        return String::new();
    }
    let is_word_char = |character: char| is_word(u32::from(character));
    if !string.chars().any(is_word_char) {
        return r"\W+".to_string();
    }
    let words: Vec<String> = string
        .split(|character: char| !is_word_char(character))
        .filter(|word| !word.is_empty())
        .map(emx_text::search::regexp_quote)
        .collect();
    let starts_non_word = string
        .chars()
        .next()
        .is_some_and(|first| !is_word_char(first));
    let ends_non_word = string
        .chars()
        .last()
        .is_some_and(|last| !is_word_char(last));
    format!(
        "{}{}{}",
        if starts_non_word { r"\W+" } else { r"\<" },
        words.join(r"\W+"),
        if ends_non_word { r"\W+" } else { r"\>" }
    )
}

/// The match being asked about, as markers so other clients' edits move
/// it: every group, the whole match first.
#[derive(Debug, Clone)]
struct MatchMarkers {
    groups: Vec<Option<(MarkerId, MarkerId)>>,
}

/// Emacs `match-again`: whether the next match may start right where this
/// one ends.
#[derive(Debug, Clone)]
enum MatchAgain {
    /// Search from point.
    Adjacent,
    /// Move one character before searching.
    NotAdjacent,
    /// The adjacent match, found before this one was replaced.
    Known(MatchMarkers),
}

/// One answered match, for `^` and `u` (`replace--push-stack`).
#[derive(Debug, Clone)]
struct StackEntry {
    point: MarkerId,
    replaced: bool,
    groups: MatchMarkers,
    search_string_replaced: Vec<u8>,
    next_replacement_replaced: Vec<u8>,
}

/// Where a search for the next match started, to go back to when a
/// one-character step finds nothing.
#[derive(Debug, Clone)]
pub(crate) struct ReplaceStep {
    restore: Option<u64>,
}

/// The query-replace of one client.
#[derive(Debug)]
pub(crate) struct QueryReplace {
    buffer: BufferId,
    window: WindowId,
    from: Vec<u8>,
    template: Template,
    pattern: CompiledPattern,
    regexp: bool,
    delimited: bool,
    backward: bool,
    /// Emacs `nocasify`: replacements keep their case.
    nocasify: bool,
    /// Asking about each match; `!` turns this off.
    query: bool,
    limit: Option<MarkerId>,
    match_again: MatchAgain,
    current: Option<MatchMarkers>,
    replaced: bool,
    stack: Vec<StackEntry>,
    replace_count: u64,
    last_was_undo: bool,
    last_was_act_and_show: bool,
    update_stack: bool,
    search_string_replaced: Vec<u8>,
    next_replacement_replaced: Vec<u8>,
    capped: bool,
    /// The question is being asked; keys answer it.
    asking: bool,
}

impl QueryReplace {
    fn release(self, buffer: &mut crate::buffer::Buffer) {
        if let Some(limit) = self.limit {
            buffer.delete_marker(limit);
        }
        if let MatchAgain::Known(groups) = &self.match_again {
            release_groups(buffer, groups);
        }
        if let Some(current) = &self.current {
            release_groups(buffer, current);
        }
        for entry in &self.stack {
            buffer.delete_marker(entry.point);
            release_groups(buffer, &entry.groups);
        }
    }
}

fn release_groups(buffer: &mut crate::buffer::Buffer, groups: &MatchMarkers) {
    for (start, end) in groups.groups.iter().flatten() {
        buffer.delete_marker(*start);
        buffer.delete_marker(*end);
    }
}

/// What a key does in a query (`query-replace-map`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    Act,
    Skip,
    Exit,
    ActAndExit,
    ActAndShow,
    Automatic,
    Backup,
    Undo,
    UndoAll,
    EditReplacement,
    EditReplacementExactCase,
    Recenter,
    Help,
    Quit,
    RecursiveEdit,
    ScrollUp,
    ScrollDown,
}

fn answer_of(key: Key) -> Option<Answer> {
    let Mods {
        ctrl, meta, super_, ..
    } = key.mods;
    let answer = match (key.code, ctrl, meta, super_) {
        (KeyCode::Char(' ' | 'y' | 'Y'), false, false, false) => Answer::Act,
        (KeyCode::Char('n' | 'N'), false, false, false)
        | (KeyCode::Backspace | KeyCode::Delete, false, false, false) => Answer::Skip,
        (KeyCode::Char('q'), false, false, false) | (KeyCode::Enter, false, false, false) => {
            Answer::Exit
        }
        (KeyCode::Char('.'), false, false, false) => Answer::ActAndExit,
        (KeyCode::Char(','), false, false, false) => Answer::ActAndShow,
        (KeyCode::Char('!'), false, false, false) => Answer::Automatic,
        (KeyCode::Char('^'), false, false, false) => Answer::Backup,
        (KeyCode::Char('u'), false, false, false) => Answer::Undo,
        (KeyCode::Char('U'), false, false, false) => Answer::UndoAll,
        (KeyCode::Char('e'), false, false, false) => Answer::EditReplacement,
        (KeyCode::Char('E'), false, false, false) => Answer::EditReplacementExactCase,
        (KeyCode::Char('l'), true, false, false) => Answer::Recenter,
        (KeyCode::Char('?'), false, false, false)
        | (KeyCode::Char('h'), true, false, false)
        | (KeyCode::F(1), false, false, false) => Answer::Help,
        (KeyCode::Char('g' | ']'), true, false, false) => Answer::Quit,
        (KeyCode::Char('r' | 'w'), true, false, false) => Answer::RecursiveEdit,
        (KeyCode::Char('v'), true, false, false) | (KeyCode::PageDown, false, false, false) => {
            Answer::ScrollUp
        }
        (KeyCode::Char('v'), false, true, false) | (KeyCode::PageUp, false, false, false) => {
            Answer::ScrollDown
        }
        _ => return None,
    };
    Some(answer)
}

/// What the minibuffer reads for a query-replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplaceArguments {
    pub regexp: bool,
    pub delimited: bool,
    pub backward: bool,
    /// The prompt's start: "Query replace", "Query replace regexp in
    /// region", ...
    pub prompt: String,
    /// Case folding and lax whitespace when started from isearch, which
    /// decides them itself.
    pub from_isearch: Option<(bool, bool)>,
}

impl Editor {
    // ---- Reading the arguments ------------------------------------------

    /// `query-replace` and `query-replace-regexp`: read what to replace and
    /// with what, then ask about each match from point (or in the active
    /// region).
    pub(crate) fn query_replace_command(
        &mut self,
        client: ClientId,
        regexp: bool,
        prefix_arg: PrefixArg,
    ) -> Result<(), CommandError> {
        let buffer = self.selected_buffer(client);
        if let Err(error) = self.buffers[buffer].check_editable() {
            return Err(command::read_only_error(&self.buffers[buffer], error));
        }
        let backward = prefix_arg == PrefixArg::Negative;
        let delimited = prefix_arg != PrefixArg::None && !backward;
        let mut prompt = QUERY_REPLACE.to_string();
        if regexp {
            prompt.push_str(" regexp");
        }
        if backward {
            prompt.push_str(" backward");
        } else if delimited {
            prompt.push_str(" word");
        }
        if self.use_region(client) {
            prompt.push_str(" in region");
        }
        let arguments = ReplaceArguments {
            regexp,
            delimited,
            backward,
            prompt,
            from_isearch: None,
        };
        self.read_replace_from(client, arguments);
        Ok(())
    }

    /// Emacs `use-region-p` in the client's selected buffer.
    fn use_region(&self, client: ClientId) -> bool {
        let buffer = &self.buffers[self.selected_buffer(client)];
        let point = self.window_point(client, self.selected_window_id(client));
        buffer.mark_active
            && buffer
                .mark
                .is_some_and(|mark| buffer.marker_pos(mark) != point)
    }

    /// `query-replace-read-from`: the prompt offers the last pair as the
    /// default, and history shows earlier pairs as `FROM → TO`.
    fn read_replace_from(&mut self, client: ClientId, arguments: ReplaceArguments) {
        let pairs: Vec<Vec<u8>> = self
            .query_replace_defaults
            .iter()
            .map(|(from, to)| {
                let mut entry = describe(from).into_bytes();
                entry.extend_from_slice(FROM_TO_SEPARATOR.as_bytes());
                entry.extend_from_slice(describe(to).as_bytes());
                entry
            })
            .collect();
        let prompt = match pairs.first() {
            Some(pair) => format!(
                "{} (default {}): ",
                arguments.prompt,
                String::from_utf8_lossy(pair)
            ),
            None => format!("{}: ", arguments.prompt),
        };
        let mut history = pairs;
        history.extend(self.query_replace_history.iter().cloned());
        let mut suggestions = Vec::new();
        if self.use_region(client) {
            let buffer = &self.buffers[self.selected_buffer(client)];
            let point = self.window_point(client, self.selected_window_id(client));
            let mark = buffer.marker_pos(buffer.mark.expect("use_region checked the mark"));
            let mut text = Vec::new();
            if buffer
                .text
                .read(mark.min(point)..mark.max(point), &mut text)
                .is_ok()
            {
                suggestions.push(text);
            }
        }
        suggestions.extend(self.search_ring.front().cloned());
        suggestions.extend(self.query_replace_history.first().cloned());
        let mode = Mode::Line {
            history: HistoryKind::QueryReplace,
            defaults: suggestions,
            require: None,
            allow_empty: true,
        };
        let minibuffer = Minibuffer::new(
            prompt,
            Vec::new(),
            mode,
            Purpose::QueryReplaceFrom(Box::new(arguments)),
        )
        .with_own_history(history, 0);
        self.start_minibuffer(client, minibuffer);
    }

    /// The answer to the "replace what" prompt.
    pub(super) fn replace_from_answered(
        &mut self,
        client: ClientId,
        arguments: ReplaceArguments,
        input: Vec<u8>,
    ) -> Result<(), CommandError> {
        if input.is_empty()
            && let Some((from, to)) = self.query_replace_defaults.first().cloned()
        {
            return self.perform_replace_from_arguments(client, &arguments, from, to);
        }
        let separator = FROM_TO_SEPARATOR.as_bytes();
        let split = input
            .windows(separator.len())
            .position(|window| window == separator);
        if let Some(at) = split {
            let from = input[..at].to_vec();
            let to = input[at + separator.len()..].to_vec();
            add_to_history(&mut self.query_replace_history, from.clone());
            add_to_history(&mut self.query_replace_history, to.clone());
            add_to_history(&mut self.query_replace_defaults, (from.clone(), to.clone()));
            return self.perform_replace_from_arguments(client, &arguments, from, to);
        }
        add_to_history(&mut self.query_replace_history, input.clone());
        if arguments.regexp {
            self.note_escapes(client, &input);
        }
        self.read_replace_to(client, arguments, input);
        Ok(())
    }

    /// A regexp with `\n` or `\t` probably meant a newline or tab.
    fn note_escapes(&mut self, client: ClientId, from: &[u8]) {
        let mut index = 0;
        while index + 1 < from.len() {
            if from[index] == b'\\' {
                match from[index + 1] {
                    b'n' => return self.set_echo(client, NEWLINE_NOTE.to_string()),
                    b't' => return self.set_echo(client, TAB_NOTE.to_string()),
                    _ => index += 2,
                }
            } else {
                index += 1;
            }
        }
    }

    /// `query-replace-read-to`.
    fn read_replace_to(&mut self, client: ClientId, arguments: ReplaceArguments, from: Vec<u8>) {
        let prompt = format!("{} {} with: ", arguments.prompt, describe(&from));
        let mode = Mode::Line {
            history: HistoryKind::QueryReplace,
            defaults: vec![from.clone()],
            require: None,
            allow_empty: true,
        };
        let history = self.query_replace_history.clone();
        let minibuffer = Minibuffer::new(
            prompt,
            Vec::new(),
            mode,
            Purpose::QueryReplaceTo(Box::new(arguments), from),
        )
        .with_own_history(history, 0);
        self.start_minibuffer(client, minibuffer);
    }

    pub(super) fn replace_to_answered(
        &mut self,
        client: ClientId,
        arguments: ReplaceArguments,
        from: Vec<u8>,
        to: Vec<u8>,
    ) -> Result<(), CommandError> {
        add_to_history(&mut self.query_replace_history, to.clone());
        add_to_history(&mut self.query_replace_defaults, (from.clone(), to.clone()));
        self.perform_replace_from_arguments(client, &arguments, from, to)
    }

    /// `isearch-query-replace`: the search string is what to replace.
    pub(super) fn query_replace_from_isearch(
        &mut self,
        client: ClientId,
        string: Vec<u8>,
        regexp: bool,
        (case_fold, lax_whitespace): (bool, bool),
        delimited: bool,
        backward: bool,
    ) {
        let mut prompt = QUERY_REPLACE.to_string();
        if delimited {
            prompt.push_str(" word");
        } else if regexp {
            prompt.push_str(" regexp");
        }
        if backward {
            prompt.push_str(" backward");
        }
        if self.use_region(client) {
            prompt.push_str(" in region");
        }
        self.query_replace_history.insert(0, string.clone());
        self.query_replace_history.truncate(HISTORY_LENGTH);
        let arguments = ReplaceArguments {
            regexp,
            delimited,
            backward,
            prompt,
            from_isearch: Some((case_fold, lax_whitespace)),
        };
        self.read_replace_to(client, arguments, string);
    }

    fn perform_replace_from_arguments(
        &mut self,
        client: ClientId,
        arguments: &ReplaceArguments,
        from: Vec<u8>,
        to: Vec<u8>,
    ) -> Result<(), CommandError> {
        let region = self.use_region(client).then(|| {
            let buffer = &self.buffers[self.selected_buffer(client)];
            let point = self.window_point(client, self.selected_window_id(client));
            let mark = buffer.marker_pos(buffer.mark.expect("use_region checked the mark"));
            (mark.min(point), mark.max(point))
        });
        self.perform_replace(client, arguments, from, to, region)
    }

    // ---- The replacing loop ---------------------------------------------

    /// `perform-replace` with the query flag.
    fn perform_replace(
        &mut self,
        client: ClientId,
        arguments: &ReplaceArguments,
        from: Vec<u8>,
        to: Vec<u8>,
        region: Option<(u64, u64)>,
    ) -> Result<(), CommandError> {
        let from_text = String::from_utf8_lossy(&from).into_owned();
        let (case_fold, lax_whitespace) = arguments
            .from_isearch
            .unwrap_or_else(|| (no_upper_case(&from_text, arguments.regexp), false));
        let pattern = if arguments.delimited {
            Pattern::Regex {
                emacs: word_search_regexp(&from_text),
                case_fold,
            }
        } else {
            isearch_pattern(&from, arguments.regexp, lax_whitespace, case_fold)
        };
        let window = self.selected_window_id(client);
        let buffer = self.window(client, window).buffer;
        let limit = region.map(|(start, end)| {
            let (stop, begin) = if arguments.backward {
                (start, end)
            } else {
                (end, start)
            };
            self.set_window_point(client, window, begin);
            self.buffers[buffer].mark_active = false;
            self.buffers[buffer].create_marker_without_undo(stop, InsertionType::StayBefore)
        });
        let point = self.window_point(client, window);
        {
            let mut ctx = command::CommandContext::new(
                self,
                client,
                "query-replace",
                None,
                PrefixArg::None,
                None,
            );
            command::push_mark(&mut ctx, point, false, false);
        }
        self.buffers[buffer].undo.boundary(point);
        // Emacs finds a bad regexp at the first search, after the mark.
        let pattern = match CompiledPattern::compile(&pattern) {
            Ok(pattern) => pattern,
            Err(error) => {
                if let Some(limit) = limit {
                    self.buffers[buffer].delete_marker(limit);
                }
                return Err(CommandError::User(invalid_regexp_message(&error)));
            }
        };
        self.client_mut(client).query_replace = Some(QueryReplace {
            buffer,
            window,
            from,
            template: Template::new(&to, arguments.regexp),
            pattern,
            regexp: arguments.regexp,
            delimited: arguments.delimited,
            backward: arguments.backward,
            nocasify: !case_fold,
            query: true,
            limit,
            match_again: MatchAgain::Adjacent,
            current: None,
            replaced: false,
            stack: Vec::new(),
            replace_count: 0,
            last_was_undo: false,
            last_was_act_and_show: false,
            update_stack: true,
            search_string_replaced: Vec::new(),
            next_replacement_replaced: Vec::new(),
            capped: false,
            asking: false,
        });
        self.replace_next(client)
    }

    fn replace(&self, client: ClientId) -> &QueryReplace {
        self.client(client)
            .query_replace
            .as_ref()
            .expect("query-replace steps run during a query-replace")
    }

    fn replace_mut(&mut self, client: ClientId) -> &mut QueryReplace {
        self.client_mut(client)
            .query_replace
            .as_mut()
            .expect("query-replace steps run during a query-replace")
    }

    pub(crate) fn query_replace_active(&self, client: ClientId) -> bool {
        self.client(client)
            .query_replace
            .as_ref()
            .is_some_and(|session| session.asking)
    }

    fn replace_point(&self, client: ClientId) -> u64 {
        self.window_point(client, self.replace(client).window)
    }

    fn replace_goto(&mut self, client: ClientId, position: u64) {
        let window = self.replace(client).window;
        self.set_window_point(client, window, position);
    }

    fn group_positions(&self, client: ClientId, groups: &MatchMarkers) -> Vec<Option<Range<u64>>> {
        let buffer = &self.buffers[self.replace(client).buffer];
        groups
            .groups
            .iter()
            .map(|group| group.map(|(start, end)| buffer.marker_pos(start)..buffer.marker_pos(end)))
            .collect()
    }

    fn markers_for(&mut self, buffer: BufferId, groups: &[Option<Range<u64>>]) -> MatchMarkers {
        let target = &mut self.buffers[buffer];
        MatchMarkers {
            groups: groups
                .iter()
                .map(|group| {
                    group.as_ref().map(|range| {
                        (
                            target
                                .create_marker_without_undo(range.start, InsertionType::StayBefore),
                            // The end follows text inserted at it, so a
                            // replacement ends where the new text ends.
                            target
                                .create_marker_without_undo(range.end, InsertionType::AdvanceAfter),
                        )
                    })
                })
                .collect(),
        }
    }

    /// The top of `perform-replace`'s loop: find the next match, if the
    /// search is to go on.
    fn replace_next(&mut self, client: ClientId) -> Result<(), CommandError> {
        loop {
            let session = self.replace(client);
            let buffer = session.buffer;
            let point = self.replace_point(client);
            let length = self.buffers[buffer].text.len();
            let limit = session
                .limit
                .map(|limit| self.buffers[buffer].marker_pos(limit));
            let at_end = if session.backward {
                point == 0 || limit.is_some_and(|limit| point <= limit)
            } else {
                point == length || limit.is_some_and(|limit| point >= limit)
            };
            if at_end {
                self.replace_finish(client, None);
                return Ok(());
            }
            let match_again = std::mem::replace(
                &mut self.replace_mut(client).match_again,
                MatchAgain::Adjacent,
            );
            let restore = match match_again {
                MatchAgain::Known(groups) => {
                    let positions = self.group_positions(client, &groups);
                    release_groups(&mut self.buffers[buffer], &groups);
                    let whole = positions[0].clone().expect("a match has group 0");
                    let backward = self.replace(client).backward;
                    self.replace_goto(client, if backward { whole.start } else { whole.end });
                    let found = Match {
                        start: whole.start,
                        end: whole.end,
                        capped: false,
                    };
                    match self.replace_found(client, found, Some(positions))? {
                        true => continue,
                        false => return Ok(()),
                    }
                }
                MatchAgain::Adjacent => None,
                MatchAgain::NotAdjacent => {
                    let backward = self.replace(client).backward;
                    let text = &self.buffers[buffer].text;
                    let step = if backward {
                        char_before(text, point)?
                    } else {
                        char_after(text, point)?
                    };
                    let target = match step {
                        Some((_, length)) if backward => point - length,
                        Some((_, length)) => point + length,
                        None => {
                            self.replace_finish(client, None);
                            return Ok(());
                        }
                    };
                    let room = if backward {
                        target > 0 && limit.is_none_or(|limit| target > limit)
                    } else {
                        target < length && limit.is_none_or(|limit| target < limit)
                    };
                    if !room {
                        self.replace_finish(client, None);
                        return Ok(());
                    }
                    self.replace_goto(client, target);
                    Some(point)
                }
            };
            match self.replace_search(client, restore)? {
                Some(true) => continue,
                Some(false) | None => return Ok(()),
            }
        }
    }

    /// Searches from point; `Some(true)` when the loop goes on at once,
    /// `Some(false)` when it stopped or waits for a key, `None` while the
    /// search runs on a worker.
    fn replace_search(
        &mut self,
        client: ClientId,
        restore: Option<u64>,
    ) -> Result<Option<bool>, CommandError> {
        let session = self.replace(client);
        let buffer = session.buffer;
        let point = self.replace_point(client);
        let limit = session
            .limit
            .map(|limit| self.buffers[buffer].marker_pos(limit));
        let request = if session.backward {
            SearchRequest::Backward {
                from: point,
                bound: limit,
            }
        } else {
            SearchRequest::Forward {
                from: point,
                bound: limit,
            }
        };
        let pattern = session.pattern.clone();
        let step = ReplaceStep { restore };
        match self.start_search(
            client,
            buffer,
            pattern,
            request,
            SearchOwner::QueryReplace(step),
        ) {
            Some(result) => self
                .replace_search_done(client, match_of(result), restore)
                .map(Some),
            None => {
                self.show_replace_searching(client);
                Ok(None)
            }
        }
    }

    fn show_replace_searching(&mut self, client: ClientId) {
        let message = format!("{}[{SEARCHING}]", self.replace_question(client));
        self.client_mut(client).echo = message;
    }

    /// The search for the next match is done.
    fn replace_search_done(
        &mut self,
        client: ClientId,
        result: Result<Option<Match>, String>,
        restore: Option<u64>,
    ) -> Result<bool, CommandError> {
        let found = result.map_err(CommandError::User)?;
        let Some(found) = found else {
            if let Some(restore) = restore {
                self.replace_goto(client, restore);
            }
            self.replace_finish(client, None);
            return Ok(false);
        };
        let backward = self.replace(client).backward;
        self.replace_goto(client, if backward { found.start } else { found.end });
        self.replace_found(client, found, None)
    }

    /// A worker's search for the next match arrived.
    pub(super) fn replace_search_arrived(
        &mut self,
        client: ClientId,
        wait: SearchWait,
        result: Result<SearchOutcome, String>,
        step: ReplaceStep,
    ) {
        if self.client(client).query_replace.is_none() {
            return;
        }
        let buffer = self.replace(client).buffer;
        let queued = wait.queued.clone();
        let journal_start = self.journal_outbox.len();
        let outcome = if self.search_is_current(buffer, &wait) {
            self.replace_search_done(client, match_of(result), step.restore)
                .and_then(|go_on| {
                    if go_on {
                        self.replace_next(client)
                    } else {
                        Ok(())
                    }
                })
        } else {
            self.replace_search(client, step.restore)
                .and_then(|go_on| match go_on {
                    Some(true) => self.replace_next(client),
                    _ => Ok(()),
                })
        };
        self.replace_outcome(client, outcome, journal_start);
        match self.client_mut(client).search_wait.as_mut() {
            Some(wait) => {
                let mut all = queued;
                all.extend(wait.queued.drain(..));
                wait.queued = all;
            }
            None => self.replay_queued(client, queued),
        }
    }

    /// Ends a step taken outside a command: an error ends the
    /// query-replace, and changes get their undo boundary.
    fn replace_outcome(
        &mut self,
        client: ClientId,
        outcome: Result<(), CommandError>,
        journal_start: usize,
    ) {
        if let Err(error) = outcome {
            self.replace_abandon(client);
            self.signal(client, Some(QUERY_REPLACE_COMMAND), &error);
        }
        self.finish_command(client, journal_start);
    }

    /// A match was found; `groups` are known when it was found before the
    /// previous replacement. Returns whether the loop goes on without a
    /// question.
    fn replace_found(
        &mut self,
        client: ClientId,
        found: Match,
        groups: Option<Vec<Option<Range<u64>>>>,
    ) -> Result<bool, CommandError> {
        let session = self.replace(client);
        let buffer = session.buffer;
        let text = self.buffers[buffer].text.clone();
        let groups = match groups {
            Some(groups) => groups,
            None => captures(&text, &session.pattern, &found).map_err(search_error)?,
        };
        let nonempty = found.start != found.end;
        // The next match may start where this one ends; with a regexp, it
        // is found now, before the replacement changes what follows.
        let match_again = if !nonempty {
            MatchAgain::NotAdjacent
        } else if !session.regexp {
            MatchAgain::Adjacent
        } else {
            let point = self.replace_point(client);
            let again = if session.backward {
                search_backward(
                    &text,
                    &session.pattern,
                    point,
                    None,
                    &std::sync::atomic::AtomicBool::new(false),
                    self.match_cap(),
                )
                .map_err(search_error)?
                .filter(|again| again.end == point)
            } else {
                match_at(&text, &session.pattern, point, None, self.match_cap())
                    .map_err(search_error)?
            };
            match again.filter(|again| again.start != again.end) {
                Some(again) => {
                    let again_groups =
                        captures(&text, &session.pattern, &again).map_err(search_error)?;
                    MatchAgain::Known(self.markers_for(buffer, &again_groups))
                }
                None => MatchAgain::NotAdjacent,
            }
        };
        let current = self.markers_for(buffer, &groups);
        let session = self.replace_mut(client);
        session.match_again = match_again;
        session.capped = found.capped;
        if let Some(old) = session.current.replace(current) {
            release_groups(&mut self.buffers[buffer], &old);
        }
        if !self.replace(client).query {
            self.replace_current(client, None)?;
            return Ok(true);
        }
        let point = self.replace_point(client);
        self.buffers[buffer].undo.boundary(point);
        self.replace_ask(client)?;
        Ok(false)
    }

    /// The text that would replace the current match, case adjusted.
    fn replacement_for_current(
        &self,
        client: ClientId,
        template: &Template,
        nocasify: bool,
    ) -> Result<Vec<u8>, CommandError> {
        let session = self.replace(client);
        let current = session.current.as_ref().expect("a match is current");
        let groups = self.group_positions(client, current);
        let text = &self.buffers[session.buffer].text;
        let expanded = template.expand(text, &groups, session.replace_count)?;
        if nocasify {
            return Ok(expanded);
        }
        let whole = groups[0].clone().expect("a match has group 0");
        let mut matched = Vec::new();
        text.read(whole, &mut matched)?;
        Ok(change_case(&expanded, case_action(&matched)))
    }

    /// `replace-match-maybe-edit` on the current match, with `template`
    /// instead of the query-replace's own when given.
    fn replace_current(
        &mut self,
        client: ClientId,
        template: Option<(Template, bool)>,
    ) -> Result<(), CommandError> {
        let (template, nocasify) = match template {
            Some(template) => template,
            None => {
                let session = self.replace(client);
                (session.template.clone(), session.nocasify)
            }
        };
        let replacement = self.replacement_for_current(client, &template, nocasify)?;
        let session = self.replace(client);
        let buffer = session.buffer;
        let backward = session.backward;
        let current = session.current.clone().expect("a match is current");
        let (start_marker, end_marker) = current.groups[0].expect("a match has group 0");
        let target = &self.buffers[buffer];
        let range = target.marker_pos(start_marker)..target.marker_pos(end_marker);
        let point = self.replace_point(client);
        let (target, recorder) = self.buffer_and_recorder(buffer);
        target
            .replace_range(range.clone(), &replacement, point, recorder)
            .map_err(|error| command::read_only_error(target, error))?;
        // The match now covers the replacement.
        target.set_marker(start_marker, range.start);
        let end = range.start + replacement.len() as u64;
        target.set_marker(end_marker, end);
        self.replace_goto(client, if backward { range.start } else { end });
        self.replace_mut(client).replace_count += 1;
        Ok(())
    }

    /// `query-replace-descr` of what replaces the current match.
    fn replace_question(&self, client: ClientId) -> String {
        let session = self.replace(client);
        let replacement = self
            .replacement_for_current(client, &session.template, session.nocasify)
            .unwrap_or_else(|_| session.template.source.clone());
        let mut question = String::from("Query replacing ");
        if session.backward {
            question.push_str("backward ");
        }
        if session.delimited {
            question.push_str("word ");
        }
        if session.regexp {
            question.push_str("regexp ");
        }
        question.push_str(&format!(
            "{} with {}: (? for help) ",
            describe(&session.from),
            describe(&replacement)
        ));
        if session.capped {
            question.push_str(&format!("[{MATCH_TRUNCATED}] "));
        }
        question
    }

    /// Shows the question for the current match and waits for a key.
    fn replace_ask(&mut self, client: ClientId) -> Result<(), CommandError> {
        if self.replace(client).last_was_undo && self.replace(client).regexp {
            // The undone text is back: its groups are found again.
            let session = self.replace(client);
            let current = session.current.clone().expect("a match is current");
            let start = self.group_positions(client, &current)[0]
                .clone()
                .expect("a match has group 0")
                .start;
            let text = self.buffers[session.buffer].text.clone();
            if let Some(again) = match_at(&text, &session.pattern, start, None, self.match_cap())
                .map_err(search_error)?
            {
                let groups = captures(&text, &session.pattern, &again).map_err(search_error)?;
                let buffer = session.buffer;
                let markers = self.markers_for(buffer, &groups);
                if let Some(old) = self.replace_mut(client).current.replace(markers) {
                    release_groups(&mut self.buffers[buffer], &old);
                }
            }
            self.replace_mut(client).last_was_undo = false;
        }
        let session = self.replace(client);
        let current = session.current.clone().expect("a match is current");
        let whole = self.group_positions(client, &current)[0]
            .clone()
            .expect("a match has group 0");
        let mut matched = Vec::new();
        self.buffers[session.buffer]
            .text
            .read(whole, &mut matched)?;
        let next = self.replacement_for_current(client, &session.template, session.nocasify)?;
        let session = self.replace_mut(client);
        session.search_string_replaced = matched;
        session.next_replacement_replaced = next;
        session.asking = true;
        let question = self.replace_question(client);
        self.client_mut(client).echo = question;
        Ok(())
    }

    /// A key while query-replace asks about a match.
    pub(super) fn query_replace_key(&mut self, client: ClientId, key: Key) {
        let journal_start = self.journal_outbox.len();
        let outcome = self.replace_answer(client, key);
        self.replace_outcome(client, outcome, journal_start);
    }

    fn replace_answer(&mut self, client: ClientId, key: Key) -> Result<(), CommandError> {
        let Some(answer) = answer_of(key) else {
            // Any other key ends query-replace and then runs as usual.
            self.replace_finish(client, Some(key));
            return Ok(());
        };
        let session = self.replace(client);
        let replaced = session.replaced;
        let done = match answer {
            Answer::Help => {
                self.replace_help(client);
                return self.replace_ask(client);
            }
            Answer::Exit => {
                self.replace_mut(client).asking = false;
                self.replace_push_stack(client);
                self.replace_finish(client, None);
                return Ok(());
            }
            Answer::Backup => {
                self.replace_backup(client);
                return self.replace_ask(client);
            }
            Answer::Undo | Answer::UndoAll => {
                self.replace_undo(client, answer == Answer::UndoAll)?;
                return self.replace_ask(client);
            }
            Answer::Act => {
                if !replaced {
                    self.replace_current(client, None)?;
                }
                let session = self.replace_mut(client);
                session.update_stack = !session.last_was_act_and_show;
                session.replaced = true;
                true
            }
            Answer::ActAndExit => {
                if !replaced {
                    self.replace_current(client, None)?;
                }
                self.replace_mut(client).replaced = true;
                self.replace_mut(client).asking = false;
                self.replace_finish(client, None);
                return Ok(());
            }
            Answer::ActAndShow => {
                if !replaced {
                    self.replace_current(client, None)?;
                    let session = self.replace_mut(client);
                    session.replaced = true;
                    session.last_was_act_and_show = true;
                    self.replace_push_entry(client);
                }
                return self.replace_ask(client);
            }
            Answer::Automatic => {
                if !replaced {
                    self.replace_current(client, None)?;
                }
                let session = self.replace_mut(client);
                session.query = false;
                session.replaced = true;
                true
            }
            Answer::Skip => {
                let session = self.replace_mut(client);
                session.update_stack = !session.last_was_act_and_show;
                true
            }
            Answer::Recenter => {
                self.client_mut(client).command_loop.last_command = Some(RECENTER_TOP_BOTTOM);
                let spec = command::lookup(RECENTER_TOP_BOTTOM).expect("recenter is a command");
                self.run_command(client, spec.name, Some(key), spec.run);
                return self.replace_ask(client);
            }
            Answer::ScrollUp | Answer::ScrollDown => {
                let name = if answer == Answer::ScrollUp {
                    "scroll-up-command"
                } else {
                    "scroll-down-command"
                };
                let spec = command::lookup(name).expect("scrolling is a command");
                self.run_command(client, spec.name, Some(key), spec.run);
                return self.replace_ask(client);
            }
            Answer::EditReplacement | Answer::EditReplacementExactCase => {
                let exact = answer == Answer::EditReplacementExactCase;
                let prompt = if exact {
                    "Edit replacement string (exact case): "
                } else {
                    "Edit replacement string: "
                };
                let initial = self.replace(client).template.source.clone();
                self.read_from_minibuffer(
                    client,
                    prompt.to_string(),
                    initial,
                    Mode::Line {
                        history: HistoryKind::QueryReplace,
                        defaults: Vec::new(),
                        require: None,
                        allow_empty: true,
                    },
                    Purpose::QueryReplaceEdit { exact_case: exact },
                );
                return Ok(());
            }
            Answer::RecursiveEdit => {
                self.ring_bell(client);
                let question = self.replace_question(client);
                self.client_mut(client).echo = format!("{question}[{NOT_SUPPORTED_HERE}]");
                return Ok(());
            }
            Answer::Quit => {
                self.replace_abandon(client);
                return Err(CommandError::Quit);
            }
        };
        if done {
            self.replace_mut(client).asking = false;
            if self.replace(client).update_stack {
                self.replace_push_stack(client);
            }
            let session = self.replace_mut(client);
            session.replaced = false;
            session.last_was_act_and_show = false;
            session.update_stack = true;
            self.client_mut(client).recenter_last_op = None;
            self.replace_next(client)?;
        }
        Ok(())
    }

    /// The replacement edited after `e` or `E`.
    pub(super) fn replace_edit_answered(
        &mut self,
        client: ClientId,
        exact_case: bool,
        replacement: Vec<u8>,
    ) -> Result<(), CommandError> {
        if self.client(client).query_replace.is_none() {
            return Ok(());
        }
        let journal_start = self.journal_outbox.len();
        let regexp = self.replace(client).regexp;
        let template = Template::new(&replacement, regexp);
        self.replace_mut(client).template = template.clone();
        let outcome = (|| {
            if !self.replace(client).replaced {
                let nocasify = exact_case || self.replace(client).nocasify;
                self.replace_current(client, Some((template, nocasify)))?;
                let session = self.replace_mut(client);
                session.replaced = true;
                session.next_replacement_replaced = replacement.clone();
            }
            self.replace_mut(client).asking = false;
            if self.replace(client).update_stack {
                self.replace_push_stack(client);
            }
            let session = self.replace_mut(client);
            session.replaced = false;
            session.last_was_act_and_show = false;
            session.update_stack = true;
            self.replace_next(client)
        })();
        self.replace_outcome(client, outcome, journal_start);
        Ok(())
    }

    /// The edit of the replacement was quit: so is query-replace.
    pub(super) fn replace_edit_quit(&mut self, client: ClientId) {
        if self.client(client).query_replace.is_some() {
            self.replace_abandon(client);
        }
    }

    fn replace_push_stack(&mut self, client: ClientId) {
        self.replace_push_entry(client);
        let session = self.replace_mut(client);
        session.search_string_replaced.clear();
        session.next_replacement_replaced.clear();
    }

    /// `replace--push-stack`.
    fn replace_push_entry(&mut self, client: ClientId) {
        let point = self.replace_point(client);
        let session = self.replace(client);
        let buffer = session.buffer;
        let current = session.current.clone().expect("a match is current");
        let groups = self.group_positions(client, &current);
        // A replaced match only needs its whole extent.
        let groups = if session.replaced {
            groups.into_iter().take(1).collect()
        } else {
            groups
        };
        let entry_groups = self.markers_for(buffer, &groups);
        let point =
            self.buffers[buffer].create_marker_without_undo(point, InsertionType::StayBefore);
        let session = self.replace(client);
        let entry = StackEntry {
            point,
            replaced: session.replaced,
            groups: entry_groups,
            search_string_replaced: session.search_string_replaced.clone(),
            next_replacement_replaced: session.next_replacement_replaced.clone(),
        };
        self.replace_mut(client).stack.push(entry);
    }

    /// `^`: back to the previous match.
    fn replace_backup(&mut self, client: ClientId) {
        let Some(entry) = self.replace_mut(client).stack.pop() else {
            self.ring_bell(client);
            self.set_echo(client, NO_PREVIOUS_MATCH.to_string());
            return;
        };
        let buffer = self.replace(client).buffer;
        let point = self.buffers[buffer].marker_pos(entry.point);
        self.buffers[buffer].delete_marker(entry.point);
        self.replace_goto(client, point);
        let session = self.replace_mut(client);
        session.replaced = entry.replaced;
        if let Some(old) = session.current.replace(entry.groups) {
            release_groups(&mut self.buffers[buffer], &old);
        }
    }

    /// `u` and `U`: put back the text of the last (or every) replacement
    /// on the stack.
    fn replace_undo(&mut self, client: ClientId, all: bool) -> Result<(), CommandError> {
        if self.replace(client).stack.is_empty() {
            self.ring_bell(client);
            self.set_echo(client, NOTHING_TO_UNDO.to_string());
            return Ok(());
        }
        let buffer = self.replace(client).buffer;
        let mut undone = 0;
        let mut index = 0;
        let mut length = self.replace(client).stack.len();
        loop {
            let session = self.replace(client);
            if index >= length
                || session.stack.is_empty()
                || session.replaced && !session.last_was_act_and_show
            {
                break;
            }
            // The stack is used newest first.
            let entry = session.stack[session.stack.len() - 1 - index].clone();
            index += 1;
            let (search, last_replacement) = if entry.replaced {
                (
                    entry.next_replacement_replaced.clone(),
                    entry.search_string_replaced.clone(),
                )
            } else {
                (
                    entry.search_string_replaced.clone(),
                    entry.next_replacement_replaced.clone(),
                )
            };
            {
                let session = self.replace_mut(client);
                session.replaced = entry.replaced;
                session.search_string_replaced = search.clone();
                session.next_replacement_replaced = last_replacement.clone();
                session.last_was_act_and_show = false;
            }
            if index == length && !entry.replaced && undone == 0 {
                self.ring_bell(client);
                self.set_echo(client, NOTHING_TO_UNDO.to_string());
            }
            if !entry.replaced {
                continue;
            }
            // Drop the entries looked at so far.
            let session = self.replace_mut(client);
            let keep = session.stack.len() - index;
            let dropped: Vec<StackEntry> = session.stack.drain(keep..).collect();
            for old in dropped.iter().filter(|old| old.point != entry.point) {
                self.buffers[buffer].delete_marker(old.point);
                release_groups(&mut self.buffers[buffer], &old.groups);
            }
            let point = self.buffers[buffer].marker_pos(entry.point);
            self.buffers[buffer].delete_marker(entry.point);
            self.replace_goto(client, point);
            let start = entry.groups.groups[0]
                .map(|(start, _)| self.buffers[buffer].marker_pos(start))
                .expect("a stacked match has group 0");
            release_groups(&mut self.buffers[buffer], &entry.groups);
            let restored = start..start + search.len() as u64;
            let markers = self.markers_for(buffer, &[Some(restored)]);
            if let Some(old) = self.replace_mut(client).current.replace(markers) {
                release_groups(&mut self.buffers[buffer], &old);
            }
            // Undoing keeps the case of the original text.
            self.replace_current(client, Some((Template::literal(&last_replacement), true)))?;
            self.replace_mut(client).replace_count -= 2;
            if self.replace(client).regexp {
                let session = self.replace_mut(client);
                session.template = Template::new(&entry.next_replacement_replaced, true);
            }
            if all {
                self.replace_mut(client).replaced = false;
                length -= index;
                index = 0;
                undone += 1;
            }
        }
        if all && undone > 0 {
            let plural = if undone == 1 { "" } else { "s" };
            self.ring_bell(client);
            self.set_echo(client, format!("Undid {undone} replacement{plural}"));
        }
        let session = self.replace_mut(client);
        session.replaced = false;
        session.last_was_undo = true;
        session.last_was_act_and_show = false;
        Ok(())
    }

    /// `?`: the help in `*Help*`, shown in another window.
    fn replace_help(&mut self, client: ClientId) {
        let session = self.replace(client);
        let mut help = String::from("Query replacing ");
        if session.backward {
            help.push_str("backward ");
        }
        if session.delimited {
            help.push_str("word ");
        }
        if session.regexp {
            help.push_str("regexp ");
        }
        help.push_str(&format!(
            "{} with {}.\n\n{QUERY_REPLACE_HELP}\n",
            String::from_utf8_lossy(&session.from),
            String::from_utf8_lossy(&session.template.source)
        ));
        self.show_text_buffer(client, HELP_BUFFER, help.into_bytes(), "Help");
    }

    /// The end of `perform-replace`: the count is reported unless a key is
    /// to run (`unread`), which then runs as usual.
    fn replace_finish(&mut self, client: ClientId, unread: Option<Key>) {
        let Some(session) = self.client_mut(client).query_replace.take() else {
            return;
        };
        let count = session.replace_count;
        let regexp = session.regexp;
        if let Some(buffer) = self.buffers.get_mut(session.buffer) {
            session.release(buffer);
        }
        if let Some(wait) = self.client(client).search_wait.as_ref()
            && matches!(wait.owner, SearchOwner::QueryReplace(_))
        {
            self.cancel_search(client);
        }
        match unread {
            Some(key) => {
                self.client_mut(client).command_loop.last_command = Some(MODE_EXITED);
                self.handle_key(client, key);
            }
            None => {
                let plural = if count == 1 { "" } else { "s" };
                self.set_echo(client, format!("Replaced {count} occurrence{plural}"));
                self.client_mut(client).command_loop.last_command = Some(if regexp {
                    "query-replace-regexp"
                } else {
                    QUERY_REPLACE_COMMAND
                });
            }
        }
    }

    /// Ends the query-replace without a report (it was quit or failed).
    fn replace_abandon(&mut self, client: ClientId) {
        if let Some(session) = self.client_mut(client).query_replace.take()
            && let Some(buffer) = self.buffers.get_mut(session.buffer)
        {
            session.release(buffer);
        }
        if let Some(wait) = self.client(client).search_wait.as_ref()
            && matches!(wait.owner, SearchOwner::QueryReplace(_))
        {
            self.cancel_search(client);
        }
    }

    /// A paste ends query-replace and is then inserted as usual.
    pub(super) fn query_replace_paste(&mut self, client: ClientId) {
        self.replace_abandon(client);
        self.client_mut(client).command_loop.last_command = Some(MODE_EXITED);
    }

    /// `C-g` while the next match was being searched for on a worker.
    pub(super) fn replace_search_quit(&mut self, client: ClientId) {
        self.replace_abandon(client);
        self.signal(client, Some(QUERY_REPLACE_COMMAND), &CommandError::Quit);
    }

    /// The query-replace's buffer was killed.
    pub(super) fn replace_forget_buffer(&mut self, buffer: BufferId) {
        let clients: Vec<ClientId> = self.clients.keys().copied().collect();
        for client in clients {
            if self
                .client(client)
                .query_replace
                .as_ref()
                .is_some_and(|session| session.buffer == buffer)
            {
                // The markers go with the buffer.
                self.client_mut(client).query_replace = None;
                if let Some(wait) = self.client(client).search_wait.as_ref()
                    && matches!(wait.owner, SearchOwner::QueryReplace(_))
                {
                    self.cancel_search(client);
                }
                self.client_mut(client).echo.clear();
            }
        }
    }

    /// Releases a detaching client's query-replace markers.
    pub(super) fn replace_detach(&mut self, session: QueryReplace) {
        if let Some(buffer) = self.buffers.get_mut(session.buffer) {
            session.release(buffer);
        }
    }

    /// What the query-replace highlights: the match asked about, and the
    /// other matches in the windows showing the buffer.
    pub(crate) fn replace_highlights(
        &self,
        client: ClientId,
    ) -> Option<super::searching::IsearchHighlights> {
        let session = self.client(client).query_replace.as_ref()?;
        if !session.asking {
            return None;
        }
        let current = session.current.as_ref().and_then(|current| {
            let buffer = &self.buffers[session.buffer];
            current.groups[0].map(|(start, end)| {
                (
                    session.window,
                    buffer.marker_pos(start)..buffer.marker_pos(end),
                )
            })
        });
        Some(super::searching::IsearchHighlights {
            buffer: session.buffer,
            current,
            lazy: Some(session.pattern.clone()),
        })
    }
}

const QUERY_REPLACE_COMMAND: &str = "query-replace";
const RECENTER_TOP_BOTTOM: &str = "recenter-top-bottom";

/// The message of Emacs's `invalid-regexp` error, which prints its data
/// as a Lisp string.
pub(super) fn invalid_regexp_message(error: &emx_text::search::PatternError) -> String {
    let mut printed = String::new();
    for character in error.to_string().chars() {
        if matches!(character, '"' | '\\') {
            printed.push('\\');
        }
        printed.push(character);
    }
    format!("Invalid regexp: \"{printed}\"")
}

fn search_error(error: emx_text::search::SearchError) -> CommandError {
    CommandError::User(error.to_string())
}

/// Emacs `add-to-history` with KEEP-ALL: newest first, at most
/// `history-length` entries.
fn add_to_history<T>(history: &mut Vec<T>, entry: T) {
    history.insert(0, entry);
    history.truncate(HISTORY_LENGTH);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_follow_replace_match() {
        assert_eq!(
            parse_template(br"a\&b\1\\\#"),
            Ok(vec![
                Piece::Text(b"a".to_vec()),
                Piece::Group(0),
                Piece::Text(b"b".to_vec()),
                Piece::Group(1),
                Piece::Text(b"\\".to_vec()),
                Piece::Count,
            ])
        );
        assert_eq!(parse_template(br"\0"), Err(INVALID_REPLACEMENT.to_string()));
        assert_eq!(parse_template(b"x\\"), Err(INVALID_REPLACEMENT.to_string()));
    }

    #[test]
    fn case_conversion_matches_emacs() {
        // Checked with replace-match in Emacs 29.3 batch.
        for (matched, replacement, expected) in [
            ("hello", "bye x", "bye x"),
            ("HELLO", "bye x", "BYE X"),
            ("Hello", "bye x", "Bye X"),
            ("hELLO", "bye x", "bye x"),
            ("A", "xy", "XY"),
            ("a", "xy", "xy"),
        ] {
            let action = case_action(matched.as_bytes());
            assert_eq!(
                String::from_utf8(change_case(replacement.as_bytes(), action)).expect("utf-8"),
                expected,
                "{matched}"
            );
        }
    }

    #[test]
    fn word_search_regexp_matches_emacs() {
        assert_eq!(word_search_regexp("foo bar"), r"\<foo\W+bar\>");
        assert_eq!(word_search_regexp(" foo."), r"\W+foo\W+");
        assert_eq!(word_search_regexp("--"), r"\W+");
    }
}
