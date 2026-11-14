//! `occur` (`M-s o`): `*Occur*` lists the lines matching a regexp, laid out
//! as replace.el's `occur-engine` lays them out, and `RET` on a line
//! visits its match. The search runs on a worker for large buffers.

use std::ops::Range;
use std::sync::Arc;

use emx_text::search::{CompiledPattern, Pattern};
use emx_text::{Sources, Text};

use super::Editor;
use super::Purpose;
use super::reload::Unjournaled;
use super::searching::{SearchOwner, SearchWait};
use crate::buffer::{BufferId, ChangeRecorder, OccurData};
use crate::command::{CommandError, PrefixArg};
use crate::event::ClientId;
use crate::isearch::{SEARCHING, describe, no_upper_case};
use crate::marker::InsertionType;
use crate::minibuffer::{HistoryKind, Minibuffer, Mode};
use crate::search_job::{CollectedMatch, OccurLine, SearchOutcome, SearchRequest};
use crate::undo::UndoLimits;

use super::replacing::Template;

const OCCUR_BUFFER: &str = "*Occur*";
/// Emacs `mode-name` of `occur-mode`.
const OCCUR_MODE: &str = "Occur";
const LIST_PROMPT: &str = "List lines matching regexp";
const COLLECT_PROMPT: &str = "Collect strings matching regexp";
const COLLECT_TEMPLATE_PROMPT: &str = "Regexp to collect";
const EMPTY_REGEXP: &str = "Occur doesn\u{2019}t work with the empty regexp";
const NO_OCCURRENCE: &str = "No occurrence on this line";
const SOURCE_KILLED: &str = "Buffer for this occurrence was killed";
const NO_MORE_MATCHES: &str = "No more matches";
const NO_EARLIER_MATCHES: &str = "No earlier matches";
/// `occur-engine` numbers lines with `%7d:`, which aligns tabs.
const LINE_NUMBER_WIDTH: usize = 7;
/// The prefix of continuation and context lines.
const CONTINUATION_PREFIX: &str = "       :";
const CONTEXT_SEPARATOR: &str = "-------\n";
/// `occur-1` leaves the regexp out of its message when the message would
/// not fit the window: this many columns are the rest of the message.
const MESSAGE_ROOM: usize = 42;
const WHOLE_MATCH: &[u8] = b"\\&";

/// What an occur search is for.
#[derive(Debug, Clone)]
pub(crate) struct OccurRequest {
    source: BufferId,
    regexp: String,
    /// Collect mode: what each match becomes.
    collect: Option<Template>,
}

/// `next-single-property-change` over the matches in `*Occur*`: the first
/// position after `from` whose "in a match" state differs from `from`'s.
fn next_change(spans: &[Range<u64>], from: u64, length: u64) -> Option<u64> {
    let inside = |position: u64| spans.iter().any(|span| span.contains(&position));
    let state = inside(from);
    spans
        .iter()
        .flat_map(|span| [span.start, span.end])
        .filter(|position| *position > from && *position <= length)
        .filter(|position| inside(*position) != state)
        .min()
}

/// `previous-single-property-change`: the last position before `from`
/// where the state of the character before it changes.
fn previous_change(spans: &[Range<u64>], from: u64) -> Option<u64> {
    if from == 0 {
        return None;
    }
    let inside = |position: u64| spans.iter().any(|span| span.contains(&position));
    let state = inside(from - 1);
    spans
        .iter()
        .flat_map(|span| [span.start, span.end])
        .filter(|position| *position < from && *position > 0)
        .filter(|position| inside(position - 1) != state)
        .max()
}

