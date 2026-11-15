use emx_text::{ReadError, Text};
use emx_types::{Key, KeyCode, Mods};

use super::kill::{kill_forward_chars, step_chars};
use super::r#move::{
    char_before, current_column, is_word, line_end, line_is_blank, line_start, move_to_column,
    scan_words, word_target,
};
use super::{CommandContext, CommandError, PrefixArg, undo_error};

/// Commands acting on fewer characters than this amalgamate their undo
/// groups (Emacs `undo-auto-amalgamate`).
const AMALGAMATE_BELOW: i64 = 2;
const UNDO: &str = "undo";
const NEWLINE: u8 = b'\n';

/// Emacs `self-insert-command`: inserts the typed character N times as one
/// change. Single insertions amalgamate into one undo group, as Emacs's
/// `undo-auto-amalgamate` does for N below 2, called before the edit.
pub(super) fn self_insert_command(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let count = ctx.prefix_arg.numeric_value();
    if count < 0 {
        return Err(ctx.error(format!("Negative repetition argument {count}")));
    }
    if count < AMALGAMATE_BELOW {
        let repeated = ctx.last_command == Some(ctx.this_command);
        ctx.buffer_mut().amalgamate_last_change(repeated);
    }
    let Some(character) = ctx.key.and_then(inserted_character) else {
        ctx.ding();
        return Ok(());
    };
    let mut encoded = [0; 4];
    let encoded = character.encode_utf8(&mut encoded).as_bytes();
    // The count comes from typed digits, so it is bounded before anything is
    // allocated: one insertion may not exceed what a buffer holds in memory.
    let limit = ctx.editor().config().in_memory_max;
    let total = u64::try_from(count)
        .ok()
        .and_then(|count| count.checked_mul(encoded.len() as u64))
        .filter(|total| *total <= limit)
        .ok_or_else(|| {
            ctx.error(format!(
                "Repetition argument {count} is too large (limit {limit} bytes)"
            ))
        })?;
    let repeat = usize::try_from(total / encoded.len() as u64)
        .expect("a total within in_memory_max fits in usize");
    ctx.insert(&encoded.repeat(repeat))
}

/// Only an unmodified character key inserts; Emacs rings the bell when the
/// invoking event is not a character.
fn inserted_character(key: Key) -> Option<char> {
    match key {
        Key {
            code: KeyCode::Char(character),
            mods,
        } if mods == Mods::default() => Some(character),
        _ => None,
    }
}

/// Emacs `newline` in Fundamental mode with `electric-indent-mode` on (the
/// Emacs 29 default): inserts N newlines through `self-insert-command`,
/// then deletes the blanks before the last one and indents the new line
/// like the line above it.
pub(super) fn newline(ctx: &mut CommandContext) -> Result<(), CommandError> {
    const NEWLINE: u8 = b'\n';
    ctx.check_editable()?;
    let count = ctx.prefix_arg.numeric_value();
    if ctx.prefix_arg != PrefixArg::None && count < 0 {
        return Err(ctx.error("Repetition argument has to be non-negative"));
    }
    if count < AMALGAMATE_BELOW {
        let repeated = ctx.last_command == Some(ctx.this_command);
        ctx.buffer_mut().amalgamate_last_change(repeated);
    }
    if count == 0 {
        return Ok(());
    }
    let limit = ctx.editor().config().in_memory_max;
    let count = u64::try_from(count)
        .ok()
        .filter(|count| *count <= limit)
        .ok_or_else(|| {
            ctx.error(format!(
                "Repetition argument {count} is too large (limit {limit} bytes)"
            ))
        })?;
    let count = usize::try_from(count).expect("a count within in_memory_max fits in usize");
    ctx.insert(&vec![NEWLINE; count])?;
    electric_indent_after_newline(ctx)?;
    // A language's mode indents the new line by its own rules.
    if let Some(language) = super::syntax::buffer_language(ctx) {
        super::syntax::indent_line(ctx, language)?;
    }
    Ok(())
}

