//! Draws a client's frame into a `Grid`: each window's text and modeline,
//! then the echo area. Rendering reads only cached bytes, so it never
//! blocks on I/O; text that is not cached yet draws blank and is returned
//! as a prefetch request.

use std::ops::{ControlFlow, Range};

use emx_text::search::matches_in_cached;
use emx_text::{DisplayUnit, display_units, unit_width};
use emx_types::{Cell, Face, Grid};

use crate::buffer::{Buffer, BufferId, FileBinding};
use crate::editor::Editor;
use crate::event::ClientId;
use crate::faces::FaceSpans;
use crate::paren::{ParenMatch, show_paren};
use crate::reload::ExternalState;
use crate::window::{
    CONTINUATION_GLYPH, CursorTracker, Fetch, LineEvent, ScreenLine, TRUNCATION_GLYPH, WindowArea,
    WindowId, frame_layout, not_cached_block, unit_glyphs,
};

/// `mode-line-front-space` on a terminal.
const MODELINE_FRONT: &str = "-";
/// `mode-line-mule-info` for UTF-8 keyboard and terminal coding; the
/// buffer's coding letter and the Unix end-of-line `:` follow.
const MODELINE_TERMINAL_CODING: &str = "UU";
const MODELINE_UNIX_EOL: &str = ":";
/// `mode-line-client` on a frame of a server client.
const MODELINE_CLIENT: &str = "@";
/// `mode-line-remote` for a local directory.
const MODELINE_REMOTE: &str = "-";
/// The `min-width` of the mule-info, client, modified and remote group.
const MODELINE_FLAGS_MIN_WIDTH: usize = 5;
/// The `min-width` of `mode-line-percent-position`.
const PERCENT_MIN_WIDTH: usize = 5;
/// `%12b`: the buffer name is padded to at least this many columns.
const BUFFER_NAME_COLUMNS: usize = 12;
const MODELINE_POSITION_GAP: &str = "   ";
/// `(6 ...)` around `mode-line-position-line-format`: ` L%l` padded to
/// this many columns, which is also its `min-width`.
const LINE_NUMBER_COLUMNS: usize = 6;
const MODELINE_MODES_GAP: &str = "  ";
/// Shown, until the buffer is killed, on a buffer whose file an in-place
/// save left partly rewritten.
const MODELINE_DAMAGED: &str = "[DAMAGED] ";
/// The buffer is modified and its file changed on disk.
const MODELINE_CONFLICT: &str = "[CONFLICT] ";
const MODELINE_DELETED: &str = "[DELETED] ";
/// The file was reloaded (or grew) while no window showed the buffer.
const MODELINE_CHANGED: &str = "[ext] ";
/// The visited file is polled, not watched (DESIGN section 3).
const MODELINE_POLLED: &str = "[poll] ";
/// Added to the mode name while the visited file is being read.
const MODELINE_LOADING: &str = " Loading";
/// Added to the mode name while `auto-revert-tail-mode` is on.
const MODELINE_TAIL: &str = " Tail";
/// The vertical border between side-by-side windows on a terminal.
const VERTICAL_BORDER_GLYPH: &str = "|";
/// `mode-line-end-spaces` on a terminal: `-%-` fills with dashes.
const MODELINE_FILL: &str = "-";
/// The panel's modeline before its root is known.
const PANEL_MODELINE: &str = "Tree";
const UNKNOWN_LINE: &str = "?";
const NEWLINE: u8 = b'\n';
/// `%p` computes differently above this size to avoid overflow.
const LARGE_BUFFER: u64 = 1_000_000;
const PERCENT: u64 = 100;
/// `%p` never shows 100%, which would not fit in two digits.
const MAX_PERCENT: u64 = 99;

/// A cache block of `buffer` the renderer needed but could not read
/// without I/O. Once it is cached, the frame should be rendered again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PrefetchRequest {
    pub buffer: BufferId,
    pub block: u64,
}

