//! Incremental search (Emacs isearch.el) and the searches the editor runs
//! for isearch, query-replace and occur. A search over text the editor
//! thread may read without I/O, and not too much of it, runs at once;
//! anything larger runs on a worker (`Effect::StartSearch`) while the echo
//! area says `[searching...]`. Keys typed meanwhile wait and are handled
//! when the result arrives, so typing ahead behaves as if the search had
//! been instant; `C-g` cancels the search.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use emx_text::search::{CompiledPattern, Match, Pattern, PatternError, match_at_cached};
use emx_text::{Original, Text};
use emx_types::{Key, KeyCode};

use super::Editor;
use super::Purpose;
use super::occurring::OccurRequest;
use super::replacing::ReplaceStep;
use crate::buffer::BufferId;
use crate::command::{
    self, CommandContext, CommandError, PrefixArg, char_after, char_before, is_word, scan_words,
};
use crate::event::{ClientId, Effect, SearchId};
use crate::isearch::{
    AfterSearch, CaseFold, Frame, Isearch, MATCH_TRUNCATED, PrefixParts, QueuedInput,
    SEARCH_WHITESPACE_REGEXP, SEARCHING, Suspended, add_to_ring, describe, downcase,
    ends_in_backslash, message_prefix, no_upper_case,
};
use crate::marker::{InsertionType, MarkerId};
use crate::minibuffer::{HistoryKind, Minibuffer, Mode, quoted_character};
use crate::search_job::{SearchJob, SearchOutcome, SearchRequest, run_search};
use crate::window::WindowId;

const MARK_SAVED: &str = "Mark saved where search started";
const NO_PREVIOUS_SEARCH_STRING: &str = "No previous search string";
const INCOMPLETE_INPUT: &str = "incomplete input";
const CASE_INSENSITIVE: &str = "case insensitive";
const CASE_SENSITIVE: &str = "case sensitive";
const SPACES_LOOSELY: &str = "match spaces loosely";
const SPACES_LITERALLY: &str = "match spaces literally";
/// Commands that keep isearch going when typed during it, as
/// `isearch-allow-prefix` lets prefix arguments through.
const PREFIX_ARGUMENT_COMMANDS: [&str; 4] = [
    "universal-argument",
    "universal-argument-more",
    "digit-argument",
    "negative-argument",
];
/// `isearch-yank-pop-only` pops only after these.
const ISEARCH_YANKS: [&str; 3] = [
    "isearch-yank-kill",
    "isearch-yank-pop",
    "isearch-yank-pop-only",
];
const TAB_CHARACTER: char = '\t';

/// A search running on a worker for one client, and what it is for.
#[derive(Debug)]
pub(super) struct SearchWait {
    pub id: SearchId,
    pub cancel: Arc<AtomicBool>,
    /// The text searched, to tell whether the buffer changed meanwhile.
    pub text: Text,
    pub owner: SearchOwner,
    pub queued: VecDeque<QueuedInput>,
}

#[derive(Debug, Clone)]
pub(super) enum SearchOwner {
    Isearch(AfterSearch),
    QueryReplace(ReplaceStep),
    Occur(OccurRequest),
}

impl SearchOwner {
    /// Isearch and query-replace read keys as their own; a key typed while
    /// their search runs waits for it. Occur does not hold keys back.
    fn holds_input(&self) -> bool {
        !matches!(self, SearchOwner::Occur(_))
    }
}

/// The character an isearch key adds to the string.
fn key_character(key: Key) -> Option<char> {
    let mods = key.mods;
    match key.code {
        KeyCode::Char(character) if !mods.ctrl && !mods.meta && !mods.super_ => Some(character),
        KeyCode::Char('j') if mods.ctrl && !mods.meta => Some('\n'),
        KeyCode::Tab if !mods.ctrl && !mods.meta => Some(TAB_CHARACTER),
        _ => None,
    }
}

/// The Emacs pattern of an isearch string.
pub(super) fn isearch_pattern(
    string: &[u8],
    regexp: bool,
    lax_whitespace: bool,
    case_fold: bool,
) -> Pattern {
    if regexp {
        return Pattern::Regex {
            emacs: String::from_utf8_lossy(string).into_owned(),
            case_fold,
        };
    }
    match std::str::from_utf8(string) {
        // Lax whitespace: each run of spaces matches spaces and tabs.
        Ok(text) if lax_whitespace && text.contains(' ') => {
            let mut emacs = String::new();
            for (index, run) in text.split(' ').enumerate() {
                if index > 0 && !emacs.ends_with(SEARCH_WHITESPACE_REGEXP) {
                    emacs.push_str(SEARCH_WHITESPACE_REGEXP);
                }
                emacs.push_str(&emx_text::search::regexp_quote(run));
            }
            Pattern::Regex { emacs, case_fold }
        }
        _ => Pattern::Literal {
            needle: string.to_vec(),
            case_fold,
        },
    }
}

/// The message isearch shows for a pattern error: unfinished regexps are
/// `incomplete input`, as in Emacs.
pub(super) fn pattern_error_message(error: &PatternError) -> String {
    if error.is_incomplete() {
        INCOMPLETE_INPUT.to_string()
    } else {
        error.to_string()
    }
}

impl Editor {
    // ---- Searching for isearch, query-replace and occur -----------------

    /// The editor thread may search `text` itself: it is in memory and no
    /// larger than `in_memory_max`.
    pub(super) fn searches_inline(&self, text: &Text) -> bool {
        matches!(text.sources().original, Original::Memory(_))
            && text.len() <= self.config.in_memory_max
    }

    pub(crate) fn match_cap(&self) -> usize {
        usize::try_from(self.config.search_match_cap).expect("search_match_cap is validated")
    }

    /// Runs `request` now when the text allows, returning its result;
    /// otherwise starts it on a worker for `owner` and returns `None`.
    pub(super) fn start_search(
        &mut self,
        client: ClientId,
        buffer: BufferId,
        pattern: CompiledPattern,
        request: SearchRequest,
        owner: SearchOwner,
    ) -> Option<Result<SearchOutcome, String>> {
        let text = self.buffers[buffer].text.clone();
        let match_cap = self.match_cap();
        if self.searches_inline(&text) {
            let never = AtomicBool::new(false);
            return Some(
                run_search(&text, &pattern, &request, &never, match_cap)
                    .map_err(|error| error.to_string()),
            );
        }
        let queued = self
            .cancel_search(client)
            .map(|wait| wait.queued)
            .unwrap_or_default();
        let id = SearchId(self.next_search);
        self.next_search += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        self.effects.push(Effect::StartSearch(SearchJob {
            client,
            id,
            text: text.clone(),
            pattern,
            request,
            match_cap,
            cancel: cancel.clone(),
        }));
        self.client_mut(client).search_wait = Some(SearchWait {
            id,
            cancel,
            text,
            owner,
            queued,
        });
        None
    }

    /// Stops the client's search on a worker, if any.
    pub(super) fn cancel_search(&mut self, client: ClientId) -> Option<SearchWait> {
        let wait = self.clients.get_mut(&client)?.search_wait.take()?;
        wait.cancel.store(true, Ordering::Relaxed);
        Some(wait)
    }

