//! Window display: text layout, the modeline, the echo area and scrolling.
//! Batch Emacs has no window, so these are checked against what
//! `emacs -nw -Q` 29.3 shows in a terminal of the same size (captured with
//! tmux) and against window.c and simple.el.

use std::fs;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use emx_core::testing::Harness;
use emx_core::{ClientId, Config, Event, PrefetchRequest, Session, render};
use emx_proto::{ClientKind, ServerMsg};
use emx_text::{BLOCK_SIZE, BlockCache, LineIndex, Sources, Text, index_file};
use emx_types::{Face, parse_kbd};

/// Lines `L00` to `L39`, each with its newline, point at the start.
fn forty_lines() -> String {
    let mut text = String::from("|");
    for line in 0..40 {
        text.push_str(&format!("L{line:02}\n"));
    }
    text
}

/// A harness whose window shows `text_rows` rows of text: the frame also
/// holds the modeline and the echo area.
fn sized(cols: u16, text_rows: u16, text: &str) -> Harness {
    let mut harness = Harness::with_text(text);
    harness.resize(cols, text_rows + 2);
    harness
}

fn padded(text: &str, cols: usize) -> String {
    format!("{text:<cols$}")
}

fn top_line(harness: &Harness) -> String {
    harness.screen()[0].trim_end().to_string()
}

/// The modeline Emacs shows for this state, with the dashes `%-` adds up
/// to the window width.
fn modeline(prefix: &str, cols: usize) -> String {
    format!("{prefix:-<cols$}")
}

fn frames(sent: &[ServerMsg]) -> Vec<&ServerMsg> {
    sent.iter()
        .filter(|message| matches!(message, ServerMsg::Frame { .. }))
        .collect()
}

#[test]
fn render_basic_screen() {
    let mut harness = Harness::with_text("|hello\nworld");
    harness.resize(20, 6);
    assert_eq!(
        harness.screen(),
        vec![
            padded("hello", 20),
            padded("world", 20),
            padded("", 20),
            padded("", 20),
            // emacs -nw -Q in a 20-column terminal, cut at the edge.
            "-UUU:---  F1  *scrat".to_string(),
            padded("", 20),
        ]
    );
    assert_eq!(harness.cursor(), Some((0, 0)));
    harness.keys("C-n C-e");
    assert_eq!(harness.cursor(), Some((5, 1)));
    let grid = harness.grid();
    assert!(grid.row(4).iter().all(|cell| cell.face == Face::ModeLine));
}

#[test]
fn modeline_matches_the_emacs_nw_screen() {
    // The modeline row of `emacs -nw -Q` in a UTF-8 tmux terminal of each
    // width, for the same buffer and keys.
    for cols in [40, 80, 120] {
        let mut harness = Harness::with_text("|hello\nworld");
        harness.resize(cols, 24);
        let width = usize::from(cols);
        let modeline = |harness: &Harness| harness.screen()[22].clone();
        let expected = |prefix: &str| {
            let mut line = format!("{prefix:-<width$}");
            line.truncate(width);
            line
        };
        assert_eq!(
            modeline(&harness),
            expected("-UUU:---  F1  *scratch*      All   L1     (Fundamental) ")
        );
        harness.keys("x");
        assert_eq!(
            modeline(&harness),
            expected("-UUU:**-  F1  *scratch*      All   L1     (Fundamental) ")
        );
        harness.buffer_mut().read_only = true;
        harness.keys("C-f");
        assert_eq!(
            modeline(&harness),
            expected("-UUU:%*-  F1  *scratch*      All   L1     (Fundamental) ")
        );
        harness.buffer_mut().set_unmodified();
        harness.keys("C-n");
        assert_eq!(
            modeline(&harness),
            expected("-UUU:%%-  F1  *scratch*      All   L2     (Fundamental) ")
        );
    }
}

