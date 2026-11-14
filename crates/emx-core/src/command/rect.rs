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