impl Editor {
    /// `occur` (`M-s o`): reads the regexp. `C-u` collects the matches
    /// instead of listing lines; a number shows that many lines of context.
    pub(crate) fn occur_command(&mut self, client: ClientId, prefix_arg: PrefixArg) {
        let collect = matches!(prefix_arg, PrefixArg::Raw(_));
        let context = match prefix_arg {
            PrefixArg::None | PrefixArg::Raw(_) => 0,
            other => other.numeric_value(),
        };
        let default = self.histories.list(HistoryKind::Regexp).first().cloned();
        let label = if collect { COLLECT_PROMPT } else { LIST_PROMPT };
        let prompt = match &default {
            Some(default) if !default.is_empty() => {
                format!("{label} (default {}): ", describe(default))
            }
            _ => format!("{label}: "),
        };
        let mode = Mode::Line {
            history: HistoryKind::Regexp,
            defaults: default.clone().into_iter().collect(),
            require: None,
            allow_empty: true,
        };
        let minibuffer = Minibuffer::new(
            prompt,
            Vec::new(),
            mode,
            Purpose::OccurRegexp {
                collect,
                context,
                default,
            },
        );
        self.start_minibuffer(client, minibuffer);
    }

    /// The regexp for `occur` was read.
    pub(super) fn occur_regexp_answered(
        &mut self,
        client: ClientId,
        input: Vec<u8>,
        collect: bool,
        context: i64,
        default: Option<Vec<u8>>,
    ) -> Result<(), CommandError> {
        let regexp = if input.is_empty() {
            default.unwrap_or_default()
        } else {
            input
        };
        let regexp = String::from_utf8_lossy(&regexp).into_owned();
        if !collect {
            return self.occur_start(client, regexp, None, context, None);
        }
        let groups = CompiledPattern::compile(&Pattern::Regex {
            emacs: regexp.clone(),
            case_fold: false,
        })
        .map(|pattern| pattern.group_count())
        .unwrap_or(1);
        if groups <= 1 {
            return self.occur_start(
                client,
                regexp,
                None,
                0,
                Some(Template::new(WHOLE_MATCH, true)),
            );
        }
        let default = self.occur_collect_history.first().cloned();
        let prompt = match &default {
            Some(default) => format!(
                "{COLLECT_TEMPLATE_PROMPT} (default {}): ",
                describe(default)
            ),
            None => format!("{COLLECT_TEMPLATE_PROMPT}: "),
        };
        let mode = Mode::Line {
            history: HistoryKind::Regexp,
            defaults: default.clone().into_iter().collect(),
            require: None,
            allow_empty: true,
        };
        let history = self.occur_collect_history.clone();
        let minibuffer = Minibuffer::new(
            prompt,
            Vec::new(),
            mode,
            Purpose::OccurCollect { regexp, default },
        )
        .with_own_history(history, 0);
        self.start_minibuffer(client, minibuffer);
        Ok(())
    }

    /// The template for `occur`'s collect mode was read.
    pub(super) fn occur_collect_answered(
        &mut self,
        client: ClientId,
        regexp: String,
        input: Vec<u8>,
        default: Option<Vec<u8>>,
    ) -> Result<(), CommandError> {
        let template = if input.is_empty() {
            default.unwrap_or_default()
        } else {
            input
        };
        if !template.is_empty() && self.occur_collect_history.first() != Some(&template) {
            self.occur_collect_history.insert(0, template.clone());
        }
        self.occur_start(
            client,
            regexp,
            None,
            0,
            Some(Template::new(&template, true)),
        )
    }

    /// `occur-1` on the selected buffer. `case_fold` is decided by the
    /// caller (isearch) or else by `search-upper-case`.
    pub(super) fn occur_start(
        &mut self,
        client: ClientId,
        regexp: String,
        case_fold: Option<bool>,
        context: i64,
        collect: Option<Template>,
    ) -> Result<(), CommandError> {
        if regexp.is_empty() {
            return Err(CommandError::User(EMPTY_REGEXP.to_string()));
        }
        let case_fold = case_fold.unwrap_or_else(|| no_upper_case(&regexp, true));
        let pattern = CompiledPattern::compile(&Pattern::Regex {
            emacs: regexp.clone(),
            case_fold,
        })
        .map_err(|error| CommandError::User(super::replacing::invalid_regexp_message(&error)))?;
        let source = self.selected_buffer(client);
        let request = if collect.is_some() {
            SearchRequest::Collect
        } else {
            SearchRequest::Occur { context }
        };
        let occur = OccurRequest {
            source,
            regexp,
            collect,
        };
        match self.start_search(
            client,
            source,
            pattern,
            request,
            SearchOwner::Occur(occur.clone()),
        ) {
            Some(result) => self.occur_finished(client, occur, result),
            None => {
                self.client_mut(client).echo = format!("Occur [{SEARCHING}]");
                Ok(())
            }
        }
    }

