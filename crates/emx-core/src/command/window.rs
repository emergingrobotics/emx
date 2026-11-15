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
    ctx.editor().delete_other_windows_command(client)
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

/// `toggle-tree` (`F8`): opens or closes the file panel.
pub(super) fn toggle_tree(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().toggle_tree_command(client)
}

pub(super) fn tree_next_line(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, count) = (ctx.client, ctx.prefix_arg.numeric_value());
    ctx.editor().tree_move_command(client, count)
}

pub(super) fn tree_previous_line(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, count) = (ctx.client, ctx.prefix_arg.numeric_value());
    ctx.editor()
        .tree_move_command(client, count.saturating_neg())
}

pub(super) fn tree_open(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().tree_open_command(client)
}

pub(super) fn tree_toggle(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().tree_toggle_command(client)
}

pub(super) fn tree_up(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().tree_up_command(client)
}

pub(super) fn tree_refresh(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().tree_refresh_command(client)
}

pub(super) fn tree_toggle_ignored(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().tree_toggle_ignored_command(client)
}

pub(super) fn tree_create_directory(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().tree_create_directory_command(client)
}

pub(super) fn tree_rename(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().tree_rename_command(client)
}

pub(super) fn tree_delete(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().tree_delete_command(client)
}

/// `project-find-file` (`C-x p f`).
pub(super) fn project_find_file(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().project_find_file_command(client);
    Ok(())
}