#[test]
fn server_client_frames_show_at_and_their_frame_number() {
    // emacsclient -t frames of a daemon, as `emacs --daemon' and two
    // `emacsclient -t' in tmux show them.
    let mut harness = Harness::with_session(Config::default(), Session::Server);
    harness.keys("C-f");
    assert_eq!(
        harness.screen()[22],
        modeline(
            "-UUU:@---  F2  *scratch*      All   L1     (Fundamental) ",
            80
        )
    );
    let second = ClientId(2);
    harness.handle(Event::ClientAttached {
        client: second,
        cols: 80,
        rows: 24,
        kind: ClientKind::Tui,
    });
    harness.handle(Event::Key(second, parse_kbd("x").expect("valid kbd")[0]));
    let (grid, _) = render(harness.editor(), second);
    let row: String = grid.row(22).iter().map(|cell| cell.text.as_str()).collect();
    assert_eq!(
        row,
        modeline(
            "-UUU:@**-  F3  *scratch*      All   L1     (Fundamental) ",
            80
        )
    );
}

#[test]
fn modeline_position_follows_emacs_percent_rules() {
    let mut text = String::from("|");
    for line in 0..100 {
        text.push_str(&format!("line {line}\n"));
    }
    let mut harness = sized(80, 10, &text);
    harness.buffer_mut().name = "a-very-long-buffer-name.txt".to_string();
    harness.keys("C-f C-b");
    assert_eq!(
        harness.screen()[10],
        modeline(
            "-UUU:---  F1  a-very-long-buffer-name.txt   Top   L1     (Fundamental) ",
            80
        )
    );
    // Start at line 8 (byte 56 of 790): (56 * 100 + 789) / 790 = 8.
    harness.keys("C-v");
    assert_eq!(
        harness.screen()[10],
        modeline(
            "-UUU:---  F1  a-very-long-buffer-name.txt    8%   L9     (Fundamental) ",
            80
        )
    );
    harness.keys("M->");
    assert_eq!(
        harness.screen()[10],
        modeline(
            "-UUU:---  F1  a-very-long-buffer-name.txt   Bot   L101   (Fundamental) ",
            80
        )
    );
}

#[test]
fn modeline_at_other_widths_and_wide_line_numbers() {
    let mut text = String::from("|");
    for line in 0..100 {
        text.push_str(&format!("line {line}\n"));
    }
    let mut harness = sized(40, 10, &text);
    harness.buffer_mut().name = "a-very-long-buffer-name.txt".to_string();
    harness.keys("C-v");
    assert_eq!(
        harness.screen()[10],
        "-UUU:---  F1  a-very-long-buffer-name.tx"
    );
    harness.resize(120, 12);
    harness.keys("M->");
    assert_eq!(
        harness.screen()[10],
        modeline(
            "-UUU:---  F1  a-very-long-buffer-name.txt   Bot   L101   (Fundamental) ",
            120
        )
    );
    // A line number wider than its min-width gets one more blank on a
    // terminal, as emacs -nw shows after M-> in 15000 lines.
    let mut harness = sized(80, 10, &format!("|{}", "x\n".repeat(15_000)));
    harness.keys("M->");
    assert_eq!(
        harness.screen()[10],
        modeline(
            "-UUU:---  F1  *scratch*      Bot   L15001   (Fundamental) ",
            80
        )
    );
}

#[test]
fn modeline_shows_unknown_line_number() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("unindexed.txt");
    fs::write(&path, b"one\ntwo\nthree\n").expect("write fixture");
    let cache = BlockCache::open(&path, BLOCK_SIZE).expect("open cache");
    let index = LineIndex::new(cache.len());
    let mut harness = Harness::new();
    harness.buffer_mut().text = Text::from_original(Sources::file(cache, index));
    harness.keys("C-n");
    let screen = harness.screen();
    assert_eq!(screen[0].trim_end(), "one");
    assert_eq!(
        screen[22],
        modeline(
            "-UUU:---  F1  *scratch*      All   L?     (Fundamental) ",
            80
        )
    );
}

