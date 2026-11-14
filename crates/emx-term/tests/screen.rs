use emx_term::{Screen, ScreenError, face_sgr};
use emx_types::{Cell, Face, Grid};

const HIDE: &str = "\x1b[?25l";
const SHOW: &str = "\x1b[?25h";
const CLEAR: &str = "\x1b[2J";

fn cell(text: &str, width: u8, face: Face) -> Cell {
    Cell {
        text: text.to_string(),
        width,
        face,
    }
}

fn grid_from(lines: &[&str]) -> Grid {
    let cols = lines
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0) as u16;
    let mut grid = Grid::new(cols, lines.len() as u16);
    for (row, line) in lines.iter().enumerate() {
        for (col, character) in line.chars().enumerate() {
            grid.set(
                col as u16,
                row as u16,
                cell(&character.to_string(), 1, Face::Default),
            );
        }
    }
    grid
}

fn draw(screen: &mut Screen, grid: &Grid) -> String {
    let mut out = Vec::new();
    screen.draw(grid, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

fn goto(col: u16, row: u16) -> String {
    format!("\x1b[{};{}H", row + 1, col + 1)
}

fn sgr(face: Face) -> String {
    format!("\x1b[{}m", face_sgr(face))
}

#[test]
fn screen_first_draw_clears_and_paints_everything() {
    let mut grid = grid_from(&["ab", "cd"]);
    grid.cursor = Some((1, 1));
    let output = draw(&mut Screen::new(), &grid);
    let expected = format!(
        "{HIDE}{}{CLEAR}{}{}ab{}cd{}{SHOW}",
        sgr(Face::Default),
        goto(0, 0),
        sgr(Face::Default),
        goto(0, 1),
        goto(1, 1)
    );
    assert_eq!(output, expected);
}

#[test]
fn screen_diff_minimal() {
    let mut screen = Screen::new();
    let mut grid = grid_from(&["hello", "world"]);
    grid.cursor = Some((2, 0));
    draw(&mut screen, &grid);
    assert_eq!(
        draw(&mut screen, &grid),
        format!("{HIDE}{}{SHOW}", goto(2, 0))
    );
    grid.cursor = None;
    assert_eq!(draw(&mut screen, &grid), HIDE);
}

#[test]
fn screen_diff_emits_only_changed_runs() {
    let mut screen = Screen::new();
    let grid = grid_from(&["hello", "world"]);
    draw(&mut screen, &grid);
    let next = grid_from(&["hEllO", "world"]);
    assert_eq!(
        draw(&mut screen, &next),
        format!("{HIDE}{}{}E{}O", goto(1, 0), sgr(Face::Default), goto(4, 0))
    );
    let mut faced = next.clone();
    faced.set(1, 1, cell("o", 1, Face::Region));
    faced.set(2, 1, cell("R", 1, Face::Region));
    assert_eq!(
        draw(&mut screen, &faced),
        format!("{HIDE}{}{}oR", goto(1, 1), sgr(Face::Region))
    );
}

#[test]
fn screen_size_change_triggers_full_redraw() {
    let mut screen = Screen::new();
    draw(&mut screen, &grid_from(&["ab", "cd"]));
    let output = draw(&mut screen, &grid_from(&["ab", "cd", "ef"]));
    assert!(output.contains(CLEAR));
    assert!(output.contains("ab") && output.contains("cd") && output.contains("ef"));
    let output = draw(&mut screen, &grid_from(&["abc", "cde", "efg"]));
    assert!(output.contains(CLEAR));
}

#[test]
fn screen_wide_char_emitted_once_and_next_cell_positioned() {
    let mut grid = Grid::new(4, 1);
    grid.set(0, 0, cell("中", 2, Face::Default));
    grid.set(1, 0, cell("", 0, Face::Default));
    grid.set(2, 0, cell("x", 1, Face::Default));
    grid.set(3, 0, cell("y", 1, Face::Default));
    let output = draw(&mut Screen::new(), &grid);
    let body = format!("{}{}中{}xy", goto(0, 0), sgr(Face::Default), goto(2, 0));
    assert!(output.ends_with(&body), "{output:?}");
    assert_eq!(output.matches('中').count(), 1);
}

#[test]
fn screen_repositions_after_every_non_ascii_cell() {
    let grid = grid_from(&["éa€b"]);
    let output = draw(&mut Screen::new(), &grid);
    let body = format!(
        "{}{}é{}a€{}b",
        goto(0, 0),
        sgr(Face::Default),
        goto(1, 0),
        goto(3, 0)
    );
    assert!(output.ends_with(&body), "{output:?}");
}

#[test]
fn screen_wide_char_change_redraws_both_halves_and_neighbours() {
    let mut screen = Screen::new();
    let mut wide = Grid::new(3, 1);
    wide.set(0, 0, cell("中", 2, Face::Default));
    wide.set(1, 0, cell("", 0, Face::Default));
    wide.set(2, 0, cell("z", 1, Face::Default));
    draw(&mut screen, &wide);
    let mut narrow = Grid::new(3, 1);
    narrow.set(0, 0, cell("a", 1, Face::Default));
    narrow.set(1, 0, cell("b", 1, Face::Default));
    narrow.set(2, 0, cell("z", 1, Face::Default));
    assert_eq!(
        draw(&mut screen, &narrow),
        format!("{HIDE}{}{}ab", goto(0, 0), sgr(Face::Default))
    );
    let mut half = narrow.clone();
    half.set(1, 0, cell("中", 2, Face::Default));
    half.set(2, 0, cell("", 0, Face::Default));
    assert_eq!(
        draw(&mut screen, &half),
        format!("{HIDE}{}{}中", goto(1, 0), sgr(Face::Default))
    );
    let mut replaced = half.clone();
    replaced.set(1, 0, cell("x", 1, Face::Default));
    replaced.set(2, 0, cell("y", 1, Face::Default));
    assert_eq!(
        draw(&mut screen, &replaced),
        format!("{HIDE}{}{}xy", goto(1, 0), sgr(Face::Default))
    );
}

#[test]
fn screen_trailing_half_change_redraws_lead() {
    let mut screen = Screen::new();
    let mut first = Grid::new(3, 1);
    first.set(0, 0, cell("中", 2, Face::Default));
    first.set(1, 0, cell("", 0, Face::Default));
    draw(&mut screen, &first);
    let mut second = first.clone();
    second.set(1, 0, cell("", 0, Face::Region));
    assert_eq!(
        draw(&mut screen, &second),
        format!("{HIDE}{}{}中", goto(0, 0), sgr(Face::Default))
    );
}

fn assert_only_screen_escapes(output: &[u8]) {
    let allowed_sgr: Vec<String> = [
        Face::Default,
        Face::ModeLine,
        Face::ModeLineInactive,
        Face::HeaderLine,
        Face::Region,
        Face::IsearchMatch,
        Face::LazyHighlight,
        Face::MinibufferPrompt,
        Face::EscapeGlyph,
        Face::Error,
        Face::Warning,
        Face::ChangedMarker,
    ]
    .iter()
    .map(|face| face_sgr(*face).to_string())
    .collect();
    let text = std::str::from_utf8(output).expect("screen output is UTF-8");
    let mut rest = text;
    while let Some(start) = rest.find(|character: char| character.is_control()) {
        let tail = &rest[start..];
        assert!(
            tail.starts_with("\x1b["),
            "raw control in output at {tail:?}"
        );
        let body = &tail[2..];
        let end = body
            .find(|character: char| ('@'..='~').contains(&character))
            .expect("terminated CSI");
        let (params, final_byte) = (&body[..end], &body[end..end + 1]);
        match final_byte {
            "m" => assert!(
                allowed_sgr.iter().any(|sgr| sgr == params),
                "foreign SGR {params:?}"
            ),
            "H" => assert!(params.split(';').all(|part| part.parse::<u16>().is_ok())),
            "J" => assert_eq!(params, "2"),
            "l" | "h" => assert_eq!(params, "?25"),
            other => panic!("foreign CSI final {other:?}"),
        }
        rest = &body[end + 1..];
    }
}

#[test]
fn screen_never_emits_raw_control_from_cells() {
    let hostile = [
        "\x1b[31m", "\x1b", "\x07", "\x7f", "\u{9b}", "\u{85}", "a\x08", "\r", "\n", "\0", "",
        "\u{9b}2J",
    ];
    let mut grid = Grid::new(hostile.len() as u16 + 2, 1);
    for (col, text) in hostile.iter().enumerate() {
        grid.set(col as u16, 0, cell(text, 1, Face::Default));
    }
    grid.set(hostile.len() as u16, 0, cell("\x1b]", 2, Face::Default));
    grid.set(hostile.len() as u16 + 1, 0, cell("", 0, Face::Default));
    grid.cursor = Some((0, 0));
    let mut out = Vec::new();
    Screen::new().draw(&grid, &mut out).unwrap();
    assert_only_screen_escapes(&out);
    let text = String::from_utf8(out)
        .unwrap()
        .replace(HIDE, "")
        .replace(SHOW, "");
    assert_eq!(text.matches('?').count(), hostile.len() + 2);
    assert!(!text.contains("31m") && !text.contains("2J?") && !text.contains("?2J"));
}

#[test]
fn screen_output_for_every_face_is_well_formed() {
    let faces = [
        Face::Default,
        Face::ModeLine,
        Face::ModeLineInactive,
        Face::HeaderLine,
        Face::Region,
        Face::IsearchMatch,
        Face::LazyHighlight,
        Face::MinibufferPrompt,
        Face::EscapeGlyph,
        Face::Error,
        Face::Warning,
        Face::ChangedMarker,
    ];
    let mut grid = Grid::new(faces.len() as u16, 1);
    for (col, face) in faces.iter().enumerate() {
        grid.set(col as u16, 0, cell("x", 1, *face));
    }
    let mut out = Vec::new();
    Screen::new().draw(&grid, &mut out).unwrap();
    assert_only_screen_escapes(&out);
    for face in faces {
        assert!(face_sgr(face).starts_with('0'), "{face:?} SGR resets first");
    }
}

#[test]
fn screen_rejects_malformed_grids_without_output_or_state_change() {
    let mut screen = Screen::new();
    let good = grid_from(&["ab"]);
    draw(&mut screen, &good);

    let mut short = good.clone();
    short.cells.pop();
    let mut lone_trailing = good.clone();
    lone_trailing.cells[0] = cell("", 0, Face::Default);
    let mut wide_at_edge = good.clone();
    wide_at_edge.cells[1] = cell("中", 2, Face::Default);
    let mut wide_without_trailing = good.clone();
    wide_without_trailing.cells[0] = cell("中", 2, Face::Default);
    let mut too_wide = good.clone();
    too_wide.cells[0] = cell("x", 3, Face::Default);
    let mut cursor_outside = good.clone();
    cursor_outside.cursor = Some((2, 0));

    for bad in [
        short,
        lone_trailing,
        wide_at_edge,
        wide_without_trailing,
        too_wide,
        cursor_outside,
    ] {
        let mut out = Vec::new();
        assert!(
            matches!(
                screen.draw(&bad, &mut out),
                Err(ScreenError::MalformedGrid(_))
            ),
            "{bad:?}"
        );
        assert!(out.is_empty());
    }
    assert_eq!(draw(&mut screen, &good), HIDE);
}

#[test]
fn screen_invalidate_forces_full_redraw() {
    let mut screen = Screen::new();
    let grid = grid_from(&["ab"]);
    draw(&mut screen, &grid);
    screen.invalidate();
    assert!(draw(&mut screen, &grid).contains(CLEAR));
}