/// `electric-indent-post-self-insert-function` after a newline, where
/// `indent-line-function` is `indent-relative`: it cannot reindent the old
/// line, so only its trailing blanks go, and the new line is indented to
/// the previous line's indentation (none if that line is blank).
fn electric_indent_after_newline(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let newline = ctx.point() - 1;
    let text = ctx.buffer().text.clone();
    let blanks_start = skip_blanks_backward(&text, newline)?;
    if blanks_start < newline {
        // `delete-horizontal-space` runs with point at the newline.
        ctx.delete_with_point(blanks_start..newline, newline)?;
    }
    let text = ctx.buffer().text.clone();
    let line = line_start(&text, ctx.point())?;
    let column = if line == 0 {
        0
    } else {
        let previous = line_start(&text, line - 1)?;
        let indentation_end = skip_blanks_forward(&text, previous, line - 1)?;
        if indentation_end == line - 1 {
            0
        } else {
            current_column(&text, indentation_end, ctx.tab_width())?
        }
    };
    // Point is at the start of the new line, so `indent-according-to-mode`
    // takes the branch that leaves point after the indentation.
    indent_line_to(ctx, column)
}

/// Emacs `indent-line-to`: replace the current line's indentation so it
/// reaches `column`, and leave point after it.
fn indent_line_to(ctx: &mut CommandContext, column: u64) -> Result<(), CommandError> {
    let tab_width = ctx.tab_width();
    let text = ctx.buffer().text.clone();
    let line = line_start(&text, ctx.point())?;
    let indentation_end = skip_blanks_forward(&text, line, line_end(&text, line)?)?;
    ctx.set_point(indentation_end);
    let current = current_column(&text, indentation_end, tab_width)?;
    if current < column {
        if column - (current / tab_width) * tab_width >= tab_width {
            let spaces_start = skip_backward(&text, indentation_end, b" ")?;
            if spaces_start < indentation_end {
                ctx.delete_with_point(spaces_start..indentation_end, indentation_end)?;
            }
        }
        indent_to(ctx, column)?;
    } else if current > column {
        let cut = move_to_column_force(ctx, line, column)?;
        let text = ctx.buffer().text.clone();
        let indentation_end = skip_blanks_forward(&text, line, line_end(&text, line)?)?;
        ctx.delete_with_point(cut..indentation_end, indentation_end)?;
        ctx.set_point(cut);
    }
    Ok(())
}

/// Emacs `indent-to` with MINIMUM 0: tabs (when `indent_tabs_mode`) then
/// spaces from point's column to `column`, leaving point after them.
fn indent_to(ctx: &mut CommandContext, column: u64) -> Result<(), CommandError> {
    let tab_width = ctx.tab_width();
    let tabs_allowed = ctx.indent_tabs_mode();
    let text = ctx.buffer().text.clone();
    let mut from = current_column(&text, ctx.point(), tab_width)?;
    if from >= column {
        return Ok(());
    }
    let mut indentation = Vec::new();
    if tabs_allowed {
        let tabs = column / tab_width - from / tab_width;
        if tabs > 0 {
            indentation.extend(std::iter::repeat_n(b'\t', tabs as usize));
            from = (column / tab_width) * tab_width;
        }
    }
    indentation.extend(std::iter::repeat_n(b' ', (column - from) as usize));
    ctx.insert(&indentation)
}

