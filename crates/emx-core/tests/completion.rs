//! Vertical incremental completion (DESIGN section 4): `fido-vertical-mode`
//! behavior with candidates listed above the prompt, file-name completion
//! fed by directory listings that never run on the editor thread, buffer
//! and command completion, and `M-y` reading a kill with completion.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use emx_core::testing::{HARNESS_ROWS, Harness};
use emx_core::{Config, Session};
use emx_proto::ClientKind;
use emx_types::Face;

/// The echo area is the last row; candidates sit right above it.
const ECHO_ROW: u16 = HARNESS_ROWS - 1;

fn harness_in(directory: &Path) -> Harness {
    let mut harness = Harness::new();
    harness.set_default_directory(directory);
    harness
}

fn fruit_directory() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    for name in ["apple.txt", "apricot.txt", "banana.txt"] {
        fs::write(directory.path().join(name), name).expect("write");
    }
    fs::create_dir(directory.path().join("sub")).expect("mkdir");
    fs::write(directory.path().join("sub/inner.txt"), "inner").expect("write");
    directory
}

fn dir_text(directory: &Path) -> String {
    format!("{}/", directory.display())
}

#[test]
fn file_completion_lists_and_narrows() {
    let directory = fruit_directory();
    let mut harness = harness_in(directory.path());
    let prefix = format!("Find file: {}", dir_text(directory.path()));
    harness.keys("C-x C-f");
    assert_eq!(harness.echo_line(), prefix);
    // Shortest first, then by name; directories end in `/`.
    assert_eq!(
        harness.candidates(),
        ["sub/", "apple.txt", "banana.txt", "apricot.txt"]
    );
    assert_eq!(harness.selected_candidate().as_deref(), Some("sub/"));
    let screen = harness.screen();
    let first = usize::from(ECHO_ROW) - 4;
    assert_eq!(screen[first].trim_end(), "sub/");
    assert_eq!(screen[first + 3].trim_end(), "apricot.txt");
    assert!(
        screen[first - 1].starts_with("-UUU:"),
        "the window's modeline moved up above the list: {screen:?}"
    );
    let grid = harness.grid();
    assert_eq!(
        grid.row(u16::try_from(first).expect("small"))[0].face,
        Face::CompletionSelected
    );
    assert_eq!(
        grid.row(u16::try_from(first + 1).expect("small"))[0].face,
        Face::Default
    );
    harness.type_text("ap");
    assert_eq!(harness.candidates(), ["apple.txt", "apricot.txt"]);
    harness.type_text("r");
    assert_eq!(harness.candidates(), ["apricot.txt"]);
    // Flex: the characters in order, ignoring case.
    harness.keys("C-a C-k");
    harness.type_text(&format!("{}BNA", dir_text(directory.path())));
    assert_eq!(harness.candidates(), ["banana.txt"]);
    harness.type_text("zz");
    assert!(harness.candidates().is_empty());
    assert_eq!(harness.echo_line(), format!("{prefix}BNAzz [No matches]"));
    harness.keys("C-g");
    assert!(harness.candidates().is_empty());
    assert!(
        harness.screen()[usize::from(ECHO_ROW) - 1].starts_with("-UUU:"),
        "the window has its rows back"
    );
}

