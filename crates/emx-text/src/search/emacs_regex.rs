//! Emacs regexp syntax translated to `regex-syntax` syntax (DESIGN section
//! 5). Emacs regexps are backtracking with leftmost-first priority, which is
//! also the `regex` crates' match semantics, so a faithful translation
//! finds the same matches. Constructs with no equivalent (back references,
//! `\=`, symbol boundaries, categories, explicitly numbered groups) are
//! rejected with an error naming the construct.
//!
//! What the translation changes, and why:
//! - Emacs's special characters are context dependent: `^` is an anchor
//!   only at the start of the pattern or of a group or alternative, `$`
//!   only at the end of one, and `*`, `+`, `?` with nothing to repeat are
//!   literal. Everything else that is special to `regex-syntax` is escaped.
//! - `.` and the negation of an ASCII-only bracket expression also match a
//!   byte that is not part of valid UTF-8, as Emacs's raw-byte characters
//!   match them.
//! - Syntax classes (`\w`, `\sC`, `[:space:]`, `[:word:]`) come from
//!   Emacs's standard syntax table, not from Unicode's notion of a word.
//! - Bracket expressions treat `\` as an ordinary character, as Emacs does.

use std::fmt::Write;
use std::sync::OnceLock;

use crate::word_syntax::{SYNTAX, WORD};

/// Matches one character other than newline, or one byte that is not part
/// of valid UTF-8 (Emacs's raw-byte characters).
const ANY_EXCEPT_NEWLINE: &str = r"(?:[^\n]|(?-u:[\x80-\xFF]))";
const RAW_BYTE: &str = r"(?-u:[\x80-\xFF])";
/// A class that never matches, for an Emacs bracket expression whose ranges
/// are all empty (`[z-a]`); `regex-syntax` rejects an empty `[]`.
const NOTHING: &str = r"[^\x{0}-\x{10FFFF}]";
const LARGEST_ASCII: u32 = 0x7F;
const LARGEST_CHAR: u32 = 0x10FFFF;
/// Emacs `RE_DUP_MAX`: the largest count an interval may give.
const DUP_MAX: u32 = 0xFFFF;
/// Emacs syntax designators for `\sC`, with `-` an alias of whitespace.
const SYNTAX_DESIGNATORS: &str = " .w_()'\"$\\/<>@!|";
const WHITESPACE_DESIGNATOR_ALIAS: char = '-';

/// Emacs's `invalid-regexp` messages (regex-emacs.c `re_error_msgid`).
pub const UNMATCHED_BRACKET: &str = "Unmatched [ or [^";
pub const UNMATCHED_OPEN_GROUP: &str = "Unmatched ( or \\(";
pub const UNMATCHED_CLOSE_GROUP: &str = "Unmatched ) or \\)";
pub const UNMATCHED_INTERVAL: &str = "Unmatched \\{";
pub const INVALID_INTERVAL: &str = "Invalid content of \\{\\}";
pub const TRAILING_BACKSLASH: &str = "Trailing backslash";
pub const INVALID_CLASS_NAME: &str = "Invalid character class name";
pub const INVALID_REGEXP: &str = "Invalid regular expression";
pub const PREMATURE_END: &str = "Premature end of regular expression";
pub const TOO_BIG: &str = "Regular expression too big";

/// Why a pattern cannot be searched for.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PatternError {
    /// A valid Emacs construct with no translation; `position` is its byte
    /// offset in the pattern.
    #[error("Unsupported regexp construct `{construct}` at position {position}")]
    Unsupported { construct: String, position: usize },
    /// An invalid Emacs regexp, with the message Emacs gives.
    #[error("{message}")]
    Invalid {
        message: &'static str,
        position: usize,
    },
    /// The translation could not be compiled (for example, it is too big).
    #[error("{0}")]
    Compile(String),
}

impl PatternError {
    /// The pattern is not wrong, only unfinished (isearch says `incomplete
    /// input` for these, as Emacs does for "Premature ..." and "Unmatched
    /// ..." errors).
    pub fn is_incomplete(&self) -> bool {
        matches!(
            self,
            PatternError::Invalid {
                message: UNMATCHED_BRACKET
                    | UNMATCHED_OPEN_GROUP
                    | UNMATCHED_CLOSE_GROUP
                    | UNMATCHED_INTERVAL
                    | PREMATURE_END,
                ..
            }
        )
    }
}

