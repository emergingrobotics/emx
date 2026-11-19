use emx_text::{ReadError, Text};

use super::mark::push_mark;
use super::r#move::{
    char_after, char_before, char_count, forward_line, line_end, line_start, word_target,
};
use super::{CommandContext, CommandError, PrefixArg};
use crate::killring::KillRingError;

/// Every kill command calls itself this, so the next kill appends.
const KILL_REGION: &str = "kill-region";
const YANK: &str = "yank";
/// Emacs `yank` sets `this-command` to t until it finishes, so a failed
/// yank is not taken for one by the next `yank-pop`.
const YANK_UNFINISHED: &str = "t";
/// A yank whose text another client changed the buffer under cannot be
/// replaced: there is no yanked text left to find.
const NOT_A_YANK: &str = "Previous command was not a yank";

/// Emacs `kill-region` from Lisp: kills between `beg` and `end`, appending
/// to the last kill when the previous command was a kill (in front when
/// `end` < `beg`). `point` is PT during the deletion, which decides where
/// undo leaves point.
fn kill_text(ctx: &mut CommandContext, beg: u64, end: u64, point: u64) -> Result<(), CommandError> {
    let range = beg.min(end)..beg.max(end);
    let result = ctx.delete_with_point(range.clone(), point);
    let killed = match result {
        Ok(deleted) => deleted,
        // A read-only buffer still copies the text to the kill ring.
        Err(CommandError::BufferReadOnly(name)) => {
            let mut copied = Vec::new();
            ctx.buffer().text.read(range, &mut copied)?;
            save_kill(ctx, copied, end < beg);
            ctx.this_command = KILL_REGION;
            return Err(CommandError::BufferReadOnly(name));
        }
        Err(error) => return Err(error),
    };
    save_kill(ctx, killed, end < beg);
    ctx.this_command = KILL_REGION;
    ctx.request_deactivate_mark();
    Ok(())
}

/// The kill-ring half of `kill-region` and `copy-region-as-kill`.
fn save_kill(ctx: &mut CommandContext, text: Vec<u8>, before: bool) {
    let appending = ctx.last_command_is(KILL_REGION);
    let ring = ctx.kill_ring_mut();
    if appending {
        ring.kill_append(&text, before);
    } else {
        ring.kill_new(text);
    }
}

/// `C-w`: kills the region, which needs a mark but not an active one.
pub(super) fn kill_region_command(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (mark, point) = ctx.mark_and_point()?;
    // The region is extracted from its beginning.
    kill_text(ctx, mark, point, mark.min(point))
}

/// `M-w`: Emacs `kill-ring-save`, through `copy-region-as-kill`.
pub(super) fn kill_ring_save(ctx: &mut CommandContext) -> Result<(), CommandError> {
    copy_region_as_kill(ctx)?;
    ctx.request_deactivate_mark();
    Ok(())
}

/// Emacs `copy-region-as-kill`, leaving the mark as it is. The copy is
/// announced once it reaches the clipboard.
pub(crate) fn copy_region_as_kill(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (mark, point) = ctx.mark_and_point()?;
    let mut copied = Vec::new();
    ctx.buffer()
        .text
        .read(mark.min(point)..mark.max(point), &mut copied)?;
    save_kill(ctx, copied, point < mark);
    ctx.editor().request_clipboard_notice();
    Ok(())
}

/// Emacs `kill-line` with `kill-whole-line` nil: without an argument, kill
/// to the end of the line, or the newline too when only blanks remain; with
/// N, kill through N newlines (backward for N <= 0, to a line start).
pub(super) fn kill_line(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let text = ctx.buffer().text.clone();
    let point = ctx.point();
    let end = if ctx.prefix_arg == PrefixArg::None {
        if point == text.len() {
            return Err(CommandError::EndOfBuffer);
        }
        let line_end = line_end(&text, point)?;
        if only_blanks(&text, point, line_end)? {
            forward_visible_line(&text, point, 1)?
        } else {
            line_end
        }
    } else {
        forward_visible_line(&text, point, ctx.prefix_arg.numeric_value())?
    };
    kill_text(ctx, point, end, point)
}

