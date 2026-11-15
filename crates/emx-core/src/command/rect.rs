//! Rectangle commands operating on the column span between mark and point:
//! `kill-rectangle` (`C-x r k`), `yank-rectangle` (`C-x r y`) and
//! `string-rectangle` (`C-x r t`). Columns and tabs follow Emacs's
//! `apply-on-rectangle`.

use std::ops::Range;

use emx_text::{ReadError, Text};

use super::r#move::{current_column, line_start, move_to_column};
use super::{CommandContext, CommandError, forward_line, push_mark};

const NEWLINE: u8 = b'\n';

/// `C-x r k`: Emacs `kill-rectangle`. Deletes the rectangle and saves it as
/// the last killed rectangle.
pub(super) fn kill_rectangle(ctx: &mut CommandContext) -> Result<(), CommandError> {
    ctx.check_editable()?;
    let (mark, point) = ctx.mark_and_point()?;
    let (start, end) = (mark.min(point), mark.max(point));
    let text = ctx.buffer().text.clone();
    let tab_width = ctx.editor().config().tab_width;
    let (start_col, end_col) = rectangle_columns(&text, start, end, tab_width)?;
    let mut extracted = Vec::new();
    let mut deletions = Vec::new();
    for line in rectangle_line_starts(&text, start, end)? {
        let (range, piece) = delete_extract_line(&text, line, start_col, end_col, tab_width)?;
        extracted.push(piece);
        if let Some(range) = range {
            deletions.push(range);
        }
    }
    ctx.editor().set_killed_rectangle(extracted);
    // Delete bottom-to-top so earlier line positions stay valid; the point
    // and mark markers move with the deletions, as Emacs's `save-excursion`
    // leaves them.
    for range in deletions.into_iter().rev() {
        let at = range.start;
        ctx.delete_with_point(range, at)?;
    }
    ctx.request_deactivate_mark();
    Ok(())
}

/// `C-x r y`: Emacs `yank-rectangle`.
pub(super) fn yank_rectangle(ctx: &mut CommandContext) -> Result<(), CommandError> {
    ctx.check_editable()?;
    let rectangle = ctx.editor().killed_rectangle().unwrap_or_default();
    insert_rectangle(ctx, &rectangle)
}

/// Emacs `insert-rectangle`: the first line at point, each next line a line
/// below at the same column, padding short lines. Mark at the upper-left
/// corner, point at the lower-right.
fn insert_rectangle(ctx: &mut CommandContext, rectangle: &[Vec<u8>]) -> Result<(), CommandError> {
    let origin = ctx.point();
    let tab_width = ctx.editor().config().tab_width;
    let text = ctx.buffer().text.clone();
    let insert_column = current_column(&text, origin, tab_width)?;
    push_mark(ctx, origin, true, false);
    for (index, line) in rectangle.iter().enumerate() {
        if index > 0 {
            let text = ctx.buffer().text.clone();
            let next = forward_line(&text, ctx.point(), 1)?.1;
            ctx.set_point(next);
            let text = ctx.buffer().text.clone();
            if !at_line_start(&text, ctx.point())? {
                ctx.insert(&[NEWLINE])?;
            }
            pad_to_column(ctx, insert_column)?;
        }
        ctx.insert(line)?;
    }
    Ok(())
}

/// `C-x r t`: Emacs `string-rectangle`. Reads the replacement string, then
/// applies it; the application is `apply_string_rectangle`.
pub(super) fn string_rectangle(ctx: &mut CommandContext) -> Result<(), CommandError> {
    ctx.check_editable()?;
    let (mark, point) = ctx.mark_and_point()?;
    let (start, end) = (mark.min(point), mark.max(point));
    let client = ctx.client;
    ctx.editor().read_string_rectangle(client, start, end);
    Ok(())
}

