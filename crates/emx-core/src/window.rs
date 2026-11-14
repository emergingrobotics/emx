//! Windows and their layout: how a window's buffer text breaks into screen
//! rows, and how screen rows map back to buffer positions. Rendering and
//! the scrolling commands share this one layout so they always agree on
//! where text appears.

use std::ops::{ControlFlow, Range};

use emx_text::{DisplayUnit, ReadError, Text, display_units, grapheme_text};
use slotmap::{SecondaryMap, SlotMap};

use crate::buffer::BufferId;
use crate::marker::MarkerId;

slotmap::new_key_type! {
    pub struct WindowId;
}

/// Bytes read per step while laying out a line; doubled while a single
/// display unit fills the whole read.
const LINE_READ_CHUNK: u64 = 4096;
/// Bytes read per step while looking back for a line start.
const LINE_START_SCAN_CHUNK: u64 = 4096;
/// A UTF-8 sequence is at most this long, so a unit starting this far
/// before the end of a read is complete.
const UTF8_MAX_LEN: usize = 4;
/// On a terminal the last column of a window holds the continuation glyph,
/// so text never uses it except for the cursor at the end of a line.
const CONTINUATION_COLUMNS: usize = 1;
pub(crate) const CONTINUATION_GLYPH: &str = "\\";
/// How a glyphless character shows: a blank, as Emacs's `thin-space`
/// glyphless display is on a terminal.
const GLYPHLESS_GLYPH: &str = " ";
const CONTROL_CARET: char = '^';
/// `^X` shows control character X - 0x40; DEL shows as `^?`.
const CONTROL_TO_CARET: u8 = 0x40;
const DELETE: u8 = 0x7f;
const DELETE_GLYPH: char = '?';
const ECHO_ROWS: u16 = 1;
const MODELINE_ROWS: u16 = 1;

/// Emacs `window-min-height`: splitting leaves the split window at least
/// this many rows, its modeline included.
pub(crate) const WINDOW_MIN_HEIGHT: u16 = 4;
/// Emacs `window-min-width`: splitting leaves the split window at least
/// this many columns, its vertical border included.
pub(crate) const WINDOW_MIN_WIDTH: u16 = 10;
/// Emacs `window-safe-min-height`: no size argument makes a window smaller.
pub(crate) const WINDOW_SAFE_MIN_HEIGHT: u16 = 1;
/// Emacs `window-safe-min-width`.
pub(crate) const WINDOW_SAFE_MIN_WIDTH: u16 = 2;
/// A window with another to its right gives its last column to the
/// vertical border between them, as a terminal Emacs does.
const BORDER_COLUMNS: u16 = 1;
/// Below this many text rows a window has no room for a header line.
const HEADER_LINE_MIN_BODY_ROWS: u16 = 2;

/// `point` and `start` are markers in `buffer` created without undo
/// tracking, since Emacs window point and start are not undo-recorded.
/// `start` is always meant to be a line start; `start_row_skip` screen
/// rows of that line are scrolled off the top, so a long wrapped line can
/// begin the window part way through, as an Emacs window start can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub buffer: BufferId,
    pub point: MarkerId,
    pub start: MarkerId,
    pub start_row_skip: u32,
    /// The text width `start_row_skip` counts rows at; a window whose width
    /// changed has its start moved to the row holding the same text.
    pub start_cols: u16,
    /// Columns scrolled off the left; always 0 while lines only wrap.
    pub hscroll: u32,
    /// Emacs's window sequence number, as `#<window 4 on foo>` shows it.
    pub number: u64,
    /// Emacs `window-use-time`: when the window was last selected; 0 for
    /// a window never selected.
    pub use_time: u64,
}

/// An entry of Emacs `window-prev-buffers`: a buffer the window showed
/// before, with where its start and point were. Both are markers in that
/// buffer, so edits and reloads move them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrevBuffer {
    pub buffer: BufferId,
    pub start: MarkerId,
    pub point: MarkerId,
}

/// A window's place on the frame: its total size, the modeline and any
/// vertical border included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowBox {
    pub left: u16,
    pub top: u16,
    pub cols: u16,
    pub rows: u16,
}

/// The grid cells a window draws into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowArea {
    pub left: u16,
    /// The first text row.
    pub top: u16,
    /// Text columns, the vertical border left out.
    pub cols: u16,
    pub text_rows: u16,
    pub header_row: Option<u16>,
    pub modeline_row: Option<u16>,
    /// The column of the border to the window's right, drawn on every row
    /// from `box_top` for `box_rows` rows.
    pub border: Option<u16>,
    pub box_top: u16,
    pub box_rows: u16,
}

impl WindowBox {
    /// The rows a window gives its text, header line and modeline. A
    /// one-row window keeps its modeline, as the single window of a
    /// two-row terminal does.
    pub(crate) fn area(&self, frame_cols: u16, wants_header: bool) -> WindowArea {
        let border = (self.cols >= BORDER_COLUMNS
            && u32::from(self.left) + u32::from(self.cols) < u32::from(frame_cols))
        .then(|| self.left + self.cols - BORDER_COLUMNS);
        let cols = if border.is_some() {
            self.cols - BORDER_COLUMNS
        } else {
            self.cols
        };
        let modeline_row = self
            .rows
            .checked_sub(MODELINE_ROWS)
            .map(|body| self.top + body);
        let body = self.rows.saturating_sub(MODELINE_ROWS);
        let header = wants_header && body >= HEADER_LINE_MIN_BODY_ROWS;
        WindowArea {
            left: self.left,
            top: if header { self.top + 1 } else { self.top },
            cols,
            text_rows: if header { body - 1 } else { body },
            header_row: header.then_some(self.top),
            modeline_row,
            border,
            box_top: self.top,
            box_rows: self.rows,
        }
    }
}

