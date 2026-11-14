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