/// Renders `client`'s frame. Pure: the editor is not changed.
pub fn render(editor: &Editor, client: ClientId) -> (Grid, Vec<PrefetchRequest>) {
    let (cols, rows) = editor.client_size(client);
    let mut grid = Grid::new(cols, rows);
    let mut prefetch = Vec::new();
    let frame = editor.frame(client);
    let mut cursor = None;
    let panel = frame.panel_box().map(|(window, _)| window);
    let windows = panel
        .into_iter()
        .chain(frame.boxes().into_iter().map(|(window, _)| window));
    for window_id in windows {
        let area = editor.window_area(client, window_id);
        let selected = window_id == frame.selected;
        let window_cursor = draw_window(
            editor,
            client,
            window_id,
            selected,
            &area,
            &mut grid,
            &mut prefetch,
        );
        // Only the selected window shows its cursor on a terminal.
        if selected {
            cursor = window_cursor;
        }
    }
    if let Some(row) = frame_layout(rows).echo_row {
        let lines = editor.candidate_lines(client);
        let first = usize::from(row) - lines.len();
        for (index, (text, selected)) in lines.iter().enumerate() {
            let line_row =
                u16::try_from(first + index).expect("candidate rows are above the echo row");
            draw_candidate(&mut grid, line_row, text, *selected, tab_width(editor));
        }
        let (line, minibuffer_cursor) = editor.echo_area(client);
        match minibuffer_cursor {
            Some(at) => {
                cursor = draw_minibuffer(&mut grid, row, &line, at, tab_width(editor));
            }
            None => draw_echo(&mut grid, row, &line, tab_width(editor)),
        }
    }
    grid.cursor = cursor;
    (grid, prefetch)
}

fn tab_width(editor: &Editor) -> usize {
    usize::try_from(editor.config().tab_width).expect("tab_width is validated small")
}

/// Draws the window's text, header line, modeline and border; returns the
/// cursor when point is shown.
fn draw_window(
    editor: &Editor,
    window_client: ClientId,
    window_id: WindowId,
    selected: bool,
    area: &WindowArea,
    grid: &mut Grid,
    prefetch: &mut Vec<PrefetchRequest>,
) -> Option<(u16, u16)> {
    let window = editor.window(window_client, window_id);
    let buffer = editor.buffer(window.buffer);
    let point = buffer.marker_pos(window.point);
    let requested = ScreenLine {
        line_start: buffer.marker_pos(window.start),
        row: u64::from(window.start_row_skip),
    };
    let layout = editor.window_layout(window_client, window_id, &buffer.text, Fetch::CachedOnly);
    // A long line scrolled part way down begins the window mid-line, reached
    // through the column checkpoints so the walk reads only the visible rows.
    let start = match layout.resolve_start(requested, &buffer.checkpoints, &buffer.text) {
        Ok(start) => start,
        Err(error) => {
            prefetch.push(PrefetchRequest {
                buffer: buffer.id,
                block: not_cached_block(error),
            });
            requested
        }
    };
    // Emacs highlights the parens of the selected window only.
    let parens = (selected && editor.config().show_paren_mode)
        .then(|| {
            show_paren(
                &buffer.text,
                &buffer.faces,
                point,
                editor.config().show_paren_max_distance,
            )
        })
        .flatten();
    let marks = Marks {
        parens,
        faces: &buffer.faces,
        region: active_region(buffer, point),
        search: search_marks(
            editor,
            window_client,
            window_id,
            &layout,
            start,
            area.text_rows,
        ),
    };
    let mut tracker = CursorTracker::new(point);
    let mut first_shown = None;
    let walk = layout.walk_window(start, area.text_rows, &mut |row, event| {
        draw_event(grid, area, row, event, &marks, &mut tracker);
        if let LineEvent::Unit { pos, .. }
        | LineEvent::End { pos, .. }
        | LineEvent::Cell { unit: pos, .. } = event
        {
            first_shown.get_or_insert(*pos);
        }
        ControlFlow::Continue(())
    });
    let end_visible = match walk {
        Ok(walk) => walk.end_visible,
        Err(error) => {
            prefetch.push(PrefetchRequest {
                buffer: buffer.id,
                block: not_cached_block(error),
            });
            false
        }
    };
    if let (Some(row), Some(menu)) = (area.header_row, &buffer.menu) {
        let mut writer = RowWriter::within(grid, row, area, tab_width(editor));
        writer.write(menu.header.as_bytes(), Face::HeaderLine, Face::HeaderLine);
        writer.fill(b" ", Face::HeaderLine);
    }
    if let Some(row) = area.modeline_row {
        let line = match buffer.text.line_of_cached(point) {
            Ok(Some(line)) => (line + 1).to_string(),
            Ok(None) => UNKNOWN_LINE.to_string(),
            Err(error) => {
                prefetch.push(PrefetchRequest {
                    buffer: buffer.id,
                    block: not_cached_block(error),
                });
                UNKNOWN_LINE.to_string()
            }
        };
        let window_start = first_shown.unwrap_or(start.line_start);
        let position = position_indicator(window_start, buffer.text.len(), end_visible);
        let modeline = Modeline {
            buffer,
            frame: editor.frame_name(window_client),
            position: &position,
            line: &line,
            face: if selected {
                Face::ModeLine
            } else {
                Face::ModeLineInactive
            },
        };
        let mut writer = RowWriter::within(grid, row, area, tab_width(editor));
        match &buffer.tree {
            Some(tree) => draw_panel_modeline(&mut writer, tree, modeline.face),
            None => draw_modeline(&mut writer, &modeline),
        }
    }
    if let Some(col) = area.border {
        for row in area.box_top..area.box_top.saturating_add(area.box_rows) {
            put_glyph(
                grid,
                row,
                usize::from(col),
                VERTICAL_BORDER_GLYPH,
                1,
                Face::ModeLineInactive,
            );
        }
    }
    let (row, col) = tracker.location()?;
    let last_col = area.cols.checked_sub(1)?;
    let col = u16::try_from(col).unwrap_or(u16::MAX).min(last_col);
    Some((area.left + col, area.top + row))
}