#[test]
fn ret_selects_top_candidate() {
    let directory = fruit_directory();
    let mut harness = harness_in(directory.path());
    harness.keys("C-x C-f");
    harness.type_text("at");
    // Each name has an "a" and later a "t": flex matches all, in listing
    // order.
    assert_eq!(
        harness.candidates(),
        ["apple.txt", "banana.txt", "apricot.txt"]
    );
    harness.keys("DEL");
    harness.type_text("n");
    assert_eq!(harness.candidates(), ["banana.txt"]);
    harness.keys("RET");
    assert_eq!(harness.buffer().name, "banana.txt");
    assert_eq!(harness.text(), "banana.txt");
    // C-n moves the selection; RET takes it.
    harness.keys("C-x C-f");
    harness.type_text("ap");
    harness.keys("C-n");
    assert_eq!(harness.selected_candidate().as_deref(), Some("apricot.txt"));
    harness.keys("C-n");
    assert_eq!(
        harness.selected_candidate().as_deref(),
        Some("apricot.txt"),
        "the selection stops at the last candidate"
    );
    harness.keys("<up> <up>");
    assert_eq!(harness.selected_candidate().as_deref(), Some("apple.txt"));
    harness.keys("<down> RET");
    assert_eq!(harness.buffer().name, "apricot.txt");
    // RET on a directory enters it instead of exiting.
    harness.keys("C-x C-f");
    harness.type_text("su");
    harness.keys("RET");
    assert_eq!(
        harness.echo_line(),
        format!("Find file: {}sub/", dir_text(directory.path()))
    );
    assert_eq!(harness.candidates(), ["inner.txt"]);
    harness.keys("RET");
    assert_eq!(harness.buffer().name, "inner.txt");
}

#[test]
fn c_j_accepts_literal_new_file() {
    let directory = fruit_directory();
    let mut harness = harness_in(directory.path());
    harness.keys("C-x C-f");
    harness.type_text("ap");
    assert_eq!(harness.candidates(), ["apple.txt", "apricot.txt"]);
    harness.keys("C-j");
    assert_eq!(harness.buffer().name, "ap");
    assert_eq!(harness.echo(), "(New file)");
    assert_eq!(
        harness.buffer().file.as_ref().expect("visits a file").path,
        directory.path().join("ap")
    );
    harness.keys("C-x C-f");
    harness.type_text("ban");
    harness.keys("M-j");
    assert_eq!(harness.buffer().name, "ban", "M-j takes the input as well");
}

#[test]
fn del_at_slash_goes_up() {
    let directory = fruit_directory();
    let parent = directory.path().parent().expect("tempdir has a parent");
    let mut harness = harness_in(directory.path());
    harness.keys("C-x C-f");
    harness.type_text("sub/");
    assert_eq!(harness.candidates(), ["inner.txt"]);
    harness.type_text("inx");
    harness.keys("DEL");
    assert_eq!(
        harness.echo_line(),
        format!("Find file: {}sub/in", dir_text(directory.path())),
        "DEL elsewhere deletes a character"
    );
    harness.keys("DEL DEL");
    assert_eq!(harness.candidates(), ["inner.txt"]);
    harness.keys("DEL");
    assert_eq!(
        harness.echo_line(),
        format!("Find file: {}", dir_text(directory.path())),
        "DEL after a slash goes up a directory"
    );
    assert_eq!(harness.candidates().len(), 4);
    harness.keys("DEL");
    assert_eq!(
        harness.echo_line(),
        format!("Find file: {}", dir_text(parent))
    );
    let name = directory
        .path()
        .file_name()
        .expect("named")
        .to_string_lossy()
        .into_owned();
    harness.type_text(&name);
    assert_eq!(
        harness.candidates().first().map(String::as_str),
        Some(format!("{name}/").as_str())
    );
}

#[test]
fn buffer_completion_order() {
    let mut harness = Harness::new();
    harness.keys("C-x b f o o RET C-x b b a r RET");
    assert_eq!(harness.buffer().name, "bar");
    harness.keys("C-x b");
    // Most recent first, the current buffer last; the default (Emacs
    // `other-buffer`) is on top.
    assert_eq!(
        harness.candidates(),
        ["foo", "*scratch*", "*Messages*", "bar"]
    );
    harness.keys("C-g C-x k");
    assert_eq!(
        harness.candidates(),
        ["bar", "foo", "*scratch*", "*Messages*"],
        "kill-buffer's default, the current buffer, is bubbled to the top"
    );
    harness.type_text("s");
    assert_eq!(harness.candidates(), ["*scratch*", "*Messages*"]);
    harness.keys("C-g C-x b");
    harness.type_text("fo");
    harness.keys("RET");
    assert_eq!(harness.buffer().name, "foo");
    // An exact name outranks a longer match listed before it.
    harness.keys("C-x b f o o b a r C-j C-x b b a r RET");
    assert_eq!(harness.buffer().name, "bar");
    harness.keys("C-x b f o o");
    assert_eq!(harness.candidates(), ["foo", "foobar"]);
    harness.keys("RET");
    assert_eq!(harness.buffer().name, "foo");
}