    /// A key or paste arrived while the client waits for a search that
    /// holds input back: queue it, or for `C-g` stop the search. Returns
    /// whether the input was taken.
    pub(super) fn hold_input_for_search(&mut self, client: ClientId, input: QueuedInput) -> bool {
        let quit = input == QueuedInput::Key(command::QUIT_KEY);
        let Some(wait) = self.client_mut(client).search_wait.as_mut() else {
            return false;
        };
        if !wait.owner.holds_input() {
            return false;
        }
        if !quit {
            wait.queued.push_back(input);
            return true;
        }
        let wait = self
            .cancel_search(client)
            .expect("the client waits for a search");
        // As in Emacs, quitting a search makes it fail; the `C-g` is then
        // read again by whoever searched.
        match wait.owner {
            SearchOwner::Isearch(then) => {
                self.isearch_search_finished(client, Some(Ok(None)), then);
                false
            }
            SearchOwner::QueryReplace(_) => {
                self.replace_search_quit(client);
                true
            }
            SearchOwner::Occur(_) => unreachable!("occur does not hold input"),
        }
    }

    /// `Event::SearchResult`: the result of the client's search, unless it
    /// was superseded or the client left.
    pub(super) fn search_result(
        &mut self,
        client: ClientId,
        id: SearchId,
        result: Result<SearchOutcome, String>,
    ) {
        let Some(state) = self.clients.get_mut(&client) else {
            return;
        };
        if state.search_wait.as_ref().is_none_or(|wait| wait.id != id) {
            return;
        }
        if matches!(result, Ok(SearchOutcome::Cancelled)) {
            return;
        }
        let wait = state.search_wait.take().expect("checked above");
        match wait.owner.clone() {
            SearchOwner::Isearch(then) => self.isearch_search_arrived(client, wait, result, then),
            SearchOwner::QueryReplace(step) => {
                self.replace_search_arrived(client, wait, result, step)
            }
            SearchOwner::Occur(request) => self.occur_search_arrived(client, wait, result, request),
        }
    }

    /// Handles input that waited for a search, until it is all handled or
    /// another search makes the rest wait again.
    pub(super) fn replay_queued(&mut self, client: ClientId, mut queued: VecDeque<QueuedInput>) {
        while let Some(input) = queued.pop_front() {
            if !self.clients.contains_key(&client) {
                return;
            }
            if let Some(wait) = self.client_mut(client).search_wait.as_mut()
                && wait.owner.holds_input()
            {
                wait.queued.push_back(input);
                wait.queued.extend(queued);
                return;
            }
            match input {
                QueuedInput::Key(key) => self.handle_key(client, key),
                QueuedInput::Paste(bytes) => self.handle_paste(client, bytes),
            }
        }
    }

    /// The text changed while the worker searched an older version:
    /// whether the result can be used.
    pub(super) fn search_is_current(&self, buffer: BufferId, wait: &SearchWait) -> bool {
        self.buffers
            .get(buffer)
            .is_some_and(|target| target.text.same_root(&wait.text))
    }

    // ---- Isearch state --------------------------------------------------

    pub(crate) fn isearch_active(&self, client: ClientId) -> bool {
        self.client(client).isearch.is_some()
    }

    fn isearch(&self, client: ClientId) -> &Isearch {
        self.client(client)
            .isearch
            .as_ref()
            .expect("isearch commands run during an isearch")
    }

    fn isearch_mut(&mut self, client: ClientId) -> &mut Isearch {
        self.client_mut(client)
            .isearch
            .as_mut()
            .expect("isearch commands run during an isearch")
    }

    fn isearch_point(&self, client: ClientId) -> u64 {
        self.window_point(client, self.isearch(client).window)
    }

    fn isearch_goto(&mut self, client: ClientId, position: u64) {
        let window = self.isearch(client).window;
        self.set_window_point(client, window, position);
    }

    fn isearch_position(&self, client: ClientId, marker: MarkerId) -> u64 {
        self.buffers[self.isearch(client).buffer].marker_pos(marker)
    }

    fn isearch_other_end(&self, client: ClientId) -> Option<u64> {
        self.isearch(client)
            .other_end
            .map(|marker| self.isearch_position(client, marker))
    }

    fn new_marker(&mut self, buffer: BufferId, position: u64) -> MarkerId {
        self.buffers[buffer].create_marker_without_undo(position, InsertionType::StayBefore)
    }

    fn set_isearch_other_end(&mut self, client: ClientId, position: Option<u64>) {
        let isearch = self.isearch(client);
        let (buffer, marker) = (isearch.buffer, isearch.other_end);
        let marker = match (marker, position) {
            (Some(marker), Some(position)) => {
                self.buffers[buffer].set_marker(marker, position);
                Some(marker)
            }
            (None, Some(position)) => Some(self.new_marker(buffer, position)),
            (Some(marker), None) => {
                self.buffers[buffer].delete_marker(marker);
                None
            }
            (None, None) => None,
        };
        self.isearch_mut(client).other_end = marker;
    }

    fn set_isearch_barrier(&mut self, client: ClientId, position: u64) {
        let isearch = self.isearch(client);
        let (buffer, marker) = (isearch.buffer, isearch.barrier);
        self.buffers[buffer].set_marker(marker, position);
    }

    /// `isearch-push-state`.
    fn isearch_push_state(&mut self, client: ClientId) {
        let point = self.isearch_point(client);
        let other_end = self.isearch_other_end(client);
        let isearch = self.isearch(client);
        let buffer = isearch.buffer;
        let barrier = self.isearch_position(client, isearch.barrier);
        let (string, success, forward, error, wrapped, case_fold) = (
            isearch.string.clone(),
            isearch.success,
            isearch.forward,
            isearch.error.clone(),
            isearch.wrapped,
            isearch.case_fold,
        );
        let frame = Frame {
            string,
            point: self.new_marker(buffer, point),
            success,
            forward,
            other_end: other_end.map(|position| self.new_marker(buffer, position)),
            error,
            wrapped,
            barrier: self.new_marker(buffer, barrier),
            case_fold,
        };
        self.isearch_mut(client).cmds.push(frame);
    }

    /// Whether the top state equals the current one (`isearch-edit-string`
    /// pushes only a changed state).
    fn isearch_state_unchanged(&self, client: ClientId) -> bool {
        let isearch = self.isearch(client);
        let Some(top) = isearch.cmds.last() else {
            return false;
        };
        let buffer = &self.buffers[isearch.buffer];
        top.string == isearch.string
            && buffer.marker_pos(top.point) == self.isearch_point(client)
            && top.success == isearch.success
            && top.forward == isearch.forward
            && top.other_end.map(|marker| buffer.marker_pos(marker))
                == self.isearch_other_end(client)
            && top.error == isearch.error
            && top.wrapped == isearch.wrapped
            && buffer.marker_pos(top.barrier) == buffer.marker_pos(isearch.barrier)
            && top.case_fold == isearch.case_fold
    }

    /// `isearch--set-state` from the top of the stack.
    fn isearch_restore_top(&mut self, client: ClientId) {
        let isearch = self.isearch(client);
        let top = isearch
            .cmds
            .last()
            .expect("the stack keeps its first state")
            .clone();
        let buffer = &self.buffers[isearch.buffer];
        let point = buffer.marker_pos(top.point);
        let barrier = buffer.marker_pos(top.barrier);
        let other_end = top.other_end.map(|marker| buffer.marker_pos(marker));
        {
            let isearch = self.isearch_mut(client);
            isearch.string = top.string;
            isearch.success = top.success;
            isearch.forward = top.forward;
            isearch.error = top.error;
            isearch.wrapped = top.wrapped;
            isearch.case_fold = top.case_fold;
        }
        self.set_isearch_other_end(client, other_end);
        self.set_isearch_barrier(client, barrier);
        self.isearch_goto(client, point);
    }

