//! The live reload state machine (DESIGN sections 1 and 3). A watcher or
//! poller event makes the editor look at the file (`Effect::Observe`);
//! a change is waited out until two looks a settle window apart agree;
//! the settled change is then classified and applied: an unmodified
//! buffer follows an append or is reloaded, both as undoable changes, and
//! a modified buffer is never touched but flagged as a conflict. All file
//! reads happen in effects; nothing here blocks on I/O.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use emx_proto::{ClientKind, ExternalKind};
use emx_text::{BlockCache, FileIdentity, Original, Sources, Text};

use super::files::Purpose;
use super::{Editor, TimerPurpose};
use crate::buffer::{BufferId, Change, ChangeRecorder};
use crate::command::CommandError;
use crate::diff::{DiffJob, DiffRequester, DiffSide};
use crate::event::{ClientId, Effect, ReloadJob, TimerId};
use crate::marker::MarkerId;
use crate::reload::{
    AppendRead, AppendedBytes, Classification, ExternalState, Observation, Reloaded, classify,
};
use crate::undo::UndoEntry;
use crate::window::PrevBuffer;

const DIFF_BUFFER: &str = "*diff-external*";
const NOT_VISITING: &str = "Buffer does not seem to be associated with any file";

/// Text that is already on disk: a reload's change is not journaled as an
/// edit; the journal's epoch ends instead (`JournalOutbox::reloaded`).
pub(super) struct Unjournaled;

impl ChangeRecorder for Unjournaled {
    fn record(&mut self, _buffer: BufferId, _change: &Change) {}
}

/// How many more settle windows a round of reloads waits for files of
/// the round still settling or being read.
const RELOAD_ROUND_EXTENSIONS: u32 = 4;
const RELOAD_SUMMARY_SEPARATOR: &str = ", ";
const RELOAD_SUMMARY_MORE: &str = ", ...";
const RELOAD_SUMMARY_ELLIPSIS: &str = "...";

/// The automatic reloads of one settle round, and the conflicts found in
/// it, summarized when it ends.
#[derive(Debug)]
pub(super) struct ReloadRound {
    reloaded: Vec<String>,
    conflicts: Vec<String>,
    timer: TimerId,
    extensions: u32,
}

/// `names` cut to its first `shown`: `...` stands for the rest.
fn listing(names: &[String], shown: usize) -> String {
    if shown == 0 {
        return RELOAD_SUMMARY_ELLIPSIS.to_string();
    }
    let mut text = names[..shown].join(RELOAD_SUMMARY_SEPARATOR);
    if shown < names.len() {
        text.push_str(RELOAD_SUMMARY_MORE);
    }
    text
}

/// `Reloaded N files changed on disk: a, b; M conflicts: x, y`, cut to
/// `width` columns. Reloaded names give way first, as a conflict is what
/// the user has to act on; then conflict names; the counts always stay.
pub(crate) fn reload_summary(
    reloaded: &[String],
    conflicts: &[String],
    width: usize,
    tab_width: usize,
) -> String {
    let fits = |text: &str| crate::render::text_width(text.as_bytes(), tab_width) <= width;
    let conflict_label = match conflicts.len() {
        1 => "; 1 conflict: ".to_string(),
        count => format!("; {count} conflicts: "),
    };
    let compose = |shown_reloaded: usize, shown_conflicts: usize| {
        let mut message = format!(
            "Reloaded {} files changed on disk: {}",
            reloaded.len(),
            listing(reloaded, shown_reloaded)
        );
        if !conflicts.is_empty() {
            message.push_str(&conflict_label);
            message.push_str(&listing(conflicts, shown_conflicts));
        }
        message
    };
    let mut choices: Vec<(usize, usize)> = (0..=reloaded.len())
        .rev()
        .map(|shown| (shown, conflicts.len()))
        .collect();
    choices.extend((0..conflicts.len()).rev().map(|shown| (0, shown)));
    choices
        .into_iter()
        .map(|(shown_reloaded, shown_conflicts)| compose(shown_reloaded, shown_conflicts))
        .find(|message| fits(message))
        // Not even the counts fit; the echo area clips what is too wide.
        .unwrap_or_else(|| compose(0, 0))
}

/// Why a buffer is being read again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReloadReason {
    /// A settled external change to an unmodified buffer.
    Automatic,
    /// `revert-buffer`, confirmed: the user's edits may be discarded.
    Revert(Option<ClientId>),
    /// `emx reload`.
    Request,
}

/// A reload in flight.
#[derive(Debug)]
pub(super) struct ReloadRequest {
    reason: ReloadReason,
    previous: Text,
    /// The window markers whose positions were sent as anchors, in order.
    markers: Vec<(MarkerId, u64)>,
}

/// Why a reload was not applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ReloadFailure {
    Modified,
    Failed(String),
}

/// Where the appended bytes come from.
enum AppendSource {
    /// Read by a worker for a buffer holding its file in memory.
    Bytes(Vec<u8>),
    /// The file-backed original grows to `new_len`.
    Original {
        cache: Arc<BlockCache>,
        new_len: u64,
    },
}

impl Editor {
    // ---- Events ----------------------------------------------------------

    pub(super) fn file_changed(&mut self, path: &std::path::Path) {
        let buffers = self.buffers_visiting(path);
        for buffer in buffers {
            self.external_event(buffer);
        }
    }

    pub(super) fn file_renamed(&mut self, from: &std::path::Path, to: &std::path::Path) {
        for buffer in self.buffers_visiting(from) {
            self.note_changed(buffer);
            let binding = self.binding_mut(buffer);
            binding.external = ExternalState::Renamed(to.to_path_buf());
            self.external_change_event(
                buffer,
                ExternalKind::Renamed {
                    to: emx_proto::WirePath::from(to),
                },
            );
        }
        self.file_changed(to);
    }

