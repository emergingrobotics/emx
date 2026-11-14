use emx_term::{InputEvent, KittyDecoder, MAX_PENDING_SEQUENCE, PASTE_CHUNK_MAX};
use emx_types::{Key, KeyCode, Mods, parse_kbd};
use proptest::prelude::*;

fn decode(bytes: &[u8]) -> Vec<InputEvent> {
    KittyDecoder::new().feed(bytes)
}

fn paste(data: &[u8]) -> InputEvent {
    InputEvent::Paste {
        data: data.to_vec(),
        last: true,
    }
}

fn kbd(text: &str) -> InputEvent {
    let mut keys = parse_kbd(text).unwrap();
    assert_eq!(keys.len(), 1, "{text} is one key");
    InputEvent::Key(keys.remove(0))
}

fn assert_keys(bytes: &[u8], expected: &[&str]) {
    let wanted: Vec<InputEvent> = expected.iter().map(|text| kbd(text)).collect();
    assert_eq!(
        decode(bytes),
        wanted,
        "decoding {:?}",
        String::from_utf8_lossy(bytes)
    );
}

#[test]
fn decode_ctrl_meta_combos() {
    assert_keys(b"\x1b[120;5u", &["C-x"]);
    assert_keys(b"\x1b[37;3u", &["M-%"]);
    assert_keys(b"\x1b[115;7u", &["C-M-s"]);
    assert_keys(b"\x1b[32;5u", &["C-SPC"]);
    assert_keys(b"\x1b[47;5u", &["C-/"]);
    assert_keys(b"\x1b[120;5u\x1b[102;5u", &["C-x", "C-f"]);
}

#[test]
fn decode_modifier_bits_and_canonical_forms() {
    assert_keys(b"\x1b[97;6u", &["C-S-a"]);
    assert_keys(b"\x1b[97;4u", &["M-A"]);
    assert_keys(b"\x1b[97;9u", &["s-a"]);
    assert_keys(b"\x1b[97;33u", &["M-a"]);
    assert_keys(b"\x1b[105;5u", &["TAB"]);
    assert_keys(b"\x1b[109;5u", &["RET"]);
    assert_keys(b"\x1b[91;5u", &["ESC"]);
    assert_keys(b"\x1b[105;7u", &["M-TAB"]);
    assert_keys(b"\x1b[97;69u", &["C-a"]);
    assert_keys(b"\x1b[97;133u", &["C-a"]);
    assert_keys(b"\x1b[97:65;6u", &["C-S-a"]);
    assert_keys(b"\x1b[97;5:1u", &["C-a"]);
    assert_keys(b"\x1b[97;5;97u", &["C-a"]);
}

#[test]
fn decode_shifted_alternate_key_replaces_shift() {
    assert_keys(b"\x1b[53:37;4u", &["M-%"]);
    assert_keys(b"\x1b[44:60;4u", &["M-<"]);
    assert_keys(b"\x1b[97:65;4u", &["M-A"]);
    assert_keys(b"\x1b[53::53;3u", &["M-5"]);
}

#[test]
fn decode_legacy_enter_tab_backspace_and_escape() {
    assert_keys(b"\r", &["RET"]);
    assert_keys(b"\t", &["TAB"]);
    assert_keys(b"\x7f", &["DEL"]);
    assert_keys(b"\x1b[27u", &["ESC"]);
    assert_keys(b"\x1b[27;3u", &["M-ESC"]);
    assert_keys(b"\x1b[13;5u", &["C-RET"]);
    assert_keys(b"\x1b[13;3u", &["M-RET"]);
    assert_keys(b"\x1b[127;3u", &["M-DEL"]);
    assert_keys(b"\x1b[127;5u", &["C-DEL"]);
    assert_keys(b"\x1b[9;2u", &["S-TAB"]);
    assert_keys(b"\x1b[Z", &["S-TAB"]);
}

#[test]
fn decode_legacy_control_bytes_as_ctrl_keys() {
    assert_keys(b"\x18\x06", &["C-x", "C-f"]);
    assert_keys(b"\x00", &["C-SPC"]);
    assert_keys(b"\x1c\x1d\x1e", &["C-\\", "C-]", "C-^"]);
    assert_keys(b"\x1f", &["C-_"]);
    assert_keys(b"\x08", &["C-h"]);
}

