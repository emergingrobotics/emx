//! The file panel (DESIGN section 6): a side window on the left of a
//! client's frame showing a tree of the project, over a hidden, read-only
//! buffer the editor writes. Listings, git status and file operations run
//! off the editor thread (`Effect::Tree`); the panel updates as they come.
//! Also quick-open (`project-find-file`), which lists the same project.

use std::collections::{BTreeSet, HashMap};
use std::ops::Range;
use std::path::{Path, PathBuf};

use emx_types::Face;

use super::files::file_name;
use super::reload::Unjournaled;
use super::{Editor, Purpose, TimerPurpose};
use crate::buffer::BufferId;
use crate::command::CommandError;
use crate::completion::{Candidate, Completer, Kind, ListSource};
use crate::event::{ClientId, Effect, ListingId};
use crate::io::{abbreviate_bytes, expand_file_name};
use crate::marker::InsertionType;
use crate::minibuffer::{HistoryKind, Minibuffer, Mode};
use crate::tree::{FileOp, GitMark, TreeEntry, TreeJob, TreeOutcome};
use crate::undo::UndoLimits;
use crate::window::{WINDOW_MIN_WIDTH, Window, WindowId};

/// The fewest text columns a panel is opened with.
pub(crate) const PANEL_MIN_TEXT_COLS: u16 = 10;
/// The panel's border column.
const PANEL_BORDER_COLS: u16 = 1;
const STATUS_COLUMNS: usize = 2;
const INDENT: &[u8] = b"  ";
const EXPANDED: &[u8] = b"- ";
const COLLAPSED: &[u8] = b"+ ";
const FILE_ICON: &[u8] = b"  ";
const IGNORED_MARK: u8 = b'!';
const CHANGED_MARK: u8 = b'*';
const NO_MARK: u8 = b' ';
const LISTING: &str = "  (listing...)";
const NOT_IN_PANEL: &str = "Not in the file panel";
const TOO_NARROW: &str = "The terminal is too narrow for the file panel";
const QUICK_OPEN_PROMPT: &str = "Find file in project: ";
const LISTING_PROJECT: &str = "Listing project files...";

/// What a panel shows: its root, the directories expanded under it, and
/// what was read about them. Held by the panel's buffer.
#[derive(Debug)]
pub(crate) struct TreeData {
    /// `None` until the first listing finds the project root.
    pub root: Option<PathBuf>,
    pub expanded: BTreeSet<PathBuf>,
    pub listings: HashMap<PathBuf, Vec<TreeEntry>>,
    /// The entry on each line after the root's.
    pub lines: Vec<TreeLine>,
    pub show_ignored: bool,
    /// Git status of files, and the strongest mark under each directory.
    pub marks: HashMap<PathBuf, GitMark>,
    /// Directories watched for changes, and those that changed since they
    /// were last listed.
    pub watched: BTreeSet<PathBuf>,
    pub dirty: BTreeSet<PathBuf>,
    /// The root could not be listed.
    pub error: Option<String>,
    pub refresh_timer: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeLine {
    pub path: PathBuf,
    pub depth: usize,
    pub is_dir: bool,
    pub ignored: bool,
}

impl TreeLine {
    /// Where the name starts on the line.
    fn name_column(&self) -> u64 {
        (STATUS_COLUMNS + INDENT.len() * self.depth + EXPANDED.len()) as u64
    }
}

impl TreeData {
    fn new() -> TreeData {
        TreeData {
            root: None,
            expanded: BTreeSet::new(),
            listings: HashMap::new(),
            lines: Vec::new(),
            show_ignored: false,
            marks: HashMap::new(),
            watched: BTreeSet::new(),
            dirty: BTreeSet::new(),
            error: None,
            refresh_timer: false,
        }
    }
}

/// What a quick-open waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QuickOpen {
    pub id: ListingId,
}

impl Editor {
    // ---- Opening and closing ---------------------------------------------

