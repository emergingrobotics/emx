//! Kills reach the terminal's clipboard (OSC 52), DESIGN section 7.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use emx_core::Config;
use emx_core::testing::Harness;
use emx_proto::ServerMsg;

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
