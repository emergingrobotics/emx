//! `*Messages*`: every echo-area message is logged, as Emacs's
//! `message_dolog` does (`message-log-max` 1000, repeats counted as
//! ` [N times]`).

use emx_core::testing::Harness;

fn log(harness: &Harness) -> String {
    harness
        .buffer_text("*Messages*")
        .expect("*Messages* exists")
}

#[test]
fn a_fresh_session_has_an_empty_read_only_log() {
    let mut harness = Harness::new();
    assert_eq!(log(&harness), "");
    assert!(!harness.buffer_modified("*Messages*"));
    harness.keys("C-x b RET");
    assert_eq!(harness.buffer().name, "*Messages*");
    harness.keys("x");
    assert_eq!(harness.echo(), "Buffer is read-only: #<buffer *Messages*>");
}

#[test]
fn messages_and_errors_are_logged_and_repeats_counted() {
    let mut harness = Harness::with_text("|");
    harness.keys("C-x o C-x o C-x o C-f C-c z");
    assert_eq!(
        log(&harness),
        "No other window to select [3 times]\nEnd of buffer\nC-c z is undefined\n"
    );
    assert!(harness.buffer_modified("*Messages*"));
    // Keystroke echo is not a message.
    harness.keys("C-x");
    harness.fire_timers();
    assert_eq!(harness.echo(), "C-x-");
    assert!(!log(&harness).contains("C-x-"));
}

#[test]
fn the_log_keeps_message_log_max_lines() {
    let mut harness = Harness::with_text("|");
    for _ in 0..600 {
        harness.keys("C-x o C-f");
    }
    let text = log(&harness);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1000);
    assert_eq!(lines[999], "End of buffer");
    assert_eq!(lines[0], "No other window to select");
}

#[test]
fn a_window_at_the_end_of_the_log_follows_it() {
    let mut harness = Harness::with_text("|");
    harness.keys("C-x o C-x 2 C-x b RET");
    assert_eq!(harness.buffer().name, "*Messages*");
    assert_eq!(harness.point(), log(&harness).len() as u64);
    harness.keys("C-f");
    assert_eq!(harness.point(), log(&harness).len() as u64);
}

#[test]
fn the_log_is_never_journaled() {
    let mut harness = Harness::with_text("|");
    harness.keys("C-x o C-f a");
    let names: Vec<String> = harness
        .journal_ops()
        .into_iter()
        .filter_map(|op| match op {
            emx_core::journal::JournalOp::Open { header, .. } => Some(header.name),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec!["*scratch*".to_string()]);
}

#[test]
fn a_killed_log_is_made_again_by_the_next_message() {
    let mut harness = Harness::with_text("|");
    harness.keys("C-x o C-x k");
    harness.type_text("*Messages*");
    harness.keys("RET");
    assert!(harness.editor().find_buffer("*Messages*").is_none());
    harness.keys("C-x o");
    assert_eq!(log(&harness), "No other window to select\n");
}

#[test]
fn one_message_to_every_client_is_logged_once() {
    let mut harness = Harness::with_text("|");
    let other = harness.attach(emx_proto::ClientKind::Tui);
    let journal = harness.journal_ops().into_iter().find_map(|op| match op {
        emx_core::journal::JournalOp::Open { buffer, .. } => Some(buffer),
        _ => None,
    });
    // Edits open *scratch*'s journal; its failure is told to everyone.
    let journal = journal.unwrap_or_else(|| {
        harness.keys("a");
        harness
            .journal_ops()
            .into_iter()
            .find_map(|op| match op {
                emx_core::journal::JournalOp::Open { buffer, .. } => Some(buffer),
                _ => None,
            })
            .expect("an edit opens the journal")
    });
    harness.journal_write_failed(journal, "disk full");
    let message = harness.echo();
    assert_eq!(harness.echo_of(other), message);
    assert_eq!(log(&harness), format!("{message}\n"));
}
