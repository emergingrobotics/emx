//! Indentation and comments in buffers of a bundled language (DESIGN
//! section 4, tier 2), by per-language rules rather than a major mode's
//! Lisp: a line is indented like the previous non-blank one, a level more
//! after an opening bracket (or a colon in Python and YAML), a level less
//! when it starts with a closing one. Buffers in no language keep Emacs's
//! Fundamental-mode behavior.

use std::ops::Range;

use emx_text::{ReadError, Text};
use emx_types::Face;

use super::r#move::{current_column, line_end, line_start};
use super::{CommandContext, CommandError};
use crate::syntax::Language;

/// Emacs `comment-column`.
pub(crate) const COMMENT_COLUMN: u64 = 32;
const NO_COMMENT_SYNTAX: &str = "No comment syntax is defined";
const OPENERS: &[u8] = b"{[(";
const CLOSERS: &[u8] = b"}])";
/// Emacs `comment-padding`: what follows the comment start.
const COMMENT_PADDING: &[u8] = b" ";

pub(crate) fn buffer_language(ctx: &CommandContext) -> Option<Language> {
    ctx.buffer().language.language
}

fn is_blank(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t')
}

/// The end of the line's indentation.
fn indentation_end(text: &Text, line: u64) -> Result<u64, ReadError> {
    let end = line_end(text, line)?;
    let mut at = line;
    while at < end && is_blank(text.byte_at(at)?) {
        at += 1;
    }
    Ok(at)
}

fn in_comment_or_string(ctx: &CommandContext, pos: u64) -> bool {
    matches!(
        ctx.buffer().faces.face_at(pos),
        Some(Face::Comment | Face::String)
    )
}

/// The column `language` indents the line starting at `line` to.
fn wanted_column(ctx: &CommandContext, line: u64, language: Language) -> Result<u64, CommandError> {
    let text = &ctx.buffer().text;
    let tab_width = ctx.tab_width();
    let (width, _) = language.indentation();
    let mut previous = line;
    let found = loop {
        if previous == 0 {
            break None;
        }
        previous = line_start(text, previous - 1)?;
        if indentation_end(text, previous)? < line_end(text, previous)? {
            break Some(previous);
        }
    };
    let Some(previous) = found else {
        return Ok(0);
    };
    let start = indentation_end(text, previous)?;
    let base = current_column(text, start, tab_width)?;
    // The last character of code on that line, a comment after it left out.
    let mut last = line_end(text, previous)?;
    let mut last_code = None;
    while last > start {
        last -= 1;
        let byte = text.byte_at(last)?;
        if is_blank(byte) || matches!(ctx.buffer().faces.face_at(last), Some(Face::Comment)) {
            continue;
        }
        last_code = Some((byte, last));
        break;
    }
    let opens = last_code.is_some_and(|(byte, at)| {
        !in_comment_or_string(ctx, at)
            && (OPENERS.contains(&byte) || (byte == b':' && language.opens_block_with_colon()))
    });
    let mut column = base + if opens { width } else { 0 };
    let first = indentation_end(text, line)?;
    if first < line_end(text, line)?
        && CLOSERS.contains(&text.byte_at(first)?)
        && !in_comment_or_string(ctx, first)
    {
        column = column.saturating_sub(width);
    }
    Ok(column)
}

/// Whitespace reaching `column`: tabs first when the language indents with
/// them.
fn indentation(column: u64, tabs: bool, tab_width: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut spaces = column;
    if tabs && tab_width > 0 {
        bytes.extend(std::iter::repeat_n(b'\t', (column / tab_width) as usize));
        spaces = column % tab_width;
    }
    bytes.extend(std::iter::repeat_n(b' ', spaces as usize));
    bytes
}

/// Indents the line point is on by the language's rule. Point inside the
/// indentation goes to its end; point in the text stays on its character.
pub(crate) fn indent_line(
    ctx: &mut CommandContext,
    language: Language,
) -> Result<(), CommandError> {
    ctx.check_editable()?;
    let text = ctx.buffer().text.clone();
    let point = ctx.point();
    let line = line_start(&text, point)?;
    let old_end = indentation_end(&text, line)?;
    let column = wanted_column(ctx, line, language)?;
    let (_, tabs) = language.indentation();
    let wanted = indentation(column, tabs, ctx.tab_width());
    let mut current = Vec::new();
    text.read(line..old_end, &mut current)?;
    if current != wanted {
        ctx.delete_with_point(line..old_end, point.max(old_end))?;
        ctx.set_point(line);
        ctx.insert(&wanted)?;
    }
    let new_end = line + wanted.len() as u64;
    if point <= old_end {
        ctx.set_point(new_end);
    } else {
        ctx.set_point(point - (old_end - line) + wanted.len() as u64);
    }
    Ok(())
}

