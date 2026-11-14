//! Windows and the buffer list (DESIGN section 4: `C-x 2/3/0/1/o`, `C-x b`,
//! `C-x C-b`). Layouts, sizes, messages and the buffer menu were captured
//! from `emacs -nw -Q` 29.3 in a tmux terminal of the same size, and from
//! window.el and buff-menu.el.

use emx_core::testing::Harness;
use emx_proto::ClientKind;
use emx_types::Face;

fn row_faces(harness: &Harness, row: u16) -> Vec<Face> {
    harness
        .grid()
        .row(row)
        .iter()
        .map(|cell| cell.face)
        .collect()
}

#[test]
fn split_and_cycle() {
    let mut harness = Harness::with_text("|abc\ndef\n");
    harness.keys("C-x 2");
    let screen = harness.screen();
    // 23 window rows split into 12 (selected, above) and 11 (new, below).
    assert_eq!(screen[0].trim_end(), "abc");
    assert_eq!(screen[1].trim_end(), "def");
    assert!(screen[11].starts_with("-UUU:---  F1  *scratch*      All   L1     (Fundamental) ---"));
    assert_eq!(screen[12].trim_end(), "abc");
    assert!(screen[22].starts_with("-UUU:---  F1  *scratch*      All   L1"));
    assert!(
        row_faces(&harness, 11)
            .iter()
            .all(|face| *face == Face::ModeLine)
    );
    assert!(
        row_faces(&harness, 22)
            .iter()
            .all(|face| *face == Face::ModeLineInactive)
    );
    assert_eq!(
        harness.cursor(),
        Some((0, 0)),
        "the upper window stays selected"
    );

    harness.keys("C-x o");
    assert_eq!(harness.cursor(), Some((0, 12)));
    assert!(
        row_faces(&harness, 11)
            .iter()
            .all(|face| *face == Face::ModeLineInactive)
    );
    assert!(
        row_faces(&harness, 22)
            .iter()
            .all(|face| *face == Face::ModeLine)
    );
    harness.keys("C-x o");
    assert_eq!(harness.cursor(), Some((0, 0)), "the cycle wraps");

    // Side by side: the left window keeps 40 columns, its last one the
    // vertical border drawn in the inactive modeline face on every row.
    harness.keys("C-x 3");
    let screen = harness.screen();
    let grid = harness.grid();
    for (row, line) in (0..12).zip(&screen) {
        assert_eq!(&line[39..40], "|", "row {row}: {line:?}");
        assert_eq!(grid.row(row)[39].face, Face::ModeLineInactive);
    }
    assert_eq!(
        &screen[0][..44],
        "abc                                    |abc "
    );
    assert!(
        screen[11].starts_with("-UUU:---  F1  *scratch*      All   L1  |-UUU:---  F1  "),
        "{:?}",
        screen[11]
    );
    assert_eq!(harness.grid().row(11)[0].face, Face::ModeLine);
    assert_eq!(harness.grid().row(11)[40].face, Face::ModeLineInactive);
    // Cyclic order: top left, top right, bottom.
    harness.keys("C-x o");
    assert_eq!(harness.cursor(), Some((40, 0)));
    harness.keys("C-x o");
    assert_eq!(harness.cursor(), Some((0, 12)));
    harness.keys("C-x o");
    assert_eq!(harness.cursor(), Some((0, 0)));
    // A count skips windows; a negative one goes backwards.
    harness.keys("C-u 2 C-x o");
    assert_eq!(harness.cursor(), Some((0, 12)));
    harness.keys("M-- C-x o");
    assert_eq!(harness.cursor(), Some((40, 0)));
    harness.keys("C-u 0 C-x o");
    assert_eq!(harness.cursor(), Some((40, 0)));
    harness.keys("C-u 4 C-x o");
    assert_eq!(harness.cursor(), Some((0, 12)));
}

#[test]
fn other_window_alone_says_so() {
    let mut harness = Harness::with_text("|abc");
    harness.keys("C-x o");
    assert_eq!(harness.echo(), "No other window to select");
    assert!(!harness.signaled());
}

