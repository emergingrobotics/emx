//! A client's windows: splitting, deleting and selecting them, which
//! buffer each one shows and where, and the Buffer Menu. Ported from
//! Emacs 29's window.el, window.c and buff-menu.el.

use std::sync::Arc;

use emx_text::{ReadError, Sources, Text};
use emx_types::Key;

use super::{Editor, Purpose};
use crate::buffer::{BufferId, BufferMenu};
use crate::command::CommandError;
use crate::event::ClientId;
use crate::io::abbreviate;
use crate::keymap::Lookup;
use crate::marker::InsertionType;
use crate::minibuffer::{HistoryKind, Mode};
use crate::render::text_width;
use crate::undo::UndoLimits;
use crate::window::{
    Fetch, Layout, PrevBuffer, Reader, ScreenLine, WINDOW_MIN_HEIGHT, WINDOW_MIN_WIDTH,
    WINDOW_SAFE_MIN_HEIGHT, WINDOW_SAFE_MIN_WIDTH, Window, WindowId,
};

const SCRATCH_BUFFER: &str = "*scratch*";
const BUFFER_LIST: &str = "*Buffer List*";
/// Emacs `split-height-threshold` and `split-width-threshold`: the size a
/// window needs before `display-buffer` splits it, unless it is alone.
const SPLIT_HEIGHT_THRESHOLD: u16 = 80;
const SPLIT_WIDTH_THRESHOLD: u16 = 160;
/// `window-splittable-p` wants room for two windows of at least this
/// height with a modeline, and this width.
const SPLITTABLE_MIN_HEIGHT: u16 = 2;
const SPLITTABLE_MIN_WIDTH: u16 = 2;
const SOLE_WINDOW: &str = "Attempt to delete minibuffer or sole ordinary window";
const NO_OTHER_WINDOWS: &str = "No other windows to delete";
const NO_OTHER_WINDOW: &str = "No other window to select";
const NEW_WINDOW_TOO_SMALL: &str = "Size of new window too small";
const NO_BUFFER_ON_LINE: &str = "No buffer on this line";
const BUFFER_KILLED: &str = "This buffer has been killed";
/// Buffers whose names start with a space are internal: Emacs neither
/// lists nor records them.
const INTERNAL_NAME_PREFIX: char = ' ';

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
    /// Emacs looks keys up in the current buffer's local map first, then
    /// in the global map.
    pub(super) fn lookup_keys(&self, client: ClientId, keys: &[Key]) -> Lookup {
        let buffer = &self.buffers[self.selected_buffer(client)];
        if buffer.menu.is_some() {
            match self.buffer_menu_keymap.lookup(keys) {
                Lookup::Undefined => {}
                found => return found,
            }
        }
        self.keymap.lookup(keys)
    }

    // ---- Selecting ------------------------------------------------------

    /// Emacs `select-window` without NORECORD: the window's use time is
    /// the newest, and its buffer goes to the front of the buffer list.
    pub(super) fn bump_use_time(&mut self, client: ClientId, window: WindowId) {
        self.window_select_count += 1;
        let count = self.window_select_count;
        let target = &mut self.client_mut(client).frame.windows[window];
        target.use_time = count;
        let buffer = target.buffer;
        self.touch_buffer(buffer);
    }

    /// Emacs `select-window`. The window left keeps its point; its
    /// buffer's point becomes that point, as Emacs's PT does.
    fn select_window(&mut self, client: ClientId, window: WindowId) {
        let old = self.selected_window_id(client);
        if old != window {
            let left = *self.window(client, old);
            let point = self.buffers[left.buffer].marker_pos(left.point);
            self.remember_point(left.buffer, point);
            self.client_mut(client).frame.selected = window;
        }
        self.bump_use_time(client, window);
    }

    /// Sets the buffer's own point, which a window newly showing it gets.
    fn remember_point(&mut self, buffer: BufferId, point: u64) {
        let target = &mut self.buffers[buffer];
        match target.last_point {
            Some(marker) => target.set_marker(marker, point),
            None => {
                target.last_point =
                    Some(target.create_marker_without_undo(point, InsertionType::StayBefore));
            }
        }
    }

    /// Emacs `BUF_PT`: the selected window's point when it shows the
    /// buffer, else the point the buffer kept.
    fn buffer_point(&self, client: ClientId, buffer: BufferId) -> u64 {
        let selected = self.client(client).frame.selected_window();
        let target = &self.buffers[buffer];
        if selected.buffer == buffer {
            target.marker_pos(selected.point)
        } else {
            target
                .last_point
                .map_or(0, |marker| target.marker_pos(marker))
        }
    }

    /// `#<window 4 on foo>`, as Emacs prints a window.
    fn window_name(&self, client: ClientId, window: WindowId) -> String {
        let target = self.window(client, window);
        format!(
            "#<window {} on {}>",
            target.number, self.buffers[target.buffer].name
        )
    }

    // ---- Splitting ------------------------------------------------------

    /// `split-window-below` (`C-x 2`) and `split-window-right` (`C-x 3`):
    /// `size` is the prefix argument, the selected window's new size, or
    /// the new window's size when negative.
    pub(crate) fn split_window_command(
        &mut self,
        client: ClientId,
        side_by_side: bool,
        size: Option<i64>,
    ) -> Result<(), CommandError> {
        let window = self.selected_window_id(client);
        let new_size = self.checked_split_size(client, window, side_by_side, size)?;
        self.split_window(client, window, side_by_side, new_size);
        Ok(())
    }

    /// The new window's size as `split-window` works it out, or the error
    /// it signals.
    fn checked_split_size(
        &self,
        client: ClientId,
        window: WindowId,
        side_by_side: bool,
        size: Option<i64>,
    ) -> Result<u16, CommandError> {
        let (minimum, safe_minimum) = if side_by_side {
            (WINDOW_MIN_WIDTH, WINDOW_SAFE_MIN_WIDTH)
        } else {
            (WINDOW_MIN_HEIGHT, WINDOW_SAFE_MIN_HEIGHT)
        };
        let (minimum, safe_minimum) = (i64::from(minimum), i64::from(safe_minimum));
        let old_size = i64::from(self.frame(client).size_along(window, side_by_side));
        if let Some(size) = size
            && size < 0
            && size.saturating_neg() < minimum
        {
            return Err(CommandError::User(NEW_WINDOW_TOO_SMALL.to_string()));
        }
        let new_size = match size {
            None => old_size / 2,
            Some(size) if size >= 0 => old_size - size,
            Some(size) => size.saturating_neg(),
        };
        let too_small = match size {
            None => new_size + minimum > old_size,
            Some(size) if size >= 0 => {
                size >= old_size || new_size < safe_minimum || old_size - new_size < safe_minimum
            }
            Some(_) => new_size < safe_minimum || old_size - new_size < safe_minimum,
        };
        if too_small {
            return Err(CommandError::User(format!(
                "Window {} too small for splitting",
                self.window_name(client, window)
            )));
        }
        Ok(u16::try_from(new_size).expect("smaller than the window being split"))
    }

    /// Emacs `split-window` with `split-window-keep-point`: the new window
    /// shows the same buffer with the split window's point, starting where
    /// the buffer was last shown.
    fn split_window(
        &mut self,
        client: ClientId,
        window: WindowId,
        side_by_side: bool,
        new_size: u16,
    ) -> WindowId {
        if self.frame(client).split_needs_parent(window, side_by_side) {
            self.next_window_number += 1;
        }
        let number = self.next_window_number;
        self.next_window_number += 1;
        let source = *self.window(client, window);
        let target = &mut self.buffers[source.buffer];
        let point = target.marker_pos(source.point);
        let start = target.last_window_start;
        let new = Window {
            buffer: source.buffer,
            point: target.create_marker_without_undo(point, InsertionType::StayBefore),
            start: target.create_marker_without_undo(start, InsertionType::StayBefore),
            start_row_skip: 0,
            start_cols: 0,
            hscroll: 0,
            number,
            use_time: 0,
        };
        self.client_mut(client)
            .frame
            .split(window, side_by_side, new_size, new)
    }

    // ---- Deleting -------------------------------------------------------

    /// `delete-window` (`C-x 0`).
    pub(crate) fn delete_window_command(&mut self, client: ClientId) -> Result<(), CommandError> {
        if self.frame(client).sole() {
            return Err(CommandError::User(SOLE_WINDOW.to_string()));
        }
        let window = self.selected_window_id(client);
        self.delete_window(client, window);
        Ok(())
    }

    /// The window's space goes to a sibling. A deleted selected window is
    /// succeeded by the most recently used one (`delete-window-choose-
    /// selected` `mru`).
    fn delete_window(&mut self, client: ClientId, window: WindowId) {
        let was_selected = self.selected_window_id(client) == window;
        self.unshow(client, window);
        let removed = self.client_mut(client).frame.delete(window);
        self.discard_window(removed.0, removed.1);
        if was_selected {
            let frame = self.frame(client);
            let mut chosen = None;
            for id in frame.order() {
                let time = frame.windows[id].use_time;
                if chosen.is_none_or(|(_, best)| time > best) {
                    chosen = Some((id, time));
                }
            }
            let (chosen, _) = chosen.expect("a frame keeps a window");
            self.client_mut(client).frame.selected = chosen;
            self.bump_use_time(client, chosen);
        }
    }

    /// `delete-other-windows` (`C-x 1`).
    pub(crate) fn delete_other_windows_command(&mut self, client: ClientId) {
        if self.frame(client).sole() {
            self.set_echo(client, NO_OTHER_WINDOWS.to_string());
            return;
        }
        let selected = self.selected_window_id(client);
        let others: Vec<WindowId> = self
            .frame(client)
            .order()
            .into_iter()
            .filter(|id| *id != selected)
            .collect();
        for window in others {
            self.unshow(client, window);
        }
        let removed = self.client_mut(client).frame.keep_only(selected);
        for (window, history) in removed {
            self.discard_window(window, history);
        }
    }

    /// Deletes the markers of a window that is gone.
    fn discard_window(&mut self, window: Window, history: Vec<PrevBuffer>) {
        let target = &mut self.buffers[window.buffer];
        target.delete_marker(window.point);
        target.delete_marker(window.start);
        for entry in history {
            let target = &mut self.buffers[entry.buffer];
            target.delete_marker(entry.start);
            target.delete_marker(entry.point);
        }
    }

    // ---- other-window ---------------------------------------------------

    /// `other-window` (`C-x o`): selects the window `count` windows on in
    /// cyclic order, backwards when negative.
    pub(crate) fn other_window_command(&mut self, client: ClientId, count: i64) {
        let frame = self.frame(client);
        let order = frame.order();
        let current = order
            .iter()
            .position(|id| *id == frame.selected)
            .expect("the selected window is live");
        let length = i64::try_from(order.len()).expect("a frame has few windows");
        let current = i64::try_from(current).expect("a frame has few windows");
        let target = usize::try_from((current + count.rem_euclid(length)) % length)
            .expect("rem_euclid is not negative");
        let window = order[target];
        if window == frame.selected {
            self.set_echo(client, NO_OTHER_WINDOW.to_string());
        }
        self.select_window(client, window);
    }

    // ---- Which buffer a window shows -------------------------------------

    /// Emacs `unshow_buffer`: the buffer remembers where the window started
    /// and, unless the selected window shows it too, the window's point.
    fn unshow(&mut self, client: ClientId, window: WindowId) {
        let frame = self.frame(client);
        let target = frame.windows[window];
        let selected = frame.selected_window();
        let shown_selected = frame.selected != window && selected.buffer == target.buffer;
        let buffer = &mut self.buffers[target.buffer];
        buffer.last_window_start = buffer.marker_pos(target.start);
        let point = buffer.marker_pos(target.point);
        if !shown_selected {
            self.remember_point(target.buffer, point);
        }
    }

    /// Emacs `record-window-buffer`: the buffer leaving the window goes to
    /// the front of its previous buffers, with its start and point.
    fn record_window_buffer(&mut self, client: ClientId, window: WindowId) {
        let target = *self.window(client, window);
        if self.buffers[target.buffer]
            .name
            .starts_with(INTERNAL_NAME_PREFIX)
        {
            return;
        }
        let buffer = &mut self.buffers[target.buffer];
        let start = buffer.marker_pos(target.start);
        let point = buffer.marker_pos(target.point);
        let history = self
            .clients
            .get_mut(&client)
            .expect("events name only attached clients")
            .frame
            .prev_buffers
            .get_mut(window)
            .expect("every live window has a history");
        let entry = match history
            .iter()
            .position(|entry| entry.buffer == target.buffer)
        {
            Some(index) => {
                let entry = history.remove(index);
                buffer.set_marker(entry.start, start);
                buffer.set_marker(entry.point, point);
                entry
            }
            None => PrevBuffer {
                buffer: target.buffer,
                start: buffer.create_marker_without_undo(start, InsertionType::StayBefore),
                point: buffer.create_marker_without_undo(point, InsertionType::StayBefore),
            },
        };
        history.insert(0, entry);
    }

    /// Emacs `set-window-buffer`: the old buffer is recorded and unshown;
    /// the window shows the new one at its point, from where it was last
    /// shown.
    pub(super) fn set_window_buffer(
        &mut self,
        client: ClientId,
        window: WindowId,
        buffer: BufferId,
    ) {
        let old = *self.window(client, window);
        if old.buffer == buffer {
            return;
        }
        self.record_window_buffer(client, window);
        self.unshow(client, window);
        let leaving = &mut self.buffers[old.buffer];
        leaving.delete_marker(old.point);
        leaving.delete_marker(old.start);
        let point = self.buffer_point(client, buffer);
        let (cols, _) = self.window_size(client, window);
        let showing = &mut self.buffers[buffer];
        let start = showing.last_window_start;
        let point_marker = showing.create_marker_without_undo(point, InsertionType::StayBefore);
        let start_marker = showing.create_marker_without_undo(start, InsertionType::StayBefore);
        let target = &mut self.client_mut(client).frame.windows[window];
        target.buffer = buffer;
        target.point = point_marker;
        target.start = start_marker;
        target.start_row_skip = 0;
        target.start_cols = cols;
        target.hscroll = 0;
    }

    pub(super) fn show_in_selected_window(&mut self, client: ClientId, buffer: BufferId) {
        self.switch_to_buffer(client, buffer);
    }

    /// Emacs `switch-to-buffer` with `switch-to-buffer-preserve-window-
    /// point` t: a buffer the window showed before comes back with that
    /// window's start and point.
    pub(super) fn switch_to_buffer(&mut self, client: ClientId, buffer: BufferId) {
        let window = self.selected_window_id(client);
        if self.window(client, window).buffer != buffer {
            let entry = self.frame(client).prev_buffers[window]
                .iter()
                .find(|entry| entry.buffer == buffer)
                .copied();
            self.set_window_buffer(client, window, buffer);
            if let Some(entry) = entry {
                let shown = &self.buffers[buffer];
                let start = shown.marker_pos(entry.start);
                let point = shown.marker_pos(entry.point);
                self.set_window_start(
                    client,
                    window,
                    ScreenLine {
                        line_start: start,
                        row: 0,
                    },
                );
                self.set_window_point(client, window, point);
            }
        }
        self.select_window(client, window);
    }

    /// Emacs `replace-buffer-in-windows` for a buffer being killed: every
    /// window showing it switches to its previous buffer, and no window
    /// remembers it.
    pub(super) fn replace_buffer_in_windows(&mut self, buffer: BufferId) {
        let clients: Vec<ClientId> = self.clients.keys().copied().collect();
        for client in clients {
            for window in self.frame(client).order() {
                if self.window(client, window).buffer == buffer
                    && !self.switch_to_prev_buffer(client, window, buffer)
                {
                    if self.frame(client).sole() {
                        let other = self.other_buffer(buffer);
                        self.set_window_buffer(client, window, other);
                    } else {
                        self.delete_window(client, window);
                    }
                }
            }
            let frame = &mut self.client_mut(client).frame;
            for history in frame.prev_buffers.values_mut() {
                // The buffer's markers go with it.
                history.retain(|entry| entry.buffer != buffer);
            }
        }
    }

    /// Emacs `switch-to-prev-buffer` for a buffer about to be killed: the
    /// window's most recent other buffer, at its start and point, or else
    /// the most recent buffer of all. Returns whether one was found.
    fn switch_to_prev_buffer(
        &mut self,
        client: ClientId,
        window: WindowId,
        killed: BufferId,
    ) -> bool {
        let entry = self.frame(client).prev_buffers[window]
            .iter()
            .find(|entry| entry.buffer != killed && self.buffers.contains_key(entry.buffer))
            .copied();
        if let Some(entry) = entry {
            let shown = &self.buffers[entry.buffer];
            let start = shown.marker_pos(entry.start);
            let point = shown.marker_pos(entry.point);
            self.set_window_buffer(client, window, entry.buffer);
            self.set_window_start(
                client,
                window,
                ScreenLine {
                    line_start: start,
                    row: 0,
                },
            );
            self.set_window_point(client, window, point);
            return true;
        }
        let next =
            self.recency.iter().copied().find(|id| {
                *id != killed && !self.buffers[*id].name.starts_with(INTERNAL_NAME_PREFIX)
            });
        match next {
            Some(next) => {
                self.set_window_buffer(client, window, next);
                true
            }
            None => false,
        }
    }

    // ---- switch-to-buffer -------------------------------------------------

    /// `switch-to-buffer` (`C-x b`) reads a buffer name; the default is the
    /// most recent buffer no window of this frame shows.
    pub(crate) fn switch_to_buffer_command(&mut self, client: ClientId) {
        let current = self.selected_buffer(client);
        let default = match self.other_buffer_for(client, current) {
            Some(buffer) => self.buffers[buffer].name.clone(),
            None => SCRATCH_BUFFER.to_string(),
        };
        let mode = Mode::Line {
            history: HistoryKind::BufferName,
            defaults: vec![default.clone()],
            require: None,
            allow_empty: true,
        };
        self.read_from_minibuffer(
            client,
            format!("Switch to buffer (default {default}): "),
            String::new(),
            mode,
            Purpose::SwitchToBuffer { default },
        );
    }

    /// Switches to the buffer named `name`, creating it when there is
    /// none, as `window-normalize-buffer-to-switch-to` does.
    pub(super) fn switch_to_buffer_named(&mut self, client: ClientId, name: &str) {
        let existing = self
            .buffers
            .iter()
            .find(|(_, buffer)| buffer.name == name)
            .map(|(id, _)| id);
        let buffer = match existing {
            Some(buffer) => buffer,
            None => {
                let buffer = self.create_buffer(name);
                self.track_journal(buffer);
                buffer
            }
        };
        self.switch_to_buffer(client, buffer);
    }

    /// Emacs `other-buffer`: the most recent buffer other than `excluding`
    /// that no window of the client's frame shows, else the most recent
    /// shown one.
    fn other_buffer_for(&self, client: ClientId, excluding: BufferId) -> Option<BufferId> {
        let frame = self.frame(client);
        let shown = |buffer: BufferId| frame.windows.values().any(|window| window.buffer == buffer);
        let candidates = self.recency.iter().copied().filter(|id| {
            *id != excluding && !self.buffers[*id].name.starts_with(INTERNAL_NAME_PREFIX)
        });
        let mut visible = None;
        for candidate in candidates {
            if !shown(candidate) {
                return Some(candidate);
            }
            visible.get_or_insert(candidate);
        }
        visible
    }

    // ---- display-buffer -------------------------------------------------

    /// Emacs `display-buffer` with its default actions: a window already
    /// showing the buffer, else a new window split off, else a window that
    /// showed it before, else the least recently used window.
    fn display_buffer(&mut self, client: ClientId, buffer: BufferId) -> WindowId {
        let frame = self.frame(client);
        let selected = frame.selected;
        let order = frame.order_from_selected();
        if let Some(window) = order
            .iter()
            .copied()
            .find(|id| frame.windows[*id].buffer == buffer)
        {
            return window;
        }
        let largest = self.largest_window(client);
        let lru = self.lru_window(client);
        for candidate in [largest, lru] {
            if let Some(window) = self.split_window_sensibly(client, candidate) {
                self.set_window_buffer(client, window, buffer);
                // A new window has no history (`window--display-buffer`).
                let history =
                    std::mem::take(&mut self.client_mut(client).frame.prev_buffers[window]);
                for entry in history {
                    let target = &mut self.buffers[entry.buffer];
                    target.delete_marker(entry.start);
                    target.delete_marker(entry.point);
                }
                return window;
            }
        }
        let frame = self.frame(client);
        let mut previous = None;
        let mut previous_selected = None;
        for window in frame.order() {
            if frame.prev_buffers[window]
                .iter()
                .any(|entry| entry.buffer == buffer)
            {
                if window == selected {
                    previous_selected = Some(window);
                } else {
                    previous = Some(window);
                }
            }
        }
        if let Some(window) = previous.or(previous_selected) {
            self.set_window_buffer(client, window, buffer);
            return window;
        }
        let window = self.lru_window(client);
        self.set_window_buffer(client, window, buffer);
        self.client_mut(client).frame.even_heights(window);
        window
    }

    /// Emacs `get-largest-window`: by area, the first of equals in cyclic
    /// order from the selected window.
    fn largest_window(&self, client: ClientId) -> WindowId {
        let frame = self.frame(client);
        let mut best: Option<(WindowId, u32)> = None;
        for window in frame.order_from_selected() {
            let place = frame.window_box(window);
            let area = u32::from(place.cols) * u32::from(place.rows);
            if best.is_none_or(|(_, size)| area > size) {
                best = Some((window, area));
            }
        }
        best.expect("a frame has a window").0
    }

    /// Emacs `get-lru-window`: the least recently used full-width window
    /// other than the selected one; failing that, the least recently
    /// used of the rest.
    fn lru_window(&self, client: ClientId) -> WindowId {
        let frame = self.frame(client);
        let mut best: Option<(WindowId, u64)> = None;
        let mut second: Option<(WindowId, u64)> = None;
        for window in frame.order_from_selected() {
            let time = frame.windows[window].use_time;
            let slot = if window == frame.selected || !frame.full_width(window) {
                &mut second
            } else {
                &mut best
            };
            if slot.is_none_or(|(_, oldest)| time < oldest) {
                *slot = Some((window, time));
            }
        }
        best.or(second).expect("a frame has a window").0
    }

    /// Emacs `split-window-sensibly`: below if the window is tall enough,
    /// else to the right if wide enough, else below if it is the frame's
    /// only window. Returns the new window.
    fn split_window_sensibly(&mut self, client: ClientId, window: WindowId) -> Option<WindowId> {
        let frame = self.frame(client);
        let rows = frame.window_box(window).rows;
        let body_cols = self.window_area(client, window).cols;
        let tall_for = |threshold: u16| {
            rows >= threshold.max(2 * WINDOW_MIN_HEIGHT.max(SPLITTABLE_MIN_HEIGHT))
        };
        let wide =
            body_cols >= SPLIT_WIDTH_THRESHOLD.max(2 * WINDOW_MIN_WIDTH.max(SPLITTABLE_MIN_WIDTH));
        let side_by_side = if tall_for(SPLIT_HEIGHT_THRESHOLD) {
            false
        } else if wide {
            true
        } else if frame.sole() && tall_for(0) {
            false
        } else {
            return None;
        };
        let new_size = self
            .checked_split_size(client, window, side_by_side, None)
            .ok()?;
        Some(self.split_window(client, window, side_by_side, new_size))
    }

    // ---- The Buffer Menu --------------------------------------------------

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
        let target = &mut self.buffers[list];
        target.reset_text(
            Text::from_original(Sources::memory(Arc::from(text.into_bytes()))),
            limits,
        );
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

    // ---- Window start across width changes --------------------------------

    /// A window whose text width changed since its start row was counted
    /// starts at the row that now holds the same text. Reads only cached
    /// text, like the rest of redisplay.
    pub(super) fn refit_window_start(
        &mut self,
        client: ClientId,
        window: WindowId,
    ) -> Result<(), ReadError> {
        let (cols, _) = self.window_size(client, window);
        let target = *self.window(client, window);
        if target.start_cols == cols {
            return Ok(());
        }
        if target.start_row_skip > 0 {
            let text = self.buffers[target.buffer].text.clone();
            let tab_width = self.tab_width();
            let reader = Reader::new(&text, Fetch::CachedOnly);
            let start = self.window_start(client, window);
            let first_shown = Layout::new(reader, target.start_cols, tab_width).row_start(start)?;
            let refit = Layout::new(reader, cols, tab_width).screen_line_of(first_shown)?;
            self.set_window_start(client, window, refit);
        }
        self.client_mut(client).frame.windows[window].start_cols = cols;
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