#[test]
fn wrap_with_backslash() {
    // Captured from emacs -nw -Q in a 10-column terminal.
    let text = "|abcdefghij\nabcdefghijk\nabcdefgh\u{4e2d}Z\nabcdefg\u{4e2d}Z\n\t\tX\nabcdefghi\tXY\nabcdefghi";
    let mut harness = sized(10, 14, text);
    harness.keys("C-f C-b");
    let screen = harness.screen();
    let expected = [
        "abcdefghi\\",
        "j         ",
        "abcdefghi\\",
        "jk        ",
        "abcdefgh\\\\",
        "\u{4e2d}Z       ",
        "abcdefg\u{4e2d}\\",
        "Z         ",
        "         \\",
        "       X  ",
        "abcdefghi\\",
        "       XY ",
        "abcdefghi ",
        "          ",
    ];
    assert_eq!(&screen[..14], &expected);
    let grid = harness.grid();
    // emacs -nw draws the continuation glyph in the default face.
    assert_eq!(grid.row(0)[9].face, Face::Default);
    assert_eq!(grid.row(4)[8].text, "\\");
    assert_eq!(grid.row(5)[0].width, 2);
    assert_eq!(grid.row(5)[1].width, 0);
    harness.keys("M->");
    assert_eq!(
        harness.cursor(),
        Some((9, 12)),
        "a full row keeps EOL in its last column"
    );
    harness.keys("C-p C-p C-p C-e");
    assert_eq!(harness.cursor(), Some((1, 7)));
}

#[test]
fn control_char_rendered_escaped() {
    let mut harness = Harness::new();
    let bytes = b"a\x1b[31mb\x7f\x01\xff\xc2\x9bc\td\x00".to_vec();
    harness.buffer_mut().text = Text::from_original(Sources::memory(Arc::from(bytes)));
    harness.keys("C-e");
    assert_eq!(
        harness.screen()[0].trim_end(),
        "a^[[31mb^?^A\\377\\233c   d^@"
    );
    let grid = harness.grid();
    for cell in &grid.cells {
        assert!(
            cell.text.chars().all(|character| !character.is_control()),
            "raw control character in {:?}",
            cell.text
        );
    }
    let faces: Vec<Face> = grid.row(0)[..4].iter().map(|cell| cell.face).collect();
    assert_eq!(
        faces,
        [
            Face::Default,
            Face::EscapeGlyph,
            Face::EscapeGlyph,
            Face::Default
        ]
    );
    assert_eq!(harness.cursor(), Some((27, 0)));
}

#[test]
fn echo_area_shows_the_message() {
    let mut harness = Harness::with_text("|abc");
    harness.keys("C-c z");
    let echo = harness.screen()[23].clone();
    assert_eq!(echo, padded("C-c z is undefined", 80));
    harness.keys("C-b");
    assert!(harness.screen()[23].starts_with("Beginning of buffer"));
}

#[test]
fn region_uses_region_face_extended_to_the_edge() {
    let mut harness = sized(10, 4, "a|b\ncd\x1be\nfghijklmnopq\nxyz");
    harness.keys("C-SPC C-n C-n C-f C-f C-f");
    let grid = harness.grid();
    let faces = |row: u16| -> Vec<Face> { grid.row(row).iter().map(|cell| cell.face).collect() };
    let mut first = vec![Face::Region; 10];
    first[0] = Face::Default;
    assert_eq!(faces(0), first, "the newline's region extends to the edge");
    assert!(faces(1).iter().all(|face| *face == Face::Region));
    let mut third = vec![Face::Default; 10];
    third[..4].fill(Face::Region);

    assert_eq!(faces(2), third);
    assert_eq!(harness.cursor(), Some((4, 2)));
}

#[test]
fn render_1x1_does_not_panic() {
    let wide = "|\u{4e2d}\u{6587}\tab\x1b\ncd\nef";
    for (cols, rows) in [
        (1, 1),
        (1, 2),
        (2, 1),
        (2, 2),
        (1, 3),
        (2, 3),
        (3, 3),
        (1, 6),
    ] {
        let mut harness = Harness::with_text(wide);
        harness.resize(cols, rows);
        harness.keys("C-e C-n C-v M-v C-l C-l C-l M->");
        let screen = harness.screen();
        assert_eq!(screen.len(), usize::from(rows), "{cols}x{rows}");
        if let Some((col, row)) = harness.cursor() {
            assert!(
                col < cols && row < rows,
                "{cols}x{rows}: cursor ({col}, {row})"
            );
        }
    }
    let mut harness = Harness::with_text("|abc");
    harness.resize(1, 1);
    harness.keys("C-f");
    assert_eq!(harness.screen(), vec![" ".to_string()]);
    harness.keys("C-f C-f C-f");
    assert_eq!(
        harness.screen(),
        vec!["E".to_string()],
        "the echo row comes first"
    );
    // A one-row window has no modeline in Emacs (`window_wants_mode_line`):
    // the row shows text.
    harness.resize(4, 2);
    assert_eq!(
        harness.screen(),
        vec!["abc ".to_string(), "End ".to_string()]
    );
}

