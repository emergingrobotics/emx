//! A client's windows: splitting, deleting and selecting them, which
//! buffer each one shows and where, and the Buffer Menu. Ported from
//! Emacs 29's window.el, window.c and buff-menu.el.

use std::ops::ControlFlow;

use emx_text::{ReadError, Text};
use emx_types::Key;

use super::{Editor, Purpose};
use crate::buffer::BufferId;
use crate::command::CommandError;
use crate::event::ClientId;
use crate::keymap::Lookup;
use crate::marker::InsertionType;
use crate::minibuffer::{HistoryKind, Minibuffer, Mode};
use crate::window::{
    Fetch, HSCROLL_END_OF_LINE_ROOM, HSCROLL_MARGIN, Layout, LineEvent, PrevBuffer, Reader,
    ScreenLine, TRUNCATE_PARTIAL_WIDTH_WINDOWS, WINDOW_MIN_HEIGHT, WINDOW_MIN_WIDTH,
    WINDOW_SAFE_MIN_HEIGHT, WINDOW_SAFE_MIN_WIDTH, Window, WindowId,
};

const SCRATCH_BUFFER: &str = "*scratch*";
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
/// Buffers whose names start with a space are internal: Emacs neither
/// lists nor records them.
pub(super) const INTERNAL_NAME_PREFIX: char = ' ';

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
            defaults: vec![default.as_bytes().to_vec()],
            require: None,
            allow_empty: true,
        };
        let completer = self.buffer_completer(client, &default, false);
        let minibuffer = Minibuffer::new(
            format!("Switch to buffer (default {default}): "),
            Vec::new(),
            mode,
            Purpose::SwitchToBuffer { default },
        )
        .with_completion(completer);
        self.start_minibuffer(client, minibuffer);
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
    pub(super) fn display_buffer(&mut self, client: ClientId, buffer: BufferId) -> WindowId {
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

    // ---- Truncation and horizontal scrolling --------------------------------

    /// `Some(hscroll)` when the window truncates lines instead of wrapping
    /// them: Emacs `truncated-partial-width-window-p` (not as wide as the
    /// frame and narrower than `truncate-partial-width-windows`), or the
    /// window is scrolled horizontally, which truncates any window.
    pub(crate) fn window_hscroll(&self, client: ClientId, window: WindowId) -> Option<u64> {
        let frame = self.frame(client);
        let place = frame.window_box(window);
        let hscroll = frame.windows[window].hscroll;
        let partial_width =
            place.cols != frame.cols() && place.cols < TRUNCATE_PARTIAL_WIDTH_WINDOWS;
        (partial_width || hscroll > 0).then_some(u64::from(hscroll))
    }

    /// How the window lays out `text`: wrapped, or truncated and scrolled.
    pub(crate) fn window_layout<'t>(
        &self,
        client: ClientId,
        window: WindowId,
        text: &'t Text,
        fetch: Fetch,
    ) -> Layout<'t> {
        let (cols, _) = self.window_size(client, window);
        let reader = Reader::new(text, fetch);
        match self.window_hscroll(client, window) {
            Some(hscroll) => Layout::truncating(reader, cols, self.tab_width(), hscroll),
            None => Layout::new(reader, cols, self.tab_width()),
        }
    }

    /// Emacs auto-hscroll (`hscroll_window_tree`, `hscroll-step` 0): a
    /// truncating window scrolls so point's column is not within
    /// `hscroll-margin` of an edge it can scroll past. Reads only cached
    /// text, like the rest of redisplay.
    pub(super) fn auto_hscroll(
        &mut self,
        client: ClientId,
        window: WindowId,
    ) -> Result<(), ReadError> {
        let Some(hscroll) = self.window_hscroll(client, window) else {
            return Ok(());
        };
        let target = *self.window(client, window);
        let text = self.buffers[target.buffer].text.clone();
        let point = self.buffers[target.buffer].marker_pos(target.point);
        let (cols, _) = self.window_size(client, window);
        let layout = Layout::truncating(
            Reader::new(&text, Fetch::CachedOnly),
            cols,
            self.tab_width(),
            0,
        );
        let line_start = layout.line_start(point)?;
        let mut column = 0;
        let mut end = (0, 0);
        layout.walk_line(line_start, &mut |event| {
            match event {
                LineEvent::Unit { pos, col, .. } if *pos <= point => column = *col,
                LineEvent::End { pos, col, .. } => {
                    if *pos <= point {
                        column = *col;
                    }
                    end = (*pos, *col);
                }
                _ => {}
            }
            ControlFlow::Continue(())
        })?;
        let line = HorizontalLine {
            column: column as u64,
            width: end.1 as u64,
            at_end: point >= end.0,
        };
        let window_width = u64::from(self.frame(client).window_box(window).cols);
        let text_width = u64::from(cols).saturating_sub(1);
        let wanted = wanted_hscroll(hscroll, window_width, text_width, line);
        if wanted != hscroll {
            // Only a line over four billion columns long could overflow.
            self.client_mut(client).frame.windows[window].hscroll =
                u32::try_from(wanted).unwrap_or(u32::MAX);
        }
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
        // A truncating window shows each line in one row, from its start.
        if target.start_row_skip > 0 && self.window_hscroll(client, window).is_some() {
            let line_start = self.window_start(client, window).line_start;
            self.set_window_start(client, window, ScreenLine { line_start, row: 0 });
            return Ok(());
        }
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

/// Point's line as auto-hscroll sees it, in columns from the line start.
#[derive(Debug, Clone, Copy)]
struct HorizontalLine {
    column: u64,
    width: u64,
    at_end: bool,
}

/// The hscroll Emacs picks for a window `window_width` columns wide, its
/// border included, of which `text_width` show text before the
/// truncation glyph. Point within the margin of the left edge of a
/// scrolled window, or of the right edge of a line that goes on past it,
/// is brought to the middle, or near the right edge when it is at the end
/// of its line.
fn wanted_hscroll(hscroll: u64, window_width: u64, text_width: u64, line: HorizontalLine) -> u64 {
    let x = line.column as i128 - hscroll as i128;
    let margin = i128::from(HSCROLL_MARGIN);
    let truncated_right = line.width > hscroll + text_width;
    let near_left = hscroll > 0 && x < margin;
    let near_right = truncated_right && x >= window_width as i128 - margin;
    if !(near_left || near_right) {
        return hscroll;
    }
    let keep = if line.at_end {
        window_width.saturating_sub(HSCROLL_END_OF_LINE_ROOM)
    } else {
        window_width / 2
    };
    line.column.saturating_sub(keep)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(column: u64, width: u64, at_end: bool) -> HorizontalLine {
        HorizontalLine {
            column,
            width,
            at_end,
        }
    }

    #[test]
    fn wanted_hscroll_matches_emacs_captures() {
        // emacs -nw -Q, 80 columns, C-x 3: the left window is 40 wide with
        // 38 text columns, the right one 40 with 39.
        assert_eq!(wanted_hscroll(0, 40, 38, line(34, 100, false)), 0);
        assert_eq!(wanted_hscroll(0, 40, 38, line(35, 100, false)), 15);
        assert_eq!(wanted_hscroll(0, 40, 39, line(35, 100, false)), 15);
        assert_eq!(wanted_hscroll(15, 40, 38, line(20, 100, false)), 15);
        assert_eq!(wanted_hscroll(15, 40, 38, line(19, 100, false)), 0);
        assert_eq!(wanted_hscroll(0, 40, 38, line(100, 100, true)), 64);
        assert_eq!(wanted_hscroll(0, 40, 38, line(43, 43, true)), 7);
        assert_eq!(wanted_hscroll(0, 40, 38, line(38, 38, true)), 0);
        assert_eq!(wanted_hscroll(0, 40, 38, line(39, 39, true)), 3);
        assert_eq!(wanted_hscroll(0, 40, 39, line(39, 39, true)), 0);
        // 70 columns: both windows are 35 wide.
        assert_eq!(wanted_hscroll(0, 35, 33, line(29, 100, false)), 0);
        assert_eq!(wanted_hscroll(0, 35, 33, line(30, 100, false)), 13);
        assert_eq!(wanted_hscroll(0, 35, 34, line(30, 100, false)), 13);
    }
}
