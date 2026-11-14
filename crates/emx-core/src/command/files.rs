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

/// Emacs `server-edit` when no client is waiting for a buffer, which is
/// always the case until `--wait` clients exist.
pub(super) fn server_edit(ctx: &mut CommandContext) -> Result<(), CommandError> {
    ctx.message("No server editing buffers exist");
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
