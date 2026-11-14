//! Streaming search over a `Text` (DESIGN section 5). The text is read in
//! windows that overlap by the match-length cap, so a match is never lost at
//! a window edge unless it is longer than the cap; look-around assertions
//! (`^`, `$`, `\b`, `\'`) see the real bytes around each window. Searches
//! can be cancelled between windows.
//!
//! Backward search follows Emacs: the match found is the one starting
//! closest before point whose anchored forward match ends at or before
//! point. Plain strings use `memchr::memmem::FinderRev`; regexps confirm
//! each candidate start with an anchored forward match, so no reverse
//! automaton (whose leftmost semantics differ from Emacs's) is involved.

mod emacs_regex;

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use memchr::memmem;
use regex_automata::meta;
use regex_automata::util::syntax;
use regex_automata::{Anchored, Input};
use regex_syntax::hir::literal::{ExtractKind, Extractor};

pub use emacs_regex::{PatternError, emacs_regex_to_rust, regexp_quote};

use crate::source::ReadError;
use crate::text::Text;

/// Bytes of candidate starts read per window. Windows read the cap and a
/// little context beyond this.
pub const SEARCH_CHUNK: u64 = 1024 * 1024;
/// Bytes on each side of a window that look-around assertions may inspect:
/// one UTF-8 character needs at most four.
const CONTEXT_BYTES: u64 = 8;
/// A backward regexp search first looks this far back from point, then
/// doubles the window up to `SEARCH_CHUNK`, so a match near point is found
/// without reading a whole chunk.
const BACKWARD_FIRST_WINDOW: u64 = 4096;
const UTF8_CONTINUATION_MASK: u8 = 0b1100_0000;
const UTF8_CONTINUATION: u8 = 0b1000_0000;
/// The most bytes a window edge moves back to land on a character start.
const UTF8_MAX_CONTINUATION: u64 = 3;

/// What to search for, as the user gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    /// A plain string (Emacs `search-forward`).
    Literal { needle: Vec<u8>, case_fold: bool },
    /// An Emacs regexp (Emacs `re-search-forward`).
    Regex { emacs: String, case_fold: bool },
}

/// A match: `start..end` in the text. `capped` is set when the match may
/// continue past `end`: it reached the match-length cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    pub start: u64,
    pub end: u64,
    pub capped: bool,
}

impl Match {
    pub fn range(&self) -> Range<u64> {
        self.start..self.end
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("{0}")]
    Read(#[from] ReadError),
    #[error("Search cancelled")]
    Cancelled,
}

#[derive(Debug)]
enum Compiled {
    Literal {
        needle: Vec<u8>,
        // Boxed because the finders' prefilter tables make this variant far
        // larger than the regex one.
        forward: Box<memmem::Finder<'static>>,
        backward: Box<memmem::FinderRev<'static>>,
    },
    Regex {
        regex: meta::Regex,
        /// The first bytes every match starts with, when the regexp has
        /// literal prefixes; candidate starts for backward search.
        first_bytes: Option<Box<[bool; 256]>>,
        groups: usize,
    },
}

/// A pattern ready to search with. Cheap to clone and shareable across
/// threads, so a worker can search with the editor's pattern.
#[derive(Debug, Clone)]
pub struct CompiledPattern {
    compiled: Arc<Compiled>,
}

/// Whether reads may go to the file or must come from cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reads {
    Io,
    CachedOnly,
}

impl CompiledPattern {
    pub fn compile(pattern: &Pattern) -> Result<CompiledPattern, PatternError> {
        let compiled = match pattern {
            Pattern::Literal {
                needle,
                case_fold: false,
            } => Compiled::Literal {
                needle: needle.clone(),
                forward: Box::new(memmem::Finder::new(needle).into_owned()),
                backward: Box::new(memmem::FinderRev::new(needle).into_owned()),
            },
            Pattern::Literal {
                needle,
                case_fold: true,
            } => compile_regex(&emacs_regex::literal_to_rust(needle), true)?,
            Pattern::Regex { emacs, case_fold } => {
                compile_regex(&emacs_regex_to_rust(emacs)?, *case_fold)?
            }
        };
        Ok(CompiledPattern {
            compiled: Arc::new(compiled),
        })
    }