    /// `isearch-pop-state`.
    fn isearch_pop_state(&mut self, client: ClientId) {
        let isearch = self.isearch_mut(client);
        let buffer = isearch.buffer;
        let frame = isearch
            .cmds
            .pop()
            .expect("only states above the first are popped");
        frame.release(&mut self.buffers[buffer]);
        self.isearch_restore_top(client);
    }

    /// Drops the states above `depth`.
    fn isearch_truncate_states(&mut self, client: ClientId, depth: usize) {
        let isearch = self.isearch_mut(client);
        let buffer = isearch.buffer;
        let dropped: Vec<Frame> = isearch.cmds.drain(depth..).collect();
        for frame in dropped {
            frame.release(&mut self.buffers[buffer]);
        }
    }

    /// The echo-area line of the isearch (`isearch-message`).
    fn isearch_message(&self, client: ClientId) -> String {
        let isearch = self.isearch(client);
        let point = self.isearch_point(client);
        let opoint = self.isearch_position(client, isearch.opoint);
        let parts = PrefixParts {
            success: isearch.success,
            adjusted: isearch.adjusted,
            overwrapped: isearch.wrapped
                && if isearch.forward {
                    point > opoint
                } else {
                    point < opoint
                },
            wrapped: isearch.wrapped,
            case_sensitive_failure: !isearch.success && !isearch.case_fold.folds(),
            regexp: isearch.regexp,
            nonincremental: isearch.nonincremental,
            forward: isearch.forward,
        };
        let mut message = message_prefix(&parts);
        message.push_str(&describe(&isearch.string));
        if let Some(error) = &isearch.error {
            message.push_str(&format!(" [{error}]"));
        }
        if isearch.capped {
            message.push_str(&format!(" [{MATCH_TRUNCATED}]"));
        }
        if let Some(momentary) = &isearch.momentary {
            message.push_str(&format!(" [{momentary}]"));
        }
        if self
            .client(client)
            .search_wait
            .as_ref()
            .is_some_and(|wait| matches!(wait.owner, SearchOwner::Isearch(_)))
        {
            message.push_str(&format!(" [{SEARCHING}]"));
        }
        message
    }

    /// `isearch-update`: shows the message; `isearch-adjusted` and the yank
    /// flag last one command. The message is not logged, as isearch binds
    /// `message-log-max` to nil.
    fn isearch_update(&mut self, client: ClientId) {
        let message = self.isearch_message(client);
        self.client_mut(client).echo = message;
        let isearch = self.isearch_mut(client);
        isearch.adjusted = false;
        isearch.yank_flag = false;
    }

    // ---- Starting and ending --------------------------------------------

    /// `isearch-mode`: starts an incremental search in the selected window.
    pub(crate) fn isearch_start(&mut self, client: ClientId, forward: bool, regexp: bool) {
        let window = self.selected_window_id(client);
        let buffer = self.window(client, window).buffer;
        let point = self.window_point(client, window);
        let barrier = self.new_marker(buffer, point);
        let opoint = self.new_marker(buffer, point);
        let last_case_fold = self.isearch_case_fold;
        self.client_mut(client).isearch = Some(Isearch {
            buffer,
            window,
            forward,
            regexp,
            lax_whitespace: true,
            string: Vec::new(),
            success: true,
            wrapped: false,
            other_end: None,
            barrier,
            opoint,
            case_fold: CaseFold::Smart,
            last_case_fold,
            error: None,
            adjusted: false,
            yank_flag: false,
            just_started: true,
            nonincremental: false,
            yank_previous_point: None,
            ring_pointer: None,
            quote_next: false,
            capped: false,
            pattern: None,
            cmds: Vec::new(),
            momentary: None,
            suspended: None,
        });
        self.isearch_push_state(client);
        self.isearch_update(client);
    }

    /// `isearch-done`: ends the search. The string goes on the search ring
    /// unless `nopush`; when point moved, the mark is pushed where the
    /// search started (silently when `edit`).
    pub(crate) fn isearch_done(&mut self, client: ClientId, nopush: bool, edit: bool) {
        if let Some(wait) = self.client(client).search_wait.as_ref()
            && matches!(wait.owner, SearchOwner::Isearch(_))
        {
            self.cancel_search(client);
        }
        let point = self.isearch_point(client);
        let isearch = self
            .client_mut(client)
            .isearch
            .take()
            .expect("an isearch is ending");
        let buffer = isearch.buffer;
        let opoint = self.buffers[buffer].marker_pos(isearch.opoint);
        self.isearch_case_fold = isearch.case_fold;
        if !isearch.string.is_empty() && !nopush {
            let ring = if isearch.regexp {
                &mut self.regexp_search_ring
            } else {
                &mut self.search_ring
            };
            add_to_ring(ring, &isearch.string);
        }
        isearch.release(&mut self.buffers[buffer]);
        if point != opoint && !self.buffers[buffer].mark_active {
            let mut ctx =
                CommandContext::new(self, client, "isearch-done", None, PrefixArg::None, None);
            command::push_mark(&mut ctx, opoint, true, false);
            if !edit {
                ctx.message(MARK_SAVED);
            }
        }
    }

    /// The isearch ends without a trace: its buffer was killed.
    pub(super) fn isearch_forget_buffer(&mut self, buffer: BufferId) {
        let clients: Vec<ClientId> = self.clients.keys().copied().collect();
        for client in clients {
            if self
                .client(client)
                .isearch
                .as_ref()
                .is_some_and(|isearch| isearch.buffer == buffer)
            {
                if let Some(wait) = self.client(client).search_wait.as_ref()
                    && matches!(wait.owner, SearchOwner::Isearch(_))
                {
                    self.cancel_search(client);
                }
                // The markers go with the buffer.
                self.client_mut(client).isearch = None;
                self.client_mut(client).echo.clear();
            }
        }
    }

    /// Releases a detaching client's isearch markers.
    pub(super) fn isearch_detach(&mut self, isearch: Isearch) {
        if let Some(buffer) = self.buffers.get_mut(isearch.buffer) {
            isearch.release(buffer);
        }
    }

    // ---- Keys -----------------------------------------------------------

    /// A key while the client isearches: isearch's own keys run isearch
    /// commands, prefix arguments pass through, and any other key ends the
    /// search and then runs as usual (`search-exit-option`).
    pub(super) fn isearch_key(&mut self, client: ClientId, key: Key) {
        if self.isearch(client).quote_next {
            self.isearch_mut(client).quote_next = false;
            if let Some(character) = quoted_character(key) {
                self.run_command(client, "isearch-quote-char", Some(key), move |ctx| {
                    let client = ctx.client;
                    let count = ctx.prefix_arg.numeric_value();
                    ctx.editor().isearch_process_char(client, character, count);
                    Ok(())
                });
            } else {
                self.ring_bell(client);
                self.isearch_update(client);
            }
            return;
        }
        self.isearch_mut(client).momentary = None;
        let state = &mut self.client_mut(client).command_loop;
        state.pending.push(key);
        let pending = state.pending.clone();
        match self.isearch_keymap.lookup(&pending) {
            crate::keymap::Lookup::Command(name) => {
                let spec = command::isearch_command(name)
                    .expect("the isearch keymap binds only isearch commands");
                self.run_command(client, spec.name, Some(key), spec.run);
                return;
            }
            crate::keymap::Lookup::Prefix => {
                self.continue_sequence(client);
                return;
            }
            crate::keymap::Lookup::Undefined => {}
        }
        match self.lookup_keys(client, &pending) {
            crate::keymap::Lookup::Command(name) if PREFIX_ARGUMENT_COMMANDS.contains(&name) => {
                self.run_key_command(client, name, key);
            }
            crate::keymap::Lookup::Prefix => self.continue_sequence(client),
            _ => {
                self.isearch_done(client, false, false);
                self.dispatch_pending(client, key);
            }
        }
    }