    /// `toggle-tree` (`F8`).
    pub(crate) fn toggle_tree_command(&mut self, client: ClientId) -> Result<(), CommandError> {
        if self.frame(client).panel().is_some() {
            self.close_panel(client);
            return Ok(());
        }
        let width = self.client(client).cols;
        let room = width.saturating_sub(WINDOW_MIN_WIDTH);
        if room < PANEL_MIN_TEXT_COLS + PANEL_BORDER_COLS {
            return Err(CommandError::User(TOO_NARROW.to_string()));
        }
        let cols = self.panel_width.min(room);
        let start = self.directory_of(self.selected_buffer(client));
        let frame_number = self.client(client).frame_number.unwrap_or(1);
        let buffer = self.create_buffer(&format!(" *tree F{frame_number}*"));
        let target = &mut self.buffers[buffer];
        target.read_only = true;
        target.tree = Some(Box::new(TreeData::new()));
        let window = Window {
            buffer,
            point: target.create_marker_without_undo(0, InsertionType::StayBefore),
            start: target.create_marker_without_undo(0, InsertionType::StayBefore),
            start_row_skip: 0,
            start_cols: 0,
            hscroll: 0,
            number: self.next_window_number,
            use_time: 0,
        };
        self.next_window_number += 1;
        let id = self.client_mut(client).frame.open_panel(window, cols);
        self.trees.insert(client, buffer);
        self.select_window(client, id);
        self.rebuild_tree(client, None);
        self.effects.push(Effect::Tree {
            client,
            job: TreeJob::List {
                dir: start,
                find_root: true,
            },
        });
        Ok(())
    }

    /// Closes the client's panel, if open, and kills its buffer.
    pub(super) fn close_panel(&mut self, client: ClientId) {
        let Some(panel) = self.frame(client).panel() else {
            return;
        };
        if self.frame(client).selected == panel.window {
            let main = self.main_window(client);
            self.select_window(client, main);
        }
        if let Some((window, history)) = self.client_mut(client).frame.close_panel() {
            self.discard_window(window, history);
        }
        if let Some(buffer) = self.trees.remove(&client) {
            self.forget_tree(buffer);
            self.kill_buffer(buffer);
        }
    }

    /// The tree buffer is going: its directories are no longer watched.
    pub(super) fn forget_tree(&mut self, buffer: BufferId) {
        let Some(tree) = self
            .buffers
            .get_mut(buffer)
            .and_then(|buffer| buffer.tree.take())
        else {
            return;
        };
        for dir in tree.watched {
            self.effects.push(Effect::UnwatchTree { dir });
        }
    }

    /// The window of the tree selected most recently: where the panel
    /// opens files.
    pub(super) fn main_window(&self, client: ClientId) -> WindowId {
        let frame = self.frame(client);
        frame
            .order()
            .into_iter()
            .max_by_key(|window| frame.windows[*window].use_time)
            .expect("a frame has a window")
    }

    /// A mouse drag on the panel's border asks for `cols` columns.
    pub(super) fn resize_panel(&mut self, client: ClientId, cols: u16) -> u16 {
        let cols = cols.max(PANEL_MIN_TEXT_COLS + PANEL_BORDER_COLS);
        self.panel_width = cols;
        self.client_mut(client).frame.set_panel_cols(cols)
    }

    // ---- Results ----------------------------------------------------------

    pub(super) fn tree_result(&mut self, client: ClientId, outcome: TreeOutcome) {
        match outcome {
            TreeOutcome::Listed { dir, result } => self.tree_listed(client, dir, result),
            TreeOutcome::GitStatus { root, result } => {
                let Some(tree) = self.tree_mut(client) else {
                    return;
                };
                if tree.root.as_ref() != Some(&root) {
                    return;
                }
                // Outside a repository there is simply no status to show.
                let marks = result.unwrap_or_default();
                tree.marks = directory_marks(&root, marks);
                self.rebuild_tree(client, None);
            }
            TreeOutcome::Operated { op, result } => self.tree_operated(client, op, result),
            TreeOutcome::ProjectListed { id, root, result } => {
                self.project_listed(client, id, root, result);
            }
        }
    }

    fn tree_listed(
        &mut self,
        client: ClientId,
        dir: PathBuf,
        result: Result<Vec<TreeEntry>, String>,
    ) {
        let Some(tree) = self.tree_mut(client) else {
            return;
        };
        let first = tree.root.is_none();
        if first {
            tree.root = Some(dir.clone());
            tree.expanded.insert(dir.clone());
        }
        let is_root = tree.root.as_ref() == Some(&dir);
        match result {
            Ok(entries) => {
                tree.listings.insert(dir.clone(), entries);
                if is_root {
                    tree.error = None;
                }
            }
            Err(message) => {
                tree.listings.remove(&dir);
                if is_root {
                    tree.error = Some(message);
                } else {
                    tree.expanded.remove(&dir);
                    self.set_echo(client, message);
                }
            }
        }
        if first {
            self.effects.push(Effect::Tree {
                client,
                job: TreeJob::GitStatus { root: dir.clone() },
            });
        }
        self.watch_expanded(client);
        self.rebuild_tree(client, None);
    }