#[test]
fn decode_plain_utf8_text() {
    assert_keys(
        "aZ é€😀 %".as_bytes(),
        &["a", "Z", "SPC", "é", "€", "😀", "SPC", "%"],
    );
    assert_eq!(
        decode(b"A"),
        vec![InputEvent::Key(Key {
            code: KeyCode::Char('A'),
            mods: Mods::default()
        })]
    );
}

#[test]
fn decode_raw_escape_before_other_byte_is_unknown() {
    let unknown = |bytes: &[u8]| InputEvent::Unknown(bytes.to_vec());
    assert_eq!(decode(b"\x1bx"), vec![unknown(b"\x1bx")]);
    assert_eq!(decode(b"\x1b\r"), vec![unknown(b"\x1b\r")]);
    assert_eq!(decode(b"\x1b\x1b[A"), vec![unknown(b"\x1b"), kbd("<up>")]);
    assert_eq!(decode("\x1bé".as_bytes()), vec![unknown(b"\x1b"), kbd("é")]);
}

#[test]
fn decode_cursor_and_editing_keys() {
    assert_keys(
        b"\x1b[A\x1b[B\x1b[C\x1b[D",
        &["<up>", "<down>", "<right>", "<left>"],
    );
    assert_keys(b"\x1b[H\x1b[F", &["<home>", "<end>"]);
    assert_keys(b"\x1b[1;2D", &["S-<left>"]);
    assert_keys(b"\x1b[1;5A", &["C-<up>"]);
    assert_keys(b"\x1b[1;3C", &["M-<right>"]);
    assert_keys(b"\x1b[1;5H", &["C-<home>"]);
    assert_keys(b"\x1b[1;6F", &["C-S-<end>"]);
    assert_keys(b"\x1bOA\x1bOH\x1bOF", &["<up>", "<home>", "<end>"]);
    assert_keys(
        b"\x1b[2~\x1b[3~\x1b[5~\x1b[6~",
        &["<insert>", "<delete>", "<prior>", "<next>"],
    );
    assert_keys(
        b"\x1b[7~\x1b[8~\x1b[1~\x1b[4~",
        &["<home>", "<end>", "<home>", "<end>"],
    );
    assert_keys(b"\x1b[3;5~\x1b[5;3~", &["C-<delete>", "M-<prior>"]);
}

#[test]
fn decode_function_keys() {
    assert_keys(
        b"\x1bOP\x1bOQ\x1bOR\x1bOS",
        &["<f1>", "<f2>", "<f3>", "<f4>"],
    );
    assert_keys(b"\x1b[P\x1b[Q\x1b[S", &["<f1>", "<f2>", "<f4>"]);
    assert_keys(
        b"\x1b[1;5P\x1b[1;2Q\x1b[1;3R\x1b[1;5S",
        &["C-<f1>", "S-<f2>", "M-<f3>", "C-<f4>"],
    );
    assert_keys(
        b"\x1b[11~\x1b[12~\x1b[13~\x1b[14~\x1b[15~\x1b[17~\x1b[18~\x1b[19~\x1b[20~\x1b[21~\x1b[23~\x1b[24~",
        &["<f1>", "<f2>", "<f3>", "<f4>", "<f5>", "<f6>", "<f7>", "<f8>", "<f9>", "<f10>", "<f11>", "<f12>"],
    );
    assert_keys(b"\x1b[24;5~\x1b[15;2~", &["C-<f12>", "S-<f5>"]);
    assert_keys(
        b"\x1b[57376u\x1b[57398u\x1b[57376;5u",
        &["<f13>", "<f35>", "C-<f13>"],
    );
}

#[test]
fn decode_keypad_keys_as_their_main_keys() {
    assert_keys(
        b"\x1b[57414u\x1b[57419u\x1b[57417;5u",
        &["RET", "<up>", "C-<left>"],
    );
    assert_keys(
        b"\x1b[57399u\x1b[57408u\x1b[57409u\x1b[57413u",
        &["0", "9", ".", "+"],
    );
    assert_keys(
        b"\x1b[57423u\x1b[57424u\x1b[57425u\x1b[57426u",
        &["<home>", "<end>", "<insert>", "<delete>"],
    );
    assert_keys(b"\x1b[57421u\x1b[57422u", &["<prior>", "<next>"]);
}