    /// Bracketed paste during isearch adds the text to the string
    /// (`isearch-xterm-paste`).
    pub(super) fn isearch_paste(&mut self, client: ClientId, bytes: Vec<u8>) {
        self.run_command(client, "isearch-xterm-paste", None, move |ctx| {
            let client = ctx.client;
            ctx.editor().isearch_yank_string(client, bytes);
            Ok(())
        });
    }

    // ---- The search -----------------------------------------------------

    /// The compiled pattern of the current string.
    fn isearch_compile(&self, client: ClientId) -> Result<CompiledPattern, PatternError> {
        let isearch = self.isearch(client);
        CompiledPattern::compile(&isearch_pattern(
            &isearch.string,
            isearch.regexp,
            isearch.lax_whitespace,
            isearch.case_fold.folds(),
        ))
    }

    /// `search-upper-case`: an upper-case letter in the string makes a
    /// `t` folding search case-sensitive.
    fn isearch_adjust_case_fold(&mut self, client: ClientId) {
        let isearch = self.isearch_mut(client);
        if isearch.case_fold == CaseFold::Smart
            && !no_upper_case(&String::from_utf8_lossy(&isearch.string), isearch.regexp)
        {
            isearch.case_fold = CaseFold::Never;
        }
    }

    /// `isearch-search`: searches from point for the string, then `then`.
    fn isearch_search(&mut self, client: ClientId, then: AfterSearch) {
        self.isearch_adjust_case_fold(client);
        {
            let isearch = self.isearch_mut(client);
            isearch.error = None;
            isearch.capped = false;
        }
        let pattern = match self.isearch_compile(client) {
            Ok(pattern) => pattern,
            Err(error) => {
                let isearch = self.isearch_mut(client);
                isearch.error = Some(pattern_error_message(&error));
                isearch.pattern = None;
                self.isearch_search_finished(client, None, then);
                return;
            }
        };
        self.isearch_mut(client).pattern = Some(pattern.clone());
        let point = self.isearch_point(client);
        let isearch = self.isearch(client);
        let request = if isearch.forward {
            SearchRequest::Forward {
                from: point,
                bound: None,
            }
        } else {
            SearchRequest::Backward {
                from: point,
                bound: None,
            }
        };
        let buffer = isearch.buffer;
        match self.start_search(client, buffer, pattern, request, SearchOwner::Isearch(then)) {
            Some(result) => self.isearch_search_finished(client, Some(match_of(result)), then),
            None => self.isearch_update(client),
        }
    }

    /// A worker's search for the isearch arrived.
    fn isearch_search_arrived(
        &mut self,
        client: ClientId,
        wait: SearchWait,
        result: Result<SearchOutcome, String>,
        then: AfterSearch,
    ) {
        let Some(isearch) = self.client(client).isearch.as_ref() else {
            return;
        };
        let buffer = isearch.buffer;
        let queued = wait.queued.clone();
        if !self.search_is_current(buffer, &wait) {
            // Another client changed the text: search the new one, keeping
            // what was typed meanwhile.
            self.isearch_search(client, then);
            self.requeue(client, queued);
            return;
        }
        self.isearch_search_finished(client, Some(match_of(result)), then);
        self.replay_queued(client, queued);
    }

    /// Puts input back in front of the client's new search, or handles it
    /// when the search finished at once.
    fn requeue(&mut self, client: ClientId, queued: VecDeque<QueuedInput>) {
        match self.client_mut(client).search_wait.as_mut() {
            Some(wait) => {
                let mut all = queued;
                all.extend(wait.queued.drain(..));
                wait.queued = all;
            }
            None => self.replay_queued(client, queued),
        }
    }

    /// The rest of `isearch-search` once the search is done (`None` when no
    /// search ran because the pattern is invalid).
    fn isearch_search_finished(
        &mut self,
        client: ClientId,
        result: Option<Result<Option<Match>, String>>,
        then: AfterSearch,
    ) {
        match result {
            Some(Ok(found)) => {
                let forward = self.isearch(client).forward;
                {
                    let isearch = self.isearch_mut(client);
                    isearch.success = found.is_some();
                    isearch.just_started = false;
                }
                if let Some(found) = found {
                    let (point, other_end) = if forward {
                        (found.end, found.start)
                    } else {
                        (found.start, found.end)
                    };
                    self.isearch_goto(client, point);
                    self.set_isearch_other_end(client, Some(other_end));
                    self.isearch_mut(client).capped = found.capped;
                }
            }
            Some(Err(error)) => {
                let isearch = self.isearch_mut(client);
                isearch.success = false;
                isearch.just_started = false;
                isearch.error = Some(error);
            }
            None => {}
        }
        if !self.isearch(client).success {
            let isearch = self.isearch(client);
            let top = isearch
                .cmds
                .last()
                .expect("the stack keeps its first state");
            let (top_success, top_point) = (
                top.success,
                self.buffers[isearch.buffer].marker_pos(top.point),
            );
            // Ding only when this search failed after the last succeeded.
            if top_success {
                self.ring_bell(client);
            }
            self.isearch_goto(client, top_point);
        }
        self.isearch_after_search(client, then);
    }

    fn isearch_after_search(&mut self, client: ClientId, then: AfterSearch) {
        match then {
            AfterSearch::PushAndUpdate => {
                self.isearch_push_state(client);
                self.isearch_update(client);
            }
            AfterSearch::Repeat {
                remaining,
                depth,
                then_direction,
            } => {
                let isearch = self.isearch(client);
                if remaining > 0 && isearch.success && isearch.error.is_none() {
                    // Kept until the repeat ends, so a later failure goes
                    // back to this match.
                    self.isearch_push_state(client);
                    self.isearch_repeat_step(client, remaining - 1, depth, then_direction);
                } else {
                    self.isearch_finish_repeat(client, depth, then_direction);
                }
            }
            AfterSearch::Edited { nonincremental } => {
                if !self.isearch_state_unchanged(client) {
                    self.isearch_push_state(client);
                }
                self.isearch_update(client);
                if nonincremental {
                    let empty = self.isearch(client).string.is_empty();
                    self.isearch_done(client, false, false);
                    // The usual message would be confusing for an empty
                    // string.
                    if empty {
                        self.client_mut(client).echo.clear();
                    }
                }
            }
        }
    }

    /// `isearch-search-and-update`, after the string grew.
    fn isearch_search_and_update(&mut self, client: ClientId) {
        let isearch = self.isearch(client);
        if !(isearch.success || isearch.regexp) {
            self.isearch_push_state(client);
            self.isearch_update(client);
            return;
        }
        if !isearch.forward && !isearch.adjusted && self.isearch_still_matches_backward(client) {
            self.isearch_adjust_case_fold(client);
            self.isearch_push_state(client);
            self.isearch_update(client);
            return;
        }
        let isearch = self.isearch(client);
        if let Some(other_end) = self.isearch_other_end(client)
            && !isearch.adjusted
        {
            let target = if isearch.forward {
                other_end
            } else {
                let opoint = self.isearch_position(client, isearch.opoint);
                let barrier = self.isearch_position(client, isearch.barrier);
                let text = &self.buffers[isearch.buffer].text;
                let after = match char_after(text, other_end) {
                    Ok(Some((_, length))) => other_end + length,
                    _ => other_end,
                };
                opoint.min(barrier).min(after)
            };
            self.isearch_goto(client, target);
        }
        self.isearch_search(client, AfterSearch::PushAndUpdate);
    }

