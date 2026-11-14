use std::collections::VecDeque;
use std::ops::Range;
use std::path::PathBuf;

use emx_text::{FileIdentity, ReadError, Text};
use slotmap::SlotMap;

use crate::marker::{InsertionType, Marker, MarkerId};
use crate::undo::{ChangeContext, UndoLimits, UndoList};

slotmap::new_key_type! {
    pub struct BufferId;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileBinding {
    pub path: PathBuf,
    /// The on-disk version the buffer was read from or last saved to;
    /// `None` when the file does not exist yet.
    pub identity: Option<FileIdentity>,
    pub saving: bool,
}

/// The buffer coding's modeline letter (Emacs `mode-line-mule-info`): a
/// visited file whose bytes are all ASCII stays undecided (`-`) until a
/// save writes something else; everything else is UTF-8 (`U`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodingMnemonic {
    Undecided,
    Utf8,
}

impl CodingMnemonic {
    pub fn letter(self) -> char {
        match self {
            CodingMnemonic::Undecided => '-',
            CodingMnemonic::Utf8 => 'U',
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("Buffer is read-only")]
    ReadOnly,
    #[error("Buffer is being saved")]
    Saving,
    #[error("{0}")]
    Read(#[from] ReadError),
    #[error("Args out of range")]
    OutOfRange,
}

/// Every text change, in the order applied; the edit journal implements it.
#[derive(Debug, Clone)]
pub enum Change {
    Insert { at: u64, bytes: Vec<u8> },
    Delete { at: u64, len: u64 },
    ReplaceAll { text: Text },
}

pub trait ChangeRecorder {
    fn record(&mut self, buffer: BufferId, change: &Change);
}

#[derive(Debug)]
pub struct Buffer {
    pub id: BufferId,
    pub name: String,
    pub text: Text,
    pub file: Option<FileBinding>,
    pub read_only: bool,
    pub mark: Option<MarkerId>,
    pub mark_active: bool,
    pub mark_ring: VecDeque<MarkerId>,
    pub undo: UndoList,
    /// The file is being read; the buffer is read-only until it arrives.
    pub loading: bool,
    pub coding: CodingMnemonic,
    /// An in-place save failed part way through the file this buffer reads
    /// its unchanged text from: the text is no longer what was visited.
    pub damaged: bool,
    /// Where point was when the buffer last left a window, so showing it
    /// again puts point back, as Emacs's buffer point does.
    pub(crate) last_point: Option<MarkerId>,
    /// Emacs `recent-auto-save-p`: auto-saved since last read or saved, so
    /// the auto-save file is this session's and a save may delete it.
    pub(crate) auto_saved_since_save: bool,
    markers: SlotMap<MarkerId, Marker>,
    modified_tick: u64,
    save_tick: u64,
    auto_save_tick: u64,
}

impl Buffer {
    pub fn new(id: BufferId, name: String, text: Text, limits: UndoLimits) -> Buffer {
        Buffer {
            id,
            name,
            text,
            file: None,
            read_only: false,
            mark: None,
            mark_active: false,
            mark_ring: VecDeque::new(),
            undo: UndoList::new(limits),
            loading: false,
            coding: CodingMnemonic::Utf8,
            damaged: false,
            last_point: None,
            auto_saved_since_save: false,
            markers: SlotMap::with_key(),
            modified_tick: 0,
            save_tick: 0,
            auto_save_tick: 0,
        }
    }

    pub fn is_modified(&self) -> bool {
        self.modified_tick > self.save_tick
    }

    pub fn set_unmodified(&mut self) {
        self.save_tick = self.modified_tick;
    }

    /// Counts changes; a save records it to tell whether the buffer
    /// changed while the save ran.
    pub(crate) fn modified_tick(&self) -> u64 {
        self.modified_tick
    }

    /// The text as of `tick` is what was saved.
    pub(crate) fn set_saved_at(&mut self, tick: u64) {
        self.save_tick = tick;
    }

    /// Emacs `(set-buffer-modified-p t)` without a change, as `write-file`
    /// does so the new file is written even when nothing was edited.
    pub(crate) fn mark_modified(&mut self) {
        if !self.is_modified() {
            self.modified_tick += 1;
        }
    }

    /// Changed since the last auto-save (or since visiting).
    pub(crate) fn changed_since_auto_save(&self) -> bool {
        self.is_modified() && self.modified_tick > self.auto_save_tick
    }

    /// Records that the current text is being auto-saved; returns the
    /// previous mark so a failed auto-save can restore it.
    pub(crate) fn mark_auto_saved(&mut self) -> u64 {
        std::mem::replace(&mut self.auto_save_tick, self.modified_tick)
    }

    pub(crate) fn restore_auto_save_mark(&mut self, tick: u64) {
        self.auto_save_tick = tick;
    }

    pub(crate) fn visited_identity(&self) -> Option<FileIdentity> {
        self.file.as_ref().and_then(|file| file.identity)
    }

    pub(crate) fn check_editable(&self) -> Result<(), EditError> {
        if self.read_only || self.loading || self.damaged {
            return Err(EditError::ReadOnly);
        }
        if self.file.as_ref().is_some_and(|file| file.saving) {
            return Err(EditError::Saving);
        }
        Ok(())
    }

    fn check_range(&self, range: &Range<u64>) -> Result<(), EditError> {
        if range.start > range.end || range.end > self.text.len() {
            return Err(EditError::OutOfRange);
        }
        Ok(())
    }

    pub fn insert(
        &mut self,
        pos: u64,
        bytes: &[u8],
        recorder: &mut dyn ChangeRecorder,
    ) -> Result<(), EditError> {
        self.check_editable()?;
        self.check_range(&(pos..pos))?;
        self.apply_insert(pos, bytes, recorder)
    }

    /// Deletes as if point were at the start of `range` (`C-d`, `C-k`,
    /// `kill-region` with point first). Use `delete_with_point` when point
    /// may be at the end, as for `DEL`, so undo puts point back there.
    pub fn delete(
        &mut self,
        range: Range<u64>,
        recorder: &mut dyn ChangeRecorder,
    ) -> Result<Vec<u8>, EditError> {
        let point = range.start;
        self.delete_with_point(range, point, recorder)
    }

    /// `point` is point when the deletion happens (Emacs PT): if it is at the
    /// end of `range`, undoing the deletion leaves point after the text.
    pub fn delete_with_point(
        &mut self,
        range: Range<u64>,
        point: u64,
        recorder: &mut dyn ChangeRecorder,
    ) -> Result<Vec<u8>, EditError> {
        self.check_editable()?;
        self.check_range(&range)?;
        self.apply_delete(range, point, recorder)
    }

    /// Replaces the whole text as one undoable change (a reload). Markers are
    /// clamped to the new length.
    pub fn replace_all(
        &mut self,
        text: Text,
        recorder: &mut dyn ChangeRecorder,
    ) -> Result<(), EditError> {
        self.check_editable()?;
        self.apply_replace_all(text, recorder);
        Ok(())
    }

    fn change_context(&self) -> ChangeContext {
        ChangeContext {
            unmodified: !self.is_modified(),
            visited: self.visited_identity(),
        }
    }

    pub(crate) fn apply_insert(
        &mut self,
        at: u64,
        bytes: &[u8],
        recorder: &mut dyn ChangeRecorder,
    ) -> Result<(), EditError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let text = self.text.insert(at, bytes)?;
        let length = bytes.len() as u64;
        let context = self.change_context();
        self.undo.record_insert(at, length, &context);
        self.text = text;
        for marker in self.markers.values_mut() {
            marker.adjust_for_insert(at, length);
        }
        self.modified_tick += 1;
        recorder.record(
            self.id,
            &Change::Insert {
                at,
                bytes: bytes.to_vec(),
            },
        );
        Ok(())
    }

    pub(crate) fn apply_delete(
        &mut self,
        range: Range<u64>,
        point: u64,
        recorder: &mut dyn ChangeRecorder,
    ) -> Result<Vec<u8>, EditError> {
        if range.is_empty() {
            return Ok(Vec::new());
        }
        let mut deleted = Vec::new();
        self.text.read(range.clone(), &mut deleted)?;
        let text = self.text.delete(range.clone())?;
        let adjustments = self
            .markers
            .iter()
            .filter_map(|(id, marker)| {
                marker
                    .deletion_adjustment(&range)
                    .map(|adjustment| (id, adjustment))
            })
            .collect();
        let context = self.change_context();
        self.undo.record_delete(
            range.start,
            deleted.clone(),
            point == range.end,
            adjustments,
            &context,
        );
        self.text = text;
        for marker in self.markers.values_mut() {
            marker.adjust_for_delete(&range);
        }
        self.modified_tick += 1;
        recorder.record(
            self.id,
            &Change::Delete {
                at: range.start,
                len: range.end - range.start,
            },
        );
        Ok(deleted)
    }

    pub(crate) fn apply_replace_all(&mut self, text: Text, recorder: &mut dyn ChangeRecorder) {
        let context = self.change_context();
        let before = std::mem::replace(&mut self.text, text.clone());
        self.undo.record_replace_all(before, text.clone(), &context);
        let length = self.text.len();
        for marker in self.markers.values_mut() {
            marker.pos = marker.pos.min(length);
        }
        self.modified_tick += 1;
        recorder.record(self.id, &Change::ReplaceAll { text });
    }

    /// Positions past the end are clamped, as Emacs `set-marker` does.
    pub fn create_marker(&mut self, pos: u64, insertion_type: InsertionType) -> MarkerId {
        self.insert_marker(pos, insertion_type, true)
    }

    /// A marker whose movement by deletions is never recorded in undo, for
    /// window point and start (Emacs PT is not a marker).
    pub fn create_marker_without_undo(
        &mut self,
        pos: u64,
        insertion_type: InsertionType,
    ) -> MarkerId {
        self.insert_marker(pos, insertion_type, false)
    }

    fn insert_marker(
        &mut self,
        pos: u64,
        insertion_type: InsertionType,
        record_in_undo: bool,
    ) -> MarkerId {
        let pos = pos.min(self.text.len());
        self.markers.insert(Marker {
            pos,
            insertion_type,
            record_in_undo,
        })
    }

    /// Panics when `marker` was deleted or belongs to another buffer.
    pub fn marker_pos(&self, marker: MarkerId) -> u64 {
        self.marker_position(marker)
            .expect("marker belongs to this buffer and is live")
    }

    pub(crate) fn marker_position(&self, marker: MarkerId) -> Option<u64> {
        self.markers.get(marker).map(|marker| marker.pos)
    }

    /// Positions past the end are clamped. Panics when `marker` was deleted.
    pub fn set_marker(&mut self, marker: MarkerId, pos: u64) {
        let length = self.text.len();
        self.markers
            .get_mut(marker)
            .expect("marker belongs to this buffer and is live")
            .pos = pos.min(length);
    }

    pub fn delete_marker(&mut self, marker: MarkerId) {
        self.markers.remove(marker);
    }
}
