//! Incremental search state (Emacs isearch.el): one search per client, the
//! stack of states `DEL` returns to, and the echo-area message. The editor
//! drives it (`editor/searching.rs`); positions are markers in the searched
//! buffer, so another client's edits move them like any other position.

use std::collections::VecDeque;

use emx_text::search::CompiledPattern;
use emx_types::Key;

use crate::buffer::{Buffer, BufferId};
use crate::marker::MarkerId;
use crate::window::WindowId;

/// Emacs `search-whitespace-regexp`: a space in a plain isearch matches a
/// run of spaces and tabs (`isearch-lax-whitespace` is on by default).
pub(crate) const SEARCH_WHITESPACE_REGEXP: &str = "[ \t]+";
/// Emacs `search-ring-max` and `regexp-search-ring-max`.
pub(crate) const SEARCH_RING_MAX: usize = 16;
pub(crate) const SEARCHING: &str = "searching...";
/// Shown when a match reached `search_match_cap` and was cut short.
pub(crate) const MATCH_TRUNCATED: &str = "Match truncated at search_match_cap";
const CONTROL_CHARACTER_OFFSET: u8 = 64;
const DELETE: u8 = 0x7F;
const SPACE: u8 = b' ';

/// Emacs `isearch-case-fold-search`: `t` folds unless the string has an
/// upper-case letter (`search-upper-case`), `yes` (after `M-c`) always
/// folds, `nil` never does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaseFold {
    Smart,
    Always,
    Never,
}

impl CaseFold {
    pub(crate) fn folds(self) -> bool {
        self != CaseFold::Never
    }
}

/// One entry of `isearch-cmds`, what `DEL` goes back to.
#[derive(Debug, Clone)]
pub(crate) struct Frame {
    pub string: Vec<u8>,
    pub point: MarkerId,
    pub success: bool,
    pub forward: bool,
    pub other_end: Option<MarkerId>,
    pub error: Option<String>,
    pub wrapped: bool,
    pub barrier: MarkerId,
    pub case_fold: CaseFold,
}

impl Frame {
    pub(crate) fn release(&self, buffer: &mut Buffer) {
        buffer.delete_marker(self.point);
        buffer.delete_marker(self.barrier);
        if let Some(other_end) = self.other_end {
            buffer.delete_marker(other_end);
        }
    }
}

/// What a search started by an isearch command does once it is done; a
/// search on a worker finishes in a later event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AfterSearch {
    /// `isearch-search-and-update`, `isearch-del-char`: push the state and
    /// update the display.
    PushAndUpdate,
    /// `isearch-repeat` with `remaining` more searches after this one. The
    /// states pushed above `depth` meanwhile are dropped at the end, and
    /// `then_direction` repeats once more that way afterwards.
    Repeat {
        remaining: u64,
        depth: usize,
        then_direction: Option<bool>,
    },
    /// Resuming after the search string was edited in the minibuffer;
    /// a nonincremental search then ends.
    Edited { nonincremental: bool },
}

/// Input that arrived while a search was running on a worker; it is
/// handled once the search is done, as if the search had been instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QueuedInput {
    Key(Key),
    Paste(Vec<u8>),
}

/// An isearch whose string is being edited in the minibuffer
/// (`isearch-edit-string`): where it was, to resume from there.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Suspended {
    pub old_point: u64,
    pub old_other_end: Option<u64>,
    pub nonincremental: bool,
}

/// The isearch of one client.
#[derive(Debug)]
pub(crate) struct Isearch {
    pub buffer: BufferId,
    pub window: WindowId,
    pub forward: bool,
    pub regexp: bool,
    pub lax_whitespace: bool,
    pub string: Vec<u8>,
    pub success: bool,
    pub wrapped: bool,
    pub other_end: Option<MarkerId>,
    pub barrier: MarkerId,
    pub opoint: MarkerId,
    pub case_fold: CaseFold,
    /// `isearch-last-case-fold-search`: the folding the previous search
    /// ended with, which `C-s C-s` restores with its string.
    pub last_case_fold: CaseFold,
    pub error: Option<String>,
    /// `isearch-adjusted`: the regexp was relaxed (`*`, `?`, `\|`), so
    /// the search restarts from an earlier match.
    pub adjusted: bool,
    pub yank_flag: bool,
    pub just_started: bool,
    pub nonincremental: bool,
    /// Where `C-w` continues from after a failing yank.
    pub yank_previous_point: Option<u64>,
    /// `search-ring-yank-pointer` / `regexp-search-ring-yank-pointer`.
    pub ring_pointer: Option<usize>,
    /// `C-q` was typed: the next key is a character to search for.
    pub quote_next: bool,
    /// The last match was cut at `search_match_cap`.
    pub capped: bool,
    /// The pattern of the current string, for lazy highlighting; `None`
    /// while the string does not compile.
    pub pattern: Option<CompiledPattern>,
    pub cmds: Vec<Frame>,
    /// Shown after the message until the next key, as
    /// `isearch--momentary-message` does.
    pub momentary: Option<String>,
    pub suspended: Option<Suspended>,
}

impl Isearch {
    /// Every marker the search holds, for releasing them when it ends.
    pub(crate) fn release(self, buffer: &mut Buffer) {
        for frame in &self.cmds {
            frame.release(buffer);
        }
        buffer.delete_marker(self.barrier);
        buffer.delete_marker(self.opoint);
        if let Some(other_end) = self.other_end {
            buffer.delete_marker(other_end);
        }
    }
}