/// The parts of a window's text drawn in another face.
struct Marks<'a> {
    /// `show-paren-mode`'s parens, drawn over the region (its overlay
    /// priority is higher).
    parens: Option<ParenMatch>,
    /// The buffer's own faces, under every other.
    faces: &'a FaceSpans,
    region: Option<Range<u64>>,
    search: SearchMarks,
}

/// What an isearch or query-replace of the client highlights in a window:
/// the current match (Emacs's `isearch` face) and, lazily, the other
/// matches in view (`lazy-highlight`).
#[derive(Default)]
struct SearchMarks {
    current: Option<Range<u64>>,
    /// Sorted, without overlaps.
    lazy: Vec<Range<u64>>,
}

impl SearchMarks {
    fn current_contains(&self, pos: u64) -> bool {
        self.current
            .as_ref()
            .is_some_and(|range| range.contains(&pos))
    }

    fn lazy_contains(&self, pos: u64) -> bool {
        let index = self.lazy.partition_point(|range| range.end <= pos);
        self.lazy
            .get(index)
            .is_some_and(|range| range.contains(&pos))
    }
}

/// The search highlights of a window. Only the text in view is searched,
/// and only what is cached: lazy highlighting never waits for I/O, and
/// text not cached yet is drawn without it until a later frame.
fn search_marks(
    editor: &Editor,
    client: ClientId,
    window_id: WindowId,
    layout: &crate::window::Layout,
    start: ScreenLine,
    rows: u16,
) -> SearchMarks {
    let Some(highlights) = editor.search_highlights(client) else {
        return SearchMarks::default();
    };
    let window = editor.window(client, window_id);
    if window.buffer != highlights.buffer {
        return SearchMarks::default();
    }
    let current = highlights
        .current
        .filter(|(searched, range)| *searched == window_id && !range.is_empty())
        .map(|(_, range)| range);
    let mut lazy = Vec::new();
    if let Some(pattern) = &highlights.lazy
        && let Ok(walk) = layout.walk_window(start, rows, &mut |_, _| ControlFlow::Continue(()))
    {
        let text = &editor.buffer(window.buffer).text;
        let end = walk.end.min(text.len());
        if start.line_start <= end
            && let Ok(found) =
                matches_in_cached(text, pattern, start.line_start..end, editor.match_cap())
        {
            lazy = found
                .into_iter()
                .filter(|found| !found.is_empty())
                .map(|found| found.range())
                .collect();
        }
    }
    SearchMarks { current, lazy }
}