/// `M-d`: Emacs `kill-word`.
pub(super) fn kill_word(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let count = ctx.prefix_arg.numeric_value();
    let point = ctx.point();
    let end = word_target(ctx, count)?;
    kill_text(ctx, point, end, point)
}

/// `M-DEL`: Emacs `backward-kill-word`, which kills back over ARG words.
pub(super) fn backward_kill_word(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let count = ctx.prefix_arg.numeric_value();
    let point = ctx.point();
    let target = word_target(ctx, count.saturating_neg())?;
    kill_text(ctx, point, target, point.min(target))
}

/// Emacs `kill-forward-chars`, the killing half of `delete-char`: kills
/// `count` characters after point (before it when negative). Running past
/// the buffer is Emacs's `args-out-of-range`, which names the buffer and
/// both 1-based character positions.
pub(super) fn kill_forward_chars(ctx: &mut CommandContext, count: i64) -> Result<(), CommandError> {
    let text = ctx.buffer().text.clone();
    let point = ctx.point();
    let Some(end) = step_chars(&text, point, count)? else {
        let start = i128::from(char_count(&text, point)?) + 1;
        let name = ctx.buffer().name.clone();
        return Err(ctx.error(format!(
            "Args out of range: #<buffer {name}>, {start}, {}",
            start + i128::from(count)
        )));
    };
    kill_text(ctx, point, end, point)
}

/// The position `count` characters (code points; an invalid byte is one)
/// away from `pos`, or `None` past a buffer edge.
pub(super) fn step_chars(text: &Text, pos: u64, count: i64) -> Result<Option<u64>, ReadError> {
    let mut position = pos;
    for _ in 0..count.unsigned_abs() {
        let next = if count > 0 {
            char_after(text, position)?.map(|(_, length)| position + length)
        } else {
            char_before(text, position)?.map(|(_, length)| position - length)
        };
        match next {
            Some(next) => position = next,
            None => return Ok(None),
        }
    }
    Ok(Some(position))
}

