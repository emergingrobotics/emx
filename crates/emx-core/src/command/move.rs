use emx_text::{ReadError, Text, display_units, unit_width};

use super::mark::push_mark;
use super::misc::recenter_if_past_window_end;
use super::{CommandContext, CommandError, PrefixArg};
use emx_text::word_syntax::{CHINESE, COMBINING_DIACRITIC, HIRAGANA, KATAKANA, SCRIPTS, WORD};

/// Bytes read around point to find one display unit; doubled while a single
/// unit fills the whole window.
const UNIT_WINDOW: u64 = 64;
const NEXT_LINE: &str = "next-line";
const PREVIOUS_LINE: &str = "previous-line";
/// `beginning-of-buffer` and `end-of-buffer` with a numeric argument N go
/// N tenths of the way through the buffer.
const BUFFER_FRACTIONS: i128 = 10;
const UTF8_MAX_LEN: u64 = 4;
const LARGEST_SCALAR: u32 = 0x10_FFFF;
/// Emacs represents an invalid byte B as the character 0x3FFF00 + B.
const RAW_BYTE_BASE: u32 = 0x3F_FF00;

/// A character category, as named in Emacs's word boundary rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Category {
    CombiningDiacritic,
    Chinese,
    Hiragana,
    Katakana,
}

/// One rule of `word-combining-categories` or `word-separating-categories`:
/// `(C1 . C2)`, where `None` matches any character.
type CategoryRule = (Option<Category>, Option<Category>);

/// Emacs 29 `word-combining-categories`: across a script change, these
/// pairs still continue a word.
const WORD_COMBINING_CATEGORIES: [CategoryRule; 4] = [
    (None, Some(Category::CombiningDiacritic)),
    (Some(Category::CombiningDiacritic), None),
    (Some(Category::Chinese), Some(Category::Hiragana)),
    (Some(Category::Chinese), Some(Category::Katakana)),
];
/// Emacs 29 `word-separating-categories`: within one script, these pairs
/// still end a word.
const WORD_SEPARATING_CATEGORIES: [CategoryRule; 1] =
    [(Some(Category::Hiragana), Some(Category::Katakana))];

pub(super) fn forward_char(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let count = ctx.prefix_arg.numeric_value();
    move_units(ctx, count)
}

pub(super) fn backward_char(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let count = ctx.prefix_arg.numeric_value();
    move_units(ctx, count.saturating_neg())
}

/// Emacs `forward-char`: at a buffer edge, point stays at the edge and the
/// command signals. A character is one display unit (a grapheme cluster, or
/// one invalid or control byte), standing in for Emacs moving over
/// compositions as a whole.
fn move_units(ctx: &mut CommandContext, count: i64) -> Result<(), CommandError> {
    let text = ctx.buffer().text.clone();
    let mut point = ctx.point();
    let mut outcome = Ok(());
    for _ in 0..count.unsigned_abs() {
        let next = if count > 0 {
            next_unit_end(&text, point)?
        } else {
            previous_unit_start(&text, point)?
        };
        match next {
            Some(position) => point = position,
            None => {
                outcome = Err(if count > 0 {
                    CommandError::EndOfBuffer
                } else {
                    CommandError::BeginningOfBuffer
                });
                break;
            }
        }
    }
    ctx.set_point(point);
    outcome
}

pub(super) fn next_line(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let count = ctx.prefix_arg.numeric_value();
    line_move(ctx, count, false, None).map(|_| ())
}

pub(super) fn previous_line(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let count = ctx.prefix_arg.numeric_value();
    line_move(ctx, count.saturating_neg(), false, None).map(|_| ())
}

/// Emacs `move-beginning-of-line`: with N other than 1, move N - 1 lines
/// first, stopping at the buffer edges.
pub(super) fn move_beginning_of_line(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let count = ctx.prefix_arg.numeric_value();
    if count != 1 {
        line_move(ctx, count.saturating_sub(1), true, None)?;
    }
    let text = ctx.buffer().text.clone();
    let start = line_start(&text, ctx.point())?;
    ctx.set_point(start);
    Ok(())
}