/// Emacs `move-to-column` with FORCE t inside indentation: a tab spanning
/// `goal` is split into spaces up to `goal` and indentation to its old
/// end. Returns the position of `goal`.
fn move_to_column_force(
    ctx: &mut CommandContext,
    line: u64,
    goal: u64,
) -> Result<u64, CommandError> {
    let tab_width = ctx.tab_width();
    let text = ctx.buffer().text.clone();
    let position = move_to_column(&text, line, goal, tab_width)?;
    let column = current_column(&text, position, tab_width)?;
    if column <= goal || position == line || text.byte_at(position - 1)? != b'\t' {
        return Ok(position);
    }
    let tab = position - 1;
    let tab_column = current_column(&text, tab, tab_width)?;
    if tab_column >= goal {
        return Ok(position);
    }
    ctx.set_point(tab);
    let spaces = vec![b' '; (goal - tab_column) as usize];
    ctx.insert(&spaces)?;
    let goal_position = ctx.point();
    ctx.delete_with_point(goal_position..goal_position + 1, goal_position)?;
    indent_to(ctx, column)?;
    Ok(goal_position)
}

/// The start of the run of spaces and tabs that ends at `pos`.
fn skip_blanks_backward(text: &Text, pos: u64) -> Result<u64, ReadError> {
    skip_backward(text, pos, b" \t")
}

/// The start of the run of bytes from `set` that ends at `pos`.
fn skip_backward(text: &Text, pos: u64, set: &[u8]) -> Result<u64, ReadError> {
    let mut position = pos;
    while position > 0 && set.contains(&text.byte_at(position - 1)?) {
        position -= 1;
    }
    Ok(position)
}

/// The end of the run of spaces and tabs that starts at `pos`, at most `limit`.
fn skip_blanks_forward(text: &Text, pos: u64, limit: u64) -> Result<u64, ReadError> {
    let mut position = pos;
    for chunk in text.chunks(pos..limit) {
        let chunk = chunk?;
        let blanks = chunk
            .bytes()
            .iter()
            .take_while(|byte| matches!(byte, b' ' | b'\t'))
            .count();
        position += blanks as u64;
        if blanks < chunk.bytes().len() {
            break;
        }
    }
    Ok(position)
}

/// `C-d`: Emacs `delete-char`. A prefix argument makes it kill. It never
/// deletes the region.
pub(super) fn delete_char(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let count = ctx.prefix_arg.numeric_value();
    let kill = ctx.prefix_arg != PrefixArg::None;
    delete_chars(ctx, count, kill)
}

/// `DEL`: Emacs `delete-backward-char`. With an active non-empty region and
/// N = 1 it deletes the region instead; a prefix argument makes it kill.
pub(super) fn delete_backward_char(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let count = ctx.prefix_arg.numeric_value();
    if count == 1 && ctx.use_region() {
        let region = ctx.region()?;
        let point = ctx.point();
        ctx.delete_with_point(region, point)?;
        return Ok(());
    }
    let kill = ctx.prefix_arg != PrefixArg::None;
    delete_chars(ctx, count.saturating_neg(), kill)
}

/// Emacs `Fdelete_char`: `count` characters after point (before it when
/// negative). Without `kill`, running past a buffer edge signals before
/// anything is deleted.
fn delete_chars(ctx: &mut CommandContext, count: i64, kill: bool) -> Result<(), CommandError> {
    if count.unsigned_abs() < AMALGAMATE_BELOW.unsigned_abs() {
        let repeated = ctx.last_command == Some(ctx.this_command);
        ctx.buffer_mut().amalgamate_last_change(repeated);
    }
    if kill {
        return kill_forward_chars(ctx, count);
    }
    let text = ctx.buffer().text.clone();
    let point = ctx.point();
    let Some(other_end) = step_chars(&text, point, count)? else {
        return Err(if count < 0 {
            CommandError::BeginningOfBuffer
        } else {
            CommandError::EndOfBuffer
        });
    };
    ctx.delete_with_point(point.min(other_end)..point.max(other_end), point)?;
    Ok(())
}