/// A node of a frame's window tree: a live window, or a combination of
/// windows side by side or stacked. Sizes are totals; `normal` is the
/// node's share of its combination (Emacs `window-normal-size`), which a
/// frame resize keeps.
#[derive(Debug, Clone)]
struct Node {
    cols: u16,
    rows: u16,
    normal: f64,
    kind: NodeKind,
}

#[derive(Debug, Clone)]
enum NodeKind {
    Live(WindowId),
    Combination {
        side_by_side: bool,
        children: Vec<Node>,
    },
}

impl Node {
    fn live(window: WindowId, cols: u16, rows: u16) -> Node {
        Node {
            cols,
            rows,
            normal: 1.0,
            kind: NodeKind::Live(window),
        }
    }

    fn size(&self, side_by_side: bool) -> u16 {
        if side_by_side { self.cols } else { self.rows }
    }

    fn set_size(&mut self, side_by_side: bool, size: u16) {
        if side_by_side {
            self.resize(size, self.rows);
        } else {
            self.resize(self.cols, size);
        }
    }

    /// Gives the node a new total size; a combination shares it among its
    /// children by their normal sizes.
    fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        if let NodeKind::Combination {
            side_by_side,
            children,
        } = &mut self.kind
        {
            let total = if *side_by_side { cols } else { rows };
            let normals: Vec<f64> = children.iter().map(|child| child.normal).collect();
            for (child, size) in children.iter_mut().zip(distribute(total, &normals)) {
                if *side_by_side {
                    child.resize(size, rows);
                } else {
                    child.resize(cols, size);
                }
            }
        }
    }

    /// Recomputes the children's normal sizes from their sizes, after a
    /// split or deletion changed them.
    fn renormalize(&mut self) {
        let total = self.size_of_combination();
        if let NodeKind::Combination {
            side_by_side,
            children,
        } = &mut self.kind
        {
            let count = children_len_f64(children.len());
            for child in children.iter_mut() {
                child.normal = if total == 0 {
                    1.0 / count
                } else {
                    f64::from(child.size(*side_by_side)) / f64::from(total)
                };
            }
        }
    }

    fn size_of_combination(&self) -> u16 {
        match &self.kind {
            NodeKind::Combination { side_by_side, .. } => self.size(*side_by_side),
            NodeKind::Live(_) => 0,
        }
    }

    fn collect_boxes(&self, left: u16, top: u16, out: &mut Vec<(WindowId, WindowBox)>) {
        match &self.kind {
            NodeKind::Live(window) => out.push((
                *window,
                WindowBox {
                    left,
                    top,
                    cols: self.cols,
                    rows: self.rows,
                },
            )),
            NodeKind::Combination {
                side_by_side,
                children,
            } => {
                let (mut left, mut top) = (left, top);
                for child in children {
                    child.collect_boxes(left, top, out);
                    if *side_by_side {
                        left = left.saturating_add(child.cols);
                    } else {
                        top = top.saturating_add(child.rows);
                    }
                }
            }
        }
    }

    /// The child indexes leading to `window`.
    fn path_to(&self, window: WindowId) -> Option<Vec<usize>> {
        match &self.kind {
            NodeKind::Live(live) => (*live == window).then(Vec::new),
            NodeKind::Combination { children, .. } => {
                children.iter().enumerate().find_map(|(index, child)| {
                    child.path_to(window).map(|mut path| {
                        path.insert(0, index);
                        path
                    })
                })
            }
        }
    }

    fn at(&self, path: &[usize]) -> &Node {
        match (path.split_first(), &self.kind) {
            (None, _) => self,
            (Some((index, rest)), NodeKind::Combination { children, .. }) => {
                children[*index].at(rest)
            }
            (Some(_), NodeKind::Live(_)) => unreachable!("paths lead through combinations"),
        }
    }

    fn at_mut(&mut self, path: &[usize]) -> &mut Node {
        match path.split_first() {
            None => self,
            Some((index, rest)) => match &mut self.kind {
                NodeKind::Combination { children, .. } => children[*index].at_mut(rest),
                NodeKind::Live(_) => unreachable!("paths lead through combinations"),
            },
        }
    }
}

fn children_len_f64(count: usize) -> f64 {
    f64::from(u32::try_from(count).expect("a frame has few windows"))
}