/// Emacs `move-end-of-line`: the end of the line N - 1 lines away, found
/// as the position before the start of the line N lines away.
pub(super) fn move_end_of_line(ctx: &mut CommandContext) -> Result<(), CommandError> {
    const GOAL_COLUMN: u64 = 0;
    let count = ctx.prefix_arg.numeric_value();
    let moved = line_move(ctx, count, true, Some(GOAL_COLUMN))?;
    if moved {
        let text = ctx.buffer().text.clone();
        let start = line_start(&text, ctx.point())?;
        // Back over the newline that ends the previous line.
        ctx.set_point(start.saturating_sub(1));
    }
    Ok(())
}

pub(super) fn forward_word(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let count = ctx.prefix_arg.numeric_value();
    let target = word_target(ctx, count)?;
    ctx.set_point(target);
    Ok(())
}

pub(super) fn backward_word(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let count = ctx.prefix_arg.numeric_value();
    let target = word_target(ctx, count.saturating_neg())?;
    ctx.set_point(target);
    Ok(())
}

/// Where Emacs `forward-word` with COUNT leaves point: past COUNT words, or
/// the buffer edge when there are fewer. Never inside a grapheme cluster.
pub(super) fn word_target(ctx: &CommandContext, count: i64) -> Result<u64, CommandError> {
    let text = &ctx.buffer().text;
    let scanned = scan_words(text, ctx.point(), count)?;
    let target = scanned.unwrap_or(if count > 0 { text.len() } else { 0 });
    Ok(snap_to_unit(text, target, count > 0)?)
}

/// Emacs `beginning-of-buffer`: pushes the mark unless given `C-u` or the
/// region is active; with N, goes N tenths of the way, then to a line start.
pub(super) fn beginning_of_buffer(ctx: &mut CommandContext) -> Result<(), CommandError> {
    jump_in_buffer(ctx, |_, numerator| 1 + numerator / BUFFER_FRACTIONS, 0)
}

/// Emacs `end-of-buffer`, the mirror of `beginning-of-buffer`, except
/// that going to the very end shows it near the window bottom.
pub(super) fn end_of_buffer(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let length = ctx.buffer().text.len();
    jump_in_buffer(
        ctx,
        |size, numerator| size - numerator / BUFFER_FRACTIONS,
        length,
    )?;
    if matches!(ctx.prefix_arg, PrefixArg::None | PrefixArg::Raw(_)) {
        recenter_if_past_window_end(ctx)?;
    }
    Ok(())
}

/// The shared part of `beginning-of-buffer` and `end-of-buffer`. `numbered`
/// maps the buffer size and size * N, both in characters as Emacs counts
/// them, to the target character position for an argument N.
fn jump_in_buffer(
    ctx: &mut CommandContext,
    numbered: impl Fn(i128, i128) -> i128,
    edge: u64,
) -> Result<(), CommandError> {
    let argument = ctx.prefix_arg;
    let universal = matches!(argument, PrefixArg::Raw(_));
    if !universal && !ctx.region_active() {
        let point = ctx.point();
        push_mark(ctx, point, false, false);
    }
    let text = ctx.buffer().text.clone();
    let target = match argument {
        PrefixArg::None | PrefixArg::Raw(_) => edge,
        PrefixArg::Numeric(_) | PrefixArg::Negative => {
            let size = i128::from(char_count(&text, text.len())?);
            let numerator = size * i128::from(argument.numeric_value());
            let wanted = numbered(size, numerator).clamp(0, size);
            let wanted = u64::try_from(wanted).expect("clamped to the buffer size");
            forward_line(&text, char_to_byte(&text, wanted)?, 1)?.1
        }
    };
    ctx.set_point(target);
    Ok(())
}