    /// A worker's occur search arrived.
    pub(super) fn occur_search_arrived(
        &mut self,
        client: ClientId,
        wait: SearchWait,
        result: Result<SearchOutcome, String>,
        request: OccurRequest,
    ) {
        if !self.buffers.contains_key(request.source) {
            return;
        }
        let outcome = if self.search_is_current(request.source, &wait) {
            self.occur_finished(client, request, result)
        } else {
            // The buffer changed meanwhile: list the lines of the new text.
            let regexp = request.regexp.clone();
            let window = self.selected_window_id(client);
            let shown = self.window(client, window).buffer;
            if shown != request.source {
                return;
            }
            self.occur_start(client, regexp, None, 0, request.collect)
        };
        if let Err(error) = outcome {
            self.fail(client, &error);
        }
    }

    /// Cancels the client's occur search (`C-g`).
    pub(crate) fn occur_cancel(&mut self, client: ClientId) {
        if let Some(wait) = self.client(client).search_wait.as_ref()
            && matches!(wait.owner, SearchOwner::Occur(_))
        {
            self.cancel_search(client);
        }
    }

    fn occur_finished(
        &mut self,
        client: ClientId,
        request: OccurRequest,
        result: Result<SearchOutcome, String>,
    ) -> Result<(), CommandError> {
        let outcome = result.map_err(CommandError::User)?;
        let (text, data, count) = match (outcome, &request.collect) {
            (SearchOutcome::Occur(lines), None) => {
                let name = self.buffers[request.source].name.clone();
                self.occur_text(&request, &lines, &name)
            }
            (SearchOutcome::Collect(matches), Some(template)) => {
                let (text, count) = collect_text(template, &matches)?;
                (text, None, count)
            }
            (other, _) => unreachable!("an occur search reports lines, not {other:?}"),
        };
        let plural = if count == 1 { "match" } else { "matches" };
        let shown_count = if count == 0 {
            "no".to_string()
        } else {
            count.to_string()
        };
        let window_columns =
            usize::from(self.window_size(client, self.selected_window_id(client)).0);
        let description = if request.regexp.chars().count() + MESSAGE_ROOM > window_columns {
            String::new()
        } else {
            format!(" for \"{}\"", request.regexp)
        };
        let message = format!("Searched 1 buffer; {shown_count} {plural}{description}");
        let existing = self.find_occur_buffer(request.source);
        if count == 0 {
            if let Some(existing) = existing {
                self.kill_buffer(existing);
            }
            self.set_echo(client, message);
            return Ok(());
        }
        let occur = match existing {
            Some(existing) => existing,
            None => {
                let created = self.create_buffer(OCCUR_BUFFER);
                self.buffers[created].read_only = true;
                created
            }
        };
        self.release_occur_data(occur);
        let data = data.map(|(targets, spans)| OccurData {
            source: request.source,
            targets: targets
                .into_iter()
                .map(|target| {
                    target.map(|position| {
                        self.buffers[request.source]
                            .create_marker_without_undo(position, InsertionType::StayBefore)
                    })
                })
                .collect(),
            match_spans: spans,
            source_killed: false,
        });
        let limits = UndoLimits::from(&self.config);
        let text = Text::from_original(Sources::memory(Arc::from(text)));
        let recorder: &mut dyn ChangeRecorder = if self.journal_outbox.tracks(occur) {
            &mut self.journal_outbox
        } else {
            &mut Unjournaled
        };
        let collecting = data.is_none();
        let target = &mut self.buffers[occur];
        target.reset_text(text, limits, recorder);
        target.read_only = true;
        target.special_mode = data.as_ref().map(|_| OCCUR_MODE);
        target.occur = data;
        let shown = self
            .frame(client)
            .windows
            .values()
            .any(|window| window.buffer == occur);
        let window = self.display_buffer(client, occur);
        // Collect mode leaves the list's point after the text it inserted,
        // where a window newly showing it puts point; occur proper goes
        // back to the top.
        if collecting && !shown {
            let end = self.buffers[occur].text.len();
            self.set_window_point(client, window, end);
        }
        self.set_echo(client, message);
        Ok(())
    }