/// Shares `total` among parts in proportion to `normals`: each part gets
/// its whole share, and the rows left over go one each to the parts with
/// the largest fractions, earlier parts first on ties.
fn distribute(total: u16, normals: &[f64]) -> Vec<u16> {
    let sum: f64 = normals.iter().sum();
    let shares: Vec<f64> = normals
        .iter()
        .map(|normal| {
            if sum > 0.0 {
                f64::from(total) * normal / sum
            } else {
                f64::from(total) / children_len_f64(normals.len())
            }
        })
        .collect();
    // Each share is at most `total`, so the floor fits in a u16.
    let mut sizes: Vec<u16> = shares.iter().map(|share| share.floor() as u16).collect();
    let given: u32 = sizes.iter().map(|size| u32::from(*size)).sum();
    let mut left_over = u32::from(total).saturating_sub(given);
    let mut order: Vec<usize> = (0..sizes.len()).collect();
    order.sort_by(|a, b| {
        let fraction = |index: usize| shares[index] - shares[index].floor();
        fraction(*b)
            .partial_cmp(&fraction(*a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(b))
    });
    for index in order {
        if left_over == 0 {
            break;
        }
        sizes[index] += 1;
        left_over -= 1;
    }
    sizes
}

/// One client's windows: a tree of live windows filling the frame above
/// the echo area. Windows and point are per client; buffers are shared
/// between clients.
#[derive(Debug, Clone)]
pub struct Frame {
    pub selected: WindowId,
    pub windows: SlotMap<WindowId, Window>,
    /// Emacs `window-prev-buffers` of each window, most recent first.
    pub prev_buffers: SecondaryMap<WindowId, Vec<PrevBuffer>>,
    root: Node,
}

impl Frame {
    /// A frame whose one window fills `cols` by `rows` terminal cells.
    pub fn new(window: Window, cols: u16, rows: u16) -> Frame {
        let mut windows = SlotMap::with_key();
        let selected = windows.insert(window);
        let mut prev_buffers = SecondaryMap::new();
        prev_buffers.insert(selected, Vec::new());
        Frame {
            selected,
            windows,
            prev_buffers,
            root: Node::live(selected, cols, window_area_rows(rows)),
        }
    }

    pub fn selected_window(&self) -> &Window {
        &self.windows[self.selected]
    }

    /// The frame's width: the root window's.
    pub(crate) fn cols(&self) -> u16 {
        self.root.cols
    }

    /// The terminal changed size: the windows keep their shares of it.
    pub(crate) fn resize(&mut self, cols: u16, rows: u16) {
        self.root.resize(cols, window_area_rows(rows));
    }

    /// Every live window with its place, in Emacs's cyclic order (the
    /// window tree left to right, top to bottom).
    pub(crate) fn boxes(&self) -> Vec<(WindowId, WindowBox)> {
        let mut boxes = Vec::new();
        self.root.collect_boxes(0, 0, &mut boxes);
        boxes
    }

    pub(crate) fn window_box(&self, window: WindowId) -> WindowBox {
        self.boxes()
            .into_iter()
            .find(|(id, _)| *id == window)
            .map(|(_, place)| place)
            .expect("live windows are in the tree")
    }

    /// Live windows in cyclic order.
    pub(crate) fn order(&self) -> Vec<WindowId> {
        self.boxes().into_iter().map(|(id, _)| id).collect()
    }

    /// Live windows in cyclic order starting with the selected one, as
    /// Emacs's `window-list-1` lists them.
    pub(crate) fn order_from_selected(&self) -> Vec<WindowId> {
        let mut order = self.order();
        let selected = order
            .iter()
            .position(|id| *id == self.selected)
            .expect("the selected window is live");
        order.rotate_left(selected);
        order
    }

    /// Whether this is the frame's only window.
    pub(crate) fn sole(&self) -> bool {
        matches!(self.root.kind, NodeKind::Live(_))
    }

    /// Emacs `window-full-width-p`.
    pub(crate) fn full_width(&self, window: WindowId) -> bool {
        self.window_box(window).cols == self.root.cols
    }

    /// The window's total columns (side by side) or rows.
    pub(crate) fn size_along(&self, window: WindowId, side_by_side: bool) -> u16 {
        let place = self.window_box(window);
        if side_by_side { place.cols } else { place.rows }
    }

    fn path(&self, window: WindowId) -> Vec<usize> {
        self.root
            .path_to(window)
            .expect("live windows are in the tree")
    }

    /// Emacs `split-window`: `new` takes `new_size` of `window`'s rows (or
    /// columns), below (or right of) it. Returns the new window's id.
    pub(crate) fn split(
        &mut self,
        window: WindowId,
        side_by_side: bool,
        new_size: u16,
        new: Window,
    ) -> WindowId {
        let in_combination = !self.split_needs_parent(window, side_by_side);
        let path = self.path(window);
        let old = self.root.at(&path).clone();
        let old_size = old.size(side_by_side);
        assert!(new_size < old_size, "the caller checked the sizes");
        let mut kept = old.clone();
        kept.set_size(side_by_side, old_size - new_size);
        let new_id = self.windows.insert(new);
        self.prev_buffers.insert(new_id, Vec::new());
        let mut added = Node::live(new_id, old.cols, old.rows);
        added.set_size(side_by_side, new_size);
        if in_combination {
            let (index, parent_path) = path.split_last().expect("checked above");
            let parent = self.root.at_mut(parent_path);
            if let NodeKind::Combination { children, .. } = &mut parent.kind {
                children[*index] = kept;
                children.insert(index + 1, added);
            }
            parent.renormalize();
        } else {
            let target = self.root.at_mut(&path);
            let normal = target.normal;
            *target = Node {
                cols: old.cols,
                rows: old.rows,
                normal,
                kind: NodeKind::Combination {
                    side_by_side,
                    children: vec![kept, added],
                },
            };
            target.renormalize();
        }
        new_id
    }

    /// Whether splitting `window` makes a new combination, which takes a
    /// window number in Emacs: it is the frame's only window, or its
    /// combination runs the other way.
    pub(crate) fn split_needs_parent(&self, window: WindowId, side_by_side: bool) -> bool {
        let path = self.path(window);
        !path.split_last().is_some_and(|(_, parent)| {
            matches!(
                self.root.at(parent).kind,
                NodeKind::Combination { side_by_side: along, .. } if along == side_by_side
            )
        })
    }

    /// Emacs `delete-window`: the window's rows (or columns) go to the
    /// sibling before it, or else after it; a combination left with one
    /// child is replaced by it. Returns the removed window and its
    /// history, whose markers the caller deletes.
    pub(crate) fn delete(&mut self, window: WindowId) -> (Window, Vec<PrevBuffer>) {
        assert!(!self.sole(), "the sole window is never deleted");
        let path = self.path(window);
        let (index, parent_path) = path
            .split_last()
            .expect("a window that is not sole has a parent");
        let parent = self.root.at_mut(parent_path);
        let NodeKind::Combination {
            side_by_side,
            children,
        } = &mut parent.kind
        else {
            unreachable!("a parent is a combination")
        };
        let side_by_side = *side_by_side;
        let removed = children.remove(*index);
        let sibling = index.checked_sub(1).unwrap_or(0);
        let grown = children[sibling].size(side_by_side) + removed.size(side_by_side);
        children[sibling].set_size(side_by_side, grown);
        if children.len() == 1 {
            let mut only = children.remove(0);
            only.normal = parent.normal;
            *parent = only;
            self.merge_into_parent(parent_path);
        } else {
            parent.renormalize();
        }
        self.remove_window(window)
    }

    /// A combination that replaced its parent's child and runs the same
    /// way as that parent joins it (Emacs `recombine_windows`).
    fn merge_into_parent(&mut self, path: &[usize]) {
        let Some((index, grandparent_path)) = path.split_last() else {
            return;
        };
        let grandparent = self.root.at_mut(grandparent_path);
        let NodeKind::Combination {
            side_by_side: outer,
            children,
        } = &mut grandparent.kind
        else {
            unreachable!("a parent is a combination")
        };
        let same_way = matches!(
            children[*index].kind,
            NodeKind::Combination { side_by_side: inner, .. } if inner == *outer
        );
        if !same_way {
            return;
        }
        let NodeKind::Combination {
            children: inner_children,
            ..
        } = children.remove(*index).kind
        else {
            unreachable!("checked above")
        };
        for (offset, child) in inner_children.into_iter().enumerate() {
            children.insert(index + offset, child);
        }
        grandparent.renormalize();
    }

    /// Emacs `delete-other-windows`: `window` fills the frame. Returns the
    /// removed windows and their histories.
    pub(crate) fn keep_only(&mut self, window: WindowId) -> Vec<(Window, Vec<PrevBuffer>)> {
        let others: Vec<WindowId> = self
            .order()
            .into_iter()
            .filter(|id| *id != window)
            .collect();
        self.root = Node::live(window, self.root.cols, self.root.rows);
        self.selected = window;
        others
            .into_iter()
            .map(|id| self.remove_window(id))
            .collect()
    }

    fn remove_window(&mut self, window: WindowId) -> (Window, Vec<PrevBuffer>) {
        let removed = self
            .windows
            .remove(window)
            .expect("live windows are in the frame");
        let history = self.prev_buffers.remove(window).unwrap_or_default();
        (removed, history)
    }

    /// Emacs `window--even-window-sizes` for `even-window-sizes`
    /// `height-only`: when `window` and the selected window are the only
    /// two windows of a stack and the selected one is taller, half the
    /// difference moves to `window`.
    pub(crate) fn even_heights(&mut self, window: WindowId) {
        let path = self.path(window);
        let Some((_, parent_path)) = path.split_last() else {
            return;
        };
        let selected = self.selected;
        let parent = self.root.at_mut(parent_path);
        let NodeKind::Combination {
            side_by_side: false,
            children,
        } = &mut parent.kind
        else {
            return;
        };
        let is =
            |node: &Node, id: WindowId| matches!(node.kind, NodeKind::Live(live) if live == id);
        if children.len() != 2 || !children.iter().any(|child| is(child, selected)) {
            return;
        }
        let (taller, shorter) = if is(&children[0], selected) {
            (0, 1)
        } else {
            (1, 0)
        };
        if children[taller].rows <= children[shorter].rows {
            return;
        }
        let moved = (children[taller].rows - children[shorter].rows) / 2;
        let (taller_rows, shorter_rows) = (
            children[taller].rows - moved,
            children[shorter].rows + moved,
        );
        children[taller].set_size(false, taller_rows);
        children[shorter].set_size(false, shorter_rows);
        parent.renormalize();
    }
}

/// The terminal rows windows share: all but the echo area's.
fn window_area_rows(rows: u16) -> u16 {
    rows.saturating_sub(ECHO_ROWS)
}

/// Which terminal rows hold what. A short terminal gives up text rows
/// first, then the modeline; the echo area is kept longest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameLayout {
    pub text_rows: u16,
    pub modeline_row: Option<u16>,
    pub echo_row: Option<u16>,
}

