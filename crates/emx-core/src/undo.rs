//! Emacs's undo model: `buffer-undo-list` from undo.c and the `undo`,
//! `undo-more`, `primitive-undo`, `undo-redo` and
//! `undo-make-selective-list` commands from simple.el.
//!
//! The list is a chain of shared, mutable cons-like nodes because Emacs's
//! algorithm depends on list identity: `pending-undo-list` is a tail of
//! `buffer-undo-list`, `undo-equiv-table` is keyed by `eq` on list tails,
//! and truncation and the undo command splice cells in place.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Weak};

use emx_text::{FileIdentity, Original, Text};
use parking_lot::Mutex;

use crate::buffer::{Buffer, ChangeRecorder, EditError};
use crate::config::Config;
use crate::marker::MarkerId;

/// `sizeof (struct Lisp_Cons)` on 64-bit Emacs, as truncate_undo_list counts.
const CONS_BYTES: u64 = 16;
/// truncate_undo_list charges `sizeof (struct Lisp_String) - 1` per string.
const STRING_HEADER_BYTES: u64 = 31;
/// A ReplaceAll entry holds two persistent texts that share structure with
/// the buffer, so its own memory is small and does not grow with file size.
const REPLACE_ALL_BYTES: u64 = 64;
/// The equivalence table is pruned of dead weak entries when it doubles past
/// this size, which keeps pruning amortized.
const EQUIV_PRUNE_MINIMUM: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UndoLimits {
    pub limit: u64,
    pub strong_limit: u64,
    pub outer_limit: u64,
    /// Emacs `amalgamating-undo-limit`.
    pub amalgamation_limit: u64,
}

impl From<&Config> for UndoLimits {
    fn from(config: &Config) -> Self {
        Self {
            limit: config.undo_limit,
            strong_limit: config.undo_strong_limit,
            outer_limit: config.undo_outer_limit,
            amalgamation_limit: config.amalgamation_limit,
        }
    }
}

/// One `buffer-undo-list` element. Positions are byte offsets.
#[derive(Debug, Clone)]
pub enum UndoEntry {
    /// `(BEG . END)`: text was inserted.
    Insertion { beg: u64, end: u64 },
    /// `(TEXT . POS)`: text was deleted at `at`. Emacs encodes "point was at
    /// the end of the deleted text" as a negative POS, which 0-based offsets
    /// cannot express at 0, hence the explicit flag.
    Deletion {
        text: Vec<u8>,
        at: u64,
        point_at_end: bool,
    },
    /// A bare integer: point before the command, restored by undo.
    PointBefore(u64),
    /// `(t . TIME)`: the buffer was unmodified before this change, relative
    /// to the visited file version recorded here.
    FirstChange {
        saved_identity: Option<FileIdentity>,
    },
    /// `(MARKER . ADJUSTMENT)`, recorded right before its Deletion.
    MarkerAdjust { marker: MarkerId, adjustment: i64 },
    /// The whole text was replaced (a reload).
    ReplaceAll { before: Text, after: Text },
    /// The change group is a reload from disk: undoing it leaves text
    /// older than the file.
    Reload,
    /// The change group undid a reload: undoing it brings the reloaded
    /// text back.
    ReloadUndone,
    /// `nil`.
    Boundary,
}

impl UndoEntry {
    /// Size as truncate_undo_list counts it: the list cell, plus the element
    /// cons, plus string storage.
    fn accounted_bytes(&self) -> u64 {
        match self {
            UndoEntry::Boundary | UndoEntry::PointBefore(_) => CONS_BYTES,
            UndoEntry::Deletion { text, .. } => {
                2 * CONS_BYTES + STRING_HEADER_BYTES + text.len() as u64
            }
            UndoEntry::ReplaceAll { before, after } => {
                2 * CONS_BYTES + REPLACE_ALL_BYTES + retained_bytes(before, after)
            }
            UndoEntry::Insertion { .. }
            | UndoEntry::Reload
            | UndoEntry::ReloadUndone
            | UndoEntry::FirstChange { .. }
            | UndoEntry::MarkerAdjust { .. } => 2 * CONS_BYTES,
        }
    }
}

