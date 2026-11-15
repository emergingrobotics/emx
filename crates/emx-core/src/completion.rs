//! Vertical incremental completion in the style of Emacs
//! `fido-vertical-mode` / vertico (DESIGN section 4): the candidates a
//! minibuffer prompt completes over are listed one per line above the
//! prompt and narrow as the input changes.
//!
//! Matching is `fido-mode`'s `flex` style, ignoring case: a candidate
//! matches when the input's characters appear in it in order. Matches are
//! ranked exact, then prefix, then substring, then other flex matches; each
//! rank keeps the source's own order. Narrowing is O(candidates) per key,
//! and when the input only grew, only the previous matches are looked at.
//!
//! In-memory sources (buffers, commands, the kill ring) implement
//! `CompletionSource`. File names come from a directory listing that the
//! editor never reads itself: it asks for one with
//! `Effect::ListDirectory`, which a worker answers with
//! `Event::DirectoryListed`, and `FileSource` (and the minibuffer's
//! `Completer`) completes over the listing once it is there.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::event::ListingId;
use crate::io::expand_file_name;

/// Only this many bytes of a candidate are matched against: a kill can be
/// megabytes long, and the ranking is decided near its start anyway.
const MATCH_BYTES_MAX: usize = 64 * 1024;
const DIRECTORY_SEPARATOR: u8 = b'/';
/// What `read-from-kill-ring` displays for leading whitespace and for the
/// end of a kill too long for the frame.
const ELLIPSIS: &str = "\u{2026}";
/// icomplete's message when nothing matches, in `[]`, or `()` when the
/// prompt requires a match.
const NO_MATCHES: &str = "No matches";
/// Shown while the directory listing is on its way.
pub(crate) const LISTING: &str = "Listing directory...";
/// Shown when the directory could not be listed because too many listings
/// are outstanding (directories on a hung mount never answer).
const OVERLOADED: &str = "Too many directory listings in progress";
/// Buffer names starting with this are internal (Emacs's convention).
const INTERNAL_BUFFER_PREFIX: char = ' ';

/// Why a directory has no listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListingError {
    /// It could not be read (missing, or not readable), with why.
    Unreadable(String),
    /// It was not read at all: as many listings as allowed are still
    /// outstanding, which only directories that never answer cause.
    Overloaded,
}

/// One thing a prompt can complete to. A directory's name ends in `/`, as
/// Emacs file-name completion gives it.
#[derive(Debug, Clone)]
pub struct Candidate {
    text: Vec<u8>,
    /// `text`, or its first `MATCH_BYTES_MAX` bytes, with letters folded to
    /// lower case: completion ignores case, as `fido-mode` sets it to.
    folded: Vec<u8>,
    /// The whole text was folded, so `folded` can equal the input exactly.
    whole: bool,
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
    }
}

impl Eq for Candidate {}

impl Candidate {
    pub fn new(text: impl Into<Vec<u8>>) -> Candidate {
        let text = text.into();
        let whole = text.len() <= MATCH_BYTES_MAX;
        let folded = fold_case(&text[..text.len().min(MATCH_BYTES_MAX)]);
        Candidate {
            text,
            folded,
            whole,
        }
    }

    pub fn text(&self) -> &[u8] {
        &self.text
    }

    /// A directory in file-name completion.
    pub fn is_directory(&self) -> bool {
        self.text.ends_with(&[DIRECTORY_SEPARATOR])
    }

    fn tier(&self, pattern: &Pattern) -> Option<Tier> {
        let text = self.folded.as_slice();
        let needle = pattern.folded.as_slice();
        if self.whole && text == needle {
            Some(Tier::Exact)
        } else if text.starts_with(needle) {
            Some(Tier::Prefix)
        } else if contains(text, needle) {
            Some(Tier::Substring)
        } else if pattern.flex_matches(text) {
            Some(Tier::Flex)
        } else {
            None
        }
    }
}

/// Where a candidate ranks for an input; earlier is better.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    Exact,
    Prefix,
    Substring,
    Flex,
}