    fn buffers_visiting(&self, path: &std::path::Path) -> Vec<BufferId> {
        self.buffers
            .iter()
            .filter(|(_, buffer)| buffer.file.as_ref().is_some_and(|file| file.path == path))
            .map(|(id, _)| id)
            .collect()
    }

    fn binding_mut(&mut self, buffer: BufferId) -> &mut crate::buffer::FileBinding {
        self.buffers[buffer]
            .file
            .as_mut()
            .expect("reloaded buffers visit a file")
    }

    /// While the buffer is read, saved or reloaded, its file is expected to
    /// change or its version is about to: it is looked at again after.
    fn reload_busy(&self, buffer: BufferId) -> bool {
        let target = &self.buffers[buffer];
        target.loading
            || target.damaged
            || target.file.as_ref().is_some_and(|file| file.saving)
            || self.pending_saves.contains_key(&buffer)
            || self.reloading.contains_key(&buffer)
            || self.appending.contains_key(&buffer)
    }

    pub(super) fn external_event(&mut self, buffer: BufferId) {
        if self.reload_busy(buffer) {
            self.reload_recheck.insert(buffer);
            return;
        }
        if matches!(
            self.binding_mut(buffer).external,
            ExternalState::Settling { .. }
        ) {
            // The settle timer looks again anyway.
            return;
        }
        self.start_observe(buffer);
    }

    fn start_observe(&mut self, buffer: BufferId) {
        if let Some(again) = self.observing.get_mut(&buffer) {
            *again = true;
            return;
        }
        self.observing.insert(buffer, false);
        let target = &self.buffers[buffer];
        let binding = target.file.as_ref().expect("observed buffers visit a file");
        self.effects.push(Effect::Observe {
            buffer,
            path: binding.path.clone(),
            old_len: binding.identity.map(|identity| identity.size),
        });
    }

    /// A save, load or reload finished: an event that came meanwhile is
    /// looked into now.
    pub(super) fn reload_after_busy(&mut self, buffer: BufferId) {
        if self.buffers.contains_key(buffer)
            && !self.reload_busy(buffer)
            && self.reload_recheck.remove(&buffer)
        {
            self.start_observe(buffer);
        }
    }

    pub(super) fn observed(&mut self, buffer: BufferId, result: Result<Observation, String>) {
        if self.save_verify.contains_key(&buffer) {
            self.save_disk_checked(buffer, result);
            return;
        }
        let again = self.observing.remove(&buffer).unwrap_or(false);
        if !self.buffers.contains_key(buffer) || self.buffers[buffer].file.is_none() {
            return;
        }
        if self.reload_busy(buffer) {
            self.reload_recheck.insert(buffer);
            return;
        }
        let observation = match result {
            Ok(observation) => observation,
            Err(message) => {
                let name = self.buffers[buffer].name.clone();
                let message = format!("Checking {name} on disk failed: {message}");
                // A confirmed revert is not carried out later by surprise:
                // whoever asked learns it failed.
                match self.revert_pending.remove(&buffer) {
                    Some(Some(client)) if self.clients.contains_key(&client) => {
                        self.fail(client, &CommandError::User(message));
                    }
                    _ => self.notify_all(message),
                }
                // A settling file is looked at again at the next window;
                // the failure may be passing (a permission being changed).
                if matches!(
                    self.binding_mut(buffer).external,
                    ExternalState::Settling { .. }
                ) {
                    self.arm_settle_timer(buffer);
                }
                return;
            }
        };
        let binding = self.binding_mut(buffer);
        let state = binding.external.clone();
        let visited = binding.identity;
        match state {
            ExternalState::Settling { last, previous, .. } => {
                if observation.same_version(&last) {
                    self.settled(buffer, observation, *previous);
                } else {
                    let binding = self.binding_mut(buffer);
                    if let ExternalState::Settling { last, .. } = &mut binding.external {
                        *last = observation;
                    }
                    self.arm_settle_timer(buffer);
                }
            }
            state => {
                if observation.identity == state.known_disk(visited) {
                    if let Some(client) = self.revert_pending.remove(&buffer) {
                        self.start_reload(buffer, ReloadReason::Revert(client));
                    } else if again {
                        self.start_observe(buffer);
                    }
                    return;
                }
                self.binding_mut(buffer).external = ExternalState::Settling {
                    first_seen: observation.at,
                    last: observation,
                    previous: Box::new(state),
                };
                self.arm_settle_timer(buffer);
            }
        }
    }

    fn arm_settle_timer(&mut self, buffer: BufferId) {
        let delay = self.config.settle_window_ms;
        let id = self.set_timer(TimerPurpose::Settle(buffer), delay);
        self.settle_timers.insert(buffer, id);
    }

    pub(super) fn settle_timer(&mut self, buffer: BufferId, id: TimerId) {
        if self.settle_timers.get(&buffer) != Some(&id) {
            return;
        }
        self.settle_timers.remove(&buffer);
        if !self.buffers.contains_key(buffer) {
            return;
        }
        let settling = self.buffers[buffer]
            .file
            .as_ref()
            .is_some_and(|file| matches!(file.external, ExternalState::Settling { .. }));
        if !settling {
            return;
        }
        if self.reload_busy(buffer) {
            self.reload_recheck.insert(buffer);
        } else {
            self.start_observe(buffer);
        }
    }