    /// How many groups a match reports, the whole match included.
    pub fn group_count(&self) -> usize {
        match &*self.compiled {
            Compiled::Literal { .. } => 1,
            Compiled::Regex { groups, .. } => *groups,
        }
    }
}

fn syntax_config(case_fold: bool) -> syntax::Config {
    // Buffers are bytes that need not be UTF-8, and the translation matches
    // raw bytes explicitly.
    syntax::Config::new()
        .utf8(false)
        .case_insensitive(case_fold)
}

fn compile_regex(rust: &str, case_fold: bool) -> Result<Compiled, PatternError> {
    let config = syntax_config(case_fold);
    let regex = meta::Regex::builder()
        .syntax(config)
        .configure(meta::Regex::config().utf8_empty(false))
        .build(rust)
        .map_err(|error| PatternError::Compile(error.to_string()))?;
    let hir = regex_syntax::ParserBuilder::new()
        .utf8(false)
        .case_insensitive(case_fold)
        .build()
        .parse(rust)
        .map_err(|error| PatternError::Compile(error.to_string()))?;
    let prefixes = Extractor::new().kind(ExtractKind::Prefix).extract(&hir);
    let first_bytes = prefixes.literals().and_then(|literals| {
        if literals.is_empty() || literals.iter().any(|literal| literal.is_empty()) {
            return None;
        }
        let mut table = Box::new([false; 256]);
        for literal in literals {
            table[usize::from(literal.as_bytes()[0])] = true;
        }
        Some(table)
    });
    let groups = regex.captures_len();
    Ok(Compiled::Regex {
        regex,
        first_bytes,
        groups,
    })
}

fn read_window(text: &Text, range: Range<u64>, reads: Reads) -> Result<Vec<u8>, ReadError> {
    let mut bytes = Vec::with_capacity(usize::try_from(range.end - range.start).unwrap_or(0));
    match reads {
        Reads::Io => text.read(range, &mut bytes)?,
        Reads::CachedOnly => text.read_cached(range, &mut bytes)?,
    }
    Ok(bytes)
}

fn offset(position: u64, base: u64) -> usize {
    usize::try_from(position - base).expect("window offsets fit in memory")
}

fn is_continuation(byte: u8) -> bool {
    byte & UTF8_CONTINUATION_MASK == UTF8_CONTINUATION
}

/// Moves a window edge back onto a character start, so the next window
/// never starts inside a character.
fn back_to_character_start(edge: u64, floor: u64, window: &[u8], base: u64) -> u64 {
    let mut edge = edge;
    let mut steps = 0;
    while edge > floor
        && steps < UTF8_MAX_CONTINUATION
        && edge >= base
        && window
            .get(offset(edge, base))
            .is_some_and(|byte| is_continuation(*byte))
    {
        edge -= 1;
        steps += 1;
    }
    if window
        .get(offset(edge, base))
        .is_some_and(|byte| is_continuation(*byte))
    {
        // Not a character after all (a run of stray continuation bytes):
        // each of those bytes is a character of its own.
        return edge + steps;
    }
    edge
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), SearchError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(SearchError::Cancelled);
    }
    Ok(())
}

/// A match `start..end` found in a span that could not extend past
/// `span_end`: capped when it was cut there (and the text goes on) or is
/// longer than `match_cap`.
fn capped_match(start: u64, end: u64, span_end: u64, limit: u64, match_cap: u64) -> Match {
    if (span_end < limit && end == span_end) || end - start > match_cap {
        Match {
            start,
            end: end.min(start + match_cap),
            capped: true,
        }
    } else {
        Match {
            start,
            end,
            capped: false,
        }
    }
}

/// The first match starting at or after `from`, ending at or before `bound`
/// (the end of the text when `None`), as Emacs's forward searches find it.
pub fn search_forward(
    text: &Text,
    pattern: &CompiledPattern,
    from: u64,
    bound: Option<u64>,
    cancel: &AtomicBool,
    match_cap: usize,
) -> Result<Option<Match>, SearchError> {
    search_forward_with(text, pattern, from, bound, cancel, match_cap, Reads::Io)
}