/// Emacs `undo`. An active region (or a non-numeric prefix argument) limits
/// undo to changes inside the region; a numeric argument is a repeat count.
pub(super) fn undo(ctx: &mut CommandContext) -> Result<(), CommandError> {
    ctx.check_editable()?;
    let continuing = ctx.last_command_is(UNDO);
    let argument = ctx.prefix_arg;
    let count = match argument {
        PrefixArg::Numeric(count) => usize::try_from(count).unwrap_or(0),
        _ => 1,
    };
    let in_region =
        ctx.region_active() || matches!(argument, PrefixArg::Raw(_) | PrefixArg::Negative);
    let region = if in_region {
        match ctx.mark() {
            Some(_) => Some(ctx.region()?),
            // Emacs reads the region only to start a new undo sequence.
            None if continuing => None,
            None => return Err(ctx.region().expect_err("there is no mark")),
        }
    } else {
        None
    };
    ctx.this_command = UNDO;
    let (buffer, recorder) = ctx.buffer_and_recorder();
    let result = match region {
        Some(range) => buffer.undo_in_region(range, count, continuing, recorder),
        None => buffer.undo(count, continuing, recorder),
    };
    let outcome = result.map_err(|error| undo_error(ctx.buffer(), error))?;
    if let Some(point) = outcome.point {
        ctx.set_point(point);
    }
    ctx.message(outcome.message);
    Ok(())
}

/// The three letter-case conversions `M-u`, `M-l` and `M-c` share.
#[derive(Debug, Clone, Copy)]
enum LetterCase {
    Up,
    Down,
    Capitalize,
}

pub(super) fn upcase_word(ctx: &mut CommandContext) -> Result<(), CommandError> {
    casify_word(ctx, LetterCase::Up)
}

pub(super) fn downcase_word(ctx: &mut CommandContext) -> Result<(), CommandError> {
    casify_word(ctx, LetterCase::Down)
}

pub(super) fn capitalize_word(ctx: &mut CommandContext) -> Result<(), CommandError> {
    casify_word(ctx, LetterCase::Capitalize)
}

/// Emacs `casify-word`: convert the following word (or ARG words) in the
/// chosen case; a negative argument converts the preceding words but leaves
/// point where it was.
fn casify_word(ctx: &mut CommandContext, case: LetterCase) -> Result<(), CommandError> {
    ctx.check_editable()?;
    let count = ctx.prefix_arg.numeric_value();
    let origin = ctx.point();
    let target = word_target(ctx, count)?;
    let (begin, end) = (origin.min(target), origin.max(target));
    if begin < end {
        let mut bytes = Vec::new();
        ctx.buffer().text.read(begin..end, &mut bytes)?;
        let converted = convert_case(&bytes, case);
        // Emacs's case commands modify the buffer even when the text is
        // unchanged (it rewrites the region in place), so always replace.
        ctx.replace_range(begin..end, &converted, begin)?;
    }
    ctx.set_point(if count < 0 { origin } else { target });
    Ok(())
}

/// Emacs `upcase-region`/`downcase-region`/`capitalize-region` over `bytes`.
fn convert_case(bytes: &[u8], case: LetterCase) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return bytes
            .iter()
            .map(|byte| match case {
                LetterCase::Up => byte.to_ascii_uppercase(),
                LetterCase::Down => byte.to_ascii_lowercase(),
                LetterCase::Capitalize => *byte,
            })
            .collect();
    };
    let mut out = String::with_capacity(text.len());
    let mut in_word = false;
    for character in text.chars() {
        match case {
            LetterCase::Up => out.extend(character.to_uppercase()),
            LetterCase::Down => out.extend(character.to_lowercase()),
            LetterCase::Capitalize => {
                if is_word(u32::from(character)) {
                    if in_word {
                        out.extend(character.to_lowercase());
                    } else {
                        out.extend(character.to_uppercase());
                    }
                    in_word = true;
                } else {
                    out.push(character);
                    in_word = false;
                }
            }
        }
    }
    out.into_bytes()
}