#[test]
fn windows_on_same_buffer_have_independent_points() {
    let mut harness = Harness::with_text("|abc\ndef\n");
    harness.keys("C-x 2 C-n C-f");
    assert_eq!(harness.point(), 5);
    harness.keys("C-x o");
    assert_eq!(harness.point(), 0, "the new window kept the split point");
    harness.keys("C-e");
    assert_eq!(harness.point(), 3);
    assert_eq!(harness.cursor(), Some((3, 12)));
    harness.keys("C-x o");
    assert_eq!(harness.point(), 5);
    assert_eq!(harness.cursor(), Some((1, 1)));

    // An insertion before the other window's point moves it.
    harness.keys("C-x o C-a X");
    assert_eq!(harness.text(), "Xabc\ndef\n");
    harness.keys("C-x o");
    assert_eq!(harness.point(), 6);
    assert_eq!(harness.screen()[12].trim_end(), "Xabc");
}

#[test]
fn split_keeps_point_and_starts_the_new_window_where_the_buffer_was_last_shown() {
    // emacs -nw -Q, 40x24, a 100-line file: C-v, C-n x3, C-x 2 leaves
    // the upper window's start alone; the new window starts where the
    // buffer was last shown (its top) and recenters around point.
    let mut text = String::from("|");
    for line in 0..100 {
        text.push_str(&format!("L{line:02}\n"));
    }
    let mut harness = Harness::with_text(&text);
    harness.resize(40, 24);
    harness.keys("C-v C-n C-n C-n");
    assert_eq!(harness.screen()[0].trim_end(), "L20");
    harness.keys("C-x 2");
    let screen = harness.screen();
    assert_eq!(screen[0].trim_end(), "L20");
    assert_eq!(screen[3].trim_end(), "L23");
    // The lower window has 10 text rows; point's row is in the middle.
    assert_eq!(screen[12].trim_end(), "L18");
    harness.keys("C-x o");
    assert_eq!(harness.point(), 23 * 4);
    assert_eq!(harness.cursor(), Some((0, 17)));
}

#[test]
fn delete_other_windows() {
    let mut harness = Harness::with_text("|abc\n");
    harness.keys("C-x 2 C-x 3 C-x o");
    assert_eq!(harness.cursor(), Some((40, 0)));
    harness.keys("C-x 1");
    let screen = harness.screen();
    assert_eq!(screen.len(), 24);
    assert_eq!(screen[0].trim_end(), "abc");
    assert!(screen[22].starts_with("-UUU:---  F1  *scratch*      All   L1"));
    assert_eq!(screen[11].trim_end(), "");
    assert!(!screen[0].contains('|'));
    assert_eq!(harness.cursor(), Some((0, 0)));
    harness.keys("C-x 1");
    assert_eq!(harness.echo(), "No other windows to delete");
    assert!(!harness.signaled());
}

#[test]
fn delete_window_gives_space_to_the_sibling_and_selects_the_most_recent() {
    let mut harness = Harness::with_text("|abc\n");
    harness.keys("C-x 0");
    assert!(harness.signaled());
    assert_eq!(
        harness.echo(),
        "Attempt to delete minibuffer or sole ordinary window"
    );

    // Three windows stacked: 12, 6 and 5 rows. Deleting the middle one
    // gives its rows to the one above (Emacs prefers the left sibling).
    harness.keys("C-x 2 C-x o C-x 2");
    assert!(harness.screen()[11].starts_with("-UUU"));
    assert!(harness.screen()[17].starts_with("-UUU"));
    assert!(harness.screen()[22].starts_with("-UUU"));
    harness.keys("C-x o");
    assert_eq!(harness.cursor(), Some((0, 18)));
    harness.keys("C-x o");
    assert_eq!(harness.cursor(), Some((0, 0)));
    // Select the middle window and delete it: the top window was used
    // more recently than the bottom one, so it is selected.
    harness.keys("C-x o C-x 0");
    let screen = harness.screen();
    assert!(screen[17].starts_with("-UUU"), "{screen:?}");
    assert!(screen[22].starts_with("-UUU"));
    assert_eq!(screen[11].trim_end(), "");
    assert_eq!(harness.cursor(), Some((0, 0)));
    assert_eq!(row_faces(&harness, 17)[0], Face::ModeLine);

    // Deleting the top window gives its rows to the one below.
    harness.keys("C-x 0");
    assert!(harness.screen()[22].starts_with("-UUU"));
    assert_eq!(harness.screen()[0].trim_end(), "abc");
    assert_eq!(harness.cursor(), Some((0, 0)));
}