/// The region Transient Mark mode highlights, between point and an
/// active mark.
fn active_region(buffer: &Buffer, point: u64) -> Option<Range<u64>> {
    if !buffer.mark_active {
        return None;
    }
    let mark = buffer.marker_pos(buffer.mark?);
    Some(mark.min(point)..mark.max(point))
}

/// Draws one layout event of window row `window_row`; the tracker sees
/// window rows, the grid gets the area's cells.
fn draw_event(
    grid: &mut Grid,
    area: &WindowArea,
    window_row: u16,
    event: &LineEvent,
    marks: &Marks,
    tracker: &mut CursorTracker,
) {
    let row = area.top + window_row;
    let in_region = |pos: u64| {
        marks
            .region
            .as_ref()
            .is_some_and(|range| range.contains(&pos))
    };
    match event {
        LineEvent::Cell {
            unit,
            col,
            text,
            width,
            escape,
            ..
        } => {
            let paren = marks
                .parens
                .filter(|found| found.here == *unit || found.there == Some(*unit));
            let face = if marks.search.current_contains(*unit) {
                Face::IsearchMatch
            } else if marks.search.lazy_contains(*unit) {
                Face::LazyHighlight
            } else if let Some(found) = paren {
                if found.matched {
                    Face::ShowParenMatch
                } else {
                    Face::ShowParenMismatch
                }
            } else if in_region(*unit) {
                Face::Region
            } else if *escape {
                Face::EscapeGlyph
            } else {
                marks.faces.face_at(*unit).unwrap_or(Face::Default)
            };
            put_window_glyph(grid, area, row, *col, text, *width, face);
        }
        LineEvent::Continuation { col, .. } => {
            put_window_glyph(grid, area, row, *col, CONTINUATION_GLYPH, 1, Face::Default);
        }
        LineEvent::Truncation { col, .. } => {
            put_window_glyph(grid, area, row, *col, TRUNCATION_GLYPH, 1, Face::Default);
        }
        LineEvent::Unit { pos, col, .. } => tracker.observe(*pos, window_row, *col),
        LineEvent::End {
            pos, col, newline, ..
        } => {
            tracker.observe(*pos, window_row, *col);
            // The region face has `:extend t`: a selected newline lights
            // the rest of its row.
            if *newline && in_region(*pos) {
                for fill in *col..usize::from(area.cols) {
                    put_window_glyph(grid, area, row, fill, " ", 1, Face::Region);
                }
            }
        }
    }
}

/// Writes a glyph at window column `col` if it fits inside the window's
/// text columns.
fn put_window_glyph(
    grid: &mut Grid,
    area: &WindowArea,
    row: u16,
    col: usize,
    text: &str,
    width: usize,
    face: Face,
) {
    if col + width > usize::from(area.cols) {
        return;
    }
    put_glyph(grid, row, usize::from(area.left) + col, text, width, face);
}

/// Writes a glyph if it fits entirely; a wide glyph's second column gets
/// a width-0 cell.
fn put_glyph(grid: &mut Grid, row: u16, col: usize, text: &str, width: usize, face: Face) {
    let cols = usize::from(grid.cols);
    if row >= grid.rows || col + width > cols {
        return;
    }
    let col = u16::try_from(col).expect("inside the u16 grid width");
    grid.set(
        col,
        row,
        Cell {
            text: text.to_string(),
            width: u8::try_from(width).expect("glyphs are one or two columns"),
            face,
        },
    );
    for trailing in 1..width {
        let trailing = u16::try_from(trailing).expect("glyphs are one or two columns");
        grid.set(
            col + trailing,
            row,
            Cell {
                text: String::new(),
                width: 0,
                face,
            },
        );
    }
}

/// Emacs `%p` limited to three columns by `(-3 "%p")`: where the window
/// starts, or whether it shows the top or bottom of the buffer.
fn position_indicator(window_start: u64, length: u64, end_visible: bool) -> String {
    match (window_start == 0, end_visible) {
        (true, true) => "All".to_string(),
        (false, true) => "Bot".to_string(),
        (true, false) => "Top".to_string(),
        (false, false) => {
            // Both round up, as decode_mode_spec's integer arithmetic does.
            let percent = if length > LARGE_BUFFER {
                window_start.div_ceil(length / PERCENT)
            } else {
                (window_start * PERCENT).div_ceil(length)
            };
            format!("{:>2}%", percent.min(MAX_PERCENT))
        }
    }
}

