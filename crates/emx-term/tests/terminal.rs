use std::collections::VecDeque;
use std::io::{self, Read};
use std::time::Duration;

use emx_term::{
    InputEvent, InputSource, KittyDecoder, ProbeResult, TermError, UNSUPPORTED_TERMINAL_MESSAGE,
    probe_kitty, restore_sequence, setup_sequence,
};

const TIMEOUT: Duration = Duration::from_millis(500);

/// Each chunk is what one read returns; an exhausted queue behaves like a
/// terminal that stays silent until the timeout.
struct ScriptedInput {
    chunks: VecDeque<Vec<u8>>,
    end_of_file: bool,
    waits: Vec<Duration>,
}

impl ScriptedInput {
    fn new(chunks: &[&[u8]]) -> ScriptedInput {
        ScriptedInput {
            chunks: chunks.iter().map(|chunk| chunk.to_vec()).collect(),
            end_of_file: false,
            waits: Vec::new(),
        }
    }
}

impl Read for ScriptedInput {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let Some(mut chunk) = self.chunks.pop_front() else {
            assert!(self.end_of_file, "read without readable input");
            return Ok(0);
        };
        let count = chunk.len().min(buffer.len());
        buffer[..count].copy_from_slice(&chunk[..count]);
        if count < chunk.len() {
            self.chunks.push_front(chunk.split_off(count));
        }
        Ok(count)
    }
}

impl InputSource for ScriptedInput {
    fn wait_readable(&mut self, timeout: Duration) -> io::Result<bool> {
        self.waits.push(timeout);
        Ok(!self.chunks.is_empty() || self.end_of_file)
    }
}

fn probe(chunks: &[&[u8]]) -> (ProbeResult, Vec<u8>) {
    let mut input = ScriptedInput::new(chunks);
    let mut output = Vec::new();
    let result = probe_kitty(&mut input, &mut output, TIMEOUT).unwrap();
    assert!(input.waits.iter().all(|wait| *wait <= TIMEOUT));
    (result, output)
}

#[test]
fn probe_detects_support_and_absence() {
    let (result, output) = probe(&[b"\x1b[?0u\x1b[?62;22c"]);
    assert_eq!(output, b"\x1b[?u\x1b[c");
    assert_eq!(
        result,
        ProbeResult {
            supported: true,
            leftover: Vec::new()
        }
    );

    let (result, _) = probe(&[b"\x1b[?1;2c"]);
    assert_eq!(
        result,
        ProbeResult {
            supported: false,
            leftover: Vec::new()
        }
    );

    let (result, _) = probe(&[b"\x1b[?", b"1u\x1b", b"[?62", b";c"]);
    assert!(result.supported);
}

#[test]
fn probe_silent_terminal_is_unsupported() {
    let (result, _) = probe(&[]);
    assert_eq!(
        result,
        ProbeResult {
            supported: false,
            leftover: Vec::new()
        }
    );
    assert!(UNSUPPORTED_TERMINAL_MESSAGE.contains("kitty, WezTerm, Ghostty, foot"));
}

#[test]
fn probe_flags_reply_after_device_attributes_is_unsupported() {
    let (result, _) = probe(&[b"\x1b[?62c\x1b[?1u"]);
    assert!(!result.supported);
    assert_eq!(result.leftover, b"\x1b[?1u");
}

#[test]
fn probe_flags_without_device_attributes_before_timeout_is_supported() {
    let (result, _) = probe(&[b"\x1b[?1u"]);
    assert_eq!(
        result,
        ProbeResult {
            supported: true,
            leftover: Vec::new()
        }
    );
}

#[test]
fn probe_returns_typed_bytes_as_leftover() {
    let (result, _) = probe(&[
        b"ab\x1b[120;5u",
        b"\x1b[?1u",
        b"\xc3",
        b"\xa9\x1b[?62c\x1b[200~p\x1b",
        b"never read",
    ]);
    assert!(result.supported);
    assert_eq!(result.leftover, b"ab\x1b[120;5u\xc3\xa9\x1b[200~p\x1b");
    let mut decoder = KittyDecoder::new();
    let events = decoder.feed(&result.leftover);
    assert_eq!(events.len(), 4);
    assert!(matches!(events[3], InputEvent::Key(_)));
    assert_eq!(
        decoder.feed(b"[201~"),
        vec![InputEvent::Paste(b"p".to_vec())]
    );
}

#[test]
fn probe_keeps_partial_typed_sequences() {
    let (result, _) = probe(&[b"\x1b[?1u\x1b[?62c\x1b[12"]);
    assert!(result.supported);
    assert_eq!(result.leftover, b"\x1b[12");
}

#[test]
fn probe_end_of_input_is_an_error() {
    let mut input = ScriptedInput::new(&[b"\x1b[?1u"]);
    input.end_of_file = true;
    let error = probe_kitty(&mut input, &mut Vec::new(), TIMEOUT).unwrap_err();
    assert!(matches!(error, TermError::InputClosed), "{error:?}");
}

#[test]
fn setup_and_restore_sequences() {
    assert_eq!(setup_sequence(), b"\x1b[?1049h\x1b[?2004h\x1b[>1u");
    assert_eq!(
        restore_sequence(),
        b"\x1b[<u\x1b[?2004l\x1b[0m\x1b[?25h\x1b[?1049l"
    );
}
