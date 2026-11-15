//! Legacy-terminal decoding: the bytes each supported legacy terminal sends
//! for the tier-1 keys, from the terminals' documented encodings (xterm's
//! ctlseqs, VTE, Alacritty, the Linux console's keymaps, tmux, screen),
//! with Alt sending an ESC prefix and xterm's `modifyOtherKeys` at level 1.

use emx_term::{Decoder, InputEvent, MAX_PENDING_SEQUENCE, Protocol};
use emx_types::{Key, Mods, MouseButton, MouseEvent, MouseKind, parse_kbd};
use proptest::prelude::*;

fn key(text: &str) -> InputEvent {
    let mut keys = parse_kbd(text).unwrap();
    assert_eq!(keys.len(), 1, "{text} is one key");
    InputEvent::Key(keys.remove(0))
}

/// Decodes `bytes` as one read, then lets the escape timeout pass.
fn decode(bytes: &[u8]) -> Vec<InputEvent> {
    let mut decoder = Decoder::new(Protocol::Legacy);
    let mut events = decoder.feed(bytes);
    events.extend(decoder.timeout());
    events
}

/// Each case is the bytes and the keys they are, as Emacs reads them
/// before `esc-map` folding: Meta arrives as ESC then the key.
fn check(terminal: &str, cases: &[(&[u8], &[&str])]) {
    for (bytes, keys) in cases {
        let wanted: Vec<InputEvent> = keys.iter().map(|text| key(text)).collect();
        assert_eq!(
            decode(bytes),
            wanted,
            "{terminal}: {:?}",
            String::from_utf8_lossy(bytes)
        );
    }
}

/// What every terminal sends the same way.
const COMMON: &[(&[u8], &[&str])] = &[
    (b"a", &["a"]),
    (b"\xc3\xa9", &["\u{e9}"]),
    (b"\x01", &["C-a"]),
    (b"\x18\x06", &["C-x", "C-f"]),
    (b"\x18\x13", &["C-x", "C-s"]),
    (b"\x00", &["C-SPC"]),
    (b"\x1f", &["C-_"]),
    (b"\x07", &["C-g"]),
    (b"\r", &["RET"]),
    (b"\t", &["TAB"]),
    (b"\x7f", &["DEL"]),
    (b"\x1b", &["ESC"]),
    (b"\x1bx", &["ESC", "x"]),
    (b"\x1b%", &["ESC", "%"]),
    (b"\x1b<", &["ESC", "<"]),
    (b"\x1b>", &["ESC", ">"]),
    (b"\x1b\x13", &["ESC", "C-s"]),
    (b"\x1b\x7f", &["ESC", "DEL"]),
    (b"\x1b\x1b\x1b", &["ESC", "ESC", "ESC"]),
    (b"\x1b]", &["ESC", "]"]),
    (b"\x1bP", &["ESC", "P"]),
    (b"\x1b[Z", &["S-TAB"]),
    (b"\x1b[3~", &["<delete>"]),
    (b"\x1b[2~", &["<insert>"]),
    (b"\x1b[5~", &["<prior>"]),
    (b"\x1b[6~", &["<next>"]),
    (b"\x1b[19~", &["<f8>"]),
    (b"\x1b[24~", &["<f12>"]),
];

/// xterm, and the terminals that follow its encoding.
const XTERM_STYLE: &[(&[u8], &[&str])] = &[
    (b"\x1b[A", &["<up>"]),
    (b"\x1b[B", &["<down>"]),
    (b"\x1b[C", &["<right>"]),
    (b"\x1b[D", &["<left>"]),
    (b"\x1bOA", &["<up>"]),
    (b"\x1b[H", &["<home>"]),
    (b"\x1b[F", &["<end>"]),
    (b"\x1bOP", &["<f1>"]),
    (b"\x1bOS", &["<f4>"]),
    (b"\x1b[15~", &["<f5>"]),
    (b"\x1b[1;5C", &["C-<right>"]),
    (b"\x1b[1;3D", &["M-<left>"]),
    (b"\x1b[1;2A", &["S-<up>"]),
    (b"\x1b[1;5H", &["C-<home>"]),
    (b"\x1b[1;5F", &["C-<end>"]),
    (b"\x1b[1;5P", &["C-<f1>"]),
    (b"\x1b[5;5~", &["C-<prior>"]),
];

/// `modifyOtherKeys` level 1 reports.
const MODIFY_OTHER_KEYS: &[(&[u8], &[&str])] = &[
    (b"\x1b[27;6;37~", &["C-%"]),
    (b"\x1b[27;8;37~", &["C-M-%"]),
    (b"\x1b[27;5;47~", &["C-/"]),
    (b"\x1b[27;5;59~", &["C-;"]),
    (b"\x1b[27;6;65~", &["C-S-a"]),
    (b"\x1b[27;5;13~", &["C-RET"]),
    (b"\x1b[27;6;95~", &["C-_"]),
];

#[test]
fn xterm() {
    check("xterm", COMMON);
    check("xterm", XTERM_STYLE);
    check("xterm", MODIFY_OTHER_KEYS);
}