/// `C-t`: Emacs `transpose-chars`. With no argument at the end of a line it
/// first steps back, so the two characters it drags are the last two.
pub(super) fn transpose_chars(ctx: &mut CommandContext) -> Result<(), CommandError> {
    ctx.check_editable()?;
    let text = ctx.buffer().text.clone();
    let point = ctx.point();
    if ctx.prefix_arg == PrefixArg::None && at_line_end(&text, point)? {
        let Some((_, length)) = char_before(&text, point)? else {
            return Err(CommandError::BeginningOfBuffer);
        };
        ctx.set_point(point - length);
    }
    let count = ctx.prefix_arg.numeric_value();
    transpose(ctx, Object::Char, count)
}

/// `M-t`: Emacs `transpose-words`.
pub(super) fn transpose_words(ctx: &mut CommandContext) -> Result<(), CommandError> {
    ctx.check_editable()?;
    let count = ctx.prefix_arg.numeric_value();
    transpose(ctx, Object::Word, count)
}

/// Whether Emacs's `forward-char` or `forward-word` names the unit being
/// transposed.
#[derive(Debug, Clone, Copy)]
enum Object {
    Char,
    Word,
}

/// Move `count` objects from `pos`; `None` only for characters past a buffer
/// edge. Words clamp at the edges, as `forward-word` does.
fn move_object(
    text: &Text,
    object: Object,
    pos: u64,
    count: i64,
) -> Result<Option<u64>, ReadError> {
    match object {
        Object::Char => step_chars(text, pos, count),
        Object::Word => Ok(Some(
            scan_words(text, pos, count)?.unwrap_or(if count > 0 { text.len() } else { 0 }),
        )),
    }
}

/// Emacs `transpose-subr`: swap the object before point with the object
/// `count` units ahead (preceding, for a negative argument).
fn transpose(ctx: &mut CommandContext, object: Object, count: i64) -> Result<(), CommandError> {
    let text = ctx.buffer().text.clone();
    let point = ctx.point();
    let edge = |step: i64| {
        if step < 0 {
            CommandError::BeginningOfBuffer
        } else {
            CommandError::EndOfBuffer
        }
    };
    let step = |pos: u64, n: i64| -> Result<u64, CommandError> {
        move_object(&text, object, pos, n)?.ok_or_else(|| edge(n))
    };
    let (pos1, pos2, final_point) = if count == 0 {
        let mark = ctx.mark().ok_or_else(|| ctx.error(NO_MARK))?;
        let a1 = step(point, 1)?;
        let pos1 = (step(a1, -1)?, a1);
        let a2 = step(mark, 1)?;
        let pos2 = (step(a2, -1)?, a2);
        (pos1, pos2, None)
    } else if count > 0 {
        let a1 = step(point, -1)?;
        let b1 = step(a1, 1)?;
        let c = step(b1, count)?;
        let d = step(c, -count)?;
        ((a1, b1), (c, d), Some(c))
    } else {
        let a1 = step(point, -1)?;
        let b1 = step(a1, 1)?;
        let c = step(a1, count)?;
        let d = step(c, -count)?;
        ((a1, b1), (c, d), Some(c + (b1.max(a1) - b1.min(a1))))
    };
    let mut first = (pos1.0.min(pos1.1), pos1.0.max(pos1.1));
    let mut second = (pos2.0.min(pos2.1), pos2.0.max(pos2.1));
    if first.0 > second.0 {
        std::mem::swap(&mut first, &mut second);
    }
    if first.1 > second.0 {
        // For a nonzero argument Emacs leaves point where the object motion
        // ended before signaling; the zero (mark) case restores point.
        if final_point.is_some() {
            ctx.set_point(pos2.1);
        }
        return Err(ctx.error("Don\u{2019}t have two things to transpose"));
    }
    let mut rebuilt = Vec::new();
    read_into(&text, second.0..second.1, &mut rebuilt)?;
    read_into(&text, first.1..second.0, &mut rebuilt)?;
    read_into(&text, first.0..first.1, &mut rebuilt)?;
    ctx.replace_range(first.0..second.1, &rebuilt, first.0)?;
    match final_point {
        Some(target) => ctx.set_point(target),
        None => {
            // `transpose-subr`'s ARG 0 branch exchanges point and mark.
            let mark = ctx.mark().expect("checked above");
            let point = ctx.point();
            ctx.set_mark_position(point);
            ctx.set_point(mark);
            ctx.activate_mark();
        }
    }
    Ok(())
}

