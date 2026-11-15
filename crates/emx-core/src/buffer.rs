use std::collections::VecDeque;
use std::ops::Range;
use std::path::PathBuf;

use emx_text::{ColumnCheckpoints, FileIdentity, ReadError, Text};
use slotmap::SlotMap;

use crate::faces::FaceSpans;
use crate::marker::{InsertionType, Marker, MarkerId};
use crate::reload::ExternalState;
use crate::undo::{ChangeContext, UndoLimits, UndoList};

slotmap::new_key_type! {
    pub struct BufferId;
}

#[derive(Debug, Clone)]
pub struct FileBinding {
    pub path: PathBuf,
    /// The on-disk version the buffer was read from or last saved to;
    /// `None` when the file does not exist yet.
    pub identity: Option<FileIdentity>,
    pub saving: bool,
    /// What happened to the file on disk since (live reload).
    pub external: ExternalState,
    /// The text before the last reload, for `diff-external`.
    pub previous_text: Option<Text>,
    /// The file changed on disk since the buffer was last looked at.
    pub changed_since_viewed: bool,
    /// xxh3 of the `append_tail_hash_bytes` before the end of the
    /// `identity` version, for telling an append from a rewrite.
    pub tail_hash: u64,
    /// The version emx itself last wrote, which must never cause a reload.
    pub own_save_identity: Option<FileIdentity>,
    /// The file is on a filesystem the watcher cannot see changes on, so
    /// it is polled.
    pub polled: bool,
    /// A reload was undone: the text is older than the file, so a save
    /// would overwrite newer changes made on disk.
    pub content_predates_disk: bool,
}

impl FileBinding {
    pub fn new(path: PathBuf, identity: Option<FileIdentity>) -> FileBinding {
        FileBinding {
            path,
            identity,
            saving: false,
            external: ExternalState::Clean,
            previous_text: None,
            changed_since_viewed: false,
            tail_hash: 0,
            own_save_identity: None,
            polled: false,
            content_predates_disk: false,
        }
    }
}

impl PartialEq for FileBinding {
    fn eq(&self, other: &Self) -> bool {
        let same_previous = match (&self.previous_text, &other.previous_text) {
            (Some(left), Some(right)) => left.same_root(right),
            (None, None) => true,
            _ => false,
        };
        self.path == other.path
            && self.identity == other.identity
            && self.saving == other.saving
            && self.external == other.external
            && same_previous
            && self.changed_since_viewed == other.changed_since_viewed
            && self.tail_hash == other.tail_hash
            && self.own_save_identity == other.own_save_identity
            && self.polled == other.polled
            && self.content_predates_disk == other.content_predates_disk
    }
}

impl Eq for FileBinding {}

/// How much of a buffer's line index the background indexer has built, as
/// recorded from the most recent `Event::IndexProgress`. The editor keeps a
/// snapshot (rather than reading the shared, concurrently filled index) so
/// rendering stays a pure function of the events seen so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexingProgress {
    pub indexed_blocks: u64,
    pub total_blocks: u64,
}

/// Percentages run from 0 to 100.
const PERCENT_SCALE: u64 = 100;
/// The badge caps at 99%: the editor clears the progress once the file is
/// wholly indexed, so a lingering "100%" would never be the true state.
const PERCENT_CAP: u64 = 99;

impl IndexingProgress {
    /// The counted fraction as a whole-number percentage, capped at 99.
    pub fn percent(self) -> u64 {
        if self.total_blocks == 0 {
            return 0;
        }
        (self.indexed_blocks.saturating_mul(PERCENT_SCALE) / self.total_blocks).min(PERCENT_CAP)
    }
}

/// How far a save of the buffer has written, as recorded from the most recent
/// `Event::SaveProgress`. Kept as a per-buffer snapshot (like
/// `IndexingProgress`) so the modeline is drawn purely from the events seen,
/// never from state a worker thread is concurrently changing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveProgress {
    pub written_bytes: u64,
    pub total_bytes: u64,
}