#[test]
fn render_zero_cols_is_error_free() {
    for (cols, rows) in [(0, 0), (0, 5), (5, 0), (0, 1)] {
        let mut harness = Harness::with_text("|abc\ndef");
        harness.resize(cols, rows);
        harness.keys("C-n C-v M-v C-l a");
        assert_eq!(harness.text().len(), 8, "{cols}x{rows}");
        let screen = harness.screen();
        assert_eq!(screen.len(), usize::from(rows));
        assert!(screen.iter().all(|row| row.is_empty() || cols > 0));
        assert_eq!(harness.cursor(), None);
    }
}

#[test]
fn c_v_scrolls_by_height_minus_2() {
    let mut harness = sized(20, 10, &forty_lines());
    harness.keys("C-v");
    assert_eq!(top_line(&harness), "L08");
    assert_eq!(harness.point(), 32);
    assert_eq!(harness.cursor(), Some((0, 0)));
    harness.keys("C-v C-v C-v");
    assert_eq!(top_line(&harness), "L32");
    harness.keys("C-v");
    assert!(harness.signaled());
    assert_eq!(harness.echo(), "End of buffer");
    assert_eq!(
        top_line(&harness),
        "L32",
        "a failed scroll leaves the window"
    );

    let mut harness = sized(20, 10, &forty_lines());
    harness.keys("M-v");
    assert_eq!(harness.echo(), "Beginning of buffer");
    harness.keys("C-u C-v");
    assert_eq!(
        top_line(&harness),
        "L04",
        "an argument scrolls that many lines"
    );
    harness.keys("M-- C-v");
    assert_eq!(
        top_line(&harness),
        "L00",
        "a bare minus scrolls back a screen"
    );
    harness.keys("C-v C-n C-n C-n C-n C-n C-n C-n C-n M-v");
    // window_scroll_line_based: point below the new window goes to its
    // last line, as emacs -nw shows.
    assert_eq!(top_line(&harness), "L00");
    assert_eq!(harness.cursor(), Some((0, 9)));
    assert_eq!(harness.point(), 36);
    harness.keys("C-v C-p M-v");
    assert_eq!(
        harness.cursor(),
        Some((0, 8)),
        "point already visible stays"
    );

    let config = Config {
        next_screen_context_lines: 0,
        ..Config::default()
    };
    let mut harness = Harness::with_config(config);
    harness.set_text(&forty_lines());
    harness.resize(20, 12);
    harness.keys("C-v");
    assert_eq!(top_line(&harness), "L10");
}

#[test]
fn scrolling_counts_screen_lines_of_wrapped_text() {
    let mut text = String::from("|");
    text.push_str(&"x".repeat(45));
    text.push('\n');
    for line in 0..20 {
        text.push_str(&format!("L{line:02}\n"));
    }
    // 45 characters take five rows of a 10-column window.
    let mut harness = sized(10, 4, &text);
    harness.keys("C-v");
    assert_eq!(top_line(&harness), "xxxxxxxxx\\");
    assert_eq!(harness.point(), 18);
    harness.keys("C-v");
    assert_eq!(top_line(&harness), "xxxxxxxxx");
    harness.keys("C-v");
    assert_eq!(top_line(&harness), "L01");
    harness.keys("M-v M-v");
    assert_eq!(top_line(&harness), "xxxxxxxxx\\");
}

