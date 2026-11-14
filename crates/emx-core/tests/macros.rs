//! Keyboard-macro recording and replay. The differential harness cannot
//! cover replay: GNU Emacs disables macro definition while it runs the case
//! through `execute-kbd-macro`, so `C-x e` there finds no macro. These tests
//! drive emx directly, where the keys are real input, and assert the Emacs
//! semantics of define-and-call and the `e` repeat.

use emx_core::testing::Harness;
use emx_proto::ServerMsg;

/// How many terminal bells the client was sent since the last check.
fn bells(harness: &mut Harness) -> usize {
    harness
        .take_sent()
        .into_iter()
        .filter(|message| matches!(message, ServerMsg::Bell))
        .count()
}

#[test]
fn define_and_call_inserts_again() {
    let mut harness = Harness::with_text("|abc");
    harness.keys("C-x ( X C-x ) C-x e");
    // The `X` typed while defining inserts once; `C-x e` replays it once.
    assert_eq!(harness.text(), "XXabc");
    assert_eq!(harness.point(), 2);
    assert!(!harness.signaled());
}

#[test]
fn bare_e_repeats_the_macro() {
    let mut harness = Harness::with_text("|abc");
    harness.keys("C-x ( X C-x ) C-x e e e");
    // Define (1) + call (1) + two repeats = four `X`.
    assert_eq!(harness.text(), "XXXXabc");
    assert_eq!(harness.point(), 4);
}

#[test]
fn a_non_repeat_key_ends_the_transient_map() {
    let mut harness = Harness::with_text("|abc");
    harness.keys("C-x ( X C-x ) C-x e");
    // A key other than `e` is no longer the repeat key; it self-inserts.
    harness.keys("z e");
    assert_eq!(harness.text(), "XXzeabc");
}

#[test]
fn prefix_argument_repeats_the_macro() {
    let mut harness = Harness::with_text("|abcdef");
    harness.keys("C-x ( C-d C-x ) C-u 3 C-x e");
    // Define deletes one char, then the macro runs three more times.
    assert_eq!(harness.text(), "ef");
    assert_eq!(harness.point(), 0);
}

#[test]
fn macro_replays_motion_and_insertion() {
    let mut harness = Harness::with_text("|a.b.c.");
    harness.keys("C-x ( C-f C-f X C-x )");
    assert_eq!(harness.text(), "a.Xb.c.");
    harness.keys("C-x e");
    assert_eq!(harness.text(), "a.Xb.Xc.");
    harness.keys("e");
    assert_eq!(harness.text(), "a.Xb.Xc.X");
}

#[test]
fn keyboard_quit_aborts_the_definition() {
    let mut harness = Harness::with_text("|abc");
    harness.keys("C-x ( X Y C-g");
    assert_eq!(harness.text(), "XYabc");
    assert!(harness.signaled());
    // The aborted macro left nothing to call.
    harness.keys("C-x e");
    assert!(harness.signaled());
    assert_eq!(harness.echo(), "No kbd macro has been defined");
    assert_eq!(harness.text(), "XYabc");
}

#[test]
fn call_without_definition_signals() {
    let mut harness = Harness::with_text("|abc");
    harness.keys("C-x e");
    assert!(harness.signaled());
    assert_eq!(harness.echo(), "No kbd macro has been defined");
}

#[test]
fn keyboard_quit_aborts_a_pending_multi_key_prefix() {
    // The brief requires C-g to abort EVERYTHING in progress, including a
    // half-entered multi-key sequence. Batch Emacs under execute-kbd-macro
    // cannot reproduce this (the `C-x C-g` difftest case was dropped), so it
    // is pinned here.
    let mut harness = Harness::with_text("|");
    harness.keys("C-x");
    harness.take_sent();
    harness.keys("C-g");
    // A quiet abort consistent with the other C-g facets: "Quit" is echoed
    // and the bell rings once.
    assert_eq!(harness.echo(), "Quit");
    assert!(harness.signaled());
    assert_eq!(bells(&mut harness), 1);
    // The pending `C-x` was discarded: the next key is read fresh and
    // self-inserts, rather than completing `C-x b` (switch-to-buffer).
    harness.keys("b");
    assert_eq!(harness.text(), "b");
    assert!(!harness.signaled());
}

#[test]
fn keyboard_quit_aborts_a_register_read() {
    // C-g during the register-name read of `C-x r s` aborts it, so the next
    // key is read fresh. (This abort was also dropped from difftest.)
    let mut harness = Harness::with_text("|abc");
    harness.keys("C-x r s");
    harness.take_sent();
    harness.keys("C-g");
    assert_eq!(harness.echo(), "Quit");
    assert!(harness.signaled());
    assert_eq!(bells(&mut harness), 1);
    // `x` is not consumed as a register name; it self-inserts.
    harness.keys("x");
    assert_eq!(harness.text(), "xabc");
    // Nothing was stored in register `a`: inserting it errors.
    harness.keys("C-x r i a");
    assert!(harness.signaled());
    assert_eq!(harness.echo(), "Register does not contain text");
}

#[test]
fn redefining_replaces_the_previous_macro() {
    let mut harness = Harness::with_text("|abc");
    harness.keys("C-x ( X C-x )");
    harness.keys("C-x ( Y C-x )");
    harness.keys("C-x e");
    // Only the second definition (`Y`) is the current macro; it replays once.
    assert_eq!(harness.text(), "XYYabc");
}
