//! Mouse input in a terminal frame with `xterm-mouse-mode` on, following
//! Emacs's mouse.el and mwheel.el: a click sets point and selects the
//! window, a drag sets the region, double and triple clicks select a word
//! and a line, the right button extends the region, the wheel scrolls the
//! window under the pointer, and modelines and borders resize windows.

use std::ops::{ControlFlow, Range};

use emx_proto::{ClientKind, ServerMsg};
use emx_text::Text;
use emx_types::{MouseButton, MouseEvent, MouseKind};

use super::Editor;
use crate::command::{CommandContext, copy_region_as_kill, scroll_window_lines};
use crate::event::{ClientId, Effect};
use crate::window::{Fetch, LineEvent, WindowId};

/// How far a word or line selection looks around the click for its ends;
/// a longer line is cut there.
const SELECTION_SCAN_BYTES: u64 = 64 * 1024;
/// Clicks cycle through these, as mouse.el's `mouse-start-end` does with
/// the click count modulo 3.
const SELECTION_UNITS: u8 = 3;
const SCROLL_UP: i64 = 1;
const SCROLL_DOWN: i64 = -1;

/// What a click selects, and a drag extends by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectUnit {
    Char,
    Word,
    Line,
}

/// A drag in progress, from the press that started it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MouseDrag {
    Text {
        window: WindowId,
        anchor: u64,
        unit: SelectUnit,
    },
    /// A modeline being dragged moves the window's bottom edge.
    Modeline { window: WindowId, row: u16 },
    /// A vertical border being dragged moves the window's right edge.
    Border { window: WindowId, col: u16 },
    /// The file panel's border being dragged resizes the panel.
    PanelBorder,
}

/// What is drawn at a cell of the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Spot {
    /// A window's text, at a row and column of its text area.
    Text {
        window: WindowId,
        row: u16,
        col: u16,
    },
    Modeline(WindowId),
    Border(WindowId),
    /// A row of the file panel's text.
    Panel {
        window: WindowId,
        row: u16,
    },
    PanelBorder(WindowId),
    Elsewhere,
}

impl Editor {
    /// Emacs `xterm-mouse-mode`, for every terminal client of the session.
    pub(crate) fn toggle_mouse_mode(&mut self) -> bool {
        self.mouse = !self.mouse;
        let terminals: Vec<ClientId> = self
            .clients
            .iter()
            .filter(|(_, client)| client.kind == ClientKind::Tui)
            .map(|(id, _)| *id)
            .collect();
        for client in terminals {
            self.client_mut(client).mouse_drag = None;
            self.effects
                .push(Effect::Send(client, ServerMsg::Modes { mouse: self.mouse }));
        }
        self.mouse
    }

    pub(super) fn handle_mouse(&mut self, client_id: ClientId, event: MouseEvent) {
        // Reports can still arrive just after the mode was turned off.
        if !self.mouse {
            return;
        }
        let client = self.client(client_id);
        // The minibuffer, isearch and query-replace keep the keyboard, as
        // they keep focus in a terminal Emacs.
        if client.minibuffer.is_some()
            || client.isearch.is_some()
            || client.query_replace.is_some()
            || client.register_read.is_some()
            || client.macro_executing > 0
        {
            return;
        }
        match (event.kind, event.button) {
            (MouseKind::Press, button) if button.is_wheel() => self.mouse_wheel(client_id, event),
            (MouseKind::Press, MouseButton::Left) => self.mouse_press(client_id, event),
            (MouseKind::Press, MouseButton::Right) => self.mouse_extend(client_id, event),
            (MouseKind::Drag, MouseButton::Left) => self.mouse_drag_to(client_id, event),
            (MouseKind::Release, _) => self.mouse_release(client_id),
            _ => {}
        }
    }

    fn spot(&self, client_id: ClientId, col: u16, row: u16) -> Spot {
        let frame = self.frame(client_id);
        if let Some((window, _)) = frame.panel_box() {
            let area = self.window_area(client_id, window);
            let within_rows =
                row >= area.box_top && row < area.box_top.saturating_add(area.box_rows);
            if within_rows && area.border == Some(col) {
                return Spot::PanelBorder(window);
            }
            if within_rows && col < area.left.saturating_add(area.cols) {
                if row >= area.top && row < area.top.saturating_add(area.text_rows) {
                    return Spot::Panel {
                        window,
                        row: row - area.top,
                    };
                }
                return Spot::Elsewhere;
            }
        }
        for (window, _) in frame.boxes() {
            let area = self.window_area(client_id, window);
            if row < area.box_top || row >= area.box_top.saturating_add(area.box_rows) {
                continue;
            }
            if area.border == Some(col) {
                return Spot::Border(window);
            }
            if col < area.left || col >= area.left.saturating_add(area.cols) {
                continue;
            }
            if area.modeline_row == Some(row) {
                return Spot::Modeline(window);
            }
            if row >= area.top && row < area.top.saturating_add(area.text_rows) {
                return Spot::Text {
                    window,
                    row: row - area.top,
                    col: col - area.left,
                };
            }
            return Spot::Elsewhere;
        }
        Spot::Elsewhere
    }