    /// The file stopped changing at `observation`.
    fn settled(&mut self, buffer: BufferId, observation: Observation, previous: ExternalState) {
        let binding = self.binding_mut(buffer);
        binding.external = previous.clone();
        let known = previous.known_disk(binding.identity);
        if let Some(client) = self.revert_pending.remove(&buffer) {
            self.start_reload(buffer, ReloadReason::Revert(client));
            return;
        }
        if observation.identity == known {
            return;
        }
        let Some(identity) = observation.identity else {
            self.note_changed(buffer);
            let binding = self.binding_mut(buffer);
            binding.external = ExternalState::Deleted;
            self.external_change_event(buffer, ExternalKind::Deleted);
            return;
        };
        let classification = match known {
            Some(old) => classify(self.binding_mut(buffer), &old, &observation),
            None => Classification::Replaced,
        };
        if matches!(
            classification,
            Classification::Unchanged | Classification::OwnSave
        ) {
            return;
        }
        let rewritten_in_place = !matches!(classification, Classification::Appended { .. })
            && known.is_some_and(|old| old.dev == identity.dev && old.ino == identity.ino);
        if rewritten_in_place {
            let affected = self.retire_caches_on(identity.dev, identity.ino, None);
            for other in affected {
                if other != buffer {
                    self.external_event(other);
                }
            }
        }
        if self.buffers[buffer].is_modified() {
            self.conflict(buffer, identity);
            return;
        }
        match classification {
            Classification::Appended { new_len } if previous == ExternalState::Clean => {
                self.follow_append(buffer, observation, identity, new_len);
            }
            Classification::Truncated => {
                self.external_change_event(buffer, ExternalKind::Truncated);
                self.start_reload(buffer, ReloadReason::Automatic);
            }
            _ => self.start_reload(buffer, ReloadReason::Automatic),
        }
    }

    /// A modified buffer whose file changed: flagged, never reloaded. If
    /// the change rewrote the file it reads unchanged text from, what it
    /// still holds is put in a `*recovered: NAME*` buffer.
    pub(super) fn conflict(&mut self, buffer: BufferId, disk: FileIdentity) {
        self.note_changed(buffer);
        let binding = self.binding_mut(buffer);
        binding.external = ExternalState::Conflict { disk };
        let name = self.buffers[buffer].name.clone();
        let retired = self.buffers[buffer].text.retired_pieces();
        let message = if retired.is_empty() {
            format!("{name} changed on disk; M-x diff-external, M-x revert-buffer, M-x keep-mine")
        } else {
            let (content, missing) = self.materialize(buffer);
            let rescue = self.create_recovered_buffer(
                &crate::journal::recovered_buffer_name(&name),
                content,
                None,
            );
            let rescue = self.buffers[rescue].name.clone();
            format!(
                "{name} was rewritten on disk: the text {name} read from the file is gone \
                 ({missing} bytes); what could be recovered is in {rescue}"
            )
        };
        self.notify_all(message);
        self.external_change_event(buffer, ExternalKind::Conflict);
        // A summary of the same round must not hide this echo.
        self.join_reload_round(buffer, true);
    }

    /// The buffer's text with every range that can no longer be read
    /// replaced by a line saying what is missing, and the bytes missing.
    fn materialize(&self, buffer: BufferId) -> (Vec<u8>, u64) {
        let target = &self.buffers[buffer];
        let text = &target.text;
        let mut content = format!(
            "Text of {} as far as it could be recovered: its file was rewritten on disk \
             and the bytes it still read from it are gone. Each gap is marked on a line \
             of its own.\n\n",
            target.name
        )
        .into_bytes();
        let mut missing = 0;
        for (offset, piece) in text.pieces() {
            let mut bytes = Vec::new();
            match text.read(offset..offset + piece.len, &mut bytes) {
                Ok(()) => content.extend_from_slice(&bytes),
                Err(_) => {
                    missing += piece.len;
                    content.extend_from_slice(
                        format!(
                            "\n[emx: {} bytes of the original file (offsets {}-{}) are gone here]\n",
                            piece.len,
                            piece.start,
                            piece.start + piece.len
                        )
                        .as_bytes(),
                    );
                }
            }
        }
        (content, missing)
    }

    /// Retires every block cache on the inode that was rewritten in place,
    /// wherever a text still reads it: buffers, their undo history and
    /// their previous text. `keep` is the cache for the new version.
    /// Returns the buffers whose current text was retired.
    pub(super) fn retire_caches_on(
        &mut self,
        dev: u64,
        ino: u64,
        keep: Option<&Arc<BlockCache>>,
    ) -> Vec<BufferId> {
        let retire = |text: &Text| {
            if let Original::File(cache) = &text.sources().original {
                let opened = cache.identity();
                let kept = keep.is_some_and(|keep| Arc::ptr_eq(keep, cache));
                if opened.dev == dev && opened.ino == ino && !kept {
                    cache.retire();
                }
            }
        };
        let mut affected = Vec::new();
        for (id, target) in &self.buffers {
            retire(&target.text);
            if let Some(previous) = target
                .file
                .as_ref()
                .and_then(|file| file.previous_text.as_ref())
            {
                retire(previous);
            }
            for entry in target.undo.entries() {
                if let UndoEntry::ReplaceAll { before, after } = entry {
                    retire(&before);
                    retire(&after);
                }
            }
            if !target.text.retired_pieces().is_empty() {
                affected.push(id);
            }
        }
        affected
    }

    // ---- Appends ---------------------------------------------------------