/// Emacs `regexp-quote`: a regexp matching exactly `text`.
pub fn regexp_quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(character, '[' | '*' | '.' | '\\' | '?' | '+' | '^' | '$') {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted
}

/// A `regex-syntax` pattern matching exactly `bytes`, which need not be
/// UTF-8.
pub(crate) fn literal_to_rust(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.utf8_chunks() {
        for character in chunk.valid().chars() {
            push_literal(&mut out, character);
        }
        for byte in chunk.invalid() {
            write!(out, r"(?-u:\x{byte:02X})").expect("writing to a String");
        }
    }
    out
}

fn push_literal(out: &mut String, character: char) {
    if regex_syntax::is_meta_character(character) {
        out.push('\\');
    }
    out.push(character);
}

/// Translates an Emacs regexp to `regex-syntax` syntax. Group numbers are
/// kept: `\(` groups become capturing groups in the same order, and every
/// group the translation adds is non-capturing.
pub fn emacs_regex_to_rust(src: &str) -> Result<String, PatternError> {
    Translator::new(src).run()
}

/// One open group: where it is in the pattern, where its output starts,
/// and where its contents start.
struct OpenGroup {
    position: usize,
    output_start: usize,
    contents_start: usize,
}

struct Translator {
    chars: Vec<(usize, char)>,
    index: usize,
    out: String,
    groups: Vec<OpenGroup>,
    /// Where the last repeatable item starts in `out`, if there is one in
    /// the current alternative (Emacs's `laststart`).
    last_start: Option<usize>,
    /// The last item already has a repetition operator, so another one must
    /// wrap it (Emacs allows `a**`; `regex-syntax` does not).
    last_repeated: bool,
    /// The previous token was a greedy `*`, `+` or `?`, so a `?` now makes
    /// it lazy.
    after_greedy_operator: bool,
}