fn search_forward_with(
    text: &Text,
    pattern: &CompiledPattern,
    from: u64,
    bound: Option<u64>,
    cancel: &AtomicBool,
    match_cap: usize,
    reads: Reads,
) -> Result<Option<Match>, SearchError> {
    let limit = bound.unwrap_or(text.len());
    assert!(
        from <= limit && limit <= text.len(),
        "search_forward from {from} to {limit} in {} bytes",
        text.len()
    );
    let match_cap = match_cap as u64;
    let mut position = from;
    loop {
        check_cancel(cancel)?;
        let chunk_end = limit.min(position + SEARCH_CHUNK);
        match &*pattern.compiled {
            Compiled::Literal {
                needle, forward, ..
            } => {
                let overlap = (needle.len() as u64).saturating_sub(1);
                let window_end = limit.min(chunk_end + overlap);
                let window = read_window(text, position..window_end, reads)?;
                if let Some(found) = forward.find(&window) {
                    let start = position + found as u64;
                    return Ok(Some(Match {
                        start,
                        end: start + needle.len() as u64,
                        capped: false,
                    }));
                }
                if chunk_end >= limit {
                    return Ok(None);
                }
                position = chunk_end;
            }
            Compiled::Regex { regex, .. } => {
                let span_end = limit.min(chunk_end + match_cap);
                let base = position.saturating_sub(CONTEXT_BYTES);
                let window_end = text.len().min(span_end + CONTEXT_BYTES);
                let window = read_window(text, base..window_end, reads)?;
                let chunk_end = if chunk_end < limit {
                    back_to_character_start(chunk_end, position + 1, &window, base)
                } else {
                    chunk_end
                };
                let input =
                    Input::new(&window).range(offset(position, base)..offset(span_end, base));
                if let Some(found) = regex.search(&input) {
                    let start = base + found.start() as u64;
                    if start < chunk_end || chunk_end >= limit {
                        let end = base + found.end() as u64;
                        return Ok(Some(capped_match(start, end, span_end, limit, match_cap)));
                    }
                }
                if chunk_end >= limit {
                    return Ok(None);
                }
                position = chunk_end;
            }
        }
    }
}

/// Emacs's backward search from `point`: the match starting closest before
/// (or at) `point`, at or after `bound`, whose anchored forward match ends
/// at or before `point`. In `aaa` with point at the end, `aa` is found at
/// 1, not 0.
pub fn search_backward(
    text: &Text,
    pattern: &CompiledPattern,
    point: u64,
    bound: Option<u64>,
    cancel: &AtomicBool,
    match_cap: usize,
) -> Result<Option<Match>, SearchError> {
    let lower = bound.unwrap_or(0);
    assert!(
        lower <= point && point <= text.len(),
        "search_backward from {point} to {lower} in {} bytes",
        text.len()
    );
    let match_cap = match_cap as u64;
    match &*pattern.compiled {
        Compiled::Literal {
            needle, backward, ..
        } => {
            let length = needle.len() as u64;
            if point - lower < length {
                return Ok(None);
            }
            // Occurrences must lie inside `lower..top`.
            let mut top = point;
            loop {
                check_cancel(cancel)?;
                let window_start = lower.max(top.saturating_sub(SEARCH_CHUNK + length));
                let window = read_window(text, window_start..top, Reads::Io)?;
                if let Some(found) = backward.rfind(&window) {
                    let start = window_start + found as u64;
                    return Ok(Some(Match {
                        start,
                        end: start + length,
                        capped: false,
                    }));
                }
                if window_start == lower {
                    return Ok(None);
                }
                top = window_start + length.saturating_sub(1);
            }
        }
        Compiled::Regex {
            regex, first_bytes, ..
        } => {
            let mut highest = point;
            let mut window_size = BACKWARD_FIRST_WINDOW;
            loop {
                check_cancel(cancel)?;
                let lowest = lower.max(highest.saturating_sub(window_size - 1));
                if let Some(found) = last_start_in_window(
                    text,
                    regex,
                    first_bytes.as_deref(),
                    lowest..=highest,
                    point,
                    match_cap,
                )? {
                    return Ok(Some(found));
                }
                if lowest == lower {
                    return Ok(None);
                }
                highest = lowest - 1;
                window_size = (window_size * 2).min(SEARCH_CHUNK);
            }
        }
    }
}