#[test]
fn m_x_runs_command() {
    let mut harness = Harness::with_text("|abcdef");
    harness.keys("M-x");
    assert_eq!(harness.echo_line(), "M-x ");
    assert!(
        harness.candidates().len() > 1,
        "every command is a candidate"
    );
    harness.type_text("forward-char");
    assert_eq!(
        harness.selected_candidate().as_deref(),
        Some("forward-char")
    );
    harness.keys("RET");
    assert_eq!(harness.point(), 1);
    // The prefix argument goes to the command and shows in the prompt.
    harness.keys("C-u 2 M-x");
    assert_eq!(harness.echo_line(), "2 M-x ");
    harness.type_text("forw");
    assert_eq!(
        harness.selected_candidate().as_deref(),
        Some("forward-char")
    );
    harness.keys("RET");
    assert_eq!(harness.point(), 3);
    harness.keys("C-u M-x");
    assert_eq!(harness.echo_line(), "C-u M-x ");
    harness.keys("C-g");
    // A name that is no command is refused, as `M-x` requires a match.
    harness.keys("M-x");
    harness.type_text("no-such-command");
    harness.keys("C-j");
    assert_eq!(harness.echo_line(), "M-x no-such-command [No match]");
    assert_eq!(harness.candidates(), Vec::<String>::new());
    harness.keys("C-g");
    // A command that prompts works from M-x too.
    harness.keys("M-x");
    harness.type_text("switch-to-buffer");
    harness.keys("RET");
    assert!(harness.echo_line().starts_with("Switch to buffer"));
}

#[test]
fn history_m_p() {
    let directory = fruit_directory();
    let mut harness = harness_in(directory.path());
    harness.keys("M-x");
    harness.type_text("forward-char");
    harness.keys("RET M-x M-p");
    assert_eq!(harness.echo_line(), "M-x forward-char");
    harness.keys("M-p");
    assert_eq!(
        harness.echo_line(),
        "M-x forward-char [Beginning of history; no preceding item]"
    );
    harness.keys("M-n");
    assert_eq!(harness.echo_line(), "M-x ");
    // Commands run before are listed first.
    assert_eq!(harness.candidates()[0], "forward-char");
    harness.keys("C-g C-x C-f");
    harness.type_text("apple.txt");
    harness.keys("RET C-x b M-p");
    assert_eq!(
        harness.echo_line(),
        "Switch to buffer (default *scratch*):  [Beginning of history; no preceding item]",
        "each kind of prompt has its own history"
    );
    harness.keys("C-g C-x C-f M-p");
    assert_eq!(
        harness.echo_line(),
        format!("Find file: {}apple.txt", dir_text(directory.path()))
    );
    assert_eq!(harness.candidates(), ["apple.txt"]);
}