    /// The buffer position shown at a row and column of the window's text
    /// area: the character drawn there, the end of the line when the cell
    /// is past it, the end of the buffer below its last line. `None` when
    /// the text there is not read yet.
    fn position_at(
        &self,
        client_id: ClientId,
        window_id: WindowId,
        row: u16,
        col: u16,
    ) -> Option<u64> {
        let window = self.window(client_id, window_id);
        let buffer = self.buffer(window.buffer);
        let text = buffer.text.clone();
        let layout = self.window_layout(client_id, window_id, &text, Fetch::CachedOnly);
        let start = layout
            .resolve_start(
                self.window_start(client_id, window_id),
                &buffer.checkpoints,
                &text,
            )
            .ok()?;
        let (_, height) = self.window_size(client_id, window_id);
        let col = usize::from(col);
        let mut found = None;
        let walk = layout
            .walk_window(start, height, &mut |event_row, event| {
                if event_row < row {
                    return ControlFlow::Continue(());
                }
                if event_row > row {
                    return ControlFlow::Break(());
                }
                match event {
                    LineEvent::Unit { pos, col: at, .. } | LineEvent::End { pos, col: at, .. }
                        if *at <= col || found.is_none() =>
                    {
                        found = Some(*pos);
                    }
                    _ => {}
                }
                ControlFlow::Continue(())
            })
            .ok()?;
        match found {
            Some(pos) => Some(pos),
            None if walk.end_visible => Some(text.len()),
            None => None,
        }
    }

    /// Ends a key sequence or prefix argument a click interrupts.
    fn mouse_interrupts(&mut self, client_id: ClientId) {
        if self.client(client_id).command_loop.mid_command() {
            self.client_mut(client_id).command_loop.end_command();
            self.update_echo(client_id);
        }
    }

    fn mouse_press(&mut self, client_id: ClientId, event: MouseEvent) {
        let drag = match self.spot(client_id, event.col, event.row) {
            Spot::Text { window, row, col } => {
                let Some(pos) = self.position_at(client_id, window, row, col) else {
                    return;
                };
                self.mouse_interrupts(client_id);
                if window != self.frame(client_id).selected {
                    self.select_window(client_id, window);
                }
                let unit = match (event.clicks.max(1) - 1) % SELECTION_UNITS {
                    0 => SelectUnit::Char,
                    1 => SelectUnit::Word,
                    _ => SelectUnit::Line,
                };
                let name = match unit {
                    SelectUnit::Char => "mouse-set-point",
                    SelectUnit::Word | SelectUnit::Line => "mouse-set-region",
                };
                self.run_command(client_id, name, None, |ctx| {
                    select_between(ctx, pos, pos, unit);
                    Ok(())
                });
                MouseDrag::Text {
                    window,
                    anchor: pos,
                    unit,
                }
            }
            Spot::Modeline(window) => {
                self.mouse_interrupts(client_id);
                if window != self.frame(client_id).selected {
                    self.select_window(client_id, window);
                }
                MouseDrag::Modeline {
                    window,
                    row: event.row,
                }
            }
            Spot::Border(window) => {
                self.mouse_interrupts(client_id);
                MouseDrag::Border {
                    window,
                    col: event.col,
                }
            }
            Spot::Panel { row, .. } => {
                self.mouse_interrupts(client_id);
                self.panel_click(client_id, row);
                return;
            }
            Spot::PanelBorder(_) => {
                self.mouse_interrupts(client_id);
                MouseDrag::PanelBorder
            }
            Spot::Elsewhere => return,
        };
        self.client_mut(client_id).mouse_drag = Some(drag);
    }

