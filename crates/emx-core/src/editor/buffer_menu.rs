//! The Buffer Menu (`list-buffers`, `C-x C-b`): `*Buffer List*` laid out
//! as buff-menu.el and tabulated-list.el lay it out on a terminal, and
//! `RET` to visit a line's buffer.

use std::sync::Arc;

use emx_text::{Sources, Text};

use super::Editor;
use super::reload::Unjournaled;
use super::windows::INTERNAL_NAME_PREFIX;
use crate::buffer::{BufferId, BufferMenu, ChangeRecorder};
use crate::command::CommandError;
use crate::event::ClientId;
use crate::io::abbreviate;
use crate::render::text_width;
use crate::undo::UndoLimits;

const BUFFER_LIST: &str = "*Buffer List*";
const NO_BUFFER_ON_LINE: &str = "No buffer on this line";
const BUFFER_KILLED: &str = "This buffer has been killed";

/// The Buffer Menu's columns (`list-buffers--refresh`): the name column is
/// as wide as the longest name, at least 19 columns and at most a 4.2th
/// of the window.
const MENU_NAME_MIN_WIDTH: usize = 19;
const MENU_NAME_WIDTH_RATIO_TENTHS: usize = 42;
const TENTHS: usize = 10;
const MENU_SIZE_WIDTH: usize = 7;
const MENU_MODE_WIDTH: usize = 16;
const MENU_FLAG_WIDTH: usize = 1;
const MENU_FILE_WIDTH: usize = 1;
const MENU_CURRENT: &str = ".";
const MENU_READ_ONLY: &str = "%";
const MENU_MODIFIED: &str = "*";
const MENU_BLANK: &str = " ";
/// `truncate-string-ellipsis` on a terminal that shows it.
const ELLIPSIS: &str = "\u{2026}";

/// One column of a tabulated list (`tabulated-list-format`).
struct Column {
    title: &'static str,
    width: usize,
    pad_right: usize,
    right_align: bool,
}

impl Editor {
    /// `list-buffers` (`C-x C-b`): `*Buffer List*` lists the buffers, most
    /// recent first, and is displayed without being selected. With
    /// `files_only`, only buffers visiting files are listed.
    pub(crate) fn list_buffers_command(&mut self, client: ClientId, files_only: bool) {
        let current = self.selected_buffer(client);
        let selected = self.selected_window_id(client);
        let window_cols = usize::from(self.window_area(client, selected).cols);
        let list = match self
            .buffers
            .iter()
            .find(|(_, buffer)| buffer.name == BUFFER_LIST)
            .map(|(id, _)| id)
        {
            Some(list) => list,
            None => self.create_buffer(BUFFER_LIST),
        };
        let entries: Vec<BufferId> = self
            .recency
            .iter()
            .copied()
            .filter(|id| {
                let buffer = &self.buffers[*id];
                *id != list
                    && (!buffer.name.starts_with(INTERNAL_NAME_PREFIX) || buffer.file.is_some())
                    && (!files_only || buffer.file.is_some())
            })
            .collect();
        let rows: Vec<Vec<String>> = entries
            .iter()
            .map(|id| self.menu_row(*id, current))
            .collect();
        let longest_name = entries
            .iter()
            .map(|id| self.buffers[*id].name.chars().count())
            .max()
            .unwrap_or(0);
        let name_width = MENU_NAME_MIN_WIDTH
            .max((window_cols * TENTHS / MENU_NAME_WIDTH_RATIO_TENTHS).min(longest_name));
        let columns = menu_columns(name_width);
        let tab_width = self.tab_width();
        let mut text = String::new();
        for (index, row) in rows.iter().enumerate() {
            let previous = &rows[index.saturating_sub(1)];
            let next = rows.get(index + 1).unwrap_or(row);
            text.push_str(&tabulated_row(
                &columns,
                row,
                [previous, row, next],
                tab_width,
            ));
            text.push('\n');
        }
        let titles: Vec<String> = columns
            .iter()
            .map(|column| column.title.to_string())
            .collect();
        let header = tabulated_row(&columns, &titles, [&titles, &titles, &titles], tab_width);
        let limits = UndoLimits::from(&self.config);
        let text = Text::from_original(Sources::memory(Arc::from(text.into_bytes())));
        // A buffer the user made under this name is journaled; the journal
        // must see its text replaced. A list emx made is not journaled.
        let recorder: &mut dyn ChangeRecorder = if self.journal_outbox.tracks(list) {
            &mut self.journal_outbox
        } else {
            &mut Unjournaled
        };
        let target = &mut self.buffers[list];
        target.reset_text(text, limits, recorder);
        target.read_only = true;
        target.menu = Some(BufferMenu { entries, header });
        self.display_buffer(client, list);
    }