#[test]
fn decode_terminal_replies() {
    assert_eq!(decode(b"\x1b[?1u"), vec![InputEvent::KittyFlags(1)]);
    assert_eq!(decode(b"\x1b[?0u"), vec![InputEvent::KittyFlags(0)]);
    assert_eq!(decode(b"\x1b[?31u"), vec![InputEvent::KittyFlags(31)]);
    assert_eq!(
        decode(b"\x1b[?62;22c"),
        vec![InputEvent::PrimaryDeviceAttributes]
    );
    assert_eq!(
        decode(b"\x1b[?1;2c"),
        vec![InputEvent::PrimaryDeviceAttributes]
    );
}

#[test]
fn decode_split_sequence_across_feeds() {
    let input: Vec<u8> = [
        b"\x1b[120;5u".as_slice(),
        "é€😀".as_bytes(),
        b"\x1b[1;5A\x1bOP\x1b[24;5~\r",
        b"\x1b[200~a\r\nb\x1b[201~",
        b"\x1b[?1u\x1b[?62;22c",
    ]
    .concat();
    let whole = decode(&input);
    assert_eq!(whole.len(), 11);
    for split in 0..=input.len() {
        let mut decoder = KittyDecoder::new();
        let mut events = decoder.feed(&input[..split]);
        events.extend(decoder.feed(&input[split..]));
        assert_eq!(events, whole, "split at {split}");
    }
    let mut decoder = KittyDecoder::new();
    let mut events = Vec::new();
    for byte in &input {
        events.extend(decoder.feed(std::slice::from_ref(byte)));
    }
    assert_eq!(events, whole, "byte at a time");
}

#[test]
fn decode_bracketed_paste() {
    assert_eq!(
        decode(b"\x1b[200~line one\r\nline two\rline three\nend\x1b[201~x"),
        vec![paste(b"line one\nline two\nline three\nend"), kbd("x")]
    );
    assert_eq!(decode(b"\x1b[200~\x1b[201~"), vec![paste(b"")]);
    assert_eq!(
        decode(b"\x1b[200~\r\r\n\r\x1b[201~"),
        vec![paste(b"\n\n\n")]
    );
}

#[test]
fn decode_paste_content_is_never_keys() {
    let content = b"\x1b[120;5u\x03\x1b[?1u\x1b[?62c\x1b\xff\x1b[200~nested";
    let input = [b"\x1b[200~".as_slice(), content, b"\x1b[201~"].concat();
    assert_eq!(decode(&input), vec![paste(content)]);
}

#[test]
fn decode_paste_split_across_feeds_is_reassembled() {
    let input = b"a\x1b[200~one\r\ntwo\r\x1b[201~b";
    let expected = vec![kbd("a"), paste(b"one\ntwo\n"), kbd("b")];
    for split in 0..=input.len() {
        let mut decoder = KittyDecoder::new();
        let mut events = decoder.feed(&input[..split]);
        events.extend(decoder.feed(&input[split..]));
        assert_eq!(events, expected, "split at {split}");
    }
    let mut decoder = KittyDecoder::new();
    let mut events = Vec::new();
    for byte in input {
        events.extend(decoder.feed(&[*byte]));
    }
    assert_eq!(events, expected);
}

#[test]
fn decode_paste_crlf_split_across_feeds() {
    let mut decoder = KittyDecoder::new();
    let mut events = decoder.feed(b"\x1b[200~a\r");
    events.extend(decoder.feed(b"\nb\x1b[201~"));
    assert_eq!(events, vec![paste(b"a\nb")]);
}