/// Emacs `isearch-no-upper-case-p`: no upper-case letter in `string`; in a
/// regexp, a letter after a backslash does not count, and `[:upper:]` or
/// `[:lower:]` counts as caring about case.
pub(crate) fn no_upper_case(string: &str, regexp: bool) -> bool {
    let mut quoted = false;
    for character in string.chars() {
        if regexp && character == '\\' {
            quoted = !quoted;
            continue;
        }
        if !quoted && character.to_lowercase().ne(std::iter::once(character)) {
            return false;
        }
        quoted = false;
    }
    !(regexp && (string.contains("[:upper:]") || string.contains("[:lower:]")))
}

/// The search string as the echo area shows it (`isearch-message`):
/// control characters as `^X`, bytes that are not text as octal escapes.
pub(crate) fn describe(string: &[u8]) -> String {
    let mut shown = String::new();
    for chunk in string.utf8_chunks() {
        for character in chunk.valid().chars() {
            match u8::try_from(u32::from(character)) {
                Ok(byte) if byte < SPACE => {
                    shown.push('^');
                    shown.push(char::from(byte + CONTROL_CHARACTER_OFFSET));
                }
                Ok(DELETE) => shown.push_str("^?"),
                _ => shown.push(character),
            }
        }
        for byte in chunk.invalid() {
            shown.push_str(&format!("\\{byte:o}"));
        }
    }
    shown
}

/// Emacs `downcase` on text that need not be UTF-8.
pub(crate) fn downcase(bytes: &[u8]) -> Vec<u8> {
    let mut lowered = Vec::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        for character in chunk.valid().chars() {
            let mut encoded = [0; 4];
            for lower in character.to_lowercase() {
                lowered.extend_from_slice(lower.encode_utf8(&mut encoded).as_bytes());
            }
        }
        lowered.extend_from_slice(chunk.invalid());
    }
    lowered
}

/// The isearch message prefix (`isearch-message-prefix`), capitalized.
pub(crate) struct PrefixParts {
    pub success: bool,
    pub adjusted: bool,
    pub overwrapped: bool,
    pub wrapped: bool,
    pub case_sensitive_failure: bool,
    pub regexp: bool,
    pub nonincremental: bool,
    pub forward: bool,
}

pub(crate) fn message_prefix(parts: &PrefixParts) -> String {
    let mut prefix = String::new();
    if !parts.success {
        prefix.push_str("failing ");
    }
    if parts.adjusted {
        prefix.push_str("pending ");
    }
    if parts.overwrapped {
        prefix.push_str("over");
    }
    if parts.wrapped {
        prefix.push_str("wrapped ");
    }
    if parts.case_sensitive_failure {
        prefix.push_str("case-sensitive ");
    }
    if parts.regexp {
        prefix.push_str("regexp ");
    }
    prefix.push_str(if parts.nonincremental {
        "search"
    } else {
        "I-search"
    });
    if !parts.forward {
        prefix.push_str(" backward");
    }
    prefix.push_str(": ");
    let mut characters = prefix.chars();
    let first = characters.next().expect("the prefix is not empty");
    first.to_uppercase().chain(characters).collect()
}

/// Emacs `add-to-history` with `history-delete-duplicates`: newest first,
/// no duplicates, at most `SEARCH_RING_MAX` entries.
pub(crate) fn add_to_ring(ring: &mut VecDeque<Vec<u8>>, entry: &[u8]) {
    ring.retain(|existing| existing != entry);
    ring.push_front(entry.to_vec());
    ring.truncate(SEARCH_RING_MAX);
}

/// Emacs `isearch-fallback`'s `isearch-backslash`: `string` ends in an odd
/// number of backslashes.
pub(crate) fn ends_in_backslash(string: &[u8]) -> bool {
    string
        .iter()
        .rev()
        .take_while(|byte| **byte == b'\\')
        .count()
        % 2
        == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefix(success: bool, wrapped: bool, overwrapped: bool, forward: bool) -> String {
        message_prefix(&PrefixParts {
            success,
            adjusted: false,
            overwrapped,
            wrapped,
            case_sensitive_failure: false,
            regexp: false,
            nonincremental: false,
            forward,
        })
    }

    #[test]
    fn prefixes_follow_isearch_message_prefix() {
        assert_eq!(prefix(true, false, false, true), "I-search: ");
        assert_eq!(prefix(false, false, false, true), "Failing I-search: ");
        assert_eq!(prefix(true, true, false, true), "Wrapped I-search: ");
        assert_eq!(prefix(true, true, true, true), "Overwrapped I-search: ");
        assert_eq!(
            prefix(false, true, false, false),
            "Failing wrapped I-search backward: "
        );
        let regexp = message_prefix(&PrefixParts {
            success: false,
            adjusted: false,
            overwrapped: false,
            wrapped: false,
            case_sensitive_failure: true,
            regexp: true,
            nonincremental: true,
            forward: true,
        });
        assert_eq!(regexp, "Failing case-sensitive regexp search: ");
    }

    #[test]
    fn upper_case_detection_matches_isearch_no_upper_case_p() {
        assert!(no_upper_case("foo", false));
        assert!(!no_upper_case("Foo", false));
        assert!(no_upper_case(r"\W", true));
        assert!(!no_upper_case(r"\W", false));
        assert!(!no_upper_case(r"\\W", true));
        assert!(!no_upper_case("[[:upper:]]", true));
        assert!(no_upper_case("ß", false));
    }

    #[test]
    fn descriptions_show_control_characters() {
        assert_eq!(describe(b"a\nb\x7f\xff"), "a^Jb^?\\377");
    }

    #[test]
    fn rings_drop_duplicates() {
        let mut ring = VecDeque::new();
        for entry in [b"a", b"b", b"a"] {
            add_to_ring(&mut ring, entry);
        }
        assert_eq!(ring, VecDeque::from([b"a".to_vec(), b"b".to_vec()]));
    }
}
