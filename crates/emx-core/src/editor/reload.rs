//! The live reload state machine (DESIGN sections 1 and 3). A watcher or
//! poller event makes the editor look at the file (`Effect::Observe`);
//! a change is waited out until two looks a settle window apart agree;
//! the settled change is then classified and applied: an unmodified
//! buffer follows an append or is reloaded, both as undoable changes, and
//! a modified buffer is never touched but flagged as a conflict. All file
//! reads happen in effects; nothing here blocks on I/O.

use std::sync::Arc;

use emx_proto::ExternalKind;
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

const DIFF_BUFFER: &str = "*diff-external*";
const NOT_VISITING: &str = "Buffer does not seem to be associated with any file";

/// Text that is already on disk: a reload's change is not journaled as an
/// edit; the journal's epoch ends instead (`JournalOutbox::reloaded`).
pub(super) struct Unjournaled;

impl ChangeRecorder for Unjournaled {
    fn record(&mut self, _buffer: BufferId, _change: &Change) {}
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
            let binding = self.binding_mut(buffer);
            binding.external = ExternalState::Renamed(to.to_path_buf());
            binding.changed_since_viewed = true;
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
                self.notify_all(format!("Checking {name} on disk failed: {message}"));
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
            let binding = self.binding_mut(buffer);
            binding.external = ExternalState::Deleted;
            binding.changed_since_viewed = true;
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
    fn conflict(&mut self, buffer: BufferId, disk: FileIdentity) {
        let binding = self.binding_mut(buffer);
        binding.external = ExternalState::Conflict { disk };
        binding.changed_since_viewed = true;
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
        let binding = self.binding_mut(buffer);
        binding.identity = Some(identity);
        binding.tail_hash = tail_hash;
        binding.external = ExternalState::Clean;
        binding.previous_text = Some(previous);
        binding.changed_since_viewed = true;
        binding.content_predates_disk = false;
        self.journal_outbox.reloaded(buffer, identity, text);
        if needs_index {
            self.effects.push(Effect::StartIndex { buffer });
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

    /// The markers of every window showing the buffer, and the point it
    /// had when last shown.
    fn window_markers(&self, buffer: BufferId) -> Vec<MarkerId> {
        let mut markers: Vec<MarkerId> = self
            .clients
            .values()
            .flat_map(|client| client.frame.windows.values())
            .filter(|window| window.buffer == buffer)
            .flat_map(|window| [window.point, window.start])
            .collect();
        markers.extend(self.buffers[buffer].last_point);
        markers
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
            let binding = self.binding_mut(buffer);
            binding.external = ExternalState::Deleted;
            binding.changed_since_viewed = true;
            let path = binding.path.display().to_string();
            return Err(ReloadFailure::Failed(match request.reason {
                ReloadReason::Request => format!("{name}: the file no longer exists"),
                _ => format!("File {path} no longer exists"),
            }));
        };
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
        let binding = self.binding_mut(buffer);
        binding.identity = Some(identity);
        binding.tail_hash = loaded.tail_hash;
        binding.external = ExternalState::Clean;
        binding.previous_text = Some(request.previous.clone());
        binding.changed_since_viewed = true;
        binding.content_predates_disk = false;
        self.journal_outbox.reloaded(buffer, identity, text);
        if loaded.needs_index {
            self.effects.push(Effect::StartIndex { buffer });
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
        for client in clients {
            self.set_echo(client, message.clone());
        }
    }

    /// A key from `client` means the user is looking at its selected
    /// buffer: its "changed externally" marker is cleared.
    pub(super) fn buffer_viewed(&mut self, client: ClientId) {
        let buffer = self.selected_buffer(client);
        if let Some(binding) = self.buffers[buffer].file.as_mut() {
            binding.changed_since_viewed = false;
        }
    }

    /// Buffers with reload work in flight, which `kill_buffer` forgets.
    pub(super) fn forget_reload_state(&mut self, buffer: BufferId) {
        self.observing.remove(&buffer);
        self.settle_timers.remove(&buffer);
        self.reload_recheck.remove(&buffer);
        self.reloading.remove(&buffer);
        self.appending.remove(&buffer);
        self.revert_pending.remove(&buffer);
    }
}