#[test]
fn decode_unknown_and_garbled_sequences() {
    let unknown = |bytes: &[u8]| InputEvent::Unknown(bytes.to_vec());
    assert_eq!(decode(b"\x1b[57441u"), vec![unknown(b"\x1b[57441u")]);
    assert_eq!(decode(b"\x1b[5555~"), vec![unknown(b"\x1b[5555~")]);
    assert_eq!(decode(b"\x1b[1;5X"), vec![unknown(b"\x1b[1;5X")]);
    assert_eq!(decode(b"\xff"), vec![unknown(b"\xff")]);
    assert_eq!(decode(b"\xc3("), vec![unknown(b"\xc3"), kbd("(")]);
    assert_eq!(decode(b"\x1bOz"), vec![unknown(b"\x1bOz")]);
    assert_eq!(
        decode(b"\x1b[12\x01"),
        vec![unknown(b"\x1b[12"), kbd("C-a")]
    );
    assert_eq!(
        decode(b"\x1b[99999999999999999999u"),
        vec![unknown(b"\x1b[99999999999999999999u")]
    );
    assert_eq!(decode(b"\x1b[97;1:3u"), vec![unknown(b"\x1b[97;1:3u")]);
    assert_eq!(decode(b"\x1b[55296u"), vec![unknown(b"\x1b[55296u")]);
    assert_eq!(decode(b"\x1b[97;0u"), vec![unknown(b"\x1b[97;0u")]);
    assert_eq!(decode(b"\x1b[97;17u"), vec![unknown(b"\x1b[97;17u")]);
    assert_eq!(decode(b"\x1b[0u"), vec![unknown(b"\x1b[0u")]);
    assert_eq!(decode(b"\x1b[?u"), vec![unknown(b"\x1b[?u")]);
    assert_eq!(decode(b"\x1b[>1u"), vec![unknown(b"\x1b[>1u")]);
    assert_eq!(decode(b"\x1b[1 q"), vec![unknown(b"\x1b[1 q")]);
    assert_eq!(
        decode("\u{85}".as_bytes()),
        vec![unknown("\u{85}".as_bytes())]
    );
    assert_eq!(decode(b"\x1b[201~"), vec![unknown(b"\x1b[201~")]);
    assert_eq!(decode(b"\x1b[2;5;7~"), vec![unknown(b"\x1b[2;5;7~")]);
}

#[test]
fn decode_pending_sequence_is_capped_and_its_tail_discarded() {
    let mut decoder = KittyDecoder::new();
    let mut input = b"\x1b[".to_vec();
    input.extend(std::iter::repeat_n(b'1', MAX_PENDING_SEQUENCE + 100));
    let events = decoder.feed(&input);
    assert_eq!(
        events,
        vec![InputEvent::Unknown(input[..MAX_PENDING_SEQUENCE].to_vec())]
    );
    assert_eq!(decoder.buffered_len(), 0);
    assert_eq!(decoder.feed(b"22;5u"), Vec::new());
    assert_eq!(decoder.feed(b"x"), vec![kbd("x")]);

    let mut decoder = KittyDecoder::new();
    let mut events = Vec::new();
    for _ in 0..MAX_PENDING_SEQUENCE {
        events.extend(decoder.feed(b"\x1b[1"));
        assert!(decoder.buffered_len() < MAX_PENDING_SEQUENCE);
    }
    assert!(events.iter().all(
        |event| matches!(event, InputEvent::Unknown(bytes) if bytes.len() <= MAX_PENDING_SEQUENCE)
    ));
}

#[test]
fn decode_overflow_tail_is_feed_boundary_independent() {
    let mut input = b"a\x1b[".to_vec();
    input.extend(std::iter::repeat_n(b'1', MAX_PENDING_SEQUENCE * 2));
    input.extend_from_slice(b"~b\x1b]");
    input.extend(std::iter::repeat_n(b'z', MAX_PENDING_SEQUENCE * 2));
    input.extend_from_slice(b"\x1b\\c");
    let whole = decode(&input);
    assert_eq!(
        whole,
        vec![
            kbd("a"),
            InputEvent::Unknown(input[1..1 + MAX_PENDING_SEQUENCE].to_vec()),
            kbd("b"),
            InputEvent::Unknown([b"\x1b]".as_slice(), &[b'z'; MAX_PENDING_SEQUENCE - 2]].concat()),
            kbd("c"),
        ]
    );
    let mut decoder = KittyDecoder::new();
    let mut events = Vec::new();
    for byte in &input {
        events.extend(decoder.feed(std::slice::from_ref(byte)));
    }
    assert_eq!(events, whole);
}