fn read_into(text: &Text, range: std::ops::Range<u64>, out: &mut Vec<u8>) -> Result<(), ReadError> {
    text.read(range, out)
}

/// `C-o`: Emacs `open-line`. Inserts N newlines after point and leaves point
/// before them.
pub(super) fn open_line(ctx: &mut CommandContext) -> Result<(), CommandError> {
    ctx.check_editable()?;
    let count = ctx.prefix_arg.numeric_value();
    if count < 0 {
        return Err(ctx.error("Repetition argument has to be non-negative"));
    }
    if count == 0 {
        return Ok(());
    }
    let count = usize::try_from(count).expect("a positive count fits in usize");
    let point = ctx.point();
    ctx.insert(&vec![NEWLINE; count])?;
    ctx.set_point(point);
    Ok(())
}

/// `C-x C-o`: Emacs `delete-blank-lines`.
pub(super) fn delete_blank_lines(ctx: &mut CommandContext) -> Result<(), CommandError> {
    ctx.check_editable()?;
    let text = ctx.buffer().text.clone();
    let line = line_start(&text, ctx.point())?;
    let this_blank = line_is_blank(&text, line)?;
    // `singleblank`: a lone blank line, with non-blank (or a buffer edge) on
    // both sides.
    let single_blank = this_blank
        && !blank_line_before_blank(&text, line)?
        && (line == 0 || !previous_line_blank(&text, line)?);
    // Delete preceding blank lines, and this one too when it stands alone.
    if this_blank {
        let text = ctx.buffer().text.clone();
        let bol = line_start(&text, ctx.point())?;
        let from = if single_blank {
            forward_line_from(&text, bol, 1)?
        } else {
            bol
        };
        let to = match search_backward_nonblank(&text, from)? {
            Some(position) => forward_line_from(&text, position, 1)?,
            None => 0,
        };
        let (low, high) = (from.min(to), from.max(to));
        if low < high {
            ctx.delete_with_point(low..high, low)?;
        }
        ctx.set_point(low);
    }
    // Delete following blank lines, unless the lone blank line stands alone;
    // Emacs does this inside `save-excursion`, so point is preserved.
    if !(this_blank && single_blank) {
        let saved = ctx.point();
        let text = ctx.buffer().text.clone();
        let after = forward_line_from(&text, line_end(&text, saved)?, 1)?;
        let to = match search_forward_nonblank(&text, after)? {
            Some(position) => line_start(&text, position)?,
            None => text.len(),
        };
        if after < to {
            ctx.delete_with_point(after..to, after)?;
        }
        ctx.set_point(saved);
    }
    // A blank line that reaches end of buffer: drop it (Emacs's final
    // `(looking-at "^[ \t]*\n\\'")` special case).
    let text = ctx.buffer().text.clone();
    let point = ctx.point();
    if at_line_start(&text, point)? && blank_line_to_eob(&text, point)? {
        ctx.delete_with_point(point..text.len(), point)?;
        ctx.set_point(point);
    }
    Ok(())
}

/// Emacs `(looking-at "[ \t]*\n[ \t]*$")`: this blank line is followed by
/// another blank line.
fn blank_line_before_blank(text: &Text, line: u64) -> Result<bool, ReadError> {
    let end = line_end(text, line)?;
    if end >= text.len() {
        return Ok(false);
    }
    line_is_blank(text, end + 1)
}

fn previous_line_blank(text: &Text, line: u64) -> Result<bool, ReadError> {
    if line == 0 {
        return Ok(false);
    }
    let previous = line_start(text, line - 1)?;
    line_is_blank(text, previous)
}