    fn tree_operated(&mut self, client: ClientId, op: FileOp, result: Result<(), String>) {
        if let Err(message) = result {
            self.fail(client, &CommandError::User(message));
            return;
        }
        let touched: Vec<PathBuf> = match &op {
            FileOp::CreateDirectory(path) | FileOp::Delete(path) => vec![path.clone()],
            FileOp::Rename { from, to } => {
                self.follow_rename(from, to);
                vec![from.clone(), to.clone()]
            }
        };
        for path in touched {
            if let Some(parent) = path.parent() {
                self.relist(client, parent.to_path_buf());
            }
        }
        self.request_git_status(client);
    }

    /// Buffers visiting a renamed file, or a file under a renamed
    /// directory, visit it under its new name, as dired renames them.
    fn follow_rename(&mut self, from: &Path, to: &Path) {
        for (_, buffer) in self.buffers.iter_mut() {
            if let Some(file) = &mut buffer.file
                && let Ok(rest) = file.path.strip_prefix(from)
            {
                file.path = if rest.as_os_str().is_empty() {
                    to.to_path_buf()
                } else {
                    to.join(rest)
                };
            }
        }
    }

    /// A watched directory changed: it is listed again once changes stop
    /// coming for a moment, with the git status.
    pub(super) fn tree_changed(&mut self, dir: &Path) {
        let clients: Vec<ClientId> = self.trees.keys().copied().collect();
        for client in clients {
            let Some(tree) = self.tree_mut(client) else {
                continue;
            };
            if !tree.expanded.contains(dir) {
                continue;
            }
            tree.dirty.insert(dir.to_path_buf());
            self.schedule_tree_refresh(client);
        }
    }

    /// Something was saved or reloaded: every panel's git status is looked
    /// at again shortly.
    pub(super) fn files_changed_for_trees(&mut self) {
        let clients: Vec<ClientId> = self.trees.keys().copied().collect();
        for client in clients {
            self.schedule_tree_refresh(client);
        }
    }

    fn schedule_tree_refresh(&mut self, client: ClientId) {
        let Some(tree) = self.tree_mut(client) else {
            return;
        };
        if tree.refresh_timer {
            return;
        }
        tree.refresh_timer = true;
        let after = self.config.settle_window_ms;
        self.set_timer(TimerPurpose::TreeRefresh(client), after);
    }

    pub(super) fn tree_refresh_timer(&mut self, client: ClientId) {
        let Some(tree) = self.tree_mut(client) else {
            return;
        };
        tree.refresh_timer = false;
        let dirty = std::mem::take(&mut tree.dirty);
        for dir in dirty {
            self.relist(client, dir);
        }
        self.request_git_status(client);
    }

    fn relist(&mut self, client: ClientId, dir: PathBuf) {
        let Some(tree) = self.tree(client) else {
            return;
        };
        if tree.expanded.contains(&dir) {
            self.effects.push(Effect::Tree {
                client,
                job: TreeJob::List {
                    dir,
                    find_root: false,
                },
            });
        }
    }

    fn request_git_status(&mut self, client: ClientId) {
        if let Some(root) = self.tree(client).and_then(|tree| tree.root.clone()) {
            self.effects.push(Effect::Tree {
                client,
                job: TreeJob::GitStatus { root },
            });
        }
    }

    /// Watches every expanded directory, and no others.
    fn watch_expanded(&mut self, client: ClientId) {
        let Some(tree) = self.tree_mut(client) else {
            return;
        };
        let wanted: BTreeSet<PathBuf> = tree
            .expanded
            .iter()
            .filter(|dir| tree.listings.contains_key(*dir))
            .cloned()
            .collect();
        let added: Vec<PathBuf> = wanted.difference(&tree.watched).cloned().collect();
        let removed: Vec<PathBuf> = tree.watched.difference(&wanted).cloned().collect();
        tree.watched = wanted;
        for dir in added {
            self.effects.push(Effect::WatchTree { dir });
        }
        for dir in removed {
            self.effects.push(Effect::UnwatchTree { dir });
        }
    }

    // ---- The buffer's text ------------------------------------------------

    fn tree_buffer(&self, client: ClientId) -> Option<BufferId> {
        self.trees.get(&client).copied()
    }

    fn tree(&self, client: ClientId) -> Option<&TreeData> {
        let buffer = self.tree_buffer(client)?;
        self.buffers.get(buffer)?.tree.as_deref()
    }

    fn tree_mut(&mut self, client: ClientId) -> Option<&mut TreeData> {
        let buffer = self.tree_buffer(client)?;
        self.buffers.get_mut(buffer)?.tree.as_deref_mut()
    }