pub(crate) fn frame_layout(rows: u16) -> FrameLayout {
    let echo_row = rows.checked_sub(ECHO_ROWS);
    let modeline_row = echo_row.and_then(|echo| echo.checked_sub(MODELINE_ROWS));
    FrameLayout {
        text_rows: modeline_row.unwrap_or(0),
        modeline_row,
        echo_row,
    }
}

/// Whether reading may block on file I/O. Rendering never may.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fetch {
    Io,
    CachedOnly,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Reader<'a> {
    text: &'a Text,
    fetch: Fetch,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(text: &'a Text, fetch: Fetch) -> Reader<'a> {
        Reader { text, fetch }
    }

    fn len(&self) -> u64 {
        self.text.len()
    }

    fn read(&self, range: Range<u64>, out: &mut Vec<u8>) -> Result<(), ReadError> {
        match self.fetch {
            Fetch::Io => self.text.read(range, out),
            Fetch::CachedOnly => self.text.read_cached(range, out),
        }
    }
}

/// The block a cached-only read needed. Reads that may not do I/O fail in
/// no other way.
pub(crate) fn not_cached_block(error: ReadError) -> u64 {
    match error {
        ReadError::NotCached { block } => block,
        other => unreachable!("a cached-only read failed with {other}"),
    }
}

