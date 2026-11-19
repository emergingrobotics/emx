//! Kills reach the terminal's clipboard (OSC 52), DESIGN section 7.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use emx_core::testing::Harness;
use emx_core::{Config, Event};
use emx_proto::ServerMsg;
use emx_types::Face;

const COPIED: &str = "Copied to clipboard";

fn clipboard(sent: &[ServerMsg]) -> Vec<Vec<u8>> {
    sent.iter()
        .filter_map(|message| match message {
            ServerMsg::Clipboard { data_b64 } => Some(STANDARD.decode(data_b64).unwrap()),
            _ => None,
        })
        .collect()
}

#[test]
fn kills_and_copies_go_to_the_clipboard() {
    let mut harness = Harness::with_text("|one two\nthree");
    harness.take_sent();
    harness.keys("M-d");
    assert_eq!(clipboard(&harness.take_sent()), vec![b"one".to_vec()]);
    harness.keys("M-d");
    assert_eq!(
        clipboard(&harness.take_sent()),
        vec![b"one two".to_vec()],
        "an appended kill is sent whole"
    );
    harness.keys("C-n C-a C-SPC C-e M-w");
    assert_eq!(clipboard(&harness.take_sent()), vec![b"three".to_vec()]);
    harness.keys("C-y");
    assert!(
        clipboard(&harness.take_sent()).is_empty(),
        "a yank sends nothing"
    );
}

#[test]
fn the_clipboard_can_be_turned_off_and_large_kills_are_not_sent() {
    let mut harness = Harness::with_config(Config {
        osc52_clipboard: false,
        ..Config::default()
    });
    harness.set_text("|word");
    harness.keys("M-d");
    assert!(clipboard(&harness.take_sent()).is_empty());
    let mut harness = Harness::with_config(Config {
        osc52_max_bytes: 3,
        ..Config::default()
    });
    harness.set_text("|word");
    harness.keys("M-d");
    assert!(clipboard(&harness.take_sent()).is_empty());
}

/// The faces of the echo row's visible characters.
fn echo_faces(harness: &Harness) -> Vec<Face> {
    let grid = harness.grid();
    let row = grid.rows - 1;
    (0..grid.cols)
        .map(|col| &grid.cells[usize::from(row) * usize::from(grid.cols) + usize::from(col)])
        .filter(|cell| cell.text != " ")
        .map(|cell| cell.face)
        .collect()
}

#[test]
fn a_copy_flashes_a_notice_that_clears_itself() {
    let mut harness = Harness::with_text("|one two");
    harness.keys("C-SPC M-f M-w");
    assert_eq!(harness.echo_line(), COPIED);
    let faces = echo_faces(&harness);
    assert!(!faces.is_empty());
    assert!(faces.iter().all(|face| *face == Face::Success), "{faces:?}");
    harness.fire_timers();
    assert_eq!(harness.echo_line(), "", "the notice expires");
}

#[test]
fn a_key_wipes_the_notice_and_a_later_message_is_not_green() {
    let mut harness = Harness::with_text("|one two");
    harness.keys("C-SPC M-f M-w");
    harness.keys("C-f");
    assert_eq!(harness.echo_line(), "");
    harness.keys("C-SPC");
    assert_eq!(harness.echo_line(), "Mark set");
    assert!(
        echo_faces(&harness)
            .iter()
            .all(|face| *face == Face::Default),
        "an ordinary message keeps the default face"
    );
}

#[test]
fn a_newer_copy_keeps_its_notice_when_an_older_timer_fires() {
    let mut harness = Harness::with_text("|one two");
    harness.keys("C-SPC M-f M-w");
    let older = harness.take_timers();
    harness.keys("C-SPC M-f M-w");
    for id in older {
        harness.handle(Event::Timer(id));
    }
    assert_eq!(harness.echo_line(), COPIED);
}

#[test]
fn kills_send_to_the_clipboard_without_a_notice() {
    let mut harness = Harness::with_text("|one two");
    harness.keys("M-d");
    assert_eq!(clipboard(&harness.take_sent()), vec![b"one".to_vec()]);
    assert_eq!(harness.echo_line(), "");
}

#[test]
fn a_copy_too_large_for_the_clipboard_warns() {
    let mut harness = Harness::with_config(Config {
        osc52_max_bytes: 3,
        ..Config::default()
    });
    harness.set_text("|word");
    harness.keys("C-SPC M-f M-w");
    assert!(clipboard(&harness.take_sent()).is_empty());
    assert_eq!(
        harness.echo_line(),
        "Not copied to clipboard: 4 bytes is over osc52_max_bytes (3)"
    );
    assert!(
        echo_faces(&harness)
            .iter()
            .all(|face| *face == Face::Warning)
    );
    assert_eq!(harness.kill_ring(), vec!["word".to_string()]);
}

#[test]
fn no_notice_when_the_clipboard_is_off() {
    let mut harness = Harness::with_config(Config {
        osc52_clipboard: false,
        ..Config::default()
    });
    harness.set_text("|word");
    harness.keys("C-SPC M-f M-w");
    assert_eq!(harness.echo_line(), "");
}

#[test]
fn a_zero_notice_time_keeps_the_notice_until_the_next_key() {
    let mut harness = Harness::with_config(Config {
        clipboard_notice_ms: 0,
        ..Config::default()
    });
    harness.set_text("|word");
    harness.keys("C-SPC M-f M-w");
    harness.fire_timers();
    assert_eq!(harness.echo_line(), COPIED);
    harness.keys("C-a");
    assert_eq!(harness.echo_line(), "");
}
