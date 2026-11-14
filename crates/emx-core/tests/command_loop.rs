//! Expected values come from GNU Emacs 29.3 running the same key sequences
//! (`emacs --batch -Q`, `execute-kbd-macro`), converted to 0-based offsets.

use std::collections::BTreeMap;

use emx_core::testing::Harness;
use emx_core::{
    BufferId, Change, ChangeRecorder, Config, ConfigError, Editor, Keymap, UndoOutcome,
};
use emx_proto::ServerMsg;
use emx_types::{Key, KeyCode, Mods};

struct Discard;

impl ChangeRecorder for Discard {
    fn record(&mut self, _buffer: BufferId, _change: &Change) {}
}

fn bells(harness: &mut Harness) -> usize {
    harness
        .take_sent()
        .into_iter()
        .filter(|message| *message == ServerMsg::Bell)
        .count()
}

fn with_keys(bindings: &[(&str, &str)]) -> Config {
    Config {
        keys: bindings
            .iter()
            .map(|(keys, command)| (keys.to_string(), command.to_string()))
            .collect(),
        ..Config::default()
    }
}

fn undo_once(harness: &mut Harness) -> UndoOutcome {
    harness
        .buffer_mut()
        .undo(1, false, &mut Discard)
        .expect("undo succeeds")
}

#[test]
fn harness_starts_with_empty_scratch() {
    let harness = Harness::new();
    assert_eq!(harness.text(), "");
    assert_eq!(harness.point(), 0);
    assert_eq!(harness.mark(), None);
    assert_eq!(harness.buffer().name, "*scratch*");
    assert_eq!(harness.echo(), "");
    assert!(harness.kill_ring().is_empty());
}

#[test]
fn with_text_places_point_and_mark() {
    let harness = Harness::with_text("ab|c^d");
    assert_eq!(harness.text(), "abcd");
    assert_eq!(harness.point(), 2);
    assert_eq!(harness.mark(), Some(3));
    let harness = Harness::with_text("^ab|cd");
    assert_eq!(harness.point(), 2);
    assert_eq!(harness.mark(), Some(0));
    assert!(!harness.buffer().is_modified());
}

#[test]
fn prefix_arg_forms() {
    let cases: [(&str, &str, &str, u64); 18] = [
        ("|", "C-u C-u a", "aaaaaaaaaaaaaaaa", 16),
        ("|", "M-5 a", "aaaaa", 5),
        ("ab|c", "C-u - C-f", "abc", 1),
        ("|", "C-u 5 C-u 1", "11111", 5),
        ("|", "C-u 1 2 a", "aaaaaaaaaaaa", 12),
        ("|", "C-5 a", "aaaaa", 5),
        ("|", "C-M-3 a", "aaa", 3),
        ("ab|c", "C-- C-f", "abc", 1),
        ("abcde|f", "M-- 3 C-f", "abcdef", 2),
        ("abcde|f", "C-u - 3 C-f", "abcdef", 2),
        ("|abc", "C-u - - C-f", "abc", 1),
        ("|", "C-u 3 - a", "---a", 4),
        ("ab|c", "C-u - 0 1 C-f", "abc", 1),
        ("|", "ESC 3 a", "aaa", 3),
        ("|", "C-u 0 a", "", 0),
        ("|", "C-u a", "aaaa", 4),
        ("|", "C-u C-u C-u x", &"x".repeat(64), 64),
        ("|abcdef", "C-u C-f", "abcdef", 4),
    ];
    for (initial, keys, text, point) in cases {
        let mut harness = Harness::with_text(initial);
        harness.keys(keys);
        assert_eq!(harness.text(), text, "{keys}");
        assert_eq!(harness.point(), point, "{keys}");
    }
    let mut harness = Harness::new();
    harness.keys("M-5 1 a");
    assert_eq!(harness.text(), "a".repeat(51));
}

#[test]
fn negative_self_insert_errors() {
    let mut harness = Harness::new();
    harness.keys("C-u - a");
    assert_eq!(harness.text(), "");
    assert_eq!(harness.echo(), "Negative repetition argument -1");
    assert_eq!(bells(&mut harness), 1);
}

#[test]
fn unbound_key_message() {
    let mut harness = Harness::new();
    harness.keys("C-c z");
    assert_eq!(harness.echo(), "C-c z is undefined");
    assert_eq!(bells(&mut harness), 1);
    harness.keys("a");
    assert_eq!(harness.echo(), "", "a key clears the echo area");
    assert_eq!(harness.text(), "a");
}

