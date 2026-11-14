//! Visiting, saving, auto-saving and killing buffers, and the minibuffer
//! questions they ask, as a state machine: each question is a `Purpose`
//! that carries everything needed to go on when it is answered, and file
//! I/O happens in effects whose results come back as events.

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use emx_proto::ServerMsg;
use emx_text::{BlockCache, FileIdentity, LineIndex, Original, PieceSource, Sources, Text};
use emx_types::Key;

use super::{Editor, Session, TimerPurpose};
use crate::autosave;
use crate::buffer::{BufferId, CodingMnemonic, EditError, FileBinding};
use crate::command::CommandError;
use crate::event::{AutoSaveJob, ClientId, Effect};
use crate::inplace::Move;
use crate::io::{self, Loaded, SaveFailure, SavePlan, abbreviate, expand_file_name, strerror};
use crate::marker::InsertionType;
use crate::minibuffer::{
    Answer, BACKSPACE, ESCAPE, HistoryKind, KeyChoice, Minibuffer, Mode, Outcome, Overlay, RETURN,
    control, plain,
};
use crate::window::WindowId;

/// Emacs `minibuffer-message-timeout`, and the `sit-for`/`sleep-for` of
/// the "Please answer" messages: how long a message over the minibuffer
/// stays before the prompt shows again.
pub(super) const MINIBUFFER_MESSAGE_MS: u64 = 2000;
const MILLISECONDS_PER_SECOND: u64 = 1000;
const SCRATCH_BUFFER: &str = "*scratch*";
const YES_OR_NO: &str = "(yes or no) ";
const Y_OR_N: &str = "(y or n) ";
const KILL_YES: &str = "yes";
const KILL_NO: &str = "no";
const KILL_SAVE: &str = "save and then kill";
const SAVE_SOME_KEYS: &str = "(y, n, !, ., q, C-f or C-h) ";
const SAVE_SOME_INVALID: &str = "Type C-h for help.";
const SAVE_SOME_HELP: &str = "y or SPC: save this file, n or DEL: skip it, !: save all the rest, \
.: save this one and stop, q or RET: stop, C-f: show this buffer and stop";
/// `file-size-human-readable` divides by this per prefix.
const SIZE_POWER: f64 = 1024.0;
const SIZE_PREFIXES: [&str; 11] = ["", "k", "M", "G", "T", "P", "E", "Z", "Y", "R", "Q"];
/// One digit after the point when the number is below this...
const ONE_DECIMAL_BELOW: f64 = 10.0;
/// ...and its fraction is between these, as `ls -lh` prints.
const FRACTION_SHOWN_FROM: f64 = 0.05;
const FRACTION_SHOWN_BELOW: f64 = 0.95;
/// Uniquified buffer names count from this, as `foo<2>`.
const FIRST_DUPLICATE: u64 = 2;