#[test]
fn m_y_after_a_non_yank_reads_from_the_kill_ring() {
    let mut harness = Harness::with_text("|a\nb\nc\n");
    harness.keys("M-y");
    assert_eq!(harness.echo(), "Kill ring is empty");
    harness.keys("C-k C-f C-k C-f M-y");
    assert_eq!(harness.echo_line(), "Yank from kill-ring: ");
    assert_eq!(harness.candidates(), ["b", "a"], "most recent first");
    harness.keys("C-n RET");
    assert_eq!(harness.text(), "\n\nac\n");
    assert_eq!(harness.point(), 3);
    assert_eq!(harness.mark(), Some(2), "the mark is set like yank's");
    assert_eq!(harness.echo(), "Mark set");
    harness.keys("C-y");
    assert_eq!(
        harness.text(),
        "\n\naac\n",
        "the yank pointer is at the kill chosen"
    );
    harness.keys("C-/ C-/");
    assert_eq!(harness.text(), "\n\nc\n", "the insertion is undoable");
    // C-u leaves point before the text; typing filters; M-p reads the ring.
    harness.keys("C-u M-y");
    harness.type_text("b");
    assert_eq!(harness.candidates(), ["b"]);
    harness.keys("RET");
    assert_eq!(harness.text(), "\n\nbc\n");
    assert_eq!(harness.point(), 2);
    assert_eq!(harness.mark(), Some(3));
    harness.keys("C-e M-y M-p");
    assert_eq!(harness.echo_line(), "Yank from kill-ring: b");
    harness.keys("M-p");
    assert_eq!(harness.echo_line(), "Yank from kill-ring: a");
    // Edited text is not a kill yet: it is inserted and pushed.
    harness.keys("x C-j");
    assert_eq!(harness.text(), "\n\nbcax\n");
    assert_eq!(harness.kill_ring()[0], "ax");
}

#[test]
fn kill_ring_candidates_are_truncated_like_emacs() {
    let long = "x".repeat(100);
    let mut harness = Harness::with_text(&format!("|   indented\n{long}\n"));
    harness.keys("C-k C-f C-k M-y");
    let mut truncated = "x".repeat(80);
    truncated.push('\u{2026}');
    assert_eq!(
        harness.candidates(),
        [truncated, "\u{2026}indented".to_string()]
    );
    // The whole kill is inserted.
    harness.keys("RET");
    assert!(harness.text().contains(&long));
}

#[test]
fn listings_arrive_later_and_stale_ones_are_ignored() {
    let directory = fruit_directory();
    let mut harness = harness_in(directory.path());
    harness.hold_listings();
    harness.keys("C-x C-f");
    let prefix = format!("Find file: {}", dir_text(directory.path()));
    assert_eq!(harness.echo_line(), prefix, "the prompt shows at once");
    assert!(harness.candidates().is_empty());
    assert_eq!(harness.held_listings(), [directory.path().to_path_buf()]);
    // The input moves to another directory before the first listing is in.
    harness.type_text("sub/");
    assert_eq!(
        harness.held_listings(),
        [directory.path().to_path_buf(), directory.path().join("sub")]
    );
    harness.release_newest_listing();
    assert_eq!(harness.candidates(), ["inner.txt"]);
    harness.release_listings();
    assert_eq!(
        harness.candidates(),
        ["inner.txt"],
        "the stale listing of the parent is ignored"
    );
    // RET while the listing is on its way takes the top candidate once it
    // arrives.
    harness.keys("C-g");
    harness.hold_listings();
    harness.keys("C-x C-f");
    harness.type_text("ban");
    harness.keys("RET");
    assert!(harness.echo_line().starts_with("Find file: "));
    harness.release_listings();
    assert_eq!(harness.buffer().name, "banana.txt");
    // A key typed meanwhile cancels the waiting RET.
    harness.hold_listings();
    harness.keys("C-x C-f");
    harness.type_text("ap");
    harness.keys("RET");
    harness.type_text("p");
    harness.release_listings();
    assert_eq!(harness.candidates(), ["apple.txt"]);
    assert!(harness.echo_line().ends_with("app"));
}

#[test]
fn unlistable_directory_says_why() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut harness = harness_in(directory.path());
    harness.keys("C-x C-f");
    harness.type_text("missing/");
    assert!(
        harness
            .echo_line()
            .ends_with("missing/ [No such file or directory]"),
        "{}",
        harness.echo_line()
    );
}

