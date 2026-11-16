//! Editing behavior the differential suite cannot check against batch
//! Emacs: messages (batch keyboard macros suppress "Mark set"), bracketed
//! paste, grapheme clusters (batch Emacs moves by code point), the
//! read-only kill path, and configuration.

use emx_core::testing::Harness;
use emx_core::{Config, Session};
use emx_proto::{ClientKind, ServerMsg};

fn with_config(config: Config, text: &str) -> Harness {
    let mut harness = Harness::with_config(config);
    harness.set_text(text);
    harness
}

#[test]
fn paste_is_a_yank_of_a_new_kill() {
    // Emacs xterm-paste with xterm-store-paste-on-kill-ring: kill-new, then
    // yank (push-mark, insert, this-command yank).
    let mut harness = Harness::with_text("ab|cd");
    harness.paste(b"XY");
    assert_eq!(harness.text(), "abXYcd");
    assert_eq!(harness.point(), 4);
    assert_eq!(harness.mark(), Some(2));
    assert!(!harness.mark_active());
    assert_eq!(harness.kill_ring(), ["XY"]);
    assert_eq!(harness.echo(), "Mark set");
}

#[test]
fn paste_undoes_as_one_change_and_yank_pop_replaces_it() {
    let mut harness = Harness::with_text("|one\ntwo");
    harness.keys("C-k");
    harness.paste(b"pasted");
    assert_eq!(harness.text(), "pasted\ntwo");
    harness.keys("M-y");
    assert_eq!(harness.text(), "one\ntwo", "M-y swaps in the previous kill");
    assert_eq!(harness.point(), 3);

    let mut harness = Harness::with_text("a|b");
    harness.paste(b"xyz");
    harness.keys("C-/");
    assert_eq!(harness.text(), "ab");
    assert_eq!(harness.point(), 1);
}

#[test]
fn mark_commands_report_like_emacs() {
    let mut harness = Harness::with_text("ab|c");
    harness.keys("C-SPC");
    assert_eq!(harness.echo(), "Mark set");
    harness.keys("C-SPC");
    assert_eq!(harness.echo(), "Mark deactivated");
    harness.keys("C-SPC");
    assert_eq!(harness.echo(), "Mark activated");
    harness.keys("C-u C-SPC");
    assert_eq!(harness.echo(), "Mark popped");
    harness.keys("M-<");
    assert_eq!(harness.echo(), "Mark set");
    assert_eq!(harness.mark(), Some(2));
    harness.keys("M->");
    assert_eq!(harness.echo(), "Mark set");
    assert_eq!(harness.mark(), Some(0));
}

#[test]
fn undo_and_redo_report_in_the_echo_area() {
    let mut harness = Harness::with_text("|");
    harness.keys("a C-/");
    assert_eq!(harness.echo(), "Undo");
    harness.keys("C-?");
    assert_eq!(harness.echo(), "Redo");
    assert_eq!(harness.text(), "a");
}

/// Emacs 28: `M-y` after a command other than a yank is
/// `yank-from-kill-ring`, which reads the kill to insert.
#[test]
fn yank_pop_after_other_commands_reads_from_the_kill_ring() {
    let mut harness = Harness::with_text("|ab");
    harness.keys("C-k C-y C-a");
    harness.keys("M-y");
    assert!(!harness.signaled());
    assert_eq!(harness.echo_line(), "Yank from kill-ring: ");
    harness.keys("RET");
    assert_eq!(harness.text(), "abab");
}

/// The yank's text is found by the client's own record, which is gone once
/// another client killed the buffer it was in: `M-y` cannot replace it.
#[test]
fn yank_pop_after_another_client_disturbed_the_yank_signals() {
    let mut harness = Harness::with_session(Config::default(), Session::Server);
    harness.set_text("|ab");
    let other = harness.attach(ClientKind::Tui);
    harness.keys("C-k C-y");
    harness.keys_as(other, "C-x k RET");
    harness.keys("M-y");
    assert!(harness.signaled());
    assert_eq!(harness.echo(), "Previous command was not a yank");
}

#[test]
fn kill_in_read_only_buffer_copies_and_signals() {
    // Emacs kill-region: copy-region-as-kill, then barf-if-buffer-read-only.
    let mut harness = Harness::with_text("|abc\ndef");
    harness.buffer_mut().read_only = true;
    harness.keys("C-k");
    assert_eq!(harness.text(), "abc\ndef");
    assert_eq!(harness.kill_ring(), ["abc"]);
    assert_eq!(harness.echo(), "Buffer is read-only: #<buffer *scratch*>");
    harness.keys("C-y");
    assert_eq!(harness.echo(), "Buffer is read-only: #<buffer *scratch*>");
}

