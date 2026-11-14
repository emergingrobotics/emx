//! `M-/`: Emacs `dabbrev-expand`. Expands the word before point to a word
//! found elsewhere in the buffer that starts with it; repeated calls cycle
//! through the candidates. The search is bounded to `IN_MEMORY_MAX` bytes on
//! each side of point.

use emx_text::{ReadError, Text};

use super::r#move::is_word;
use super::{CommandContext, CommandError};

const DABBREV_EXPAND: &str = "dabbrev-expand";

/// Per-client cycling state between successive `M-/` presses.
#[derive(Debug, Clone)]
pub(crate) struct DabbrevState {
    abbrev: Vec<u8>,
    /// Position right after the abbreviation, where the expansion is inserted.
    abbrev_end: u64,
    candidates: Vec<Vec<u8>>,
    index: usize,
    insert_len: u64,
}

pub(super) fn dabbrev_expand(ctx: &mut CommandContext) -> Result<(), CommandError> {
    ctx.check_editable()?;
    let client = ctx.client;
    let continuing = ctx.last_command_is(DABBREV_EXPAND);
    let state = ctx.editor().take_dabbrev(client);
    if continuing && let Some(mut state) = state {
        let remove = state.abbrev_end..state.abbrev_end + state.insert_len;
        ctx.delete_with_point(remove, state.abbrev_end)?;
        ctx.set_point(state.abbrev_end);
        state.index += 1;
        if state.index >= state.candidates.len() {
            return Err(ctx.error(format!(
                "No further dynamic expansion for \u{2018}{}\u{2019} found",
                String::from_utf8_lossy(&state.abbrev)
            )));
        }
        let expansion = state.candidates[state.index].clone();
        ctx.insert(&expansion)?;
        state.insert_len = expansion.len() as u64;
        ctx.editor().set_dabbrev(client, state);
        return Ok(());
    }
    let point = ctx.point();
    let text = ctx.buffer().text.clone();
    let abbrev_start = abbreviation_start(&text, point)?;
    if abbrev_start == point {
        return Err(ctx.error("No possible abbreviation"));
    }
    let mut abbrev = Vec::new();
    text.read(abbrev_start..point, &mut abbrev)?;
    let limit = ctx.editor().config().in_memory_max;
    let candidates = find_candidates(&text, &abbrev, abbrev_start, point, limit)?;
    let Some(expansion) = candidates.first().cloned() else {
        return Err(ctx.error(format!(
            "No dynamic expansion for \u{2018}{}\u{2019} found",
            String::from_utf8_lossy(&abbrev)
        )));
    };
    ctx.insert(&expansion)?;
    let state = DabbrevState {
        abbrev,
        abbrev_end: point,
        candidates,
        index: 0,
        insert_len: expansion.len() as u64,
    };
    ctx.editor().set_dabbrev(client, state);
    Ok(())
}

/// Emacs's `dabbrev-abbrev-char-regexp` default treats word characters as the
/// abbreviation; a byte is tested as its own code point (ASCII suffices).
fn byte_is_word(byte: u8) -> bool {
    is_word(u32::from(byte))
}

/// The start of the word immediately before `point`.
fn abbreviation_start(text: &Text, point: u64) -> Result<u64, ReadError> {
    let mut start = point;
    while start > 0 && byte_is_word(text.byte_at(start - 1)?) {
        start -= 1;
    }
    Ok(start)
}

/// Candidate expansions, nearest backward first then forward, deduplicated.
fn find_candidates(
    text: &Text,
    abbrev: &[u8],
    abbrev_start: u64,
    point: u64,
    limit: u64,
) -> Result<Vec<Vec<u8>>, ReadError> {
    let mut seen: Vec<Vec<u8>> = Vec::new();
    let mut out: Vec<Vec<u8>> = Vec::new();
    let consider = |expansion: Vec<u8>, out: &mut Vec<Vec<u8>>, seen: &mut Vec<Vec<u8>>| {
        if !seen.contains(&expansion) {
            seen.push(expansion.clone());
            out.push(expansion);
        }
    };
    let floor = abbrev_start.saturating_sub(limit);
    let mut pos = abbrev_start;
    while pos > floor {
        pos -= 1;
        if let Some(expansion) = expansion_at(text, pos, abbrev)? {
            consider(expansion, &mut out, &mut seen);
        }
    }
    let ceil = (point + limit).min(text.len());
    let mut pos = point;
    while pos < ceil {
        if let Some(expansion) = expansion_at(text, pos, abbrev)? {
            consider(expansion, &mut out, &mut seen);
        }
        pos += 1;
    }
    Ok(out)
}

/// If a word begins at `pos` with `abbrev` as a strict prefix, the rest of
/// that word (the expansion to insert).
fn expansion_at(text: &Text, pos: u64, abbrev: &[u8]) -> Result<Option<Vec<u8>>, ReadError> {
    if pos > 0 && byte_is_word(text.byte_at(pos - 1)?) {
        return Ok(None);
    }
    let length = abbrev.len() as u64;
    if pos + length > text.len() {
        return Ok(None);
    }
    let mut head = Vec::new();
    text.read(pos..pos + length, &mut head)?;
    if head != abbrev {
        return Ok(None);
    }
    if pos + length >= text.len() || !byte_is_word(text.byte_at(pos + length)?) {
        return Ok(None);
    }
    let mut end = pos + length;
    while end < text.len() && byte_is_word(text.byte_at(end)?) {
        end += 1;
    }
    let mut expansion = Vec::new();
    text.read(pos + length..end, &mut expansion)?;
    Ok(Some(expansion))
}