#[test]
fn undefined_key_consumes_prefix_arg_and_excludes_it_from_message() {
    let mut harness = Harness::new();
    harness.keys("C-u C-c z");
    assert_eq!(harness.echo(), "C-c z is undefined");
    harness.keys("a");
    assert_eq!(harness.text(), "a");
}

#[test]
fn undefined_messages_describe_keys_like_emacs() {
    for (keys, message) in [
        ("M-z", "M-z is undefined"),
        ("ESC z", "M-z is undefined"),
        ("C-x C-c", "C-x C-c is undefined"),
        ("C-c Z", "C-c Z is undefined"),
        ("C-S-z", "C-S-z is undefined"),
        ("<f5>", "<f5> is undefined"),
    ] {
        let mut harness = Harness::new();
        harness.keys(keys);
        assert_eq!(harness.echo(), message, "{keys}");
    }
}

#[test]
fn shift_translation_falls_back_to_unshifted_binding() {
    let config = with_keys(&[("C-c a", "forward-char"), ("M-f", "forward-char")]);
    let mut harness = Harness::with_config(config);
    harness.set_text("|abc");
    harness.keys("C-c A");
    assert_eq!(harness.point(), 1);
    harness.keys("M-F");
    assert_eq!(harness.point(), 2);
    harness.keys("C-S-f");
    assert_eq!(harness.point(), 3);
}

#[test]
fn override_rebinds() {
    let mut harness = Harness::with_config(with_keys(&[("C-c t", "forward-char")]));
    harness.set_text("|abc");
    harness.keys("C-c t");
    assert_eq!(harness.point(), 1);
    assert_eq!(harness.echo(), "");
}

#[test]
fn override_can_replace_a_default_and_bind_through_escape() {
    let config = with_keys(&[("C-f", "backward-char"), ("ESC t", "forward-char")]);
    let mut harness = Harness::with_config(config);
    harness.set_text("a|bc");
    harness.keys("C-f");
    assert_eq!(harness.point(), 0);
    harness.keys("M-t");
    assert_eq!(harness.point(), 1);
}

#[test]
fn unknown_command_override_errors() {
    let mut keymap = Keymap::emacs_default();
    let overrides = BTreeMap::from([("C-c t".to_string(), "no-such-command".to_string())]);
    let error = keymap
        .apply_overrides(&overrides)
        .expect_err("unknown command rejected");
    assert!(
        matches!(&error, ConfigError::UnknownCommand { keys, command }
            if keys == "C-c t" && command == "no-such-command"),
        "{error}"
    );
    assert!(Editor::new(with_keys(&[("C-c t", "no-such-command")])).is_err());
    let invalid = with_keys(&[("C-c t", "no-such-command")])
        .validate()
        .expect_err("validate checks keys");
    assert_eq!(invalid.key, "keys");
    assert!(invalid.reason.contains("no-such-command"), "{invalid:?}");
}

#[test]
fn override_errors_name_bad_keys_and_non_prefix_keys() {
    let mut keymap = Keymap::emacs_default();
    let bad_key = BTreeMap::from([("C-".to_string(), "forward-char".to_string())]);
    assert!(matches!(
        keymap.apply_overrides(&bad_key),
        Err(ConfigError::InvalidKey { .. })
    ));
    let through_command = BTreeMap::from([("C-f x".to_string(), "forward-char".to_string())]);
    let error = keymap
        .apply_overrides(&through_command)
        .expect_err("C-f is not a prefix");
    assert_eq!(
        error.to_string(),
        "Key sequence C-f x starts with non-prefix key C-f"
    );
}

#[test]
fn keyboard_quit_cancels_prefix() {
    let mut harness = Harness::new();
    harness.keys("C-u C-g");
    assert_eq!(harness.echo(), "Quit");
    assert_eq!(bells(&mut harness), 1);
    harness.keys("a");
    assert_eq!(harness.text(), "a");
}

#[test]
fn quit_inside_a_key_sequence_discards_it() {
    let mut harness = Harness::new();
    harness.keys("C-x C-g");
    assert_eq!(harness.echo(), "Quit");
    assert_eq!(bells(&mut harness), 1);
    harness.keys("a");
    assert_eq!(harness.text(), "a");
    harness.keys("C-u C-c C-g a");
    assert_eq!(harness.text(), "aa");
}