    /// In a backward search, a longer string may still match at point
    /// without moving, as long as the match does not extend past where the
    /// search started. Only cached text is looked at: the editor thread
    /// does not wait for I/O, and point's text is on screen.
    fn isearch_still_matches_backward(&mut self, client: ClientId) -> bool {
        let isearch = self.isearch(client);
        let mut case_fold = isearch.case_fold;
        if case_fold == CaseFold::Smart
            && !no_upper_case(&String::from_utf8_lossy(&isearch.string), isearch.regexp)
        {
            case_fold = CaseFold::Never;
        }
        let Ok(pattern) = CompiledPattern::compile(&isearch_pattern(
            &isearch.string,
            isearch.regexp,
            isearch.lax_whitespace,
            case_fold.folds(),
        )) else {
            return false;
        };
        let point = self.isearch_point(client);
        let text = &self.buffers[isearch.buffer].text;
        let Ok(Some(found)) = match_at_cached(text, &pattern, point, None, self.match_cap()) else {
            return false;
        };
        let opoint = self.isearch_position(client, isearch.opoint);
        let barrier = self.isearch_position(client, isearch.barrier);
        if !(isearch.yank_flag || found.end <= opoint.min(barrier)) {
            return false;
        }
        {
            let isearch = self.isearch_mut(client);
            isearch.success = true;
            isearch.error = None;
            isearch.pattern = Some(pattern);
        }
        self.set_isearch_other_end(client, Some(found.end));
        true
    }

    // ---- Commands -------------------------------------------------------

    /// `isearch-printing-char` and `isearch-process-search-char`.
    pub(crate) fn isearch_printing_char(&mut self, client: ClientId, key: Option<Key>, count: i64) {
        let Some(character) = key.and_then(key_character) else {
            self.ring_bell(client);
            return;
        };
        self.isearch_process_char(client, character, count);
    }

    fn isearch_process_char(&mut self, client: ClientId, character: char, count: i64) {
        // `*` and `?` are special in regexps unless after `\`; `}` and `|`
        // only after it.
        match character {
            '*' | '?' => self.isearch_fallback(client, false, false, false),
            '}' => self.isearch_fallback(client, true, true, false),
            '|' => self.isearch_fallback(client, true, false, true),
            _ => {}
        }
        let mut encoded = [0; 4];
        let encoded = character.encode_utf8(&mut encoded).as_bytes();
        let repeats = usize::try_from(count)
            .ok()
            .filter(|count| *count > 1)
            .unwrap_or(1);
        let added: Vec<u8> = encoded.repeat(repeats);
        self.isearch_mut(client).string.extend_from_slice(&added);
        self.isearch_search_and_update(client);
    }

    /// `isearch-fallback`: a character that makes the last regexp item
    /// optional goes back to an earlier match, so the search can find
    /// matches the shorter regexp skipped.
    fn isearch_fallback(
        &mut self,
        client: ClientId,
        want_backslash: bool,
        allow_invalid: bool,
        to_barrier: bool,
    ) {
        let isearch = self.isearch(client);
        if !isearch.regexp || ends_in_backslash(&isearch.string) != want_backslash {
            return;
        }
        let below_has_error = isearch
            .cmds
            .len()
            .checked_sub(2)
            .and_then(|index| isearch.cmds.get(index))
            .is_some_and(|frame| frame.error.is_some());
        if isearch.error.is_some() && below_has_error && !allow_invalid {
            return;
        }
        let barrier = self.isearch_position(client, isearch.barrier);
        if to_barrier {
            self.isearch_goto(client, barrier);
            self.isearch_mut(client).adjusted = true;
            return;
        }
        // Walk down to the last valid regexp whose last item is not
        // optional, then go to where the match before it was.
        let frames = &isearch.cmds;
        let mut index = frames.len() - 1;
        while index > 0 {
            let frame = &frames[index];
            let below = &frames[index - 1];
            let optional_last = match frame.string.last() {
                Some(last) if ends_in_backslash(&below.string) => *last == b'}',
                Some(last) => matches!(last, b'*' | b'?' | b'+'),
                None => false,
            };
            if !(frame.error.is_some() || optional_last) {
                break;
            }
            index -= 1;
        }
        let last_other_end = index
            .checked_sub(1)
            .and_then(|below| frames[below].other_end)
            .map_or(barrier, |marker| self.isearch_position(client, marker));
        let target = if isearch.forward {
            last_other_end.max(barrier)
        } else {
            last_other_end.min(barrier)
        };
        self.isearch_goto(client, target);
        self.isearch_mut(client).adjusted = true;
    }

    /// `isearch-repeat-forward` / `-backward` with the prefix argument.
    pub(crate) fn isearch_repeat_command(
        &mut self,
        client: ClientId,
        forward: bool,
        prefix_arg: PrefixArg,
    ) {
        if prefix_arg == PrefixArg::None {
            self.isearch_repeat(client, forward, 1, None);
            return;
        }
        let count = prefix_arg.numeric_value();
        if count < 0 {
            // Repeat the other way, then turn back to this direction.
            self.isearch_repeat(client, !forward, count.unsigned_abs(), Some(forward));
            return;
        }
        let mut count = count.unsigned_abs();
        // One more search to come back to the current match first.
        if self.isearch(client).forward != forward {
            count += 1;
        }
        self.isearch_repeat(client, forward, count, None);
    }

    /// `isearch-repeat`: searches `count` times in `forward`'s direction;
    /// with a string still empty, the last search string is used. A failing
    /// search wraps around. `then_direction` repeats once more that way
    /// afterwards.
    fn isearch_repeat(
        &mut self,
        client: ClientId,
        forward: bool,
        count: u64,
        then_direction: Option<bool>,
    ) {
        let isearch = self.isearch(client);
        if isearch.forward == forward {
            if isearch.string.is_empty() {
                let ring = if isearch.regexp {
                    &self.regexp_search_ring
                } else {
                    &self.search_ring
                };
                match ring.front().cloned() {
                    None => {
                        self.isearch_mut(client).error = Some(NO_PREVIOUS_SEARCH_STRING.into());
                    }
                    Some(string) => {
                        let isearch = self.isearch_mut(client);
                        isearch.string = string;
                        isearch.case_fold = isearch.last_case_fold;
                        self.isearch_ring_adjust1(client, false);
                    }
                }
            } else if !isearch.success {
                let target = if forward {
                    0
                } else {
                    self.buffers[isearch.buffer].text.len()
                };
                self.isearch_mut(client).wrapped = true;
                self.isearch_goto(client, target);
            }
        } else {
            let isearch = self.isearch_mut(client);
            isearch.forward = !isearch.forward;
            isearch.success = true;
        }
        let point = self.isearch_point(client);
        self.set_isearch_barrier(client, point);
        if self.isearch(client).string.is_empty() {
            self.isearch_mut(client).success = true;
            self.isearch_finish_repeat(client, self.isearch(client).cmds.len(), then_direction);
            return;
        }
        let depth = self.isearch(client).cmds.len();
        if count == 0 {
            self.isearch_finish_repeat(client, depth, then_direction);
            return;
        }
        self.isearch_repeat_step(client, count - 1, depth, then_direction);
    }