/// Emacs `line-move-1` without invisible text: move COUNT logical lines
/// and then to the goal column. When the buffer ends first, point goes to
/// the end (or start) of the last line reached and, unless `noerror`, the
/// motion signals. Returns whether all lines were moved.
pub(super) fn line_move(
    ctx: &mut CommandContext,
    count: i64,
    noerror: bool,
    goal_column: Option<u64>,
) -> Result<bool, CommandError> {
    let text = ctx.buffer().text.clone();
    let tab_width = ctx.editor().config().tab_width;
    let origin = ctx.point();
    if !(ctx.last_command_is(NEXT_LINE) || ctx.last_command_is(PREVIOUS_LINE)) {
        let column = current_column(&text, origin, tab_width)?;
        ctx.set_temporary_goal_column(column);
    }
    let (moved, reached) = if count > 0 {
        let mut position = origin;
        if count > 1 {
            position = forward_line(&text, position, count - 1)?.1;
        }
        position = line_end(&text, position)?;
        let (shortage, after) = forward_line(&text, position, 1)?;
        (shortage == 0, after)
    } else {
        let (shortage, after) = forward_line(&text, origin, count)?;
        (shortage == 0, after)
    };
    if !moved {
        let stop = if count > 0 {
            line_end(&text, reached)?
        } else {
            line_start(&text, reached)?
        };
        ctx.set_point(stop);
        if noerror {
            return Ok(false);
        }
        return Err(if count > 0 {
            CommandError::EndOfBuffer
        } else {
            CommandError::BeginningOfBuffer
        });
    }
    let column = goal_column.unwrap_or_else(|| ctx.temporary_goal_column());
    let target = move_to_column(&text, reached, column, tab_width)?;
    ctx.set_point(target);
    Ok(true)
}

/// The start of the line containing `pos`.
pub(crate) fn line_start(text: &Text, pos: u64) -> Result<u64, ReadError> {
    Ok(text
        .find_newline_backward(pos)?
        .map_or(0, |newline| newline + 1))
}

/// The end of the line containing `pos`: its newline, or the buffer end.
pub(crate) fn line_end(text: &Text, pos: u64) -> Result<u64, ReadError> {
    Ok(text.find_newline_forward(pos)?.unwrap_or(text.len()))
}

/// Emacs `forward-line`: the position reached and the count of lines left
/// unmoved, signed like COUNT. Going forward, a last line without a newline
/// that point crossed counts as moved.
pub(crate) fn forward_line(text: &Text, pos: u64, count: i64) -> Result<(i64, u64), ReadError> {
    if count > 0 {
        let mut position = pos;
        let mut found = 0;
        while found < count {
            match text.find_newline_forward(position)? {
                Some(newline) => {
                    position = newline + 1;
                    found += 1;
                }
                None => {
                    position = text.len();
                    break;
                }
            }
        }
        let mut shortage = count - found;
        if shortage > 0 && position != pos && text.byte_at(position - 1)? != b'\n' {
            shortage -= 1;
        }
        return Ok((shortage, position));
    }
    let mut position = line_start(text, pos)?;
    let mut remaining = count.unsigned_abs();
    while remaining > 0 && position > 0 {
        position = line_start(text, position - 1)?;
        remaining -= 1;
    }
    let shortage = i64::try_from(remaining).expect("at most the magnitude of an i64 count");
    Ok((-shortage, position))
}

/// Emacs `current-column`: display columns from the line start to `pos`.
pub(crate) fn current_column(text: &Text, pos: u64, tab_width: u64) -> Result<u64, ReadError> {
    let start = line_start(text, pos)?;
    let mut bytes = Vec::new();
    text.read(start..pos, &mut bytes)?;
    let tab_width = usize::try_from(tab_width).expect("tab_width is validated small");
    let mut column = 0;
    for (_, unit) in display_units(&bytes) {
        column += unit_width(&unit, column, tab_width);
    }
    Ok(column as u64)
}

/// Emacs `move-to-column` without FORCE, from the line start `start`: stops
/// at the first unit boundary at or past `goal`, so a unit that spans the
/// goal column is passed, or at the end of a short line.
pub(crate) fn move_to_column(
    text: &Text,
    start: u64,
    goal: u64,
    tab_width: u64,
) -> Result<u64, ReadError> {
    let end = line_end(text, start)?;
    let mut bytes = Vec::new();
    text.read(start..end, &mut bytes)?;
    let tab_width = usize::try_from(tab_width).expect("tab_width is validated small");
    let mut column = 0;
    for (offset, unit) in display_units(&bytes) {
        if column as u64 >= goal {
            return Ok(start + offset as u64);
        }
        column += unit_width(&unit, column, tab_width);
    }
    Ok(end)
}