    /// The `*Occur*` buffer to fill: a list of `source` itself is renamed
    /// out of the way first, as `occur-1` does.
    fn find_occur_buffer(&mut self, source: BufferId) -> Option<BufferId> {
        let existing = self
            .buffers
            .iter()
            .find(|(_, buffer)| buffer.name == OCCUR_BUFFER)
            .map(|(id, _)| id)?;
        if existing != source {
            return Some(existing);
        }
        let mut number = 2;
        loop {
            let name = format!("{OCCUR_BUFFER}<{number}>");
            if self.find_buffer(&name).is_none() {
                self.buffers[existing].name = name;
                return None;
            }
            number += 1;
        }
    }

    /// Drops the markers an `*Occur*` buffer keeps in its source.
    pub(super) fn release_occur_data(&mut self, occur: BufferId) {
        let Some(data) = self
            .buffers
            .get_mut(occur)
            .and_then(|buffer| buffer.occur.take())
        else {
            return;
        };
        if !data.source_killed
            && let Some(source) = self.buffers.get_mut(data.source)
        {
            for marker in data.targets.into_iter().flatten() {
                source.delete_marker(marker);
            }
        }
    }

    /// A buffer is being killed: `*Occur*` lists of it keep their text but
    /// lose their targets, and its own targets go.
    pub(super) fn occur_forget_buffer(&mut self, buffer: BufferId) {
        self.release_occur_data(buffer);
        let lists: Vec<BufferId> = self
            .buffers
            .iter()
            .filter(|(_, list)| {
                list.occur
                    .as_ref()
                    .is_some_and(|data| data.source == buffer)
            })
            .map(|(id, _)| id)
            .collect();
        for list in lists {
            // The markers die with the source buffer; the lines still know
            // which of them showed a match.
            if let Some(data) = self.buffers[list].occur.as_mut() {
                data.source_killed = true;
            }
        }
    }