#[test]
fn deleting_a_side_window_merges_the_layout_back() {
    let mut harness = Harness::with_text("|abc\n");
    harness.keys("C-x 3 C-x 2 C-x o C-x o C-x 0");
    // The right window is gone: the left column (two windows) spans the
    // frame again with no border.
    let screen = harness.screen();
    assert!(!screen[0].contains('|'), "{screen:?}");
    assert!(screen[11].starts_with("-UUU:---  F1  *scratch*      All   L1     (Fundamental) ---"));
    assert!(screen[22].starts_with("-UUU"));
    // The most recently used window left, the lower one, is selected.
    assert_eq!(harness.cursor(), Some((0, 12)));
    harness.keys("C-x 2");
    assert!(
        harness.screen()[17].starts_with("-UUU"),
        "the selected window split again: {:?}",
        harness.screen()
    );
}

#[test]
fn split_too_small_errors() {
    // 7 terminal rows: a 6-row root window cannot give half its rows
    // away and keep window-min-height (4).
    let mut harness = Harness::with_text("|abc\n");
    harness.resize(80, 7);
    harness.keys("C-x 2");
    assert!(harness.signaled());
    assert_eq!(
        harness.echo(),
        "Window #<window 1 on *scratch*> too small for splitting"
    );
    harness.resize(80, 8);
    harness.keys("C-x 2");
    assert!(!harness.signaled());
    // 4 rows above, 3 below.
    assert!(harness.screen()[3].starts_with("-UUU"));
    assert!(harness.screen()[6].starts_with("-UUU"));

    // window-min-width is 10 columns.
    let mut harness = Harness::with_text("|abc\n");
    harness.resize(18, 10);
    harness.keys("C-x 3");
    assert_eq!(
        harness.echo(),
        "Window #<window 1 on *scratch*> too small for splitting"
    );
    harness.resize(19, 10);
    harness.keys("C-x 3");
    assert!(!harness.signaled());
    assert_eq!(&harness.screen()[0][9..10], "|");

    // Window numbers are Emacs's: the minibuffer window is 2, the first
    // split's new parent 3 and its new window 4, the second split adds 5.
    let mut harness = Harness::with_text("|abc\n");
    harness.keys("C-x 2 C-x 2 C-x o C-x 2");
    assert_eq!(
        harness.echo(),
        "Window #<window 5 on *scratch*> too small for splitting"
    );
}

#[test]
fn split_sizes_from_the_prefix_argument() {
    let mut harness = Harness::with_text("|abc\n");
    harness.resize(60, 12);
    // C-u 3: the selected window gets 3 rows.
    harness.keys("C-u 3 C-x 2");
    assert!(harness.screen()[2].starts_with("-UUU"));
    assert!(harness.screen()[10].starts_with("-UUU"));

    let mut harness = Harness::with_text("|abc\n");
    harness.resize(60, 12);
    harness.keys("C-u - 3 C-x 2");
    assert_eq!(harness.echo(), "Size of new window too small");
    harness.keys("C-u 1 1 C-x 2");
    assert_eq!(
        harness.echo(),
        "Window #<window 1 on *scratch*> too small for splitting"
    );
    harness.keys("C-u - 4 C-x 2");
    assert!(!harness.signaled());
    assert!(harness.screen()[6].starts_with("-UUU"));
    assert!(harness.screen()[10].starts_with("-UUU"));

    let mut harness = Harness::with_text("|abc\n");
    harness.keys("C-u 2 5 C-x 3");
    assert_eq!(&harness.screen()[0][24..25], "|");
}

#[test]
fn resizing_the_terminal_keeps_window_proportions() {
    let mut harness = Harness::with_text("|abc\n");
    harness.keys("C-x 3");
    harness.resize(120, 24);
    assert_eq!(&harness.screen()[0][59..60], "|");
    harness.resize(80, 24);
    assert_eq!(&harness.screen()[0][39..40], "|");
}