const TIERS: usize = 4;

impl Tier {
    fn index(self) -> usize {
        self as usize
    }
}

/// The input a candidate is matched against, folded once.
struct Pattern {
    folded: Vec<u8>,
    /// The folded input split into characters (an invalid byte is one),
    /// which a flex match finds in order.
    units: Vec<(usize, usize)>,
}

impl Pattern {
    fn new(input: &[u8]) -> Pattern {
        let folded = fold_case(input);
        let mut units = Vec::new();
        let mut offset = 0;
        for chunk in folded.utf8_chunks() {
            for character in chunk.valid().chars() {
                units.push((offset, character.len_utf8()));
                offset += character.len_utf8();
            }
            for _ in chunk.invalid() {
                units.push((offset, 1));
                offset += 1;
            }
        }
        Pattern { folded, units }
    }

    fn flex_matches(&self, text: &[u8]) -> bool {
        let mut rest = text;
        for (offset, length) in &self.units {
            let unit = &self.folded[*offset..offset + length];
            match find(rest, unit) {
                Some(at) => rest = &rest[at + length..],
                None => return false,
            }
        }
        true
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    find(haystack, needle).is_some()
}

/// Lower-cases each character; bytes that are not UTF-8 are kept as they
/// are, so a file name that is not UTF-8 still matches itself.
fn fold_case(bytes: &[u8]) -> Vec<u8> {
    let mut folded = Vec::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        let valid = chunk.valid();
        if valid.is_ascii() {
            folded.extend(valid.bytes().map(|byte| byte.to_ascii_lowercase()));
        } else {
            folded.extend_from_slice(valid.to_lowercase().as_bytes());
        }
        folded.extend_from_slice(chunk.invalid());
    }
    folded
}

/// Characters in `bytes`, an invalid byte counting as one (as Emacs counts
/// a raw byte).
fn unit_count(bytes: &[u8]) -> usize {
    bytes
        .utf8_chunks()
        .map(|chunk| chunk.valid().chars().count() + chunk.invalid().len())
        .sum()
}

/// Emacs `minibuffer--sort-by-length-alpha`, the order completion lists
/// candidates in when the table gives none: shorter first, then by bytes.
pub fn sort_by_length_then_name(candidates: &mut [Candidate]) {
    candidates
        .sort_by_cached_key(|candidate| (unit_count(&candidate.text), candidate.text.clone()));
}

/// The result of matching: `order` ranks the matches for display, and
/// `pool` holds the same indexes in source order, which a longer input
/// narrows further.
struct Filtered {
    order: Vec<usize>,
    pool: Vec<usize>,
}

/// Matches the candidates at `within` (in source order) against `input`.
fn filter(
    candidates: &[Candidate],
    within: impl IntoIterator<Item = usize>,
    input: &[u8],
) -> Filtered {
    if input.is_empty() {
        let pool: Vec<usize> = within.into_iter().collect();
        return Filtered {
            order: pool.clone(),
            pool,
        };
    }
    let pattern = Pattern::new(input);
    let mut tiers: [Vec<usize>; TIERS] = Default::default();
    let mut pool = Vec::new();
    for index in within {
        if let Some(tier) = candidates[index].tier(&pattern) {
            tiers[tier.index()].push(index);
            pool.push(index);
        }
    }
    Filtered {
        order: tiers.concat(),
        pool,
    }
}

/// A source of candidates that are known without I/O.
pub trait CompletionSource {
    /// The candidates matching `input`, best first. Input is bytes because
    /// file names need not be UTF-8.
    fn candidates(&self, input: &[u8]) -> Vec<Candidate>;
}

/// A fixed list in its display order: buffers, commands, kills.
#[derive(Debug, Clone, Default)]
pub struct ListSource {
    candidates: Vec<Candidate>,
}

impl ListSource {
    pub fn new(candidates: Vec<Candidate>) -> ListSource {
        ListSource { candidates }
    }

