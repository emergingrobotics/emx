use emx_types::KeyCode;

use super::{CommandContext, CommandError, PrefixArg};

/// `C-u` multiplies the raw argument by this.
const UNIVERSAL_ARGUMENT_FACTOR: i64 = 4;
const DECIMAL: u32 = 10;

pub(super) fn universal_argument(ctx: &mut CommandContext) -> Result<(), CommandError> {
    ctx.set_prefix_arg(PrefixArg::Raw(UNIVERSAL_ARGUMENT_FACTOR));
    ctx.universal_argument_mode();
    Ok(())
}

/// Emacs `universal-argument-more`: after digits, `C-u` ends the argument
/// and the next key is read normally.
pub(super) fn universal_argument_more(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let next = match ctx.prefix_arg {
        PrefixArg::Raw(value) => PrefixArg::Raw(value.saturating_mul(UNIVERSAL_ARGUMENT_FACTOR)),
        PrefixArg::Negative => PrefixArg::Raw(-UNIVERSAL_ARGUMENT_FACTOR),
        other => other,
    };
    ctx.set_prefix_arg(next);
    if matches!(next, PrefixArg::Raw(_)) {
        ctx.universal_argument_mode();
    }
    Ok(())
}

pub(super) fn negative_argument(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let next = match ctx.prefix_arg {
        PrefixArg::Numeric(value) => PrefixArg::Numeric(value.saturating_neg()),
        PrefixArg::Negative => PrefixArg::None,
        PrefixArg::None | PrefixArg::Raw(_) => PrefixArg::Negative,
    };
    ctx.set_prefix_arg(next);
    ctx.universal_argument_mode();
    Ok(())
}

/// Emacs `digit-argument`: the digit comes from the key that invoked it
/// (`M-5`, `C-5`, or `5` after `C-u`).
pub(super) fn digit_argument(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let digit = ctx.key.and_then(|key| match key.code {
        KeyCode::Char(character) => character.to_digit(DECIMAL),
        _ => None,
    });
    let Some(digit) = digit.map(i64::from) else {
        ctx.ding();
        return Ok(());
    };
    let next = match ctx.prefix_arg {
        PrefixArg::Numeric(value) => {
            let signed_digit = if value < 0 { -digit } else { digit };
            PrefixArg::Numeric(
                value
                    .saturating_mul(i64::from(DECIMAL))
                    .saturating_add(signed_digit),
            )
        }
        // -0 is just -, so that -01 works.
        PrefixArg::Negative if digit == 0 => PrefixArg::Negative,
        PrefixArg::Negative => PrefixArg::Numeric(-digit),
        PrefixArg::None | PrefixArg::Raw(_) => PrefixArg::Numeric(digit),
    };
    ctx.set_prefix_arg(next);
    ctx.universal_argument_mode();
    Ok(())
}

/// Emacs `keyboard-quit`: deactivate the mark, then signal `quit`.
pub(super) fn keyboard_quit(ctx: &mut CommandContext) -> Result<(), CommandError> {
    ctx.buffer_mut().mark_active = false;
    Err(CommandError::Quit)
}

/// `scroll-up-command` (`C-v`) moves the text up, toward the buffer end.
const SCROLL_UP: i64 = 1;
const SCROLL_DOWN: i64 = -1;
const RECENTER_TOP_BOTTOM: &str = "recenter-top-bottom";
/// `end-of-buffer` puts the end this many rows above the window bottom.
const END_OF_BUFFER_RECENTER: i64 = -3;

/// An entry of Emacs `recenter-positions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecenterPosition {
    Middle,
    Top,
    Bottom,
}

/// Emacs's default `recenter-positions`.
const RECENTER_POSITIONS: [RecenterPosition; 3] = [
    RecenterPosition::Middle,
    RecenterPosition::Top,
    RecenterPosition::Bottom,
];

/// Where `recenter` puts point's row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Recenter {
    Middle,
    /// From the top when not negative, from the bottom (-1 is the last
    /// row) when negative.
    Row(i64),
}

/// Emacs `scroll-up-command` with `scroll-error-top-bottom` nil.
pub(super) fn scroll_up_command(ctx: &mut CommandContext) -> Result<(), CommandError> {
    scroll_command(ctx, SCROLL_UP)
}

/// Emacs `scroll-down-command` with `scroll-error-top-bottom` nil.
pub(super) fn scroll_down_command(ctx: &mut CommandContext) -> Result<(), CommandError> {
    scroll_command(ctx, SCROLL_DOWN)
}

/// window.c `scroll_command`: no argument scrolls a screenful, `-` a
/// screenful the other way, a number that many rows.
fn scroll_command(ctx: &mut CommandContext, direction: i64) -> Result<(), CommandError> {
    match ctx.prefix_arg {
        PrefixArg::None => window_scroll(ctx, direction, true),
        PrefixArg::Negative => window_scroll(ctx, -direction, true),
        argument => window_scroll(
            ctx,
            argument.numeric_value().saturating_mul(direction),
            false,
        ),
    }
}