    /// Writes the panel's text again from its model, keeping point on the
    /// entry it was on (or on `focus`), at the start of its name.
    fn rebuild_tree(&mut self, client: ClientId, focus: Option<PathBuf>) {
        let Some(buffer) = self.tree_buffer(client) else {
            return;
        };
        let changed: BTreeSet<PathBuf> = self
            .buffers
            .iter()
            .filter_map(|(_, buffer)| buffer.file.as_ref())
            .filter(|file| file.changed_since_viewed)
            .map(|file| file.path.clone())
            .collect();
        let focus = focus.or_else(|| self.tree_entry_at_point(client).map(|line| line.path));
        let limits = UndoLimits::from(&self.config);
        let target = &mut self.buffers[buffer];
        let Some(tree) = target.tree.as_deref_mut() else {
            return;
        };
        let (text, lines, spans) = tree_text(tree, &changed);
        tree.lines = lines;
        let point = focus
            .and_then(|path| {
                let index = tree.lines.iter().position(|line| line.path == path)?;
                Some((index, tree.lines[index].name_column()))
            })
            .map(|(index, column)| line_offset(&text, index + 1) + column);
        let length = text.len() as u64;
        target.read_only = true;
        target.reset_text(
            emx_text::Text::from_original(emx_text::Sources::memory(std::sync::Arc::from(text))),
            limits,
            &mut Unjournaled,
        );
        target.faces.replace_range(0..length, spans);
        let point = point.unwrap_or(0).min(length);
        let window = self.frame(client).panel().map(|panel| panel.window);
        if let Some(window) = window {
            let window = self.frame(client).windows[window];
            let target = &mut self.buffers[buffer];
            target.set_marker(window.point, point);
        }
    }

    /// The entry on the line point is on in the panel, if any.
    fn tree_entry_at_point(&self, client: ClientId) -> Option<TreeLine> {
        let panel = self.frame(client).panel()?;
        let window = self.frame(client).windows[panel.window];
        let buffer = &self.buffers[window.buffer];
        let tree = buffer.tree.as_deref()?;
        let line = line_of(buffer, buffer.marker_pos(window.point))?;
        tree.lines.get(line.checked_sub(1)?).cloned()
    }

    /// The selected window shows this client's panel: its tree's buffer.
    fn panel_here(&self, client: ClientId) -> Result<BufferId, CommandError> {
        let buffer = self.selected_buffer(client);
        if self.tree_buffer(client) == Some(buffer) {
            Ok(buffer)
        } else {
            Err(CommandError::User(NOT_IN_PANEL.to_string()))
        }
    }

    /// The line point is on in the selected panel: 0 is the root's.
    fn panel_line(&self, client: ClientId) -> Result<usize, CommandError> {
        let buffer = self.panel_here(client)?;
        let window = self.window(client, self.selected_window_id(client));
        let target = &self.buffers[buffer];
        Ok(line_of(target, target.marker_pos(window.point)).unwrap_or(0))
    }

    fn panel_entry(&self, client: ClientId) -> Result<Option<TreeLine>, CommandError> {
        let line = self.panel_line(client)?;
        let tree = self
            .tree(client)
            .ok_or_else(|| CommandError::User(NOT_IN_PANEL.to_string()))?;
        Ok(line
            .checked_sub(1)
            .and_then(|index| tree.lines.get(index).cloned()))
    }

    /// Puts point on line `line` (0 is the root's) at its name.
    fn panel_goto_line(&mut self, client: ClientId, line: usize) {
        let Some(buffer) = self.tree_buffer(client) else {
            return;
        };
        let Some(tree) = self.buffers[buffer].tree.as_deref() else {
            return;
        };
        let column = match line.checked_sub(1).and_then(|index| tree.lines.get(index)) {
            Some(entry) => entry.name_column(),
            None => STATUS_COLUMNS as u64,
        };
        let mut text = Vec::new();
        let target = &self.buffers[buffer];
        if target.text.read(0..target.text.len(), &mut text).is_err() {
            return;
        }
        let point = (line_offset(&text, line) + column).min(text.len() as u64);
        if let Some(panel) = self.frame(client).panel() {
            let window = self.frame(client).windows[panel.window];
            self.buffers[buffer].set_marker(window.point, point);
        }
    }

    // ---- Commands -----------------------------------------------------------