impl SaveProgress {
    /// The written fraction as a whole-number percentage, capped at 99: the
    /// editor clears the snapshot on `Event::SaveFinished`, so a lingering
    /// "100%" would never be the true state.
    pub fn percent(self) -> u64 {
        if self.total_bytes == 0 {
            return 0;
        }
        (self.written_bytes.saturating_mul(PERCENT_SCALE) / self.total_bytes).min(PERCENT_CAP)
    }
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

/// What a Buffer Menu lists: the buffer of each line, and its header line.
#[derive(Debug, Clone)]
pub(crate) struct BufferMenu {
    pub entries: Vec<BufferId>,
    pub header: String,
}

/// What an `*Occur*` buffer lists: the buffer searched, for each of its
/// lines the start of the match it shows (in that buffer), and where the
/// matches are in its own text.
#[derive(Debug, Clone)]
pub(crate) struct OccurData {
    pub source: BufferId,
    pub targets: Vec<Option<MarkerId>>,
    pub match_spans: Vec<Range<u64>>,
    /// The source buffer was killed; its markers are gone.
    pub source_killed: bool,
}

/// Emacs `mode-name` of a Fundamental-mode buffer.
const FUNDAMENTAL_MODE: &str = "Fundamental";
/// Emacs `mode-name` of `Buffer-menu-mode`.
const BUFFER_MENU_MODE: &str = "Buffer Menu";
/// Emacs `mode-name` of `messages-buffer-mode`.
const MESSAGES_MODE: &str = "Messages";
/// The mode name of the file panel's buffer.
const TREE_MODE: &str = "Tree";

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
    /// Progress of the background line indexer, `None` once the file is
    /// wholly indexed or when no index is being built.
    pub indexing: Option<IndexingProgress>,
    /// Progress of a save in flight, `None` when no save is running.
    pub saving_progress: Option<SaveProgress>,
    /// Emacs `auto-revert-tail-mode`: follow appends to the visited file,
    /// keeping the end of the buffer in view like `tail -f`.
    pub tail: bool,
    pub coding: CodingMnemonic,
    /// An in-place save failed part way through the file this buffer reads
    /// its unchanged text from: the text is no longer what was visited.
    pub damaged: bool,
    /// Holds text rescued or recovered from the edit journal that is saved
    /// nowhere else, so it counts as unsaved work though it visits no file.
    pub recovered: bool,
    /// Where point was when the buffer last left a window, so showing it
    /// again puts point back, as Emacs's buffer point does.
    pub(crate) last_point: Option<MarkerId>,
    /// Emacs `last_window_start`: where the last window to stop showing
    /// the buffer started, which is where a window newly showing it
    /// starts. A position, not a marker, as in Emacs.
    pub(crate) last_window_start: u64,
    /// Set while the buffer is a Buffer Menu (`*Buffer List*`).
    pub(crate) menu: Option<BufferMenu>,
    /// The buffer is the message log, `*Messages*`.
    pub(crate) messages: bool,
    /// Set while the buffer is an `*Occur*` list.
    pub(crate) occur: Option<OccurData>,
    /// The mode name of a special buffer (`*Occur*`, `*Help*`).
    pub(crate) special_mode: Option<&'static str>,
    /// Set while the buffer is a client's file panel.
    pub(crate) tree: Option<Box<crate::editor::TreeData>>,
    /// The language the buffer's file is in, for highlighting,
    /// indentation and comments.
    pub(crate) language: crate::editor::LanguageState,
    pub(crate) highlight: crate::editor::HighlightState,
    /// Emacs `recent-auto-save-p`: auto-saved since last read or saved, so
    /// the auto-save file is this session's and a save may delete it.
    pub(crate) auto_saved_since_save: bool,
    /// Column checkpoints for the buffer's long lines, so a scrolled long
    /// line is navigated and rendered without rescanning it from the line
    /// start. Cleared whenever the text changes, since edits shift the byte
    /// offsets the checkpoints are keyed by.
    pub(crate) checkpoints: ColumnCheckpoints,
    /// Faces the text is drawn in (Emacs `face` text properties): the file
    /// panel's markers, syntax highlighting. They move with edits.
    pub(crate) faces: FaceSpans,
    markers: SlotMap<MarkerId, Marker>,
    modified_tick: u64,
    save_tick: u64,
    auto_save_tick: u64,
}