#[test]
fn motion_steps_over_grapheme_clusters() {
    // "e" + COMBINING ACUTE ACCENT is one cluster of three bytes.
    let mut harness = Harness::with_text("|e\u{301}x");
    harness.keys("C-f");
    assert_eq!(harness.point(), 3);
    harness.keys("C-b");
    assert_eq!(harness.point(), 0);

    // Column 1 on the next line falls after the whole cluster.
    let mut harness = Harness::with_text("a|bc\ne\u{301}xy");
    harness.keys("C-n");
    assert_eq!(harness.point(), 7);
}

#[test]
fn word_motion_never_stops_inside_a_cluster() {
    // MAN ZWJ WOMAN: Emacs stops after MAN, since ZWJ is not a word
    // constituent; emx moves on to the end of the cluster.
    let family = "\u{1f468}\u{200d}\u{1f469}";
    let mut harness = Harness::with_text(&format!("|{family} x"));
    harness.keys("M-f");
    assert_eq!(harness.point(), family.len() as u64);
    harness.keys("C-e M-b M-b");
    assert_eq!(harness.point(), 0);

    let mut harness = Harness::with_text(&format!("|{family} x"));
    harness.keys("M-d");
    assert_eq!(harness.text(), " x");
    assert_eq!(harness.kill_ring(), [family]);
}

#[test]
fn deletion_removes_code_points_like_emacs() {
    // Emacs delete-char and delete-backward-char delete characters, not
    // compositions; point stays on a cluster boundary either way.
    let mut harness = Harness::with_text("e\u{301}|x");
    harness.keys("DEL");
    assert_eq!(harness.text(), "ex");
    assert_eq!(harness.point(), 1);
    let mut harness = Harness::with_text("|e\u{301}x");
    harness.keys("C-d");
    assert_eq!(harness.text(), "\u{301}x");
    assert_eq!(harness.point(), 0);
}

#[test]
fn mark_ring_is_bounded_by_config() {
    let config = Config {
        mark_ring_max: 2,
        ..Config::default()
    };
    let mut harness = with_config(config, "|abcdef");
    harness.keys("C-SPC C-f C-SPC C-f C-SPC C-f C-SPC C-f");
    assert_eq!(harness.buffer().mark_ring.len(), 2);
    assert_eq!(harness.mark(), Some(3));
    harness.keys("C-u C-SPC");
    assert_eq!((harness.point(), harness.mark()), (3, Some(2)));
    harness.keys("C-u C-SPC");
    assert_eq!((harness.point(), harness.mark()), (2, Some(1)));
    harness.keys("C-u C-SPC");
    assert_eq!(
        (harness.point(), harness.mark()),
        (1, Some(3)),
        "popping cycles through the ring"
    );
}

#[test]
fn newline_indents_with_spaces_without_indent_tabs_mode() {
    let config = Config {
        indent_tabs_mode: false,
        ..Config::default()
    };
    let mut harness = with_config(config, "          x|");
    harness.keys("RET");
    assert_eq!(harness.text(), "          x\n          ");
    assert_eq!(harness.point(), 22);
}

#[test]
fn arrow_and_home_keys_follow_emacs_bindings() {
    let mut harness = Harness::with_text("ab|c\ndef");
    harness.keys("<down>");
    assert_eq!(harness.point(), 6);
    harness.keys("<home>");
    assert_eq!(harness.point(), 4);
    harness.keys("<end>");
    assert_eq!(harness.point(), 7);
    harness.keys("<up>");
    assert_eq!(harness.point(), 3);
    harness.keys("C-<end>");
    assert_eq!(harness.point(), 7);
    harness.keys("C-<home>");
    assert_eq!(harness.point(), 0);
    harness.keys("C-@ C-f C-w");
    assert_eq!(harness.text(), "bc\ndef");
}

#[test]
fn delete_key_deletes_the_character_under_point() {
    let mut harness = Harness::with_text("a|bc");
    harness.keys("<delete>");
    assert_eq!(harness.text(), "ac");
    assert_eq!(harness.point(), 1);
    assert_eq!(harness.echo(), "");
}

#[test]
fn control_z_suspends_a_terminal_client() {
    let mut harness = Harness::with_text("a|bc");
    harness.take_sent();
    harness.keys("C-z");
    assert!(
        harness
            .take_sent()
            .iter()
            .any(|message| matches!(message, ServerMsg::Suspend))
    );
    assert_eq!(harness.text(), "abc");
}
