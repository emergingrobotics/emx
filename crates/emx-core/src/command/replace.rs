//! `query-replace` (`M-%`) and `query-replace-regexp` (`C-M-%`). The
//! replacing loop is the editor's: it outlives the command, reading a key
//! for each match.

use super::{CommandContext, CommandError};

pub(super) fn query_replace(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, prefix_arg) = (ctx.client, ctx.prefix_arg);
    ctx.editor()
        .query_replace_command(client, false, prefix_arg)
}

pub(super) fn query_replace_regexp(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, prefix_arg) = (ctx.client, ctx.prefix_arg);
    ctx.editor().query_replace_command(client, true, prefix_arg)
}