#[test]
fn gnome_terminal() {
    // VTE: xterm's encoding without modifyOtherKeys; Ctrl+Backspace is ^H.
    check("GNOME Terminal", COMMON);
    check("GNOME Terminal", XTERM_STYLE);
    check("GNOME Terminal", &[(b"\x08", &["C-h"])]);
}

#[test]
fn alacritty() {
    check("Alacritty", COMMON);
    check("Alacritty", XTERM_STYLE);
}

#[test]
fn linux_console() {
    check("Linux console", COMMON);
    check(
        "Linux console",
        &[
            (b"\x1b[A", &["<up>"]),
            (b"\x1b[D", &["<left>"]),
            (b"\x1b[1~", &["<home>"]),
            (b"\x1b[4~", &["<end>"]),
            (b"\x1b[[A", &["<f1>"]),
            (b"\x1b[[B", &["<f2>"]),
            (b"\x1b[[E", &["<f5>"]),
            (b"\x1b[17~", &["<f6>"]),
            (b"\x1b[[Ax", &["<f1>", "x"]),
        ],
    );
}

#[test]
fn tmux() {
    check("tmux", COMMON);
    check("tmux", XTERM_STYLE);
    check("tmux", MODIFY_OTHER_KEYS);
    check(
        "tmux",
        &[(b"\x1b[1~", &["<home>"]), (b"\x1b[4~", &["<end>"])],
    );
}

#[test]
fn screen() {
    check("screen", COMMON);
    check(
        "screen",
        &[
            (b"\x1bOA", &["<up>"]),
            (b"\x1bOD", &["<left>"]),
            (b"\x1b[A", &["<up>"]),
            (b"\x1b[1~", &["<home>"]),
            (b"\x1b[4~", &["<end>"]),
            (b"\x1bOP", &["<f1>"]),
        ],
    );
}

#[test]
fn a_held_escape_waits_for_the_rest_of_its_sequence() {
    let mut decoder = Decoder::new(Protocol::Legacy);
    assert_eq!(decoder.feed(b"\x1b"), vec![]);
    assert!(decoder.waiting_for_escape());
    assert_eq!(decoder.feed(b"["), vec![]);
    assert!(decoder.waiting_for_escape());
    assert_eq!(decoder.feed(b"A"), vec![key("<up>")]);
    assert!(!decoder.waiting_for_escape());
    assert_eq!(decoder.timeout(), vec![], "nothing is held");
}

#[test]
fn a_timeout_makes_a_held_escape_the_escape_key() {
    let mut decoder = Decoder::new(Protocol::Legacy);
    assert_eq!(decoder.feed(b"\x1b"), vec![]);
    assert_eq!(decoder.timeout(), vec![key("ESC")]);
    assert_eq!(decoder.feed(b"[A"), vec![key("["), key("A")]);

    // An unfinished sequence typed by hand is its keys.
    let mut decoder = Decoder::new(Protocol::Legacy);
    assert_eq!(decoder.feed(b"\x1b[1;"), vec![]);
    assert_eq!(
        decoder.timeout(),
        vec![key("ESC"), key("["), key("1"), key(";")]
    );
    assert!(!decoder.waiting_for_escape());
}

#[test]
fn an_escape_before_a_byte_no_sequence_takes_is_the_escape_key() {
    check(
        "any",
        &[
            (b"\x1b[\x01", &["ESC", "[", "C-a"]),
            (b"\x1bO\x01", &["ESC", "O", "C-a"]),
            (b"\x1b[[\x01", &["ESC", "[", "[", "C-a"]),
        ],
    );
}

#[test]
fn unrecognized_complete_sequences_are_unknown() {
    let mut decoder = Decoder::new(Protocol::Legacy);
    assert_eq!(
        decoder.feed(b"\x1b[I\x1b[5555~x"),
        vec![
            InputEvent::Unknown(b"\x1b[I".to_vec()),
            InputEvent::Unknown(b"\x1b[5555~".to_vec()),
            key("x"),
        ]
    );
}

#[test]
fn kitty_mode_never_waits_on_escape() {
    let mut decoder = Decoder::new(Protocol::Kitty);
    assert_eq!(decoder.feed(b"\x1b"), vec![]);
    assert!(!decoder.waiting_for_escape());
    assert_eq!(decoder.timeout(), vec![]);
    assert_eq!(decoder.feed(b"[27u"), vec![key("ESC")]);
}

#[test]
fn bracketed_paste_in_legacy_mode() {
    let mut decoder = Decoder::new(Protocol::Legacy);
    assert_eq!(
        decoder.feed(b"\x1b[200~a\x1bb\x1b[201~c"),
        vec![
            InputEvent::Paste {
                data: b"a\x1bb".to_vec(),
                last: true
            },
            key("c"),
        ]
    );
    assert!(!decoder.waiting_for_escape());
}

fn mouse(kind: MouseKind, button: MouseButton, col: u16, row: u16, mods: Mods) -> InputEvent {
    InputEvent::Mouse(MouseEvent {
        kind,
        button,
        col,
        row,
        mods,
        clicks: 1,
    })
}