    fn follow_append(
        &mut self,
        buffer: BufferId,
        observation: Observation,
        identity: FileIdentity,
        new_len: u64,
    ) {
        let target = &self.buffers[buffer];
        let binding = target.file.as_ref().expect("visits a file");
        let old = binding.identity.expect("an append grows an existing file");
        match &target.text.sources().original {
            Original::Memory(_) => {
                self.appending.insert(buffer, target.text.clone());
                self.effects.push(Effect::ReadAppend {
                    buffer,
                    path: binding.path.clone(),
                    old,
                    new: identity,
                    old_tail_hash: binding.tail_hash,
                });
            }
            Original::File(cache) => match observation.end_hash {
                // Only an original that is exactly the old version may grow
                // into the new one.
                Some(end_hash) if cache.identity() == old => {
                    let cache = cache.clone();
                    let previous = target.text.clone();
                    self.apply_append(
                        buffer,
                        previous,
                        AppendSource::Original { cache, new_len },
                        identity,
                        end_hash,
                    );
                }
                _ => self.start_reload(buffer, ReloadReason::Automatic),
            },
        }
    }

    pub(super) fn append_read(&mut self, buffer: BufferId, result: Result<AppendRead, String>) {
        let Some(previous) = self.appending.remove(&buffer) else {
            return;
        };
        if !self.buffers.contains_key(buffer) {
            return;
        }
        match result {
            Ok(AppendRead::Read(AppendedBytes {
                identity,
                bytes,
                end_hash,
            })) if self.buffers[buffer].text.same_root(&previous) => {
                self.apply_append(
                    buffer,
                    previous,
                    AppendSource::Bytes(bytes),
                    identity,
                    end_hash,
                );
            }
            // The file or the buffer changed meanwhile: classify afresh.
            Ok(_) => {
                self.reload_recheck.insert(buffer);
            }
            Err(message) => {
                let name = self.buffers[buffer].name.clone();
                self.notify_all(format!(
                    "Reading what was appended to {name} failed: {message}"
                ));
            }
        }
        self.reload_after_busy(buffer);
    }

    fn apply_append(
        &mut self,
        buffer: BufferId,
        previous: Text,
        source: AppendSource,
        identity: FileIdentity,
        tail_hash: u64,
    ) {
        let point = self.reload_boundary_point(buffer);
        let followers = self.window_markers_at(buffer, previous.len());
        let target = &mut self.buffers[buffer];
        target.undo.boundary(point);
        target.undo.record_reload();
        let at = target.text.len();
        let needs_index = match source {
            AppendSource::Bytes(bytes) => {
                if let Err(error) = target.apply_insert(at, &bytes, &mut Unjournaled) {
                    // Nothing changed: no reload marker may be left to make
                    // a later undo think a reload was undone.
                    target.undo.discard_reload_mark();
                    let name = target.name.clone();
                    self.notify_all(format!("Following the append to {name} failed: {error}"));
                    return;
                }
                false
            }
            AppendSource::Original { cache, new_len } => {
                let old_len = cache.len();
                cache.appended(identity);
                target.text.sources().index.extend(new_len);
                target.apply_append_original(old_len..new_len);
                true
            }
        };
        let end = target.text.len();
        target.undo.boundary(point);
        target.set_unmodified();
        for marker in followers {
            target.set_marker(marker, end);
        }
        let text = target.text.clone();
        self.note_changed(buffer);
        let binding = self.binding_mut(buffer);
        binding.identity = Some(identity);
        binding.tail_hash = tail_hash;
        binding.external = ExternalState::Clean;
        binding.previous_text = Some(previous);
        binding.content_predates_disk = false;
        self.journal_outbox.reloaded(buffer, identity, text);
        if needs_index {
            self.start_indexing(buffer);
        }
        self.external_change_event(buffer, ExternalKind::Appended);
    }

    // ---- Reloads ---------------------------------------------------------

    /// Point for the undo boundaries around a reload: where the buffer is
    /// shown, so undoing the reload leaves point there.
    fn reload_boundary_point(&self, buffer: BufferId) -> u64 {
        self.clients
            .values()
            .flat_map(|client| client.frame.windows.values())
            .find(|window| window.buffer == buffer)
            .map_or(0, |window| self.buffers[buffer].marker_pos(window.point))
    }

    /// The markers of every window showing the buffer, of every window's
    /// memory of it (`window-prev-buffers`), and the point it had when last
    /// shown.
    fn window_markers(&self, buffer: BufferId) -> Vec<MarkerId> {
        let mut markers: Vec<MarkerId> = self
            .clients
            .values()
            .flat_map(|client| client.frame.windows.values())
            .filter(|window| window.buffer == buffer)
            .flat_map(|window| [window.point, window.start])
            .collect();
        markers.extend(
            self.prev_buffer_entries(buffer)
                .flat_map(|entry| [entry.point, entry.start]),
        );
        markers.extend(self.buffers[buffer].last_point);
        markers
    }