/// The match whose start is the highest candidate in `candidates` that
/// matches with its end at or before `point`.
fn last_start_in_window(
    text: &Text,
    regex: &meta::Regex,
    first_bytes: Option<&[bool; 256]>,
    candidates: std::ops::RangeInclusive<u64>,
    point: u64,
    match_cap: u64,
) -> Result<Option<Match>, SearchError> {
    let (lowest, highest) = (*candidates.start(), *candidates.end());
    let span_end = point.min(highest + match_cap);
    let base = lowest.saturating_sub(CONTEXT_BYTES);
    let window_end = text.len().min(span_end + CONTEXT_BYTES);
    let window = read_window(text, base..window_end, Reads::Io)?;
    let leftmost = |from: u64| -> Option<u64> {
        let input = Input::new(&window).range(offset(from, base)..offset(span_end, base));
        regex
            .search(&input)
            .map(|found| base + found.start() as u64)
            .filter(|start| *start <= highest)
    };
    // No match starts in the window at all: the common case, settled by
    // one forward scan.
    let Some(first) = leftmost(lowest) else {
        return Ok(None);
    };
    let anchored = |start: u64| -> Option<Match> {
        let input = Input::new(&window)
            .range(offset(start, base)..offset(span_end, base))
            .anchored(Anchored::Yes);
        regex
            .search(&input)
            .map(|found| capped_match(start, base + found.end() as u64, span_end, point, match_cap))
    };
    let start = match first_bytes {
        // Literal prefixes: try each start whose byte can begin a match,
        // from the highest down.
        Some(table) => (first..=highest).rev().find(|start| {
            window
                .get(offset(*start, base))
                .is_some_and(|byte| table[usize::from(*byte)])
                && anchored(*start).is_some()
        }),
        // Otherwise every start is a candidate. "Some match starts at or
        // after q" only gets less true as q grows, so the highest matching
        // start is found by bisection over forward scans.
        None => {
            let (mut low, mut high) = (first, highest);
            while low < high {
                let middle = low + (high - low).div_ceil(2);
                if leftmost(middle).is_some() {
                    low = middle;
                } else {
                    high = middle - 1;
                }
            }
            Some(low)
        }
    };
    Ok(start.and_then(anchored))
}

/// The match anchored at `at` that ends at or before `limit`, as Emacs
/// `looking-at` finds it.
pub fn match_at(
    text: &Text,
    pattern: &CompiledPattern,
    at: u64,
    limit: Option<u64>,
    match_cap: usize,
) -> Result<Option<Match>, SearchError> {
    match_at_with(text, pattern, at, limit, match_cap, Reads::Io)
}

/// `match_at` reading only cached text, for the editor thread, which must
/// never wait for I/O; uncached text is a `ReadError::NotCached`.
pub fn match_at_cached(
    text: &Text,
    pattern: &CompiledPattern,
    at: u64,
    limit: Option<u64>,
    match_cap: usize,
) -> Result<Option<Match>, SearchError> {
    match_at_with(text, pattern, at, limit, match_cap, Reads::CachedOnly)
}