/// Emacs `(looking-at "^[ \t]*\n\\'")`: from `pos`, only spaces and tabs then
/// a newline that ends the buffer.
fn blank_line_to_eob(text: &Text, pos: u64) -> Result<bool, ReadError> {
    let mut position = pos;
    while position < text.len() && matches!(text.byte_at(position)?, b' ' | b'\t') {
        position += 1;
    }
    Ok(position + 1 == text.len() && text.byte_at(position)? == NEWLINE)
}

/// Emacs `forward-line`, returning only the position reached.
fn forward_line_from(text: &Text, pos: u64, count: i64) -> Result<u64, ReadError> {
    Ok(super::forward_line(text, pos, count)?.1)
}

/// The position after the last non-blank character strictly before `pos`,
/// matching Emacs `(re-search-backward "[^ \t\n]")` which leaves point on
/// that character.
fn search_backward_nonblank(text: &Text, pos: u64) -> Result<Option<u64>, ReadError> {
    let mut position = pos;
    while position > 0 {
        let byte = text.byte_at(position - 1)?;
        if !matches!(byte, b' ' | b'\t' | NEWLINE) {
            return Ok(Some(position - 1));
        }
        position -= 1;
    }
    Ok(None)
}

/// The position of the first non-blank character at or after `pos`, matching
/// Emacs `(re-search-forward "[^ \t\n]")` which leaves point after it; the
/// caller takes the line start, so the position itself is enough.
fn search_forward_nonblank(text: &Text, pos: u64) -> Result<Option<u64>, ReadError> {
    let length = text.len();
    let mut position = pos;
    while position < length {
        let byte = text.byte_at(position)?;
        if !matches!(byte, b' ' | b'\t' | NEWLINE) {
            return Ok(Some(position));
        }
        position += 1;
    }
    Ok(None)
}

const NO_MARK: &str = "No mark set in this buffer";

fn at_line_end(text: &Text, pos: u64) -> Result<bool, ReadError> {
    Ok(pos == text.len() || text.byte_at(pos)? == NEWLINE)
}

fn at_line_start(text: &Text, pos: u64) -> Result<bool, ReadError> {
    Ok(pos == 0 || text.byte_at(pos - 1)? == NEWLINE)
}

/// `TAB` in Fundamental mode: Emacs `indent-for-tab-command` runs
/// `indent-relative`, spacing out under the next indent point of the
/// previous non-blank line, or to the next tab stop when there is none.
pub(super) fn indent_for_tab_command(ctx: &mut CommandContext) -> Result<(), CommandError> {
    if let Some(language) = super::syntax::buffer_language(ctx) {
        return super::syntax::indent_line(ctx, language);
    }
    ctx.check_editable()?;
    let indent = relative_indent_column(ctx)?;
    match indent {
        Some(column) => {
            let opoint = ctx.point();
            let tab_width = ctx.tab_width();
            let text = ctx.buffer().text.clone();
            let current = current_column(&text, opoint, tab_width)?;
            if current < column {
                indent_to_column(ctx, column)?;
            }
            // `indent-relative` never moves point left of where it started.
            if opoint > ctx.point() {
                ctx.set_point(opoint);
            }
        }
        None => tab_to_tab_stop(ctx)?,
    }
    Ok(())
}

/// The column of the next indent point of the previous non-blank line past
/// the current column, as Emacs `indent-relative` finds it; `None` when
/// there is no such line or indent point.
fn relative_indent_column(ctx: &mut CommandContext) -> Result<Option<u64>, CommandError> {
    let tab_width = ctx.tab_width();
    let text = &ctx.buffer().text;
    let start_column = current_column(text, ctx.point(), tab_width)?;
    let line = line_start(text, ctx.point())?;
    // `(re-search-backward "^[^\n]")`: a previous line that is not empty.
    let Some(previous) = previous_nonempty_line(text, line)? else {
        return Ok(None);
    };
    let end = line_end(text, previous)?;
    let mut scan = move_to_column(text, previous, start_column, tab_width)?;
    if current_column(text, scan, tab_width)? > start_column
        && let Some((_, length)) = char_before(text, scan)?
    {
        scan -= length;
    }
    // Skip to the next whitespace then over it, to the following indent point.
    if !at_whitespace(text, scan)? {
        scan = skip_to_whitespace(text, scan, end)?;
    }
    scan = skip_whitespace(text, scan, end)?;
    if scan == end {
        return Ok(None);
    }
    Ok(Some(current_column(text, scan, tab_width)?))
}

