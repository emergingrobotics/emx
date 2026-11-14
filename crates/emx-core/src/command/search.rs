//! Incremental search commands. `C-s` and friends start an isearch; the
//! commands its keymap runs while it is active (`isearch-mode-map`) are in
//! `ISEARCH_COMMANDS`, apart from the global commands, as Emacs keeps them
//! in a keymap of their own.

use super::{CommandContext, CommandError, CommandSpec, PrefixArg};

/// `isearch-forward` (`C-s`); with a prefix argument, a regexp search.
pub(super) fn isearch_forward(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, regexp) = (ctx.client, ctx.prefix_arg != PrefixArg::None);
    ctx.editor().isearch_start(client, true, regexp);
    Ok(())
}

/// `isearch-backward` (`C-r`).
pub(super) fn isearch_backward(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, regexp) = (ctx.client, ctx.prefix_arg != PrefixArg::None);
    ctx.editor().isearch_start(client, false, regexp);
    Ok(())
}

/// `isearch-forward-regexp` (`C-M-s`); with a prefix argument, a plain
/// search.
pub(super) fn isearch_forward_regexp(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, regexp) = (ctx.client, ctx.prefix_arg == PrefixArg::None);
    ctx.editor().isearch_start(client, true, regexp);
    Ok(())
}

/// `isearch-backward-regexp` (`C-M-r`).
pub(super) fn isearch_backward_regexp(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, regexp) = (ctx.client, ctx.prefix_arg == PrefixArg::None);
    ctx.editor().isearch_start(client, false, regexp);
    Ok(())
}

fn printing_char(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, key, count) = (ctx.client, ctx.key, ctx.prefix_arg.numeric_value());
    ctx.editor().isearch_printing_char(client, key, count);
    Ok(())
}

fn repeat_forward(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, prefix_arg) = (ctx.client, ctx.prefix_arg);
    ctx.editor()
        .isearch_repeat_command(client, true, prefix_arg);
    Ok(())
}

fn repeat_backward(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, prefix_arg) = (ctx.client, ctx.prefix_arg);
    ctx.editor()
        .isearch_repeat_command(client, false, prefix_arg);
    Ok(())
}

fn delete_char(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().isearch_delete_char(client);
    Ok(())
}

fn del_char(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, count) = (ctx.client, ctx.prefix_arg.numeric_value());
    ctx.editor().isearch_del_char(client, count);
    Ok(())
}

fn abort(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().isearch_abort(client)
}

fn cancel(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().isearch_cancel(client)
}

fn exit(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().isearch_exit(client);
    Ok(())
}

fn quote_char(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().isearch_quote_char(client);
    Ok(())
}

fn yank_word_or_char(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, count) = (ctx.client, ctx.prefix_arg.numeric_value());
    ctx.editor().isearch_yank_word_or_char(client, count)
}

fn yank_char(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, count) = (ctx.client, ctx.prefix_arg.numeric_value());
    ctx.editor().isearch_yank_char(client, count)
}

fn yank_line(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, count) = (ctx.client, ctx.prefix_arg.numeric_value());
    ctx.editor().isearch_yank_line(client, count)
}

fn yank_kill(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().isearch_yank_kill(client)
}

fn yank_pop_only(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, last_command) = (ctx.client, ctx.last_command);
    ctx.editor().isearch_yank_pop_only(client, last_command)
}

fn toggle_case_fold(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().isearch_toggle_case_fold(client);
    Ok(())
}

fn toggle_regexp(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().isearch_toggle_regexp(client);
    Ok(())
}

fn toggle_lax_whitespace(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().isearch_toggle_lax_whitespace(client);
    Ok(())
}

fn edit_string(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().isearch_edit_string(client, false);
    Ok(())
}

fn ring_advance(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().isearch_ring_adjust(client, true);
    Ok(())
}

fn ring_retreat(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let client = ctx.client;
    ctx.editor().isearch_ring_adjust(client, false);
    Ok(())
}

fn beginning_of_buffer(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, count) = (ctx.client, ctx.prefix_arg.numeric_value());
    ctx.editor().isearch_edge_of_buffer(client, true, count);
    Ok(())
}

fn end_of_buffer(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, count) = (ctx.client, ctx.prefix_arg.numeric_value());
    ctx.editor().isearch_edge_of_buffer(client, false, count);
    Ok(())
}

fn occur(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, prefix_arg) = (ctx.client, ctx.prefix_arg);
    ctx.editor().isearch_occur(client, prefix_arg)
}

fn query_replace(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, prefix_arg) = (ctx.client, ctx.prefix_arg);
    ctx.editor()
        .isearch_query_replace(client, false, prefix_arg)
}

fn query_replace_regexp(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let (client, prefix_arg) = (ctx.client, ctx.prefix_arg);
    ctx.editor().isearch_query_replace(client, true, prefix_arg)
}

/// The commands of `isearch-mode-map`.
static ISEARCH_COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "isearch-printing-char",
        run: printing_char,
    },
    CommandSpec {
        name: "isearch-repeat-forward",
        run: repeat_forward,
    },
    CommandSpec {
        name: "isearch-repeat-backward",
        run: repeat_backward,
    },
    CommandSpec {
        name: "isearch-delete-char",
        run: delete_char,
    },
    CommandSpec {
        name: "isearch-del-char",
        run: del_char,
    },
    CommandSpec {
        name: "isearch-abort",
        run: abort,
    },
    CommandSpec {
        name: "isearch-cancel",
        run: cancel,
    },
    CommandSpec {
        name: "isearch-exit",
        run: exit,
    },
    CommandSpec {
        name: "isearch-quote-char",
        run: quote_char,
    },
    CommandSpec {
        name: "isearch-yank-word-or-char",
        run: yank_word_or_char,
    },
    CommandSpec {
        name: "isearch-yank-char",
        run: yank_char,
    },
    CommandSpec {
        name: "isearch-yank-line",
        run: yank_line,
    },
    CommandSpec {
        name: "isearch-yank-kill",
        run: yank_kill,
    },
    CommandSpec {
        name: "isearch-yank-pop-only",
        run: yank_pop_only,
    },
    CommandSpec {
        name: "isearch-toggle-case-fold",
        run: toggle_case_fold,
    },
    CommandSpec {
        name: "isearch-toggle-regexp",
        run: toggle_regexp,
    },
    CommandSpec {
        name: "isearch-toggle-lax-whitespace",
        run: toggle_lax_whitespace,
    },
    CommandSpec {
        name: "isearch-edit-string",
        run: edit_string,
    },
    CommandSpec {
        name: "isearch-ring-advance",
        run: ring_advance,
    },
    CommandSpec {
        name: "isearch-ring-retreat",
        run: ring_retreat,
    },
    CommandSpec {
        name: "isearch-beginning-of-buffer",
        run: beginning_of_buffer,
    },
    CommandSpec {
        name: "isearch-end-of-buffer",
        run: end_of_buffer,
    },
    CommandSpec {
        name: "isearch-occur",
        run: occur,
    },
    CommandSpec {
        name: "isearch-query-replace",
        run: query_replace,
    },
    CommandSpec {
        name: "isearch-query-replace-regexp",
        run: query_replace_regexp,
    },
];

/// The isearch command named `name`.
pub(crate) fn isearch_command(name: &str) -> Option<&'static CommandSpec> {
    ISEARCH_COMMANDS.iter().find(|spec| spec.name == name)
}