/// What laying out one logical line produces, in order. Rows count from
/// the line's first screen row; columns are window columns.
#[derive(Debug)]
pub(crate) enum LineEvent<'a> {
    /// The display unit at `pos` begins here (its first cell, after any
    /// wrap): where the cursor goes when point is at `pos`.
    Unit { pos: u64, row: u64, col: usize },
    /// One glyph cell of the unit at `unit`. `escape` marks the cells of
    /// `^X` and `\ooo` notation.
    Cell {
        unit: u64,
        row: u64,
        col: usize,
        text: &'a str,
        width: usize,
        escape: bool,
    },
    /// A continuation glyph: the row goes on below.
    Continuation { row: u64, col: usize },
    /// The line ends at `pos`, its newline or the end of the buffer.
    End {
        pos: u64,
        row: u64,
        col: usize,
        newline: bool,
    },
}

impl LineEvent<'_> {
    fn row(&self) -> u64 {
        match self {
            LineEvent::Unit { row, .. }
            | LineEvent::Cell { row, .. }
            | LineEvent::Continuation { row, .. }
            | LineEvent::End { row, .. } => *row,
        }
    }

    /// The buffer position the event shows, if any.
    fn pos(&self) -> Option<u64> {
        match self {
            LineEvent::Unit { pos, .. } | LineEvent::End { pos, .. } => Some(*pos),
            LineEvent::Cell { unit, .. } => Some(*unit),
            LineEvent::Continuation { .. } => None,
        }
    }
}

type Visit<'v> = dyn FnMut(&LineEvent) -> ControlFlow<()> + 'v;

/// The glyph cells of one display unit: graphemes as themselves, a tab as
/// spaces to the next tab stop after `column`, and control and invalid
/// bytes as `^X` or `\ooo` notation, one cell per character of it.
pub(crate) fn unit_glyphs(
    bytes: &[u8],
    unit: DisplayUnit,
    column: u64,
    tab_width: usize,
    emit: &mut dyn FnMut(&str, usize, bool) -> ControlFlow<()>,
) -> ControlFlow<()> {
    match unit {
        DisplayUnit::Grapheme { len, width } => {
            let cluster = std::str::from_utf8(&bytes[..len])
                .expect("display_units yields graphemes of valid UTF-8");
            emit(&grapheme_text(cluster), usize::from(width), false)
        }
        DisplayUnit::Tab => {
            let tab_width_u64 = tab_width as u64;
            let spaces = tab_width_u64 - column % tab_width_u64;
            for _ in 0..spaces {
                emit(" ", 1, false)?;
            }
            ControlFlow::Continue(())
        }
        DisplayUnit::Control(byte) => {
            let shown = if byte == DELETE {
                DELETE_GLYPH
            } else {
                char::from(byte ^ CONTROL_TO_CARET)
            };
            emit_chars(&[CONTROL_CARET, shown], emit)
        }
        DisplayUnit::InvalidByte(byte) | DisplayUnit::C1(byte) => {
            let notation: Vec<char> = format!("\\{byte:03o}").chars().collect();
            emit_chars(&notation, emit)
        }
        DisplayUnit::Glyphless { .. } => emit(GLYPHLESS_GLYPH, 1, false),
        DisplayUnit::Newline => unreachable!("lines are laid out up to their newline"),
    }
}

fn emit_chars(
    characters: &[char],
    emit: &mut dyn FnMut(&str, usize, bool) -> ControlFlow<()>,
) -> ControlFlow<()> {
    let mut buffer = [0; UTF8_MAX_LEN];
    for character in characters {
        emit(character.encode_utf8(&mut buffer), 1, true)?;
    }
    ControlFlow::Continue(())
}

/// Places glyphs on rows, wrapping like an Emacs terminal window: a glyph
/// that would reach the last column moves to the next row, and the columns
/// it leaves are filled with the continuation glyph.
struct LineWalker<'v, 'w> {
    cols: usize,
    text_width: usize,
    tab_width: usize,
    row: u64,
    x: usize,
    /// Columns of the rows above in this line, so tab stops count from
    /// the start of the line as Emacs's do.
    continued_columns: u64,
    visit: &'w mut Visit<'v>,
}

impl LineWalker<'_, '_> {
    fn place(&mut self, width: usize) -> ControlFlow<(), (u64, usize)> {
        // At column 0 a glyph is placed even when it cannot fit, so a
        // window narrower than one glyph still advances.
        if self.x > 0 && self.x + width > self.text_width {
            for col in self.x..self.cols {
                (self.visit)(&LineEvent::Continuation { row: self.row, col })?;
            }
            self.continued_columns += self.x as u64;
            self.row += 1;
            self.x = 0;
        }
        let at = (self.row, self.x);
        self.x += width;
        ControlFlow::Continue(at)
    }

    fn unit(&mut self, pos: u64, bytes: &[u8], unit: DisplayUnit) -> ControlFlow<()> {
        let column = self.continued_columns + self.x as u64;
        let tab_width = self.tab_width;
        let mut first = true;
        unit_glyphs(
            bytes,
            unit,
            column,
            tab_width,
            &mut |text, width, escape| {
                let (row, col) = self.place(width)?;
                if first {
                    first = false;
                    (self.visit)(&LineEvent::Unit { pos, row, col })?;
                }
                (self.visit)(&LineEvent::Cell {
                    unit: pos,
                    row,
                    col,
                    text,
                    width,
                    escape,
                })
            },
        )
    }
}

/// Where the cursor goes for `point`: the unit at point, or the unit that
/// contains it when point is inside one.
#[derive(Debug)]
pub(crate) struct CursorTracker {
    point: u64,
    candidate: Option<(u16, usize)>,
    decided: Option<Option<(u16, usize)>>,
}

impl CursorTracker {
    pub(crate) fn new(point: u64) -> CursorTracker {
        CursorTracker {
            point,
            candidate: None,
            decided: None,
        }
    }

