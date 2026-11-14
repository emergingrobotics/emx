use crate::marker::InsertionType;

use super::{CommandContext, CommandError, PrefixArg};

const SET_MARK_COMMAND: &str = "set-mark-command";
const POP_TO_MARK_COMMAND: &str = "pop-to-mark-command";
const NO_MARK: &str = "No mark set in this buffer";
/// `C-u C-u C-SPC` and beyond set the mark instead of popping it.
const UNIVERSAL_ARGUMENT_ONCE: i64 = 4;

/// Emacs `set-mark-command` in Transient Mark mode.
pub(super) fn set_mark_command(ctx: &mut CommandContext) -> Result<(), CommandError> {
    match ctx.prefix_arg {
        PrefixArg::Raw(value) if value > UNIVERSAL_ARGUMENT_ONCE => {
            push_mark_command(ctx);
            Ok(())
        }
        PrefixArg::None if ctx.last_command_is(SET_MARK_COMMAND) => {
            if ctx.region_active() {
                ctx.deactivate_mark_now();
                ctx.message("Mark deactivated");
            } else {
                ctx.activate_mark();
                ctx.message("Mark activated");
            }
            Ok(())
        }
        PrefixArg::None => {
            push_mark_command(ctx);
            Ok(())
        }
        _ => {
            ctx.this_command = POP_TO_MARK_COMMAND;
            pop_to_mark_command(ctx)
        }
    }
}

/// `C-x h`: Emacs `mark-whole-buffer`. Point to the start, an active mark at
/// the end.
pub(super) fn mark_whole_buffer(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let point = ctx.point();
    push_mark(ctx, point, true, false);
    let end = ctx.buffer().text.len();
    push_mark(ctx, end, true, true);
    ctx.set_point(0);
    Ok(())
}

/// Emacs `exchange-point-and-mark`: the mark ends up active, unless a
/// prefix argument was given.
pub(super) fn exchange_point_and_mark(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let old_mark = ctx.mark().ok_or_else(|| ctx.error(NO_MARK))?;
    let point = ctx.point();
    ctx.set_mark_position(point);
    ctx.set_point(old_mark);
    if ctx.prefix_arg == PrefixArg::None {
        ctx.activate_mark();
    } else {
        ctx.deactivate_mark_now();
    }
    Ok(())
}

/// Emacs `push-mark-command` (interactive, so it reports): set and activate
/// the mark at point, or just activate it when it is already there.
fn push_mark_command(ctx: &mut CommandContext) {
    let point = ctx.point();
    if ctx.mark() == Some(point) {
        ctx.activate_mark();
        ctx.message("Mark activated");
    } else {
        push_mark(ctx, point, false, true);
    }
}

/// Emacs `pop-to-mark-command`.
fn pop_to_mark_command(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let mark = ctx.mark().ok_or_else(|| ctx.error(NO_MARK))?;
    if ctx.point() == mark {
        ctx.message("Mark popped");
    }
    ctx.set_point(mark);
    pop_mark(ctx);
    Ok(())
}

/// Emacs `push-mark`: the old mark goes onto the mark ring (dropping the
/// oldest past `mark_ring_max`), and the mark moves to `location`.
pub(crate) fn push_mark(ctx: &mut CommandContext, location: u64, quiet: bool, activate: bool) {
    let limit =
        usize::try_from(ctx.editor().config().mark_ring_max).expect("mark_ring_max fits in usize");
    if let Some(old) = ctx.mark() {
        let buffer = ctx.buffer_mut();
        let saved = buffer.create_marker(old, InsertionType::StayBefore);
        buffer.mark_ring.push_front(saved);
        while buffer.mark_ring.len() > limit {
            let dropped = buffer
                .mark_ring
                .pop_back()
                .expect("a ring longer than its limit is not empty");
            buffer.delete_marker(dropped);
        }
    }
    ctx.set_mark_position(location);
    if !quiet {
        ctx.message("Mark set");
    }
    if activate {
        ctx.activate_mark();
    }
}

/// Emacs `pop-mark`: the mark takes the oldest ring entry and the current
/// mark goes to the back of the ring, so popping cycles. Deactivates.
fn pop_mark(ctx: &mut CommandContext) {
    if let Some(current) = ctx.mark() {
        let buffer = ctx.buffer_mut();
        if let Some(next) = buffer.mark_ring.pop_front() {
            let saved = buffer.create_marker(current, InsertionType::StayBefore);
            buffer.mark_ring.push_back(saved);
            let position = buffer.marker_pos(next);
            buffer.delete_marker(next);
            ctx.set_mark_position(position);
        }
    }
    ctx.deactivate_mark_now();
}
