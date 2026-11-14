use emx_types::{Key, KeyCode, Mods};

use super::{CommandContext, CommandError};

/// Emacs `self-insert-command`: inserts the typed character N times as one
/// change. Single insertions amalgamate into one undo group, as Emacs's
/// `undo-auto-amalgamate` does for N below 2, called before the edit.
pub(super) fn self_insert_command(ctx: &mut CommandContext) -> Result<(), CommandError> {
    const AMALGAMATE_BELOW: i64 = 2;
    let count = ctx.prefix_arg.numeric_value();
    if count < 0 {
        return Err(ctx.error(format!("Negative repetition argument {count}")));
    }
    if count < AMALGAMATE_BELOW {
        let repeated = ctx.last_command == Some(ctx.this_command);
        ctx.buffer_mut().amalgamate_last_change(repeated);
    }
    let Some(character) = ctx.key.and_then(inserted_character) else {
        ctx.ding();
        return Ok(());
    };
    let count = usize::try_from(count)
        .map_err(|_| ctx.error(format!("Repetition argument {count} is too large")))?;
    let mut encoded = [0; 4];
    let bytes = character.encode_utf8(&mut encoded).as_bytes().repeat(count);
    ctx.insert(&bytes)
}

/// Only an unmodified character key inserts; Emacs rings the bell when the
/// invoking event is not a character.
fn inserted_character(key: Key) -> Option<char> {
    match key {
        Key {
            code: KeyCode::Char(character),
            mods,
        } if mods == Mods::default() => Some(character),
        _ => None,
    }
}