    /// The text of `*Occur*` (`occur-engine`), the target of each of its
    /// lines in the source, the spans of the matches in it, and the number
    /// of matches.
    #[allow(clippy::type_complexity)]
    fn occur_text(
        &self,
        request: &OccurRequest,
        lines: &[OccurLine],
        name: &str,
    ) -> (Vec<u8>, Option<(Vec<Option<u64>>, Vec<Range<u64>>)>, u64) {
        let context = lines
            .iter()
            .any(|line| !line.before.is_empty() || !line.after.is_empty());
        let mut body = Vec::new();
        let mut targets = vec![None];
        let mut spans = Vec::new();
        let mut matches = 0;
        let mut previous_line: Option<u64> = None;
        let mut previous_after: Vec<Vec<u8>> = Vec::new();
        let positive_context = lines.iter().any(|line| !line.after.is_empty());
        let push_context = |body: &mut Vec<u8>, targets: &mut Vec<Option<u64>>, line: &[u8]| {
            body.extend_from_slice(CONTINUATION_PREFIX.as_bytes());
            body.extend_from_slice(line);
            body.push(b'\n');
            targets.push(None);
        };
        // Spans are offsets in the body; the header goes in front later.
        for line in lines {
            let length = line.bytes.len() as u64;
            let counted: Vec<Range<u64>> = line
                .matches
                .iter()
                .filter(|found| found.start - line.start < length)
                .cloned()
                .collect();
            matches += counted.len() as u64 + u64::from(length == 0);
            if context {
                let mut before: &[Vec<u8>] = &line.before;
                let mut separator = false;
                if !previous_after.is_empty() {
                    let previous = previous_line.expect("after lines follow a match");
                    if previous + previous_after.len() as u64 >= line.number - before.len() as u64 {
                        let keep = line
                            .number
                            .saturating_sub(previous + before.len() as u64 + 1);
                        previous_after.truncate(usize::try_from(keep).unwrap_or(usize::MAX));
                    } else {
                        separator = true;
                    }
                }
                if let Some(previous) = previous_line {
                    if line.number - before.len() as u64 <= previous {
                        let keep = usize::try_from(line.number - previous - 1).unwrap_or(0);
                        before = &before[before.len() - keep.min(before.len())..];
                    } else if !positive_context {
                        separator = true;
                    }
                }
                for after in previous_after.drain(..) {
                    push_context(&mut body, &mut targets, &after);
                }
                if separator {
                    body.extend_from_slice(CONTEXT_SEPARATOR.as_bytes());
                    targets.push(None);
                }
                for context_line in before {
                    push_context(&mut body, &mut targets, context_line);
                }
            }
            let first_match = line.matches.first().map_or(line.start, |found| found.start);
            let line_start = body.len() as u64;
            body.extend_from_slice(format!("{:>LINE_NUMBER_WIDTH$}:", line.number).as_bytes());
            targets.push(Some(first_match));
            let prefix = (LINE_NUMBER_WIDTH + 1) as u64;
            let shown_offset = |offset: u64| {
                let newlines = line.bytes[..usize::try_from(offset).expect("inside the line")]
                    .iter()
                    .filter(|byte| **byte == b'\n')
                    .count() as u64;
                line_start + prefix + offset + newlines * prefix
            };
            for found in &counted {
                let start = found.start - line.start;
                let end = (found.end - line.start).min(length);
                spans.push(shown_offset(start)..shown_offset(end));
            }
            for byte in &line.bytes {
                body.push(*byte);
                if *byte == b'\n' {
                    body.extend_from_slice(CONTINUATION_PREFIX.as_bytes());
                    targets.push(Some(first_match));
                }
            }
            body.push(b'\n');
            if context {
                previous_after = line.after.clone();
            }
            previous_line = Some(line.last_number);
        }
        for after in previous_after.drain(..) {
            push_context(&mut body, &mut targets, &after);
        }
        let line_count = lines.len() as u64;
        let match_word = if matches == 1 { "match" } else { "matches" };
        let lines_part = if line_count == matches {
            String::new()
        } else {
            let line_word = if line_count == 1 { "line" } else { "lines" };
            format!(" in {line_count} {line_word}")
        };
        let header = format!(
            "{matches} {match_word}{lines_part} for \"{}\" in buffer: {name}\n",
            request.regexp
        );
        let offset = header.len() as u64;
        let mut text = header.into_bytes();
        text.extend_from_slice(&body);
        let spans = spans
            .into_iter()
            .map(|span| span.start + offset..span.end + offset)
            .collect();
        (text, Some((targets, spans)), matches)
    }

    /// `occur-mode-goto-occurrence` (`RET`) and its other-window variant
    /// (`o`): the match of point's line, in its buffer's window.
    pub(crate) fn occur_goto_occurrence(&mut self, client: ClientId) -> Result<(), CommandError> {
        let (source, position) = self.occur_target(client)?;
        let window = self.display_buffer(client, source);
        self.select_window(client, window);
        self.set_window_point(client, window, position);
        Ok(())
    }

    /// `occur-mode-display-occurrence` (`C-o`): shows the match without
    /// leaving `*Occur*`.
    pub(crate) fn occur_display_occurrence(
        &mut self,
        client: ClientId,
    ) -> Result<(), CommandError> {
        let (source, position) = self.occur_target(client)?;
        let window = self.display_buffer(client, source);
        self.set_window_point(client, window, position);
        Ok(())
    }

    /// The buffer and position of the match on point's line in `*Occur*`.
    fn occur_target(&self, client: ClientId) -> Result<(BufferId, u64), CommandError> {
        let list = &self.buffers[self.selected_buffer(client)];
        let no_occurrence = || CommandError::User(NO_OCCURRENCE.to_string());
        let data = list.occur.as_ref().ok_or_else(no_occurrence)?;
        let point = self.window_point(client, self.selected_window_id(client));
        let target = list
            .text
            .line_of(point)?
            .and_then(|line| usize::try_from(line).ok())
            .and_then(|line| data.targets.get(line).copied().flatten())
            .ok_or_else(no_occurrence)?;
        if data.source_killed {
            return Err(CommandError::User(SOURCE_KILLED.to_string()));
        }
        Ok((data.source, self.buffers[data.source].marker_pos(target)))
    }