#[test]
fn c_l_cycles_middle_top_bottom() {
    let mut harness = sized(20, 10, &forty_lines());
    harness.keys("C-u 2 5 C-n");
    assert_eq!(
        harness.cursor(),
        Some((0, 5)),
        "recentered when C-n left the window"
    );
    harness.keys("C-l");
    assert_eq!(top_line(&harness), "L20");
    assert_eq!(harness.cursor(), Some((0, 5)));
    harness.keys("C-l");
    assert_eq!(top_line(&harness), "L25");
    assert_eq!(harness.cursor(), Some((0, 0)));
    harness.keys("C-l");
    assert_eq!(top_line(&harness), "L16");
    assert_eq!(harness.cursor(), Some((0, 9)));
    harness.keys("C-l");
    assert_eq!(harness.cursor(), Some((0, 5)));
    harness.keys("C-f C-b C-l");
    assert_eq!(
        harness.cursor(),
        Some((0, 5)),
        "the cycle restarts at middle"
    );

    harness.keys("C-u 3 C-l");
    assert_eq!(harness.cursor(), Some((0, 3)));
    harness.keys("M-- C-l");
    assert_eq!(harness.cursor(), Some((0, 9)));
    harness.keys("C-u - 3 C-l");
    assert_eq!(harness.cursor(), Some((0, 7)));
    harness.keys("C-u 9 9 C-l");
    assert_eq!(harness.cursor(), Some((0, 9)), "clipped to the window");
    harness.keys("C-u C-l");
    assert_eq!(harness.cursor(), Some((0, 5)));
    // recenter-top-bottom with an argument leaves recenter-last-op alone,
    // so the cycle goes on from the last bare C-l (middle).
    harness.keys("C-l");
    assert_eq!(harness.cursor(), Some((0, 0)));
    harness.keys("C-l");
    assert_eq!(harness.cursor(), Some((0, 9)));
}

#[test]
fn c_l_middle_redraws_the_whole_frame() {
    let mut harness = sized(20, 10, &forty_lines());
    harness.keys("C-n");
    harness.take_sent();
    harness.keys("C-l");
    let sent = harness.take_sent();
    let frames = frames(&sent);
    assert_eq!(frames.len(), 1);
    assert!(matches!(frames[0], ServerMsg::Frame { full: true, .. }));
}

#[test]
fn point_moving_offscreen_recenters() {
    let mut harness = sized(20, 10, &forty_lines());
    harness.keys("C-u 1 0 C-n");
    assert_eq!(top_line(&harness), "L05");
    assert_eq!(harness.cursor(), Some((0, 5)));
    harness.keys("M-< C-v C-p");
    assert_eq!(top_line(&harness), "L02");
    assert_eq!(harness.cursor(), Some((0, 5)));
    harness.keys("M->");
    // end-of-buffer recenters with -3 when the end is off screen.
    assert_eq!(top_line(&harness), "L33");
    assert_eq!(harness.cursor(), Some((0, 7)));
}

#[test]
fn resize_keeps_point_visible() {
    let mut harness = Harness::with_text(&forty_lines());
    harness.keys("C-u 2 0 C-n");
    assert_eq!(harness.cursor(), Some((0, 20)));
    harness.resize(20, 12);
    assert_eq!(top_line(&harness), "L15");
    assert_eq!(harness.cursor(), Some((0, 5)));
}

#[test]
fn window_start_left_mid_line_by_another_client_recenters() {
    let mut harness = sized(20, 10, &forty_lines());
    harness.keys("C-u 3 0 C-n");
    assert_eq!(top_line(&harness), "L25");
    let other = ClientId(2);
    harness.handle(Event::ClientAttached {
        client: other,
        cols: 20,
        rows: 12,
        kind: ClientKind::Tui,
    });
    // Deleting "00\nL01...L25" leaves this window's start marker inside
    // "L05"; redisplay recenters on point, as Emacs does when a window
    // start that was at a line beginning no longer is.
    for key in parse_kbd("C-f C-f C-u 1 0 0 C-d").expect("valid kbd") {
        harness.handle(Event::Key(other, key));
    }
    assert!(harness.text().starts_with("L05\nL26\n"));
    assert_eq!(top_line(&harness), "L05");
    assert_eq!(harness.cursor(), Some((0, 5)));
}