    /// Buffer names, most recently shown first and the current buffer
    /// last. Names starting with a space are internal and left out, as
    /// Emacs `internal-complete-buffer` leaves them out.
    pub fn buffers<'a>(
        most_recent_first: impl IntoIterator<Item = &'a str>,
        current: &str,
    ) -> ListSource {
        let mut candidates: Vec<Candidate> = most_recent_first
            .into_iter()
            .filter(|name| *name != current && !name.starts_with(INTERNAL_BUFFER_PREFIX))
            .map(Candidate::new)
            .collect();
        if !current.starts_with(INTERNAL_BUFFER_PREFIX) {
            candidates.push(Candidate::new(current));
        }
        ListSource { candidates }
    }

    /// Command names shortest first, then alphabetically, with the ones in
    /// `history` (newest first) moved to the front, as Emacs sorts `M-x`
    /// candidates (`completion-all-sorted-completions`).
    pub fn commands<'a>(
        names: impl IntoIterator<Item = &'a str>,
        history: &[Vec<u8>],
    ) -> ListSource {
        let mut candidates: Vec<Candidate> = names.into_iter().map(Candidate::new).collect();
        sort_by_length_then_name(&mut candidates);
        let position = |candidate: &Candidate| {
            history
                .iter()
                .position(|entry| entry.as_slice() == candidate.text())
                .unwrap_or(history.len())
        };
        candidates.sort_by_cached_key(position);
        ListSource { candidates }
    }

    /// Kills newest first, without repeats (`read-from-kill-ring` keeps the
    /// kill ring's order; completion drops duplicates).
    pub fn kill_ring<'a>(entries: impl IntoIterator<Item = &'a [u8]>) -> ListSource {
        let mut candidates: Vec<Candidate> = Vec::new();
        for entry in entries {
            if !candidates.iter().any(|seen| seen.text() == entry) {
                candidates.push(Candidate::new(entry));
            }
        }
        ListSource { candidates }
    }

    fn shared(&self) -> Arc<[Candidate]> {
        Arc::from(self.candidates.as_slice())
    }
}

impl CompletionSource for ListSource {
    fn candidates(&self, input: &[u8]) -> Vec<Candidate> {
        filter(&self.candidates, 0..self.candidates.len(), input)
            .order
            .into_iter()
            .map(|index| self.candidates[index].clone())
            .collect()
    }
}

/// File names in the input's directory. The listing is read elsewhere and
/// handed in with `set_listing`; until it is, there are no candidates.
///
/// This deviates from the plain `CompletionSource` shape in where the
/// candidates come from: reading a directory can block (NFS), so it is a
/// cached listing fed by `Event::DirectoryListed`, never a read here.
#[derive(Debug, Clone)]
pub struct FileSource {
    default_directory: PathBuf,
    listing: Option<(PathBuf, Arc<[Candidate]>)>,
}

impl FileSource {
    pub fn new(default_directory: PathBuf) -> FileSource {
        FileSource {
            default_directory,
            listing: None,
        }
    }

    /// The directory whose entries complete `input`.
    pub fn directory_of(&self, input: &[u8]) -> PathBuf {
        input_directory(split_file_input(input).0, &self.default_directory)
    }

    pub fn set_listing(&mut self, directory: PathBuf, entries: Vec<Candidate>) {
        self.listing = Some((directory, Arc::from(entries)));
    }
}

impl CompletionSource for FileSource {
    fn candidates(&self, input: &[u8]) -> Vec<Candidate> {
        let (directory_part, name) = split_file_input(input);
        let directory = input_directory(directory_part, &self.default_directory);
        match &self.listing {
            Some((listed, entries)) if *listed == directory => {
                filter(entries, 0..entries.len(), name)
                    .order
                    .into_iter()
                    .map(|index| entries[index].clone())
                    .collect()
            }
            _ => Vec::new(),
        }
    }
}

/// Splits file-name input after its last `/`: the directory part, and the
/// name being completed (Emacs's completion boundaries).
pub(crate) fn split_file_input(input: &[u8]) -> (&[u8], &[u8]) {
    match input.iter().rposition(|byte| *byte == DIRECTORY_SEPARATOR) {
        Some(slash) => input.split_at(slash + 1),
        None => (&[], input),
    }
}