/// Memory a ReplaceAll entry keeps alive on its own, which Emacs would
/// count as the size of the deleted string: the in-memory original of the
/// text it restores, unless that text shares it with the text it replaces.
/// Texts read from a file through the block cache cost no memory.
fn retained_bytes(before: &Text, after: &Text) -> u64 {
    if Arc::ptr_eq(before.sources(), after.sources()) {
        return 0;
    }
    match &before.sources().original {
        Original::Memory(bytes) => bytes.len() as u64,
        Original::File(_) => 0,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UndoError {
    #[error("No further undo information")]
    NoFurtherUndo,
    #[error("No further undo information for region")]
    NoFurtherUndoForRegion,
    #[error("No undone changes to redo")]
    NoUndoneChangesToRedo,
    #[error("Changes to be undone are outside visible portion of buffer")]
    OutsideBuffer,
    #[error("Buffer text differs from the reloaded text; cannot undo the reload")]
    ReloadMismatch,
    #[error(
        "Cannot undo the reload: the file was rewritten on disk and the text before it is gone"
    )]
    OriginalGone,
    #[error("{0}")]
    Edit(#[from] EditError),
}

/// Compares two equal-length texts chunk by chunk without materializing
/// either, since reloaded files may be far larger than memory.
fn texts_equal(left: &Text, right: &Text) -> Result<bool, emx_text::ReadError> {
    if left.same_root(right) {
        return Ok(true);
    }
    let length = left.len();
    let mut right_chunks = right.chunks(0..length);
    let mut pending: Vec<u8> = Vec::new();
    let mut pending_start = 0;
    for left_chunk in left.chunks(0..length) {
        let left_chunk = left_chunk?;
        let mut left_bytes = left_chunk.bytes();
        while !left_bytes.is_empty() {
            if pending_start == pending.len() {
                let Some(right_chunk) = right_chunks.next() else {
                    return Ok(false);
                };
                pending.clear();
                pending.extend_from_slice(right_chunk?.bytes());
                pending_start = 0;
            }
            let count = left_bytes.len().min(pending.len() - pending_start);
            if left_bytes[..count] != pending[pending_start..pending_start + count] {
                return Ok(false);
            }
            left_bytes = &left_bytes[count..];
            pending_start += count;
        }
    }
    Ok(true)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoOutcome {
    /// Where point goes; `None` leaves it where it is.
    pub point: Option<u64>,
    pub message: &'static str,
}

/// The gist of Emacs's `undo-outer-limit-truncate` warning.
pub fn outer_limit_warning(buffer_name: &str) -> String {
    format!("Buffer `{buffer_name}' undo info was discarded because it exceeded `undo-outer-limit'")
}

struct Node {
    id: u64,
    entry: UndoEntry,
    next: Mutex<List>,
}

type List = Option<Arc<Node>>;

impl Drop for Node {
    // Dropping a long chain recursively would overflow the stack.
    fn drop(&mut self) {
        let mut next = self.next.get_mut().take();
        while let Some(node) = next {
            match Arc::try_unwrap(node) {
                Ok(mut node) => next = node.next.get_mut().take(),
                Err(_) => break,
            }
        }
    }
}

fn next_of(node: &Arc<Node>) -> List {
    node.next.lock().clone()
}

fn is_boundary(list: &List) -> bool {
    matches!(list, Some(node) if matches!(node.entry, UndoEntry::Boundary))
}

fn same_list(left: &List, right: &List) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        (None, None) => true,
        _ => false,
    }
}

fn strip_boundaries(mut list: List) -> List {
    while is_boundary(&list) {
        list = list.as_ref().and_then(next_of);
    }
    list
}

/// A Lisp value that `pending-undo-list` or an `undo-equiv-table` entry can
/// hold: a list (possibly nil), `t`, `undo-in-region` or `empty`.
#[derive(Clone)]
enum Pending {
    List(List),
    True,
    UndoInRegion,
    Empty,
}

/// An equivalence-table value. Lists are weak like Emacs's weak table.
enum Equiv {
    List(Weak<Node>),
    True,
    UndoInRegion,
    Empty,
}