/// `M-e`: Emacs `forward-sentence`, with `sentence-end-double-space` t (the
/// default): a sentence ends at `.`, `?` or `!` (and any closing quotes or
/// brackets) when followed by end of line, a tab, or two spaces.
pub(super) fn forward_sentence(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let count = ctx.prefix_arg.numeric_value();
    move_by_sentences(ctx, count)
}

/// `M-a`: Emacs `backward-sentence`.
pub(super) fn backward_sentence(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let count = ctx.prefix_arg.numeric_value();
    move_by_sentences(ctx, count.saturating_neg())
}

fn move_by_sentences(ctx: &mut CommandContext, count: i64) -> Result<(), CommandError> {
    let text = ctx.buffer().text.clone();
    let mut point = ctx.point();
    for _ in 0..count.unsigned_abs() {
        if count > 0 {
            // `end-of-paragraph-text` steps a character forward at the buffer
            // end, which signals, as Emacs's `forward-sentence` does there.
            if point == text.len() {
                ctx.set_point(point);
                return Err(CommandError::EndOfBuffer);
            }
            point = forward_one_sentence(&text, point)?;
        } else {
            point = backward_one_sentence(&text, point)?;
        }
    }
    ctx.set_point(point);
    Ok(())
}

const SENTENCE_PUNCTUATION: [u8; 3] = [b'.', b'?', b'!'];
/// The ASCII closing quotes and brackets of Emacs's `sentence-end`.
const SENTENCE_CLOSERS: [u8; 5] = [b']', b'"', b'\'', b')', b'}'];

fn is_sentence_punctuation(byte: u8) -> bool {
    SENTENCE_PUNCTUATION.contains(&byte)
}

fn is_sentence_closer(byte: u8) -> bool {
    SENTENCE_CLOSERS.contains(&byte)
}

/// A sentence-end match starting at `pos`: its landing position (right after
/// the punctuation and closers, where `forward-sentence` stops) and the end
/// of the whole match including the trailing whitespace.
fn sentence_end_at(text: &Text, pos: u64, len: u64) -> Result<Option<(u64, u64)>, ReadError> {
    if !is_sentence_punctuation(text.byte_at(pos)?) {
        return Ok(None);
    }
    let mut landing = pos + 1;
    while landing < len && is_sentence_closer(text.byte_at(landing)?) {
        landing += 1;
    }
    if !sentence_separator(text, landing, len)? {
        return Ok(None);
    }
    let mut end = landing;
    while end < len && matches!(text.byte_at(end)?, b' ' | b'\t' | b'\n') {
        end += 1;
    }
    Ok(Some((landing, end)))
}

/// The `\($\| $\|\t\|  \)` separator of `sentence-end`: end of line, a tab,
/// a space before end of line, or two spaces.
fn sentence_separator(text: &Text, pos: u64, len: u64) -> Result<bool, ReadError> {
    if pos >= len {
        return Ok(true);
    }
    match text.byte_at(pos)? {
        b'\n' | b'\t' => Ok(true),
        b' ' => Ok(pos + 1 >= len || matches!(text.byte_at(pos + 1)?, b' ' | b'\n')),
        _ => Ok(false),
    }
}

fn forward_one_sentence(text: &Text, point: u64) -> Result<u64, ReadError> {
    let bound = end_of_paragraph_text(text, point)?;
    let mut pos = point;
    while pos < bound {
        if let Some((landing, _)) = sentence_end_at(text, pos, text.len())? {
            return Ok(landing.min(bound));
        }
        pos += 1;
    }
    Ok(bound)
}

fn backward_one_sentence(text: &Text, point: u64) -> Result<u64, ReadError> {
    let floor = start_of_paragraph_text(text, point)?;
    let mut matches = Vec::new();
    let mut pos = floor;
    while pos < point {
        if let Some((_, end)) = sentence_end_at(text, pos, text.len())? {
            matches.push((pos, end));
        }
        pos += 1;
    }
    // Emacs's `re-search-backward` twice: the nearest match before point, and
    // when that one straddles point, the one before it.
    let Some(&(first_pos, first_end)) = matches.last() else {
        return Ok(floor);
    };
    if first_end < point {
        return Ok(first_end);
    }
    match matches.iter().rev().find(|(pos, _)| *pos < first_pos) {
        Some(&(_, end)) => Ok(end),
        None => Ok(floor),
    }
}

