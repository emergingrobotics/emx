//! Emacs registers: `copy-to-register`, `insert-register`,
//! `point-to-register` and `jump-to-register`. Each reads a one-character
//! register name after its key sequence; the read is driven by the editor's
//! command loop (see `editor::registers`), so these commands only set it up.

use super::{CommandContext, CommandError, PrefixArg};

/// `C-x r s`: Emacs `copy-to-register`. A prefix argument deletes the region
/// after copying it.
pub(super) fn copy_to_register(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (mark, point) = ctx.mark_and_point()?;
    let delete = ctx.prefix_arg != PrefixArg::None;
    if delete {
        ctx.check_editable()?;
    }
    let range = mark.min(point)..mark.max(point);
    let client = ctx.client;
    ctx.editor().begin_copy_to_register(client, range, delete);
    Ok(())
}

/// `C-x r i`: Emacs `insert-register`. Without a prefix argument point ends
/// after the inserted text; with one, before it.
pub(super) fn insert_register(ctx: &mut CommandContext) -> Result<(), CommandError> {
    ctx.check_editable()?;
    let exchange = ctx.prefix_arg != PrefixArg::None;
    let client = ctx.client;
    ctx.editor().begin_insert_register(client, exchange);
    Ok(())
}

/// `C-x r SPC`: Emacs `point-to-register`.
pub(super) fn point_to_register(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let point = ctx.point();
    let client = ctx.client;
    ctx.editor().begin_point_to_register(client, point);
    Ok(())
}

/// `C-x r j`: Emacs `jump-to-register`.
pub(super) fn jump_to_register(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().begin_jump_to_register(client);
    Ok(())
}
