//! Mouse input with `xterm-mouse-mode` on, after Emacs's mouse.el and
//! mwheel.el in a terminal frame.

use emx_core::testing::Harness;
use emx_core::{Config, Event};
use emx_proto::{ClientKind, ServerMsg};
use emx_types::{Mods, MouseButton, MouseEvent, MouseKind};

fn mouse_harness(text: &str) -> Harness {
    let mut harness = Harness::with_config(Config {
        mouse: true,
        ..Config::default()
    });
    harness.set_text(text);
    harness
}

fn report(kind: MouseKind, button: MouseButton, col: u16, row: u16, clicks: u8) -> MouseEvent {
    MouseEvent {
        kind,
        button,
        col,
        row,
        mods: Mods::default(),
        clicks,
    }
}

fn send(harness: &mut Harness, event: MouseEvent) {
    let client = harness.client();
    harness.handle(Event::Mouse(client, event));
}

fn click(harness: &mut Harness, col: u16, row: u16) {
    clicks(harness, col, row, 1);
}

fn clicks(harness: &mut Harness, col: u16, row: u16, count: u8) {
    send(
        harness,
        report(MouseKind::Press, MouseButton::Left, col, row, count),
    );
    send(
        harness,
        report(MouseKind::Release, MouseButton::Left, col, row, count),
    );
}

fn region(harness: &Harness) -> Option<(u64, u64)> {
    let mark = harness.mark()?;
    harness
        .buffer()
        .mark_active
        .then(|| (mark.min(harness.point()), mark.max(harness.point())))
}

#[test]
fn a_click_moves_point_and_deactivates_the_mark() {
    let mut harness = mouse_harness("|one two\nthree\n");
    harness.keys("C-SPC C-f");
    assert!(region(&harness).is_some());
    click(&mut harness, 4, 0);
    assert_eq!(harness.point(), 4);
    assert_eq!(region(&harness), None);
    click(&mut harness, 2, 1);
    assert_eq!(harness.point(), 10);
}

#[test]
fn a_click_past_a_line_or_below_the_text_goes_to_its_end() {
    let mut harness = mouse_harness("|one\nthree\n");
    click(&mut harness, 40, 0);
    assert_eq!(harness.point(), 3, "end of the first line");
    click(&mut harness, 5, 10);
    assert_eq!(harness.point(), 10, "end of the buffer");
}

#[test]
fn a_click_on_the_echo_area_is_ignored() {
    let mut harness = mouse_harness("one|\n");
    click(&mut harness, 0, 23);
    assert_eq!(harness.point(), 3);
}

#[test]
fn reports_are_ignored_while_the_mode_is_off() {
    let mut harness = Harness::with_text("one|\n");
    click(&mut harness, 0, 0);
    assert_eq!(harness.point(), 3);
}

#[test]
fn dragging_sets_the_region() {
    let mut harness = mouse_harness("|one two three\n");
    send(
        &mut harness,
        report(MouseKind::Press, MouseButton::Left, 4, 0, 1),
    );
    send(
        &mut harness,
        report(MouseKind::Drag, MouseButton::Left, 7, 0, 1),
    );
    send(
        &mut harness,
        report(MouseKind::Release, MouseButton::Left, 7, 0, 1),
    );
    assert_eq!(region(&harness), Some((4, 7)));
    assert_eq!(harness.point(), 7);
    // Backward, and the pointer leaving the window keeps to its edge.
    send(
        &mut harness,
        report(MouseKind::Press, MouseButton::Left, 8, 0, 1),
    );
    send(
        &mut harness,
        report(MouseKind::Drag, MouseButton::Left, 2, 0, 1),
    );
    assert_eq!(region(&harness), Some((2, 8)));
    assert_eq!(harness.point(), 2);
}