#[test]
fn a_wrapped_window_start_survives_a_width_change() {
    // One long line wrapped over many rows: the window starts part way
    // through it, then the window is split side by side. The new width
    // shows the same text at the top, from the start of its row.
    let line: String = (0..400)
        .map(|index| char::from(b'a' + (index % 26) as u8))
        .collect();
    let mut harness = Harness::with_text(&format!("{line}\n|end\n"));
    harness.resize(80, 6);
    // Point at "end": the window shows the end of the long line.
    // Recentered: the window starts at the long line's fifth row.
    let top = harness.screen()[0].clone();
    assert!(top.starts_with(&line[316..395]), "{top:?}");
    harness.keys("C-x 3");
    let top = harness.screen()[0].clone();
    // The left window has 39 text columns, 38 of them for text: the row
    // holding position 316 starts at 304.
    assert!(top.starts_with(&line[304..342]), "{top:?}");
}

#[test]
fn switch_to_buffer_creates_and_restores_window_point() {
    let mut harness = Harness::with_text("abc|def\n");
    harness.keys("C-x b");
    assert_eq!(
        harness.echo_line(),
        "Switch to buffer (default *scratch*): "
    );
    harness.type_text("foo");
    harness.keys("RET");
    assert_eq!(harness.buffer().name, "foo");
    harness.type_text("hello");
    harness.keys("C-x b");
    assert_eq!(
        harness.echo_line(),
        "Switch to buffer (default *scratch*): "
    );
    harness.keys("RET");
    assert_eq!(harness.buffer().name, "*scratch*");
    assert_eq!(harness.point(), 3, "the window's point for it is restored");
    harness.keys("C-x b RET");
    assert_eq!(harness.buffer().name, "foo");
    assert_eq!(harness.point(), 5);

    // Two windows: the other one shows *scratch*, so C-x b offers a buffer
    // no window shows.
    harness.keys("C-x 2 C-x b b a r RET C-x o C-x b");
    assert_eq!(
        harness.echo_line(),
        "Switch to buffer (default *scratch*): "
    );
    harness.keys("C-g");
}

#[test]
fn each_window_remembers_its_own_point_per_buffer() {
    let mut harness = Harness::with_text("|abc\ndef\n");
    harness.keys("C-x b f o o RET x y z");
    harness.keys("C-x 2");
    // Upper: foo at 3. Lower: foo at 3 too; move it, then show *scratch*
    // in both and come back.
    harness.keys("C-x o C-a C-f C-x b RET");
    assert_eq!(harness.buffer().name, "*scratch*");
    harness.keys("C-n");
    harness.keys("C-x o C-x b RET");
    assert_eq!(harness.buffer().name, "*scratch*");
    harness.keys("C-x b RET");
    assert_eq!(harness.buffer().name, "foo");
    assert_eq!(harness.point(), 3);
    harness.keys("C-x o C-x b RET");
    assert_eq!(harness.buffer().name, "foo");
    assert_eq!(harness.point(), 1);
}

/// `*Buffer List*` as Emacs 29 prints it: columns C, R, M, the buffer name
/// in 19 columns, the size right-aligned in 7, the mode in 16, the file.
fn buffer_list_line(flags: &str, name: &str, size: u64, file: &str) -> String {
    format!("{flags} {name:<19} {size:>7} {:<16} {file}", "Fundamental")
}