#[test]
fn sgr_mouse_reports_in_both_protocols() {
    let none = Mods::default();
    let ctrl = Mods {
        ctrl: true,
        ..Mods::default()
    };
    let cases: &[(&[u8], InputEvent)] = &[
        (
            b"\x1b[<0;10;5M",
            mouse(MouseKind::Press, MouseButton::Left, 9, 4, none),
        ),
        (
            b"\x1b[<0;10;5m",
            mouse(MouseKind::Release, MouseButton::Left, 9, 4, none),
        ),
        (
            b"\x1b[<32;11;5M",
            mouse(MouseKind::Drag, MouseButton::Left, 10, 4, none),
        ),
        (
            b"\x1b[<35;3;3M",
            mouse(MouseKind::Move, MouseButton::None, 2, 2, none),
        ),
        (
            b"\x1b[<1;1;1M",
            mouse(MouseKind::Press, MouseButton::Middle, 0, 0, none),
        ),
        (
            b"\x1b[<2;1;1M",
            mouse(MouseKind::Press, MouseButton::Right, 0, 0, none),
        ),
        (
            b"\x1b[<64;7;2M",
            mouse(MouseKind::Press, MouseButton::WheelUp, 6, 1, none),
        ),
        (
            b"\x1b[<65;7;2M",
            mouse(MouseKind::Press, MouseButton::WheelDown, 6, 1, none),
        ),
        (
            b"\x1b[<16;1;1M",
            mouse(MouseKind::Press, MouseButton::Left, 0, 0, ctrl),
        ),
        (
            b"\x1b[<0;300;200M",
            mouse(MouseKind::Press, MouseButton::Left, 299, 199, none),
        ),
    ];
    for protocol in [Protocol::Kitty, Protocol::Legacy] {
        for (bytes, event) in cases {
            let mut decoder = Decoder::new(protocol);
            assert_eq!(
                decoder.feed(bytes),
                vec![event.clone()],
                "{protocol:?}: {:?}",
                String::from_utf8_lossy(bytes)
            );
        }
        for bad in [
            &b"\x1b[<0;0;1M"[..],
            b"\x1b[<0;1;0M",
            b"\x1b[<128;1;1M",
            b"\x1b[<0;1M",
            b"\x1b[<0;1;1x",
            b"\x1b[<0;99999;1M",
        ] {
            let mut decoder = Decoder::new(protocol);
            assert_eq!(
                decoder.feed(bad),
                vec![InputEvent::Unknown(bad.to_vec())],
                "{protocol:?}: {:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }
}

proptest! {
    /// Decoding does not depend on read boundaries, and a timeout that
    /// comes only when nothing is buffered changes nothing.
    #[test]
    fn legacy_decoding_ignores_read_boundaries(
        input in prop::collection::vec(
            prop_oneof![
                4 => prop::sample::select(vec![0x1b, b'[', b'O', b'1', b';', b'5', b'A', b'~', b'x', b'<', b'M', 0x01, 0x7f, 0xc3, 0xa9]),
                1 => any::<u8>(),
            ],
            0..200,
        ),
        chunk in 1usize..20,
    ) {
        let mut whole = Decoder::new(Protocol::Legacy);
        let mut expected = whole.feed(&input);
        expected.extend(whole.timeout());
        let mut pieces = Decoder::new(Protocol::Legacy);
        let mut events = Vec::new();
        for piece in input.chunks(chunk) {
            events.extend(pieces.feed(piece));
        }
        events.extend(pieces.timeout());
        prop_assert_eq!(events, expected);
    }

    /// Timeouts at any point never lose or invent bytes: every key the
    /// decoder reports came from input, and nothing stays held.
    #[test]
    fn legacy_timeouts_keep_every_byte(
        input in prop::collection::vec(
            prop_oneof![
                4 => prop::sample::select(vec![0x1b, b'[', b'O', b'1', b';', b'A', b'~', b'x']),
                1 => any::<u8>(),
            ],
            0..200,
        ),
        chunk in 1usize..20,
    ) {
        let mut decoder = Decoder::new(Protocol::Legacy);
        let mut keys = 0usize;
        for piece in input.chunks(chunk) {
            keys += decoder.feed(piece).len();
            keys += decoder.timeout().len();
            prop_assert!(!decoder.waiting_for_escape());
        }
        prop_assert!(keys <= input.len());
        prop_assert!(decoder.buffered_len() < MAX_PENDING_SEQUENCE);
    }
}

#[test]
fn ctrl_letters_have_no_shift_and_meta_keys_fold_in_the_keymap() {
    // The decoder reports ESC then the key; this pins the canonical forms
    // the keymap's folding relies on.
    let events = decode(b"\x1bf");
    assert_eq!(events, vec![key("ESC"), key("f")]);
    let InputEvent::Key(Key { mods, .. }) = &events[1] else {
        panic!("a key");
    };
    assert_eq!(*mods, Mods::default());
}