    /// `tree-next-line` (`n`) and `tree-previous-line` (`p`): to the name
    /// on the next or previous line.
    pub(crate) fn tree_move_command(
        &mut self,
        client: ClientId,
        count: i64,
    ) -> Result<(), CommandError> {
        let line = self.panel_line(client)?;
        let last = self.tree(client).map_or(0, |tree| tree.lines.len());
        let target = i64::try_from(line)
            .unwrap_or(i64::MAX)
            .saturating_add(count)
            .clamp(0, i64::try_from(last).unwrap_or(i64::MAX));
        self.panel_goto_line(client, usize::try_from(target).unwrap_or(0));
        Ok(())
    }

    /// `tree-open` (`RET`): a file is shown in the most recently selected
    /// other window, which is selected; a directory opens or closes.
    pub(crate) fn tree_open_command(&mut self, client: ClientId) -> Result<(), CommandError> {
        match self.panel_entry(client)? {
            Some(entry) if entry.is_dir => self.toggle_directory(client, &entry.path),
            Some(entry) => {
                let main = self.main_window(client);
                self.select_window(client, main);
                self.visit_file(client, entry.path);
            }
            None => {}
        }
        Ok(())
    }

    /// `tree-toggle` (`TAB`): opens or closes the directory on the line.
    pub(crate) fn tree_toggle_command(&mut self, client: ClientId) -> Result<(), CommandError> {
        if let Some(entry) = self.panel_entry(client)?
            && entry.is_dir
        {
            self.toggle_directory(client, &entry.path);
        }
        Ok(())
    }

    fn toggle_directory(&mut self, client: ClientId, dir: &Path) {
        let Some(tree) = self.tree_mut(client) else {
            return;
        };
        let listed = tree.listings.contains_key(dir);
        if !tree.expanded.remove(dir) {
            tree.expanded.insert(dir.to_path_buf());
            if !listed {
                self.effects.push(Effect::Tree {
                    client,
                    job: TreeJob::List {
                        dir: dir.to_path_buf(),
                        find_root: false,
                    },
                });
            }
        }
        self.watch_expanded(client);
        self.rebuild_tree(client, Some(dir.to_path_buf()));
    }

    /// `tree-up` (`^`): to the parent directory's line; on the root's
    /// line, the root's parent becomes the root.
    pub(crate) fn tree_up_command(&mut self, client: ClientId) -> Result<(), CommandError> {
        let entry = self.panel_entry(client)?;
        let Some(tree) = self.tree_mut(client) else {
            return Ok(());
        };
        let Some(root) = tree.root.clone() else {
            return Ok(());
        };
        match entry {
            Some(entry) => {
                let parent = entry.path.parent().map(Path::to_path_buf);
                let line = parent
                    .and_then(|parent| tree.lines.iter().position(|line| line.path == parent))
                    .map_or(0, |index| index + 1);
                self.panel_goto_line(client, line);
            }
            None => {
                let Some(parent) = root.parent().map(Path::to_path_buf) else {
                    return Ok(());
                };
                tree.root = Some(parent.clone());
                tree.expanded.insert(parent.clone());
                tree.marks.clear();
                self.effects.push(Effect::Tree {
                    client,
                    job: TreeJob::List {
                        dir: parent,
                        find_root: false,
                    },
                });
                self.request_git_status(client);
                self.rebuild_tree(client, Some(root));
            }
        }
        Ok(())
    }

    /// `tree-refresh` (`g`): every expanded directory is listed again, with
    /// the git status.
    pub(crate) fn tree_refresh_command(&mut self, client: ClientId) -> Result<(), CommandError> {
        self.panel_here(client)?;
        let expanded: Vec<PathBuf> = self
            .tree(client)
            .map(|tree| tree.expanded.iter().cloned().collect())
            .unwrap_or_default();
        for dir in expanded {
            self.relist(client, dir);
        }
        self.request_git_status(client);
        Ok(())
    }

    /// `tree-toggle-ignored` (`i`).
    pub(crate) fn tree_toggle_ignored_command(
        &mut self,
        client: ClientId,
    ) -> Result<(), CommandError> {
        self.panel_here(client)?;
        let Some(tree) = self.tree_mut(client) else {
            return Ok(());
        };
        tree.show_ignored = !tree.show_ignored;
        let message = if tree.show_ignored {
            "Showing ignored files"
        } else {
            "Hiding ignored files"
        };
        self.rebuild_tree(client, None);
        self.set_echo(client, message.to_string());
        Ok(())
    }