/// Writes glyphs left to right along one row from column `col`, clipped
/// at column `right`.
struct RowWriter<'g> {
    grid: &'g mut Grid,
    row: u16,
    col: usize,
    right: usize,
    tab_width: usize,
}

impl<'g> RowWriter<'g> {
    /// A writer across the whole grid row.
    fn full_row(grid: &'g mut Grid, row: u16, tab_width: usize) -> RowWriter<'g> {
        let right = usize::from(grid.cols);
        RowWriter {
            grid,
            row,
            col: 0,
            right,
            tab_width,
        }
    }

    /// A writer across a window's text columns.
    fn within(grid: &'g mut Grid, row: u16, area: &WindowArea, tab_width: usize) -> RowWriter<'g> {
        let left = usize::from(area.left);
        RowWriter {
            grid,
            row,
            col: left,
            right: left + usize::from(area.cols),
            tab_width,
        }
    }

    /// Repeats `bytes` to the right edge.
    fn fill(&mut self, bytes: &[u8], face: Face) {
        while self.col < self.right {
            let before = self.col;
            self.write(bytes, face, face);
            if self.col == before {
                return;
            }
        }
    }

    /// Writes `bytes` with control and invalid bytes escaped; escape
    /// notation gets `escape_face`.
    fn write(&mut self, bytes: &[u8], face: Face, escape_face: Face) {
        let cols = self.right;
        for (offset, unit) in display_units(bytes) {
            let unit = one_line(unit);
            let column = self.col as u64;
            let flow = unit_glyphs(
                &bytes[offset..],
                unit,
                column,
                self.tab_width,
                &mut |text, width, escape| {
                    if self.col + width > cols {
                        return ControlFlow::Break(());
                    }
                    let face = if escape { escape_face } else { face };
                    put_glyph(self.grid, self.row, self.col, text, width, face);
                    self.col += width;
                    ControlFlow::Continue(())
                },
            );
            if flow.is_break() {
                self.col = cols;
                return;
            }
        }
    }

    fn pad_to(&mut self, column: usize, face: Face) {
        while self.col < column {
            self.write(b" ", face, face);
            if self.col >= self.right {
                return;
            }
        }
    }
}

/// What one window's modeline shows.
struct Modeline<'a> {
    buffer: &'a Buffer,
    /// The frame number and whether it is a server client's frame.
    frame: (u64, bool),
    position: &'a str,
    line: &'a str,
    /// `mode-line` for the selected window, `mode-line-inactive` for the
    /// others.
    face: Face,
}

impl RowWriter<'_> {
    /// A segment with a `min-width` display property, as Emacs 29 draws it
    /// on a terminal: padded with blanks up to the width, and followed by
    /// one blank when wider (`emacs -nw` shows `-UUU:---  F1` and
    /// `L15001   (` where `format-mode-line` gives `-UUU:--- F1` and
    /// `L15001  (`).
    fn min_width(&mut self, width: usize, face: Face, write: impl FnOnce(&mut Self)) {
        let start = self.col;
        write(self);
        let written = self.col - start;
        if written < width {
            self.pad_to(start + width, face);
        } else if written > width {
            self.write(b" ", face, face);
        }
    }
}

/// The file panel's modeline: the name of the project it shows.
fn draw_panel_modeline(writer: &mut RowWriter, tree: &crate::editor::TreeData, face: Face) {
    let name = tree
        .root
        .as_ref()
        .and_then(|root| root.file_name())
        .map_or_else(
            || PANEL_MODELINE.as_bytes().to_vec(),
            |name| name.as_encoded_bytes().to_vec(),
        );
    writer.write(MODELINE_FRONT.as_bytes(), face, face);
    writer.write(b" ", face, face);
    writer.write(&name, face, Face::EscapeGlyph);
    writer.write(b" ", face, face);
    writer.fill(MODELINE_FILL.as_bytes(), face);
}

