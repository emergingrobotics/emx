//! The minibuffer's line editing, history and answer prompts, compared
//! with Emacs 29.3 `emacs -nw -Q`.

use std::fs;
use std::path::Path;

use emx_core::testing::Harness;

fn harness_in(directory: &Path) -> Harness {
    let mut harness = Harness::new();
    harness.set_default_directory(directory);
    harness
}

#[test]
fn find_file_prompt_edits_a_line() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut harness = harness_in(directory.path());
    let prefix = format!("Find file: {}/", directory.path().display());
    harness.keys("C-x C-f");
    assert_eq!(harness.echo_line(), prefix);
    let rows = harness.screen().len() as u16;
    assert_eq!(
        harness.cursor(),
        Some((prefix.chars().count() as u16, rows - 1)),
        "the cursor is in the minibuffer"
    );
    harness.type_text("abc");
    harness.keys("C-b C-b x");
    assert_eq!(harness.echo_line(), format!("{prefix}axbc"));
    harness.keys("C-e DEL");
    assert_eq!(harness.echo_line(), format!("{prefix}axb"));
    harness.keys("C-b C-b C-d");
    assert_eq!(harness.echo_line(), format!("{prefix}ab"));
    harness.keys("C-a C-k");
    assert_eq!(harness.echo_line(), "Find file: ");
    harness.keys("C-y C-y");
    let typed = format!("{}/ab", directory.path().display());
    assert_eq!(harness.echo_line(), format!("Find file: {typed}{typed}"));
    assert_eq!(
        harness.kill_ring()[0],
        format!("{}/ab", directory.path().display())
    );
    harness.keys("C-a C-k");
    harness.paste(b"pasted");
    assert_eq!(harness.echo_line(), "Find file: pasted");
    harness.keys("C-g");
    assert_eq!(harness.echo(), "Quit");
    assert!(harness.signaled());
    assert_eq!(harness.echo_line(), "Quit");
    harness.keys("a");
    assert_eq!(harness.text(), "a", "keys edit the buffer again");
}

#[test]
fn history_per_prompt_kind() {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("a.txt"), "a\n").expect("write");
    fs::write(directory.path().join("b.txt"), "b\n").expect("write");
    let mut harness = harness_in(directory.path());
    let dir = format!("{}/", directory.path().display());
    harness.keys("C-x C-f M-p");
    assert_eq!(
        harness.echo_line(),
        format!("Find file: {dir} [Beginning of history; no preceding item]")
    );
    harness.keys("M-n");
    assert_eq!(
        harness.echo_line(),
        format!("Find file: {dir} [End of history; no default available]")
    );
    harness.type_text("a.txt");
    harness.keys("RET C-x C-f");
    harness.type_text("b.txt");
    harness.keys("RET");
    assert_eq!(harness.buffer().name, "b.txt");
    harness.keys("C-x C-f M-p");
    assert_eq!(harness.echo_line(), format!("Find file: {dir}b.txt"));
    harness.keys("M-p");
    assert_eq!(harness.echo_line(), format!("Find file: {dir}a.txt"));
    harness.keys("M-n M-n");
    assert_eq!(
        harness.echo_line(),
        format!("Find file: {dir}"),
        "back to the input"
    );
    harness.keys("M-n");
    assert_eq!(
        harness.echo_line(),
        format!("Find file: {dir}b.txt"),
        "then the default: the visited file"
    );
    harness.keys("M-n");
    assert_eq!(
        harness.echo_line(),
        format!("Find file: {dir}b.txt [End of defaults; no next item]")
    );
    harness.keys("C-g");
    // Buffer names have their own history.
    harness.keys("C-x k M-p");
    assert_eq!(
        harness.echo_line(),
        "Kill buffer (default b.txt):  [Beginning of history; no preceding item]"
    );
    harness.keys("M-n");
    assert_eq!(harness.echo_line(), "Kill buffer (default b.txt): b.txt");
}

#[test]
fn y_or_n_answers_like_emacs() {
    let directory = tempfile::tempdir().expect("tempdir");
    let target = directory.path().join("t.txt");
    fs::write(&target, "t\n").expect("write");
    let mut harness = harness_in(directory.path());
    harness.keys("h C-x C-w");
    harness.type_text("t.txt");
    harness.keys("RET");
    let question = format!(
        "File \u{2018}{}\u{2019} exists; overwrite? (y or n) ",
        target.display()
    );
    assert_eq!(harness.echo_line(), question);
    harness.keys("x");
    assert_eq!(
        harness.echo_line(),
        format!("{question} [Please answer y or n]")
    );
    harness.fire_timers();
    assert_eq!(harness.echo_line(), question);
    harness.keys("n");
    assert_eq!(harness.echo(), "Canceled");
    assert_eq!(fs::read(&target).expect("read"), b"t\n");
    harness.keys("C-x C-w");
    harness.type_text("t.txt");
    harness.keys("RET SPC");
    assert_eq!(fs::read(&target).expect("read"), b"h");
}

#[test]
fn yes_or_no_needs_a_whole_word() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    fs::write(&path, "a\n").expect("write");
    let mut harness = harness_in(directory.path());
    harness.keys("C-x C-f");
    harness.type_text("a.txt");
    harness.keys("RET x C-x C-c n");
    let question = "Modified buffers exist; exit anyway? (yes or no) ";
    assert_eq!(harness.echo_line(), question);
    harness.keys("y RET");
    assert_eq!(harness.echo_line(), "Please answer yes or no.");
    harness.keys("n");
    assert_eq!(
        harness.echo_line(),
        format!("{question}n"),
        "typing resumes"
    );
    harness.keys("o RET");
    assert!(!harness.shutdown_requested());
}

#[test]
fn read_only_mode_toggles() {
    let mut harness = Harness::new();
    harness.keys("C-x C-q a");
    assert_eq!(harness.echo(), "Buffer is read-only: #<buffer *scratch*>");
    harness.keys("C-x C-q a");
    assert_eq!(harness.text(), "a");
}