/// Emacs `string-rectangle` once the string is read: replace each line's
/// rectangle span with `string`, leaving point after the string on the last
/// line.
pub(crate) fn apply_string_rectangle(
    ctx: &mut CommandContext,
    start: u64,
    end: u64,
    string: &[u8],
) -> Result<(), CommandError> {
    ctx.check_editable()?;
    let text = ctx.buffer().text.clone();
    let tab_width = ctx.editor().config().tab_width;
    let (start_col, end_col) = rectangle_columns(&text, start, end, tab_width)?;
    let line_count = rectangle_line_count(&text, start, end)?;
    let mut line = line_start(&text, start)?;
    let mut final_point = line;
    for _ in 0..line_count {
        final_point = string_rectangle_line(ctx, line, start_col, end_col, string, tab_width)?;
        let text = ctx.buffer().text.clone();
        match text.find_newline_forward(line)? {
            Some(newline) => line = newline + 1,
            None => break,
        }
    }
    ctx.set_point(final_point);
    Ok(())
}

/// Emacs `string-rectangle-line`: pad to `start_col`, delete the span, insert
/// the string; returns point after the string.
fn string_rectangle_line(
    ctx: &mut CommandContext,
    line: u64,
    start_col: u64,
    end_col: u64,
    string: &[u8],
    tab_width: u64,
) -> Result<u64, CommandError> {
    let text = ctx.buffer().text.clone();
    let reached = move_to_column(&text, line, start_col, tab_width)?;
    let reached_col = current_column(&text, reached, tab_width)?;
    if reached_col < start_col {
        ctx.set_point(reached);
        ctx.insert(&vec![b' '; (start_col - reached_col) as usize])?;
    }
    let text = ctx.buffer().text.clone();
    let begin = move_to_column(&text, line, start_col, tab_width)?;
    let finish = move_to_column(&text, line, end_col, tab_width)?;
    if begin < finish {
        ctx.delete_with_point(begin..finish, begin)?;
    }
    ctx.set_point(begin);
    ctx.insert(string)?;
    Ok(ctx.point())
}

/// Emacs `delete-extract-rectangle-line` with no fill: the range to delete
/// (if any) and the extracted text, padded for short lines.
fn delete_extract_line(
    text: &Text,
    line: u64,
    start_col: u64,
    end_col: u64,
    tab_width: u64,
) -> Result<(Option<Range<u64>>, Vec<u8>), ReadError> {
    let begin = move_to_column(text, line, start_col, tab_width)?;
    let begin_col = current_column(text, begin, tab_width)?;
    if begin_col < start_col {
        return Ok((None, vec![b' '; (end_col - start_col) as usize]));
    }
    let finish = move_to_column(text, line, end_col, tab_width)?;
    let finish_col = current_column(text, finish, tab_width)?;
    let mut piece = Vec::new();
    text.read(begin..finish, &mut piece)?;
    if finish_col < end_col {
        piece.extend(std::iter::repeat_n(b' ', (end_col - finish_col) as usize));
    }
    Ok((Some(begin..finish), piece))
}

/// The left and right columns of the rectangle with corners at `start` and
/// `end`, left column first.
fn rectangle_columns(
    text: &Text,
    start: u64,
    end: u64,
    tab_width: u64,
) -> Result<(u64, u64), ReadError> {
    let start_col = current_column(text, start, tab_width)?;
    let end_col = current_column(text, end, tab_width)?;
    Ok((start_col.min(end_col), start_col.max(end_col)))
}

/// The number of lines the rectangle spans, as Emacs's `apply-on-rectangle`
/// loops over them.
fn rectangle_line_count(text: &Text, start: u64, end: u64) -> Result<u64, ReadError> {
    let first = line_start(text, start)?;
    let last = line_start(text, end)?;
    let mut count = 1;
    let mut pos = first;
    while pos < last {
        match text.find_newline_forward(pos)? {
            Some(newline) if newline < last => {
                count += 1;
                pos = newline + 1;
            }
            _ => break,
        }
    }
    Ok(count)
}

/// The start position of each line of the rectangle, top to bottom.
fn rectangle_line_starts(text: &Text, start: u64, end: u64) -> Result<Vec<u64>, ReadError> {
    let count = rectangle_line_count(text, start, end)?;
    let mut line = line_start(text, start)?;
    let mut starts = Vec::with_capacity(count as usize);
    for _ in 0..count {
        starts.push(line);
        match text.find_newline_forward(line)? {
            Some(newline) => line = newline + 1,
            None => break,
        }
    }
    Ok(starts)
}