    /// One Buffer Menu entry: current, read-only and modified flags, name,
    /// size, mode and file.
    fn menu_row(&self, buffer: BufferId, current: BufferId) -> Vec<String> {
        let target = &self.buffers[buffer];
        let flag = |set: bool, shown: &str| if set { shown } else { MENU_BLANK }.to_string();
        vec![
            flag(buffer == current, MENU_CURRENT),
            flag(target.read_only, MENU_READ_ONLY),
            flag(target.is_modified(), MENU_MODIFIED),
            target.name.clone(),
            target.text.len().to_string(),
            target.mode_name().to_string(),
            target
                .file
                .as_ref()
                .map(|file| abbreviate(&file.path))
                .unwrap_or_default(),
        ]
    }

    /// `Buffer-menu-this-window` (`RET` in the Buffer Menu): shows the
    /// buffer of point's line in this window.
    pub(crate) fn buffer_menu_this_window_command(
        &mut self,
        client: ClientId,
    ) -> Result<(), CommandError> {
        let list = &self.buffers[self.selected_buffer(client)];
        let Some(menu) = &list.menu else {
            return Err(CommandError::User(NO_BUFFER_ON_LINE.to_string()));
        };
        let point = self.window_point(client, self.selected_window_id(client));
        let line = list.text.line_of(point)?;
        let entry = line
            .and_then(|line| usize::try_from(line).ok())
            .and_then(|line| menu.entries.get(line).copied())
            .ok_or_else(|| CommandError::User(NO_BUFFER_ON_LINE.to_string()))?;
        if !self.buffers.contains_key(entry) {
            return Err(CommandError::User(BUFFER_KILLED.to_string()));
        }
        self.switch_to_buffer(client, entry);
        Ok(())
    }
}

/// `list-buffers--refresh`'s `tabulated-list-format`.
fn menu_columns(name_width: usize) -> Vec<Column> {
    let flag = |title, pad_right| Column {
        title,
        width: MENU_FLAG_WIDTH,
        pad_right,
        right_align: false,
    };
    vec![
        flag("C", 0),
        flag("R", 0),
        flag("M", 1),
        Column {
            title: "Buffer",
            width: name_width,
            pad_right: 1,
            right_align: false,
        },
        Column {
            title: "Size",
            width: MENU_SIZE_WIDTH,
            pad_right: 1,
            right_align: true,
        },
        Column {
            title: "Mode",
            width: MENU_MODE_WIDTH,
            pad_right: 1,
            right_align: false,
        },
        Column {
            title: "File",
            width: MENU_FILE_WIDTH,
            pad_right: 1,
            right_align: false,
        },
    ]
}