    /// Every window's remembered start and point for the buffer.
    fn prev_buffer_entries(&self, buffer: BufferId) -> impl Iterator<Item = PrevBuffer> + '_ {
        self.clients
            .values()
            .flat_map(|client| client.frame.prev_buffers.values())
            .flatten()
            .filter(move |entry| entry.buffer == buffer)
            .copied()
    }

    /// Window points (and the remembered point) at `position`.
    fn window_markers_at(&self, buffer: BufferId, position: u64) -> Vec<MarkerId> {
        let target = &self.buffers[buffer];
        let mut points: Vec<MarkerId> = self
            .clients
            .values()
            .flat_map(|client| client.frame.windows.values())
            .filter(|window| window.buffer == buffer)
            .map(|window| window.point)
            .collect();
        points.extend(self.prev_buffer_entries(buffer).map(|entry| entry.point));
        points.extend(target.last_point);
        points
            .into_iter()
            .filter(|marker| target.marker_position(*marker) == Some(position))
            .collect()
    }

    pub(super) fn start_reload(&mut self, buffer: BufferId, reason: ReloadReason) {
        if let Some(request) = self.reloading.get_mut(&buffer) {
            // A reload is already reading the file; a revert or request
            // rides on it.
            if reason != ReloadReason::Automatic {
                request.reason = reason;
            }
            return;
        }
        let target = &self.buffers[buffer];
        let markers: Vec<(MarkerId, u64)> = self
            .window_markers(buffer)
            .into_iter()
            .filter_map(|marker| target.marker_position(marker).map(|pos| (marker, pos)))
            .collect();
        let previous = target.text.clone();
        let path = target
            .file
            .as_ref()
            .expect("reloaded buffers visit a file")
            .path
            .clone();
        let anchors = markers.iter().map(|(_, pos)| *pos).collect();
        self.reloading.insert(
            buffer,
            ReloadRequest {
                reason,
                previous: previous.clone(),
                markers,
            },
        );
        self.effects.push(Effect::Reload(ReloadJob {
            buffer,
            path,
            previous,
            anchors,
        }));
    }

    pub(super) fn reloaded(&mut self, buffer: BufferId, result: Result<Reloaded, String>) {
        let Some(request) = self.reloading.remove(&buffer) else {
            return;
        };
        let outcome = self.finish_reload(buffer, &request, result);
        if let Err(failure) = &outcome {
            let message = match failure {
                ReloadFailure::Modified => None,
                ReloadFailure::Failed(message) => Some(message.clone()),
            };
            match (request.reason, message) {
                (ReloadReason::Revert(Some(client)), Some(message)) => {
                    self.fail(client, &CommandError::User(message));
                }
                (ReloadReason::Automatic, Some(message)) => self.notify_all(message),
                _ => {}
            }
        }
        if self.reverting.remove(&buffer) {
            self.reload_request_done(buffer, outcome);
        }
        self.reload_after_busy(buffer);
    }

    fn finish_reload(
        &mut self,
        buffer: BufferId,
        request: &ReloadRequest,
        result: Result<Reloaded, String>,
    ) -> Result<(), ReloadFailure> {
        let Some(target) = self.buffers.get(buffer) else {
            return Err(ReloadFailure::Failed(
                "the buffer was killed while it was read".to_string(),
            ));
        };
        let name = target.name.clone();
        let reloaded =
            result.map_err(|message| ReloadFailure::Failed(format!("{name}: {message}")))?;
        let changed_meanwhile = !target.text.same_root(&request.previous);
        let modified = target.is_modified();
        let Some(identity) = reloaded.loaded.identity else {
            self.note_changed(buffer);
            let binding = self.binding_mut(buffer);
            binding.external = ExternalState::Deleted;
            let path = binding.path.display().to_string();
            return Err(ReloadFailure::Failed(match request.reason {
                ReloadReason::Request => format!("{name}: the file no longer exists"),
                _ => format!("File {path} no longer exists"),
            }));
        };
        self.retire_if_rewritten(buffer, &request.previous, &reloaded, identity);
        match request.reason {
            ReloadReason::Automatic if changed_meanwhile || modified => {
                // Typed into while it was read: never replace the edits.
                if modified {
                    self.conflict(buffer, identity);
                } else {
                    self.reload_recheck.insert(buffer);
                }
                return Err(ReloadFailure::Modified);
            }
            ReloadReason::Request if modified => return Err(ReloadFailure::Modified),
            ReloadReason::Revert(_) | ReloadReason::Request if changed_meanwhile => {
                return Err(ReloadFailure::Failed(format!(
                    "{name} changed while it was being read again; not reverted"
                )));
            }
            _ => {}
        }
        self.apply_reload(buffer, request, reloaded, identity);
        Ok(())
    }

    /// A reload that did not settle first (`emx reload`, a revert) may be
    /// the first to see that the file was rewritten in place: the text it
    /// replaces, and anything else reading the old version, is retired
    /// before anyone can read new bytes at old offsets. A larger file on
    /// the same inode may have been only appended to, but nothing here
    /// proves it, so it is treated the same.
    fn retire_if_rewritten(
        &mut self,
        buffer: BufferId,
        previous: &Text,
        reloaded: &Reloaded,
        identity: FileIdentity,
    ) {
        let Original::File(cache) = &previous.sources().original else {
            return;
        };
        let old = cache.identity();
        if old.dev != identity.dev || old.ino != identity.ino || old == identity {
            return;
        }
        let keep = match &reloaded.loaded.sources.original {
            Original::File(cache) => Some(cache.clone()),
            Original::Memory(_) => None,
        };
        let affected = self.retire_caches_on(identity.dev, identity.ino, keep.as_ref());
        for other in affected {
            if other != buffer {
                self.external_event(other);
            }
        }
    }

    fn apply_reload(
        &mut self,
        buffer: BufferId,
        request: &ReloadRequest,
        reloaded: Reloaded,
        identity: FileIdentity,
    ) {
        let loaded = reloaded.loaded;
        let text = Text::from_original(loaded.sources.clone());
        let point = self.reload_boundary_point(buffer);
        let target = &mut self.buffers[buffer];
        // Markers moved since the anchors were taken stay where the
        // replacement clamps them.
        let unmoved: Vec<(MarkerId, u64)> = request
            .markers
            .iter()
            .zip(&reloaded.anchors)
            .filter(|((marker, old), _)| target.marker_position(*marker) == Some(*old))
            .map(|((marker, _), new)| (*marker, *new))
            .collect();
        target.undo.boundary(point);
        target.undo.record_reload();
        target.apply_replace_all(text.clone(), &mut Unjournaled);
        for (marker, position) in unmoved {
            target.set_marker(marker, position);
        }
        let point_after = point.min(target.text.len());
        target.undo.boundary(point_after);
        target.set_unmodified();
        target.coding = loaded.coding;
        target.read_only = !loaded.writable;
        target.auto_saved_since_save = false;
        self.note_changed(buffer);
        let binding = self.binding_mut(buffer);
        binding.identity = Some(identity);
        binding.tail_hash = loaded.tail_hash;
        binding.external = ExternalState::Clean;
        binding.previous_text = Some(request.previous.clone());
        binding.content_predates_disk = false;
        self.journal_outbox.reloaded(buffer, identity, text);
        if loaded.needs_index {
            self.start_indexing(buffer);
        }
        if self.follow_edit
            && let Some(first) = reloaded.first_change
        {
            let points: Vec<MarkerId> = self
                .clients
                .values()
                .flat_map(|client| client.frame.windows.values())
                .filter(|window| window.buffer == buffer)
                .map(|window| window.point)
                .collect();
            for marker in points {
                self.buffers[buffer].set_marker(marker, first);
            }
        }
        self.external_change_event(buffer, ExternalKind::Reloaded);
        if request.reason == ReloadReason::Automatic {
            self.join_reload_round(buffer, false);
        }
    }

    /// An `emx reload` request starts the one reload path; the reply waits
    /// for the result.
    pub(super) fn request_reload_of(&mut self, buffer: BufferId) {
        self.start_reload(buffer, ReloadReason::Request);
    }

    // ---- Commands --------------------------------------------------------

    pub(crate) fn revert_buffer_command(&mut self, client: ClientId) -> Result<(), CommandError> {
        let buffer = self.selected_buffer(client);
        let target = &self.buffers[buffer];
        let Some(binding) = &target.file else {
            return Err(CommandError::User(NOT_VISITING.to_string()));
        };
        if target.loading {
            return Err(CommandError::BufferReadOnly(target.name.clone()));
        }
        if binding.saving || self.pending_saves.contains_key(&buffer) {
            return Err(crate::buffer::EditError::Saving.into());
        }
        if target.is_modified() {
            let question = format!("Revert buffer from file {}? ", binding.path.display());
            self.yes_or_no_p(client, question, Purpose::Revert { buffer });
            return Ok(());
        }
        self.start_revert(client, buffer);
        Ok(())
    }

    /// The user confirmed: look at the file once more, then read it.
    pub(super) fn start_revert(&mut self, client: ClientId, buffer: BufferId) {
        if !self.buffers.contains_key(buffer) {
            return;
        }
        self.revert_pending.insert(buffer, Some(client));
        if matches!(
            self.binding_mut(buffer).external,
            ExternalState::Settling { .. }
        ) {
            return;
        }
        self.start_observe(buffer);
    }

    pub(crate) fn keep_mine_command(&mut self, client: ClientId) -> Result<(), CommandError> {
        let buffer = self.selected_buffer(client);
        let target = &self.buffers[buffer];
        let name = target.name.clone();
        let Some(binding) = &target.file else {
            return Err(CommandError::User(NOT_VISITING.to_string()));
        };
        let disk = match &binding.external {
            ExternalState::Conflict { disk } | ExternalState::KeepMine { disk } => *disk,
            ExternalState::Clean if binding.content_predates_disk => match binding.identity {
                Some(identity) => identity,
                None => {
                    return Err(CommandError::User(format!(
                        "No change on disk to keep {name} over"
                    )));
                }
            },
            ExternalState::Settling { .. } => {
                return Err(CommandError::User(format!(
                    "{name} is still changing on disk; try again when it settles"
                )));
            }
            _ => {
                return Err(CommandError::User(format!(
                    "No change on disk to keep {name} over"
                )));
            }
        };
        self.binding_mut(buffer).external = ExternalState::KeepMine { disk };
        self.set_echo(
            client,
            format!("Keeping your version of {name}; saving it overwrites the file on disk"),
        );
        Ok(())
    }

    pub(crate) fn follow_edit_mode_command(&mut self, client: ClientId) {
        self.follow_edit = !self.follow_edit;
        let state = if self.follow_edit {
            "enabled"
        } else {
            "disabled"
        };
        self.set_echo(client, format!("Follow-Edit mode {state}"));
    }

    pub(crate) fn diff_external_command(&mut self, client: ClientId) -> Result<(), CommandError> {
        let buffer = self.selected_buffer(client);
        let job = self
            .diff_job(buffer, DiffRequester::Command(client))
            .map_err(CommandError::User)?;
        self.effects.push(Effect::Diff(job));
        Ok(())
    }

    /// The diff `diff-external` shows: the buffer against the file when
    /// they conflict, otherwise the text before the last reload against
    /// the text now.
    pub(super) fn diff_job(
        &self,
        buffer: BufferId,
        requester: DiffRequester,
    ) -> Result<DiffJob, String> {
        let target = &self.buffers[buffer];
        let name = target.name.clone();
        let Some(binding) = &target.file else {
            return Err(NOT_VISITING.to_string());
        };
        let limit = self.config.in_memory_max;
        let job = match (&binding.external, &binding.previous_text) {
            (ExternalState::Conflict { .. } | ExternalState::KeepMine { .. }, _) => DiffJob {
                buffer,
                requester,
                old_label: format!("{name} (buffer)"),
                new_label: format!("{name} (on disk)"),
                old: target.text.clone(),
                new: DiffSide::Disk(binding.path.clone()),
                limit,
            },
            (_, Some(previous)) => DiffJob {
                buffer,
                requester,
                old_label: format!("{name} (before reload)"),
                new_label: format!("{name} (reloaded)"),
                old: previous.clone(),
                new: DiffSide::Text(target.text.clone()),
                limit,
            },
            (_, None) => return Err(format!("No external change recorded for {name}")),
        };
        Ok(job)
    }

    pub(super) fn diff_ready(&mut self, requester: DiffRequester, result: Result<String, String>) {
        match requester {
            DiffRequester::Command(client) => {
                if !self.clients.contains_key(&client) {
                    return;
                }
                match result {
                    Ok(diff) => self.show_diff(client, diff),
                    Err(message) => self.fail(client, &CommandError::User(message)),
                }
            }
            DiffRequester::Request(client, id) => self.diff_request_done(client, id, result),
        }
    }

    fn show_diff(&mut self, client: ClientId, diff: String) {
        let existing: Vec<BufferId> = self
            .buffers
            .iter()
            .filter(|(_, buffer)| buffer.name == DIFF_BUFFER)
            .map(|(id, _)| id)
            .collect();
        for buffer in existing {
            self.kill_buffer(buffer);
        }
        let buffer = self.create_buffer(DIFF_BUFFER);
        let target = &mut self.buffers[buffer];
        target.text = Text::from_original(Sources::memory(diff.into_bytes().into()));
        target.read_only = true;
        self.show_in_selected_window(client, buffer);
    }

    // ---- Notifications ---------------------------------------------------

    /// Non-modal: shown in every terminal client's echo area.
    fn notify_all(&mut self, message: String) {
        let clients: Vec<ClientId> = self.clients.keys().copied().collect();
        self.show_echo_to(&clients, message);
    }

    /// Whether a terminal shows the buffer in one of its windows.
    fn shown(&self, buffer: BufferId) -> bool {
        self.clients
            .values()
            .filter(|client| client.kind == ClientKind::Tui)
            .flat_map(|client| client.frame.windows.values())
            .any(|window| window.buffer == buffer)
    }

    /// The file changed on disk: a buffer no window shows is marked
    /// "changed externally" (`[ext]`) for when it is shown again.
    fn note_changed(&mut self, buffer: BufferId) {
        if !self.shown(buffer) {
            self.binding_mut(buffer).changed_since_viewed = true;
        }
    }

    /// A key from `client` means its user has seen the buffers its windows
    /// show: their "changed externally" markers are cleared. The marker
    /// stays on the frames that first show such a buffer, so it is seen.
    pub(super) fn buffer_viewed(&mut self, client: ClientId) {
        let buffers: Vec<BufferId> = self
            .client(client)
            .frame
            .windows
            .values()
            .map(|window| window.buffer)
            .collect();
        for buffer in buffers {
            if let Some(binding) = self.buffers[buffer].file.as_mut() {
                binding.changed_since_viewed = false;
            }
        }
    }

    /// One message for the automatic reloads of a settle round: a
    /// checkout or a multi-file agent edit is reported once, not per file.
    fn join_reload_round(&mut self, buffer: BufferId, conflict: bool) {
        let name = self.buffers[buffer].name.clone();
        if self.reload_round.is_none() {
            let timer = self.set_timer(TimerPurpose::ReloadSummary, self.config.settle_window_ms);
            self.reload_round = Some(ReloadRound {
                reloaded: Vec::new(),
                conflicts: Vec::new(),
                timer,
                extensions: 0,
            });
        }
        let round = self.reload_round.as_mut().expect("started above");
        let names = if conflict {
            &mut round.conflicts
        } else {
            &mut round.reloaded
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }

    /// The round ends once no other file of it is still settling or being
    /// read, up to `RELOAD_ROUND_EXTENSIONS` windows later, so a log being
    /// appended to all the time cannot hold the message back.
    pub(super) fn reload_summary_timer(&mut self, id: TimerId) {
        let Some(round) = self.reload_round.as_mut() else {
            return;
        };
        if round.timer != id {
            return;
        }
        let in_flight = self
            .reloading
            .values()
            .any(|request| request.reason == ReloadReason::Automatic)
            || self.buffers.values().any(|buffer| {
                buffer
                    .file
                    .as_ref()
                    .is_some_and(|file| matches!(file.external, ExternalState::Settling { .. }))
            });
        if in_flight && round.extensions < RELOAD_ROUND_EXTENSIONS {
            round.extensions += 1;
            let timer = self.set_timer(TimerPurpose::ReloadSummary, self.config.settle_window_ms);
            if let Some(round) = self.reload_round.as_mut() {
                round.timer = timer;
            }
            return;
        }
        let Some(mut round) = self.reload_round.take() else {
            return;
        };
        // A single reload stays silent (DESIGN section 3), and a conflict
        // has said so on its own already.
        if round.reloaded.len() < 2 {
            return;
        }
        round.reloaded.sort();
        round.conflicts.sort();
        let tab_width = usize::try_from(self.config.tab_width).expect("tab_width is bounded");
        let clients: Vec<(ClientId, u16)> = self
            .clients
            .iter()
            .map(|(id, client)| (*id, client.cols))
            .collect();
        // Each terminal gets the summary fitted to its width; the log
        // keeps it once, whole.
        let unfitted = reload_summary(&round.reloaded, &round.conflicts, usize::MAX, tab_width);
        self.log_message(&unfitted);
        for (client, cols) in clients {
            let message = reload_summary(
                &round.reloaded,
                &round.conflicts,
                usize::from(cols),
                tab_width,
            );
            self.client_mut(client).echo = message;
        }
    }

    // ---- Watching --------------------------------------------------------

    /// Every visited file that has been read is watched once; a path no
    /// buffer visits any more is let go. Run after every event, so every
    /// way a buffer gains, changes or loses its file is covered.
    pub(super) fn sync_watches(&mut self) {
        // Compared by reference: this runs after every key.
        let visited: BTreeSet<&PathBuf> = self
            .buffers
            .values()
            .filter(|buffer| !buffer.loading)
            .filter_map(|buffer| buffer.file.as_ref().map(|file| &file.path))
            .collect();
        let unchanged = visited.len() == self.watched.len()
            && visited.iter().all(|path| self.watched.contains(*path));
        if !unchanged {
            let visited: BTreeSet<PathBuf> = visited.into_iter().cloned().collect();
            for path in self.watched.difference(&visited) {
                self.polled_paths.remove(path);
                self.effects.push(Effect::Unwatch { path: path.clone() });
            }
            for path in visited.difference(&self.watched) {
                self.effects.push(Effect::Watch { path: path.clone() });
            }
            self.watched = visited;
        }
        let polled = &self.polled_paths;
        for buffer in self.buffers.values_mut() {
            if let Some(file) = buffer.file.as_mut() {
                file.polled = polled.contains(&file.path);
            }
        }
    }

    pub(super) fn file_polled(&mut self, path: PathBuf, notice: Option<String>) {
        // The path may have been let go since.
        if !self.watched.contains(&path) {
            return;
        }
        self.polled_paths.insert(path);
        if let Some(notice) = notice {
            self.notify_all(notice);
        }
    }

    /// Buffers with reload work in flight, which `kill_buffer` forgets.
    pub(super) fn forget_reload_state(&mut self, buffer: BufferId) {
        if self.reverting.remove(&buffer) {
            let name = self.buffers[buffer].name.clone();
            self.reload_request_done(
                buffer,
                Err(ReloadFailure::Failed(format!(
                    "{name}: the buffer was killed while it was read"
                ))),
            );
        }
        if let Some((client, _)) = self.save_verify.remove(&buffer) {
            let name = self.buffers[buffer].name.clone();
            self.fail(
                client,
                &CommandError::User(format!("{name} was killed; not saved")),
            );
        }
        self.observing.remove(&buffer);
        self.settle_timers.remove(&buffer);
        self.reload_recheck.remove(&buffer);
        self.reloading.remove(&buffer);
        self.appending.remove(&buffer);
        self.revert_pending.remove(&buffer);
    }
}