/// What to do with the minibuffer's answer.
#[derive(Debug)]
pub(crate) enum Purpose {
    FindFile,
    WriteFile {
        buffer: BufferId,
    },
    /// `save-buffer` in a buffer with no file: the file to save in.
    SaveAs {
        buffer: BufferId,
    },
    ConfirmOverwrite {
        buffer: BufferId,
        path: PathBuf,
        write_file: bool,
    },
    SaveCheck {
        flow: SaveFlow,
        question: Question,
    },
    KillBufferName,
    KillModified {
        buffer: BufferId,
    },
    SaveSome {
        session: SaveSession,
        buffer: BufferId,
    },
    ConfirmExit,
    Recover {
        buffer: BufferId,
        file: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Question {
    Changed,
    WriteProtected,
    CreateDirectory,
    LargeSave,
    InPlaceRisk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
    Rename,
    InPlace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AfterSave {
    Nothing,
    Kill,
}

/// A save being prepared: the checks already confirmed by the user and
/// the save-some session to go back to.
#[derive(Debug)]
pub(crate) struct SaveFlow {
    buffer: BufferId,
    /// No "Saving"/"Wrote" messages (`save-some-buffers`).
    quiet: bool,
    after: AfterSave,
    changed_confirmed: bool,
    write_protected_confirmed: bool,
    method: Option<Method>,
    session: Option<SaveSession>,
}

impl SaveFlow {
    fn new(buffer: BufferId, quiet: bool, after: AfterSave, session: Option<SaveSession>) -> Self {
        SaveFlow {
            buffer,
            quiet,
            after,
            changed_confirmed: false,
            write_protected_confirmed: false,
            method: None,
            session,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionEnd {
    /// `save-some-buffers`.
    Report,
    /// `save-buffers-kill-terminal`.
    Exit,
}

/// `save-some-buffers` asking about each modified file buffer in turn.
#[derive(Debug)]
pub(crate) struct SaveSession {
    queue: VecDeque<BufferId>,
    save_all: bool,
    offered: bool,
    end: SessionEnd,
}

/// A save in flight, who asked for it, and what it wrote.
#[derive(Debug)]
pub(super) struct SaveRequest {
    client: ClientId,
    quiet: bool,
    after: AfterSave,
    /// The text written and the buffer's change count when it was taken:
    /// only if the buffer is still at that count is it unmodified after.
    text: Text,
    tick: u64,
    /// An in-place save of the inode the buffer's original is read from.
    rewrites_original: bool,
}

/// `save-buffers-kill-terminal` waiting for its saves before exiting.
#[derive(Debug, Default)]
pub(super) struct ExitWait {
    in_flight: BTreeSet<BufferId>,
    /// Still asking about buffers.
    prompting: bool,
    /// A save failed: the exit is abandoned and the error stays shown.
    failed: bool,
}

#[derive(Debug)]
pub(super) struct AutoSaveRequest {
    client: Option<ClientId>,
    previous_mark: u64,
}

/// Emacs `file-size-human-readable` with its default flavor.
fn human_size(bytes: u64) -> String {
    let mut size = bytes as f64;
    let mut prefix = 0;
    while size >= SIZE_POWER && prefix + 1 < SIZE_PREFIXES.len() {
        size /= SIZE_POWER;
        prefix += 1;
    }
    let fraction = size % 1.0;
    if size < ONE_DECIMAL_BELOW && (FRACTION_SHOWN_FROM..FRACTION_SHOWN_BELOW).contains(&fraction) {
        format!("{size:.1}{}", SIZE_PREFIXES[prefix])
    } else {
        format!("{size:.0}{}", SIZE_PREFIXES[prefix])
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Emacs `file-name-directory`: the directory with its trailing slash.
fn directory_name(path: &Path) -> String {
    let directory = io::parent_directory(path).display().to_string();
    if directory.ends_with('/') {
        directory
    } else {
        format!("{directory}/")
    }
}

fn user_error(message: impl Into<String>) -> CommandError {
    CommandError::User(message.into())
}

fn choice(key: Key, answer: char) -> KeyChoice {
    KeyChoice { key, answer }
}

impl Editor {
    /// The directory relative file names in prompts start from for buffers
    /// that visit no file (Emacs `default-directory`).
    pub fn set_default_directory(&mut self, directory: &Path) {
        self.default_directory = directory.to_path_buf();
    }

    /// Every buffer's name, sorted.
    pub fn buffer_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .buffers
            .values()
            .map(|buffer| buffer.name.clone())
            .collect();
        names.sort();
        names
    }

    /// A buffer's `default-directory`: its file's directory, or the
    /// editor's for buffers without a file.
    fn directory_of(&self, buffer: BufferId) -> PathBuf {
        match &self.buffers[buffer].file {
            Some(file) => io::parent_directory(&file.path).to_path_buf(),
            None => self.default_directory.clone(),
        }
    }

    /// The default directory as file prompts start with it: abbreviated,
    /// with a trailing slash.
    fn directory_input(&self, buffer: BufferId) -> String {
        let mut text = abbreviate(&self.directory_of(buffer));
        if !text.ends_with('/') {
            text.push('/');
        }
        text
    }

    // ---- The minibuffer -------------------------------------------------

    fn read_from_minibuffer(
        &mut self,
        client: ClientId,
        prompt: String,
        initial: String,
        mode: Mode,
        purpose: Purpose,
    ) {
        self.client_mut(client).minibuffer = Some(Minibuffer::new(prompt, initial, mode, purpose));
    }

    fn read_file_name(&mut self, client: ClientId, prompt: &str, purpose: Purpose) {
        let buffer = self.selected_buffer(client);
        let initial = self.directory_input(buffer);
        let defaults = self.buffers[buffer]
            .file
            .as_ref()
            .map(|file| abbreviate(&file.path))
            .into_iter()
            .collect();
        let mode = Mode::Line {
            history: HistoryKind::FileName,
            defaults,
            require: None,
            allow_empty: true,
        };
        self.read_from_minibuffer(client, prompt.to_string(), initial, mode, purpose);
    }

    /// Emacs `yes-or-no-p`.
    fn yes_or_no_p(&mut self, client: ClientId, question: String, purpose: Purpose) {
        self.read_from_minibuffer(
            client,
            format!("{question}{YES_OR_NO}"),
            String::new(),
            Mode::YesOrNo,
            purpose,
        );
    }

    /// Emacs `y-or-n-p`, which pads the question with a space.
    fn y_or_n_p(&mut self, client: ClientId, question: String, purpose: Purpose) {
        let separator = if question.is_empty() || question.ends_with(' ') {
            ""
        } else {
            " "
        };
        self.read_from_minibuffer(
            client,
            format!("{question}{separator}{Y_OR_N}"),
            String::new(),
            Mode::YOrN,
            purpose,
        );
    }

    /// What the echo area shows, and where the cursor is in it while the
    /// minibuffer is active.
    pub(crate) fn echo_area(&self, client: ClientId) -> (String, Option<usize>) {
        let client = self.client(client);
        match &client.minibuffer {
            Some(minibuffer) => {
                let (line, cursor) = minibuffer.display();
                (line, Some(cursor))
            }
            None => (client.echo.clone(), None),
        }
    }

    pub(crate) fn minibuffer_active(&self, client: ClientId) -> bool {
        self.client(client).minibuffer.is_some()
    }

    pub(super) fn minibuffer_key(&mut self, client: ClientId, key: Key) {
        let mut minibuffer = self
            .client_mut(client)
            .minibuffer
            .take()
            .expect("keys reach the minibuffer only while it is active");
        match minibuffer.handle_key(key, &mut self.histories, &mut self.kill_ring) {
            Outcome::Continue => self.restore_minibuffer(client, minibuffer),
            Outcome::Ding => {
                self.ring_bell(client);
                self.restore_minibuffer(client, minibuffer);
            }
            Outcome::Quit => {
                self.client_mut(client).exit_wait = None;
                self.signal(client, None, &CommandError::Quit);
            }
            Outcome::Exit(answer) => {
                if let (Mode::YOrN, Answer::Yes(yes)) = (&minibuffer.mode, &answer) {
                    let letter = if *yes { 'y' } else { 'n' };
                    self.set_echo(client, format!("{}{letter}", minibuffer.prompt));
                }
                self.answer(client, minibuffer.purpose, answer);
            }
        }
    }

    /// Bracketed paste into the minibuffer inserts the text as typed.
    pub(super) fn minibuffer_paste(&mut self, client: ClientId, bytes: &[u8]) {
        let reads_a_line = self
            .client(client)
            .minibuffer
            .as_ref()
            .is_some_and(|minibuffer| matches!(minibuffer.mode, Mode::Line { .. } | Mode::YesOrNo));
        // Pasted characters must not answer single-key questions.
        if !reads_a_line {
            self.ring_bell(client);
            return;
        }
        let text = String::from_utf8_lossy(bytes).into_owned();
        for character in text.chars() {
            self.minibuffer_key(client, plain(character));
            if !self.minibuffer_active(client) {
                return;
            }
        }
    }

    fn restore_minibuffer(&mut self, client: ClientId, minibuffer: Minibuffer) {
        if minibuffer.overlay.is_some() {
            self.set_timer(
                TimerPurpose::MinibufferMessage(client, minibuffer.overlay_serial),
                MINIBUFFER_MESSAGE_MS,
            );
        }
        self.client_mut(client).minibuffer = Some(minibuffer);
    }

    pub(super) fn minibuffer_message_expired(&mut self, client: ClientId, serial: u64) {
        let Some(client) = self.clients.get_mut(&client) else {
            return;
        };
        if let Some(minibuffer) = client.minibuffer.as_mut()
            && minibuffer.overlay_serial == serial
        {
            minibuffer.overlay = None;
        }
    }

    /// Shows an error from a step that is not a command: Emacs's
    /// `command-error` display.
    fn fail(&mut self, client: ClientId, error: &CommandError) {
        if self.clients.contains_key(&client) {
            self.client_mut(client).exit_wait = None;
            self.signal(client, None, error);
        }
    }

    fn message_to(&mut self, client: ClientId, text: String) {
        if self.clients.contains_key(&client) {
            self.set_echo(client, text);
        }
    }

    fn answer(&mut self, client: ClientId, purpose: Purpose, answer: Answer) {
        let result = match (purpose, answer) {
            (Purpose::FindFile, Answer::Line(input)) => {
                let directory = self.directory_of(self.selected_buffer(client));
                let path = expand_file_name(&input, &directory);
                self.visit_file(client, path);
                Ok(())
            }
            (Purpose::WriteFile { buffer }, Answer::Line(input)) => {
                self.write_file_named(client, buffer, &input)
            }
            (Purpose::SaveAs { buffer }, Answer::Line(input)) => {
                self.save_as_named(client, buffer, &input)
            }
            (
                Purpose::ConfirmOverwrite {
                    buffer,
                    path,
                    write_file,
                },
                Answer::Yes(yes),
            ) => {
                if yes {
                    self.check_savable(buffer).and_then(|()| {
                        self.visit_new_name(buffer, path, write_file);
                        let mut flow = SaveFlow::new(buffer, false, AfterSave::Nothing, None);
                        flow.changed_confirmed = true;
                        self.continue_save(client, flow)
                    })
                } else {
                    Err(user_error("Canceled"))
                }
            }
            (Purpose::SaveCheck { flow, question }, answer) => {
                self.answer_save_check(client, flow, question, answer)
            }
            (Purpose::KillBufferName, Answer::Line(name)) => {
                let buffer = if name.is_empty() {
                    Some(self.selected_buffer(client))
                } else {
                    self.buffers
                        .iter()
                        .find(|(_, buffer)| buffer.name == name)
                        .map(|(id, _)| id)
                };
                match buffer {
                    Some(buffer) => self.kill_buffer_asking(client, buffer),
                    None => Err(user_error(format!("No such buffer {name}"))),
                }
            }
            (Purpose::KillModified { buffer }, Answer::Line(choice)) => {
                if !self.buffers.contains_key(buffer) {
                    Ok(())
                } else if choice == KILL_YES {
                    self.kill_buffer(buffer);
                    Ok(())
                } else if choice == KILL_SAVE {
                    self.continue_save(client, SaveFlow::new(buffer, false, AfterSave::Kill, None))
                } else {
                    Ok(())
                }
            }
            (Purpose::SaveSome { session, buffer }, Answer::Key(key)) => {
                self.answer_save_some(client, session, buffer, key)
            }
            (Purpose::ConfirmExit, Answer::Yes(yes)) => {
                if yes {
                    self.exit_now(client);
                }
                Ok(())
            }
            (Purpose::Recover { buffer, file }, Answer::Yes(yes)) => {
                if yes {
                    self.pending_reads.insert(buffer, client);
                    self.effects.push(Effect::ReadAutoSave { buffer, file });
                    Ok(())
                } else {
                    Err(user_error("Recover-file canceled"))
                }
            }
            (purpose, answer) => {
                unreachable!("{purpose:?} is asked in a mode that answers {answer:?}")
            }
        };
        if let Err(error) = result {
            self.fail(client, &error);
        }
    }

    // ---- Visiting -------------------------------------------------------

    pub(crate) fn find_file_command(&mut self, client: ClientId) {
        self.read_file_name(client, "Find file: ", Purpose::FindFile);
    }

    /// Shows the buffer visiting `path`, creating it in the loading state
    /// and asking for the file to be read when there is none.
    fn visit_file(&mut self, client: ClientId, path: PathBuf) {
        let existing = self
            .buffers
            .iter()
            .find(|(_, buffer)| buffer.file.as_ref().is_some_and(|file| file.path == path))
            .map(|(id, _)| id);
        if let Some(buffer) = existing {
            self.show_in_selected_window(client, buffer);
            return;
        }
        let name = self.unique_buffer_name(&file_name(&path), &path, None);
        let buffer = self.create_buffer(&name);
        let created = &mut self.buffers[buffer];
        created.loading = true;
        created.file = Some(FileBinding {
            path: path.clone(),
            identity: None,
            saving: false,
        });
        self.show_in_selected_window(client, buffer);
        self.pending_loads.insert(buffer, client);
        self.effects.push(Effect::Load { buffer, path });
    }

    /// Emacs names buffers after their file; a name in use gets the
    /// directory name added, then a number.
    fn unique_buffer_name(&self, name: &str, path: &Path, excluding: Option<BufferId>) -> String {
        let taken = |candidate: &str| {
            self.buffers
                .iter()
                .any(|(id, buffer)| Some(id) != excluding && buffer.name == candidate)
        };
        if !taken(name) {
            return name.to_string();
        }
        if let Some(parent) = path.parent().and_then(Path::file_name) {
            let candidate = format!("{name}<{}>", parent.to_string_lossy());
            if !taken(&candidate) {
                return candidate;
            }
        }
        (FIRST_DUPLICATE..)
            .map(|number| format!("{name}<{number}>"))
            .find(|candidate| !taken(candidate))
            .expect("some number is free")
    }

    pub(super) fn file_loaded(&mut self, buffer: BufferId, result: Result<Loaded, String>) {
        let client = self.pending_loads.remove(&buffer);
        if !self.buffers.contains_key(buffer) {
            return;
        }
        let loaded = match result {
            Ok(loaded) => loaded,
            Err(message) => {
                self.kill_buffer(buffer);
                if let Some(client) = client {
                    self.fail(client, &user_error(message));
                }
                return;
            }
        };
        let target = &mut self.buffers[buffer];
        let name = target
            .file
            .as_ref()
            .map(|file| file_name(&file.path))
            .expect("a loading buffer visits a file");
        target.text = Text::from_original(loaded.sources);
        target.loading = false;
        target.coding = loaded.coding;
        target.auto_saved_since_save = false;
        target
            .file
            .as_mut()
            .expect("a loading buffer visits a file")
            .identity = loaded.identity;
        let exists = loaded.identity.is_some();
        // Emacs `after-find-file`.
        target.read_only = !loaded.writable;
        let message = if loaded.writable {
            if loaded.auto_save_newer {
                Some(format!(
                    "{name} has auto save data; consider M-x recover-this-file"
                ))
            } else if !exists {
                Some("(New file)".to_string())
            } else {
                None
            }
        } else if exists {
            Some("Note: file is write protected".to_string())
        } else if loaded.directory_exists {
            Some("File not found and directory write-protected".to_string())
        } else {
            target.read_only = false;
            Some(
                "Use M-x make-directory RET RET to create the directory and its parents"
                    .to_string(),
            )
        };
        if loaded.needs_index {
            self.effects.push(Effect::StartIndex { buffer });
        }
        if let (Some(client), Some(message)) = (client, message) {
            self.message_to(client, message);
        }
    }

    fn show_in_selected_window(&mut self, client: ClientId, buffer: BufferId) {
        let window = self.selected_window_id(client);
        self.show_buffer(client, window, buffer);
    }

    /// Emacs `set-window-buffer`: point comes back where it was when the
    /// buffer was last shown.
    pub(super) fn show_buffer(&mut self, client: ClientId, window_id: WindowId, buffer: BufferId) {
        let window = *self.window(client, window_id);
        self.touch_buffer(buffer);
        if window.buffer == buffer {
            return;
        }
        let old = &mut self.buffers[window.buffer];
        let old_point = old.marker_pos(window.point);
        match old.last_point {
            Some(marker) => old.set_marker(marker, old_point),
            None => {
                old.last_point =
                    Some(old.create_marker_without_undo(old_point, InsertionType::StayBefore));
            }
        }
        old.delete_marker(window.point);
        old.delete_marker(window.start);
        let new = &mut self.buffers[buffer];
        let point = new.last_point.map_or(0, |marker| new.marker_pos(marker));
        let point_marker = new.create_marker_without_undo(point, InsertionType::StayBefore);
        let start_marker = new.create_marker_without_undo(0, InsertionType::StayBefore);
        let target = &mut self.client_mut(client).frame.windows[window_id];
        target.buffer = buffer;
        target.point = point_marker;
        target.start = start_marker;
        target.start_row_skip = 0;
        target.hscroll = 0;
    }

    /// Moves `buffer` to the front of the most-recently-shown order.
    fn touch_buffer(&mut self, buffer: BufferId) {
        self.recency.retain(|id| *id != buffer);
        self.recency.insert(0, buffer);
    }

    /// Emacs `other-buffer`: the most recently shown other buffer, or a new
    /// `*scratch*` when there is none.
    pub(super) fn other_buffer(&mut self, excluding: BufferId) -> BufferId {
        match self.recency.iter().copied().find(|id| *id != excluding) {
            Some(buffer) => buffer,
            None => {
                let name = self.unique_buffer_name(SCRATCH_BUFFER, Path::new(""), None);
                self.create_buffer(&name)
            }
        }
    }

    // ---- Killing --------------------------------------------------------

    pub(crate) fn kill_buffer_command(&mut self, client: ClientId) {
        let current = self.buffers[self.selected_buffer(client)].name.clone();
        let names = self
            .buffers
            .values()
            .map(|buffer| buffer.name.clone())
            .collect();
        let mode = Mode::Line {
            history: HistoryKind::BufferName,
            defaults: vec![current.clone()],
            require: Some(names),
            allow_empty: true,
        };
        self.read_from_minibuffer(
            client,
            format!("Kill buffer (default {current}): "),
            String::new(),
            mode,
            Purpose::KillBufferName,
        );
    }

    /// Emacs `kill-buffer` interactively: a modified file buffer asks
    /// first (`kill-buffer--possibly-save`).
    fn kill_buffer_asking(
        &mut self,
        client: ClientId,
        buffer: BufferId,
    ) -> Result<(), CommandError> {
        let target = &self.buffers[buffer];
        if target.file.as_ref().is_some_and(|file| file.saving) {
            return Err(EditError::Saving.into());
        }
        if target.file.is_some() && target.is_modified() {
            let mode = Mode::Line {
                history: HistoryKind::MultipleChoice,
                defaults: Vec::new(),
                require: Some(vec![
                    KILL_YES.to_string(),
                    KILL_NO.to_string(),
                    KILL_SAVE.to_string(),
                ]),
                allow_empty: false,
            };
            let prompt = format!(
                "Buffer {} modified; kill anyway? ({KILL_YES}/{KILL_NO}/{KILL_SAVE}) ",
                target.name
            );
            self.read_from_minibuffer(
                client,
                prompt,
                String::new(),
                mode,
                Purpose::KillModified { buffer },
            );
            return Ok(());
        }
        self.kill_buffer(buffer);
        Ok(())
    }

    /// Removes the buffer; windows showing it show another buffer.
    pub(super) fn kill_buffer(&mut self, buffer: BufferId) {
        let replacement = self.other_buffer(buffer);
        let showing: Vec<(ClientId, WindowId)> = self
            .clients
            .iter()
            .flat_map(|(client, state)| {
                state
                    .frame
                    .windows
                    .iter()
                    .filter(|(_, window)| window.buffer == buffer)
                    .map(|(window, _)| (*client, window))
            })
            .collect();
        for (client, window) in showing {
            self.show_buffer(client, window, replacement);
        }
        self.recency.retain(|id| *id != buffer);
        self.pending_loads.remove(&buffer);
        self.pending_reads.remove(&buffer);
        self.auto_saves.remove(&buffer);
        self.buffers.remove(buffer);
    }

    // ---- Saving ---------------------------------------------------------

    pub(crate) fn save_buffer_command(&mut self, client: ClientId) -> Result<(), CommandError> {
        let buffer = self.selected_buffer(client);
        self.check_savable(buffer)?;
        let target = &self.buffers[buffer];
        let Some(file) = &target.file else {
            self.read_file_name(client, "File to save in: ", Purpose::SaveAs { buffer });
            return Ok(());
        };
        // A file that does not exist is written even when unmodified.
        if !target.is_modified() && file.path.exists() {
            self.set_echo(client, "(No changes need to be saved)".to_string());
            return Ok(());
        }
        self.continue_save(
            client,
            SaveFlow::new(buffer, false, AfterSave::Nothing, None),
        )
    }

    pub(crate) fn write_file_command(&mut self, client: ClientId) -> Result<(), CommandError> {
        let buffer = self.selected_buffer(client);
        self.check_savable(buffer)?;
        self.read_file_name(client, "Write file: ", Purpose::WriteFile { buffer });
        Ok(())
    }

    /// A buffer being read, being saved, or damaged has no text to save
    /// (and a save in flight must finish before another starts).
    fn check_savable(&self, buffer: BufferId) -> Result<(), CommandError> {
        let Some(target) = self.buffers.get(buffer) else {
            return Err(user_error("Selecting deleted buffer"));
        };
        if target.loading {
            return Err(CommandError::BufferReadOnly(target.name.clone()));
        }
        if target.file.as_ref().is_some_and(|file| file.saving)
            || self.pending_saves.contains_key(&buffer)
        {
            return Err(EditError::Saving.into());
        }
        if target.damaged {
            return Err(user_error(format!(
                "Buffer {} is damaged: its file was left partly rewritten; nothing to save",
                target.name
            )));
        }
        Ok(())
    }

    /// `write-file`: a directory gets the buffer's file name appended, and
    /// an existing file needs confirmation.
    fn write_file_named(
        &mut self,
        client: ClientId,
        buffer: BufferId,
        input: &str,
    ) -> Result<(), CommandError> {
        let directory = self.directory_of(buffer);
        let mut path = expand_file_name(input, &directory);
        if input.ends_with('/') || path.is_dir() {
            let name = match &self.buffers[buffer].file {
                Some(file) => file_name(&file.path),
                None => self.buffers[buffer].name.clone(),
            };
            path = path.join(name);
        }
        self.name_then_save(client, buffer, path, true)
    }

    fn save_as_named(
        &mut self,
        client: ClientId,
        buffer: BufferId,
        input: &str,
    ) -> Result<(), CommandError> {
        let directory = self.directory_of(buffer);
        let path = expand_file_name(input, &directory);
        if path.is_dir() {
            return Err(user_error(format!("{} is a directory", path.display())));
        }
        self.name_then_save(client, buffer, path, false)
    }

    fn name_then_save(
        &mut self,
        client: ClientId,
        buffer: BufferId,
        path: PathBuf,
        write_file: bool,
    ) -> Result<(), CommandError> {
        self.check_savable(buffer)?;
        if path.exists() {
            let question = format!("File \u{2018}{}\u{2019} exists; overwrite?", path.display());
            self.y_or_n_p(
                client,
                question,
                Purpose::ConfirmOverwrite {
                    buffer,
                    path,
                    write_file,
                },
            );
            return Ok(());
        }
        self.visit_new_name(buffer, path, write_file);
        self.continue_save(
            client,
            SaveFlow::new(buffer, false, AfterSave::Nothing, None),
        )
    }

    /// Emacs `set-visited-file-name`; `write-file` also marks the buffer
    /// modified so it is written.
    fn visit_new_name(&mut self, buffer: BufferId, path: PathBuf, write_file: bool) {
        let name = self.unique_buffer_name(&file_name(&path), &path, Some(buffer));
        let target = &mut self.buffers[buffer];
        target.name = name;
        target.file = Some(FileBinding {
            path,
            identity: None,
            saving: false,
        });
        target.auto_saved_since_save = false;
        if write_file {
            target.mark_modified();
        }
    }

    /// Runs the checks `basic-save-buffer` makes, asking where Emacs asks,
    /// then plans the save and starts it.
    fn continue_save(&mut self, client: ClientId, mut flow: SaveFlow) -> Result<(), CommandError> {
        let buffer = flow.buffer;
        // A question may have been pending while another client saved, or
        // the buffer was killed.
        if !self.buffers.contains_key(buffer) {
            return self.resume_session(client, flow.session);
        }
        self.check_savable(buffer)?;
        let Some(binding) = self
            .buffers
            .get(buffer)
            .and_then(|target| target.file.clone())
        else {
            return self.resume_session(client, flow.session);
        };
        let name = file_name(&binding.path);
        let info = io::probe_target(&binding.path).map_err(|error| {
            user_error(format!(
                "Opening output file: {}, {}",
                strerror(&error),
                binding.path.display()
            ))
        })?;
        if !flow.changed_confirmed
            && let Some(existing) = &info.existing
            && Some(existing.identity) != binding.identity
        {
            let question = format!("{name} has changed since visited or saved.  Save anyway? ");
            self.yes_or_no_p(
                client,
                question,
                Purpose::SaveCheck {
                    flow,
                    question: Question::Changed,
                },
            );
            return Ok(());
        }
        let directory = directory_name(&binding.path);
        if !info.directory_exists {
            let question = format!("Directory \u{2018}{directory}\u{2019} does not exist; create?");
            self.y_or_n_p(
                client,
                question,
                Purpose::SaveCheck {
                    flow,
                    question: Question::CreateDirectory,
                },
            );
            return Ok(());
        }
        match &info.existing {
            Some(existing) if !existing.writable && !flow.write_protected_confirmed => {
                let question = format!("File {name} is write-protected; try to save anyway? ");
                self.yes_or_no_p(
                    client,
                    question,
                    Purpose::SaveCheck {
                        flow,
                        question: Question::WriteProtected,
                    },
                );
                return Ok(());
            }
            None if !info.directory_writable => {
                return Err(user_error(format!("Directory {directory} write-protected")));
            }
            _ => {}
        }
        let text = self.buffers[buffer].text.clone();
        let euid = rustix::process::geteuid().as_raw();
        let planned = match flow.method {
            Some(Method::InPlace) => io::in_place_plan(&binding, &text, &info),
            Some(Method::Rename) => io::rename_plan(&binding, &text, &info),
            None => io::plan_save(&binding, &text, &info, euid),
        };
        let plan = planned.map_err(|error| user_error(error.to_string()))?;
        if let SavePlan::InPlace { moves, .. } = &plan {
            self.refuse_in_place_over_aliases(buffer, &name, &info)?;
            let shifts = moves.iter().any(|step| matches!(step, Move::Shift { .. }));
            if flow.method.is_none()
                && shifts
                && let Some(reason) = io::in_place_reason(&info, euid)
            {
                self.ask_in_place_risk(client, flow, &name, reason, info.directory_writable);
                return Ok(());
            }
        }
        let plan = match flow.method {
            Some(_) => plan,
            None => {
                if matches!(plan, SavePlan::Rename { .. })
                    && text.len() > self.config.large_save_warn
                {
                    let size = human_size(text.len());
                    let prompt = format!(
                        "Saving {name} rewrites {size} and needs {size} free. \
                         (r)ewrite safely, (i)n place, (c)ancel? "
                    );
                    let mode = Mode::Keys {
                        choices: vec![
                            choice(plain('r'), 'r'),
                            choice(plain('i'), 'i'),
                            choice(plain('c'), 'c'),
                        ],
                        invalid: None,
                        help: None,
                    };
                    self.read_from_minibuffer(
                        client,
                        prompt,
                        String::new(),
                        mode,
                        Purpose::SaveCheck {
                            flow,
                            question: Question::LargeSave,
                        },
                    );
                    return Ok(());
                }
                plan
            }
        };
        let rewrites_original = matches!(plan, SavePlan::InPlace { .. })
            && original_inode_is(&text, info.existing.map(|file| file.identity));
        let session = flow.session.take();
        self.start_save_effect(
            client,
            &flow,
            binding.path,
            plan,
            rewrites_original,
            session.is_some(),
        )?;
        self.resume_session(client, session)
    }

    /// An in-place save rewrites the inode; any other buffer reading its
    /// unchanged text from that inode (a hardlink or symlink alias) would
    /// be corrupted.
    fn refuse_in_place_over_aliases(
        &self,
        buffer: BufferId,
        name: &str,
        info: &io::TargetInfo,
    ) -> Result<(), CommandError> {
        let target = info.existing.map(|file| file.identity);
        let alias = self
            .buffers
            .iter()
            .find(|(id, other)| *id != buffer && original_inode_is(&other.text, target));
        match alias {
            Some((_, other)) => Err(user_error(format!(
                "{name} is also visited as buffer {}; saving it in place would corrupt that buffer",
                other.name
            ))),
            None => Ok(()),
        }
    }

    /// Automatic in-place saves that move the buffer's own file bytes are
    /// not crash-safe: a failure loses the file and the buffer. Ask first.
    fn ask_in_place_risk(
        &mut self,
        client: ClientId,
        flow: SaveFlow,
        name: &str,
        reason: io::InPlaceReason,
        rename_possible: bool,
    ) {
        let why = match reason {
            io::InPlaceReason::Hardlinked => "is hardlinked",
            io::InPlaceReason::OtherOwner => "is owned by another user",
            io::InPlaceReason::UnwritableDirectory => "is in an unwritable directory",
        };
        let mut choices = vec![choice(plain('i'), 'i'), choice(plain('c'), 'c')];
        let options = if rename_possible {
            choices.push(choice(plain('r'), 'r'));
            "(i)n place, (r)ewrite (breaks links/ownership), (c)ancel? "
        } else {
            "(i)n place, (c)ancel? "
        };
        let prompt = format!(
            "{name} {why}; saving in place is not crash-safe and a failure loses the file \
             and this buffer. {options}"
        );
        let mode = Mode::Keys {
            choices,
            invalid: None,
            help: None,
        };
        self.read_from_minibuffer(
            client,
            prompt,
            String::new(),
            mode,
            Purpose::SaveCheck {
                flow,
                question: Question::InPlaceRisk,
            },
        );
    }

    fn answer_save_check(
        &mut self,
        client: ClientId,
        mut flow: SaveFlow,
        question: Question,
        answer: Answer,
    ) -> Result<(), CommandError> {
        match (question, answer) {
            (Question::Changed, Answer::Yes(true)) => flow.changed_confirmed = true,
            (Question::Changed, Answer::Yes(false)) => {
                return Err(user_error("Save not confirmed"));
            }
            (Question::WriteProtected, Answer::Yes(true)) => {
                flow.write_protected_confirmed = true;
            }
            (Question::WriteProtected, Answer::Yes(false)) => {
                return Err(user_error(
                    "Attempt to save to a file that you aren\u{2019}t allowed to write",
                ));
            }
            (Question::CreateDirectory, Answer::Yes(true)) => {
                let Some(path) = self
                    .buffers
                    .get(flow.buffer)
                    .and_then(|target| target.file.as_ref())
                    .map(|file| file.path.clone())
                else {
                    return Ok(());
                };
                let directory = io::parent_directory(&path);
                std::fs::create_dir_all(directory).map_err(|error| {
                    user_error(format!(
                        "Creating directory: {}, {}",
                        strerror(&error),
                        directory.display()
                    ))
                })?;
            }
            (Question::CreateDirectory, Answer::Yes(false)) => {
                return Err(user_error("Canceled"));
            }
            (Question::LargeSave, Answer::Key('r')) => flow.method = Some(Method::Rename),
            (Question::LargeSave, Answer::Key('i')) => flow.method = Some(Method::InPlace),
            (Question::LargeSave, Answer::Key(_)) => return Err(user_error("Canceled")),
            (Question::InPlaceRisk, Answer::Key('i')) => flow.method = Some(Method::InPlace),
            (Question::InPlaceRisk, Answer::Key('r')) => flow.method = Some(Method::Rename),
            (Question::InPlaceRisk, Answer::Key(_)) => return Err(user_error("Canceled")),
            (question, answer) => unreachable!("{question:?} cannot be answered {answer:?}"),
        }
        self.continue_save(client, flow)
    }

    /// Makes the buffer read-only and asks for the plan to be carried out.
    fn start_save_effect(
        &mut self,
        client: ClientId,
        flow: &SaveFlow,
        path: PathBuf,
        plan: SavePlan,
        rewrites_original: bool,
        in_session: bool,
    ) -> Result<(), CommandError> {
        let buffer = flow.buffer;
        self.check_savable(buffer)?;
        let target = &mut self.buffers[buffer];
        target
            .file
            .as_mut()
            .expect("a saved buffer visits a file")
            .saving = true;
        let request = SaveRequest {
            client,
            quiet: flow.quiet,
            after: flow.after,
            text: plan.text().clone(),
            tick: target.modified_tick(),
            rewrites_original,
        };
        self.pending_saves.insert(buffer, request);
        if in_session && let Some(wait) = self.client_mut(client).exit_wait.as_mut() {
            wait.in_flight.insert(buffer);
        }
        if !flow.quiet {
            self.set_echo(client, format!("Saving file {}...", path.display()));
        }
        self.effects.push(Effect::Save { buffer, plan });
        Ok(())
    }

    /// A worker's save result. Results are external input: one for a save
    /// the editor is not waiting on is ignored.
    pub(super) fn save_finished(
        &mut self,
        buffer: BufferId,
        result: Result<FileIdentity, SaveFailure>,
    ) {
        let Some(request) = self.pending_saves.remove(&buffer) else {
            return;
        };
        let client = request.client;
        if let Some(binding) = self
            .buffers
            .get_mut(buffer)
            .and_then(|target| target.file.as_mut())
        {
            binding.saving = false;
            let path = binding.path.clone();
            match &result {
                Ok(identity) => {
                    self.adopt_saved_file(buffer, *identity, &request);
                    if !request.quiet {
                        self.message_to(client, format!("Wrote {}", path.display()));
                    }
                    if request.after == AfterSave::Kill && !self.buffers[buffer].is_modified() {
                        self.kill_buffer(buffer);
                    }
                }
                Err(failure) => {
                    let mut message = failure.message.clone();
                    if failure.file_damaged && request.rewrites_original {
                        self.mark_damaged(buffer);
                        message.push_str(&format!(
                            "; buffer {} read that file and is damaged",
                            self.buffers[buffer].name
                        ));
                    }
                    // Not `fail`: a waiting exit must learn of the failure.
                    if self.clients.contains_key(&client) {
                        self.signal(client, None, &user_error(message.clone()));
                    }
                    self.abandon_exits(&message);
                }
            }
        }
        let waiting: Vec<ClientId> = self
            .clients
            .iter_mut()
            .filter_map(|(id, state)| {
                let wait = state.exit_wait.as_mut()?;
                wait.in_flight.remove(&buffer);
                Some(*id)
            })
            .collect();
        for client in waiting {
            self.check_exit(client);
        }
        self.maybe_arm_grace();
    }

    /// A failed save abandons every pending exit; the exiting clients see
    /// why.
    fn abandon_exits(&mut self, message: &str) {
        let exiting: Vec<ClientId> = self
            .clients
            .iter()
            .filter(|(_, state)| state.exit_wait.is_some())
            .map(|(id, _)| *id)
            .collect();
        for client in exiting {
            let state = self.client_mut(client);
            if let Some(wait) = state.exit_wait.as_mut() {
                wait.failed = true;
            }
            if state.minibuffer.is_none() {
                state.echo = message.to_string();
            }
        }
    }

    /// The buffer's unchanged text came from a file an in-place save left
    /// partly rewritten: nothing it shows can be trusted, so it says so and
    /// can be neither edited nor saved.
    fn mark_damaged(&mut self, buffer: BufferId) {
        let target = &mut self.buffers[buffer];
        if let Original::File(cache) = &target.text.sources().original {
            cache.invalidate_from(0);
        }
        target.damaged = true;
    }

    /// After a save the buffer remembers the file version it wrote. If the
    /// buffer did not change while the save ran it is unmodified and reads
    /// from the new file: pieces never refer to bytes an in-place save
    /// changed, nor keep a replaced file alive. If it changed, it stays
    /// modified with its own text; text an in-place save pulled from under
    /// it is marked damaged.
    fn adopt_saved_file(
        &mut self,
        buffer: BufferId,
        identity: FileIdentity,
        request: &SaveRequest,
    ) {
        let capacity = usize::try_from(self.config.block_cache_bytes).unwrap_or(usize::MAX);
        let target = &mut self.buffers[buffer];
        let binding = target.file.as_mut().expect("a saved buffer visits a file");
        binding.identity = Some(identity);
        let path = binding.path.clone();
        if target.modified_tick() != request.tick {
            if request.rewrites_original {
                self.mark_damaged(buffer);
            }
            return;
        }
        target.set_saved_at(request.tick);
        let text = request.text.clone();
        if target.coding == CodingMnemonic::Undecided && !inserted_text_is_ascii(&text) {
            target.coding = CodingMnemonic::Utf8;
        }
        let replacement = match &text.sources().original {
            Original::Memory(_) => {
                let mut bytes = Vec::new();
                text.read(0..text.len(), &mut bytes)
                    .expect("a text with a memory original reads without I/O");
                Some((Sources::memory(bytes.into()), false))
            }
            Original::File(cache) => {
                let opened = cache.identity();
                if opened.dev == identity.dev && opened.ino == identity.ino {
                    cache.rewritten(identity);
                    Some((
                        Sources::file(cache.clone(), LineIndex::new(identity.size)),
                        true,
                    ))
                } else {
                    match BlockCache::open(&path, capacity) {
                        Ok(reopened) if reopened.identity() == identity => {
                            let index = LineIndex::new(reopened.len());
                            Some((Sources::file(reopened, index), true))
                        }
                        // Something replaced the file again already; the
                        // old pieces still read the inode the rename left
                        // intact, which holds what was saved.
                        _ => None,
                    }
                }
            }
        };
        let target = &mut self.buffers[buffer];
        if let Some((sources, needs_index)) = replacement {
            target.text = Text::from_original(sources);
            if needs_index {
                self.effects.push(Effect::StartIndex { buffer });
            }
        }
        let target = &mut self.buffers[buffer];
        if target.auto_saved_since_save {
            target.auto_saved_since_save = false;
            self.effects.push(Effect::RemoveAutoSave { file: path });
        }
    }

    // ---- save-some-buffers and exiting -----------------------------------

    pub(crate) fn save_some_buffers_command(
        &mut self,
        client: ClientId,
    ) -> Result<(), CommandError> {
        self.start_session(client, SessionEnd::Report)
    }

    /// `save-buffers-kill-terminal` without a server: offer to save each
    /// modified file buffer, then exit, asking again if any stay modified.
    pub(crate) fn save_buffers_kill_terminal_command(
        &mut self,
        client: ClientId,
    ) -> Result<(), CommandError> {
        self.client_mut(client).exit_wait = Some(ExitWait {
            prompting: true,
            ..ExitWait::default()
        });
        self.start_session(client, SessionEnd::Exit)
    }

    fn start_session(&mut self, client: ClientId, end: SessionEnd) -> Result<(), CommandError> {
        let current = self.selected_buffer(client);
        let queue = std::iter::once(current)
            .chain(self.recency.iter().copied().filter(|id| *id != current))
            .collect();
        let session = SaveSession {
            queue,
            save_all: false,
            offered: false,
            end,
        };
        self.resume_session(client, Some(session))
    }

    fn needs_saving(&self, buffer: BufferId) -> bool {
        self.buffers.get(buffer).is_some_and(|target| {
            target.is_modified()
                && !target.loading
                && target.file.as_ref().is_some_and(|file| !file.saving)
        })
    }

    fn resume_session(
        &mut self,
        client: ClientId,
        session: Option<SaveSession>,
    ) -> Result<(), CommandError> {
        let Some(mut session) = session else {
            return Ok(());
        };
        while let Some(buffer) = session.queue.pop_front() {
            if !self.needs_saving(buffer) {
                continue;
            }
            session.offered = true;
            if session.save_all {
                let flow = SaveFlow::new(buffer, true, AfterSave::Nothing, Some(session));
                return self.continue_save(client, flow);
            }
            let path = self.buffers[buffer]
                .file
                .as_ref()
                .expect("needs_saving checked the file")
                .path
                .display()
                .to_string();
            let mode = Mode::Keys {
                choices: vec![
                    choice(plain('y'), 'y'),
                    choice(plain(' '), 'y'),
                    choice(plain('n'), 'n'),
                    choice(BACKSPACE, 'n'),
                    choice(plain('!'), '!'),
                    choice(plain('.'), '.'),
                    choice(plain('q'), 'q'),
                    choice(RETURN, 'q'),
                    choice(ESCAPE, 'q'),
                    choice(control('f'), 'f'),
                ],
                invalid: Some(Overlay::Replace(SAVE_SOME_INVALID.to_string())),
                help: Some((control('h'), SAVE_SOME_HELP.to_string())),
            };
            self.read_from_minibuffer(
                client,
                format!("Save file {path}? {SAVE_SOME_KEYS}"),
                String::new(),
                mode,
                Purpose::SaveSome { session, buffer },
            );
            return Ok(());
        }
        self.end_session(client, session);
        Ok(())
    }

    fn answer_save_some(
        &mut self,
        client: ClientId,
        mut session: SaveSession,
        buffer: BufferId,
        key: char,
    ) -> Result<(), CommandError> {
        match key {
            'y' => self.continue_save(
                client,
                SaveFlow::new(buffer, true, AfterSave::Nothing, Some(session)),
            ),
            'n' => self.resume_session(client, Some(session)),
            '!' => {
                session.save_all = true;
                let flow = SaveFlow::new(buffer, true, AfterSave::Nothing, Some(session));
                self.continue_save(client, flow)
            }
            '.' => {
                session.queue.clear();
                let flow = SaveFlow::new(buffer, true, AfterSave::Nothing, Some(session));
                self.continue_save(client, flow)
            }
            'q' => {
                self.end_session(client, session);
                Ok(())
            }
            'f' => {
                self.client_mut(client).exit_wait = None;
                if self.buffers.contains_key(buffer) {
                    self.show_in_selected_window(client, buffer);
                }
                Ok(())
            }
            other => unreachable!("save-some-buffers offers no key {other}"),
        }
    }

    fn end_session(&mut self, client: ClientId, session: SaveSession) {
        match session.end {
            SessionEnd::Report => {
                if !session.offered {
                    self.set_echo(client, "(No files need saving)".to_string());
                }
            }
            SessionEnd::Exit => {
                if let Some(wait) = self.client_mut(client).exit_wait.as_mut() {
                    wait.prompting = false;
                }
                self.check_exit(client);
            }
        }
    }

    /// Exits once the exit's saves are done, unless one failed.
    fn check_exit(&mut self, client: ClientId) {
        let Some(state) = self.clients.get_mut(&client) else {
            return;
        };
        // Every save must land first, including ones started before the
        // exit: shutting down would abandon them.
        let ready = state
            .exit_wait
            .as_ref()
            .is_some_and(|wait| !wait.prompting && wait.in_flight.is_empty())
            && self.pending_saves.is_empty();
        if !ready {
            return;
        }
        let wait = state.exit_wait.take().expect("checked above");
        if wait.failed {
            return;
        }
        match self.session {
            Session::NoServer if self.has_unsaved() => {
                self.yes_or_no_p(
                    client,
                    "Modified buffers exist; exit anyway? ".to_string(),
                    Purpose::ConfirmExit,
                );
            }
            _ => self.exit_now(client),
        }
    }

    /// `--no-server` ends the process; a server client's terminal is let go.
    fn exit_now(&mut self, client: ClientId) {
        match self.session {
            Session::NoServer => self.effects.push(Effect::Shutdown),
            Session::Server => self.effects.push(Effect::Send(
                client,
                ServerMsg::Exit {
                    reason: String::new(),
                },
            )),
        }
    }

    /// The daemon exits a grace period after the last client detached and
    /// nothing is unsaved; a save can make that true with no client.
    pub(super) fn maybe_arm_grace(&mut self) {
        if self.clients.is_empty() && !self.has_unsaved() && self.grace_timer.is_none() {
            let id = self.set_timer(TimerPurpose::DaemonGrace, self.config.daemon_grace_ms);
            self.grace_timer = Some(id);
        }
    }

    // ---- Auto-saving ----------------------------------------------------

    /// Counts a command that changed text toward Emacs `auto-save-interval`
    /// and keeps an idle timer for `auto-save-timeout`.
    pub(super) fn note_edit(&mut self, client: ClientId) {
        self.edits_total += 1;
        self.edits_since_auto_save += 1;
        if self.edits_since_auto_save > self.config.auto_save_interval_keys {
            self.do_auto_save(Some(client));
        }
        if self.auto_save_timer.is_none() {
            self.arm_auto_save_timer();
        }
    }

    fn arm_auto_save_timer(&mut self) {
        let delay = self.config.auto_save_timeout_s * MILLISECONDS_PER_SECOND;
        let id = self.set_timer(TimerPurpose::AutoSaveIdle, delay);
        self.auto_save_timer = Some((id, self.edits_total));
    }

    /// The idle timer fired: auto-save if nothing was edited since it was
    /// set, otherwise wait a full timeout from now.
    pub(super) fn auto_save_idle(&mut self, id: crate::event::TimerId) {
        let Some((current, edits)) = self.auto_save_timer else {
            return;
        };
        if current != id {
            return;
        }
        self.auto_save_timer = None;
        if edits == self.edits_total {
            self.do_auto_save(None);
        } else {
            self.arm_auto_save_timer();
        }
    }

    /// Emacs `do-auto-save` for the buffers that changed since their last
    /// auto-save; only buffers small enough to be held in memory.
    fn do_auto_save(&mut self, client: Option<ClientId>) {
        self.edits_since_auto_save = 0;
        let limit = self.config.in_memory_max;
        let due: Vec<BufferId> = self
            .buffers
            .iter()
            .filter(|(id, target)| {
                target.changed_since_auto_save()
                    && !target.loading
                    && target.text.len() <= limit
                    && !self.auto_saves.contains_key(id)
                    && target.file.as_ref().is_some_and(|file| !file.saving)
            })
            .map(|(id, _)| id)
            .collect();
        for buffer in due {
            let target = &mut self.buffers[buffer];
            let previous_mark = target.mark_auto_saved();
            let file = target
                .file
                .as_ref()
                .expect("filtered to file buffers")
                .path
                .clone();
            self.auto_saves.insert(
                buffer,
                AutoSaveRequest {
                    client,
                    previous_mark,
                },
            );
            self.effects.push(Effect::AutoSave(AutoSaveJob {
                buffer,
                file,
                text: target.text.clone(),
            }));
        }
    }

    pub(super) fn auto_saved(&mut self, buffer: BufferId, result: Result<(), String>) {
        let Some(request) = self.auto_saves.remove(&buffer) else {
            return;
        };
        let Some(target) = self.buffers.get_mut(buffer) else {
            return;
        };
        let clients: Vec<ClientId> = match request.client {
            Some(client) if self.clients.contains_key(&client) => vec![client],
            Some(_) => Vec::new(),
            None => self.clients.keys().copied().collect(),
        };
        match result {
            Ok(()) => {
                target.auto_saved_since_save = true;
                for client in clients {
                    if self.client(client).echo.is_empty() {
                        self.set_echo(client, "Auto-saving...done".to_string());
                    }
                }
            }
            Err(message) => {
                target.restore_auto_save_mark(request.previous_mark);
                let text = format!("Auto-saving {}: {message}", target.name);
                for client in clients {
                    self.ring_bell(client);
                    self.set_echo(client, text.clone());
                }
            }
        }
    }

    // ---- Recovering -----------------------------------------------------

    pub(crate) fn recover_this_file_command(
        &mut self,
        client: ClientId,
    ) -> Result<(), CommandError> {
        let buffer = self.selected_buffer(client);
        let Some(file) = self.buffers[buffer]
            .file
            .as_ref()
            .map(|file| file.path.clone())
        else {
            return Err(user_error("This buffer is not visiting a file"));
        };
        let auto_save = autosave::auto_save_path(&file);
        let file_exists = file.exists();
        let auto_save_exists = auto_save.exists();
        if file_exists && !auto_save_exists {
            return Err(user_error(format!(
                "Auto save file {} does not exist",
                abbreviate(&auto_save)
            )));
        }
        let current = if file_exists {
            autosave::auto_save_newer(&file)
        } else {
            auto_save_exists
        };
        if !current {
            return Err(user_error(format!(
                "Auto-save file {} not current",
                abbreviate(&auto_save)
            )));
        }
        self.yes_or_no_p(
            client,
            format!("Recover auto save file {}? ", auto_save.display()),
            Purpose::Recover { buffer, file },
        );
        Ok(())
    }

    /// Replaces the text with the auto-save file's as one undoable change,
    /// as `recover-file` erases and inserts with `inhibit-read-only`.
    pub(super) fn auto_save_read(&mut self, buffer: BufferId, result: Result<Vec<u8>, String>) {
        let client = self.pending_reads.remove(&buffer);
        if !self.buffers.contains_key(buffer) {
            return;
        }
        let bytes = match result {
            Ok(bytes) => bytes,
            Err(message) => {
                if let Some(client) = client {
                    self.fail(client, &user_error(message));
                }
                return;
            }
        };
        let target = &self.buffers[buffer];
        let busy = target.loading || target.file.as_ref().is_some_and(|file| file.saving);
        if busy {
            if let Some(client) = client {
                self.fail(client, &EditError::Saving.into());
            }
            return;
        }
        let text = Text::from_original(Sources::memory(bytes.into()));
        let (target, recorder) = self.buffer_and_recorder(buffer);
        target.apply_replace_all(text, recorder);
        target.mark_auto_saved();
        target.auto_saved_since_save = true;
        target.undo.boundary(0);
        if let Some(client) = client.filter(|client| self.clients.contains_key(client)) {
            let window = self.selected_window_id(client);
            if self.window(client, window).buffer == buffer {
                self.set_window_point(client, window, 0);
            }
        }
    }
}

/// Whether the text reads its original from the file `identity` names
/// (same device and inode, whatever its version).
fn original_inode_is(text: &Text, identity: Option<FileIdentity>) -> bool {
    let (Original::File(cache), Some(identity)) = (&text.sources().original, identity) else {
        return false;
    };
    let opened = cache.identity();
    opened.dev == identity.dev && opened.ino == identity.ino
}

/// Whether the text's inserted bytes are all ASCII; an original read as
/// undecided is known to be.
fn inserted_text_is_ascii(text: &Text) -> bool {
    let mut bytes = Vec::new();
    text.pieces()
        .filter(|(_, piece)| piece.source == PieceSource::Add)
        .all(|(offset, piece)| {
            bytes.clear();
            text.read(offset..offset + piece.len, &mut bytes)
                .expect("inserted text is in memory");
            bytes.is_ascii()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_matches_file_size_human_readable() {
        // (file-size-human-readable N) in Emacs 29.3.
        assert_eq!(human_size(0), "0");
        assert_eq!(human_size(1023), "1023");
        assert_eq!(human_size(3001), "2.9k");
        assert_eq!(human_size(10 * 1024), "10k");
        assert_eq!(human_size(600 * 1024 * 1024), "600M");
        assert_eq!(human_size(1024 * 1024 * 1024 + 1), "1G");
    }
}