/// window.c `window_scroll_line_based`, which Emacs uses on terminals,
/// with `scroll-margin` 0 and `scroll-preserve-screen-position` nil.
fn window_scroll(ctx: &mut CommandContext, count: i64, whole: bool) -> Result<(), CommandError> {
    let (_, height) = ctx.window_size();
    let count = if whole {
        let context = ctx.editor().config().next_screen_context_lines;
        let page = u64::from(height).saturating_sub(context).max(1);
        count.saturating_mul(i64::try_from(page).expect("at most the u16 window height"))
    } else {
        count
    };
    let text = ctx.buffer().text.clone();
    let layout = ctx.layout(&text);
    let point = ctx.point();
    let mut start = ctx.window_start();
    if layout.point_location(start, height, point)?.is_none() {
        start = layout.centered_start(point, height)?;
    }
    let lose = count < 0 && layout.row_start(start)? == 0;
    let (target, moved) = layout.move_lines(start, count)?;
    // vertical-motion that runs out of rows going down stops at the end.
    let position = if count > 0 && moved < count.unsigned_abs() {
        text.len()
    } else {
        layout.row_start(target)?
    };
    if lose {
        return Err(CommandError::BeginningOfBuffer);
    }
    if position >= text.len() {
        return Err(CommandError::EndOfBuffer);
    }
    ctx.set_window_start(target);
    if count > 0 && position > point {
        ctx.set_point(position);
    } else if count < 0 {
        let rows = i64::from(height);
        let (bottom, moved) = layout.move_lines(target, rows)?;
        let bottom_margin = if moved == u64::from(height) {
            layout.row_start(bottom)?
        } else {
            text.len() + 1
        };
        if bottom_margin <= point {
            let (last_row, _) = layout.move_lines(bottom, -1)?;
            ctx.set_point(layout.row_start(last_row)?);
        }
    }
    Ok(())
}

/// Emacs `recenter-top-bottom`: without an argument, successive calls
/// cycle through `recenter-positions`.
pub(super) fn recenter_top_bottom(ctx: &mut CommandContext) -> Result<(), CommandError> {
    match ctx.prefix_arg {
        PrefixArg::None => {
            let position = match ctx.recenter_last_op() {
                Some(last) if ctx.last_command_is(RECENTER_TOP_BOTTOM) => {
                    let index = RECENTER_POSITIONS
                        .iter()
                        .position(|position| *position == last)
                        .expect("recenter-last-op is one of recenter-positions");
                    RECENTER_POSITIONS[(index + 1) % RECENTER_POSITIONS.len()]
                }
                _ => RECENTER_POSITIONS[0],
            };
            ctx.set_recenter_last_op(position);
            match position {
                RecenterPosition::Middle => {
                    recenter(ctx, Recenter::Middle)?;
                    // `recenter-redisplay' is `tty': a bare recenter also
                    // redraws the terminal.
                    ctx.request_full_redraw();
                    Ok(())
                }
                RecenterPosition::Top => recenter(ctx, Recenter::Row(0)),
                RecenterPosition::Bottom => recenter(ctx, Recenter::Row(-1)),
            }
        }
        PrefixArg::Raw(_) => recenter(ctx, Recenter::Middle),
        argument => recenter(ctx, Recenter::Row(argument.numeric_value())),
    }
}

/// window.c `Frecenter` with `scroll-margin` 0: scrolls so point's row is
/// at `target`.
pub(super) fn recenter(ctx: &mut CommandContext, target: Recenter) -> Result<(), CommandError> {
    let (_, height) = ctx.window_size();
    let height = u64::from(height);
    let rows_above = match target {
        Recenter::Middle => height / 2,
        Recenter::Row(row) if row >= 0 => row.unsigned_abs().min(height.saturating_sub(1)),
        Recenter::Row(row) => height - row.unsigned_abs().max(1).min(height),
    };
    let text = ctx.buffer().text.clone();
    let layout = ctx.layout(&text);
    let start = layout.start_with_point_at(ctx.point(), rows_above)?;
    ctx.set_window_start(start);
    Ok(())
}

/// What `end-of-buffer` does after moving without a numeric argument: if
/// point is past Emacs `(window-end nil t)`, show it near the bottom.
pub(super) fn recenter_if_past_window_end(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (_, height) = ctx.window_size();
    let text = ctx.buffer().text.clone();
    let layout = ctx.layout(&text);
    let walk = layout.walk_window(ctx.window_start(), height, &mut |_, _| {
        std::ops::ControlFlow::Continue(())
    })?;
    if ctx.point() > walk.end {
        recenter(ctx, Recenter::Row(END_OF_BUFFER_RECENTER))?;
    }
    Ok(())
}

/// `M-x`: reads a command name and runs the command with this prefix
/// argument.
pub(super) fn execute_extended_command(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, prefix_arg, last_command) = (ctx.client, ctx.prefix_arg, ctx.last_command);
    ctx.editor()
        .execute_extended_command_command(client, prefix_arg, last_command);
    Ok(())
}
