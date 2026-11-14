use emx_text::{ReadError, Text};
use emx_types::{Key, KeyCode, Mods};

use super::kill::{kill_forward_chars, step_chars};
use super::r#move::{current_column, line_end, line_start, move_to_column};
use super::{CommandContext, CommandError, PrefixArg, undo_error};

/// Commands acting on fewer characters than this amalgamate their undo
/// groups (Emacs `undo-auto-amalgamate`).
const AMALGAMATE_BELOW: i64 = 2;
const UNDO: &str = "undo";

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
    electric_indent_after_newline(ctx)
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
            current_column(&text, indentation_end, ctx.editor().config().tab_width)?
        }
    };
    // Point is at the start of the new line, so `indent-according-to-mode`
    // takes the branch that leaves point after the indentation.
    indent_line_to(ctx, column)
}

/// Emacs `indent-line-to`: replace the current line's indentation so it
/// reaches `column`, and leave point after it.
fn indent_line_to(ctx: &mut CommandContext, column: u64) -> Result<(), CommandError> {
    let tab_width = ctx.editor().config().tab_width;
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
    let tab_width = ctx.editor().config().tab_width;
    let tabs_allowed = ctx.editor().config().indent_tabs_mode;
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
    let tab_width = ctx.editor().config().tab_width;
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
