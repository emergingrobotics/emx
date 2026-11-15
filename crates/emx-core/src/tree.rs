//! The file system work behind the file panel (DESIGN section 6), run off
//! the editor thread: listing a directory with git's ignore rules, git
//! status, file operations, and listing a project for quick-open. Each job
//! runs on a thread of its own and reports one `Event::Tree`.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::event::{ClientId, Event, ListingId};
use crate::io::strerror;

/// The repository directory git keeps; never listed.
const GIT_DIRECTORY: &str = ".git";

/// One entry of a listed directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub name: OsString,
    pub is_dir: bool,
    /// Git's ignore rules (`.gitignore`, `.ignore`, the excludes) hide it.
    pub ignored: bool,
}

/// What git status says about a path, strongest last: a directory shows
/// the strongest of what lies beneath it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GitMark {
    Untracked,
    Added,
    Modified,
}

impl GitMark {
    pub fn letter(self) -> char {
        match self {
            GitMark::Untracked => '?',
            GitMark::Added => 'A',
            GitMark::Modified => 'M',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOp {
    CreateDirectory(PathBuf),
    Rename {
        from: PathBuf,
        to: PathBuf,
    },
    /// A file, or an empty directory.
    Delete(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeJob {
    /// Lists `dir`; with `find_root`, first goes up from `dir` to the
    /// nearest directory holding `.git`, if any, and lists that.
    List {
        dir: PathBuf,
        find_root: bool,
    },
    GitStatus {
        root: PathBuf,
    },
    Operate(FileOp),
    /// Every file under `root` that the ignore rules keep, at most `max`,
    /// relative to `root`.
    ListProject {
        id: ListingId,
        root: PathBuf,
        max: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeOutcome {
    Listed {
        dir: PathBuf,
        result: Result<Vec<TreeEntry>, String>,
    },
    GitStatus {
        root: PathBuf,
        /// Absolute paths with their marks; `Err` when `root` is in no
        /// repository or git status failed.
        result: Result<Vec<(PathBuf, GitMark)>, String>,
    },
    Operated {
        op: FileOp,
        result: Result<(), String>,
    },
    ProjectListed {
        id: ListingId,
        root: PathBuf,
        /// The files, and whether the list stopped at the cap.
        result: Result<(Vec<PathBuf>, bool), String>,
    },
}

pub fn run_tree_job(client: ClientId, job: &TreeJob) -> Event {
    let outcome = match job {
        TreeJob::List { dir, find_root } => {
            let dir = if *find_root {
                project_root(dir)
            } else {
                dir.clone()
            };
            let result = list_directory(&dir);
            TreeOutcome::Listed { dir, result }
        }
        TreeJob::GitStatus { root } => TreeOutcome::GitStatus {
            root: root.clone(),
            result: git_status(root),
        },
        TreeJob::Operate(op) => TreeOutcome::Operated {
            op: op.clone(),
            result: operate(op),
        },
        TreeJob::ListProject { id, root, max } => TreeOutcome::ProjectListed {
            id: *id,
            root: root.clone(),
            result: list_project(root, *max),
        },
    };
    Event::Tree { client, outcome }
}

/// The job could not be started (too many in flight).
pub fn refused_tree_job(client: ClientId, job: &TreeJob, reason: &str) -> Event {
    let outcome = match job {
        TreeJob::List { dir, .. } => TreeOutcome::Listed {
            dir: dir.clone(),
            result: Err(reason.to_string()),
        },
        TreeJob::GitStatus { root } => TreeOutcome::GitStatus {
            root: root.clone(),
            result: Err(reason.to_string()),
        },
        TreeJob::Operate(op) => TreeOutcome::Operated {
            op: op.clone(),
            result: Err(reason.to_string()),
        },
        TreeJob::ListProject { id, root, .. } => TreeOutcome::ProjectListed {
            id: *id,
            root: root.clone(),
            result: Err(reason.to_string()),
        },
    };
    Event::Tree { client, outcome }
}

/// The nearest directory at or above `dir` holding `.git` (a directory, or
/// the file a worktree has), else `dir` itself.
pub fn project_root(dir: &Path) -> PathBuf {
    dir.ancestors()
        .find(|ancestor| ancestor.join(GIT_DIRECTORY).exists())
        .unwrap_or(dir)
        .to_path_buf()
}

/// `dir`'s entries, `.git` left out, directories first and each group in
/// byte order. Symbolic links to directories count as directories.
pub fn list_directory(dir: &Path) -> Result<Vec<TreeEntry>, String> {
    let describe = |error: &std::io::Error| format!("{}: {}", dir.display(), strerror(error));
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir).map_err(|error| describe(&error))? {
        let entry = entry.map_err(|error| describe(&error))?;
        let name = entry.file_name();
        if name == GIT_DIRECTORY {
            continue;
        }
        let is_dir = fs::metadata(entry.path()).is_ok_and(|metadata| metadata.is_dir());
        entries.push(TreeEntry {
            name,
            is_dir,
            ignored: false,
        });
    }
    let kept = kept_names(dir);
    for entry in &mut entries {
        entry.ignored = !kept.contains(&entry.name);
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.as_encoded_bytes().cmp(b.name.as_encoded_bytes()))
    });
    Ok(entries)
}

/// The names in `dir` git's ignore rules keep, dot files included.
fn kept_names(dir: &Path) -> BTreeSet<OsString> {
    ignore::WalkBuilder::new(dir)
        .max_depth(Some(1))
        .hidden(false)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.depth() == 1)
        .map(|entry| entry.file_name().to_os_string())
        .collect()
}

fn git_status(root: &Path) -> Result<Vec<(PathBuf, GitMark)>, String> {
    use gix::status::UntrackedFiles;
    use gix::status::index_worktree::Item as WorktreeItem;

    let repository = gix::discover(root).map_err(|error| error.to_string())?;
    let workdir = repository
        .workdir()
        .ok_or_else(|| "the repository has no working tree".to_string())?
        .to_path_buf();
    let items = repository
        .status(gix::progress::Discard)
        .map_err(|error| error.to_string())?
        .untracked_files(UntrackedFiles::Files)
        .index_worktree_submodules(None)
        .into_iter(Vec::<gix::bstr::BString>::new())
        .map_err(|error| error.to_string())?;
    let mut marks = Vec::new();
    for item in items {
        let item = item.map_err(|error| error.to_string())?;
        let mark = match &item {
            gix::status::Item::TreeIndex(change) => {
                if matches!(change, gix::diff::index::Change::Addition { .. }) {
                    GitMark::Added
                } else {
                    GitMark::Modified
                }
            }
            gix::status::Item::IndexWorktree(WorktreeItem::DirectoryContents { entry, .. }) => {
                if entry.status != gix::dir::entry::Status::Untracked {
                    continue;
                }
                GitMark::Untracked
            }
            gix::status::Item::IndexWorktree(_) => GitMark::Modified,
        };
        let relative = gix::path::from_bstr(item.location())
            .map_err(|error| error.to_string())?
            .into_owned();
        marks.push((workdir.join(relative), mark));
    }
    Ok(marks)
}

fn operate(op: &FileOp) -> Result<(), String> {
    let failed = |action: &str, path: &Path, error: std::io::Error| {
        format!("{action}: {}, {}", strerror(&error), path.display())
    };
    match op {
        FileOp::CreateDirectory(path) => {
            fs::create_dir(path).map_err(|error| failed("Creating directory", path, error))
        }
        FileOp::Rename { from, to } => {
            if fs::symlink_metadata(to).is_ok() {
                return Err(format!("File exists, {}", to.display()));
            }
            fs::rename(from, to).map_err(|error| failed("Renaming", from, error))
        }
        FileOp::Delete(path) => {
            let metadata =
                fs::symlink_metadata(path).map_err(|error| failed("Removing", path, error))?;
            if metadata.is_dir() {
                fs::remove_dir(path).map_err(|error| failed("Removing directory", path, error))
            } else {
                fs::remove_file(path).map_err(|error| failed("Removing", path, error))
            }
        }
    }
}

fn list_project(root: &Path, max: usize) -> Result<(Vec<PathBuf>, bool), String> {
    let mut files = Vec::new();
    let mut capped = false;
    for entry in ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|entry| entry.file_name() != GIT_DIRECTORY)
        .build()
    {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if files.len() == max {
            capped = true;
            break;
        }
        if let Ok(relative) = entry.path().strip_prefix(root) {
            files.push(relative.to_path_buf());
        }
    }
    files.sort();
    Ok((files, capped))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(entries: &[TreeEntry]) -> Vec<(String, bool, bool)> {
        entries
            .iter()
            .map(|entry| {
                (
                    entry.name.to_string_lossy().into_owned(),
                    entry.is_dir,
                    entry.ignored,
                )
            })
            .collect()
    }

    #[test]
    fn listing_puts_directories_first_and_marks_ignored_entries() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::create_dir(root.join("target")).unwrap();
        fs::write(root.join("b.txt"), "").unwrap();
        fs::write(root.join("a.txt"), "").unwrap();
        fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        fs::write(root.join("x.log"), "").unwrap();
        assert_eq!(
            names(&list_directory(root).unwrap()),
            vec![
                ("src".into(), true, false),
                ("target".into(), true, true),
                (".gitignore".into(), false, false),
                ("a.txt".into(), false, false),
                ("b.txt".into(), false, false),
                ("x.log".into(), false, true),
            ]
        );
        assert_eq!(project_root(&root.join("src")), root);
    }

    #[test]
    fn project_listing_skips_ignored_files_and_caps() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src/deep")).unwrap();
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        fs::write(root.join("src/deep/a.rs"), "").unwrap();
        fs::write(root.join("src/b.rs"), "").unwrap();
        fs::write(root.join("x.log"), "").unwrap();
        let (files, capped) = list_project(root, 100).unwrap();
        assert!(!capped);
        assert_eq!(
            files,
            vec![
                PathBuf::from(".gitignore"),
                PathBuf::from("src/b.rs"),
                PathBuf::from("src/deep/a.rs"),
            ]
        );
        let (files, capped) = list_project(root, 2).unwrap();
        assert!(capped);
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn file_operations_report_errors_like_emacs() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        operate(&FileOp::CreateDirectory(root.join("d"))).unwrap();
        fs::write(root.join("d/f"), "").unwrap();
        let error = operate(&FileOp::Delete(root.join("d"))).unwrap_err();
        assert!(
            error.starts_with("Removing directory: Directory not empty"),
            "{error}"
        );
        fs::write(root.join("g"), "").unwrap();
        let error = operate(&FileOp::Rename {
            from: root.join("g"),
            to: root.join("d"),
        })
        .unwrap_err();
        assert!(error.starts_with("File exists"), "{error}");
        operate(&FileOp::Rename {
            from: root.join("g"),
            to: root.join("h"),
        })
        .unwrap();
        operate(&FileOp::Delete(root.join("h"))).unwrap();
        assert!(!root.join("h").exists());
    }
}
