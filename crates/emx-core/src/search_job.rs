//! Searches the editor hands to a worker (`Effect::StartSearch`): over text
//! that may need file reads, or more text than the editor thread may scan
//! (DESIGN section 8: the editor thread never blocks on I/O). The job holds
//! a snapshot of the text, so the buffer can change meanwhile; the editor
//! checks the result against the buffer when it arrives. The editor sets
//! `cancel` when the search is no longer wanted (`C-g`, or a newer search
//! from the same client supersedes it).

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use emx_text::Text;
use emx_text::search::{
    CompiledPattern, Match, SearchError, captures, matches_in, search_backward, search_forward,
};

use crate::event::{ClientId, Event, SearchId};

const NEWLINE: u8 = b'\n';
const UTF8_MAX_LENGTH: u64 = 4;

/// What to look for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchRequest {
    /// The first match at or after `from`, ending by `bound`.
    Forward { from: u64, bound: Option<u64> },
    /// Emacs's backward search from `from`, starting at or after `bound`.
    Backward { from: u64, bound: Option<u64> },
    /// Every line holding a match, for `occur`, with `context` lines
    /// around each (only before it when negative).
    Occur { context: i64 },
    /// Every match with its groups, for `occur`'s collect mode.
    Collect,
}

#[derive(Debug, Clone)]
pub struct SearchJob {
    pub client: ClientId,
    pub id: SearchId,
    pub text: Text,
    pub pattern: CompiledPattern,
    pub request: SearchRequest,
    pub match_cap: usize,
    pub cancel: Arc<AtomicBool>,
}

impl PartialEq for SearchJob {
    fn eq(&self, other: &Self) -> bool {
        self.client == other.client
            && self.id == other.id
            && self.request == other.request
            && self.text.same_root(&other.text)
    }
}

impl Eq for SearchJob {}

/// One block of `occur` output: the lines a match spans (several when the
/// match spans newlines). `number` is the 1-based number of its first
/// line, `last_number` of its last; `bytes` are the lines without the
/// final newline; `matches` are the matches in them, as offsets in the
/// text. `before` and `after` are the context lines asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OccurLine {
    pub number: u64,
    pub last_number: u64,
    pub start: u64,
    pub bytes: Vec<u8>,
    pub matches: Vec<Range<u64>>,
    pub before: Vec<Vec<u8>>,
    pub after: Vec<Vec<u8>>,
}

/// One match for `occur`'s collect mode: the text of each group, the whole
/// match first.
pub type CollectedMatch = Vec<Option<Vec<u8>>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchOutcome {
    Found(Option<Match>),
    Occur(Vec<OccurLine>),
    Collect(Vec<CollectedMatch>),
    /// The editor gave up on the search before it finished.
    Cancelled,
}

/// Runs the job where it is (a worker, or the harness), turning it into
/// the event that reports it.
pub fn run_search_job(job: &SearchJob) -> Event {
    let result = match run_search(
        &job.text,
        &job.pattern,
        &job.request,
        &job.cancel,
        job.match_cap,
    ) {
        Ok(outcome) => Ok(outcome),
        Err(SearchError::Cancelled) => Ok(SearchOutcome::Cancelled),
        Err(SearchError::Read(error)) => Err(error.to_string()),
    };
    Event::SearchResult {
        client: job.client,
        id: job.id,
        result,
    }
}

/// Carries out `request` over `text`.
pub fn run_search(
    text: &Text,
    pattern: &CompiledPattern,
    request: &SearchRequest,
    cancel: &AtomicBool,
    match_cap: usize,
) -> Result<SearchOutcome, SearchError> {
    match request {
        SearchRequest::Forward { from, bound } => Ok(SearchOutcome::Found(search_forward(
            text, pattern, *from, *bound, cancel, match_cap,
        )?)),
        SearchRequest::Backward { from, bound } => Ok(SearchOutcome::Found(search_backward(
            text, pattern, *from, *bound, cancel, match_cap,
        )?)),
        SearchRequest::Occur { context } => Ok(SearchOutcome::Occur(occur_lines(
            text, pattern, *context, cancel, match_cap,
        )?)),
        SearchRequest::Collect => Ok(SearchOutcome::Collect(collect(
            text, pattern, cancel, match_cap,
        )?)),
    }
}