/// Emacs 29's default `mode-line-format` for a Fundamental-mode buffer as
/// `emacs -nw -Q` (or `emacsclient -t`) shows it on screen, the rest of the
/// row filled with dashes.
fn draw_modeline(writer: &mut RowWriter, modeline: &Modeline) {
    let face = modeline.face;
    let buffer = modeline.buffer;
    let (frame_number, client_frame) = modeline.frame;
    // `%1*%1+`: `%` for read-only, `*` for modified, `-` otherwise.
    let flags = match (buffer.read_only || buffer.loading, buffer.is_modified()) {
        (true, true) => "%*",
        (true, false) => "%%",
        (false, true) => "**",
        (false, false) => "--",
    };
    writer.write(MODELINE_FRONT.as_bytes(), face, face);
    writer.min_width(MODELINE_FLAGS_MIN_WIDTH, face, |writer| {
        writer.write(MODELINE_TERMINAL_CODING.as_bytes(), face, face);
        let mut letter = [0; 4];
        let letter = buffer.coding.letter().encode_utf8(&mut letter);
        writer.write(letter.as_bytes(), face, face);
        writer.write(MODELINE_UNIX_EOL.as_bytes(), face, face);
        if client_frame {
            writer.write(MODELINE_CLIENT.as_bytes(), face, face);
        }
        writer.write(flags.as_bytes(), face, face);
        writer.write(MODELINE_REMOTE.as_bytes(), face, face);
    });
    // `mode-line-frame-identification` on a terminal: ` %F  `.
    writer.write(format!(" F{frame_number}  ").as_bytes(), face, face);
    let name_start = writer.col;
    writer.write(buffer.name.as_bytes(), face, face);
    writer.pad_to(name_start + BUFFER_NAME_COLUMNS, face);
    writer.write(MODELINE_POSITION_GAP.as_bytes(), face, face);
    writer.min_width(PERCENT_MIN_WIDTH, face, |writer| {
        writer.write(modeline.position.as_bytes(), face, face);
    });
    writer.min_width(LINE_NUMBER_COLUMNS, face, |writer| {
        let line_start = writer.col;
        writer.write(format!(" L{}", modeline.line).as_bytes(), face, face);
        writer.pad_to(line_start + LINE_NUMBER_COLUMNS, face);
    });
    writer.write(MODELINE_MODES_GAP.as_bytes(), face, face);
    let loading = if buffer.loading { MODELINE_LOADING } else { "" };
    let tail = if buffer.tail { MODELINE_TAIL } else { "" };
    let modes = format!("({}{loading}{tail}) ", buffer.mode_name());
    writer.write(modes.as_bytes(), face, face);
    if buffer.damaged {
        writer.write(MODELINE_DAMAGED.as_bytes(), face, face);
    }
    if let Some(badge) = buffer.file.as_ref().and_then(external_badge) {
        writer.write(badge.as_bytes(), face, face);
    }
    if buffer.file.as_ref().is_some_and(|file| file.polled) {
        writer.write(MODELINE_POLLED.as_bytes(), face, face);
    }
    if let Some(progress) = buffer.indexing {
        let badge = format!("[Indexing {}%] ", progress.percent());
        writer.write(badge.as_bytes(), face, face);
    }
    if let Some(progress) = buffer.saving_progress {
        let badge = format!("Saving {}% ", progress.percent());
        writer.write(badge.as_bytes(), face, face);
    }
    writer.fill(MODELINE_FILL.as_bytes(), face);
}

/// The modeline notice for what happened to the visited file on disk. A
/// settling file still shows the state it settles from.
fn external_badge(binding: &FileBinding) -> Option<String> {
    let mut state = &binding.external;
    while let ExternalState::Settling { previous, .. } = state {
        state = previous;
    }
    match state {
        ExternalState::Conflict { .. } => Some(MODELINE_CONFLICT.to_string()),
        ExternalState::Deleted => Some(MODELINE_DELETED.to_string()),
        ExternalState::Renamed(to) => Some(format!("[MOVED to {}] ", to.display())),
        ExternalState::Clean | ExternalState::KeepMine { .. } if binding.changed_since_viewed => {
            Some(MODELINE_CHANGED.to_string())
        }
        _ => None,
    }
}

fn draw_echo(grid: &mut Grid, row: u16, echo: &[u8], tab_width: usize) {
    let mut writer = RowWriter::full_row(grid, row, tab_width);
    writer.write(echo, Face::Default, Face::EscapeGlyph);
}