    fn mouse_drag_to(&mut self, client_id: ClientId, event: MouseEvent) {
        let Some(drag) = self.client(client_id).mouse_drag else {
            return;
        };
        let live = |editor: &Editor, window: WindowId| {
            editor.frame(client_id).windows.contains_key(window)
        };
        match drag {
            MouseDrag::Text {
                window,
                anchor,
                unit,
            } => {
                if !live(self, window) || window != self.frame(client_id).selected {
                    self.client_mut(client_id).mouse_drag = None;
                    return;
                }
                // The pointer may leave the window; the region follows it
                // to the nearest of the window's own cells.
                let area = self.window_area(client_id, window);
                if area.text_rows == 0 || area.cols == 0 {
                    return;
                }
                let row = event.row.clamp(area.top, area.top + area.text_rows - 1) - area.top;
                let col = event.col.clamp(area.left, area.left + area.cols - 1) - area.left;
                let Some(pos) = self.position_at(client_id, window, row, col) else {
                    return;
                };
                self.run_command(client_id, "mouse-drag-region", None, |ctx| {
                    select_between(ctx, anchor, pos, unit);
                    if unit == SelectUnit::Char {
                        ctx.activate_mark();
                    }
                    Ok(())
                });
            }
            MouseDrag::Modeline { window, row } => {
                if !live(self, window) {
                    self.client_mut(client_id).mouse_drag = None;
                    return;
                }
                let delta = i32::from(event.row) - i32::from(row);
                let moved = self
                    .client_mut(client_id)
                    .frame
                    .adjust_edge(window, false, delta);
                self.client_mut(client_id).mouse_drag = Some(MouseDrag::Modeline {
                    window,
                    row: moved_by(row, moved),
                });
            }
            MouseDrag::PanelBorder => {
                // The border is the panel's last column.
                self.resize_panel(client_id, event.col.saturating_add(1));
            }
            MouseDrag::Border { window, col } => {
                if !live(self, window) {
                    self.client_mut(client_id).mouse_drag = None;
                    return;
                }
                let delta = i32::from(event.col) - i32::from(col);
                let moved = self
                    .client_mut(client_id)
                    .frame
                    .adjust_edge(window, true, delta);
                self.client_mut(client_id).mouse_drag = Some(MouseDrag::Border {
                    window,
                    col: moved_by(col, moved),
                });
            }
        }
    }

    /// Ends a drag. With Emacs `mouse-drag-copy-region`, a drag or a double
    /// or triple click that leaves a region copies it.
    fn mouse_release(&mut self, client_id: ClientId) {
        let drag = self.client_mut(client_id).mouse_drag.take();
        if !self.config.mouse_drag_copy_region {
            return;
        }
        let Some(MouseDrag::Text { window, .. }) = drag else {
            return;
        };
        let buffer = self.selected_buffer(client_id);
        if window != self.frame(client_id).selected || !self.buffers[buffer].mark_active {
            return;
        }
        self.run_command(client_id, "mouse-set-region", None, |ctx| {
            if ctx.use_region() {
                copy_region_as_kill(ctx)
            } else {
                Ok(())
            }
        });
    }

    /// Emacs `mouse-save-then-kill` without the kill: the region runs from
    /// the mark (point, if the region was not active) to the click.
    fn mouse_extend(&mut self, client_id: ClientId, event: MouseEvent) {
        let Spot::Text { window, row, col } = self.spot(client_id, event.col, event.row) else {
            return;
        };
        let Some(pos) = self.position_at(client_id, window, row, col) else {
            return;
        };
        self.mouse_interrupts(client_id);
        if window != self.frame(client_id).selected {
            self.select_window(client_id, window);
        }
        self.run_command(client_id, "mouse-save-then-kill", None, |ctx| {
            if !ctx.region_active() {
                let point = ctx.point();
                ctx.set_mark_position(point);
            }
            ctx.set_point(pos);
            ctx.activate_mark();
            Ok(())
        });
    }

    /// Emacs `mwheel-scroll` with `mouse-wheel-follow-mouse`: the window
    /// under the pointer scrolls without being selected, stopping quietly
    /// at either end of the buffer.
    fn mouse_wheel(&mut self, client_id: ClientId, event: MouseEvent) {
        let window = match self.spot(client_id, event.col, event.row) {
            Spot::Text { window, .. }
            | Spot::Modeline(window)
            | Spot::Border(window)
            | Spot::Panel { window, .. }
            | Spot::PanelBorder(window) => window,
            Spot::Elsewhere => return,
        };
        let direction = match event.button {
            MouseButton::WheelDown => SCROLL_UP,
            MouseButton::WheelUp => SCROLL_DOWN,
            _ => return,
        };
        let lines = self.config.mouse_wheel_lines;
        self.run_command(client_id, "mwheel-scroll", None, |ctx| {
            let client = ctx.client;
            ctx.window = window;
            ctx.buffer = ctx.editor().window(client, window).buffer;
            for _ in 0..lines {
                if scroll_window_lines(ctx, direction).is_err() {
                    break;
                }
            }
            Ok(())
        });
    }
}

fn moved_by(at: u16, moved: i32) -> u16 {
    u16::try_from(i32::from(at) + moved).unwrap_or(at)
}