fn input_directory(directory_part: &[u8], default_directory: &Path) -> PathBuf {
    if directory_part.is_empty() {
        return default_directory.to_path_buf();
    }
    expand_file_name(directory_part, default_directory)
}

/// What a prompt completes, which decides where candidates come from and
/// how they are shown.
#[derive(Debug, Clone)]
pub(crate) enum Kind {
    File {
        default_directory: PathBuf,
    },
    Buffer,
    Command,
    KillRing,
    /// `project-find-file`: paths relative to the project root.
    ProjectFile,
}

/// One prompt's completion state: the candidates, the matches for the
/// current input, and which one is selected.
#[derive(Debug)]
pub(crate) struct Completer {
    kind: Kind,
    /// Every candidate, in source order. For files, the listing of the
    /// directory in `listed`, empty until it arrives.
    base: Arc<[Candidate]>,
    listed: Option<PathBuf>,
    listing_error: Option<ListingError>,
    /// The listing asked for and not yet arrived. Any other listing that
    /// arrives is stale and ignored.
    awaited: Option<(ListingId, PathBuf)>,
    /// A listing to ask for; the editor takes it and sends the request.
    to_request: Option<PathBuf>,
    /// Bubbled to the top while the input is still the initial input, as
    /// icomplete does, so `RET` on untouched input takes it.
    default: Option<Vec<u8>>,
    initial: Vec<u8>,
    require_match: bool,
    /// The input part the matches are for, `None` when they must be
    /// computed from scratch.
    filtered_for: Option<Vec<u8>>,
    pool: Vec<usize>,
    matches: Vec<usize>,
    selected: usize,
    first_shown: usize,
    /// How many candidates a screen shows, which scrolling keeps the
    /// selected one within.
    rows: usize,
}

impl Completer {
    /// Completion over a fixed list.
    pub(crate) fn listed(
        kind: Kind,
        source: &ListSource,
        default: Option<Vec<u8>>,
        require_match: bool,
        rows: usize,
    ) -> Completer {
        Completer::new(kind, source.shared(), default, require_match, rows)
    }

    /// File-name completion relative to `default_directory`.
    pub(crate) fn files(default_directory: PathBuf, rows: usize) -> Completer {
        Completer::new(
            Kind::File { default_directory },
            Arc::from(Vec::new()),
            None,
            false,
            rows,
        )
    }

    fn new(
        kind: Kind,
        base: Arc<[Candidate]>,
        default: Option<Vec<u8>>,
        require_match: bool,
        rows: usize,
    ) -> Completer {
        Completer {
            kind,
            base,
            listed: None,
            listing_error: None,
            awaited: None,
            to_request: None,
            default,
            initial: Vec::new(),
            require_match,
            filtered_for: None,
            pool: Vec::new(),
            matches: Vec::new(),
            selected: 0,
            first_shown: 0,
            rows,
        }
    }

    /// Starts on the prompt's initial input.
    pub(crate) fn start(&mut self, initial: &[u8]) {
        self.initial = initial.to_vec();
        self.update(initial);
    }

    pub(crate) fn completes_files(&self) -> bool {
        matches!(self.kind, Kind::File { .. })
    }