    /// One search of `isearch-repeat`'s loop; `remaining` more follow.
    fn isearch_repeat_step(
        &mut self,
        client: ClientId,
        remaining: u64,
        depth: usize,
        then_direction: Option<bool>,
    ) {
        let then = AfterSearch::Repeat {
            remaining,
            depth,
            then_direction,
        };
        let isearch = self.isearch(client);
        let point = self.isearch_point(client);
        if isearch.success && self.isearch_other_end(client) == Some(point) && !isearch.just_started
        {
            // The last match was empty: move on, or fail at the edge.
            let text = &self.buffers[isearch.buffer].text;
            let step = if isearch.forward {
                char_after(text, point)
            } else {
                char_before(text, point)
            };
            match step {
                Ok(Some((_, length))) => {
                    let target = if isearch.forward {
                        point + length
                    } else {
                        point - length
                    };
                    self.isearch_goto(client, target);
                    self.isearch_search(client, then);
                }
                _ => {
                    self.isearch_mut(client).success = false;
                    self.ring_bell(client);
                    self.isearch_finish_repeat(client, depth, then_direction);
                }
            }
            return;
        }
        self.isearch_search(client, then);
    }

    /// The end of `isearch-repeat`: the states kept along the way go, and
    /// the final one is pushed.
    fn isearch_finish_repeat(
        &mut self,
        client: ClientId,
        depth: usize,
        then_direction: Option<bool>,
    ) {
        self.isearch_truncate_states(client, depth);
        self.isearch_push_state(client);
        self.isearch_update(client);
        if let Some(direction) = then_direction {
            self.isearch_repeat(client, direction, 1, None);
        }
    }

    /// `isearch-beginning-of-buffer` (`forward`) and `isearch-end-of-buffer`.
    pub(crate) fn isearch_edge_of_buffer(&mut self, client: ClientId, forward: bool, count: i64) {
        if count < 0 {
            self.isearch_edge_of_buffer(client, !forward, count.saturating_neg());
            return;
        }
        let current = self.isearch(client).forward;
        self.isearch_mut(client).just_started = true;
        let target = if forward {
            0
        } else {
            self.buffers[self.isearch(client).buffer].text.len()
        };
        self.isearch_goto(client, target);
        // Turning around costs no extra search here, unlike `C-s` after
        // `C-r`: the search starts from the edge of the buffer.
        let then_direction = (current != forward).then_some(current);
        self.isearch_repeat(client, forward, count.unsigned_abs(), then_direction);
    }

    /// `isearch-delete-char` (`DEL`): back to the previous state.
    pub(crate) fn isearch_delete_char(&mut self, client: ClientId) {
        if self.isearch(client).cmds.len() <= 1 {
            self.ring_bell(client);
        } else {
            self.isearch_pop_state(client);
        }
        self.isearch_update(client);
    }

    /// `isearch-del-char` (`C-M-d`): drops characters from the string and
    /// searches again from the current match.
    pub(crate) fn isearch_del_char(&mut self, client: ClientId, count: i64) {
        let isearch = self.isearch(client);
        if isearch.string.is_empty() {
            self.ring_bell(client);
        } else {
            let text = String::from_utf8_lossy(&isearch.string).into_owned();
            let keep = text
                .chars()
                .count()
                .saturating_sub(usize::try_from(count.max(1)).unwrap_or(usize::MAX));
            let kept: String = text.chars().take(keep).collect();
            self.isearch_mut(client).string = kept.into_bytes();
        }
        if let Some(other_end) = self.isearch_other_end(client) {
            self.isearch_goto(client, other_end);
        }
        self.isearch_search(client, AfterSearch::PushAndUpdate);
    }

    /// `isearch-abort` (`C-g`): a failing search goes back to its last
    /// success; a successful one is cancelled.
    pub(crate) fn isearch_abort(&mut self, client: ClientId) -> Result<(), CommandError> {
        let isearch = self.isearch(client);
        if isearch.success && isearch.error.is_none() {
            self.isearch_mut(client).success = false;
            return self.isearch_cancel(client);
        }
        loop {
            let isearch = self.isearch(client);
            if (isearch.success && isearch.error.is_none()) || isearch.cmds.len() <= 1 {
                break;
            }
            self.isearch_pop_state(client);
        }
        self.isearch_update(client);
        Ok(())
    }

    /// `isearch-cancel`: back to where the search started, then quit.
    pub(crate) fn isearch_cancel(&mut self, client: ClientId) -> Result<(), CommandError> {
        let opoint = self.isearch_position(client, self.isearch(client).opoint);
        self.isearch_goto(client, opoint);
        self.isearch_done(client, true, false);
        Err(CommandError::Quit)
    }

    /// `isearch-exit` (`RET`); with nothing typed yet, a nonincremental
    /// search is read in the minibuffer instead.
    pub(crate) fn isearch_exit(&mut self, client: ClientId) {
        if self.isearch(client).string.is_empty() {
            self.isearch_edit_string(client, true);
        } else {
            self.isearch_done(client, false, false);
        }
    }

    /// `C-q`: the next key is a character to search for.
    pub(crate) fn isearch_quote_char(&mut self, client: ClientId) {
        self.isearch_mut(client).quote_next = true;
        let message = format!("{}^Q", self.isearch_message(client));
        self.client_mut(client).echo = message;
    }

    /// `isearch-yank-string`: text from the buffer or the kill ring is
    /// added to the string, downcased when the search folds case
    /// (`search-upper-case` is `not-yanks`) and quoted in a regexp search.
    pub(crate) fn isearch_yank_string(&mut self, client: ClientId, bytes: Vec<u8>) {
        let isearch = self.isearch(client);
        let mut bytes = if isearch.case_fold.folds() {
            downcase(&bytes)
        } else {
            bytes
        };
        if isearch.regexp {
            bytes = emx_text::search::regexp_quote(&String::from_utf8_lossy(&bytes)).into_bytes();
        }
        let isearch = self.isearch_mut(client);
        isearch.yank_flag = true;
        isearch.string.extend_from_slice(&bytes);
        self.isearch_search_and_update(client);
    }

    /// `isearch-yank-internal`: yanks from the search position to where
    /// `jump` moves from there.
    fn isearch_yank_to(
        &mut self,
        client: ClientId,
        jump: impl FnOnce(&Text, u64) -> Result<u64, CommandError>,
    ) -> Result<(), CommandError> {
        let isearch = self.isearch(client);
        let mut start = self.isearch_point(client);
        if !isearch.forward
            && let Some(other_end) = self.isearch_other_end(client)
        {
            start = other_end;
        }
        if !isearch.success
            && let Some(previous) = isearch.yank_previous_point
        {
            start = previous;
        }
        let text = self.buffers[isearch.buffer].text.clone();
        let end = jump(&text, start)?.max(start);
        let success = isearch.success;
        self.isearch_mut(client).yank_previous_point = (!success).then_some(end);
        let mut bytes = Vec::new();
        text.read(start..end, &mut bytes)?;
        self.isearch_yank_string(client, bytes);
        Ok(())
    }

    /// `isearch-yank-word-or-char` (`C-w`): the next word when a word
    /// character is at point or just after it, else the next character.
    pub(crate) fn isearch_yank_word_or_char(
        &mut self,
        client: ClientId,
        count: i64,
    ) -> Result<(), CommandError> {
        self.isearch_yank_to(client, move |text, start| {
            let mut position = start;
            for _ in 0..count.max(1) {
                let here = char_after(text, position)?;
                let next = match here {
                    Some((_, length)) => char_after(text, position + length)?,
                    None => None,
                };
                let word_near = here.is_some_and(|(code, _)| is_word(code))
                    || next.is_some_and(|(code, _)| is_word(code));
                position = if word_near {
                    scan_words(text, position, 1)?.unwrap_or(text.len())
                } else {
                    match here {
                        Some((_, length)) => position + length,
                        None => position,
                    }
                };
            }
            Ok(position)
        })
    }