#[test]
fn double_and_triple_clicks_select_a_word_and_a_line() {
    let mut harness = mouse_harness("|let foo_bar = 1;\nnext\n");
    clicks(&mut harness, 5, 0, 2);
    assert_eq!(region(&harness), Some((4, 7)));
    assert_eq!(harness.point(), 7);
    clicks(&mut harness, 5, 0, 3);
    assert_eq!(region(&harness), Some((0, 17)), "the line and its newline");
    clicks(&mut harness, 5, 0, 4);
    assert_eq!(region(&harness), None, "a fourth click is a single one");
    assert_eq!(harness.point(), 5);
}

#[test]
fn a_right_click_extends_the_region_from_point() {
    let mut harness = mouse_harness("one |two three\n");
    send(
        &mut harness,
        report(MouseKind::Press, MouseButton::Right, 9, 0, 1),
    );
    assert_eq!(region(&harness), Some((4, 9)));
    send(
        &mut harness,
        report(MouseKind::Press, MouseButton::Right, 12, 0, 1),
    );
    assert_eq!(region(&harness), Some((4, 12)), "an active region grows");
}

#[test]
fn a_click_selects_the_window_under_it() {
    let mut harness = mouse_harness("|abc\ndef\n");
    harness.keys("C-x 2");
    // The new window is below: rows 12 to 21, its modeline on 22.
    click(&mut harness, 1, 13);
    harness.type_text("X");
    let screen = harness.screen();
    assert_eq!(screen[1].trim_end(), "dXef");
    assert!(
        screen[22].starts_with("-UUU:**-  F1  *scratch*"),
        "{:?}",
        screen[22]
    );
}

#[test]
fn the_wheel_scrolls_the_window_under_the_pointer() {
    let mut text = String::from("|");
    for line in 0..100 {
        text.push_str(&format!("line {line}\n"));
    }
    let mut harness = mouse_harness(&text);
    harness.keys("C-x 2");
    send(
        &mut harness,
        report(MouseKind::Press, MouseButton::WheelDown, 0, 14, 1),
    );
    let screen = harness.screen();
    assert_eq!(screen[0].trim_end(), "line 0", "the selected window stays");
    assert_eq!(screen[12].trim_end(), "line 5", "the window below scrolled");
    harness.type_text("X");
    assert!(
        harness.screen()[0].starts_with("Xline 0"),
        "the selected window is still selected"
    );
    send(
        &mut harness,
        report(MouseKind::Press, MouseButton::WheelUp, 0, 14, 1),
    );
    send(
        &mut harness,
        report(MouseKind::Press, MouseButton::WheelUp, 0, 14, 1),
    );
    assert_eq!(
        harness.screen()[12].trim_end(),
        "Xline 0",
        "scrolling stops at the top"
    );
}

#[test]
fn dragging_a_modeline_or_border_resizes_windows() {
    let mut harness = mouse_harness("|abc\n");
    harness.keys("C-x 2");
    // The top window's modeline is row 11.
    send(
        &mut harness,
        report(MouseKind::Press, MouseButton::Left, 5, 11, 1),
    );
    send(
        &mut harness,
        report(MouseKind::Drag, MouseButton::Left, 5, 15, 1),
    );
    send(
        &mut harness,
        report(MouseKind::Release, MouseButton::Left, 5, 15, 1),
    );
    let screen = harness.screen();
    assert!(screen[15].starts_with("-UUU:"), "{:?}", screen[15]);
    // Dragging past the other window's minimum stops at it.
    send(
        &mut harness,
        report(MouseKind::Press, MouseButton::Left, 5, 15, 1),
    );
    send(
        &mut harness,
        report(MouseKind::Drag, MouseButton::Left, 5, 22, 1),
    );
    let screen = harness.screen();
    assert!(screen[18].starts_with("-UUU:"), "{:?}", screen[18]);

    let mut harness = mouse_harness("|abc\n");
    harness.keys("C-x 3");
    // The left window is 40 columns, its border at column 39.
    send(
        &mut harness,
        report(MouseKind::Press, MouseButton::Left, 39, 3, 1),
    );
    send(
        &mut harness,
        report(MouseKind::Drag, MouseButton::Left, 29, 3, 1),
    );
    assert_eq!(harness.grid().row(3)[29].text, "|");
    assert_eq!(harness.grid().row(3)[39].text, " ");
}