    /// Feed every `Unit` and `End` event in window order.
    pub(crate) fn observe(&mut self, pos: u64, row: u16, col: usize) {
        if self.decided.is_some() {
            return;
        }
        if pos == self.point {
            self.decided = Some(Some((row, col)));
        } else if pos > self.point {
            self.decided = Some(self.candidate);
        } else {
            self.candidate = Some((row, col));
        }
    }

    pub(crate) fn decided(&self) -> bool {
        self.decided.is_some()
    }

    /// `None` when point is outside the rows walked.
    pub(crate) fn location(&self) -> Option<(u16, usize)> {
        self.decided.flatten()
    }
}

/// A screen row: row `row` of the logical line starting at `line_start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ScreenLine {
    pub line_start: u64,
    pub row: u64,
}

/// How a walk over a window's rows ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowWalk {
    /// The visitor stopped the walk.
    pub stopped: bool,
    /// The end of the buffer is at or before the end of the window, which
    /// is Emacs's test for `All` and `Bot`.
    pub end_visible: bool,
    /// Emacs `window-end`: the first position not shown.
    pub end: u64,
}

/// Lays out one buffer's text for a window `cols` wide.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Layout<'a> {
    reader: Reader<'a>,
    cols: u16,
    tab_width: usize,
}

impl<'a> Layout<'a> {
    pub(crate) fn new(reader: Reader<'a>, cols: u16, tab_width: usize) -> Layout<'a> {
        Layout {
            reader,
            cols,
            tab_width,
        }
    }

    pub(crate) fn len(&self) -> u64 {
        self.reader.len()
    }

    /// Lays out the logical line starting at `line_start`, reading only as
    /// far as the visitor goes.
    pub(crate) fn walk_line(
        &self,
        line_start: u64,
        visit: &mut Visit<'_>,
    ) -> Result<(), ReadError> {
        let cols = usize::from(self.cols);
        let mut walker = LineWalker {
            cols,
            text_width: cols.saturating_sub(CONTINUATION_COLUMNS),
            tab_width: self.tab_width,
            row: 0,
            x: 0,
            continued_columns: 0,
            visit,
        };
        let length = self.reader.len();
        let mut pos = line_start;
        let mut chunk = LINE_READ_CHUNK;
        let mut bytes = Vec::new();
        loop {
            let end = length.min(pos.saturating_add(chunk));
            bytes.clear();
            self.reader.read(pos..end, &mut bytes)?;
            let newline = bytes.iter().position(|byte| *byte == b'\n');
            let segment = &bytes[..newline.unwrap_or(bytes.len())];
            let complete = newline.is_some() || end == length;
            let units: Vec<(usize, DisplayUnit)> = display_units(segment).collect();
            // Without the rest of the line, only units followed by a whole
            // UTF-8 sequence are settled: more bytes could extend the last
            // grapheme or complete a truncated sequence.
            let usable = if complete {
                units.len()
            } else {
                let settled = segment.len().saturating_sub(UTF8_MAX_LEN);
                units
                    .iter()
                    .rposition(|(offset, _)| *offset < settled)
                    .unwrap_or(0)
            };
            for (offset, unit) in &units[..usable] {
                let unit_pos = pos + *offset as u64;
                if walker.unit(unit_pos, &segment[*offset..], *unit).is_break() {
                    return Ok(());
                }
            }
            if complete {
                pos += segment.len() as u64;
                let ending = LineEvent::End {
                    pos,
                    row: walker.row,
                    col: walker.x,
                    newline: newline.is_some(),
                };
                // The line is done whether or not the visitor stops here.
                let _ = (walker.visit)(&ending);
                return Ok(());
            }
            let consumed = units[usable].0;
            pos += consumed as u64;
            chunk = if consumed == 0 {
                chunk.saturating_mul(2)
            } else {
                LINE_READ_CHUNK
            };
        }
    }

    /// The start of the line holding `pos`.
    pub(crate) fn line_start(&self, pos: u64) -> Result<u64, ReadError> {
        let mut end = pos;
        let mut bytes = Vec::new();
        while end > 0 {
            let start = end.saturating_sub(LINE_START_SCAN_CHUNK);
            bytes.clear();
            self.reader.read(start..end, &mut bytes)?;
            if let Some(newline) = bytes.iter().rposition(|byte| *byte == b'\n') {
                return Ok(start + newline as u64 + 1);
            }
            end = start;
        }
        Ok(0)
    }

    pub(crate) fn at_line_start(&self, pos: u64) -> Result<bool, ReadError> {
        if pos == 0 {
            return Ok(true);
        }
        let mut byte = Vec::new();
        self.reader.read(pos - 1..pos, &mut byte)?;
        Ok(byte == b"\n")
    }

    /// The screen row showing `pos`.
    pub(crate) fn screen_line_of(&self, pos: u64) -> Result<ScreenLine, ReadError> {
        let line_start = self.line_start(pos)?;
        let mut row = 0;
        self.walk_line(line_start, &mut |event| match event {
            LineEvent::Unit {
                pos: at, row: on, ..
            }
            | LineEvent::End {
                pos: at, row: on, ..
            } if *at <= pos => {
                row = *on;
                if *at == pos {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            }
            LineEvent::Unit { .. } | LineEvent::End { .. } => ControlFlow::Break(()),
            _ => ControlFlow::Continue(()),
        })?;
        Ok(ScreenLine { line_start, row })
    }

    /// Emacs `vertical-motion` to the start of a screen row: the first
    /// position shown on it.
    pub(crate) fn row_start(&self, line: ScreenLine) -> Result<u64, ReadError> {
        let mut found = None;
        self.walk_line(line.line_start, &mut |event| {
            if event.row() < line.row {
                return ControlFlow::Continue(());
            }
            match event.pos() {
                Some(pos) => {
                    found = Some(pos);
                    ControlFlow::Break(())
                }
                None => ControlFlow::Continue(()),
            }
        })?;
        Ok(found.expect("every line walk ends with an End event"))
    }

    /// The number of screen rows the line at `line_start` takes.
    fn line_rows(&self, line_start: u64) -> Result<u64, ReadError> {
        let mut rows = 0;
        self.walk_line(line_start, &mut |event| {
            if let LineEvent::End { row, .. } = event {
                rows = row + 1;
            }
            ControlFlow::Continue(())
        })?;
        Ok(rows)
    }

    /// Moves `count` screen rows down (up when negative), stopping at the
    /// buffer's first or last row. Returns the row reached and how many
    /// rows were moved.
    pub(crate) fn move_lines(
        &self,
        from: ScreenLine,
        count: i64,
    ) -> Result<(ScreenLine, u64), ReadError> {
        let wanted = count.unsigned_abs();
        if count >= 0 {
            self.move_down(from, wanted)
        } else {
            self.move_up(from, wanted)
        }
    }

    fn move_down(&self, from: ScreenLine, wanted: u64) -> Result<(ScreenLine, u64), ReadError> {
        let mut current = from;
        let mut remaining = wanted;
        while remaining > 0 {
            let target = current.row.saturating_add(remaining);
            let mut reached = false;
            let mut end = None;
            self.walk_line(current.line_start, &mut |event| {
                if event.row() >= target {
                    reached = true;
                    return ControlFlow::Break(());
                }
                if let LineEvent::End {
                    pos, row, newline, ..
                } = event
                {
                    end = Some((*pos, *row, *newline));
                }
                ControlFlow::Continue(())
            })?;
            if reached {
                current.row = target;
                return Ok((current, wanted));
            }
            let (end_pos, last_row, newline) = end.expect("an unstopped line walk ends");
            // A start row left past the end of a line an edit shortened
            // counts as its last row.
            remaining -= last_row.saturating_sub(current.row);
            current.row = last_row;
            if !newline {
                return Ok((current, wanted - remaining));
            }
            remaining -= 1;
            current = ScreenLine {
                line_start: end_pos + 1,
                row: 0,
            };
        }
        Ok((current, wanted))
    }

    fn move_up(&self, from: ScreenLine, wanted: u64) -> Result<(ScreenLine, u64), ReadError> {
        let mut current = from;
        let mut remaining = wanted;
        while remaining > 0 {
            if current.row >= remaining {
                current.row -= remaining;
                return Ok((current, wanted));
            }
            remaining -= current.row;
            current.row = 0;
            if current.line_start == 0 {
                return Ok((current, wanted - remaining));
            }
            let previous = self.line_start(current.line_start - 1)?;
            let rows = self.line_rows(previous)?;
            remaining -= 1;
            current = ScreenLine {
                line_start: previous,
                row: rows - 1,
            };
        }
        Ok((current, wanted))
    }

    /// Lays out `height` window rows from `start`, giving each event with
    /// its window row.
    pub(crate) fn walk_window(
        &self,
        start: ScreenLine,
        height: u16,
        visit: &mut dyn FnMut(u16, &LineEvent) -> ControlFlow<()>,
    ) -> Result<WindowWalk, ReadError> {
        let height = u64::from(height);
        let length = self.len();
        let mut line_start = start.line_start;
        let mut skip = start.row;
        let mut offset: u64 = 0;
        if height == 0 {
            return Ok(WindowWalk {
                stopped: false,
                end_visible: false,
                end: line_start,
            });
        }
        loop {
            let mut end = None;
            let mut full_at = None;
            let mut stopped = false;
            self.walk_line(line_start, &mut |event| {
                if let LineEvent::End {
                    pos, row, newline, ..
                } = event
                {
                    end = Some((*pos, *row, *newline));
                }
                let row = event.row();
                if row < skip {
                    return ControlFlow::Continue(());
                }
                let window_row = offset + (row - skip);
                if window_row >= height {
                    full_at = Some(event.pos());
                    return ControlFlow::Break(());
                }
                let window_row = u16::try_from(window_row).expect("below the u16 window height");
                let flow = visit(window_row, event);
                stopped = flow.is_break();
                flow
            })?;
            if stopped {
                return Ok(WindowWalk {
                    stopped: true,
                    end_visible: false,
                    end: line_start,
                });
            }
            if let Some(position) = full_at {
                return Ok(WindowWalk {
                    stopped: false,
                    end_visible: false,
                    end: position.unwrap_or(line_start),
                });
            }
            let (end_pos, last_row, newline) = end.expect("an unstopped line walk ends");
            if !newline {
                return Ok(WindowWalk {
                    stopped: false,
                    end_visible: true,
                    end: length,
                });
            }
            offset += (last_row + 1).saturating_sub(skip);
            skip = 0;
            line_start = end_pos + 1;
            if offset >= height {
                return Ok(WindowWalk {
                    stopped: false,
                    end_visible: line_start >= length,
                    end: line_start,
                });
            }
        }
    }

    /// Where the cursor for `point` is in a window of `height` rows from
    /// `start`, or `None` when point is not shown.
    pub(crate) fn point_location(
        &self,
        start: ScreenLine,
        height: u16,
        point: u64,
    ) -> Result<Option<(u16, usize)>, ReadError> {
        let mut tracker = CursorTracker::new(point);
        self.walk_window(start, height, &mut |row, event| {
            if let LineEvent::Unit { pos, col, .. } | LineEvent::End { pos, col, .. } = event {
                tracker.observe(*pos, row, *col);
            }
            if tracker.decided() {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })?;
        Ok(tracker.location())
    }

    /// Where redisplay puts the window start when point is not visible:
    /// point's row in the middle (Emacs `scroll-conservatively` 0).
    pub(crate) fn centered_start(&self, point: u64, height: u16) -> Result<ScreenLine, ReadError> {
        self.start_with_point_at(point, u64::from(height / 2))
    }

    /// The window start that shows point's row `rows_above` rows down.
    pub(crate) fn start_with_point_at(
        &self,
        point: u64,
        rows_above: u64,
    ) -> Result<ScreenLine, ReadError> {
        let point_line = self.screen_line_of(point)?;
        let count = i64::try_from(rows_above).unwrap_or(i64::MAX);
        Ok(self.move_lines(point_line, -count)?.0)
    }

    /// The new window start if point is not visible in a window of
    /// `height` rows from `start`, or if `start` is no longer at a line
    /// start (an edit removed the newline before it); Emacs recenters in
    /// both cases.
    pub(crate) fn keep_point_visible(
        &self,
        start: ScreenLine,
        height: u16,
        point: u64,
    ) -> Result<Option<ScreenLine>, ReadError> {
        if height == 0 {
            return Ok(None);
        }
        if self.at_line_start(start.line_start)?
            && self.point_location(start, height, point)?.is_some()
        {
            return Ok(None);
        }
        Ok(Some(self.centered_start(point, height)?))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use emx_text::{Sources, unit_width};

    use super::*;

    fn text(bytes: &[u8]) -> Text {
        Text::from_original(Sources::memory(Arc::from(bytes)))
    }

    /// Each row's cells as text, skipping continuation glyphs.
    fn rows(text: &Text, cols: u16) -> Vec<String> {
        let layout = Layout::new(Reader::new(text, Fetch::Io), cols, 8);
        let mut rows: Vec<String> = Vec::new();
        layout
            .walk_line(0, &mut |event| {
                if let LineEvent::Cell {
                    row, text, width, ..
                } = event
                {
                    let row = usize::try_from(*row).expect("small");
                    if rows.len() <= row {
                        rows.resize(row + 1, String::new());
                    }
                    rows[row].push_str(text);
                    assert!(*width <= 2);
                }
                ControlFlow::Continue(())
            })
            .expect("memory read");
        rows
    }

    #[test]
    fn frame_layout_gives_up_text_then_modeline() {
        let none = FrameLayout {
            text_rows: 0,
            modeline_row: None,
            echo_row: None,
        };
        assert_eq!(frame_layout(0), none);
        assert_eq!(
            frame_layout(1),
            FrameLayout {
                echo_row: Some(0),
                ..none
            }
        );
        assert_eq!(
            frame_layout(2),
            FrameLayout {
                text_rows: 0,
                modeline_row: Some(0),
                echo_row: Some(1),
            }
        );
        assert_eq!(
            frame_layout(24),
            FrameLayout {
                text_rows: 22,
                modeline_row: Some(22),
                echo_row: Some(23),
            }
        );
    }

    #[test]
    fn long_lines_lay_out_the_same_across_read_chunks() {
        // Multi-byte graphemes and combining marks straddle every chunk
        // boundary of a line several chunks long.
        let mut line = Vec::new();
        let pieces: [&[u8]; 5] = [b"e\xcc\x81", "\u{4e2d}".as_bytes(), b"x", b"\t", b"\xff"];
        let mut index = 0;
        while line.len() < 3 * LINE_READ_CHUNK as usize + 7 {
            line.extend_from_slice(pieces[index % pieces.len()]);
            index += 1;
        }
        let cols = 37;
        let mut expected: Vec<String> = vec![String::new()];
        let mut x = 0;
        let mut column = 0;
        for (offset, unit) in display_units(&line) {
            let width = unit_width(&unit, column, 8);
            let mut glyphs = Vec::new();
            let _ = unit_glyphs(
                &line[offset..],
                unit,
                column as u64,
                8,
                &mut |text, width, _| {
                    glyphs.push((text.to_string(), width));
                    ControlFlow::Continue(())
                },
            );
            for (glyph, glyph_width) in glyphs {
                if x > 0 && x + glyph_width > usize::from(cols) - 1 {
                    expected.push(String::new());
                    x = 0;
                }
                expected.last_mut().expect("a row").push_str(&glyph);
                x += glyph_width;
            }
            column += width;
        }
        assert_eq!(rows(&text(&line), cols), expected);
    }

    #[test]
    fn move_lines_counts_wrapped_rows_and_stops_at_the_edges() {
        let content = text(b"0123456789abcdef\nxy\n");
        let layout = Layout::new(Reader::new(&content, Fetch::Io), 10, 8);
        let top = ScreenLine {
            line_start: 0,
            row: 0,
        };
        let line = |line_start, row| ScreenLine { line_start, row };
        assert_eq!(layout.move_lines(top, 1).expect("read"), (line(0, 1), 1));
        assert_eq!(layout.move_lines(top, 2).expect("read"), (line(17, 0), 2));
        assert_eq!(layout.move_lines(top, 3).expect("read"), (line(20, 0), 3));
        assert_eq!(layout.move_lines(top, 9).expect("read"), (line(20, 0), 3));
        assert_eq!(
            layout.move_lines(line(20, 0), -2).expect("read"),
            (line(0, 1), 2)
        );
        assert_eq!(layout.move_lines(line(0, 1), -5).expect("read"), (top, 1));
        assert_eq!(
            layout.move_lines(line(0, 7), 1).expect("read"),
            (line(17, 0), 1),
            "a stale row past the line's end"
        );
        assert_eq!(layout.row_start(line(0, 1)).expect("read"), 9);
        assert_eq!(layout.screen_line_of(12).expect("read"), line(0, 1));
        assert_eq!(layout.screen_line_of(16).expect("read"), line(0, 1));
        assert_eq!(layout.screen_line_of(20).expect("read"), line(20, 0));
    }
}
