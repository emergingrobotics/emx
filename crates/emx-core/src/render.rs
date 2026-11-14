//! Draws a client's frame into a `Grid`: each window's text and modeline,
//! then the echo area. Rendering reads only cached bytes, so it never
//! blocks on I/O; text that is not cached yet draws blank and is returned
//! as a prefetch request.

use std::ops::{ControlFlow, Range};

use emx_text::{DisplayUnit, display_units, unit_width};
use emx_types::{Cell, Face, Grid};

use crate::buffer::{Buffer, BufferId, FileBinding};
use crate::editor::Editor;
use crate::event::ClientId;
use crate::reload::ExternalState;
use crate::window::{
    CONTINUATION_GLYPH, CursorTracker, Fetch, Layout, LineEvent, Reader, ScreenLine, Window,
    frame_layout, not_cached_block, unit_glyphs,
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
const MODELINE_MODES: &str = "(Fundamental) ";
/// Shown, until the buffer is killed, on a buffer whose file an in-place
/// save left partly rewritten.
const MODELINE_DAMAGED: &str = "[DAMAGED] ";
/// The buffer is modified and its file changed on disk.
const MODELINE_CONFLICT: &str = "[CONFLICT] ";
const MODELINE_DELETED: &str = "[DELETED] ";
/// The file was reloaded (or grew) since the buffer was last looked at.
const MODELINE_CHANGED: &str = "[CHANGED] ";
/// Shown while the visited file is being read.
const MODELINE_MODES_LOADING: &str = "(Fundamental Loading) ";
/// `mode-line-end-spaces` on a terminal: `-%-` fills with dashes.
const MODELINE_FILL: &str = "-";
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
    let layout = frame_layout(rows);
    let frame = editor.frame(client);
    // A single window fills the frame until the window tree arrives; each
    // window draws into its own rectangle so more can be laid out later.
    let area = WindowArea {
        top: 0,
        cols,
        text_rows: layout.text_rows,
        modeline_row: layout.modeline_row,
    };
    let cursor = draw_window(
        editor,
        client,
        frame.selected_window(),
        &area,
        &mut grid,
        &mut prefetch,
    );
    let mut cursor = cursor;
    if let Some(row) = layout.echo_row {
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

/// The grid rows a window draws into.
struct WindowArea {
    top: u16,
    cols: u16,
    text_rows: u16,
    modeline_row: Option<u16>,
}

fn tab_width(editor: &Editor) -> usize {
    usize::try_from(editor.config().tab_width).expect("tab_width is validated small")
}

/// Draws the window's text and modeline; returns the cursor when point is
/// shown.
fn draw_window(
    editor: &Editor,
    window_client: ClientId,
    window: &Window,
    area: &WindowArea,
    grid: &mut Grid,
    prefetch: &mut Vec<PrefetchRequest>,
) -> Option<(u16, u16)> {
    let buffer = editor.buffer(window.buffer);
    let point = buffer.marker_pos(window.point);
    let start = ScreenLine {
        line_start: buffer.marker_pos(window.start),
        row: u64::from(window.start_row_skip),
    };
    let layout = Layout::new(
        Reader::new(&buffer.text, Fetch::CachedOnly),
        area.cols,
        tab_width(editor),
    );
    let region = active_region(buffer, point);
    let mut tracker = CursorTracker::new(point);
    let mut first_shown = None;
    let walk = layout.walk_window(start, area.text_rows, &mut |row, event| {
        let row = area.top + row;
        draw_event(grid, row, event, &region, &mut tracker);
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
        };
        draw_modeline(grid, row, &modeline, tab_width(editor));
    }
    let (row, col) = tracker.location()?;
    let last_col = area.cols.checked_sub(1)?;
    let col = u16::try_from(col).unwrap_or(u16::MAX).min(last_col);
    Some((col, area.top + row))
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

fn draw_event(
    grid: &mut Grid,
    row: u16,
    event: &LineEvent,
    region: &Option<Range<u64>>,
    tracker: &mut CursorTracker,
) {
    let in_region = |pos: u64| region.as_ref().is_some_and(|range| range.contains(&pos));
    match event {
        LineEvent::Cell {
            unit,
            col,
            text,
            width,
            escape,
            ..
        } => {
            let face = if in_region(*unit) {
                Face::Region
            } else if *escape {
                Face::EscapeGlyph
            } else {
                Face::Default
            };
            put_glyph(grid, row, *col, text, *width, face);
        }
        LineEvent::Continuation { col, .. } => {
            put_glyph(grid, row, *col, CONTINUATION_GLYPH, 1, Face::Default);
        }
        LineEvent::Unit { pos, col, .. } => tracker.observe(*pos, row, *col),
        LineEvent::End {
            pos, col, newline, ..
        } => {
            tracker.observe(*pos, row, *col);
            // The region face has `:extend t`: a selected newline lights
            // the rest of its row.
            if *newline && in_region(*pos) {
                for fill in *col..usize::from(grid.cols) {
                    put_glyph(grid, row, fill, " ", 1, Face::Region);
                }
            }
        }
    }
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

/// Writes glyphs left to right along one row, clipped at the right edge.
struct RowWriter<'g> {
    grid: &'g mut Grid,
    row: u16,
    col: usize,
    tab_width: usize,
}

impl RowWriter<'_> {
    /// Writes `bytes` with control and invalid bytes escaped; escape
    /// notation gets `escape_face`.
    fn write(&mut self, bytes: &[u8], face: Face, escape_face: Face) {
        let cols = usize::from(self.grid.cols);
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
            if self.col >= usize::from(self.grid.cols) {
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

/// Emacs 29's default `mode-line-format` for a Fundamental-mode buffer as
/// `emacs -nw -Q` (or `emacsclient -t`) shows it on screen, the rest of the
/// row filled with dashes.
fn draw_modeline(grid: &mut Grid, row: u16, modeline: &Modeline, tab_width: usize) {
    let face = Face::ModeLine;
    let buffer = modeline.buffer;
    let (frame_number, client_frame) = modeline.frame;
    // `%1*%1+`: `%` for read-only, `*` for modified, `-` otherwise.
    let flags = match (buffer.read_only || buffer.loading, buffer.is_modified()) {
        (true, true) => "%*",
        (true, false) => "%%",
        (false, true) => "**",
        (false, false) => "--",
    };
    let mut writer = RowWriter {
        grid,
        row,
        col: 0,
        tab_width,
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
    let modes = if buffer.loading {
        MODELINE_MODES_LOADING
    } else {
        MODELINE_MODES
    };
    writer.write(modes.as_bytes(), face, face);
    if buffer.damaged {
        writer.write(MODELINE_DAMAGED.as_bytes(), face, face);
    }
    if let Some(badge) = buffer.file.as_ref().and_then(external_badge) {
        writer.write(badge.as_bytes(), face, face);
    }
    while writer.col < usize::from(writer.grid.cols) {
        writer.write(MODELINE_FILL.as_bytes(), face, face);
    }
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

fn draw_echo(grid: &mut Grid, row: u16, echo: &str, tab_width: usize) {
    let mut writer = RowWriter {
        grid,
        row,
        col: 0,
        tab_width,
    };
    writer.write(echo.as_bytes(), Face::Default, Face::EscapeGlyph);
}

/// Draws the minibuffer line with the cursor at byte `cursor`; a line too
/// wide for the row drops columns on the left so the cursor stays visible.
/// Returns the cursor's cell.
fn draw_minibuffer(
    grid: &mut Grid,
    row: u16,
    line: &str,
    cursor: usize,
    tab_width: usize,
) -> Option<(u16, u16)> {
    let cols = usize::from(grid.cols);
    let last_col = cols.checked_sub(1)?;
    let mut start = 0;
    while text_width(&line.as_bytes()[start..cursor], tab_width) > last_col {
        let rest = &line.as_bytes()[start..];
        let mut units = display_units(rest);
        units.next();
        start += units.next().map_or(rest.len(), |(offset, _)| offset);
    }
    let mut writer = RowWriter {
        grid,
        row,
        col: 0,
        tab_width,
    };
    writer.write(
        &line.as_bytes()[start..cursor],
        Face::Default,
        Face::EscapeGlyph,
    );
    let cursor_col = writer.col;
    writer.write(&line.as_bytes()[cursor..], Face::Default, Face::EscapeGlyph);
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

fn text_width(bytes: &[u8], tab_width: usize) -> usize {
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
        draw_echo(&mut grid, 0, "a\u{1b}b\u{9b}\tc", 8);
        assert_eq!(row_text(&grid, 0), "a^[b\\233        c   ");
        assert_eq!(grid.row(0)[1].face, Face::EscapeGlyph);
        assert_eq!(grid.row(0)[0].face, Face::Default);
    }

    #[test]
    fn echo_clips_a_wide_glyph_at_the_edge() {
        let mut grid = Grid::new(3, 1);
        draw_echo(&mut grid, 0, "a\u{4e2d}\u{4e2d}", 8);
        assert_eq!(row_text(&grid, 0), "a\u{4e2d}");
        let mut grid = Grid::new(2, 1);
        draw_echo(&mut grid, 0, "a\u{4e2d}", 8);
        assert_eq!(row_text(&grid, 0), "a ");
    }
}