#[test]
fn non_utf8_file_names_round_trip() {
    let directory = tempfile::tempdir().expect("tempdir");
    let name = OsStr::from_bytes(b"caf\xe9.txt");
    fs::write(directory.path().join(name), "old").expect("write");
    let mut harness = harness_in(directory.path());
    harness.keys("C-x C-f");
    assert_eq!(harness.candidates(), ["caf\u{fffd}.txt"]);
    let first = usize::from(ECHO_ROW) - 1;
    assert_eq!(
        harness.screen()[first].trim_end(),
        "caf\\351.txt",
        "shown with an escape, as emx shows bytes that are not text"
    );
    harness.keys("TAB");
    let mut expected = format!("Find file: {}", dir_text(directory.path())).into_bytes();
    expected.extend_from_slice(b"caf\xe9.txt");
    assert_eq!(harness.echo_line_bytes(), expected, "the bytes are kept");
    harness.keys("RET");
    assert_eq!(
        harness
            .buffer()
            .file
            .as_ref()
            .expect("visits the file")
            .path,
        directory.path().join(name)
    );
    assert_eq!(harness.text(), "old");
    harness.keys("M-> x C-x C-s");
    assert_eq!(
        fs::read(directory.path().join(name)).expect("read"),
        b"oldx"
    );
    // The file's history entry keeps its bytes too.
    harness.keys("C-x C-f M-p");
    assert_eq!(harness.echo_line_bytes(), expected);
}

#[test]
fn completion_is_per_client() {
    let directory = fruit_directory();
    let mut harness = Harness::with_session(Config::default(), Session::Server);
    harness.set_default_directory(directory.path());
    let other = harness.attach(ClientKind::Tui);
    harness.keys("C-x C-f");
    harness.keys_as(other, "C-x b");
    assert_eq!(harness.candidates().len(), 4);
    assert_eq!(harness.candidates_of(other), ["*Messages*", "*scratch*"]);
    harness.type_as(other, "mes");
    assert_eq!(harness.candidates_of(other), ["*Messages*"]);
    assert_eq!(
        harness.candidates().len(),
        4,
        "the first client's list is its own"
    );
    harness.type_text("ban");
    harness.keys("RET");
    assert_eq!(harness.buffer().name, "banana.txt");
    assert_eq!(harness.candidates_of(other), ["*Messages*"]);
    harness.detach(other);
}

#[test]
fn split_windows_shrink_for_candidates_and_come_back() {
    let directory = fruit_directory();
    let mut harness = harness_in(directory.path());
    harness.keys("C-x 2");
    let before = harness.screen();
    let modelines = |screen: &[String]| -> Vec<usize> {
        screen
            .iter()
            .enumerate()
            .filter(|(_, row)| row.starts_with("-UUU:"))
            .map(|(index, _)| index)
            .collect()
    };
    let rows_before = modelines(&before);
    harness.keys("C-x C-f");
    let during = modelines(&harness.screen());
    assert_eq!(during.len(), 2);
    assert!(during[1] < rows_before[1], "the windows gave up rows");
    harness.keys("C-g");
    assert_eq!(modelines(&harness.screen()), rows_before);
}

#[test]
fn large_directories_show_ten_candidates() {
    const ENTRIES: usize = 100_000;
    let directory = tempfile::tempdir().expect("tempdir");
    for number in 0..ENTRIES {
        fs::File::create(directory.path().join(format!("f{number:06}"))).expect("create");
    }
    let mut harness = harness_in(directory.path());
    harness.keys("C-x C-f");
    assert_eq!(harness.candidates().len(), 10);
    assert_eq!(harness.completion_selection(), Some((0, ENTRIES)));
    assert_eq!(harness.candidates()[0], "f000000");
    harness.type_text("9999");
    assert_eq!(harness.candidates()[0], "f009999");
    harness.keys("M->");
    assert_eq!(
        harness.completion_selection().map(|(selected, _)| selected),
        harness.completion_selection().map(|(_, count)| count - 1)
    );
    assert_eq!(
        harness.candidates().len(),
        10,
        "the list scrolls to the selection"
    );
}