impl Translator {
    fn new(src: &str) -> Translator {
        Translator {
            chars: src.char_indices().collect(),
            index: 0,
            out: String::with_capacity(src.len() * 2),
            groups: Vec::new(),
            last_start: None,
            last_repeated: false,
            after_greedy_operator: false,
        }
    }

    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars
            .get(self.index + ahead)
            .map(|(_, character)| *character)
    }

    fn invalid(&self, message: &'static str, position: usize) -> PatternError {
        PatternError::Invalid { message, position }
    }

    fn unsupported(&self, construct: &str, position: usize) -> PatternError {
        PatternError::Unsupported {
            construct: construct.to_string(),
            position,
        }
    }

    /// Emacs `at_begline_loc_p` (or the very start): `^` here is an anchor.
    fn at_alternative_start(&self) -> bool {
        self.last_start.is_none() && self.out_ends_alternative_start()
    }

    /// The output so far ends where an alternative begins: at the start, or
    /// just after a group opened or a `|`.
    fn out_ends_alternative_start(&self) -> bool {
        self.out.is_empty()
            || self.out.ends_with('|')
            || self
                .groups
                .last()
                .is_some_and(|group| group.contents_start == self.out.len())
    }

    /// Emacs `at_endline_loc_p` (or the very end): `$` here is an anchor.
    fn at_alternative_end(&self) -> bool {
        matches!(
            (self.peek(1), self.peek(2)),
            (None, _) | (Some('\\'), Some(')' | '|'))
        )
    }

    /// Starts a new repeatable item at the current end of the output.
    fn begin_item(&mut self) -> usize {
        let start = self.out.len();
        self.last_start = Some(start);
        self.last_repeated = false;
        self.after_greedy_operator = false;
        start
    }

    /// An item that cannot be repeated (an anchor or assertion) or the
    /// start of an alternative.
    fn reset_item(&mut self) {
        self.last_start = None;
        self.last_repeated = false;
        self.after_greedy_operator = false;
    }

    fn run(mut self) -> Result<String, PatternError> {
        while self.index < self.chars.len() {
            let (position, character) = self.chars[self.index];
            match character {
                '\\' => self.backslash(position)?,
                '[' => self.bracket(position)?,
                '.' => {
                    self.begin_item();
                    self.out.push_str(ANY_EXCEPT_NEWLINE);
                    self.index += 1;
                }
                '^' if self.at_alternative_start() => {
                    self.out.push_str("(?m:^)");
                    // `*` right after `^` is literal, as at the start.
                    self.reset_item();
                    self.index += 1;
                }
                '$' if self.at_alternative_end() => {
                    self.out.push_str("(?m:$)");
                    self.reset_item();
                    self.index += 1;
                }
                '*' | '+' | '?' => self.repetition(character)?,
                other => {
                    self.begin_item();
                    push_literal(&mut self.out, other);
                    self.index += 1;
                }
            }
        }
        if let Some(group) = self.groups.last() {
            return Err(self.invalid(UNMATCHED_OPEN_GROUP, group.position));
        }
        Ok(self.out)
    }

    /// `*`, `+` or `?` after an item; literal when there is nothing to
    /// repeat. `?` right after a greedy operator makes it lazy.
    fn repetition(&mut self, operator: char) -> Result<(), PatternError> {
        self.index += 1;
        if operator == '?' && self.after_greedy_operator {
            self.out.push('?');
            self.after_greedy_operator = false;
            return Ok(());
        }
        let Some(start) = self.last_start else {
            self.begin_item();
            push_literal(&mut self.out, operator);
            return Ok(());
        };
        self.wrap_if_repeated(start);
        self.out.push(operator);
        self.last_repeated = true;
        self.after_greedy_operator = true;
        Ok(())
    }

    /// Emacs repeats an already repeated item again (`a**`, `a\{2\}*`);
    /// `regex-syntax` needs a group around it first.
    fn wrap_if_repeated(&mut self, start: usize) {
        if self.last_repeated {
            self.out.insert_str(start, "(?:");
            self.out.push(')');
        }
    }

    fn backslash(&mut self, position: usize) -> Result<(), PatternError> {
        let Some(next) = self.peek(1) else {
            return Err(self.invalid(TRAILING_BACKSLASH, position));
        };
        self.index += 2;
        match next {
            '(' => self.open_group(position),
            ')' => self.close_group(position),
            '|' => {
                self.out.push('|');
                self.reset_item();
                Ok(())
            }
            '{' => self.interval(position),
            '`' => {
                self.out.push_str(r"\A");
                self.reset_item();
                Ok(())
            }
            '\'' => {
                self.out.push_str(r"\z");
                self.reset_item();
                Ok(())
            }
            // These map to the regex engine's Unicode word boundaries, which
            // classify `_` as a word character. Emacs instead decides word and
            // symbol boundaries from the buffer's syntax table, where `_` is a
            // symbol constituent, not a word one, so a boundary next to `_` can
            // disagree with Emacs. Syntax-table-aware boundaries are deferred.
            'b' => self.assertion(r"\b"),
            'B' => self.assertion(r"\B"),
            '<' => self.assertion(r"\b{start}"),
            '>' => self.assertion(r"\b{end}"),
            '_' => {
                let construct = match self.peek(0) {
                    Some(kind) => format!("\\_{kind}"),
                    None => return Err(self.invalid(PREMATURE_END, position)),
                };
                Err(self.unsupported(&construct, position))
            }
            'w' => self.class_item(&format!("[{}]", word_class())),
            'W' => self.class_item(&format!("[^{}]", word_class())),
            's' | 'S' => {
                let Some(designator) = self.peek(0) else {
                    return Err(self.invalid(PREMATURE_END, position));
                };
                self.index += 1;
                let body = syntax_class(designator)
                    .ok_or_else(|| self.invalid(INVALID_REGEXP, position))?;
                let class = match (next, body.is_empty()) {
                    ('s', true) => NOTHING.to_string(),
                    ('s', false) => format!("[{body}]"),
                    (_, true) => format!("(?:[\\x{{0}}-\\x{{{LARGEST_CHAR:X}}}]|{RAW_BYTE})"),
                    (_, false) => format!("[^{body}]"),
                };
                self.class_item(&class)
            }
            'c' | 'C' => {
                let construct = match self.peek(0) {
                    Some(category) => format!("\\{next}{category}"),
                    None => return Err(self.invalid(PREMATURE_END, position)),
                };
                Err(self.unsupported(&construct, position))
            }
            '=' => Err(self.unsupported("\\=", position)),
            '1'..='9' => Err(self.unsupported(&format!("\\{next}"), position)),
            other => {
                self.begin_item();
                push_literal(&mut self.out, other);
                Ok(())
            }
        }
    }

    fn assertion(&mut self, syntax: &str) -> Result<(), PatternError> {
        self.out.push_str(syntax);
        self.reset_item();
        Ok(())
    }

    fn class_item(&mut self, class: &str) -> Result<(), PatternError> {
        self.begin_item();
        self.out.push_str(class);
        Ok(())
    }

    /// `\(`, `\(?:` (shy) and `\(?N:` (explicitly numbered, which
    /// `regex-syntax` cannot express).
    fn open_group(&mut self, position: usize) -> Result<(), PatternError> {
        let mut capturing = true;
        if self.peek(0) == Some('?') {
            match self.peek(1) {
                Some(':') => {
                    capturing = false;
                    self.index += 2;
                }
                Some(digit) if digit.is_ascii_digit() => {
                    let mut end = self.index + 1;
                    while self
                        .chars
                        .get(end)
                        .is_some_and(|(_, character)| character.is_ascii_digit())
                    {
                        end += 1;
                    }
                    let number: String = self.chars[self.index + 1..end]
                        .iter()
                        .map(|(_, character)| *character)
                        .collect();
                    return Err(self.unsupported(&format!("\\(?{number}:"), position));
                }
                Some(_) => return Err(self.invalid(INVALID_REGEXP, position)),
                None => return Err(self.invalid(PREMATURE_END, position)),
            }
        }
        let output_start = self.out.len();
        self.out.push_str(if capturing { "(" } else { "(?:" });
        self.groups.push(OpenGroup {
            position,
            output_start,
            contents_start: self.out.len(),
        });
        self.reset_item();
        Ok(())
    }

    fn close_group(&mut self, position: usize) -> Result<(), PatternError> {
        let Some(group) = self.groups.pop() else {
            return Err(self.invalid(UNMATCHED_CLOSE_GROUP, position));
        };
        self.out.push(')');
        // The group as a whole is the item a later operator repeats.
        self.last_start = Some(group.output_start);
        self.last_repeated = false;
        self.after_greedy_operator = false;
        Ok(())
    }

    /// `\{N\}`, `\{N,\}`, `\{,M\}`, `\{N,M\}`; with nothing to repeat Emacs
    /// reads `\{` as a literal `{`.
    fn interval(&mut self, position: usize) -> Result<(), PatternError> {
        let Some(start) = self.last_start else {
            self.begin_item();
            self.out.push_str(r"\{");
            return Ok(());
        };
        let mut minimum = String::new();
        let mut maximum = String::new();
        let mut comma = false;
        loop {
            match self.peek(0) {
                None => return Err(self.invalid(UNMATCHED_INTERVAL, position)),
                Some('\\') => {
                    match self.peek(1) {
                        Some('}') => {}
                        None => return Err(self.invalid(UNMATCHED_INTERVAL, position)),
                        Some(_) => return Err(self.invalid(INVALID_INTERVAL, position)),
                    }
                    self.index += 2;
                    break;
                }
                Some(',') if !comma => comma = true,
                Some(digit) if digit.is_ascii_digit() => {
                    if comma {
                        maximum.push(digit);
                    } else {
                        minimum.push(digit);
                    }
                }
                Some(_) => return Err(self.invalid(INVALID_INTERVAL, position)),
            }
            self.index += 1;
        }
        let parse = |digits: &str| -> Result<Option<u32>, PatternError> {
            if digits.is_empty() {
                return Ok(None);
            }
            match digits.parse::<u32>() {
                Ok(value) if value <= DUP_MAX => Ok(Some(value)),
                _ => Err(PatternError::Invalid {
                    message: TOO_BIG,
                    position,
                }),
            }
        };
        let low = parse(&minimum)?.unwrap_or(0);
        let high = parse(&maximum)?;
        let operator = match (comma, high) {
            (false, _) => format!("{{{low}}}"),
            (true, None) => format!("{{{low},}}"),
            (true, Some(high)) if high < low => {
                return Err(self.invalid(INVALID_INTERVAL, position));
            }
            (true, Some(high)) => format!("{{{low},{high}}}"),
        };
        self.wrap_if_repeated(start);
        self.out.push_str(&operator);
        self.last_repeated = true;
        self.after_greedy_operator = false;
        Ok(())
    }

    /// An Emacs bracket expression: `]` first is literal, `-` first or last
    /// is literal, `\` is an ordinary character, and `[:class:]` names a
    /// character class.
    fn bracket(&mut self, position: usize) -> Result<(), PatternError> {
        self.index += 1;
        let negated = self.peek(0) == Some('^');
        if negated {
            self.index += 1;
        }
        let mut items = String::new();
        let mut ascii_only = true;
        let mut first = true;
        loop {
            let Some(character) = self.peek(0) else {
                return Err(self.invalid(UNMATCHED_BRACKET, position));
            };
            if character == ']' && !first {
                self.index += 1;
                break;
            }
            first = false;
            if character == '['
                && self.peek(1) == Some(':')
                && let Some((name, length)) = self.class_name()
            {
                let (body, ascii) =
                    posix_class(&name).ok_or_else(|| self.invalid(INVALID_CLASS_NAME, position))?;
                ascii_only &= ascii;
                items.push_str(&body);
                self.index += length;
                continue;
            }
            let is_range = self.peek(1) == Some('-') && self.peek(2).is_some_and(|end| end != ']');
            if is_range {
                let end = self.peek(2).expect("checked above");
                self.index += 3;
                if u32::from(end) > LARGEST_ASCII {
                    ascii_only = false;
                }
                if character <= end {
                    write!(
                        items,
                        r"\x{{{:X}}}-\x{{{:X}}}",
                        u32::from(character),
                        u32::from(end)
                    )
                    .expect("writing to a String");
                }
                continue;
            }
            if u32::from(character) > LARGEST_ASCII {
                ascii_only = false;
            }
            write!(items, r"\x{{{:X}}}", u32::from(character)).expect("writing to a String");
            self.index += 1;
        }
        let class = match (negated, items.is_empty()) {
            (false, true) => NOTHING.to_string(),
            (false, false) => format!("[{items}]"),
            (true, true) => format!("(?:[\\x{{0}}-\\x{{{LARGEST_CHAR:X}}}]|{RAW_BYTE})"),
            // A negated class of ASCII characters only also matches raw
            // bytes; otherwise the raw-byte branch could take the first byte
            // of an excluded multibyte character.
            (true, false) if ascii_only => format!("(?:[^{items}]|{RAW_BYTE})"),
            (true, false) => format!("[^{items}]"),
        };
        self.class_item(&class)
    }

    /// At `[:`, the class name and how many characters `[:name:]` takes, if
    /// the class is closed.
    fn class_name(&self) -> Option<(String, usize)> {
        let mut name = String::new();
        let mut offset = 2;
        loop {
            match self.peek(offset)? {
                ':' if self.peek(offset + 1) == Some(']') => {
                    return Some((name, offset + 2));
                }
                ']' => return None,
                character => name.push(character),
            }
            offset += 1;
        }
    }
}