impl Equiv {
    fn from_pending(value: &Pending) -> Option<Equiv> {
        match value {
            Pending::List(None) => None,
            Pending::List(Some(node)) => Some(Equiv::List(Arc::downgrade(node))),
            Pending::True => Some(Equiv::True),
            Pending::UndoInRegion => Some(Equiv::UndoInRegion),
            Pending::Empty => Some(Equiv::Empty),
        }
    }

    fn to_pending(&self) -> Option<Pending> {
        match self {
            Equiv::List(node) => node.upgrade().map(|node| Pending::List(Some(node))),
            Equiv::True => Some(Pending::True),
            Equiv::UndoInRegion => Some(Pending::UndoInRegion),
            Equiv::Empty => Some(Pending::Empty),
        }
    }

    fn is_live(&self) -> bool {
        match self {
            Equiv::List(node) => node.strong_count() > 0,
            _ => true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoundaryCause {
    /// Any boundary not added after an amalgamating command, and Emacs's
    /// reset of the cause to 0 when amalgamation stops.
    Other,
    /// `(N . BUFFERS)`: the Nth consecutive amalgamating boundary.
    Amalgamating(u64),
}

/// Whether the buffer was unmodified before a change, and the visited file
/// version that `(t . TIME)` records.
pub(crate) struct ChangeContext {
    pub(crate) unmodified: bool,
    pub(crate) visited: Option<FileIdentity>,
}

pub struct UndoList {
    head: List,
    next_id: u64,
    limits: UndoLimits,
    pending: Pending,
    /// The `undo-in-region` variable.
    in_region: bool,
    equiv: HashMap<u64, (Weak<Node>, Equiv)>,
    equiv_of_nil: Option<Equiv>,
    equiv_prune_at: usize,
    /// Emacs `point_before_last_command_or_undo`.
    last_boundary_point: u64,
    last_boundary_cause: BoundaryCause,
    this_command_amalgamating: bool,
    outer_limit_warning: bool,
}

impl std::fmt::Debug for UndoList {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UndoList")
            .field("entries", &self.entries())
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl UndoList {
    pub fn new(limits: UndoLimits) -> UndoList {
        UndoList {
            head: None,
            next_id: 0,
            limits,
            pending: Pending::List(None),
            in_region: false,
            equiv: HashMap::new(),
            equiv_of_nil: None,
            equiv_prune_at: EQUIV_PRUNE_MINIMUM,
            // A new buffer is shown with point at its start.
            last_boundary_point: 0,
            last_boundary_cause: BoundaryCause::Other,
            this_command_amalgamating: false,
            outer_limit_warning: false,
        }
    }

    /// The list from newest to oldest, for inspection.
    pub fn entries(&self) -> Vec<UndoEntry> {
        let mut entries = Vec::new();
        let mut list = self.head.clone();
        while let Some(node) = list {
            entries.push(node.entry.clone());
            list = next_of(&node);
        }
        entries
    }

    /// Called by the command loop after every command (Emacs
    /// `undo-auto--add-boundary` plus the `point_before_last_command_or_undo`
    /// update), with point as the command left it. Truncation runs here,
    /// standing in for Emacs's GC-time `truncate_undo_list`.
    pub fn boundary(&mut self, point_at_boundary: u64) {
        let amalgamating = std::mem::take(&mut self.this_command_amalgamating);
        let needs_boundary = self.head.is_some() && !is_boundary(&self.head);
        if needs_boundary {
            let last_amalgamating = match self.last_boundary_cause {
                BoundaryCause::Amalgamating(count) => Some(count),
                BoundaryCause::Other => None,
            };
            self.push(UndoEntry::Boundary);
            self.last_boundary_cause = if amalgamating {
                BoundaryCause::Amalgamating(last_amalgamating.map_or(0, |count| count + 1))
            } else {
                BoundaryCause::Other
            };
        }
        self.last_boundary_point = point_at_boundary;
        if needs_boundary {
            self.truncate();
        }
    }

    /// True once after the outer limit discarded the undo list.
    pub fn take_outer_limit_warning(&mut self) -> bool {
        std::mem::take(&mut self.outer_limit_warning)
    }

    /// Emacs `undo-auto-amalgamate`, run at the start of an amalgamating
    /// command before its edit.
    fn amalgamate(&mut self, repeated_command: bool) {
        self.this_command_amalgamating = true;
        if let BoundaryCause::Amalgamating(count) = self.last_boundary_cause {
            if count < self.limits.amalgamation_limit && repeated_command {
                if is_boundary(&self.head) {
                    self.head = self.head.as_ref().and_then(next_of);
                }
            } else {
                self.last_boundary_cause = BoundaryCause::Other;
            }
        }
    }

    fn node(&mut self, entry: UndoEntry, next: List) -> Arc<Node> {
        let id = self.next_id;
        self.next_id += 1;
        Arc::new(Node {
            id,
            entry,
            next: Mutex::new(next),
        })
    }

    fn push(&mut self, entry: UndoEntry) {
        let next = self.head.take();
        self.head = Some(self.node(entry, next));
    }

    /// Emacs `record_point`, including `record_first_change`.
    fn record_point(&mut self, beg: u64, context: &ChangeContext) {
        let at_boundary = self.head.is_none() || is_boundary(&self.head);
        if context.unmodified {
            self.push(UndoEntry::FirstChange {
                saved_identity: context.visited,
            });
        }
        if at_boundary && self.last_boundary_point != beg {
            self.push(UndoEntry::PointBefore(self.last_boundary_point));
        }
    }

    /// Emacs `record_insert`: extends the previous insertion when contiguous.
    pub(crate) fn record_insert(&mut self, beg: u64, len: u64, context: &ChangeContext) {
        self.record_point(beg, context);
        if let Some(head) = &self.head
            && let UndoEntry::Insertion {
                beg: previous_beg,
                end,
            } = head.entry
            && end == beg
        {
            let next = next_of(head);
            let extended = UndoEntry::Insertion {
                beg: previous_beg,
                end: beg + len,
            };
            self.head = Some(self.node(extended, next));
            return;
        }
        self.push(UndoEntry::Insertion {
            beg,
            end: beg + len,
        });
    }

    /// Emacs `record_delete` with `record_marker_adjustments`.
    pub(crate) fn record_delete(
        &mut self,
        beg: u64,
        text: Vec<u8>,
        point_at_end: bool,
        marker_adjustments: Vec<(MarkerId, i64)>,
        context: &ChangeContext,
    ) {
        self.record_point(beg, context);
        for (marker, adjustment) in marker_adjustments {
            self.push(UndoEntry::MarkerAdjust { marker, adjustment });
        }
        self.push(UndoEntry::Deletion {
            text,
            at: beg,
            point_at_end,
        });
    }

    /// Marks the change group just recorded as a reload from disk.
    pub(crate) fn record_reload(&mut self) {
        self.push(UndoEntry::Reload);
    }

    /// Takes back the mark `record_reload` just pushed, for a reload that
    /// then failed before changing anything. Panics when the newest entry
    /// is not that mark.
    pub(crate) fn discard_reload_mark(&mut self) {
        let head = self.head.take().expect("a reload mark was just recorded");
        assert!(
            matches!(head.entry, UndoEntry::Reload),
            "the newest undo entry is the reload mark"
        );
        self.head = next_of(&head);
    }

    pub(crate) fn record_replace_all(
        &mut self,
        before: Text,
        after: Text,
        context: &ChangeContext,
    ) {
        self.record_point(0, context);
        self.push(UndoEntry::ReplaceAll { before, after });
    }

    /// Emacs `truncate_undo_list` with `undo-outer-limit-truncate` (no
    /// question asked: the list is discarded and a warning raised).
    fn truncate(&mut self) {
        let mut size: u64 = 0;
        let mut previous: List = None;
        let mut next = self.head.clone();

        if is_boundary(&next) {
            size += CONS_BYTES;
            previous = next.take();
            next = previous.as_ref().and_then(next_of);
        }
        // The most recent change group is always kept unless it alone
        // exceeds the outer limit.
        while let Some(node) = next.clone() {
            if matches!(node.entry, UndoEntry::Boundary) {
                break;
            }
            size += node.entry.accounted_bytes();
            next = next_of(&node);
            previous = Some(node);
        }
        if size > self.limits.outer_limit {
            self.head = None;
            self.outer_limit_warning = true;
            return;
        }

        let mut last_boundary: List = None;
        if next.is_some() {
            last_boundary = previous.clone();
        }
        while let Some(node) = next.clone() {
            if matches!(node.entry, UndoEntry::Boundary) {
                if size > self.limits.strong_limit {
                    break;
                }
                last_boundary = previous.clone();
                if size > self.limits.limit {
                    break;
                }
            }
            size += node.entry.accounted_bytes();
            next = next_of(&node);
            previous = Some(node);
        }

        if next.is_none() {
            return;
        }
        match last_boundary {
            Some(cell) => *cell.next.lock() = None,
            None => self.head = None,
        }
    }

    fn lookup(&self, list: &List) -> Option<Pending> {
        match list {
            None => self.equiv_of_nil.as_ref().and_then(Equiv::to_pending),
            Some(node) => self
                .equiv
                .get(&node.id)
                .and_then(|(_, value)| value.to_pending()),
        }
    }

    /// Emacs `undo--last-change-was-undo-p`.
    fn last_change_was_undo(&self, list: &List) -> Option<Pending> {
        self.lookup(&strip_boundaries(list.clone()))
    }

    fn store(&mut self, key: &List, value: &Pending) {
        let value = Equiv::from_pending(value);
        let Some(node) = key else {
            self.equiv_of_nil = value;
            return;
        };
        match value {
            None => {
                self.equiv.remove(&node.id);
            }
            Some(value) => {
                self.equiv.insert(node.id, (Arc::downgrade(node), value));
            }
        }
        if self.equiv.len() >= self.equiv_prune_at {
            self.equiv
                .retain(|_, (key, value)| key.strong_count() > 0 && value.is_live());
            self.equiv_prune_at = (self.equiv.len() * 2).max(EQUIV_PRUNE_MINIMUM);
        }
    }

    fn unlink(&mut self, previous: &List, node: &Arc<Node>) {
        let next = next_of(node);
        match previous {
            Some(previous) => *previous.next.lock() = next,
            None => self.head = next,
        }
    }

    /// The step in Emacs `undo` that removes the first point record from the
    /// undo command's own change group (and later equal ones), so undoing
    /// this undo moves point to where the change is.
    fn strip_point_records_of_newest_group(&mut self) {
        let mut previous: List = None;
        let mut tail = self.head.clone();
        while let Some(node) = tail.clone() {
            match node.entry {
                UndoEntry::Boundary => break,
                UndoEntry::PointBefore(position) => {
                    self.unlink(&previous, &node);
                    tail = next_of(&node);
                    while let Some(candidate) = tail.clone() {
                        match candidate.entry {
                            UndoEntry::Boundary => break,
                            UndoEntry::PointBefore(other) if other == position => {
                                self.unlink(&previous, &candidate);
                            }
                            _ => previous = Some(candidate.clone()),
                        }
                        tail = next_of(&candidate);
                    }
                    break;
                }
                _ => {
                    tail = next_of(&node);
                    previous = Some(node);
                }
            }
        }
    }

    /// Emacs `undo-make-selective-list`: the elements wholly inside
    /// `start..end`, with positions adjusted through the out-of-region
    /// changes made after them. A ReplaceAll entry has no position mapping,
    /// so nothing older than it is considered.
    fn selective_list(&mut self, start: u64, end: u64) -> List {
        let start = start as i64;
        let mut end = end as i64;
        let mut selected = vec![UndoEntry::Boundary];
        let mut deltas: Vec<(i64, i64)> = Vec::new();
        let mut list = self.head.clone();
        while let Some(node) = list {
            match &node.entry {
                UndoEntry::Boundary => {
                    if !matches!(selected.last(), Some(UndoEntry::Boundary)) {
                        selected.push(UndoEntry::Boundary);
                    }
                }
                UndoEntry::FirstChange { .. } => {
                    if deltas.is_empty() {
                        selected.push(node.entry.clone());
                    }
                }
                UndoEntry::MarkerAdjust { .. } | UndoEntry::Reload | UndoEntry::ReloadUndone => {}
                UndoEntry::ReplaceAll { .. } => break,
                entry => {
                    let adjusted = adjust_entry(entry, &deltas);
                    if entry_in_region(&adjusted, start, end) {
                        end += entry_delta(&adjusted).1;
                        let is_deletion = matches!(adjusted, UndoEntry::Deletion { .. });
                        selected.push(adjusted);
                        if is_deletion {
                            let mut adjustments = next_of(&node);
                            while let Some(adjustment) = adjustments {
                                if !matches!(adjustment.entry, UndoEntry::MarkerAdjust { .. }) {
                                    break;
                                }
                                selected.push(adjustment.entry.clone());
                                adjustments = next_of(&adjustment);
                            }
                        }
                    } else {
                        let delta = entry_delta(entry);
                        if delta.1 != 0 {
                            deltas.push(delta);
                        }
                    }
                }
            }
            list = next_of(&node);
        }
        let mut result: List = None;
        for entry in selected.into_iter().rev() {
            result = Some(self.node(entry, result));
        }
        result
    }
}

/// Emacs `undo-adjust-pos`. `deltas` is in the order found (newest first in
/// the undo list); Emacs pushes them, so it applies the last found first.
fn adjust_position(position: i64, deltas: &[(i64, i64)], use_less_than: bool) -> i64 {
    let mut position = position;
    for &(delta_position, delta) in deltas.iter().rev() {
        let applies = if use_less_than {
            delta_position < position
        } else {
            delta_position <= position
        };
        if applies {
            position = delta_position.max(position - delta);
        }
    }
    position
}

fn adjust_entry(entry: &UndoEntry, deltas: &[(i64, i64)]) -> UndoEntry {
    match entry {
        UndoEntry::PointBefore(position) => {
            UndoEntry::PointBefore(adjust_position(*position as i64, deltas, false) as u64)
        }
        UndoEntry::Insertion { beg, end } => {
            let adjusted_beg = adjust_position(*beg as i64, deltas, false);
            let adjusted_end = adjusted_beg.max(adjust_position(*end as i64, deltas, true));
            UndoEntry::Insertion {
                beg: adjusted_beg as u64,
                end: adjusted_end as u64,
            }
        }
        UndoEntry::Deletion {
            text,
            at,
            point_at_end,
        } => UndoEntry::Deletion {
            text: text.clone(),
            at: adjust_position(*at as i64, deltas, false) as u64,
            point_at_end: *point_at_end,
        },
        other => other.clone(),
    }
}

/// Emacs `undo-elt-in-region`.
fn entry_in_region(entry: &UndoEntry, start: i64, end: i64) -> bool {
    match entry {
        UndoEntry::PointBefore(position) => {
            let position = *position as i64;
            position >= start && position <= end
        }
        UndoEntry::Deletion { at, .. } => {
            let at = *at as i64;
            at >= start && at <= end
        }
        UndoEntry::Insertion {
            beg,
            end: insertion_end,
        } => *beg as i64 >= start && *insertion_end as i64 <= end,
        _ => false,
    }
}

/// Emacs `undo-delta`: the first affected position and how much later
/// positions shift if the element is undone.
fn entry_delta(entry: &UndoEntry) -> (i64, i64) {
    match entry {
        UndoEntry::Deletion { text, at, .. } => (*at as i64, text.len() as i64),
        UndoEntry::Insertion { beg, end } => (*beg as i64, *beg as i64 - *end as i64),
        _ => (0, 0),
    }
}

impl Buffer {
    fn set_content_predates_disk(&mut self, predates: bool) {
        if let Some(file) = self.file.as_mut() {
            file.content_predates_disk = predates;
        }
    }

    /// Emacs `undo-auto-amalgamate`: call at the start of `self-insert-command`,
    /// `delete-char` and the commands built on them (`newline`,
    /// `delete-backward-char`), before their edit, when they act on a single
    /// character. `repeated_command` is Emacs's `(eq this-command
    /// last-command)`. Up to `amalgamation_limit` + 1 consecutive such commands
    /// then undo as one.
    pub fn amalgamate_last_change(&mut self, repeated_command: bool) {
        self.undo.amalgamate(repeated_command);
    }

    /// Emacs `undo` without an active region. `continuing` is Emacs's
    /// `(eq last-command 'undo)`; a failed undo or `undo_redo` still counts
    /// as an undo command.
    pub fn undo(
        &mut self,
        count: usize,
        continuing: bool,
        recorder: &mut dyn ChangeRecorder,
    ) -> Result<UndoOutcome, UndoError> {
        self.undo_command(None, count, continuing, recorder)
    }

    /// Emacs `undo` with an active region: only changes wholly inside
    /// `range` are undone. When continuing an undo chain, the region is
    /// ignored as in Emacs.
    pub fn undo_in_region(
        &mut self,
        range: Range<u64>,
        count: usize,
        continuing: bool,
        recorder: &mut dyn ChangeRecorder,
    ) -> Result<UndoOutcome, UndoError> {
        self.undo_command(Some(range), count, continuing, recorder)
    }

    /// Emacs `undo-redo`: undo the last undo, without needing a fresh chain.
    pub fn undo_redo(
        &mut self,
        count: usize,
        recorder: &mut dyn ChangeRecorder,
    ) -> Result<UndoOutcome, UndoError> {
        self.check_editable()?;
        if self.undo.last_change_was_undo(&self.undo.head).is_none() {
            return Err(UndoError::NoUndoneChangesToRedo);
        }
        let undo_list = strip_boundaries(self.undo.head.clone());
        let mut point = None;
        let remaining = self.primitive_undo(count, undo_list, &mut point, recorder)?;
        let new_pending = self.undo.last_change_was_undo(&remaining);
        let message = if self.undo.in_region {
            "Redo in region"
        } else {
            "Redo"
        };
        self.undo.pending = new_pending.unwrap_or(Pending::List(None));
        self.undo.head = remaining;
        Ok(UndoOutcome { point, message })
    }

    fn undo_command(
        &mut self,
        region: Option<Range<u64>>,
        count: usize,
        continuing: bool,
        recorder: &mut dyn ChangeRecorder,
    ) -> Result<UndoOutcome, UndoError> {
        self.check_editable()?;
        let mut point = None;
        let chain_intact = continuing
            && (matches!(self.undo.pending, Pending::True)
                || self.undo.last_change_was_undo(&self.undo.head).is_some());
        if !chain_intact {
            self.undo.in_region = region.is_some();
            let pending = match region {
                Some(range) if range.start != range.end => {
                    let (start, end) = (range.start.min(range.end), range.start.max(range.end));
                    self.undo.selective_list(start, end)
                }
                _ => self.undo.head.clone(),
            };
            self.undo.pending = Pending::List(pending);
            // Consumes the boundary left by the previous command.
            self.undo_more(1, &mut point, recorder)?;
        }

        let undoing_an_undo = match &self.undo.pending {
            Pending::List(list) => self.undo.lookup(list).is_some(),
            _ => false,
        };
        let message = match (undoing_an_undo, self.undo.in_region) {
            (false, false) => "Undo",
            (false, true) => "Undo in region",
            (true, false) => "Redo",
            (true, true) => "Redo in region",
        };
        self.undo_more(count, &mut point, recorder)?;

        // Map this undo's records to what remains to be undone, so the next
        // `undo` sees an unbroken chain and `undo_redo` can find its target.
        let key = strip_boundaries(self.undo.head.clone());
        let value = if self.undo.in_region {
            Pending::UndoInRegion
        } else {
            match &self.undo.pending {
                Pending::List(pending) if same_list(&key, pending) => {
                    self.undo.lookup(&key).unwrap_or(Pending::Empty)
                }
                pending => pending.clone(),
            }
        };
        self.undo.store(&key, &value);
        self.undo.strip_point_records_of_newest_group();
        Ok(UndoOutcome { point, message })
    }

    /// Emacs `undo-more`.
    fn undo_more(
        &mut self,
        count: usize,
        point: &mut Option<u64>,
        recorder: &mut dyn ChangeRecorder,
    ) -> Result<(), UndoError> {
        let Pending::List(list) = self.undo.pending.clone() else {
            return Err(if self.undo.in_region {
                UndoError::NoFurtherUndoForRegion
            } else {
                UndoError::NoFurtherUndo
            });
        };
        let remaining = self.primitive_undo(count, list, point, recorder)?;
        self.undo.pending = match remaining {
            None => Pending::True,
            some => Pending::List(some),
        };
        Ok(())
    }

    /// Emacs `primitive-undo`: undo `count` change groups from the front of
    /// `list` and return the rest. The changes are recorded on this buffer's
    /// undo list like any other edit.
    fn primitive_undo(
        &mut self,
        count: usize,
        mut list: List,
        point: &mut Option<u64>,
        recorder: &mut dyn ChangeRecorder,
    ) -> Result<List, UndoError> {
        // Applied only once every change undid: a failed undo leaves the
        // text, and so the warning, as they were.
        let mut predates_disk = None;
        for _ in 0..count {
            while let Some(node) = list.take() {
                list = next_of(&node);
                match &node.entry {
                    UndoEntry::Boundary => break,
                    UndoEntry::PointBefore(position) => {
                        *point = Some((*position).min(self.text.len()));
                    }
                    UndoEntry::FirstChange { saved_identity } => {
                        if *saved_identity == self.visited_identity() {
                            self.set_unmodified();
                        }
                    }
                    UndoEntry::Insertion { beg, end } => {
                        if beg > end || *end > self.text.len() {
                            return Err(UndoError::OutsideBuffer);
                        }
                        *point = Some(*beg);
                        self.apply_delete(*beg..*end, *beg, recorder)?;
                    }
                    UndoEntry::Deletion {
                        text,
                        at,
                        point_at_end,
                    } => {
                        if *at > self.text.len() {
                            return Err(UndoError::OutsideBuffer);
                        }
                        // Adjustments are valid only for markers that have
                        // not moved since the deletion.
                        let mut valid_adjustments = Vec::new();
                        while let Some(adjustment) = list.clone() {
                            let UndoEntry::MarkerAdjust {
                                marker,
                                adjustment: offset,
                            } = &adjustment.entry
                            else {
                                break;
                            };
                            let (marker, offset) = (*marker, *offset);
                            if self.marker_position(marker) == Some(*at) {
                                valid_adjustments.push((marker, offset));
                            }
                            list = next_of(&adjustment);
                        }
                        self.apply_insert(*at, text, recorder)?;
                        let length = text.len() as u64;
                        *point = Some(if *point_at_end { at + length } else { *at });
                        for (marker, offset) in valid_adjustments {
                            if let Some(position) = self.marker_position(marker) {
                                self.set_marker(marker, (position as i64 - offset) as u64);
                            }
                        }
                    }
                    UndoEntry::MarkerAdjust { marker, adjustment } => {
                        // Emacs warns about an adjustment with no deletion
                        // before it but still applies it.
                        if let Some(position) = self.marker_position(*marker) {
                            let adjusted = (position as i64 - adjustment).max(0) as u64;
                            self.set_marker(*marker, adjusted);
                        }
                    }
                    UndoEntry::Reload => {
                        self.undo.push(UndoEntry::ReloadUndone);
                        predates_disk = Some(true);
                    }
                    UndoEntry::ReloadUndone => {
                        self.undo.push(UndoEntry::Reload);
                        predates_disk = Some(false);
                    }
                    UndoEntry::ReplaceAll { before, after } => {
                        if !before.retired_pieces().is_empty() {
                            return Err(UndoError::OriginalGone);
                        }
                        // Edits undone since the reload leave equal content
                        // under a new root, so compare content, not identity.
                        if after.len() != self.text.len()
                            || !texts_equal(&self.text, after).map_err(EditError::from)?
                        {
                            return Err(UndoError::ReloadMismatch);
                        }
                        self.apply_replace_all(before.clone(), recorder);
                        if let Some(position) = point {
                            *position = (*position).min(self.text.len());
                        }
                    }
                }
            }
        }
        if let Some(predates) = predates_disk {
            self.set_content_predates_disk(predates);
        }
        Ok(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> UndoLimits {
        UndoLimits::from(&Config::default())
    }

    #[test]
    fn a_discarded_reload_mark_leaves_no_trace() {
        let mut list = UndoList::new(limits());
        let context = ChangeContext {
            unmodified: false,
            visited: None,
        };
        list.record_insert(0, 1, &context);
        list.boundary(1);
        let before = format!("{:?}", list.entries());
        list.boundary(1);
        list.record_reload();
        list.discard_reload_mark();
        assert_eq!(format!("{:?}", list.entries()), before);
        assert!(
            !list
                .entries()
                .iter()
                .any(|entry| matches!(entry, UndoEntry::Reload))
        );
    }
}
