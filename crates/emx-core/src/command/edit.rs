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
    let mut encoded = [0; 4];
    let encoded = character.encode_utf8(&mut encoded).as_bytes();
    // The count comes from typed digits, so it is bounded before anything is
    // allocated: one insertion may not exceed what a buffer holds in memory.
    let limit = ctx.editor().config().in_memory_max;
    let total = u64::try_from(count)
        .ok()
        .and_then(|count| count.checked_mul(encoded.len() as u64))
        .filter(|total| *total <= limit)
        .ok_or_else(|| {
            ctx.error(format!(
                "Repetition argument {count} is too large (limit {limit} bytes)"
            ))
        })?;
    let repeat = usize::try_from(total / encoded.len() as u64)
        .expect("a total within in_memory_max fits in usize");
    ctx.insert(&encoded.repeat(repeat))
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
