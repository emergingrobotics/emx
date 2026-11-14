//! Decoder for terminal input with Kitty keyboard protocol flag 1
//! ("disambiguate escape codes") pushed.
//!
//! Under that flag Escape never arrives as a bare 0x1b (it is `CSI 27 u`), so
//! a trailing 0x1b is always the start of a sequence and the decoder can wait
//! for more bytes without a timeout. Decoding never depends on where `feed`
//! boundaries fall.

use emx_types::{Key, KeyCode, Mods};

/// Longest escape sequence the decoder buffers; a sequence with no final
/// byte within this many bytes is reported as `Unknown` and decoding resyncs.
pub const MAX_PENDING_SEQUENCE: usize = 4096;

const ESCAPE: u8 = 0x1b;
const CSI_PREFIX: &[u8] = b"\x1b[";
const PASTE_END: &[u8] = b"\x1b[201~";
const PASTE_START_NUMBER: u32 = 200;

const MODIFIER_SHIFT: u32 = 1;
const MODIFIER_ALT: u32 = 2;
const MODIFIER_CTRL: u32 = 4;
const MODIFIER_SUPER: u32 = 8;
const MODIFIER_HYPER: u32 = 16;
const MODIFIER_META: u32 = 32;
const MODIFIER_CAPS_LOCK: u32 = 64;
const MODIFIER_NUM_LOCK: u32 = 128;
const KNOWN_MODIFIERS: u32 = MODIFIER_SHIFT
    | MODIFIER_ALT
    | MODIFIER_CTRL
    | MODIFIER_SUPER
    | MODIFIER_HYPER
    | MODIFIER_META
    | MODIFIER_CAPS_LOCK
    | MODIFIER_NUM_LOCK;

const EVENT_PRESS: u32 = 1;
const EVENT_REPEAT: u32 = 2;

const CODE_TAB: u32 = 9;
const CODE_ENTER: u32 = 13;
const CODE_ESCAPE: u32 = 27;
const CODE_BACKSPACE: u32 = 127;
const CODE_F13: u32 = 57376;
const CODE_F35: u32 = 57398;
const FIRST_EXTENDED_FUNCTION_KEY: u8 = 13;
const KITTY_PRIVATE_USE: std::ops::RangeInclusive<u32> = 57344..=63743;

/// Keypad keys map to the key they stand for, as Emacs `function-key-map`
/// does for `kp-*` events.
const KEYPAD_KEYS: [(u32, KeyCode); 28] = [
    (57399, KeyCode::Char('0')),
    (57400, KeyCode::Char('1')),
    (57401, KeyCode::Char('2')),
    (57402, KeyCode::Char('3')),
    (57403, KeyCode::Char('4')),
    (57404, KeyCode::Char('5')),
    (57405, KeyCode::Char('6')),
    (57406, KeyCode::Char('7')),
    (57407, KeyCode::Char('8')),
    (57408, KeyCode::Char('9')),
    (57409, KeyCode::Char('.')),
    (57410, KeyCode::Char('/')),
    (57411, KeyCode::Char('*')),
    (57412, KeyCode::Char('-')),
    (57413, KeyCode::Char('+')),
    (57414, KeyCode::Enter),
    (57415, KeyCode::Char('=')),
    (57416, KeyCode::Char(',')),
    (57417, KeyCode::Left),
    (57418, KeyCode::Right),
    (57419, KeyCode::Up),
    (57420, KeyCode::Down),
    (57421, KeyCode::PageUp),
    (57422, KeyCode::PageDown),
    (57423, KeyCode::Home),
    (57424, KeyCode::End),
    (57425, KeyCode::Insert),
    (57426, KeyCode::Delete),
];

const TILDE_KEYS: [(u32, KeyCode); 20] = [
    (1, KeyCode::Home),
    (2, KeyCode::Insert),
    (3, KeyCode::Delete),
    (4, KeyCode::End),
    (5, KeyCode::PageUp),
    (6, KeyCode::PageDown),
    (7, KeyCode::Home),
    (8, KeyCode::End),
    (11, KeyCode::F(1)),
    (12, KeyCode::F(2)),
    (13, KeyCode::F(3)),
    (14, KeyCode::F(4)),
    (15, KeyCode::F(5)),
    (17, KeyCode::F(6)),
    (18, KeyCode::F(7)),
    (19, KeyCode::F(8)),
    (20, KeyCode::F(9)),
    (21, KeyCode::F(10)),
    (23, KeyCode::F(11)),
    (24, KeyCode::F(12)),
];