#[cfg(test)]
mod tests {
    use super::reload_summary;

    const TAB_WIDTH: usize = 8;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn summary_lists_every_name_that_fits() {
        let all = names(&["a.rs", "b.rs", "c.rs"]);
        let full = "Reloaded 3 files changed on disk: a.rs, b.rs, c.rs";
        assert_eq!(reload_summary(&all, &[], 80, TAB_WIDTH), full);
        assert_eq!(reload_summary(&all, &[], full.len(), TAB_WIDTH), full);
    }

    #[test]
    fn summary_is_cut_to_the_echo_width() {
        let all = names(&["alpha.rs", "beta.rs", "gamma.rs", "delta.rs"]);
        let cut = reload_summary(&all, &[], 56, TAB_WIDTH);
        assert_eq!(
            cut,
            "Reloaded 4 files changed on disk: alpha.rs, beta.rs, ..."
        );
        assert!(cut.len() <= 56);
        assert_eq!(
            reload_summary(&all, &[], 10, TAB_WIDTH),
            "Reloaded 4 files changed on disk: ..."
        );
    }

    #[test]
    fn summary_names_conflicts_and_cuts_reloads_first() {
        let reloaded = names(&["alpha.rs", "beta.rs", "gamma.rs"]);
        let one = names(&["x.rs"]);
        assert_eq!(
            reload_summary(&reloaded, &one, 80, TAB_WIDTH),
            "Reloaded 3 files changed on disk: alpha.rs, beta.rs, gamma.rs; 1 conflict: x.rs"
        );
        assert_eq!(
            reload_summary(&reloaded, &one, 74, TAB_WIDTH),
            "Reloaded 3 files changed on disk: alpha.rs, beta.rs, ...; 1 conflict: x.rs"
        );
        let two = names(&["x.rs", "y.rs"]);
        assert_eq!(
            reload_summary(&reloaded, &two, 62, TAB_WIDTH),
            "Reloaded 3 files changed on disk: ...; 2 conflicts: x.rs, y.rs"
        );
        assert_eq!(
            reload_summary(&reloaded, &two, 61, TAB_WIDTH),
            "Reloaded 3 files changed on disk: ...; 2 conflicts: x.rs, ..."
        );
        assert_eq!(
            reload_summary(&reloaded, &two, 20, TAB_WIDTH),
            "Reloaded 3 files changed on disk: ...; 2 conflicts: ..."
        );
    }

    #[test]
    fn summary_width_counts_columns_not_bytes() {
        let all = names(&["\u{4e2d}\u{6587}.rs", "b.rs"]);
        let full = "Reloaded 2 files changed on disk: \u{4e2d}\u{6587}.rs, b.rs";
        let columns = full.chars().count() + 2;
        assert_eq!(reload_summary(&all, &[], columns, TAB_WIDTH), full);
        assert_eq!(
            reload_summary(&all, &[], columns - 1, TAB_WIDTH),
            "Reloaded 2 files changed on disk: \u{4e2d}\u{6587}.rs, ..."
        );
    }
}
