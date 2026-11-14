//! File commands. The work, and the questions they ask, is the editor's
//! file state machine; these start it.

use super::{CommandContext, CommandError};

pub(super) fn find_file(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().find_file_command(client);
    Ok(())
}

pub(super) fn save_buffer(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().save_buffer_command(client)
}

pub(super) fn write_file(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().write_file_command(client)
}

pub(super) fn save_some_buffers(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().save_some_buffers_command(client)
}

pub(super) fn kill_buffer(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().kill_buffer_command(client);
    Ok(())
}

/// Without a server this offers to save, then exits; a server client's
/// frame is let go instead.
pub(super) fn save_buffers_kill_terminal(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().save_buffers_kill_terminal_command(client)
}

/// Emacs `server-edit`: this buffer is done; release the clients waiting
/// for it.
pub(super) fn server_edit(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().server_edit_command(client);
    Ok(())
}

pub(super) fn recover_this_file(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().recover_this_file_command(client)
}

/// Emacs `read-only-mode` (`C-x C-q`) toggles `buffer-read-only`.
pub(super) fn read_only_mode(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let buffer = ctx.buffer_mut();
    buffer.read_only = !buffer.read_only;
    Ok(())
}

/// Emacs `revert-buffer`: the file replaces the buffer's text, undoably,
/// asking first when that discards edits.
pub(super) fn revert_buffer(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().revert_buffer_command(client)
}

/// Keeps the buffer over a newer file on disk: the next save overwrites it
/// without asking.
pub(super) fn keep_mine(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().keep_mine_command(client)
}

/// Shows what an external change changed, in `*diff-external*`.
pub(super) fn diff_external(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().diff_external_command(client)
}

/// Toggles moving point to the first changed line when a file reloads.
pub(super) fn follow_edit_mode(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().follow_edit_mode_command(client);
    Ok(())
}
