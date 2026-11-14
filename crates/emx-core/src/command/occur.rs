//! `occur` (`M-s o`) and the commands of `*Occur*` (`occur-mode-map`).

use super::{CommandContext, CommandError};

pub(super) fn occur(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, prefix_arg) = (ctx.client, ctx.prefix_arg);
    ctx.editor().occur_command(client, prefix_arg);
    Ok(())
}

/// `occur-mode-goto-occurrence` (`RET`).
pub(super) fn goto_occurrence(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().occur_goto_occurrence(client)
}

/// `occur-mode-goto-occurrence-other-window` (`o`).
pub(super) fn goto_occurrence_other_window(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().occur_goto_occurrence(client)
}

/// `occur-mode-display-occurrence` (`C-o`).
pub(super) fn display_occurrence(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().occur_display_occurrence(client)
}

/// `occur-next` (`M-n`).
pub(super) fn next(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, count) = (ctx.client, ctx.prefix_arg.numeric_value());
    ctx.editor().occur_move(client, true, count)
}

/// `occur-prev` (`M-p`).
pub(super) fn previous(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, count) = (ctx.client, ctx.prefix_arg.numeric_value());
    ctx.editor().occur_move(client, false, count)
}

/// `next-error-no-select` (`n`) in `*Occur*`.
pub(super) fn next_error_no_select(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().occur_next_error_no_select(client, true)
}

/// `previous-error-no-select` (`p`) in `*Occur*`.
pub(super) fn previous_error_no_select(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().occur_next_error_no_select(client, false)
}