/// Emacs `end-of-paragraph-text`: the end of the current paragraph's text,
/// before any trailing newline.
fn end_of_paragraph_text(text: &Text, start: u64) -> Result<u64, ReadError> {
    let mut opoint = start;
    loop {
        let mut pos = forward_paragraph(text, opoint)?;
        if pos > 0 && text.byte_at(pos - 1)? == b'\n' {
            pos -= 1;
        }
        if pos <= opoint {
            let next = (pos + 1).min(text.len());
            if next < text.len() {
                opoint = next;
                continue;
            }
            return Ok(next);
        }
        return Ok(pos);
    }
}

/// Emacs `start-of-paragraph-text`: the first non-blank position of the
/// current paragraph.
fn start_of_paragraph_text(text: &Text, start: u64) -> Result<u64, ReadError> {
    let mut opoint = start;
    loop {
        let npoint = backward_paragraph(text, opoint)?;
        let mut pos = npoint;
        while pos < text.len() && matches!(text.byte_at(pos)?, b' ' | b'\t' | b'\n') {
            pos += 1;
        }
        if pos >= opoint && npoint < opoint && npoint > 0 {
            opoint = npoint;
            continue;
        }
        return Ok(pos);
    }
}

/// Emacs `forward-paragraph` by one: to the start of the blank-line run that
/// follows the current paragraph, or the buffer end.
fn forward_paragraph(text: &Text, pos: u64) -> Result<u64, ReadError> {
    let mut line = line_start(text, pos)?;
    while line < text.len() && line_is_blank(text, line)? {
        line = next_line_start(text, line)?;
    }
    while line < text.len() && !line_is_blank(text, line)? {
        line = next_line_start(text, line)?;
    }
    Ok(line)
}

/// Emacs `forward-paragraph` with a negative argument: to the start of the
/// current paragraph's block, skipping the blank lines above it.
fn backward_paragraph(text: &Text, pos: u64) -> Result<u64, ReadError> {
    let mut line = line_start(text, pos)?;
    while line > 0 {
        let previous = line_start(text, line - 1)?;
        if line_is_blank(text, previous)? {
            line = previous;
        } else {
            break;
        }
    }
    while line > 0 {
        let previous = line_start(text, line - 1)?;
        if line_is_blank(text, previous)? {
            break;
        }
        line = previous;
    }
    Ok(line)
}

/// The start of the line after the one containing `line`, or the buffer end.
fn next_line_start(text: &Text, line: u64) -> Result<u64, ReadError> {
    Ok(match text.find_newline_forward(line)? {
        Some(newline) => newline + 1,
        None => text.len(),
    })
}