/// The ranges of `table` whose value is `class`, as a class body.
fn class_body(ranges: impl Iterator<Item = (u32, u32)>) -> String {
    let mut body = String::new();
    for (start, end) in ranges {
        if start == end {
            write!(body, r"\x{{{start:X}}}").expect("writing to a String");
        } else {
            write!(body, r"\x{{{start:X}}}-\x{{{end:X}}}").expect("writing to a String");
        }
    }
    body
}

/// Word constituents of Emacs's standard syntax table, as a class body.
fn word_class() -> &'static str {
    static BODY: OnceLock<String> = OnceLock::new();
    BODY.get_or_init(|| class_body(WORD.iter().copied()))
}

/// The class body of syntax designator `designator` (`\sC`); `None` for a
/// character that designates no syntax class. A class with no characters
/// gives an empty body.
fn syntax_class(designator: char) -> Option<String> {
    let designator = if designator == WHITESPACE_DESIGNATOR_ALIAS {
        ' '
    } else {
        designator
    };
    if !SYNTAX_DESIGNATORS.contains(designator) {
        return None;
    }
    let code = u16::try_from(u32::from(designator)).expect("designators are ASCII");
    Some(class_body(
        SYNTAX
            .iter()
            .filter(|(_, _, class)| *class == code)
            .map(|(start, end, _)| (*start, *end)),
    ))
}