#[test]
fn keyboard_quit_deactivates_the_mark() {
    let mut harness = Harness::with_text("a|b^c");
    harness.buffer_mut().mark_active = true;
    harness.keys("C-g");
    assert!(!harness.buffer().mark_active);
    assert_eq!(harness.mark(), Some(2));
}

#[test]
fn motion_errors_at_buffer_edges() {
    let mut harness = Harness::with_text("|abc");
    harness.keys("C-b");
    assert_eq!(harness.echo(), "Beginning of buffer");
    assert_eq!(bells(&mut harness), 1);
    harness.keys("C-u 1 0 C-f");
    assert_eq!(harness.point(), 3, "moves to the end, then signals");
    assert_eq!(harness.echo(), "End of buffer");
    harness.keys("<left> <left>");
    assert_eq!(harness.point(), 1);
    harness.keys("<right>");
    assert_eq!(harness.point(), 2);
}

#[test]
fn motion_steps_over_whole_characters_and_graphemes() {
    let mut harness = Harness::with_text("|\u{e9}e\u{301}x\u{ff}");
    harness.keys("C-f");
    assert_eq!(harness.point(), 2);
    harness.keys("C-f");
    assert_eq!(harness.point(), 5);
    harness.keys("C-f C-f");
    assert_eq!(harness.point(), 8);
    harness.keys("C-b");
    assert_eq!(harness.point(), 6);
    harness.keys("C-b");
    assert_eq!(harness.point(), 5);
    harness.keys("C-b");
    assert_eq!(harness.point(), 2);
    harness.keys("C-b");
    assert_eq!(harness.point(), 0);
}

#[test]
fn self_insert_inserts_multibyte_characters() {
    let mut harness = Harness::new();
    harness.keys("\u{e9} \u{3bb}");
    assert_eq!(harness.text(), "\u{e9}\u{3bb}");
    assert_eq!(harness.point(), 4);
}

#[test]
fn self_insert_amalgamates_like_emacs() {
    // a a b C-/ => ""
    let mut harness = Harness::new();
    harness.keys("a a b");
    undo_once(&mut harness);
    assert_eq!(harness.text(), "");
    // a M-1 b C-/ => "": prefix commands keep last-command.
    let mut harness = Harness::new();
    harness.keys("a M-1 b");
    undo_once(&mut harness);
    assert_eq!(harness.text(), "");
    // a C-u 2 b C-/ => "a": a repeat count of 2 or more does not amalgamate.
    let mut harness = Harness::new();
    harness.keys("a C-u 2 b");
    undo_once(&mut harness);
    assert_eq!(harness.text(), "a");
    // a M-1 C-u b C-/ => "" and a C-u 1 C-u b C-/ => "": C-u after digits
    // leaves the argument set, so last-command stays self-insert-command.
    for keys in ["a M-1 C-u b", "a C-u 1 C-u b"] {
        let mut harness = Harness::new();
        harness.keys(keys);
        assert_eq!(harness.text(), "ab", "{keys}");
        undo_once(&mut harness);
        assert_eq!(harness.text(), "", "{keys}");
    }
    // a C-f b C-/ => "a": another command breaks the run.
    let mut harness = Harness::new();
    harness.keys("a C-b C-f b");
    undo_once(&mut harness);
    assert_eq!(harness.text(), "a");
}

#[test]
fn amalgamation_limit_applies_through_the_command_loop() {
    // 25 x then C-/ leaves 21.
    let mut harness = Harness::new();
    harness.keys(&["x"; 25].join(" "));
    let outcome = undo_once(&mut harness);
    assert_eq!(outcome.message, "Undo");
    assert_eq!(harness.text(), "x".repeat(21));
}

#[test]
fn paste_is_one_undoable_change_without_key_lookup() {
    let mut harness = Harness::with_text("a|");
    harness.keys("b");
    harness.paste(b"C-f\nxy");
    assert_eq!(harness.text(), "abC-f\nxy");
    assert_eq!(harness.point(), 8);
    undo_once(&mut harness);
    assert_eq!(harness.text(), "ab", "the paste is undone as one change");

    let mut harness = Harness::with_text("|");
    harness.paste(b"xy");
    harness.keys("c");
    undo_once(&mut harness);
    assert_eq!(
        harness.text(),
        "xy",
        "typing after a paste does not amalgamate"
    );
}