/// Whether the line starting at `line` holds only spaces and tabs, matching
/// Emacs `paragraph-separate` `"[ \t]*$"`.
pub(crate) fn line_is_blank(text: &Text, line: u64) -> Result<bool, ReadError> {
    let end = line_end(text, line)?;
    for chunk in text.chunks(line..end) {
        if !chunk?
            .bytes()
            .iter()
            .all(|byte| matches!(byte, b' ' | b'\t'))
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `M-g g` / `M-g M-g`: Emacs `goto-line`. A bare command reads the line
/// number in the minibuffer; a numeric prefix argument goes there directly.
pub(super) fn goto_line(ctx: &mut CommandContext) -> Result<(), CommandError> {
    match ctx.prefix_arg {
        PrefixArg::Numeric(line) => {
            let point = ctx.point();
            super::mark::push_mark(ctx, point, true, false);
            let text = ctx.buffer().text.clone();
            ctx.set_point(line_number_position(&text, line)?);
            Ok(())
        }
        _ => {
            let client = ctx.client;
            ctx.editor().read_goto_line(client);
            Ok(())
        }
    }
}

/// The position of the start of line `line` (1-based), as Emacs `goto-line`
/// reaches it: `(goto-char (point-min)) (forward-line (1- line))`.
pub(crate) fn line_number_position(text: &Text, line: i64) -> Result<u64, ReadError> {
    let steps = line.saturating_sub(1);
    Ok(forward_line(text, 0, steps)?.1)
}

/// Emacs `scan_words`: the position after COUNT words (before, when
/// negative), or `None` when the buffer edge comes first.
pub(crate) fn scan_words(text: &Text, from: u64, count: i64) -> Result<Option<u64>, ReadError> {
    let mut position = from;
    let mut remaining = count;
    while remaining > 0 {
        let mut previous = loop {
            let Some((character, length)) = char_after(text, position)? else {
                return Ok(None);
            };
            position += length;
            if is_word(character) {
                break character;
            }
        };
        while let Some((character, length)) = char_after(text, position)? {
            if !is_word(character) || word_boundary(previous, character) {
                break;
            }
            position += length;
            previous = character;
        }
        remaining -= 1;
    }
    while remaining < 0 {
        let mut next = loop {
            let Some((character, length)) = char_before(text, position)? else {
                return Ok(None);
            };
            position -= length;
            if is_word(character) {
                break character;
            }
        };
        while let Some((character, length)) = char_before(text, position)? {
            if !is_word(character) || word_boundary(character, next) {
                break;
            }
            position -= length;
            next = character;
        }
        remaining += 1;
    }
    Ok(Some(position))
}

/// A position inside a grapheme cluster moves to the cluster's end (or its
/// start, going backward), since point never rests inside one.
pub(crate) fn snap_to_unit(text: &Text, pos: u64, forward: bool) -> Result<u64, ReadError> {
    let Some(start) = previous_unit_start(text, pos)? else {
        return Ok(pos);
    };
    let end = next_unit_end(text, start)?.expect("a unit starts before pos");
    if end <= pos {
        return Ok(pos);
    }
    Ok(if forward { end } else { start })
}

/// Characters before `end`, as Emacs counts positions: each invalid byte is
/// one character.
pub(crate) fn char_count(text: &Text, end: u64) -> Result<u64, ReadError> {
    let mut count = 0;
    let mut pending: Vec<u8> = Vec::new();
    for chunk in text.chunks(0..end) {
        pending.extend_from_slice(chunk?.bytes());
        let consumed = count_chars(&pending, false, &mut count);
        pending.drain(..consumed);
    }
    count_chars(&pending, true, &mut count);
    Ok(count)
}

/// Counts the characters in `bytes` and returns how many bytes it consumed;
/// a sequence cut short at the end is left for the next chunk unless this
/// is the last.
fn count_chars(bytes: &[u8], last: bool, count: &mut u64) -> usize {
    let mut index = 0;
    loop {
        match std::str::from_utf8(&bytes[index..]) {
            Ok(valid) => {
                *count += valid.chars().count() as u64;
                return bytes.len();
            }
            Err(error) => {
                let valid = &bytes[index..index + error.valid_up_to()];
                *count += std::str::from_utf8(valid)
                    .expect("valid_up_to marks valid UTF-8")
                    .chars()
                    .count() as u64;
                index += error.valid_up_to();
                match error.error_len() {
                    Some(invalid) => {
                        *count += invalid as u64;
                        index += invalid;
                    }
                    None if last => {
                        *count += (bytes.len() - index) as u64;
                        return bytes.len();
                    }
                    None => return index,
                }
            }
        }
    }
}

/// The byte offset of the 0-based character position `target`, or the
/// buffer end when there are fewer characters.
fn char_to_byte(text: &Text, target: u64) -> Result<u64, ReadError> {
    let mut position = 0;
    let mut remaining = target;
    while remaining > 0 {
        let Some((_, length)) = char_after(text, position)? else {
            break;
        };
        position += length;
        remaining -= 1;
    }
    Ok(position)
}

/// The character starting at `pos` and its length in bytes, or `None` at
/// the end. An invalid byte is one character, as Emacs's raw bytes are.
pub(crate) fn char_after(text: &Text, pos: u64) -> Result<Option<(u32, u64)>, ReadError> {
    if pos >= text.len() {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    text.read(pos..text.len().min(pos + UTF8_MAX_LEN), &mut bytes)?;
    let expected = utf8_sequence_length(bytes[0]);
    if let Some(sequence) = bytes.get(..expected)
        && let Ok(decoded) = std::str::from_utf8(sequence)
        && let Some(character) = decoded.chars().next()
    {
        return Ok(Some((u32::from(character), expected as u64)));
    }
    Ok(Some((RAW_BYTE_BASE + u32::from(bytes[0]), 1)))
}

/// The character ending at `pos` and its length in bytes, or `None` at the
/// start.
pub(crate) fn char_before(text: &Text, pos: u64) -> Result<Option<(u32, u64)>, ReadError> {
    if pos == 0 {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    text.read(pos.saturating_sub(UTF8_MAX_LEN)..pos, &mut bytes)?;
    for length in 1..=bytes.len() {
        let sequence = &bytes[bytes.len() - length..];
        if utf8_sequence_length(sequence[0]) == length
            && let Ok(decoded) = std::str::from_utf8(sequence)
            && let Some(character) = decoded.chars().next()
        {
            return Ok(Some((u32::from(character), length as u64)));
        }
    }
    let last = *bytes.last().expect("pos > 0 reads at least one byte");
    Ok(Some((RAW_BYTE_BASE + u32::from(last), 1)))
}

/// The length a UTF-8 sequence starting with `lead` has; 0 for a byte that
/// cannot start one.
fn utf8_sequence_length(lead: u8) -> usize {
    match lead {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 0,
    }
}

fn in_ranges(ranges: &[(u32, u32)], code: u32) -> bool {
    let index = ranges.partition_point(|(start, _)| *start <= code);
    index > 0 && ranges[index - 1].1 >= code
}

/// Word syntax in Emacs's `standard-syntax-table`; raw bytes are words too.
pub(crate) fn is_word(code: u32) -> bool {
    code > LARGEST_SCALAR || in_ranges(WORD, code)
}

fn script(code: u32) -> Option<u16> {
    let index = SCRIPTS.partition_point(|(start, _, _)| *start <= code);
    (index > 0 && SCRIPTS[index - 1].1 >= code).then(|| SCRIPTS[index - 1].2)
}

fn in_category(code: u32, category: Category) -> bool {
    let ranges = match category {
        Category::CombiningDiacritic => COMBINING_DIACRITIC,
        Category::Chinese => CHINESE,
        Category::Hiragana => HIRAGANA,
        Category::Katakana => KATAKANA,
    };
    in_ranges(ranges, code)
}

/// Emacs `word_boundary_p`: two adjacent word characters split a word when
/// their scripts differ, unless a combining rule joins them; within one
/// script, only a separating rule splits them.
fn word_boundary(first: u32, second: u32) -> bool {
    let same_script = script(first) == script(second);
    let (rules, default): (&[CategoryRule], bool) = if same_script {
        (&WORD_SEPARATING_CATEGORIES, false)
    } else {
        (&WORD_COMBINING_CATEGORIES, true)
    };
    let matches = |(left, right): &CategoryRule| {
        let left_matches = left
            .is_none_or(|category| in_category(first, category) && !in_category(second, category));
        let right_matches = right
            .is_none_or(|category| !in_category(first, category) && in_category(second, category));
        left_matches && right_matches
    };
    if rules.iter().any(matches) {
        !default
    } else {
        default
    }
}

/// The end of the display unit that starts at `pos`, or `None` at the end.
pub(crate) fn next_unit_end(text: &Text, pos: u64) -> Result<Option<u64>, ReadError> {
    let length = text.len();
    if pos >= length {
        return Ok(None);
    }
    let mut window = UNIT_WINDOW;
    let mut bytes = Vec::new();
    loop {
        let end = length.min(pos.saturating_add(window));
        bytes.clear();
        text.read(pos..end, &mut bytes)?;
        // A character cut by the window end would read as invalid bytes and
        // fake a boundary, so it is left out. A grapheme boundary depends
        // only on what precedes it, so any other boundary is final.
        let usable = if end < length {
            without_partial_tail(&bytes)
        } else {
            &bytes
        };
        let mut units = display_units(usable);
        units
            .next()
            .expect("a window has at least one whole character");
        if let Some((second, _)) = units.next() {
            return Ok(Some(pos + second as u64));
        }
        if end == length {
            return Ok(Some(end));
        }
        window = window.saturating_mul(2);
    }
}

/// The start of the display unit that ends at `pos`, or `None` at the start.
pub(crate) fn previous_unit_start(text: &Text, pos: u64) -> Result<Option<u64>, ReadError> {
    if pos == 0 {
        return Ok(None);
    }
    let mut window = UNIT_WINDOW;
    let mut bytes = Vec::new();
    loop {
        let start = pos.saturating_sub(window);
        bytes.clear();
        text.read(start..pos, &mut bytes)?;
        // Segmentation is exact from a line start or the buffer start. From
        // anywhere else the first unit may be a fragment, so it only counts
        // once a later unit confirms the boundary.
        let (origin, exact) = match bytes.iter().rposition(|byte| *byte == b'\n') {
            Some(newline) if newline + 1 == bytes.len() => return Ok(Some(pos - 1)),
            Some(newline) => (newline + 1, true),
            None if start == 0 => (0, true),
            None => (leading_continuation_bytes(&bytes), false),
        };
        let mut unit_count = 0;
        let mut last_start = origin;
        for (unit_start, _) in display_units(&bytes[origin..]) {
            unit_count += 1;
            last_start = origin + unit_start;
        }
        if exact || unit_count >= 2 {
            return Ok(Some(start + last_start as u64));
        }
        window = window.saturating_mul(2);
    }
}

/// `bytes` without a trailing UTF-8 sequence that the window end cut short.
fn without_partial_tail(bytes: &[u8]) -> &[u8] {
    const MAX_CONTINUATION: usize = 3;
    for back in 1..=MAX_CONTINUATION.min(bytes.len()) {
        let byte = bytes[bytes.len() - back];
        if (0x80..0xc0).contains(&byte) {
            continue;
        }
        let needed = match byte {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => 1,
        };
        return if needed > back {
            &bytes[..bytes.len() - back]
        } else {
            bytes
        };
    }
    bytes
}

/// UTF-8 continuation bytes at the start of a window that began inside a
/// character; at most three.
fn leading_continuation_bytes(bytes: &[u8]) -> usize {
    const MAX_CONTINUATION: usize = 3;
    bytes
        .iter()
        .take(MAX_CONTINUATION)
        .take_while(|byte| (0x80..0xc0).contains(*byte))
        .count()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use emx_text::Sources;

    use super::*;

    fn text_of(content: &[u8]) -> Text {
        Text::from_original(Sources::memory(Arc::from(content)))
    }

    fn boundaries_forward(text: &Text) -> Vec<u64> {
        let mut positions = vec![0];
        let mut pos = 0;
        while let Some(next) = next_unit_end(text, pos).expect("memory read") {
            positions.push(next);
            pos = next;
        }
        positions
    }

    fn boundaries_backward(text: &Text) -> Vec<u64> {
        let mut positions = vec![text.len()];
        let mut pos = text.len();
        while let Some(previous) = previous_unit_start(text, pos).expect("memory read") {
            positions.push(previous);
            pos = previous;
        }
        positions.reverse();
        positions
    }

    #[test]
    fn forward_and_backward_agree_on_mixed_text() {
        let mut content = "a\u{e9}e\u{301}\n\u{1f600}\tx\r\n".as_bytes().to_vec();
        content.push(0xff);
        content.extend_from_slice("\u{4e2d}z".as_bytes());
        let text = text_of(&content);
        let forward = boundaries_forward(&text);
        assert_eq!(forward, boundaries_backward(&text));
        assert_eq!(forward, vec![0, 1, 3, 6, 7, 11, 12, 13, 14, 15, 16, 19, 20]);
    }

    #[test]
    fn long_clusters_and_long_lines_are_handled() {
        let mut content = "e".to_string();
        content.push_str(&"\u{301}".repeat(100));
        content.push_str(&"y".repeat(500));
        let text = text_of(content.as_bytes());
        assert_eq!(next_unit_end(&text, 0).expect("read"), Some(201));
        assert_eq!(previous_unit_start(&text, 201).expect("read"), Some(0));
        assert_eq!(previous_unit_start(&text, 701).expect("read"), Some(700));
    }
}