    /// The input changed: match again, and for files, ask for the listing
    /// of a directory not listed yet. The selection goes back to the top.
    pub(crate) fn update(&mut self, input: &[u8]) {
        let pattern = match &self.kind {
            Kind::File { default_directory } => {
                let (directory_part, name) = split_file_input(input);
                let directory = input_directory(directory_part, default_directory);
                if self.listed.as_ref() == Some(&directory) {
                    self.to_request = None;
                } else {
                    self.base = Arc::from(Vec::new());
                    self.listed = None;
                    self.listing_error = None;
                    self.filtered_for = None;
                    let awaited = self.awaited.as_ref().map(|(_, awaited)| awaited);
                    if awaited != Some(&directory) {
                        self.awaited = None;
                        self.to_request = Some(directory);
                    }
                }
                name.to_vec()
            }
            Kind::Buffer | Kind::Command | Kind::KillRing | Kind::ProjectFile => input.to_vec(),
        };
        let narrowing = self
            .filtered_for
            .as_ref()
            .is_some_and(|previous| pattern.starts_with(previous));
        let filtered = if narrowing {
            filter(&self.base, self.pool.iter().copied(), &pattern)
        } else {
            filter(&self.base, 0..self.base.len(), &pattern)
        };
        self.pool = filtered.pool;
        self.matches = filtered.order;
        self.filtered_for = Some(pattern);
        if input == self.initial.as_slice() {
            self.bubble_default();
        }
        self.selected = 0;
        self.first_shown = 0;
    }

    fn bubble_default(&mut self) {
        let Some(default) = &self.default else {
            return;
        };
        let found = self
            .matches
            .iter()
            .position(|index| self.base[*index].text() == default.as_slice());
        if let Some(position) = found {
            let index = self.matches.remove(position);
            self.matches.insert(0, index);
        }
    }

    /// The directory to list next, if any; the caller sends the request
    /// and reports its id with `await_listing`.
    pub(crate) fn take_listing_request(&mut self) -> Option<PathBuf> {
        self.to_request.take()
    }

    pub(crate) fn await_listing(&mut self, id: ListingId, directory: PathBuf) {
        self.awaited = Some((id, directory));
    }

    /// A listing arrived. Returns whether it was the one awaited; a stale
    /// one (the input moved on to another directory) changes nothing.
    pub(crate) fn listing_arrived(
        &mut self,
        id: ListingId,
        result: Result<Vec<Candidate>, ListingError>,
        input: &[u8],
    ) -> bool {
        let Some((_, directory)) = self.awaited.take_if(|(awaited, _)| *awaited == id) else {
            return false;
        };
        match result {
            Ok(entries) => {
                self.base = Arc::from(entries);
                self.listing_error = None;
            }
            Err(error) => {
                self.base = Arc::from(Vec::new());
                self.listing_error = Some(error);
            }
        }
        self.listed = Some(directory);
        self.filtered_for = None;
        self.update(input);
        true
    }

    /// A listing is wanted or on its way: the candidates are not known yet.
    pub(crate) fn pending(&self) -> bool {
        self.awaited.is_some() || self.to_request.is_some()
    }

    pub(crate) fn match_count(&self) -> usize {
        self.matches.len()
    }

    pub(crate) fn selected(&self) -> Option<&Candidate> {
        self.matches
            .get(self.selected)
            .map(|index| &self.base[*index])
    }

    pub(crate) fn selected_index(&self) -> usize {
        self.selected
    }

    /// The input with the selected candidate completed into it: for files,
    /// the candidate replaces the name after the last `/`.
    pub(crate) fn completed_input(&self, input: &[u8], candidate: &Candidate) -> Vec<u8> {
        match self.kind {
            Kind::File { .. } => {
                let mut completed = split_file_input(input).0.to_vec();
                completed.extend_from_slice(candidate.text());
                completed
            }
            Kind::Buffer | Kind::Command | Kind::KillRing | Kind::ProjectFile => {
                candidate.text().to_vec()
            }
        }
    }

    /// `icomplete-forward-completions` with `icomplete-scroll`: stops at
    /// the last candidate.
    pub(crate) fn select_next(&mut self) {
        if self.selected + 1 < self.matches.len() {
            self.select(self.selected + 1);
        }
    }

    pub(crate) fn select_previous(&mut self) {
        if self.selected > 0 {
            self.select(self.selected - 1);
        }
    }

    pub(crate) fn select_first(&mut self) {
        self.select(0);
    }

    pub(crate) fn select_last(&mut self) {
        self.select(self.matches.len().saturating_sub(1));
    }

    fn select(&mut self, index: usize) {
        self.selected = index;
        self.first_shown = first_shown(self.first_shown, self.selected, self.rows);
    }

