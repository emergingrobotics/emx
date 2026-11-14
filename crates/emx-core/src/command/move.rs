use emx_text::{ReadError, Text, display_units};

use super::{CommandContext, CommandError};

/// Bytes read around point to find one display unit; doubled while a single
/// unit fills the whole window.
const UNIT_WINDOW: u64 = 64;

pub(super) fn forward_char(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let count = ctx.prefix_arg.numeric_value();
    move_units(ctx, count)
}

pub(super) fn backward_char(ctx: &mut CommandContext) -> Result<(), CommandError> {
    let count = ctx.prefix_arg.numeric_value();
    move_units(ctx, count.saturating_neg())
}

/// Emacs `forward-char`: at a buffer edge, point stays at the edge and the
/// command signals. A character is one display unit (a grapheme cluster, or
/// one invalid or control byte), standing in for Emacs moving over
/// compositions as a whole.
fn move_units(ctx: &mut CommandContext, count: i64) -> Result<(), CommandError> {
    let text = ctx.buffer().text.clone();
    let mut point = ctx.point();
    let mut outcome = Ok(());
    for _ in 0..count.unsigned_abs() {
        let next = if count > 0 {
            next_unit_end(&text, point)?
        } else {
            previous_unit_start(&text, point)?
        };
        match next {
            Some(position) => point = position,
            None => {
                outcome = Err(if count > 0 {
                    CommandError::EndOfBuffer
                } else {
                    CommandError::BeginningOfBuffer
                });
                break;
            }
        }
    }
    ctx.set_point(point);
    outcome
}

/// The end of the display unit that starts at `pos`, or `None` at the end.
pub(crate) fn next_unit_end(text: &Text, pos: u64) -> Result<Option<u64>, ReadError> {
    let length = text.len();
    if pos >= length {
        return Ok(None);
    }
    let mut window = UNIT_WINDOW;
    let mut bytes = Vec::new();
    loop {
        let end = length.min(pos.saturating_add(window));
        bytes.clear();
        text.read(pos..end, &mut bytes)?;
        // A character cut by the window end would read as invalid bytes and
        // fake a boundary, so it is left out. A grapheme boundary depends
        // only on what precedes it, so any other boundary is final.
        let usable = if end < length {
            without_partial_tail(&bytes)
        } else {
            &bytes
        };
        let mut units = display_units(usable);
        units
            .next()
            .expect("a window has at least one whole character");
        if let Some((second, _)) = units.next() {
            return Ok(Some(pos + second as u64));
        }
        if end == length {
            return Ok(Some(end));
        }
        window = window.saturating_mul(2);
    }
}

/// The start of the display unit that ends at `pos`, or `None` at the start.
pub(crate) fn previous_unit_start(text: &Text, pos: u64) -> Result<Option<u64>, ReadError> {
    if pos == 0 {
        return Ok(None);
    }
    let mut window = UNIT_WINDOW;
    let mut bytes = Vec::new();
    loop {
        let start = pos.saturating_sub(window);
        bytes.clear();
        text.read(start..pos, &mut bytes)?;
        // Segmentation is exact from a line start or the buffer start. From
        // anywhere else the first unit may be a fragment, so it only counts
        // once a later unit confirms the boundary.
        let (origin, exact) = match bytes.iter().rposition(|byte| *byte == b'\n') {
            Some(newline) if newline + 1 == bytes.len() => return Ok(Some(pos - 1)),
            Some(newline) => (newline + 1, true),
            None if start == 0 => (0, true),
            None => (leading_continuation_bytes(&bytes), false),
        };
        let mut unit_count = 0;
        let mut last_start = origin;
        for (unit_start, _) in display_units(&bytes[origin..]) {
            unit_count += 1;
            last_start = origin + unit_start;
        }
        if exact || unit_count >= 2 {
            return Ok(Some(start + last_start as u64));
        }
        window = window.saturating_mul(2);
    }
}

/// `bytes` without a trailing UTF-8 sequence that the window end cut short.
fn without_partial_tail(bytes: &[u8]) -> &[u8] {
    const MAX_CONTINUATION: usize = 3;
    for back in 1..=MAX_CONTINUATION.min(bytes.len()) {
        let byte = bytes[bytes.len() - back];
        if (0x80..0xc0).contains(&byte) {
            continue;
        }
        let needed = match byte {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => 1,
        };
        return if needed > back {
            &bytes[..bytes.len() - back]
        } else {
            bytes
        };
    }
    bytes
}

/// UTF-8 continuation bytes at the start of a window that began inside a
/// character; at most three.
fn leading_continuation_bytes(bytes: &[u8]) -> usize {
    const MAX_CONTINUATION: usize = 3;
    bytes
        .iter()
        .take(MAX_CONTINUATION)
        .take_while(|byte| (0x80..0xc0).contains(*byte))
        .count()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use emx_text::Sources;

    use super::*;

    fn text_of(content: &[u8]) -> Text {
        Text::from_original(Sources::memory(Arc::from(content)))
    }

    fn boundaries_forward(text: &Text) -> Vec<u64> {
        let mut positions = vec![0];
        let mut pos = 0;
        while let Some(next) = next_unit_end(text, pos).expect("memory read") {
            positions.push(next);
            pos = next;
        }
        positions
    }

    fn boundaries_backward(text: &Text) -> Vec<u64> {
        let mut positions = vec![text.len()];
        let mut pos = text.len();
        while let Some(previous) = previous_unit_start(text, pos).expect("memory read") {
            positions.push(previous);
            pos = previous;
        }
        positions.reverse();
        positions
    }

    #[test]
    fn forward_and_backward_agree_on_mixed_text() {
        let mut content = "a\u{e9}e\u{301}\n\u{1f600}\tx\r\n".as_bytes().to_vec();
        content.push(0xff);
        content.extend_from_slice("\u{4e2d}z".as_bytes());
        let text = text_of(&content);
        let forward = boundaries_forward(&text);
        assert_eq!(forward, boundaries_backward(&text));
        assert_eq!(forward, vec![0, 1, 3, 6, 7, 11, 12, 13, 14, 15, 16, 19, 20]);
    }

    #[test]
    fn long_clusters_and_long_lines_are_handled() {
        let mut content = "e".to_string();
        content.push_str(&"\u{301}".repeat(100));
        content.push_str(&"y".repeat(500));
        let text = text_of(content.as_bytes());
        assert_eq!(next_unit_end(&text, 0).expect("read"), Some(201));
        assert_eq!(previous_unit_start(&text, 201).expect("read"), Some(0));
        assert_eq!(previous_unit_start(&text, 701).expect("read"), Some(700));
    }
}