#[test]
fn buffer_list_visits() {
    let mut harness = Harness::with_text("|");
    harness.keys("C-x b f o o RET h e l l o C-x b RET");
    harness.keys("C-x C-b");
    let screen = harness.screen();
    // The list pops up below, unselected, with its header line.
    assert_eq!(harness.buffer().name, "*scratch*");
    assert_eq!(harness.cursor(), Some((0, 0)));
    assert_eq!(
        screen[12].trim_end(),
        "CRM Buffer                 Size Mode             File"
    );
    assert_eq!(harness.grid().row(12)[0].face, Face::HeaderLine);
    assert_eq!(
        screen[13].trim_end(),
        buffer_list_line(".  ", "*scratch*", 0, "").trim_end()
    );
    assert_eq!(
        screen[14].trim_end(),
        buffer_list_line("  *", "foo", 5, "").trim_end()
    );
    assert!(
        screen[22].starts_with("-UUU:%%-  F1  *Buffer List*   All   L1     (Buffer Menu) ---"),
        "{:?}",
        screen[22]
    );
    assert_eq!(
        harness.buffer_text("*Buffer List*").expect("listed"),
        format!(
            "{}\n{}\n",
            buffer_list_line(".  ", "*scratch*", 0, ""),
            buffer_list_line("  *", "foo", 5, "")
        )
    );

    harness.keys("C-x o");
    assert_eq!(harness.buffer().name, "*Buffer List*");
    assert_eq!(harness.point(), 0);
    harness.keys("C-n RET");
    assert_eq!(harness.buffer().name, "foo");
    assert_eq!(harness.point(), 5, "the buffer's point");
    assert_eq!(harness.cursor(), Some((5, 12)));

    // A line with no buffer.
    // The selected window showed the list before, so it shows it again.
    harness.keys("C-x C-b");
    assert_eq!(harness.buffer().name, "*Buffer List*");
    assert_eq!(harness.point(), 0);
    harness.keys("M-> RET");
    assert!(harness.signaled());
    assert_eq!(harness.echo(), "No buffer on this line");
}

#[test]
fn buffer_list_names_files_and_flags() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("notes.txt");
    std::fs::write(&path, "twelve bytes").expect("write");
    let mut harness = Harness::with_text("|");
    harness.set_default_directory(directory.path());
    harness.keys("C-x C-f C-a C-k");
    harness.type_text(path.to_str().expect("UTF-8 path"));
    harness.keys("RET C-x C-q C-x C-b C-x o");
    let text = harness.buffer_text("*Buffer List*").expect("listed");
    let mut lines = text.lines();
    assert_eq!(
        lines.next().expect("first entry"),
        buffer_list_line(".% ", "notes.txt", 12, &path.display().to_string())
    );
    assert_eq!(
        lines.next().expect("second entry").trim_end(),
        buffer_list_line("   ", "*scratch*", 0, "").trim_end()
    );
    assert_eq!(lines.next(), None);
}

#[test]
fn buffer_list_truncates_long_names_into_the_size_column() {
    // As tabulated-list-print-col shows it: a name may run into the unused
    // part of the right-aligned size column, with an ellipsis. Emacs keeps
    // the whole name in the buffer behind a display property, which emx
    // has no equivalent of, so the text is what the screen shows.
    let mut harness = Harness::with_text("|");
    harness.keys("C-x b");
    harness.type_text("a-very-long-buffer-name-indeed");
    harness.keys("RET C-x b RET C-x C-b");
    let text = harness.buffer_text("*Buffer List*").expect("listed");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[1],
        "    a-very-long-buffer-name-\u{2026} 0 Fundamental      "
    );
}

#[test]
fn killing_a_buffer_shows_the_windows_previous_buffer() {
    let mut harness = Harness::with_text("|");
    harness.keys("C-x b f o o RET C-x b b a r RET C-x 2");
    // Both windows show bar. The upper showed foo before bar; the lower
    // never showed anything else, so it takes the most recent buffer.
    harness.keys("C-x k RET");
    assert_eq!(harness.buffer().name, "foo");
    harness.keys("C-x o");
    assert_eq!(harness.buffer().name, "foo");
    assert!(harness.editor().find_buffer("bar").is_none());
}

#[test]
fn windows_of_other_clients_are_their_own() {
    let mut harness = Harness::with_text("|abc\n");
    let other = harness.attach(ClientKind::Tui);
    harness.keys("C-x 2");
    let screen = harness.screen_of(other);
    assert_eq!(screen[0].trim_end(), "abc");
    assert!(screen[22].starts_with("-UUU:---  F1  *scratch*"));
    assert_eq!(screen[11].trim_end(), "", "the other frame is not split");
    harness.keys_as(other, "C-x 3 C-x o");
    assert_eq!(harness.cursor(), Some((0, 0)));
    harness.detach(other);
    harness.keys("C-x o");
    assert_eq!(harness.cursor(), Some((0, 12)));
}