    /// The directory a new entry goes in: the one on the line, or the one
    /// holding the file on it, or the root.
    fn panel_directory(&self, client: ClientId) -> Result<PathBuf, CommandError> {
        let entry = self.panel_entry(client)?;
        let root = self
            .tree(client)
            .and_then(|tree| tree.root.clone())
            .ok_or_else(|| CommandError::User(NOT_IN_PANEL.to_string()))?;
        Ok(match entry {
            Some(entry) if entry.is_dir => entry.path,
            Some(entry) => entry.path.parent().map_or(root, Path::to_path_buf),
            None => root,
        })
    }

    /// `tree-create-directory` (`+`), as dired's `+`.
    pub(crate) fn tree_create_directory_command(
        &mut self,
        client: ClientId,
    ) -> Result<(), CommandError> {
        let base = self.panel_directory(client)?;
        let mut initial = abbreviate_bytes(&base);
        if !initial.ends_with(b"/") {
            initial.push(b'/');
        }
        self.read_from_minibuffer(
            client,
            "Create directory: ".to_string(),
            initial,
            line_mode(),
            Purpose::TreeCreateDirectory { base },
        );
        Ok(())
    }

    /// `tree-rename` (`R`), as dired's `R` on one file.
    pub(crate) fn tree_rename_command(&mut self, client: ClientId) -> Result<(), CommandError> {
        let Some(entry) = self.panel_entry(client)? else {
            return Err(CommandError::User("No file on this line".to_string()));
        };
        let prompt = format!("Rename {} to: ", file_name(&entry.path));
        self.read_from_minibuffer(
            client,
            prompt,
            abbreviate_bytes(&entry.path),
            line_mode(),
            Purpose::TreeRename { from: entry.path },
        );
        Ok(())
    }

    /// `tree-delete` (`d`), as dired's `D` on one file: asks first.
    pub(crate) fn tree_delete_command(&mut self, client: ClientId) -> Result<(), CommandError> {
        let Some(entry) = self.panel_entry(client)? else {
            return Err(CommandError::User("No file on this line".to_string()));
        };
        let question = format!("Delete {}? ", file_name(&entry.path));
        self.yes_or_no_p(client, question, Purpose::TreeDelete { path: entry.path });
        Ok(())
    }

    pub(super) fn tree_create_answered(&mut self, client: ClientId, base: &Path, input: &[u8]) {
        let path = expand_file_name(input, base);
        self.effects.push(Effect::Tree {
            client,
            job: TreeJob::Operate(FileOp::CreateDirectory(path)),
        });
    }

    pub(super) fn tree_rename_answered(&mut self, client: ClientId, from: PathBuf, input: &[u8]) {
        let base = from.parent().map_or_else(PathBuf::new, Path::to_path_buf);
        let to = expand_file_name(input, &base);
        if to == from {
            return;
        }
        self.effects.push(Effect::Tree {
            client,
            job: TreeJob::Operate(FileOp::Rename { from, to }),
        });
    }

    pub(super) fn tree_delete_answered(&mut self, client: ClientId, path: PathBuf, yes: bool) {
        if yes {
            self.effects.push(Effect::Tree {
                client,
                job: TreeJob::Operate(FileOp::Delete(path)),
            });
        }
    }

    /// A click on panel line `row` of the panel's text (0 is its first
    /// row on screen): point goes there, and a file opens or a directory
    /// opens or closes, as `RET` does.
    pub(super) fn panel_click(&mut self, client: ClientId, row: u16) {
        let Some(panel) = self.frame(client).panel() else {
            return;
        };
        self.select_window(client, panel.window);
        let window = self.frame(client).windows[panel.window];
        let buffer = &self.buffers[window.buffer];
        let top = line_of(buffer, buffer.marker_pos(window.start)).unwrap_or(0);
        let lines = self.tree(client).map_or(0, |tree| tree.lines.len());
        let line = top + usize::from(row);
        if line > lines {
            return;
        }
        self.panel_goto_line(client, line);
        if line > 0
            && let Err(error) = self.tree_open_command(client)
        {
            self.fail(client, &error);
        }
    }

    // ---- Quick-open ---------------------------------------------------------

    /// `project-find-file` (`C-x p f`): the files of the project the
    /// current buffer's directory is in (or the panel's root), then one of
    /// them read with completion.
    pub(crate) fn project_find_file_command(&mut self, client: ClientId) {
        let dir = self
            .tree(client)
            .and_then(|tree| tree.root.clone())
            .unwrap_or_else(|| self.directory_of(self.selected_buffer(client)));
        let id = ListingId(self.next_listing);
        self.next_listing += 1;
        self.client_mut(client).quick_open = Some(QuickOpen { id });
        self.set_echo(client, LISTING_PROJECT.to_string());
        let max = usize::try_from(self.config.quick_open_max_files).unwrap_or(usize::MAX);
        self.effects.push(Effect::Tree {
            client,
            job: TreeJob::ListProject { id, root: dir, max },
        });
    }