impl Buffer {
    pub fn new(
        id: BufferId,
        name: String,
        text: Text,
        limits: UndoLimits,
        checkpoints: ColumnCheckpoints,
    ) -> Buffer {
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
            indexing: None,
            saving_progress: None,
            tail: false,
            coding: CodingMnemonic::Utf8,
            damaged: false,
            recovered: false,
            last_point: None,
            last_window_start: 0,
            menu: None,
            messages: false,
            occur: None,
            special_mode: None,
            tree: None,
            language: crate::editor::LanguageState::default(),
            highlight: crate::editor::HighlightState::default(),
            auto_saved_since_save: false,
            checkpoints,
            faces: FaceSpans::new(),
            markers: SlotMap::with_key(),
            modified_tick: 0,
            save_tick: 0,
            auto_save_tick: 0,
        }
    }

    /// Emacs `mode-name`.
    pub(crate) fn mode_name(&self) -> &'static str {
        if self.menu.is_some() {
            BUFFER_MENU_MODE
        } else if self.tree.is_some() {
            TREE_MODE
        } else if let Some(mode) = self.special_mode {
            mode
        } else if self.messages {
            MESSAGES_MODE
        } else if let Some(language) = self.language.language {
            language.mode_name()
        } else {
            FUNDAMENTAL_MODE
        }
    }

    /// Replaces the text as Emacs's `erase-buffer` then an insertion with
    /// undo disabled would: every marker ends up at the start, undo is
    /// cleared, and the buffer is unmodified. The change goes to
    /// `recorder` like any other, so a journal stays in step.
    pub(crate) fn reset_text(
        &mut self,
        text: Text,
        limits: UndoLimits,
        recorder: &mut dyn ChangeRecorder,
    ) {
        self.text = text.clone();
        recorder.record(self.id, &Change::ReplaceAll { text });
        for marker in self.markers.values_mut() {
            marker.pos = 0;
        }
        self.undo = UndoList::new(limits);
        self.modified_tick += 1;
        self.checkpoints.clear();
        self.faces.clear();
        self.set_unmodified();
    }

    /// Inserts with no undo and no journal record, for a buffer whose
    /// changes are kept nowhere (Emacs's `*Messages*` has undo disabled).
    /// Markers move as for any insertion.
    pub(crate) fn insert_unrecorded(&mut self, at: u64, bytes: &[u8]) -> Result<(), EditError> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.text = self.text.insert(at, bytes)?;
        for marker in self.markers.values_mut() {
            marker.adjust_for_insert(at, bytes.len() as u64);
        }
        self.faces.adjust_for_insert(at, bytes.len() as u64);
        self.modified_tick += 1;
        self.checkpoints.clear();
        Ok(())
    }

    /// Deletes with no undo and no journal record; see `insert_unrecorded`.
    pub(crate) fn delete_unrecorded(&mut self, range: Range<u64>) -> Result<(), EditError> {
        if range.is_empty() {
            return Ok(());
        }
        self.text = self.text.delete(range.clone())?;
        for marker in self.markers.values_mut() {
            marker.adjust_for_delete(&range);
        }
        self.faces.adjust_for_delete(&range);
        self.modified_tick += 1;
        self.checkpoints.clear();
        Ok(())
    }

    /// The markers at `pos`.
    pub(crate) fn markers_at(&self, pos: u64) -> Vec<MarkerId> {
        self.markers
            .iter()
            .filter(|(_, marker)| marker.pos == pos)
            .map(|(id, _)| id)
            .collect()
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
            self.checkpoints.clear();
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
        self.faces.adjust_for_insert(at, length);
        self.modified_tick += 1;
        self.checkpoints.clear();
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
        self.apply_delete_recording(range, point, recorder, true)
    }

    /// Emacs `replace_range`, which `replace-match` uses: `range` becomes
    /// `bytes`. Markers at or after the end of `range` keep their distance
    /// from its end (so one at an empty range's position ends up after the
    /// new text), markers inside it go to its start, and undo records no
    /// marker adjustments.
    pub(crate) fn replace_range(
        &mut self,
        range: Range<u64>,
        bytes: &[u8],
        point: u64,
        recorder: &mut dyn ChangeRecorder,
    ) -> Result<(), EditError> {
        self.check_editable()?;
        self.check_range(&range)?;
        let before: Vec<(MarkerId, u64)> = self
            .markers
            .iter()
            .map(|(id, marker)| (id, marker.pos))
            .collect();
        self.apply_delete_recording(range.clone(), point, recorder, false)?;
        self.apply_insert(range.start, bytes, recorder)?;
        let removed = range.end - range.start;
        let added = bytes.len() as u64;
        for (id, position) in before {
            let moved = if position >= range.end {
                position - removed + added
            } else if position > range.start {
                range.start
            } else {
                position
            };
            self.markers[id].pos = moved;
        }
        Ok(())
    }

    fn apply_delete_recording(
        &mut self,
        range: Range<u64>,
        point: u64,
        recorder: &mut dyn ChangeRecorder,
        record_markers: bool,
    ) -> Result<Vec<u8>, EditError> {
        if range.is_empty() {
            return Ok(Vec::new());
        }
        let mut deleted = Vec::new();
        self.text.read(range.clone(), &mut deleted)?;
        let text = self.text.delete(range.clone())?;
        let adjustments = if record_markers {
            self.markers
                .iter()
                .filter_map(|(id, marker)| {
                    marker
                        .deletion_adjustment(&range)
                        .map(|adjustment| (id, adjustment))
                })
                .collect()
        } else {
            Vec::new()
        };
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
        self.faces.adjust_for_delete(&range);
        self.modified_tick += 1;
        self.checkpoints.clear();
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
        self.faces.clear();
        self.modified_tick += 1;
        self.checkpoints.clear();
        recorder.record(self.id, &Change::ReplaceAll { text });
    }

    /// Appends original bytes `range` at the end of the text: the visited
    /// file grew by an append. Undo records it as an insertion. It is not
    /// journaled: the caller ends the journal's epoch, since the text is
    /// the file's again.
    pub(crate) fn apply_append_original(&mut self, range: Range<u64>) {
        let at = self.text.len();
        let length = range.end - range.start;
        if length == 0 {
            return;
        }
        let text = self.text.append_original(range);
        let context = self.change_context();
        self.undo.record_insert(at, length, &context);
        self.text = text;
        for marker in self.markers.values_mut() {
            marker.adjust_for_insert(at, length);
        }
        self.faces.adjust_for_insert(at, length);
        self.modified_tick += 1;
        self.checkpoints.clear();
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