#[test]
fn frames_are_sent_only_when_the_screen_changes() {
    let mut harness = Harness::with_text("|abc");
    harness.take_sent();
    harness.keys("C-e");
    let sent = harness.take_sent();
    let frames = frames(&sent);
    assert_eq!(frames.len(), 1);
    harness.keys("C-e");
    assert!(
        self::frames(&harness.take_sent()).is_empty(),
        "an unchanged screen sends nothing"
    );
    harness.keys("x");
    let sent = harness.take_sent();
    match self::frames(&sent).as_slice() {
        [
            ServerMsg::Frame {
                full: false,
                rows_changed,
                cursor,
                ..
            },
        ] => {
            let rows: Vec<u16> = rows_changed.iter().map(|update| update.row).collect();
            assert_eq!(rows, [0, 22], "the text row and the modeline");
            assert_eq!(*cursor, Some((4, 0)));
        }
        other => panic!("expected one incremental frame, got {other:?}"),
    }
    harness.resize(30, 5);
    let sent = harness.take_sent();
    assert!(matches!(
        self::frames(&sent).as_slice(),
        [ServerMsg::Frame {
            full: true,
            cols: 30,
            rows: 5,
            ..
        }]
    ));
}

#[test]
fn notcached_block_requests_prefetch() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("big.txt");
    let mut content = Vec::new();
    while content.len() < 2 * BLOCK_SIZE + 100 {
        content.extend_from_slice(b"0123456789abcdef\n");
    }
    fs::write(&path, &content).expect("write fixture");
    let cache = BlockCache::open(&path, 2 * BLOCK_SIZE).expect("open cache");
    let index = LineIndex::new(cache.len());
    index_file(&cache, &index, &AtomicBool::new(false), &mut |_| {}).expect("index fixture");
    let text = Text::from_original(Sources::file(cache, index));

    let mut harness = Harness::new();
    harness.buffer_mut().text = text.clone();
    let buffer = harness.buffer().id;
    let (grid, prefetch) = render(harness.editor(), harness.client());
    assert_eq!(prefetch, vec![PrefetchRequest { buffer, block: 0 }]);
    assert!(
        grid.row(0).iter().all(|cell| cell.text == " "),
        "uncached text renders blank"
    );

    let mut warm = Vec::new();
    text.read(0..1, &mut warm).expect("read block 0");
    let (grid, prefetch) = render(harness.editor(), harness.client());
    assert!(prefetch.is_empty());
    let first: String = grid.row(0).iter().map(|cell| cell.text.as_str()).collect();
    assert_eq!(first.trim_end(), "0123456789abcdef");

    harness.keys("C-n");
    assert!(harness.take_prefetches().is_empty());
}

#[test]
fn zero_width_and_format_characters_render_as_one_blank() {
    let mut characters: Vec<char> = vec!['\u{200b}', '\u{feff}', '\u{2028}', '\u{2029}'];
    characters.extend('\u{200c}'..='\u{200f}');
    characters.extend('\u{202a}'..='\u{202e}');
    characters.extend('\u{2060}'..='\u{2064}');
    characters.extend('\u{2066}'..='\u{2069}');
    characters.extend([
        '\u{ad}',
        '\u{61c}',
        '\u{fff9}',
        '\u{e0001}',
        '\u{301}',
        '\u{34f}',
    ]);
    for character in characters {
        let mut harness = Harness::with_text(&format!("|{character}x"));
        harness.keys("C-f");
        let grid = harness.grid();
        let label = format!("U+{:04X}", u32::from(character));
        assert_eq!(grid.row(0)[0].text, " ", "{label}");
        assert_eq!(grid.row(0)[0].face, Face::Default, "{label}");
        assert_eq!(grid.row(0)[1].text, "x", "{label}");
        assert_eq!(harness.cursor(), Some((1, 0)), "{label}: no cursor drift");
        assert!(
            grid.cells.iter().all(|cell| !cell.text.contains(character)),
            "{label} reached a cell raw"
        );
    }
    // A non-joiner after a letter joins its cluster but is not sent.
    let mut harness = Harness::with_text("|a\u{200c}b");
    harness.keys("C-e");
    let grid = harness.grid();
    assert_eq!(grid.row(0)[0].text, "a");
    assert_eq!(grid.row(0)[1].text, "b");
    assert_eq!(harness.cursor(), Some((2, 0)));
}