#[test]
fn paste_inside_a_key_sequence_is_undefined() {
    let mut harness = Harness::new();
    harness.keys("C-x");
    harness.paste(b"text");
    assert_eq!(harness.echo(), "C-x <xterm-paste> is undefined");
    assert_eq!(bells(&mut harness), 1);
    assert_eq!(harness.text(), "");
}

#[test]
fn raw_keys_are_canonicalized() {
    let mut harness = Harness::with_config(with_keys(&[("TAB", "forward-char")]));
    harness.set_text("|ab");
    harness.key(Key {
        code: KeyCode::Char('i'),
        mods: Mods {
            ctrl: true,
            ..Mods::default()
        },
    });
    assert_eq!(harness.point(), 1);
    harness.key(Key {
        code: KeyCode::Char('A'),
        mods: Mods {
            shift: true,
            ..Mods::default()
        },
    });
    assert_eq!(harness.text(), "aAb");
}

#[test]
fn prefix_keys_echo_after_echo_keystrokes() {
    let mut harness = Harness::new();
    harness.keys("C-x");
    assert_eq!(harness.echo(), "", "nothing until the timer fires");
    harness.fire_timers();
    assert_eq!(harness.echo(), "C-x-");
    harness.keys("C-g");
    assert_eq!(harness.echo(), "Quit");

    harness.keys("C-u");
    harness.fire_timers();
    assert_eq!(harness.echo(), "C-u-");
    harness.keys("C-u");
    assert_eq!(harness.echo(), "C-u C-u-", "echoing continues immediately");
    harness.keys("C-c");
    assert_eq!(harness.echo(), "C-u C-u C-c-");
    harness.keys("C-g");

    harness.keys("M-5");
    harness.fire_timers();
    assert_eq!(harness.echo(), "C-u 5-");
    harness.keys("-");
    assert_eq!(harness.text(), "-----");
    assert_eq!(harness.echo(), "");
}

#[test]
fn stale_echo_timer_does_nothing() {
    let mut harness = Harness::new();
    harness.keys("C-x C-g a");
    harness.fire_timers();
    assert_eq!(harness.echo(), "");
    harness.keys("C-u");
    harness.keys("C-u");
    harness.fire_timers();
    assert_eq!(harness.echo(), "C-u C-u-");
}

#[test]
fn zero_echo_keystrokes_disables_echo() {
    let config = Config {
        echo_keystrokes_ms: 0,
        ..Config::default()
    };
    let mut harness = Harness::with_config(config);
    harness.keys("C-x");
    harness.fire_timers();
    assert_eq!(harness.echo(), "");
}

#[test]
fn absurd_repeat_counts_error_without_allocating() {
    for keys in [
        "C-u 1 0 0 0 0 0 0 0 0 0 0 a",
        &["C-u"; 32].join(" "),
        "C-u 9 9 9 9 9 9 9 9 9 9 9 9 9 9 9 9 9 9 9 9 9 9 \u{3bb}",
    ] {
        let mut harness = Harness::with_text("x|");
        harness.keys(keys);
        if !keys.ends_with("a") && !keys.ends_with("\u{3bb}") {
            harness.keys("a");
        }
        assert_eq!(harness.text(), "x", "{keys}");
        assert_eq!(harness.point(), 1, "{keys}");
        assert!(
            harness.echo().contains("too large"),
            "{keys}: {}",
            harness.echo()
        );
        assert_eq!(bells(&mut harness), 1, "{keys}");
    }
}

#[test]
fn numeric_argument_kept_by_c_u_still_echoes() {
    let mut harness = Harness::new();
    harness.keys("C-u 5 C-u");
    harness.fire_timers();
    assert_eq!(harness.echo(), "C-u 5-");
    harness.keys("1");
    assert_eq!(harness.text(), "11111");
}

#[test]
fn read_only_buffer_signals_with_emacs_message() {
    let mut harness = Harness::with_text("|");
    harness.buffer_mut().read_only = true;
    harness.keys("a");
    assert_eq!(harness.echo(), "Buffer is read-only: #<buffer *scratch*>");
    assert_eq!(bells(&mut harness), 1);
    assert_eq!(harness.text(), "");
}
