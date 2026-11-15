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
    /// A directory in the file panel.
    PanelDirectory,
    /// Git status in the file panel: modified, added, untracked, and an
    /// ignored entry shown on request.
    GitModified,
    GitAdded,
    GitUntracked,
    GitIgnored,
    /// Syntax highlighting, after Emacs's font-lock faces.
    Comment,
    String,
    Keyword,
    Type,
    Function,
    Variable,
    Constant,
    Builtin,
    Preprocessor,
    /// Emacs `show-paren-match` and `show-paren-mismatch`.
    ShowParenMatch,
    ShowParenMismatch,
    /// Text colored by ANSI SGR codes (`ansi-color-mode`): the 16 colors
    /// 0-15, or 16 for the terminal's default.
    Ansi {
        foreground: u8,
        background: u8,
        bold: bool,
    },
}

impl Face {
    pub const ALL: [Face; 29] = [
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
        Face::CompletionSelected,
        Face::PanelDirectory,
        Face::GitModified,
        Face::GitAdded,
        Face::GitUntracked,
        Face::GitIgnored,
        Face::Comment,
        Face::String,
        Face::Keyword,
        Face::Type,
        Face::Function,
        Face::Variable,
        Face::Constant,
        Face::Builtin,
        Face::Preprocessor,
        Face::ShowParenMatch,
        Face::ShowParenMismatch,
    ];

    /// The face's name in the config's `[faces]` table.
    pub fn name(self) -> &'static str {
        match self {
            Face::Default => "default",
            Face::ModeLine => "mode_line",
            Face::ModeLineInactive => "mode_line_inactive",
            Face::HeaderLine => "header_line",
            Face::Region => "region",
            Face::IsearchMatch => "isearch_match",
            Face::LazyHighlight => "lazy_highlight",
            Face::MinibufferPrompt => "minibuffer_prompt",
            Face::EscapeGlyph => "escape_glyph",
            Face::Error => "error",
            Face::Warning => "warning",
            Face::ChangedMarker => "changed_marker",
            Face::CompletionSelected => "completion_selected",
            Face::PanelDirectory => "panel_directory",
            Face::GitModified => "git_modified",
            Face::GitAdded => "git_added",
            Face::GitUntracked => "git_untracked",
            Face::GitIgnored => "git_ignored",
            Face::Comment => "comment",
            Face::String => "string",
            Face::Keyword => "keyword",
            Face::Type => "type",
            Face::Function => "function",
            Face::Variable => "variable",
            Face::Constant => "constant",
            Face::Builtin => "builtin",
            Face::Preprocessor => "preprocessor",
            Face::ShowParenMatch => "show_paren_match",
            Face::ShowParenMismatch => "show_paren_mismatch",
            Face::Ansi { .. } => "ansi",
        }
    }

    pub fn from_name(name: &str) -> Option<Face> {
        Face::ALL.into_iter().find(|face| face.name() == name)
    }
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
    fn face_names_round_trip() {
        for face in Face::ALL {
            assert_eq!(Face::from_name(face.name()), Some(face));
        }
        assert_eq!(Face::Keyword.name(), "keyword");
        assert_eq!(Face::ModeLineInactive.name(), "mode_line_inactive");
        assert_eq!(Face::from_name("nope"), None);
    }

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