/// Point and the mark around `anchor` and `pos` by `unit`: a character
/// selection puts point at `pos` (and the mark at `anchor` once dragged); a
/// word or line selection covers every unit from one to the other, point
/// at the end toward `pos`.
fn select_between(ctx: &mut CommandContext, anchor: u64, pos: u64, unit: SelectUnit) {
    if unit == SelectUnit::Char {
        if anchor == pos {
            ctx.set_point(pos);
            ctx.deactivate_mark_now();
        } else {
            ctx.set_mark_position(anchor);
            ctx.set_point(pos);
        }
        return;
    }
    let text = ctx.buffer().text.clone();
    let from = unit_range(&text, anchor, unit);
    let to = unit_range(&text, pos, unit);
    if pos >= anchor {
        ctx.set_mark_position(from.start.min(to.start));
        ctx.set_point(from.end.max(to.end));
    } else {
        ctx.set_mark_position(from.end.max(to.end));
        ctx.set_point(from.start.min(to.start));
    }
    ctx.activate_mark();
}

/// Character classes a double click selects runs of, as the standard
/// syntax table classes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Symbol,
    Space,
    Newline,
    Punctuation,
}

fn class(character: char) -> Class {
    match character {
        '\n' => Class::Newline,
        ' ' | '\t' => Class::Space,
        '_' => Class::Symbol,
        character if character.is_alphanumeric() => Class::Word,
        _ => Class::Punctuation,
    }
}

/// The word or line around `pos`, within the scan limit.
fn unit_range(text: &Text, pos: u64, unit: SelectUnit) -> Range<u64> {
    let length = text.len();
    let from = pos.saturating_sub(SELECTION_SCAN_BYTES);
    let to = pos.saturating_add(SELECTION_SCAN_BYTES).min(length);
    let mut bytes = Vec::new();
    if text.read_cached(from..to, &mut bytes).is_err() {
        return pos..pos;
    }
    let at = usize::try_from(pos - from).expect("within the scan window");
    let line_start = bytes[..at]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let line_end = bytes[at..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(bytes.len(), |index| at + index);
    let range = match unit {
        SelectUnit::Char => at..at,
        SelectUnit::Line => line_start..(line_end + 1).min(bytes.len()),
        SelectUnit::Word => word_at(&bytes[line_start..], at - line_start, line_end - line_start)
            .map(|run| line_start + run.start..line_start + run.end)
            .unwrap_or(at..(at + 1).min(bytes.len())),
    };
    from + range.start as u64..from + range.end as u64
}

/// The run of one class around byte `at` of a line (`end` is where its
/// newline is); bytes that are not UTF-8 count as punctuation.
fn word_at(line: &[u8], at: usize, end: usize) -> Option<Range<usize>> {
    if at >= end {
        return None;
    }
    let mut characters: Vec<(usize, usize, Class)> = Vec::new();
    let mut index = 0;
    while index < end {
        let rest = &line[index..end];
        let (length, character) = match std::str::from_utf8(&rest[..rest.len().min(4)]) {
            Ok(text) => {
                let character = text.chars().next().expect("non-empty");
                (character.len_utf8(), Some(character))
            }
            Err(error) if error.valid_up_to() > 0 => {
                let text = std::str::from_utf8(&rest[..error.valid_up_to()])
                    .expect("valid_up_to bounds a valid prefix");
                let character = text.chars().next().expect("non-empty");
                (character.len_utf8(), Some(character))
            }
            Err(_) => (1, None),
        };
        characters.push((index, length, character.map_or(Class::Punctuation, class)));
        index += length;
    }
    let position = characters
        .iter()
        .position(|(start, length, _)| *start <= at && at < start + length)?;
    let wanted = characters[position].2;
    let first = characters[..position]
        .iter()
        .rposition(|(_, _, class)| *class != wanted)
        .map_or(0, |index| index + 1);
    let last = characters[position..]
        .iter()
        .position(|(_, _, class)| *class != wanted)
        .map_or(characters.len(), |index| position + index);
    let (start, _, _) = characters[first];
    let (end_start, end_length, _) = characters[last - 1];
    Some(start..end_start + end_length)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_at_selects_runs_of_one_class() {
        let line = "let foo_bar = 42;  x".as_bytes();
        let end = line.len();
        let word = |at| word_at(line, at, end).map(|range| &line[range]);
        assert_eq!(word(5), Some(&b"foo"[..]));
        assert_eq!(word(7), Some(&b"_"[..]));
        assert_eq!(word(8), Some(&b"bar"[..]));
        assert_eq!(word(12), Some(&b"="[..]));
        assert_eq!(word(14), Some(&b"42"[..]));
        assert_eq!(word(17), Some(&b"  "[..]));
        assert_eq!(word(end), None);
        let wide = "héllo wörld".as_bytes();
        assert_eq!(
            word_at(wide, 8, wide.len()).map(|range| &wide[range]),
            Some("wörld".as_bytes())
        );
    }
}