#[test]
fn decode_string_sequences_are_unknown_through_their_terminator() {
    let unknown = |bytes: &[u8]| InputEvent::Unknown(bytes.to_vec());
    assert_eq!(
        decode(b"\x1b]0;title\x07a"),
        vec![unknown(b"\x1b]0;title\x07"), kbd("a")]
    );
    assert_eq!(
        decode(b"\x1b]52;c;?\x1b\\a"),
        vec![unknown(b"\x1b]52;c;?\x1b\\"), kbd("a")]
    );
    assert_eq!(
        decode(b"\x1bP>|kitty(0.30)\x1b\\"),
        vec![unknown(b"\x1bP>|kitty(0.30)\x1b\\")]
    );
    assert_eq!(
        decode(b"\x1b_Gi=1;OK\x1b\\"),
        vec![unknown(b"\x1b_Gi=1;OK\x1b\\")]
    );
    assert_eq!(decode(b"\x1b^pm\x07"), vec![unknown(b"\x1b^pm\x07")]);
    assert_eq!(decode(b"\x1bXsos\x07"), vec![unknown(b"\x1bXsos\x07")]);
    assert_eq!(
        decode(b"\x1b]0;t\x1b[A"),
        vec![unknown(b"\x1b]0;t"), kbd("<up>")]
    );
    let mut decoder = KittyDecoder::new();
    assert_eq!(decoder.feed(b"\x1b]0;abc\x1b"), Vec::new());
    assert_eq!(
        decoder.feed(b"\\d"),
        vec![unknown(b"\x1b]0;abc\x1b\\"), kbd("d")]
    );
}

#[test]
fn decode_string_overflow_with_split_terminator() {
    let mut input = b"\x1b]".to_vec();
    input.extend(std::iter::repeat_n(b'a', MAX_PENDING_SEQUENCE - 3));
    input.extend_from_slice(b"\x1b\\b");
    assert_eq!(input[MAX_PENDING_SEQUENCE - 1], 0x1b);
    assert_eq!(
        decode(&input),
        vec![
            InputEvent::Unknown(input[..MAX_PENDING_SEQUENCE - 1].to_vec()),
            kbd("b")
        ]
    );
    let mut long = b"\x1b]".to_vec();
    long.extend(std::iter::repeat_n(b'a', MAX_PENDING_SEQUENCE * 3));
    long.extend_from_slice(b"\x07b");
    assert_eq!(
        decode(&long),
        vec![
            InputEvent::Unknown(long[..MAX_PENDING_SEQUENCE].to_vec()),
            kbd("b")
        ]
    );
}

#[test]
fn decode_ss3_garbage_is_unknown_through_its_final_byte() {
    let unknown = |bytes: &[u8]| InputEvent::Unknown(bytes.to_vec());
    assert_eq!(decode(b"\x1bO5P"), vec![unknown(b"\x1bO5P")]);
    assert_eq!(decode(b"\x1bO1;5Px"), vec![unknown(b"\x1bO1;5P"), kbd("x")]);
    assert_eq!(decode(b"\x1bO\x01"), vec![unknown(b"\x1bO"), kbd("C-a")]);
    assert_eq!(decode(b"\x1b[1 1u"), vec![unknown(b"\x1b[1 1u")]);
}