    /// `isearch-yank-char` (`C-M-y`).
    pub(crate) fn isearch_yank_char(
        &mut self,
        client: ClientId,
        count: i64,
    ) -> Result<(), CommandError> {
        self.isearch_yank_to(client, move |text, start| {
            let mut position = start;
            for _ in 0..count.max(1) {
                match char_after(text, position)? {
                    Some((_, length)) => position += length,
                    None => break,
                }
            }
            Ok(position)
        })
    }

    /// `isearch-yank-line` (`M-s C-e`): the rest of the line, or the next
    /// line at its end.
    pub(crate) fn isearch_yank_line(
        &mut self,
        client: ClientId,
        count: i64,
    ) -> Result<(), CommandError> {
        self.isearch_yank_to(client, move |text, start| {
            let at_line_end = start == command::line_end(text, start)?;
            let lines = count.max(1) + i64::from(at_line_end);
            let (_, line) = command::forward_line(text, start, lines - 1)?;
            Ok(command::line_end(text, line)?)
        })
    }

    /// `isearch-yank-kill` (`C-y`).
    pub(crate) fn isearch_yank_kill(&mut self, client: ClientId) -> Result<(), CommandError> {
        let kill = self
            .kill_ring
            .current_kill(0)
            .map_err(|error| CommandError::User(error.to_string()))?
            .to_vec();
        self.isearch_yank_string(client, kill);
        Ok(())
    }

    /// `isearch-yank-pop-only` (`M-y`): after a yank into the string, the
    /// yank is replaced by the previous kill; otherwise it yanks.
    pub(crate) fn isearch_yank_pop_only(
        &mut self,
        client: ClientId,
        last_command: Option<&'static str>,
    ) -> Result<(), CommandError> {
        if !last_command.is_some_and(|name| ISEARCH_YANKS.contains(&name)) {
            return self.isearch_yank_kill(client);
        }
        if self.isearch(client).cmds.len() > 1 {
            self.isearch_pop_state(client);
        }
        let kill = self
            .kill_ring
            .current_kill(1)
            .map_err(|error| CommandError::User(error.to_string()))?
            .to_vec();
        self.isearch_yank_string(client, kill);
        Ok(())
    }

    /// `isearch-toggle-case-fold` (`M-c`).
    pub(crate) fn isearch_toggle_case_fold(&mut self, client: ClientId) {
        let isearch = self.isearch_mut(client);
        isearch.case_fold = if isearch.case_fold.folds() {
            CaseFold::Never
        } else {
            CaseFold::Always
        };
        let message = if isearch.case_fold.folds() {
            CASE_INSENSITIVE
        } else {
            CASE_SENSITIVE
        };
        isearch.momentary = Some(message.to_string());
        isearch.success = true;
        self.isearch_update(client);
    }

    /// `isearch-toggle-regexp` (`M-r`).
    pub(crate) fn isearch_toggle_regexp(&mut self, client: ClientId) {
        let isearch = self.isearch_mut(client);
        isearch.regexp = !isearch.regexp;
        isearch.success = true;
        self.isearch_update(client);
    }

    /// `isearch-toggle-lax-whitespace` (`M-s SPC`).
    pub(crate) fn isearch_toggle_lax_whitespace(&mut self, client: ClientId) {
        let isearch = self.isearch_mut(client);
        isearch.lax_whitespace = !isearch.lax_whitespace;
        let message = if isearch.lax_whitespace {
            SPACES_LOOSELY
        } else {
            SPACES_LITERALLY
        };
        isearch.momentary = Some(message.to_string());
        isearch.success = true;
        self.isearch_update(client);
    }

    /// `isearch-ring-adjust1`: the next (`advance`) or previous search
    /// string from the ring becomes the string.
    fn isearch_ring_adjust1(&mut self, client: ClientId, advance: bool) {
        let isearch = self.isearch(client);
        let ring = if isearch.regexp {
            &self.regexp_search_ring
        } else {
            &self.search_ring
        };
        if ring.is_empty() {
            return;
        }
        let length = i64::try_from(ring.len()).expect("the ring is bounded");
        let start = match isearch.ring_pointer {
            Some(pointer) => i64::try_from(pointer).expect("the ring is bounded"),
            None if advance => 0,
            None => -1,
        };
        let pointer = (start + if advance { -1 } else { 1 }).rem_euclid(length);
        let pointer = usize::try_from(pointer).expect("rem_euclid is not negative");
        let string = ring[pointer].clone();
        let isearch = self.isearch_mut(client);
        isearch.ring_pointer = Some(pointer);
        isearch.string = string;
    }

    /// `isearch-ring-advance` (`M-n`) and `isearch-ring-retreat` (`M-p`):
    /// with `search-ring-update` off, the string is then edited.
    pub(crate) fn isearch_ring_adjust(&mut self, client: ClientId, advance: bool) {
        self.isearch_ring_adjust1(client, advance);
        let nonincremental = self.isearch(client).nonincremental;
        self.isearch_edit_string(client, nonincremental);
    }

    /// `isearch-edit-string` (`M-e`): the string is edited in the
    /// minibuffer, then the search resumes from where it was.
    pub(crate) fn isearch_edit_string(&mut self, client: ClientId, nonincremental: bool) {
        if let Some(wait) = self.client(client).search_wait.as_ref()
            && matches!(wait.owner, SearchOwner::Isearch(_))
        {
            self.cancel_search(client);
        }
        let point = self.isearch_point(client);
        let old_other_end = self.isearch_other_end(client);
        let isearch = self.isearch(client);
        let prompt = message_prefix(&PrefixParts {
            success: isearch.success,
            adjusted: false,
            overwrapped: false,
            wrapped: isearch.wrapped,
            case_sensitive_failure: !isearch.success && !isearch.case_fold.folds(),
            regexp: isearch.regexp,
            nonincremental,
            forward: isearch.forward,
        });
        let ring = if isearch.regexp {
            &self.regexp_search_ring
        } else {
            &self.search_ring
        };
        let entries: Vec<Vec<u8>> = ring.iter().cloned().collect();
        let position = isearch.ring_pointer.map_or(0, |pointer| pointer + 1);
        let cursor = self
            .isearch_fail_position(client)
            .unwrap_or(isearch.string.len());
        let initial = isearch.string.clone();
        self.isearch_mut(client).suspended = Some(Suspended {
            old_point: point,
            old_other_end,
            nonincremental,
        });
        let mode = Mode::Line {
            history: HistoryKind::Search,
            defaults: Vec::new(),
            require: None,
            allow_empty: true,
        };
        let mut minibuffer = Minibuffer::new(prompt, initial, mode, Purpose::IsearchEdit)
            .with_own_history(
                entries,
                i64::try_from(position).expect("the ring is bounded"),
            );
        minibuffer.cursor = cursor;
        self.client_mut(client).echo.clear();
        self.start_minibuffer(client, minibuffer);
    }

    /// `isearch-fail-pos`: where the string stops matching, from the last
    /// successful state's string.
    fn isearch_fail_position(&self, client: ClientId) -> Option<usize> {
        let isearch = self.isearch(client);
        if isearch.success && isearch.error.is_none() {
            return None;
        }
        let succeeded = isearch
            .cmds
            .iter()
            .rev()
            .find(|frame| frame.success && frame.error.is_none())?;
        Some(
            if succeeded.string.len() < isearch.string.len()
                && isearch.string.starts_with(&succeeded.string)
            {
                succeeded.string.len()
            } else {
                0
            },
        )
    }