/// Emacs `comment-dwim`: with an active region, comment its lines (or
/// uncomment them when each is a comment); else go to the line's comment,
/// or start one.
pub(crate) fn comment_dwim(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let start = buffer_language(ctx)
        .and_then(Language::comment_start)
        .ok_or_else(|| ctx.error(NO_COMMENT_SYNTAX))?
        .as_bytes();
    ctx.check_editable()?;
    if ctx.use_region() {
        let region = ctx.region()?;
        comment_or_uncomment_region(ctx, region, start)?;
        ctx.request_deactivate_mark();
        return Ok(());
    }
    let text = ctx.buffer().text.clone();
    let line = line_start(&text, ctx.point())?;
    let end = line_end(&text, line)?;
    if let Some(comment) = find_comment(ctx, &text, line..end, start)? {
        let mut body = comment + start.len() as u64;
        while body < end && is_blank(text.byte_at(body)?) {
            body += 1;
        }
        ctx.set_point(body);
        return Ok(());
    }
    if indentation_end(&text, line)? == end {
        if let Some(language) = buffer_language(ctx) {
            indent_line(ctx, language)?;
        }
    } else {
        // `comment-indent`: at `comment-column`, or one space after code
        // that reaches past it.
        let mut code_end = end;
        while code_end > line && is_blank(text.byte_at(code_end - 1)?) {
            code_end -= 1;
        }
        if code_end < end {
            ctx.delete_with_point(code_end..end, end)?;
        }
        ctx.set_point(code_end);
        let tab_width = ctx.tab_width();
        let column = current_column(&ctx.buffer().text, code_end, tab_width)?;
        let padding = if column < COMMENT_COLUMN {
            COMMENT_COLUMN - column
        } else {
            1
        };
        ctx.insert(&vec![b' '; padding as usize])?;
    }
    let mut comment = start.to_vec();
    comment.extend_from_slice(COMMENT_PADDING);
    ctx.insert(&comment)
}

/// Where a comment starts on the line, if it has one: the comment start
/// that highlighting says is a comment, or failing faces, the first one
/// outside a string.
fn find_comment(
    ctx: &CommandContext,
    text: &Text,
    line: Range<u64>,
    start: &[u8],
) -> Result<Option<u64>, CommandError> {
    let mut bytes = Vec::new();
    text.read(line.clone(), &mut bytes)?;
    let faces = &ctx.buffer().faces;
    let highlighted = faces.overlapping(line.clone()).next().is_some();
    for (offset, window) in bytes.windows(start.len()).enumerate() {
        if window != start {
            continue;
        }
        let pos = line.start + offset as u64;
        let face = faces.face_at(pos);
        if (highlighted && face == Some(Face::Comment))
            || (!highlighted && face != Some(Face::String))
        {
            return Ok(Some(pos));
        }
    }
    Ok(None)
}

/// Emacs `comment-or-uncomment-region` with `comment-style` `indent`:
/// blank lines are left alone; the others get the comment start at their
/// least indentation, unless every one is already a comment, when the
/// comment starts come off.
fn comment_or_uncomment_region(
    ctx: &mut CommandContext,
    region: Range<u64>,
    start: &[u8],
) -> Result<(), CommandError> {
    let text = ctx.buffer().text.clone();
    let mut lines = Vec::new();
    let mut line = line_start(&text, region.start)?;
    loop {
        let end = line_end(&text, line)?;
        let indented = indentation_end(&text, line)?;
        if indented < end {
            lines.push((line, indented, end));
        }
        if end >= text.len() || end + 1 >= region.end.max(region.start + 1) {
            break;
        }
        line = end + 1;
    }
    let commented = |text: &Text, at: u64| -> Result<bool, ReadError> {
        let mut bytes = Vec::new();
        let to = (at + start.len() as u64).min(text.len());
        text.read(at..to, &mut bytes)?;
        Ok(bytes == start)
    };
    let mut all_commented = !lines.is_empty();
    for (_, indented, _) in &lines {
        if !commented(&text, *indented)? {
            all_commented = false;
            break;
        }
    }
    let least = lines
        .iter()
        .map(|(line, indented, _)| indented - line)
        .min()
        .unwrap_or(0);
    let point = ctx.point();
    for (line, indented, end) in lines.into_iter().rev() {
        if all_commented {
            let mut cut = indented + start.len() as u64;
            if cut < end && ctx.buffer().text.byte_at(cut)? == COMMENT_PADDING[0] {
                cut += 1;
            }
            ctx.delete_with_point(indented..cut, point)?;
        } else {
            let mut comment = start.to_vec();
            comment.extend_from_slice(COMMENT_PADDING);
            let at = line + least;
            ctx.replace_range(at..at, &comment, point)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indentation_uses_tabs_only_when_asked() {
        assert_eq!(indentation(6, false, 8), b"      ");
        assert_eq!(indentation(16, true, 8), b"\t\t");
        assert_eq!(indentation(10, true, 8), b"\t  ");
    }
}