/// One completion candidate above the echo area; the selected one is
/// highlighted (`icomplete-selected-match`). Bytes that are not text show
/// escaped, as everywhere else.
fn draw_candidate(grid: &mut Grid, row: u16, text: &[u8], selected: bool, tab_width: usize) {
    let face = if selected {
        Face::CompletionSelected
    } else {
        Face::Default
    };
    let mut writer = RowWriter::full_row(grid, row, tab_width);
    writer.write(text, face, Face::EscapeGlyph);
}

/// Draws the minibuffer line with the cursor at byte `cursor`; a line too
/// wide for the row drops columns on the left so the cursor stays visible.
/// Returns the cursor's cell.
fn draw_minibuffer(
    grid: &mut Grid,
    row: u16,
    line: &[u8],
    cursor: usize,
    tab_width: usize,
) -> Option<(u16, u16)> {
    let cols = usize::from(grid.cols);
    let last_col = cols.checked_sub(1)?;
    let mut start = 0;
    while text_width(&line[start..cursor], tab_width) > last_col {
        let rest = &line[start..];
        let mut units = display_units(rest);
        units.next();
        start += units.next().map_or(rest.len(), |(offset, _)| offset);
    }
    let mut writer = RowWriter::full_row(grid, row, tab_width);
    writer.write(&line[start..cursor], Face::Default, Face::EscapeGlyph);
    let cursor_col = writer.col;
    writer.write(&line[cursor..], Face::Default, Face::EscapeGlyph);
    let col = u16::try_from(cursor_col.min(last_col)).expect("inside the u16 grid width");
    Some((col, row))
}

/// The echo area and modeline are one row: a newline in a message (a file
/// name can hold one) shows as `^J`, as Emacs shows it in a one-line
/// minibuffer.
fn one_line(unit: DisplayUnit) -> DisplayUnit {
    match unit {
        DisplayUnit::Newline => DisplayUnit::Control(NEWLINE),
        other => other,
    }
}

pub(crate) fn text_width(bytes: &[u8], tab_width: usize) -> usize {
    let mut column = 0;
    for (_, unit) in display_units(bytes) {
        column += unit_width(&one_line(unit), column, tab_width);
    }
    column
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(grid: &Grid, row: u16) -> String {
        grid.row(row)
            .iter()
            .filter(|cell| cell.width > 0)
            .map(|cell| cell.text.as_str())
            .collect()
    }

    #[test]
    fn position_indicator_follows_decode_mode_spec() {
        assert_eq!(position_indicator(0, 0, true), "All");
        assert_eq!(position_indicator(0, 500, false), "Top");
        assert_eq!(position_indicator(10, 500, true), "Bot");
        assert_eq!(position_indicator(96, 160, false), "60%");
        assert_eq!(position_indicator(32, 160, false), "20%");
        assert_eq!(position_indicator(1, 1000, false), " 1%");
        assert_eq!(position_indicator(999, 1000, false), "99%");
        assert_eq!(position_indicator(2_000_000, 4_000_000, false), "50%");
        assert_eq!(position_indicator(3_999_999, 4_000_000, false), "99%");
    }

    #[test]
    fn echo_escapes_control_and_invalid_bytes() {
        let mut grid = Grid::new(20, 1);
        draw_echo(&mut grid, 0, "a\u{1b}b\u{9b}\tc".as_bytes(), 8);
        assert_eq!(row_text(&grid, 0), "a^[b\\233        c   ");
        assert_eq!(grid.row(0)[1].face, Face::EscapeGlyph);
        assert_eq!(grid.row(0)[0].face, Face::Default);
    }

    #[test]
    fn echo_clips_a_wide_glyph_at_the_edge() {
        let mut grid = Grid::new(3, 1);
        draw_echo(&mut grid, 0, "a\u{4e2d}\u{4e2d}".as_bytes(), 8);
        assert_eq!(row_text(&grid, 0), "a\u{4e2d}");
        let mut grid = Grid::new(2, 1);
        draw_echo(&mut grid, 0, "a\u{4e2d}".as_bytes(), 8);
        assert_eq!(row_text(&grid, 0), "a ");
    }
}
