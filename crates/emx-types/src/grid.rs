use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Face {
    #[default]
    Default,
    ModeLine,
    ModeLineInactive,
    /// A window's header line, as the Buffer Menu's column titles.
    HeaderLine,
    Region,
    IsearchMatch,
    LazyHighlight,
    MinibufferPrompt,
    EscapeGlyph,
    Error,
    Warning,
    ChangedMarker,
    /// The selected completion candidate (Emacs `icomplete-selected-match`).
    CompletionSelected,
}

/// A width of 0 marks the trailing half of a wide character.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub text: String,
    pub width: u8,
    pub face: Face,
}

impl Cell {
    pub fn blank() -> Cell {
        Cell {
            text: " ".to_string(),
            width: 1,
            face: Face::Default,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grid {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<Cell>,
    pub cursor: Option<(u16, u16)>,
}

impl Grid {
    pub fn new(cols: u16, rows: u16) -> Grid {
        Grid {
            cols,
            rows,
            cells: vec![Cell::blank(); usize::from(cols) * usize::from(rows)],
            cursor: None,
        }
    }

    pub fn set(&mut self, col: u16, row: u16, cell: Cell) {
        assert!(
            col < self.cols && row < self.rows,
            "cell ({col}, {row}) outside {}x{} grid",
            self.cols,
            self.rows
        );
        let index = usize::from(row) * usize::from(self.cols) + usize::from(col);
        self.cells[index] = cell;
    }

    pub fn row(&self, row: u16) -> &[Cell] {
        assert!(row < self.rows, "row {row} outside {} rows", self.rows);
        let width = usize::from(self.cols);
        let start = usize::from(row) * width;
        &self.cells[start..start + width]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_grid_is_blank() {
        let grid = Grid::new(3, 2);
        assert_eq!(grid.cells.len(), 6);
        assert_eq!(grid.row(1).len(), 3);
        assert_eq!(grid.cursor, None);
        assert!(
            grid.cells
                .iter()
                .all(|cell| cell.text == " " && cell.width == 1)
        );
    }

    #[test]
    fn set_writes_row_major() {
        let mut grid = Grid::new(3, 2);
        let cell = Cell {
            text: "x".into(),
            width: 1,
            face: Face::Region,
        };
        grid.set(2, 1, cell.clone());
        assert_eq!(grid.row(1)[2], cell);
        assert_eq!(grid.row(0)[2].face, Face::Default);
    }

    #[test]
    #[should_panic]
    fn set_out_of_bounds_panics() {
        Grid::new(2, 2).set(2, 0, Cell::blank());
    }

    #[test]
    fn grid_serde_roundtrip() {
        let mut grid = Grid::new(2, 1);
        grid.set(
            0,
            0,
            Cell {
                text: "é".into(),
                width: 1,
                face: Face::ModeLine,
            },
        );
        grid.cursor = Some((1, 0));
        let json = serde_json::to_string(&grid).unwrap();
        assert_eq!(serde_json::from_str::<Grid>(&json).unwrap(), grid);
    }
}