/// Only spaces and tabs between `from` and `to`.
fn only_blanks(text: &Text, from: u64, to: u64) -> Result<bool, ReadError> {
    for chunk in text.chunks(from..to) {
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

/// Emacs `forward-visible-line` without invisible text: forward over
/// `count` newlines, or to the start of the line `-count` lines up; it
/// stops quietly at the buffer edges.
fn forward_visible_line(text: &Text, pos: u64, count: i64) -> Result<u64, ReadError> {
    if count > 0 {
        let mut position = pos;
        for _ in 0..count {
            let (shortage, next) = forward_line(text, position, 1)?;
            position = next;
            if shortage != 0 {
                break;
            }
        }
        return Ok(position);
    }
    let mut position = line_start(text, pos)?;
    let mut remaining = count;
    if position != 0 {
        remaining += 1;
    }
    while remaining <= 0 {
        let (shortage, previous) = forward_line(text, position, -1)?;
        position = previous;
        if shortage != 0 {
            break;
        }
        if position != 0 {
            remaining += 1;
        }
    }
    Ok(position)
}

/// Emacs `yank`: insert the newest kill (or the Nth with N) with the mark
/// at its start; `C-u` leaves point at the start instead.
pub(super) fn yank(ctx: &mut CommandContext) -> Result<(), CommandError> {
    ctx.check_editable()?;
    let rotation = match ctx.prefix_arg {
        PrefixArg::None | PrefixArg::Raw(_) => 0,
        PrefixArg::Negative => -2,
        PrefixArg::Numeric(count) => count.saturating_sub(1),
    };
    ctx.this_command = YANK_UNFINISHED;
    let point = ctx.point();
    push_mark(ctx, point, false, false);
    let kill = current_kill(ctx, rotation)?;
    ctx.insert(&kill)?;
    ctx.record_yank(point, ctx.point());
    if matches!(ctx.prefix_arg, PrefixArg::Raw(_)) {
        swap_point_and_mark_quietly(ctx);
    }
    ctx.this_command = YANK;
    Ok(())
}

/// Emacs `yank-pop` right after a yank: replace the yanked text with the
/// kill N further along the ring. Emacs finds the yanked text between point
/// and the mark; the mark is shared by every client of a buffer, so the
/// range the client's own yank recorded is used instead.
pub(super) fn yank_pop(ctx: &mut CommandContext) -> Result<(), CommandError> {
    if !ctx.last_command_is(YANK) {
        // Emacs 28: after any other command, `M-y` is `yank-from-kill-ring`.
        let (client, prefix_arg, last_command) = (ctx.client, ctx.prefix_arg, ctx.last_command);
        return ctx
            .editor()
            .yank_from_kill_ring_command(client, prefix_arg, last_command);
    }
    // Another client may have killed the buffer or shown another one.
    let Some((start, end)) = ctx.yank_range() else {
        return Err(ctx.error(NOT_A_YANK));
    };
    ctx.this_command = YANK;
    let rotation = ctx.prefix_arg.numeric_value();
    let point = ctx.point();
    // Point before the end means a `C-u` yank left point at the start.
    let point_first = point < end;
    ctx.delete_with_point(start..end, point)?;
    ctx.set_point(start);
    ctx.set_mark_position(start);
    let kill = current_kill(ctx, rotation)?;
    ctx.insert(&kill)?;
    ctx.record_yank(start, ctx.point());
    if point_first {
        swap_point_and_mark_quietly(ctx);
    }
    Ok(())
}

/// Emacs `yank-from-kill-ring` with the kill read: set the mark, insert
/// the text, and point the yank pointer at that kill (a kill edited in the
/// minibuffer becomes a new kill). `C-u` leaves point before the text.
pub(crate) fn yank_from_kill_ring(
    ctx: &mut CommandContext,
    text: &[u8],
) -> Result<(), CommandError> {
    let point = ctx.point();
    push_mark(ctx, point, false, false);
    ctx.insert(text)?;
    let ring = ctx.kill_ring_mut();
    match ring.position(text) {
        Some(index) => ring.set_yank_pointer(index),
        None => ring.kill_new(text.to_vec()),
    }
    if matches!(ctx.prefix_arg, PrefixArg::Raw(_)) {
        swap_point_and_mark_quietly(ctx);
    }
    Ok(())
}

/// A bracketed paste, as Emacs's `xterm-paste` with
/// `xterm-store-paste-on-kill-ring`: the text becomes the newest kill and
/// is yanked, so `M-y` can replace it.
pub(crate) fn xterm_paste(ctx: &mut CommandContext, bytes: &[u8]) -> Result<(), CommandError> {
    ctx.this_command = YANK_UNFINISHED;
    let point = ctx.point();
    push_mark(ctx, point, false, false);
    ctx.kill_ring_mut().kill_new(bytes.to_vec());
    let kill = current_kill(ctx, 0)?;
    ctx.insert(&kill)?;
    ctx.record_yank(point, ctx.point());
    ctx.this_command = YANK;
    Ok(())
}

fn current_kill(ctx: &mut CommandContext, rotation: i64) -> Result<Vec<u8>, CommandError> {
    ctx.kill_ring_mut()
        .current_kill(rotation)
        .map(<[u8]>::to_vec)
        .map_err(|error: KillRingError| CommandError::User(error.to_string()))
}

/// Exchanges point and mark without activating the mark, as `yank` does.
fn swap_point_and_mark_quietly(ctx: &mut CommandContext) {
    let mark = ctx.mark().expect("yank set the mark");
    let point = ctx.point();
    ctx.set_mark_position(point);
    ctx.set_point(mark);
}