    fn project_listed(
        &mut self,
        client: ClientId,
        id: ListingId,
        root: PathBuf,
        result: Result<(Vec<PathBuf>, bool), String>,
    ) {
        let Some(client_state) = self.clients.get_mut(&client) else {
            return;
        };
        if client_state.quick_open != Some(QuickOpen { id }) {
            return;
        }
        client_state.quick_open = None;
        if client_state.minibuffer.is_some() {
            return;
        }
        let (files, capped) = match result {
            Ok(listed) => listed,
            Err(message) => {
                self.fail(client, &CommandError::User(message));
                return;
            }
        };
        let candidates: Vec<Candidate> = files
            .iter()
            .map(|path| Candidate::new(path.as_os_str().as_encoded_bytes().to_vec()))
            .collect();
        let source = ListSource::new(candidates);
        let completer = Completer::listed(
            Kind::ProjectFile,
            &source,
            None,
            true,
            self.completion_rows(),
        );
        let mode = Mode::Line {
            history: HistoryKind::FileName,
            defaults: Vec::new(),
            require: None,
            allow_empty: false,
        };
        let minibuffer = Minibuffer::new(
            QUICK_OPEN_PROMPT.to_string(),
            Vec::new(),
            mode,
            Purpose::ProjectFile { root },
        )
        .with_completion(completer);
        self.start_minibuffer(client, minibuffer);
        if capped {
            let shown = self.config.quick_open_max_files;
            self.set_echo(client, format!("Only the first {shown} files are listed"));
        }
    }

    pub(super) fn project_file_answered(&mut self, client: ClientId, root: &Path, input: &[u8]) {
        let path = expand_file_name(input, root);
        if self.frame(client).is_panel(self.selected_window_id(client)) {
            let main = self.main_window(client);
            self.select_window(client, main);
        }
        self.visit_file(client, path);
    }
}

fn line_mode() -> Mode {
    Mode::Line {
        history: HistoryKind::FileName,
        defaults: Vec::new(),
        require: None,
        allow_empty: false,
    }
}

/// Git marks of files, with each directory between them and `root`
/// marked with the strongest mark beneath it.
fn directory_marks(root: &Path, marks: Vec<(PathBuf, GitMark)>) -> HashMap<PathBuf, GitMark> {
    let mut all: HashMap<PathBuf, GitMark> = HashMap::new();
    for (path, mark) in marks {
        for ancestor in path.ancestors() {
            let entry = all.entry(ancestor.to_path_buf()).or_insert(mark);
            *entry = (*entry).max(mark);
            if ancestor == root || !ancestor.starts_with(root) {
                break;
            }
        }
    }
    all
}

/// The 0-based line `pos` is on, from the buffer's cached text.
fn line_of(buffer: &crate::buffer::Buffer, pos: u64) -> Option<usize> {
    let mut text = Vec::new();
    buffer
        .text
        .read(0..pos.min(buffer.text.len()), &mut text)
        .ok()?;
    Some(text.iter().filter(|byte| **byte == b'\n').count())
}

/// The byte where 0-based line `line` starts.
fn line_offset(text: &[u8], line: usize) -> u64 {
    if line == 0 {
        return 0;
    }
    text.iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'\n')
        .nth(line - 1)
        .map_or(text.len(), |(index, _)| index + 1) as u64
}

type Spans = Vec<(Range<u64>, Face)>;

/// The panel's text, the entry on each line after the root's, and the
/// faces it is drawn in.
fn tree_text(tree: &TreeData, changed: &BTreeSet<PathBuf>) -> (Vec<u8>, Vec<TreeLine>, Spans) {
    let mut text = Vec::new();
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let Some(root) = &tree.root else {
        text.extend_from_slice(LISTING.as_bytes());
        return (text, lines, spans);
    };
    text.extend_from_slice(&[NO_MARK; STATUS_COLUMNS]);
    let start = text.len() as u64;
    text.extend_from_slice(&abbreviate_bytes(root));
    if !text.ends_with(b"/") {
        text.push(b'/');
    }
    spans.push((start..text.len() as u64, Face::PanelDirectory));
    if let Some(error) = &tree.error {
        text.extend_from_slice(b"\n  ");
        let start = text.len() as u64;
        text.extend_from_slice(error.as_bytes());
        spans.push((start..text.len() as u64, Face::Error));
    }
    add_directory(tree, root, 0, changed, &mut text, &mut lines, &mut spans);
    (text, lines, spans)
}