fn match_at_with(
    text: &Text,
    pattern: &CompiledPattern,
    at: u64,
    limit: Option<u64>,
    match_cap: usize,
    reads: Reads,
) -> Result<Option<Match>, SearchError> {
    let limit = limit.unwrap_or(text.len());
    assert!(
        at <= limit && limit <= text.len(),
        "match_at({at}) past {limit}"
    );
    let match_cap = match_cap as u64;
    match &*pattern.compiled {
        Compiled::Literal { needle, .. } => {
            let end = at + needle.len() as u64;
            if end > limit {
                return Ok(None);
            }
            let window = read_window(text, at..end, reads)?;
            Ok((window == *needle).then_some(Match {
                start: at,
                end,
                capped: false,
            }))
        }
        Compiled::Regex { regex, .. } => {
            let span_end = limit.min(at + match_cap);
            let base = at.saturating_sub(CONTEXT_BYTES);
            let window = read_window(text, base..text.len().min(span_end + CONTEXT_BYTES), reads)?;
            let input = Input::new(&window)
                .range(offset(at, base)..offset(span_end, base))
                .anchored(Anchored::Yes);
            Ok(regex.search(&input).map(|found| {
                capped_match(at, base + found.end() as u64, span_end, limit, match_cap)
            }))
        }
    }
}

/// Every match lying inside `range`, left to right without overlaps, as
/// lazy highlighting shows them. After an empty match the next search
/// starts one character later.
pub fn matches_in(
    text: &Text,
    pattern: &CompiledPattern,
    range: Range<u64>,
    match_cap: usize,
) -> Result<Vec<Match>, SearchError> {
    matches_in_with(text, pattern, range, match_cap, Reads::Io)
}

/// `matches_in` reading only cached text, for rendering, which must never
/// wait for I/O; uncached text is a `ReadError::NotCached`.
pub fn matches_in_cached(
    text: &Text,
    pattern: &CompiledPattern,
    range: Range<u64>,
    match_cap: usize,
) -> Result<Vec<Match>, SearchError> {
    matches_in_with(text, pattern, range, match_cap, Reads::CachedOnly)
}

fn matches_in_with(
    text: &Text,
    pattern: &CompiledPattern,
    range: Range<u64>,
    match_cap: usize,
    reads: Reads,
) -> Result<Vec<Match>, SearchError> {
    assert!(
        range.start <= range.end && range.end <= text.len(),
        "matches_in {range:?} in {} bytes",
        text.len()
    );
    let never = AtomicBool::new(false);
    let mut found = Vec::new();
    let mut position = range.start;
    while position <= range.end {
        let Some(next) = search_forward_with(
            text,
            pattern,
            position,
            Some(range.end),
            &never,
            match_cap,
            reads,
        )?
        else {
            break;
        };
        found.push(next);
        position = if next.is_empty() {
            if next.end >= range.end {
                break;
            }
            next_character(text, next.end, reads)?
        } else {
            next.end
        };
    }
    Ok(found)
}

/// The start of the character after the one at `at`.
fn next_character(text: &Text, at: u64, reads: Reads) -> Result<u64, ReadError> {
    let end = text.len().min(at + UTF8_MAX_CONTINUATION + 1);
    let window = read_window(text, at..end, reads)?;
    let length = window.utf8_chunks().next().map_or(1, |chunk| {
        chunk.valid().chars().next().map_or(1, char::len_utf8)
    });
    Ok(at + length as u64)
}

/// The groups of `found` (a match of `pattern`): the whole match first, then
/// each `\(...\)` group, `None` for a group that did not take part.
pub fn captures(
    text: &Text,
    pattern: &CompiledPattern,
    found: &Match,
) -> Result<Vec<Option<Range<u64>>>, SearchError> {
    match &*pattern.compiled {
        Compiled::Literal { .. } => Ok(vec![Some(found.range())]),
        Compiled::Regex { regex, .. } => {
            let base = found.start.saturating_sub(CONTEXT_BYTES);
            let window = read_window(
                text,
                base..text.len().min(found.end + CONTEXT_BYTES),
                Reads::Io,
            )?;
            let input = Input::new(&window)
                .range(offset(found.start, base)..offset(found.end, base))
                .anchored(Anchored::Yes);
            let mut groups = regex.create_captures();
            regex.search_captures(&input, &mut groups);
            Ok((0..regex.captures_len())
                .map(|group| {
                    groups
                        .get_group(group)
                        .map(|span| base + span.start as u64..base + span.end as u64)
                })
                .collect())
        }
    }
}