fn paste_data(events: &[InputEvent]) -> (Vec<u8>, Vec<bool>) {
    let mut data = Vec::new();
    let mut lasts = Vec::new();
    for event in events {
        match event {
            InputEvent::Paste { data: chunk, last } => {
                assert!(chunk.len() <= PASTE_CHUNK_MAX);
                data.extend_from_slice(chunk);
                lasts.push(*last);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    (data, lasts)
}

#[test]
fn decode_large_paste_is_chunked_across_feeds() {
    let size = 3 * PASTE_CHUNK_MAX + PASTE_CHUNK_MAX / 2;
    let mut content = Vec::with_capacity(size);
    let mut expected = Vec::with_capacity(size);
    let mut index = 0usize;
    while content.len() < size {
        match index % 7 {
            0 => {
                content.extend_from_slice(b"\r\n");
                expected.push(b'\n');
            }
            3 => {
                content.push(b'\r');
                expected.push(b'\n');
            }
            _ => {
                let byte = b'a' + (index % 26) as u8;
                content.push(byte);
                expected.push(byte);
            }
        }
        index += 1;
    }
    let input = [b"\x1b[200~".as_slice(), &content, b"\x1b[201~x"].concat();
    let mut decoder = KittyDecoder::new();
    let mut events = Vec::new();
    for piece in input.chunks(100_003) {
        events.extend(decoder.feed(piece));
        assert!(decoder.buffered_len() < PASTE_CHUNK_MAX + b"\x1b[201~".len());
    }
    assert_eq!(events.pop(), Some(kbd("x")));
    let (data, lasts) = paste_data(&events);
    assert_eq!(data, expected);
    assert_eq!(lasts.len(), 4);
    assert_eq!(lasts, vec![false, false, false, true]);
    assert_eq!(events, decode(&input)[..4].to_vec());
}

#[test]
fn decode_unterminated_paste_emits_chunks_as_data_arrives() {
    let mut decoder = KittyDecoder::new();
    assert_eq!(decoder.feed(b"\x1b[200~"), Vec::new());
    let mut events = Vec::new();
    for _ in 0..5 {
        events.extend(decoder.feed(&vec![b'q'; PASTE_CHUNK_MAX / 2]));
        assert!(decoder.buffered_len() < PASTE_CHUNK_MAX + b"\x1b[201~".len());
    }
    let (data, lasts) = paste_data(&events);
    assert_eq!(lasts, vec![false, false]);
    assert_eq!(data.len(), 2 * PASTE_CHUNK_MAX);
}

#[test]
fn decode_paste_crlf_split_at_chunk_boundary() {
    let mut content = vec![b'a'; PASTE_CHUNK_MAX - 1];
    content.extend_from_slice(b"\r\nb");
    let input = [b"\x1b[200~".as_slice(), &content, b"\x1b[201~"].concat();
    let events = decode(&input);
    let mut first = vec![b'a'; PASTE_CHUNK_MAX - 1];
    first.push(b'\n');
    assert_eq!(
        events,
        vec![
            InputEvent::Paste {
                data: first,
                last: false
            },
            InputEvent::Paste {
                data: b"b".to_vec(),
                last: true
            },
        ]
    );
}

fn byte_stream() -> impl Strategy<Value = Vec<u8>> {
    let interesting = prop::sample::select(vec![
        0x1b_u8, b'[', b'O', b'?', b';', b':', b'u', b'~', b'c', b'A', b'P', b'0', b'1', b'2',
        b'5', b'9', b'\r', b'\n', b']', b'\\', b'_', 0x07, 0x7f, 0xc3, 0xa9, 0xe2, 0x82, 0xac,
        0xf0, 0x9f,
    ]);
    prop::collection::vec(prop_oneof![3 => interesting, 1 => any::<u8>()], 0..400)
}

proptest! {
    #[test]
    fn decode_random_bytes_never_panics_and_is_chunk_invariant(
        mut input in byte_stream(),
        splits in prop::collection::vec(any::<prop::sample::Index>(), 0..8),
        with_paste in any::<bool>(),
    ) {
        if with_paste {
            input.splice(0..0, b"\x1b[200~".iter().copied());
            input.extend_from_slice(b"\x1b[201~");
        }
        let mut whole_decoder = KittyDecoder::new();
        let whole = whole_decoder.feed(&input);
        let mut points: Vec<usize> = splits.iter().map(|index| index.index(input.len() + 1)).collect();
        points.sort_unstable();
        let mut decoder = KittyDecoder::new();
        let mut events = Vec::new();
        let mut start = 0;
        for point in points {
            events.extend(decoder.feed(&input[start..point]));
            start = point;
        }
        events.extend(decoder.feed(&input[start..]));
        prop_assert_eq!(events, whole);
        prop_assert!(decoder.buffered_len() <= input.len());
    }
}