/// The start of the nearest line before `line` that is not empty (its first
/// character is not a newline), as Emacs `(re-search-backward "^[^\n]")`.
fn previous_nonempty_line(text: &Text, line: u64) -> Result<Option<u64>, ReadError> {
    let mut current = line;
    while current > 0 {
        let previous = line_start(text, current - 1)?;
        if previous < text.len() && text.byte_at(previous)? != NEWLINE {
            return Ok(Some(previous));
        }
        current = previous;
    }
    Ok(None)
}

fn at_whitespace(text: &Text, pos: u64) -> Result<bool, ReadError> {
    if pos >= text.len() {
        return Ok(false);
    }
    Ok(matches!(text.byte_at(pos)?, b' ' | b'\t'))
}

fn skip_to_whitespace(text: &Text, pos: u64, limit: u64) -> Result<u64, ReadError> {
    let mut position = pos;
    while position < limit && !matches!(text.byte_at(position)?, b' ' | b'\t') {
        position += 1;
    }
    Ok(position)
}

fn skip_whitespace(text: &Text, pos: u64, limit: u64) -> Result<u64, ReadError> {
    let mut position = pos;
    while position < limit && matches!(text.byte_at(position)?, b' ' | b'\t') {
        position += 1;
    }
    Ok(position)
}

/// Emacs `indent-to`, leaving point after the inserted whitespace, used when
/// point is already at the indentation being set.
fn indent_to_column(ctx: &mut CommandContext, column: u64) -> Result<(), CommandError> {
    let tab_width = ctx.tab_width();
    let tabs_allowed = ctx.indent_tabs_mode();
    let text = ctx.buffer().text.clone();
    let from = current_column(&text, ctx.point(), tab_width)?;
    if from >= column {
        return Ok(());
    }
    let mut indentation = Vec::new();
    let mut at = from;
    if tabs_allowed {
        let tabs = column / tab_width - from / tab_width;
        if tabs > 0 {
            indentation.extend(std::iter::repeat_n(b'\t', tabs as usize));
            at = (column / tab_width) * tab_width;
        }
    }
    indentation.extend(std::iter::repeat_n(b' ', (column - at) as usize));
    ctx.insert(&indentation)
}

/// Emacs `tab-to-tab-stop` with the default (empty) `tab-stop-list`: indent
/// to the next multiple of `tab-width`.
fn tab_to_tab_stop(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let tab_width = ctx.tab_width();
    let text = ctx.buffer().text.clone();
    let column = current_column(&text, ctx.point(), tab_width)?;
    let next = (column / tab_width + 1) * tab_width;
    indent_to_column(ctx, next)
}

/// Emacs `undo-redo`: undo the last undos.
pub(super) fn undo_redo(ctx: &mut CommandContext) -> Result<(), CommandError> {
    ctx.check_editable()?;
    let count = usize::try_from(ctx.prefix_arg.numeric_value()).unwrap_or(0);
    ctx.this_command = UNDO;
    let (buffer, recorder) = ctx.buffer_and_recorder();
    let result = buffer.undo_redo(count, recorder);
    let outcome = result.map_err(|error| undo_error(ctx.buffer(), error))?;
    if let Some(point) = outcome.point {
        ctx.set_point(point);
    }
    ctx.message(outcome.message);
    Ok(())
}