    /// `occur-next` (`M-n`) and `occur-prev` (`M-p`): to the next or
    /// previous match, `count` times.
    pub(crate) fn occur_move(
        &mut self,
        client: ClientId,
        forward: bool,
        count: i64,
    ) -> Result<(), CommandError> {
        let window = self.selected_window_id(client);
        let list = &self.buffers[self.selected_buffer(client)];
        let Some(data) = list.occur.as_ref() else {
            return Err(CommandError::User(NO_OCCURRENCE.to_string()));
        };
        let spans = data.match_spans.clone();
        let length = list.text.len();
        let inside = |position: u64| spans.iter().any(|span| span.contains(&position));
        let mut point = self.window_point(client, window);
        for _ in 0..count.max(1) {
            let mut target = if forward {
                next_change(&spans, point, length)
            } else {
                previous_change(&spans, point)
            };
            if let Some(found) = target
                && inside(found)
            {
                target = if forward {
                    next_change(&spans, found, length)
                } else {
                    previous_change(&spans, found)
                };
            }
            point = target.ok_or_else(|| {
                CommandError::User(
                    if forward {
                        NO_MORE_MATCHES
                    } else {
                        NO_EARLIER_MATCHES
                    }
                    .to_string(),
                )
            })?;
            self.set_window_point(client, window, point);
        }
        Ok(())
    }

    /// `next-error-no-select` (`n`) and `previous-error-no-select` (`p`) in
    /// `*Occur*`: to the next match, shown in its buffer's window.
    pub(crate) fn occur_next_error_no_select(
        &mut self,
        client: ClientId,
        forward: bool,
    ) -> Result<(), CommandError> {
        let window = self.selected_window_id(client);
        let buffer = self.selected_buffer(client);
        let point = self.window_point(client, window);
        let text = self.buffers[buffer].text.clone();
        let edge = if forward {
            crate::command::line_end(&text, point)?
        } else {
            crate::command::line_start(&text, point)?
        };
        self.set_window_point(client, window, edge);
        self.occur_move(client, forward, 1)?;
        self.occur_display_occurrence(client)
    }

    /// `*Help*` and similar buffers: `text`, read-only, shown in another
    /// window without being selected (`with-output-to-temp-buffer`).
    pub(super) fn show_text_buffer(
        &mut self,
        client: ClientId,
        name: &str,
        text: Vec<u8>,
        mode: &'static str,
    ) {
        let buffer = match self
            .buffers
            .iter()
            .find(|(_, buffer)| buffer.name == name)
            .map(|(id, _)| id)
        {
            Some(existing) => existing,
            None => self.create_buffer(name),
        };
        let limits = UndoLimits::from(&self.config);
        let text = Text::from_original(Sources::memory(Arc::from(text)));
        let recorder: &mut dyn ChangeRecorder = if self.journal_outbox.tracks(buffer) {
            &mut self.journal_outbox
        } else {
            &mut Unjournaled
        };
        let target = &mut self.buffers[buffer];
        target.reset_text(text, limits, recorder);
        target.read_only = true;
        target.special_mode = Some(mode);
        self.display_buffer(client, buffer);
    }
}

/// The text of collect mode: each match's expansion, on a line of its own
/// unless it ends one already.
fn collect_text(
    template: &Template,
    matches: &[CollectedMatch],
) -> Result<(Vec<u8>, u64), CommandError> {
    let mut text = Vec::new();
    let mut count = 0;
    for groups in matches {
        let expansion = template.expand_groups(groups, count)?;
        text.extend_from_slice(&expansion);
        count += 1;
        let at_line_start = text.last().is_none_or(|byte| *byte == b'\n');
        if !at_line_start {
            text.push(b'\n');
        }
    }
    Ok((text, count))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_changes_follow_emacs() {
        // Matches at 55..58, 71..74 and 75..78 (0-based here).
        let spans = [55..58, 71..74, 75..78];
        assert_eq!(next_change(&spans, 0, 90), Some(55));
        assert_eq!(next_change(&spans, 55, 90), Some(58));
        assert_eq!(next_change(&spans, 74, 90), Some(75));
        assert_eq!(previous_change(&spans, 78), Some(75));
        assert_eq!(previous_change(&spans, 75), Some(74));
        assert_eq!(next_change(&spans, 78, 90), None);
    }
}