    /// The candidates `rows` lines show, with the selected one's line.
    pub(crate) fn view(&self, rows: usize) -> (Vec<&Candidate>, Option<usize>) {
        if rows == 0 || self.matches.is_empty() {
            return (Vec::new(), None);
        }
        let first = first_shown(self.first_shown, self.selected, rows);
        let shown = self.matches[first..]
            .iter()
            .take(rows)
            .map(|index| &self.base[*index])
            .collect();
        (shown, Some(self.selected - first))
    }

    /// What the prompt shows after the input: that the listing is on its
    /// way; icomplete's ` [No matches]` when nothing matches (also for a
    /// directory that cannot be read, as Emacs 29 fido shows it); or that
    /// the listing was refused because too many are outstanding.
    pub(crate) fn status(&self) -> Option<String> {
        let (open, close) = if self.require_match {
            ('(', ')')
        } else {
            ('[', ']')
        };
        if self.pending() {
            return Some(format!(" [{LISTING}]"));
        }
        if !self.matches.is_empty() {
            return None;
        }
        let text = match &self.listing_error {
            Some(ListingError::Overloaded) => OVERLOADED,
            Some(ListingError::Unreadable(_)) => NO_MATCHES,
            None => {
                // An empty directory with nothing typed yet: fido would
                // still list `./` and `../`, which emx leaves out.
                let typed = self
                    .filtered_for
                    .as_ref()
                    .is_some_and(|pattern| !pattern.is_empty());
                if !typed {
                    return None;
                }
                NO_MATCHES
            }
        };
        Some(format!(" {open}{text}{close}"))
    }

    /// How a candidate is shown on a line `columns` wide.
    pub(crate) fn display(&self, candidate: &Candidate, columns: usize) -> Vec<u8> {
        match self.kind {
            Kind::KillRing => kill_display(candidate.text(), columns),
            Kind::File { .. } | Kind::Buffer | Kind::Command | Kind::ProjectFile => {
                candidate.text().to_vec()
            }
        }
    }
}

/// The first line shown so that `selected` is on one of `rows` lines,
/// scrolling as little as possible from `previous`.
fn first_shown(previous: usize, selected: usize, rows: usize) -> usize {
    if rows == 0 {
        return selected;
    }
    if selected < previous {
        selected
    } else if selected >= previous + rows {
        selected + 1 - rows
    } else {
        previous
    }
}

/// `read-from-kill-ring`'s display of a kill: leading whitespace shows as
/// one ellipsis, and a kill longer than the frame is cut to `limit`
/// characters after it, ending in an ellipsis.
pub(crate) fn kill_display(text: &[u8], limit: usize) -> Vec<u8> {
    let mut units = units(text).peekable();
    let mut leading = 0;
    while units.peek().is_some_and(|unit| {
        std::str::from_utf8(unit).is_ok_and(|unit| unit.chars().all(char::is_whitespace))
    }) {
        units.next();
        leading += 1;
    }
    let mut shown = Vec::new();
    if leading > 0 {
        shown.extend_from_slice(ELLIPSIS.as_bytes());
    }
    let rest: Vec<&[u8]> = units.collect();
    // Emacs compares the whole length with the limit plus the leading
    // whitespace, which comes to comparing what follows that whitespace.
    if rest.len() > limit {
        for unit in &rest[..limit] {
            shown.extend_from_slice(unit);
        }
        shown.extend_from_slice(ELLIPSIS.as_bytes());
    } else {
        for unit in rest {
            shown.extend_from_slice(unit);
        }
    }
    shown
}

