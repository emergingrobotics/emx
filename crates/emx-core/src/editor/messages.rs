//! The `*Messages*` buffer: every message shown in an echo area is also
//! logged there, as Emacs's `message_dolog` does. The log lives in memory
//! and is never journaled.

use super::Editor;
use crate::buffer::{Buffer, BufferId, EditError};
use crate::marker::InsertionType;
use emx_text::LineLookup;

pub(super) const MESSAGES_BUFFER: &str = "*Messages*";
/// Emacs `message-log-max`: the log keeps this many lines.
const MESSAGE_LOG_MAX: u64 = 1000;
const NEWLINE: u8 = b'\n';

impl Editor {
    /// Makes the log buffer, read-only as `messages-buffer-mode` is.
    pub(super) fn create_messages_buffer(&mut self) -> BufferId {
        let buffer = self.create_buffer(MESSAGES_BUFFER);
        let target = &mut self.buffers[buffer];
        target.read_only = true;
        target.messages = true;
        // The log's own point starts at its end and, staying there,
        // follows each message, so a window that shows it shows the end.
        target.last_point = Some(target.create_marker_without_undo(0, InsertionType::StayBefore));
        buffer
    }

    /// Appends `message` to `*Messages*`, made again if it was killed. A
    /// repeat of the last line is counted on it instead (` [2 times]`).
    pub(crate) fn log_message(&mut self, message: &str) {
        if message.is_empty() {
            return;
        }
        let existing = self
            .buffers
            .iter()
            .find(|(_, buffer)| buffer.messages)
            .map(|(id, _)| id);
        let buffer = match existing {
            Some(buffer) => buffer,
            None => self.create_messages_buffer(),
        };
        log_into(&mut self.buffers[buffer], message)
            .expect("the message log is in memory, so editing it cannot fail");
    }
}

fn log_into(buffer: &mut Buffer, message: &str) -> Result<(), EditError> {
    let old_end = buffer.text.len();
    // Windows whose point is at the end keep following the log.
    let followers = buffer.markers_at(old_end);
    let mut line = message.to_string();
    if old_end > 0 {
        let mut last = Vec::new();
        buffer.text.read(old_end - 1..old_end, &mut last)?;
        if last != [NEWLINE] {
            buffer.insert_unrecorded(old_end, &[NEWLINE])?;
        } else {
            let start = buffer
                .text
                .find_newline_backward(old_end - 1)?
                .map_or(0, |newline| newline + 1);
            let mut previous = Vec::new();
            buffer.text.read(start..old_end - 1, &mut previous)?;
            if let Some(count) = duplicate_count(&String::from_utf8_lossy(&previous), message) {
                line = format!("{message} [{} times]", count + 1);
                buffer.delete_unrecorded(start..old_end)?;
            }
        }
    }
    line.push(char::from(NEWLINE));
    let at = buffer.text.len();
    buffer.insert_unrecorded(at, line.as_bytes())?;
    trim(buffer)?;
    let end = buffer.text.len();
    for marker in followers {
        if buffer.marker_position(marker).is_some() {
            buffer.set_marker(marker, end);
        }
    }
    Ok(())
}

/// Emacs `message_log_check_duplicate`: 1 when `previous` is `message`,
/// N when it is `message [N times]`.
fn duplicate_count(previous: &str, message: &str) -> Option<u64> {
    if previous == message {
        return Some(1);
    }
    previous
        .strip_prefix(message)?
        .strip_prefix(" [")?
        .strip_suffix(" times]")?
        .parse()
        .ok()
}

/// Drops the oldest lines past `message-log-max`.
fn trim(buffer: &mut Buffer) -> Result<(), EditError> {
    let Some(lines) = buffer.text.line_count() else {
        return Ok(());
    };
    // The text ends with a newline, so the last "line" is empty.
    let logged = lines - 1;
    if logged <= MESSAGE_LOG_MAX {
        return Ok(());
    }
    if let LineLookup::Found(cut) = buffer.text.line_start(logged - MESSAGE_LOG_MAX)? {
        buffer.delete_unrecorded(0..cut)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicates_are_counted_as_message_log_check_duplicate_does() {
        assert_eq!(duplicate_count("Mark set", "Mark set"), Some(1));
        assert_eq!(duplicate_count("Mark set [4 times]", "Mark set"), Some(4));
        assert_eq!(duplicate_count("Mark set [x times]", "Mark set"), None);
        assert_eq!(duplicate_count("Mark", "Mark set"), None);
        assert_eq!(duplicate_count("Mark set!", "Mark set"), None);
    }
}