/// One row of a tabulated list as Emacs displays it
/// (`tabulated-list-print-col`): each column padded to its width, a label
/// too wide for it truncated with an ellipsis, except that a label before
/// a right-aligned column may run into that column's unused space, judged
/// from the rows around it (`near`: previous, this, next).
fn tabulated_row(
    columns: &[Column],
    cells: &[String],
    near: [&Vec<String>; 3],
    tab_width: usize,
) -> String {
    let width_of = |text: &str| text_width(text.as_bytes(), tab_width);
    let mut out = String::new();
    let mut column = 0;
    let mut x = 0;
    for (index, (format, cell)) in columns.iter().zip(cells).enumerate() {
        let not_last = index + 1 < columns.len();
        let mut width = format.width;
        let mut label = cell.clone();
        let mut label_width = width_of(&label);
        let available = match columns.get(index + 1) {
            Some(next) if !format.right_align && next.right_align => {
                let local_max = near
                    .iter()
                    .map(|row| width_of(&row[index + 1]))
                    .max()
                    .unwrap_or(0);
                width + next.width - next.width.min(local_max)
            }
            _ => width,
        };
        if not_last && label_width > available {
            label = truncate_with_ellipsis(&label, available, tab_width);
            label_width = available;
        }
        if format.right_align && width > label_width {
            let shift = width - label_width;
            align_to(&mut out, &mut column, x + shift);
            width -= shift;
            x += shift;
        }
        out.push_str(&label);
        column += label_width;
        let next_x = x + format.pad_right + width;
        if not_last {
            out.push_str(&" ".repeat(format.pad_right));
            column += format.pad_right;
            align_to(&mut out, &mut column, next_x);
        }
        x = next_x;
    }
    out
}

/// A `(space :align-to TARGET)` stretch: blanks up to the target column,
/// none when the text already reaches past it.
fn align_to(out: &mut String, column: &mut usize, target: usize) {
    if target > *column {
        out.push_str(&" ".repeat(target - *column));
        *column = target;
    }
}

/// Emacs `truncate-string-to-width` with an ellipsis: as many whole
/// characters as fit before the ellipsis.
fn truncate_with_ellipsis(label: &str, width: usize, tab_width: usize) -> String {
    let room = width.saturating_sub(text_width(ELLIPSIS.as_bytes(), tab_width));
    let mut kept = String::new();
    let mut used = 0;
    for character in label.chars() {
        let mut encoded = [0; 4];
        let character_width = text_width(character.encode_utf8(&mut encoded).as_bytes(), tab_width);
        if used + character_width > room {
            break;
        }
        kept.push(character);
        used += character_width;
    }
    kept.push_str(ELLIPSIS);
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(cells: &[&str]) -> Vec<String> {
        cells.iter().map(|cell| cell.to_string()).collect()
    }

    #[test]
    fn header_matches_the_emacs_screen() {
        let columns = menu_columns(MENU_NAME_MIN_WIDTH);
        let titles = row(&["C", "R", "M", "Buffer", "Size", "Mode", "File"]);
        assert_eq!(
            tabulated_row(&columns, &titles, [&titles, &titles, &titles], 8),
            "CRM Buffer                 Size Mode             File"
        );
    }

    #[test]
    fn a_long_name_runs_into_the_free_size_columns() {
        // emacs -nw -Q, C-x C-b: ` %  *Async-native-compile-l…  0 Fundamental`
        // below a `*Messages*` row of size 66.
        let columns = menu_columns(MENU_NAME_MIN_WIDTH);
        let previous = row(&[" ", "%", "*", "*Messages*", "66", "Messages", ""]);
        let this = row(&[
            " ",
            "%",
            " ",
            "*Async-native-compile-log*",
            "0",
            "Fundamental",
            "",
        ]);
        assert_eq!(
            tabulated_row(&columns, &this, [&previous, &this, &this], 8),
            " %  *Async-native-compile-l\u{2026}  0 Fundamental      "
        );
        // Between rows of sizes 145 and 66: three columns of the size
        // column are taken.
        let above = row(&[".", " ", " ", "*scratch*", "145", "Fundamental", ""]);
        let this = row(&[
            " ",
            " ",
            " ",
            "a-very-long-buffer-name-indeed",
            "0",
            "Fundamental",
            "",
        ]);
        assert_eq!(
            tabulated_row(&columns, &this, [&above, &this, &previous], 8),
            "    a-very-long-buffer-nam\u{2026}   0 Fundamental      "
        );
    }
}
