//! Turns `Grid` frames into terminal output, writing only cells that changed
//! since the previous frame.
//!
//! The cursor is positioned explicitly at the start of every run of changed
//! cells and again after any cell that is not a single printable ASCII byte,
//! so a terminal that measures a character's width differently from us can
//! never shift the cells that follow it.

use emx_types::{Cell, Face, Grid};

const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
const CLEAR_SCREEN: &[u8] = b"\x1b[2J";
const REPLACEMENT: char = '?';
const WIDE: u8 = 2;
const TRAILING_HALF: u8 = 0;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScreenError {
    #[error("malformed grid: {0}")]
    MalformedGrid(String),
}

/// SGR parameters for each face in the fixed default theme. Every entry
/// starts with `0` so a face never inherits attributes from the previous one.
/// Colors are the 16 ANSI colors so the user's terminal palette applies;
/// they approximate Emacs `-nw` defaults.
pub fn face_sgr(face: Face) -> &'static str {
    match face {
        Face::Default => "0",
        Face::ModeLine => "0;7",
        Face::ModeLineInactive => "0;2;7",
        // Emacs draws `header-line` underlined on a terminal.
        Face::HeaderLine => "0;4",
        Face::Region => "0;97;44",
        Face::IsearchMatch => "0;97;45",
        Face::LazyHighlight => "0;30;46",
        Face::MinibufferPrompt => "0;36",
        Face::EscapeGlyph => "0;33",
        Face::Error => "0;1;31",
        Face::Warning => "0;1;33",
        Face::ChangedMarker => "0;35",
    }
}

#[derive(Debug, Default)]
pub struct Screen {
    current: Option<Grid>,
}

impl Screen {
    pub fn new() -> Screen {
        Screen::default()
    }

    /// Forgets what the terminal shows, so the next draw repaints everything;
    /// for when something else may have written to the terminal.
    pub fn invalidate(&mut self) {
        self.current = None;
    }

    /// Appends the output that turns the previous frame into `next`. A
    /// malformed grid appends nothing and leaves the screen state unchanged.
    pub fn draw(&mut self, next: &Grid, out: &mut Vec<u8>) -> Result<(), ScreenError> {
        validate(next)?;
        let previous = self
            .current
            .as_ref()
            .filter(|current| current.cols == next.cols && current.rows == next.rows);
        out.extend_from_slice(HIDE_CURSOR);
        if previous.is_none() {
            push_sgr(out, Face::Default);
            out.extend_from_slice(CLEAR_SCREEN);
        }
        let mut last_face = None;
        for row in 0..next.rows {
            let cells = next.row(row);
            let changed = match previous {
                Some(previous) => changed_columns(previous.row(row), cells),
                None => vec![true; cells.len()],
            };
            draw_row(row, cells, &changed, &mut last_face, out);
        }
        if let Some((col, row)) = next.cursor {
            push_goto(out, col, row);
            out.extend_from_slice(SHOW_CURSOR);
        }
        self.current = Some(next.clone());
        Ok(())
    }
}

fn validate(grid: &Grid) -> Result<(), ScreenError> {
    let malformed = |message: String| Err(ScreenError::MalformedGrid(message));
    let expected = usize::from(grid.cols) * usize::from(grid.rows);
    if grid.cells.len() != expected {
        return malformed(format!(
            "{} cells for a {}x{} grid",
            grid.cells.len(),
            grid.cols,
            grid.rows
        ));
    }
    for row in 0..grid.rows {
        let cells = grid.row(row);
        for (col, cell) in cells.iter().enumerate() {
            let follows_wide = col > 0 && cells[col - 1].width == WIDE;
            if cell.width > WIDE {
                return malformed(format!("cell width {} at ({col}, {row})", cell.width));
            }
            if follows_wide != (cell.width == TRAILING_HALF) {
                return malformed(format!(
                    "wide lead and trailing half do not pair at ({col}, {row})"
                ));
            }
            if cell.width == WIDE && col + 1 == cells.len() {
                return malformed(format!("wide cell at ({col}, {row}) in the last column"));
            }
        }
    }
    if let Some((col, row)) = grid.cursor
        && (col >= grid.cols || row >= grid.rows)
    {
        return malformed(format!(
            "cursor ({col}, {row}) outside {}x{} grid",
            grid.cols, grid.rows
        ));
    }
    Ok(())
}

/// Columns to redraw: cells that differ, widened so a wide character is
/// always redrawn whole and anything a vanished wide character covered is
/// redrawn too.
fn changed_columns(previous: &[Cell], next: &[Cell]) -> Vec<bool> {
    let mut changed: Vec<bool> = previous
        .iter()
        .zip(next)
        .map(|(old, new)| old != new)
        .collect();
    let mut grew = true;
    while grew {
        grew = false;
        for col in 0..changed.len() {
            if !changed[col] {
                continue;
            }
            let trailing = previous[col].width == TRAILING_HALF || next[col].width == TRAILING_HALF;
            let wide = previous[col].width == WIDE || next[col].width == WIDE;
            if trailing && !changed[col - 1] {
                changed[col - 1] = true;
                grew = true;
            }
            if wide && !changed[col + 1] {
                changed[col + 1] = true;
                grew = true;
            }
        }
    }
    changed
}

fn draw_row(
    row: u16,
    cells: &[Cell],
    changed: &[bool],
    last_face: &mut Option<Face>,
    out: &mut Vec<u8>,
) {
    let mut positioned = false;
    for (col, cell) in cells.iter().enumerate() {
        if !changed[col] {
            positioned = false;
            continue;
        }
        if cell.width == TRAILING_HALF {
            continue;
        }
        if !positioned {
            let col = u16::try_from(col).expect("grid columns fit in u16");
            push_goto(out, col, row);
        }
        if *last_face != Some(cell.face) {
            push_sgr(out, cell.face);
            *last_face = Some(cell.face);
        }
        positioned = push_cell_text(out, cell);
    }
}

/// Returns whether the terminal cursor is known to have advanced exactly one
/// column, which only a single printable ASCII byte guarantees.
fn push_cell_text(out: &mut Vec<u8>, cell: &Cell) -> bool {
    let safe = !cell.text.is_empty() && !cell.text.chars().any(char::is_control);
    if !safe {
        for _ in 0..cell.width {
            let mut buffer = [0; 4];
            out.extend_from_slice(REPLACEMENT.encode_utf8(&mut buffer).as_bytes());
        }
        return cell.width == 1;
    }
    out.extend_from_slice(cell.text.as_bytes());
    cell.width == 1 && cell.text.len() == 1
}

fn push_goto(out: &mut Vec<u8>, col: u16, row: u16) {
    out.extend_from_slice(
        format!("\x1b[{};{}H", u32::from(row) + 1, u32::from(col) + 1).as_bytes(),
    );
}

fn push_sgr(out: &mut Vec<u8>, face: Face) {
    out.extend_from_slice(format!("\x1b[{}m", face_sgr(face)).as_bytes());
}