/// The characters of `bytes`, an invalid byte being one.
fn units(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.utf8_chunks().flat_map(|chunk| {
        let valid = chunk.valid();
        let characters = valid
            .char_indices()
            .map(move |(at, character)| &valid.as_bytes()[at..at + character.len_utf8()]);
        let invalid = chunk.invalid();
        characters.chain((0..invalid.len()).map(move |at| &invalid[at..=at]))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(candidates: &[Candidate]) -> Vec<String> {
        candidates
            .iter()
            .map(|candidate| String::from_utf8_lossy(candidate.text()).into_owned())
            .collect()
    }

    #[test]
    fn ranks_exact_then_prefix_then_substring_then_flex() {
        let source = ListSource::new(
            ["xfoo", "f-o-o", "foobar", "Foo", "barfoo"]
                .into_iter()
                .map(Candidate::new)
                .collect(),
        );
        assert_eq!(
            texts(&source.candidates(b"foo")),
            ["Foo", "foobar", "xfoo", "barfoo", "f-o-o"]
        );
        assert_eq!(texts(&source.candidates(b"FOB")), ["foobar"]);
        assert!(source.candidates(b"zz").is_empty());
        assert_eq!(source.candidates(b"").len(), 5, "empty input lists all");
    }

    #[test]
    fn invalid_utf8_matches_itself() {
        let source = ListSource::new(vec![Candidate::new(b"caf\xe9.txt".to_vec())]);
        assert_eq!(source.candidates(b"\xe9").len(), 1);
        assert_eq!(source.candidates(b"cf.t").len(), 1);
    }

    #[test]
    fn buffers_put_the_current_one_last() {
        let source = ListSource::buffers(["b", " *internal*", "cur", "a"], "cur");
        assert_eq!(texts(&source.candidates(b"")), ["b", "a", "cur"]);
    }

    #[test]
    fn commands_sort_shortest_first_with_history_in_front() {
        let source = ListSource::commands(
            ["undo", "yank", "forward-char", "kill-line"],
            &[b"kill-line".to_vec()],
        );
        assert_eq!(
            texts(&source.candidates(b"")),
            ["kill-line", "undo", "yank", "forward-char"]
        );
    }

    #[test]
    fn narrowing_matches_what_a_fresh_filter_gives() {
        let entries: Vec<Candidate> = (0..500)
            .map(|number| Candidate::new(format!("file-{number:03}.txt")))
            .collect();
        let mut completer =
            Completer::new(Kind::Command, Arc::from(entries.clone()), None, false, 10);
        let source = ListSource::new(entries);
        let mut input = Vec::new();
        for byte in b"f12t" {
            input.push(*byte);
            completer.update(&input);
            let fresh = source.candidates(&input);
            let (shown, _) = completer.view(usize::MAX);
            assert_eq!(shown.len(), fresh.len());
            assert!(shown.iter().zip(&fresh).all(|(a, b)| *a == b));
        }
    }

    #[test]
    fn file_source_completes_only_from_the_input_directory() {
        let mut source = FileSource::new(PathBuf::from("/work"));
        assert!(
            source.candidates(b"/work/a").is_empty(),
            "nothing listed yet"
        );
        source.set_listing(
            PathBuf::from("/work"),
            vec![Candidate::new("abc"), Candidate::new("sub/")],
        );
        assert_eq!(texts(&source.candidates(b"/work/a")), ["abc"]);
        assert_eq!(texts(&source.candidates(b"s")), ["sub/"]);
        assert!(source.candidates(b"/other/a").is_empty());
        assert_eq!(source.directory_of(b"/work//etc/x"), PathBuf::from("/etc/"));
    }

    #[test]
    fn kill_display_follows_read_from_kill_ring() {
        assert_eq!(kill_display(b"  \n  text", 80), "\u{2026}text".as_bytes());
        let long = "x".repeat(85);
        let mut expected = "x".repeat(80);
        expected.push('\u{2026}');
        assert_eq!(kill_display(long.as_bytes(), 80), expected.as_bytes());
        let exact = "y".repeat(80);
        assert_eq!(kill_display(exact.as_bytes(), 80), exact.as_bytes());
        assert_eq!(kill_display(b"a\nb", 80), b"a\nb");
    }

    #[test]
    fn scrolling_keeps_the_selection_shown() {
        assert_eq!(first_shown(0, 9, 10), 0);
        assert_eq!(first_shown(0, 10, 10), 1);
        assert_eq!(first_shown(5, 3, 10), 3);
    }
}