#[test]
fn xterm_mouse_mode_toggles_reporting_for_every_terminal() {
    let mut harness = Harness::new();
    let other = harness.attach(ClientKind::Tui);
    harness.take_sent();
    harness.keys("M-x");
    harness.type_text("xterm-mouse-mode");
    harness.keys("RET");
    assert!(harness.echo().contains("Xterm-Mouse mode enabled"));
    assert!(
        harness
            .take_sent()
            .contains(&ServerMsg::Modes { mouse: true })
    );
    assert!(
        harness
            .take_messages(other)
            .contains(&ServerMsg::Modes { mouse: true })
    );
    harness.keys("M-x");
    harness.type_text("xterm-mouse-mode");
    harness.keys("RET");
    assert!(harness.echo().contains("Xterm-Mouse mode disabled"));
}

#[test]
fn a_terminal_attaching_with_the_mode_on_is_told() {
    let mut harness = mouse_harness("|x");
    let other = harness.attach(ClientKind::Tui);
    assert!(
        harness
            .take_messages(other)
            .contains(&ServerMsg::Modes { mouse: true })
    );
}

fn drag_copy_harness(text: &str) -> Harness {
    let mut harness = Harness::with_config(Config {
        mouse: true,
        mouse_drag_copy_region: true,
        ..Config::default()
    });
    harness.set_text(text);
    harness
}

fn drag(harness: &mut Harness, from: u16, to: u16, row: u16) {
    send(
        harness,
        report(MouseKind::Press, MouseButton::Left, from, row, 1),
    );
    send(
        harness,
        report(MouseKind::Drag, MouseButton::Left, to, row, 1),
    );
    send(
        harness,
        report(MouseKind::Release, MouseButton::Left, to, row, 1),
    );
}

fn sent_to_clipboard(harness: &mut Harness) -> bool {
    harness
        .take_sent()
        .iter()
        .any(|message| matches!(message, ServerMsg::Clipboard { .. }))
}

#[test]
fn releasing_a_drag_copies_the_region_when_asked_to() {
    let mut harness = drag_copy_harness("|one two three\n");
    harness.take_sent();
    drag(&mut harness, 4, 7, 0);
    assert_eq!(harness.kill_ring(), vec!["two".to_string()]);
    assert!(sent_to_clipboard(&mut harness));
    assert_eq!(harness.echo_line(), "Copied to clipboard");
    assert_eq!(region(&harness), Some((4, 7)), "the region stays");
}

#[test]
fn a_double_click_copies_the_word_when_asked_to() {
    let mut harness = drag_copy_harness("|let foo_bar = 1;\n");
    clicks(&mut harness, 5, 0, 2);
    assert_eq!(harness.kill_ring(), vec!["foo".to_string()]);
}

#[test]
fn a_plain_click_copies_nothing() {
    let mut harness = drag_copy_harness("|one two three\n");
    harness.take_sent();
    click(&mut harness, 4, 0);
    assert!(harness.kill_ring().is_empty());
    assert!(!sent_to_clipboard(&mut harness));
    assert_eq!(harness.echo_line(), "");
}

#[test]
fn a_drag_copies_nothing_by_default() {
    let mut harness = mouse_harness("|one two three\n");
    drag(&mut harness, 4, 7, 0);
    assert_eq!(region(&harness), Some((4, 7)));
    assert!(harness.kill_ring().is_empty());
}

#[test]
fn successive_drag_copies_do_not_append() {
    let mut harness = drag_copy_harness("|one two three\n");
    drag(&mut harness, 0, 3, 0);
    drag(&mut harness, 4, 7, 0);
    assert_eq!(
        harness.kill_ring(),
        vec!["two".to_string(), "one".to_string()]
    );
}