fn at_line_start(text: &Text, pos: u64) -> Result<bool, ReadError> {
    Ok(pos == 0 || text.byte_at(pos - 1)? == NEWLINE)
}

/// Emacs `move-to-column` with FORCE: pad with spaces to reach `column`.
fn pad_to_column(ctx: &mut CommandContext, column: u64) -> Result<(), CommandError> {
    let tab_width = ctx.editor().config().tab_width;
    let text = ctx.buffer().text.clone();
    let current = current_column(&text, ctx.point(), tab_width)?;
    if current < column {
        ctx.insert(&vec![b' '; (column - current) as usize])?;
    }
    Ok(())
}

/// Inserts `bytes` at `at` without moving point past them: markers there,
/// point included, stay before the text, as under Emacs `save-excursion`.
fn insert_at(ctx: &mut CommandContext, at: u64, bytes: &[u8]) -> Result<(), CommandError> {
    if bytes.is_empty() {
        return Ok(());
    }
    let (buffer, recorder) = ctx.buffer_and_recorder();
    buffer
        .insert(at, bytes, recorder)
        .map_err(|error| super::read_only_error(buffer, error))
}

/// Emacs `indent-to` from column `from` at `at`: tabs (when
/// `indent-tabs-mode`) then spaces up to `column`.
fn indentation_bytes(from: u64, column: u64, tab_width: u64, tabs: bool) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut at = from;
    if tabs && tab_width > 0 {
        let count = column / tab_width - from / tab_width;
        if count > 0 {
            bytes.extend(std::iter::repeat_n(b'\t', count as usize));
            at = (column / tab_width) * tab_width;
        }
    }
    bytes.extend(std::iter::repeat_n(
        b' ',
        column.saturating_sub(at) as usize,
    ));
    bytes
}

/// The rectangle between mark and point: its columns and line starts.
fn rectangle(ctx: &CommandContext) -> Result<(u64, u64, u64, Vec<u64>), CommandError> {
    let (mark, point) = ctx.mark_and_point()?;
    let (start, end) = (mark.min(point), mark.max(point));
    let text = &ctx.buffer().text;
    let (start_col, end_col) = rectangle_columns(text, start, end, ctx.tab_width())?;
    Ok((
        start,
        start_col,
        end_col,
        rectangle_line_starts(text, start, end)?,
    ))
}

/// The start of a line `index` lines below the first, after edits to the
/// lines above moved it.
fn nth_line(text: &Text, first: u64, index: usize) -> Result<u64, ReadError> {
    let mut line = first;
    for _ in 0..index {
        match text.find_newline_forward(line)? {
            Some(newline) => line = newline + 1,
            None => break,
        }
    }
    Ok(line)
}

/// `C-x r d`: Emacs `delete-rectangle`, which keeps no copy.
pub(super) fn delete_rectangle(ctx: &mut CommandContext) -> Result<(), CommandError> {
    ctx.check_editable()?;
    let (_, start_col, end_col, lines) = rectangle(ctx)?;
    let text = ctx.buffer().text.clone();
    let tab_width = ctx.tab_width();
    let mut deletions = Vec::new();
    for line in lines {
        if let (Some(range), _) = delete_extract_line(&text, line, start_col, end_col, tab_width)? {
            deletions.push(range);
        }
    }
    for range in deletions.into_iter().rev() {
        let at = range.start;
        ctx.delete_with_point(range, at)?;
    }
    ctx.request_deactivate_mark();
    Ok(())
}

/// `C-x r M-w`: Emacs `copy-rectangle-as-kill`.
pub(super) fn copy_rectangle_as_kill(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (_, start_col, end_col, lines) = rectangle(ctx)?;
    let text = ctx.buffer().text.clone();
    let tab_width = ctx.tab_width();
    let mut extracted = Vec::new();
    for line in lines {
        extracted.push(delete_extract_line(&text, line, start_col, end_col, tab_width)?.1);
    }
    ctx.editor().set_killed_rectangle(extracted);
    ctx.request_deactivate_mark();
    Ok(())
}

