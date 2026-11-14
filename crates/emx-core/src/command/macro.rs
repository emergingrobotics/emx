//! Keyboard macros: `kmacro-start-macro` (`C-x (`), `kmacro-end-macro`
//! (`C-x )`) and `kmacro-end-and-call-macro` (`C-x e`). Recording and replay
//! live in the editor's command loop (see `editor::kmacro`); these commands
//! drive it.

use super::{CommandContext, CommandError};

/// `C-x (`: Emacs `kmacro-start-macro`.
pub(super) fn kmacro_start_macro(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().start_kbd_macro(client)
}

/// `C-x )`: Emacs `kmacro-end-macro`.
pub(super) fn kmacro_end_macro(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().end_kbd_macro(client)
}

/// `C-x e`: Emacs `kmacro-end-and-call-macro`. Ends any definition in
/// progress, then asks the command loop to run the macro `count` times once
/// this command is finished.
pub(super) fn kmacro_end_and_call_macro(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let count = ctx.prefix_arg.numeric_value();
    let client = ctx.client;
    ctx.editor().end_kbd_macro_if_defining(client);
    ctx.request_macro_call(count);
    Ok(())
}
