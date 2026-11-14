//! Window and buffer-switching commands. The window tree and which buffer
//! each window shows are the editor's; these start the work.

use super::{CommandContext, CommandError, PrefixArg};

/// The size argument of the split commands: none without a prefix
/// argument, else its numeric value (`C-u` alone is 4).
fn split_size(prefix_arg: PrefixArg) -> Option<i64> {
    match prefix_arg {
        PrefixArg::None => None,
        argument => Some(argument.numeric_value()),
    }
}

pub(super) fn split_window_below(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, size) = (ctx.client, split_size(ctx.prefix_arg));
    ctx.editor().split_window_command(client, false, size)
}

pub(super) fn split_window_right(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, size) = (ctx.client, split_size(ctx.prefix_arg));
    ctx.editor().split_window_command(client, true, size)
}

pub(super) fn delete_window(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().delete_window_command(client)
}

pub(super) fn delete_other_windows(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().delete_other_windows_command(client);
    Ok(())
}

pub(super) fn other_window(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, count) = (ctx.client, ctx.prefix_arg.numeric_value());
    ctx.editor().other_window_command(client, count);
    Ok(())
}

pub(super) fn switch_to_buffer(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().switch_to_buffer_command(client);
    Ok(())
}

/// With a prefix argument whose value is not negative, only buffers
/// visiting files are listed (`Buffer-menu-files-only`).
pub(super) fn list_buffers(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    let files_only = ctx.prefix_arg != PrefixArg::None && ctx.prefix_arg.numeric_value() >= 0;
    ctx.editor().list_buffers_command(client, files_only);
    Ok(())
}

pub(super) fn buffer_menu_this_window(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().buffer_menu_this_window_command(client)
}