    /// The minibuffer edit of the string ended: `forward` is set when it
    /// ended with `C-s` (`Some(true)`) or `C-r`.
    pub(super) fn isearch_edit_finished(
        &mut self,
        client: ClientId,
        string: Vec<u8>,
        forward: Option<bool>,
    ) {
        if !self.isearch_active(client) {
            return;
        }
        let suspended = self
            .isearch_mut(client)
            .suspended
            .take()
            .expect("the isearch was suspended for the edit");
        let nonincremental = forward.is_none() && suspended.nonincremental;
        let new_forward = forward.unwrap_or(self.isearch(client).forward);
        let old_forward = self.isearch(client).forward;
        {
            let isearch = self.isearch_mut(client);
            isearch.string = string;
            isearch.forward = new_forward;
            isearch.nonincremental = nonincremental;
            isearch.just_started = true;
            isearch.yank_flag = false;
            isearch.adjusted = false;
        }
        let point = self.isearch_point(client);
        if let Some(other_end) = suspended.old_other_end
            && point == suspended.old_point
            && old_forward == new_forward
        {
            self.isearch_goto(client, other_end);
        }
        if self.isearch(client).string.is_empty() {
            let isearch = self.isearch(client);
            let ring = if isearch.regexp {
                &self.regexp_search_ring
            } else {
                &self.search_ring
            };
            let string = ring.front().cloned().unwrap_or_default();
            self.isearch_mut(client).string = string;
            self.isearch_ring_adjust1(client, false);
        }
        self.isearch_search(client, AfterSearch::Edited { nonincremental });
    }

    /// The minibuffer edit was quit: `isearch-abort` on the search as it
    /// was before the edit.
    pub(super) fn isearch_edit_quit(&mut self, client: ClientId) -> Result<(), CommandError> {
        if !self.isearch_active(client) {
            return Err(CommandError::Quit);
        }
        self.isearch_mut(client).suspended = None;
        self.isearch_abort(client)
    }

    /// A key in the minibuffer editing the search string: `C-s` and `C-r`
    /// resume the search in that direction (`minibuffer-local-isearch-map`).
    pub(super) fn isearch_edit_key(&mut self, client: ClientId, key: Key) -> bool {
        let direction = if key == command::CONTROL_S {
            true
        } else if key == command::CONTROL_R {
            false
        } else {
            return false;
        };
        let minibuffer = self
            .client_mut(client)
            .minibuffer
            .take()
            .expect("the edit happens in the minibuffer");
        self.fit_frame(client);
        self.isearch_edit_finished(client, minibuffer.input, Some(direction));
        true
    }

    /// `isearch-occur` (`M-s o`): occur for the search string, folding case
    /// as the search does.
    /// `C-u` collects the matches (each whole match: emx reads no collect
    /// template during a search); a number shows that many context lines.
    pub(crate) fn isearch_occur(
        &mut self,
        client: ClientId,
        prefix_arg: PrefixArg,
    ) -> Result<(), CommandError> {
        let isearch = self.isearch(client);
        let string = String::from_utf8_lossy(&isearch.string).into_owned();
        let regexp = match isearch_pattern(
            &isearch.string,
            isearch.regexp,
            isearch.lax_whitespace,
            false,
        ) {
            Pattern::Regex { emacs, .. } => emacs,
            Pattern::Literal { .. } => emx_text::search::regexp_quote(&string),
        };
        let case_fold = isearch.case_fold.folds();
        let (context, collect) = match prefix_arg {
            PrefixArg::None => (0, None),
            PrefixArg::Raw(_) => (0, Some(super::replacing::Template::new(b"\\&", true))),
            other => (other.numeric_value(), None),
        };
        self.occur_start(client, regexp, Some(case_fold), context, collect)
    }

    /// `isearch-query-replace` (`M-%`, `C-M-%` with `regexp`): ends the
    /// search and asks what to replace its string with.
    pub(crate) fn isearch_query_replace(
        &mut self,
        client: ClientId,
        regexp: bool,
        prefix_arg: PrefixArg,
    ) -> Result<(), CommandError> {
        let isearch = self.isearch(client);
        if let Err(error) = self.buffers[isearch.buffer].check_editable() {
            return Err(command::read_only_error(
                &self.buffers[isearch.buffer],
                error,
            ));
        }
        let regexp = regexp || isearch.regexp;
        let string = isearch.string.clone();
        let case_fold = isearch.case_fold.folds();
        let lax_whitespace = isearch.lax_whitespace;
        let backward = prefix_arg == PrefixArg::Negative;
        let delimited = prefix_arg != PrefixArg::None && !backward;
        let other_end = self.isearch_other_end(client);
        self.isearch_done(client, false, true);
        let point = self.window_point(client, self.selected_window_id(client));
        let buffer = &self.buffers[self.selected_buffer(client)];
        let mark_before = buffer.mark_active
            && buffer.mark.is_some_and(|mark| {
                let mark = buffer.marker_pos(mark);
                if backward { mark > point } else { mark < point }
            });
        if let Some(other_end) = other_end
            && (if backward {
                other_end > point
            } else {
                other_end < point
            })
            && !mark_before
        {
            let window = self.selected_window_id(client);
            self.set_window_point(client, window, other_end);
        }
        self.query_replace_from_isearch(
            client,
            string,
            regexp,
            (case_fold, lax_whitespace),
            delimited,
            backward,
        );
        Ok(())
    }

    /// What the client's isearch or query-replace highlights.
    pub(crate) fn search_highlights(&self, client: ClientId) -> Option<IsearchHighlights> {
        self.isearch_highlights(client)
            .or_else(|| self.replace_highlights(client))
    }

    /// What isearch shows in the windows of `client`: the current match in
    /// the window searched, and the pattern to highlight lazily in every
    /// window showing the buffer.
    pub(crate) fn isearch_highlights(&self, client: ClientId) -> Option<IsearchHighlights> {
        let isearch = self.client(client).isearch.as_ref()?;
        if isearch.suspended.is_some() {
            return None;
        }
        let lazy = (isearch.success && isearch.error.is_none() && !isearch.string.is_empty())
            .then(|| isearch.pattern.clone())
            .flatten();
        let point = self.isearch_point(client);
        let current = self
            .isearch_other_end(client)
            .filter(|_| isearch.success)
            .map(|other_end| (isearch.window, other_end.min(point)..other_end.max(point)));
        Some(IsearchHighlights {
            buffer: isearch.buffer,
            current,
            lazy,
        })
    }
}

/// What a client's isearch or query-replace highlights: the current match
/// in the window searched, and the pattern to highlight lazily in every
/// window of the client showing the buffer.
#[derive(Debug, Clone)]
pub(crate) struct IsearchHighlights {
    pub buffer: BufferId,
    pub current: Option<(WindowId, std::ops::Range<u64>)>,
    pub lazy: Option<CompiledPattern>,
}

/// A search outcome as isearch and query-replace use it.
pub(super) fn match_of(result: Result<SearchOutcome, String>) -> Result<Option<Match>, String> {
    match result {
        Ok(SearchOutcome::Found(found)) => Ok(found),
        Ok(other) => unreachable!("a forward or backward search reports a match, not {other:?}"),
        Err(error) => Err(error),
    }
}