/// `C-x r c`: Emacs `clear-rectangle`: the rectangle becomes blank; a line
/// ending inside it is cut there instead.
pub(super) fn clear_rectangle(ctx: &mut CommandContext) -> Result<(), CommandError> {
    ctx.check_editable()?;
    let (_, start_col, end_col, lines) = rectangle(ctx)?;
    let tab_width = ctx.tab_width();
    let tabs = ctx.editor().config().indent_tabs_mode;
    let first = lines.first().copied().unwrap_or(0);
    for index in 0..lines.len() {
        let text = ctx.buffer().text.clone();
        let line = nth_line(&text, first, index)?;
        let begin = move_to_column(&text, line, start_col, tab_width)?;
        if current_column(&text, begin, tab_width)? != start_col {
            continue;
        }
        let line_end = super::line_end(&text, line)?;
        if current_column(&text, line_end, tab_width)? <= end_col {
            if begin < line_end {
                ctx.delete_with_point(begin..line_end, begin)?;
            }
            continue;
        }
        let finish = move_to_column(&text, line, end_col, tab_width)?;
        let reached = current_column(&text, finish, tab_width)?;
        ctx.delete_with_point(begin..finish, begin)?;
        insert_at(
            ctx,
            begin,
            &indentation_bytes(start_col, reached, tab_width, tabs),
        )?;
    }
    ctx.request_deactivate_mark();
    Ok(())
}

/// `C-x r o`: Emacs `open-rectangle`: blank space fills the rectangle,
/// pushing the text right; point goes to its upper-left corner.
pub(super) fn open_rectangle(ctx: &mut CommandContext) -> Result<(), CommandError> {
    ctx.check_editable()?;
    let (start, start_col, end_col, lines) = rectangle(ctx)?;
    let tab_width = ctx.tab_width();
    let tabs = ctx.editor().config().indent_tabs_mode;
    let first = lines.first().copied().unwrap_or(0);
    let corner = ctx
        .buffer_mut()
        .create_marker(start, crate::marker::InsertionType::StayBefore);
    for index in 0..lines.len() {
        let text = ctx.buffer().text.clone();
        let line = nth_line(&text, first, index)?;
        let begin = move_to_column(&text, line, start_col, tab_width)?;
        if current_column(&text, begin, tab_width)? != start_col
            || begin == super::line_end(&text, line)?
        {
            continue;
        }
        insert_at(
            ctx,
            begin,
            &indentation_bytes(start_col, end_col, tab_width, tabs),
        )?;
    }
    let corner_position = ctx.buffer().marker_pos(corner);
    ctx.buffer_mut().delete_marker(corner);
    ctx.set_point(corner_position);
    ctx.request_deactivate_mark();
    Ok(())
}

/// `C-x r N`: Emacs `rectangle-number-lines` from 1: each line of the
/// rectangle gets its number, right-aligned to the widest, and a space at
/// the rectangle's left column.
pub(super) fn rectangle_number_lines(ctx: &mut CommandContext) -> Result<(), CommandError> {
    ctx.check_editable()?;
    let (_, start_col, _, lines) = rectangle(ctx)?;
    let tab_width = ctx.tab_width();
    let width = lines.len().to_string().len();
    let first = lines.first().copied().unwrap_or(0);
    for index in 0..lines.len() {
        let text = ctx.buffer().text.clone();
        let line = nth_line(&text, first, index)?;
        let begin = move_to_column(&text, line, start_col, tab_width)?;
        let reached = current_column(&text, begin, tab_width)?;
        let mut bytes = vec![b' '; start_col.saturating_sub(reached) as usize];
        bytes.extend(format!("{:>width$} ", index + 1).into_bytes());
        insert_at(ctx, begin, &bytes)?;
    }
    ctx.request_deactivate_mark();
    Ok(())
}