/// Emacs `occur-engine` for one buffer: each match is extended to whole
/// lines, the next search starts on the line after the last one shown, and
/// every match within the shown lines is listed with it.
fn occur_lines(
    text: &Text,
    pattern: &CompiledPattern,
    context: i64,
    cancel: &AtomicBool,
    match_cap: usize,
) -> Result<Vec<OccurLine>, SearchError> {
    let mut lines = Vec::new();
    let mut position = 0;
    // Line number of `counted_to`, which only moves forward, so counting
    // lines costs one pass over the text.
    let mut counted_to = 0;
    let mut line_number = 1;
    let length = text.len();
    while position < length {
        let Some(found) = search_forward(text, pattern, position, None, cancel, match_cap)? else {
            break;
        };
        let start = line_start(text, found.start)?;
        let end = match text.find_newline_forward(found.end.max(start))? {
            Some(newline) => newline,
            None => length,
        };
        line_number += count_newlines(text, counted_to..start)?;
        counted_to = start;
        let mut bytes = Vec::new();
        text.read(start..end, &mut bytes)?;
        let matches = matches_in(text, pattern, start..end, match_cap)?
            .into_iter()
            .map(|found| found.range())
            .collect();
        let last_number = line_number + memchr::memchr_iter(NEWLINE, &bytes).count() as u64;
        let before = lines_around(text, start, -context.saturating_abs())?;
        let after = if context > 0 {
            lines_around(text, end, context)?
        } else {
            Vec::new()
        };
        lines.push(OccurLine {
            number: line_number,
            last_number,
            start,
            bytes,
            matches,
            before,
            after,
        });
        position = end + 1;
    }
    Ok(lines)
}

/// Emacs `occur-accumulate-lines` without the line of `position`: the
/// `count` lines after it, or before it (nearest last) when negative.
fn lines_around(text: &Text, position: u64, count: i64) -> Result<Vec<Vec<u8>>, SearchError> {
    let mut lines = Vec::new();
    let length = text.len();
    if count > 0 {
        let mut start = match text.find_newline_forward(position)? {
            Some(newline) => newline + 1,
            None => return Ok(lines),
        };
        for _ in 0..count {
            if start >= length {
                break;
            }
            let end = text.find_newline_forward(start)?.unwrap_or(length);
            let mut line = Vec::new();
            text.read(start..end, &mut line)?;
            lines.push(line);
            start = end + 1;
        }
    } else {
        let mut end = line_start(text, position)?;
        for _ in 0..count.unsigned_abs() {
            if end == 0 {
                break;
            }
            let line_end = end - 1;
            let start = line_start(text, line_end)?;
            let mut line = Vec::new();
            text.read(start..line_end, &mut line)?;
            lines.push(line);
            end = start;
        }
        lines.reverse();
    }
    Ok(lines)
}

/// Every match from the start, with its groups' text (`occur`'s collect
/// mode); after an empty match the search moves on one character.
fn collect(
    text: &Text,
    pattern: &CompiledPattern,
    cancel: &AtomicBool,
    match_cap: usize,
) -> Result<Vec<CollectedMatch>, SearchError> {
    let mut found_all = Vec::new();
    let mut position = 0;
    let length = text.len();
    while position <= length {
        let Some(found) = search_forward(text, pattern, position, None, cancel, match_cap)? else {
            break;
        };
        let mut groups = Vec::new();
        for group in captures(text, pattern, &found)? {
            groups.push(match group {
                Some(range) => {
                    let mut bytes = Vec::new();
                    text.read(range, &mut bytes)?;
                    Some(bytes)
                }
                None => None,
            });
        }
        found_all.push(groups);
        position = if found.is_empty() {
            if found.end >= length {
                break;
            }
            next_character(text, found.end)?
        } else {
            found.end
        };
    }
    Ok(found_all)
}

/// The start of the character after the one at `at`; an invalid byte is a
/// character of its own.
fn next_character(text: &Text, at: u64) -> Result<u64, SearchError> {
    let mut bytes = Vec::new();
    text.read(at..text.len().min(at + UTF8_MAX_LENGTH), &mut bytes)?;
    let length = bytes.utf8_chunks().next().map_or(1, |chunk| {
        chunk.valid().chars().next().map_or(1, char::len_utf8)
    });
    Ok(at + length as u64)
}

fn line_start(text: &Text, position: u64) -> Result<u64, SearchError> {
    Ok(match text.find_newline_backward(position)? {
        Some(newline) => newline + 1,
        None => 0,
    })
}

fn count_newlines(text: &Text, range: Range<u64>) -> Result<u64, SearchError> {
    let mut count = 0;
    for chunk in text.chunks(range) {
        count += memchr::memchr_iter(NEWLINE, chunk?.bytes()).count() as u64;
    }
    Ok(count)
}