/// Final bytes of `CSI 1 ; mods X` and `SS3 X` forms.
const LETTER_KEYS: [(u8, KeyCode); 10] = [
    (b'A', KeyCode::Up),
    (b'B', KeyCode::Down),
    (b'C', KeyCode::Right),
    (b'D', KeyCode::Left),
    (b'H', KeyCode::Home),
    (b'F', KeyCode::End),
    (b'P', KeyCode::F(1)),
    (b'Q', KeyCode::F(2)),
    (b'R', KeyCode::F(3)),
    (b'S', KeyCode::F(4)),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    Key(Key),
    /// Pasted bytes with CR LF and lone CR turned into LF, as Emacs
    /// `xterm--pasted-text` does.
    Paste(Vec<u8>),
    KittyFlags(u32),
    PrimaryDeviceAttributes,
    Unknown(Vec<u8>),
}

/// One decoding result and how many input bytes it consumed. A paste start
/// consumes its marker without producing an event yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Decoded {
    pub(crate) event: Option<InputEvent>,
    pub(crate) consumed: usize,
}

#[derive(Debug, Default)]
struct PasteInProgress {
    raw: Vec<u8>,
    searched: usize,
}

#[derive(Debug, Default)]
pub struct KittyDecoder {
    pending: Vec<u8>,
    paste: Option<PasteInProgress>,
}

enum Step {
    Event(InputEvent, usize),
    PasteStart(usize),
    Incomplete,
}

impl KittyDecoder {
    pub fn new() -> KittyDecoder {
        KittyDecoder::default()
    }