#[allow(clippy::too_many_arguments)]
fn add_directory(
    tree: &TreeData,
    dir: &Path,
    depth: usize,
    changed: &BTreeSet<PathBuf>,
    text: &mut Vec<u8>,
    lines: &mut Vec<TreeLine>,
    spans: &mut Spans,
) {
    let Some(entries) = tree.listings.get(dir) else {
        if tree.expanded.contains(dir) {
            text.push(b'\n');
            text.extend_from_slice(&[NO_MARK; STATUS_COLUMNS]);
            text.extend(INDENT.repeat(depth + 1));
            text.extend_from_slice(b"...");
        }
        return;
    };
    for entry in entries {
        if entry.ignored && !tree.show_ignored {
            continue;
        }
        let path = dir.join(&entry.name);
        text.push(b'\n');
        let line_start = text.len() as u64;
        let (mark, mark_face) = if entry.ignored {
            (IGNORED_MARK, Some(Face::GitIgnored))
        } else {
            match tree.marks.get(&path) {
                Some(GitMark::Modified) => (b'M', Some(Face::GitModified)),
                Some(GitMark::Added) => (b'A', Some(Face::GitAdded)),
                Some(GitMark::Untracked) => (b'?', Some(Face::GitUntracked)),
                None => (NO_MARK, None),
            }
        };
        text.push(mark);
        if let Some(face) = mark_face {
            spans.push((line_start..line_start + 1, face));
        }
        if changed.contains(&path) {
            text.push(CHANGED_MARK);
            spans.push((line_start + 1..line_start + 2, Face::ChangedMarker));
        } else {
            text.push(NO_MARK);
        }
        text.extend(INDENT.repeat(depth));
        let expanded = entry.is_dir && tree.expanded.contains(&path);
        text.extend_from_slice(match (entry.is_dir, expanded) {
            (true, true) => EXPANDED,
            (true, false) => COLLAPSED,
            (false, _) => FILE_ICON,
        });
        let name_start = text.len() as u64;
        text.extend_from_slice(entry.name.as_encoded_bytes());
        if entry.is_dir {
            text.push(b'/');
        }
        let face = if entry.ignored {
            Some(Face::GitIgnored)
        } else if entry.is_dir {
            Some(Face::PanelDirectory)
        } else {
            None
        };
        if let Some(face) = face {
            spans.push((name_start..text.len() as u64, face));
        }
        lines.push(TreeLine {
            path: path.clone(),
            depth,
            is_dir: entry.is_dir,
            ignored: entry.ignored,
        });
        if expanded {
            add_directory(tree, &path, depth + 1, changed, text, lines, spans);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    fn entry(name: &str, is_dir: bool, ignored: bool) -> TreeEntry {
        TreeEntry {
            name: OsString::from(name),
            is_dir,
            ignored,
        }
    }

    #[test]
    fn tree_text_lays_out_expanded_directories_with_marks() {
        let root = PathBuf::from("/p");
        let mut tree = TreeData::new();
        tree.root = Some(root.clone());
        tree.expanded.insert(root.clone());
        tree.expanded.insert(root.join("src"));
        tree.listings.insert(
            root.clone(),
            vec![
                entry("src", true, false),
                entry("target", true, true),
                entry("README", false, false),
            ],
        );
        tree.listings
            .insert(root.join("src"), vec![entry("main.rs", false, false)]);
        tree.marks = directory_marks(&root, vec![(root.join("src/main.rs"), GitMark::Modified)]);
        let changed = BTreeSet::from([root.join("README")]);
        let (text, lines, spans) = tree_text(&tree, &changed);
        assert_eq!(
            String::from_utf8(text.clone()).unwrap(),
            "  /p/\nM - src/\nM     main.rs\n *  README"
        );
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].depth, 1);
        assert_eq!(lines[1].name_column(), 6);
        assert!(spans.contains(&(6..7, Face::GitModified)));
        tree.show_ignored = true;
        let (text, _, _) = tree_text(&tree, &changed);
        assert!(String::from_utf8(text).unwrap().contains("\n! + target/"));
    }

    #[test]
    fn line_offsets() {
        let text = b"ab\ncd\n\nef";
        assert_eq!(line_offset(text, 0), 0);
        assert_eq!(line_offset(text, 1), 3);
        assert_eq!(line_offset(text, 3), 7);
        assert_eq!(line_offset(text, 9), 9);
    }
}
