//! Keyboard-macro recording and replay. The differential harness cannot
//! cover replay: GNU Emacs disables macro definition while it runs the case
//! through `execute-kbd-macro`, so `C-x e` there finds no macro. These tests
//! drive emx directly, where the keys are real input, and assert the Emacs
//! semantics of define-and-call and the `e` repeat.

use emx_core::testing::Harness;

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
fn redefining_replaces_the_previous_macro() {
    let mut harness = Harness::with_text("|abc");
    harness.keys("C-x ( X C-x )");
    harness.keys("C-x ( Y C-x )");
    harness.keys("C-x e");
    // Only the second definition (`Y`) is the current macro; it replays once.
    assert_eq!(harness.text(), "XYYabc");
}