    /// Bytes held for a sequence, character or paste that has not finished.
    pub fn buffered_len(&self) -> usize {
        self.pending.len() + self.paste.as_ref().map_or(0, |paste| paste.raw.len())
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<InputEvent> {
        self.feed_accounted(bytes)
            .into_iter()
            .filter_map(|decoded| decoded.event)
            .collect()
    }

    pub(crate) fn feed_accounted(&mut self, bytes: &[u8]) -> Vec<Decoded> {
        match &mut self.paste {
            Some(paste) => paste.raw.extend_from_slice(bytes),
            None => self.pending.extend_from_slice(bytes),
        }
        let mut decoded = Vec::new();
        loop {
            if self.paste.is_some() {
                match self.finish_paste() {
                    Some(paste) => decoded.push(paste),
                    None => break,
                }
                continue;
            }
            match self.decode_pending(&mut decoded) {
                PendingOutcome::PasteStarted => continue,
                PendingOutcome::Stalled => break,
            }
        }
        decoded
    }

    fn decode_pending(&mut self, decoded: &mut Vec<Decoded>) -> PendingOutcome {
        let mut offset = 0;
        let mut outcome = PendingOutcome::Stalled;
        while offset < self.pending.len() {
            match decode_step(&self.pending[offset..]) {
                Step::Event(event, consumed) => {
                    decoded.push(Decoded {
                        event: Some(event),
                        consumed,
                    });
                    offset += consumed;
                }
                Step::PasteStart(consumed) => {
                    decoded.push(Decoded {
                        event: None,
                        consumed,
                    });
                    offset += consumed;
                    self.paste = Some(PasteInProgress {
                        raw: self.pending.split_off(offset),
                        searched: 0,
                    });
                    outcome = PendingOutcome::PasteStarted;
                    break;
                }
                Step::Incomplete => break,
            }
        }
        self.pending.drain(..offset);
        outcome
    }

    fn finish_paste(&mut self) -> Option<Decoded> {
        let paste = self
            .paste
            .as_mut()
            .expect("finish_paste is only called during a paste");
        let found = paste.raw[paste.searched..]
            .windows(PASTE_END.len())
            .position(|window| window == PASTE_END)
            .map(|position| paste.searched + position);
        let Some(end) = found else {
            paste.searched = paste.raw.len().saturating_sub(PASTE_END.len() - 1);
            return None;
        };
        let mut raw = std::mem::take(&mut paste.raw);
        self.paste = None;
        self.pending = raw.split_off(end + PASTE_END.len());
        raw.truncate(end);
        Some(Decoded {
            event: Some(InputEvent::Paste(normalize_line_ends(&raw))),
            consumed: end + PASTE_END.len(),
        })
    }
}

enum PendingOutcome {
    PasteStarted,
    Stalled,
}

fn normalize_line_ends(raw: &[u8]) -> Vec<u8> {
    let mut text = Vec::with_capacity(raw.len());
    let mut bytes = raw.iter().peekable();
    while let Some(&byte) = bytes.next() {
        if byte == b'\r' {
            text.push(b'\n');
            if bytes.peek() == Some(&&b'\n') {
                bytes.next();
            }
        } else {
            text.push(byte);
        }
    }
    text
}

fn key_event(code: KeyCode, mods: Mods) -> InputEvent {
    InputEvent::Key(Key { code, mods }.canonical())
}

fn unknown(bytes: &[u8], length: usize) -> Step {
    Step::Event(InputEvent::Unknown(bytes[..length].to_vec()), length)
}

fn ctrl() -> Mods {
    Mods {
        ctrl: true,
        ..Mods::default()
    }
}

/// `bytes` is never empty.
fn decode_step(bytes: &[u8]) -> Step {
    let first = bytes[0];
    let plain = Mods::default();
    match first {
        ESCAPE => decode_escape(bytes),
        b'\r' => Step::Event(key_event(KeyCode::Enter, plain), 1),
        b'\t' => Step::Event(key_event(KeyCode::Tab, plain), 1),
        0x7f => Step::Event(key_event(KeyCode::Backspace, plain), 1),
        0x00 => Step::Event(key_event(KeyCode::Char(' '), ctrl()), 1),
        0x01..=0x1a => Step::Event(
            key_event(KeyCode::Char(char::from(first + 0x60)), ctrl()),
            1,
        ),
        0x1c..=0x1f => Step::Event(
            key_event(KeyCode::Char(char::from(first + 0x40)), ctrl()),
            1,
        ),
        _ => decode_utf8(bytes),
    }
}

fn decode_utf8(bytes: &[u8]) -> Step {
    const MAX_UTF8_LENGTH: usize = 4;
    let prefix = &bytes[..bytes.len().min(MAX_UTF8_LENGTH)];
    let valid = match std::str::from_utf8(prefix) {
        Ok(text) => text,
        Err(error) if error.valid_up_to() > 0 => {
            std::str::from_utf8(&prefix[..error.valid_up_to()])
                .expect("valid_up_to bounds a valid prefix")
        }
        Err(error) => {
            return match error.error_len() {
                None => Step::Incomplete,
                Some(length) => unknown(bytes, length),
            };
        }
    };
    let character = valid
        .chars()
        .next()
        .expect("a non-empty valid prefix has a first character");
    let length = character.len_utf8();
    if character.is_control() {
        return unknown(bytes, length);
    }
    Step::Event(key_event(KeyCode::Char(character), Mods::default()), length)
}

fn decode_escape(bytes: &[u8]) -> Step {
    const SS3_LENGTH: usize = 3;
    let Some(&second) = bytes.get(1) else {
        return Step::Incomplete;
    };
    match second {
        b'[' => decode_csi(bytes),
        b'O' => {
            let Some(&final_byte) = bytes.get(2) else {
                return Step::Incomplete;
            };
            if !is_final_byte(final_byte) {
                return unknown(bytes, 2);
            }
            match letter_key(final_byte) {
                Some(code) => Step::Event(key_event(code, Mods::default()), SS3_LENGTH),
                None => unknown(bytes, SS3_LENGTH),
            }
        }
        _ => Step::Event(key_event(KeyCode::Escape, Mods::default()), 1),
    }
}

fn is_final_byte(byte: u8) -> bool {
    (0x40..=0x7e).contains(&byte)
}

fn is_parameter_byte(byte: u8) -> bool {
    (0x30..=0x3f).contains(&byte)
}

fn is_intermediate_byte(byte: u8) -> bool {
    (0x20..=0x2f).contains(&byte)
}

fn decode_csi(bytes: &[u8]) -> Step {
    let mut index = CSI_PREFIX.len();
    let mut intermediate_start = None;
    let final_index = loop {
        if index == MAX_PENDING_SEQUENCE {
            return unknown(bytes, MAX_PENDING_SEQUENCE);
        }
        let Some(&byte) = bytes.get(index) else {
            return Step::Incomplete;
        };
        if is_final_byte(byte) {
            break index;
        }
        if is_parameter_byte(byte) && intermediate_start.is_none() {
            index += 1;
            continue;
        }
        if is_intermediate_byte(byte) {
            intermediate_start.get_or_insert(index);
            index += 1;
            continue;
        }
        return unknown(bytes, index);
    };
    let length = final_index + 1;
    if intermediate_start.is_some() {
        return unknown(bytes, length);
    }
    let parameters = &bytes[CSI_PREFIX.len()..final_index];
    match interpret_csi(parameters, bytes[final_index]) {
        Some(Interpretation::Event(event)) => Step::Event(event, length),
        Some(Interpretation::PasteStart) => Step::PasteStart(length),
        None => unknown(bytes, length),
    }
}

enum Interpretation {
    Event(InputEvent),
    PasteStart,
}

/// Semicolon-separated fields of colon-separated numbers; an empty number is
/// `None`. Returns `None` for anything but digits, `;` and `:`, or overflow.
fn parse_fields(parameters: &[u8]) -> Option<Vec<Vec<Option<u32>>>> {
    if parameters.is_empty() {
        return Some(Vec::new());
    }
    parameters
        .split(|byte| *byte == b';')
        .map(|field| {
            field
                .split(|byte| *byte == b':')
                .map(|number| {
                    if number.is_empty() {
                        return Some(None);
                    }
                    if !number.iter().all(u8::is_ascii_digit) {
                        return None;
                    }
                    std::str::from_utf8(number)
                        .ok()?
                        .parse::<u32>()
                        .ok()
                        .map(Some)
                })
                .collect()
        })
        .collect()
}

fn interpret_csi(parameters: &[u8], final_byte: u8) -> Option<Interpretation> {
    if let Some(reply) = parameters.strip_prefix(b"?") {
        return interpret_reply(reply, final_byte).map(Interpretation::Event);
    }
    let fields = parse_fields(parameters)?;
    match final_byte {
        b'u' => interpret_unicode_key(&fields).map(Interpretation::Event),
        b'~' => interpret_tilde(&fields),
        b'Z' if fields.is_empty() => Some(Interpretation::Event(key_event(
            KeyCode::Tab,
            Mods {
                shift: true,
                ..Mods::default()
            },
        ))),
        letter => {
            let code = letter_key(letter)?;
            let mods = match fields.as_slice() {
                [] => Mods::default(),
                [number] if single_number(number)? == 1 => Mods::default(),
                [number, modifiers] if single_number(number)? == 1 => parse_modifiers(modifiers)?,
                _ => return None,
            };
            Some(Interpretation::Event(key_event(code, mods)))
        }
    }
}

fn interpret_reply(reply: &[u8], final_byte: u8) -> Option<InputEvent> {
    match final_byte {
        b'u' => {
            let fields = parse_fields(reply)?;
            match fields.as_slice() {
                [flags] => Some(InputEvent::KittyFlags(single_number(flags)?)),
                _ => None,
            }
        }
        b'c' => {
            let fields = parse_fields(reply)?;
            let well_formed = !fields.is_empty()
                && fields
                    .iter()
                    .all(|field| matches!(field.as_slice(), [Some(_)]));
            well_formed.then_some(InputEvent::PrimaryDeviceAttributes)
        }
        _ => None,
    }
}

fn single_number(field: &[Option<u32>]) -> Option<u32> {
    match field {
        [Some(number)] => Some(*number),
        _ => None,
    }
}

fn letter_key(final_byte: u8) -> Option<KeyCode> {
    LETTER_KEYS
        .iter()
        .find(|(letter, _)| *letter == final_byte)
        .map(|(_, code)| *code)
}

/// `modifiers[:event-type]`; release events and hyper are not representable.
fn parse_modifiers(field: &[Option<u32>]) -> Option<Mods> {
    let (value, event) = match field {
        [value] => (value.unwrap_or(1), EVENT_PRESS),
        [value, event] => (value.unwrap_or(1), event.unwrap_or(EVENT_PRESS)),
        _ => return None,
    };
    if event != EVENT_PRESS && event != EVENT_REPEAT {
        return None;
    }
    let bits = value.checked_sub(1)?;
    if bits & !KNOWN_MODIFIERS != 0 || bits & MODIFIER_HYPER != 0 {
        return None;
    }
    Some(Mods {
        ctrl: bits & MODIFIER_CTRL != 0,
        meta: bits & (MODIFIER_ALT | MODIFIER_META) != 0,
        shift: bits & MODIFIER_SHIFT != 0,
        super_: bits & MODIFIER_SUPER != 0,
    })
}

fn unicode_key_code(code: u32) -> Option<KeyCode> {
    match code {
        CODE_ENTER => return Some(KeyCode::Enter),
        CODE_TAB => return Some(KeyCode::Tab),
        CODE_BACKSPACE => return Some(KeyCode::Backspace),
        CODE_ESCAPE => return Some(KeyCode::Escape),
        CODE_F13..=CODE_F35 => {
            let offset = u8::try_from(code - CODE_F13).expect("F13..=F35 offset fits in u8");
            return Some(KeyCode::F(FIRST_EXTENDED_FUNCTION_KEY + offset));
        }
        _ => {}
    }
    if let Some((_, keypad)) = KEYPAD_KEYS.iter().find(|(number, _)| *number == code) {
        return Some(*keypad);
    }
    if KITTY_PRIVATE_USE.contains(&code) {
        return None;
    }
    printable_char(code).map(KeyCode::Char)
}

fn printable_char(code: u32) -> Option<char> {
    char::from_u32(code).filter(|character| !character.is_control())
}

/// `CSI code[:shifted[:base]] ; mods[:event] ; text u`. The shifted key, when
/// the terminal reports it, is what Emacs sees (M-% rather than M-S-5).
fn interpret_unicode_key(fields: &[Vec<Option<u32>>]) -> Option<InputEvent> {
    let (key_field, modifier_field) = match fields {
        [key] => (key, None),
        [key, modifiers] => (key, Some(modifiers)),
        [key, modifiers, _text] => (key, Some(modifiers)),
        _ => return None,
    };
    let (code, shifted) = match key_field.as_slice() {
        [Some(code)] => (*code, None),
        [Some(code), shifted] | [Some(code), shifted, _] => (*code, *shifted),
        _ => return None,
    };
    let mut mods = match modifier_field {
        Some(field) => parse_modifiers(field)?,
        None => Mods::default(),
    };
    let mut key_code = unicode_key_code(code)?;
    if let (true, KeyCode::Char(_), Some(shifted)) = (mods.shift, key_code, shifted) {
        key_code = KeyCode::Char(printable_char(shifted)?);
        mods.shift = false;
    }
    Some(key_event(key_code, mods))
}

fn interpret_tilde(fields: &[Vec<Option<u32>>]) -> Option<Interpretation> {
    let (number, mods) = match fields {
        [number] => (single_number(number)?, None),
        [number, modifiers] => (single_number(number)?, Some(parse_modifiers(modifiers)?)),
        _ => return None,
    };
    if number == PASTE_START_NUMBER && mods.is_none() {
        return Some(Interpretation::PasteStart);
    }
    let code = TILDE_KEYS
        .iter()
        .find(|(candidate, _)| *candidate == number)
        .map(|(_, code)| *code)?;
    Some(Interpretation::Event(key_event(
        code,
        mods.unwrap_or_default(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn every_byte_is_accounted_for(
            input in prop::collection::vec(
                prop_oneof![
                    3 => prop::sample::select(vec![ESCAPE, b'[', b'O', b'?', b';', b':', b'u', b'~', b'c', b'2', b'0', b'1', b'\r', 0xc3, 0xa9]),
                    1 => any::<u8>(),
                ],
                0..400,
            ),
            chunk in 1usize..40,
        ) {
            let mut decoder = KittyDecoder::new();
            let mut consumed = 0;
            for piece in input.chunks(chunk) {
                for decoded in decoder.feed_accounted(piece) {
                    prop_assert!(decoded.consumed > 0);
                    prop_assert!(decoded.event.is_some() || decoded.consumed == b"\x1b[200~".len());
                    consumed += decoded.consumed;
                }
                prop_assert!(decoder.pending.len() < MAX_PENDING_SEQUENCE);
            }
            prop_assert_eq!(consumed + decoder.buffered_len(), input.len());
        }
    }

    #[test]
    fn unknown_events_carry_exactly_their_bytes() {
        let input = b"\x1b[5555~\xff\x1bOz";
        let decoded = KittyDecoder::new().feed_accounted(input);
        let mut rebuilt = Vec::new();
        for item in decoded {
            match item.event {
                Some(InputEvent::Unknown(bytes)) => {
                    assert_eq!(bytes.len(), item.consumed);
                    rebuilt.extend(bytes);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(rebuilt, input);
    }
}