/// The complement of `ranges` (sorted, disjoint) within `low..=high`.
fn complement_within(ranges: &[(u32, u32)], low: u32, high: u32) -> Vec<(u32, u32)> {
    let mut gaps = Vec::new();
    let mut next = low;
    for (start, end) in ranges {
        if *end < low || *start > high {
            continue;
        }
        if *start > next {
            gaps.push((next, start - 1));
        }
        next = next.max(end + 1);
    }
    if next <= high {
        gaps.push((next, high));
    }
    gaps
}

/// The body of a POSIX class inside a bracket expression, as Emacs defines
/// it for multibyte text, and whether it holds only ASCII characters.
fn posix_class(name: &str) -> Option<(String, bool)> {
    let non_ascii_start = LARGEST_ASCII + 1;
    let class = match name {
        "alpha" => (r"\p{L}\p{M}\p{Nl}".to_string(), false),
        "alnum" => (r"\p{L}\p{M}\p{Nl}\p{Nd}".to_string(), false),
        "digit" => (r"0-9".to_string(), true),
        "xdigit" => (r"0-9A-Fa-f".to_string(), true),
        "upper" => (r"\p{Lu}".to_string(), false),
        "lower" => (r"\p{Ll}".to_string(), false),
        "blank" => (r"\t\p{Zs}".to_string(), false),
        "cntrl" => (r"\x00-\x1F".to_string(), true),
        "ascii" | "unibyte" => (r"\x00-\x7F".to_string(), true),
        "nonascii" | "multibyte" => (format!(r"\x{{80}}-\x{{{LARGEST_CHAR:X}}}"), false),
        "space" => (
            syntax_class(' ').expect("whitespace is a syntax class"),
            false,
        ),
        "word" => (word_class().to_string(), false),
        // ASCII punctuation, and any multibyte character that is not a
        // word constituent.
        "punct" => {
            let non_word = complement_within(WORD, non_ascii_start, LARGEST_CHAR);
            (
                format!(
                    r"\x21-\x2F\x3A-\x40\x5B-\x60\x7B-\x7E{}",
                    class_body(non_word.into_iter())
                ),
                false,
            )
        }
        "graph" => (
            format!(r"\x21-\x7E[\x{{80}}-\x{{{LARGEST_CHAR:X}}}&&[^\p{{Z}}\p{{C}}]]"),
            false,
        ),
        "print" => (
            format!(r"\x20-\x7E[\x{{80}}-\x{{{LARGEST_CHAR:X}}}&&[^\p{{C}}]]"),
            false,
        ),
        _ => return None,
    };
    Some(class)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complement_fills_the_gaps() {
        assert_eq!(
            complement_within(&[(2, 3), (6, 6)], 0, 9),
            vec![(0, 1), (4, 5), (7, 9)]
        );
        assert_eq!(complement_within(&[(0, 9)], 0, 9), vec![]);
    }

    #[test]
    fn whitespace_syntax_includes_tab_newline_and_space() {
        let body = syntax_class('-').expect("whitespace");
        for character in [' ', '\t', '\n', '\r', '\x0C'] {
            let probe = format!("[{body}]");
            let regex = regex_automata::meta::Regex::new(&probe).expect("valid class");
            assert!(
                regex.is_match(character.to_string().as_bytes()),
                "{character:?}"
            );
        }
        assert_eq!(syntax_class('w').as_deref(), Some(word_class()));
        assert!(syntax_class('x').is_none());
    }
}
