//! Opening, saving, auto-saving and exiting, against real files in temporary
//! directories. Expected messages are Emacs 29.3's, captured from
//! `emacs -nw -Q` in tmux.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use emx_core::io::{
    CodingMnemonic, ExistingFile, LoadError, SavePlan, TargetInfo, load_file, plan_save,
    probe_target,
};
use emx_core::testing::{Harness, fail_after};
use emx_core::{Config, Effect, Event, FileBinding};
use emx_text::{FileIdentity, Original, Sources, Text};
use proptest::prelude::*;

const ENOSPC_MESSAGE: &str = "No space left on device";

fn harness_in(directory: &Path) -> Harness {
    harness_with(Config::default(), directory)
}

fn harness_with(config: Config, directory: &Path) -> Harness {
    let mut harness = Harness::with_config(config);
    harness.set_default_directory(directory);
    harness
}

/// `C-x C-f`, replace the default directory with `path`, `RET`.
fn visit(harness: &mut Harness, path: &Path) {
    harness.keys("C-x C-f C-a C-k");
    harness.type_text(path.to_str().expect("test paths are UTF-8"));
    harness.keys("RET");
}

fn modeline(harness: &Harness) -> String {
    let screen = harness.screen();
    screen[screen.len() - 2].clone()
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("stat").permissions().mode() & 0o7777
}

fn write_file(path: &Path, content: &[u8]) {
    fs::write(path, content).expect("write fixture");
}

fn temp_entries(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(directory)
        .expect("list directory")
        .map(|entry| {
            entry
                .expect("directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

fn assert_not_root() {
    assert_ne!(
        rustix::process::geteuid().as_raw(),
        0,
        "permission tests need a non-root user: root bypasses file modes"
    );
}

#[test]
fn open_small_reads_memory() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"hello\nworld\n");
    let loaded = load_file(&path, &Config::default()).expect("load");
    assert!(matches!(loaded.sources.original, Original::Memory(_)));
    assert!(loaded.sources.index.is_complete());
    assert!(!loaded.needs_index);
    assert_eq!(loaded.coding, CodingMnemonic::Undecided);
    assert_eq!(
        loaded.identity,
        Some(FileIdentity::of_path(&path).expect("stat"))
    );

    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    assert_eq!(harness.text(), "hello\nworld\n");
    assert_eq!(harness.buffer().name, "a.txt");
    assert!(!harness.buffer().is_modified());
    assert_eq!(harness.echo(), "");
    assert!(
        modeline(&harness).starts_with("-UU-:---  F1  a.txt"),
        "{}",
        modeline(&harness)
    );
    assert!(harness.take_index_requests().is_empty());
}

#[test]
fn open_large_uses_block_cache() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("big.log");
    let content: Vec<u8> = (0..4096u32)
        .map(|index| if index % 64 == 63 { b'\n' } else { b'x' })
        .collect();
    write_file(&path, &content);
    let config = Config {
        in_memory_max: 1024,
        ..Config::default()
    };
    let loaded = load_file(&path, &config).expect("load");
    assert!(matches!(loaded.sources.original, Original::File(_)));
    assert!(!loaded.sources.index.is_complete());
    assert!(loaded.needs_index);

    let mut harness = harness_with(config, directory.path());
    visit(&mut harness, &path);
    assert_eq!(harness.text().as_bytes(), content.as_slice());
    let buffer = harness.buffer().id;
    assert_eq!(harness.take_index_requests(), vec![buffer]);
    assert!(matches!(
        harness.buffer().text.sources().original,
        Original::File(_)
    ));
}

#[test]
fn coding_mnemonic_follows_content_like_emacs() {
    let directory = tempfile::tempdir().expect("tempdir");
    let ascii = directory.path().join("ascii.txt");
    let utf8 = directory.path().join("utf.txt");
    write_file(&ascii, b"hello ascii\n");
    write_file(&utf8, "caf\u{e9}\n".as_bytes());
    let mut harness = harness_in(directory.path());
    assert!(modeline(&harness).starts_with("-UUU:---  F1  *scratch*"));
    visit(&mut harness, &utf8);
    assert!(
        modeline(&harness).starts_with("-UUU:---  F1  utf.txt"),
        "{}",
        modeline(&harness)
    );
    visit(&mut harness, &ascii);
    assert!(modeline(&harness).starts_with("-UU-:---  F1  ascii.txt"));
    // Saving ASCII keeps the coding undecided; saving a non-ASCII
    // character makes it UTF-8, as `emacs -nw` shows.
    harness.keys("x C-x C-s");
    assert!(modeline(&harness).starts_with("-UU-:---"));
    harness.type_text("\u{e9}");
    harness.keys("C-x C-s");
    assert!(
        modeline(&harness).starts_with("-UUU:---  F1  ascii.txt"),
        "{}",
        modeline(&harness)
    );
}

#[test]
fn nonexistent_file_is_a_new_empty_buffer() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("new.txt");
    let mut harness = harness_in(directory.path());
    harness.keys("C-x C-f");
    let directory_text = format!("{}/", directory.path().display());
    assert_eq!(harness.echo_line(), format!("Find file: {directory_text}"));
    harness.type_text("new.txt");
    harness.keys("RET");
    assert_eq!(harness.echo(), "(New file)");
    assert_eq!(harness.buffer().name, "new.txt");
    assert_eq!(harness.text(), "");
    assert!(modeline(&harness).starts_with("-UUU:---  F1  new.txt"));
    assert!(!path.exists());
    // Saving an unmodified buffer whose file does not exist creates it.
    harness.keys("C-x C-s");
    assert_eq!(harness.echo(), format!("Wrote {}", path.display()));
    assert_eq!(fs::read(&path).expect("created"), b"");
    harness.keys("C-x C-s");
    assert_eq!(harness.echo(), "(No changes need to be saved)");
}

#[test]
fn directory_and_unreadable_files_are_errors() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, directory.path());
    assert_eq!(
        harness.echo(),
        format!("{} is a directory", directory.path().display())
    );
    assert!(harness.signaled());
    assert_eq!(harness.buffer().name, "*scratch*");
    assert_eq!(harness.editor().buffer_names(), vec!["*scratch*"]);
    assert!(matches!(
        load_file(directory.path(), &Config::default()),
        Err(LoadError::IsDirectory(_))
    ));

    assert_not_root();
    let unreadable = directory.path().join("secret");
    write_file(&unreadable, b"x");
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).expect("chmod");
    visit(&mut harness, &unreadable);
    assert_eq!(
        harness.echo(),
        format!("File is not readable: {}", unreadable.display())
    );
    assert_eq!(harness.editor().buffer_names(), vec!["*scratch*"]);
}

#[test]
fn visiting_a_visited_file_switches_to_its_buffer() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"abc\n");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    harness.keys("C-e x");
    visit(&mut harness, &directory.path().join("b.txt"));
    visit(&mut harness, &path);
    assert_eq!(harness.text(), "abcx\n");
    assert_eq!(harness.point(), 4, "point kept from when it was left");
    assert_eq!(
        harness.editor().buffer_names(),
        vec!["*scratch*", "a.txt", "b.txt"]
    );
}

#[test]
fn save_rename_preserves_mode_and_xattr() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"hello\n");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("chmod");
    if let Err(error) = xattr::set(&path, "user.emx-test", b"kept") {
        panic!(
            "the test directory {} must support user xattrs (tmpfs or ext4): {error}",
            directory.path().display()
        );
    }
    let before = fs::metadata(&path).expect("stat");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    harness.keys("M-> b y e");
    harness.keys("C-x C-s");
    assert_eq!(harness.echo(), format!("Wrote {}", path.display()));
    assert_eq!(fs::read(&path).expect("read"), b"hello\nbye");
    let after = fs::metadata(&path).expect("stat");
    assert_ne!(after.ino(), before.ino(), "written to a new inode");
    assert_eq!(mode(&path), 0o640);
    assert_eq!(
        xattr::get(&path, "user.emx-test").expect("read xattr"),
        Some(b"kept".to_vec())
    );
    assert!(!harness.buffer().is_modified());
    assert!(modeline(&harness).starts_with("-UU-:---"));
    let binding = harness.buffer().file.clone().expect("visiting");
    assert_eq!(
        binding.identity,
        Some(FileIdentity::of_path(&path).expect("stat"))
    );
    assert_eq!(temp_entries(directory.path()), vec!["a.txt"]);
    // The buffer no longer depends on the replaced file.
    let Original::Memory(bytes) = &harness.buffer().text.sources().original else {
        panic!("small file stays in memory");
    };
    assert_eq!(&bytes[..], b"hello\nbye");
}

#[test]
fn save_symlink_writes_target_keeps_link() {
    let directory = tempfile::tempdir().expect("tempdir");
    let target = directory.path().join("target.txt");
    let link = directory.path().join("link.txt");
    write_file(&target, b"one\n");
    std::os::unix::fs::symlink("target.txt", &link).expect("symlink");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &link);
    assert_eq!(harness.buffer().name, "link.txt");
    harness.keys("M-> t w o");
    harness.keys("C-x C-s");
    assert_eq!(harness.echo(), format!("Wrote {}", link.display()));
    assert!(
        fs::symlink_metadata(&link)
            .expect("lstat")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_link(&link).expect("readlink"),
        Path::new("target.txt")
    );
    assert_eq!(fs::read(&target).expect("read"), b"one\ntwo");
    assert_eq!(harness.buffer().file.as_ref().expect("visiting").path, link);
}

#[test]
fn save_hardlinked_uses_in_place() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    let other = directory.path().join("other-name.txt");
    write_file(&path, b"0123456789\n");
    fs::hard_link(&path, &other).expect("hard link");
    let inode = fs::metadata(&path).expect("stat").ino();
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    harness.keys("C-f C-f C-d C-d A B C");
    harness.hold_saves();
    harness.keys("C-x C-s");
    let held = harness.held_saves();
    assert_eq!(held.len(), 1);
    assert!(matches!(held[0], SavePlan::InPlace { .. }), "{:?}", held[0]);
    harness.release_saves();
    assert_eq!(harness.echo(), format!("Wrote {}", path.display()));
    assert_eq!(fs::read(&path).expect("read"), b"01ABC456789\n");
    assert_eq!(fs::read(&other).expect("read"), b"01ABC456789\n");
    assert_eq!(fs::metadata(&path).expect("stat").ino(), inode);
    assert_eq!(fs::metadata(&path).expect("stat").nlink(), 2);
    assert!(!harness.buffer().is_modified());
}

#[test]
fn in_place_save_of_a_block_cache_buffer_reuses_the_file() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("big.log");
    let content: Vec<u8> = (0..200_000u32)
        .map(|index| b'a' + (index % 26) as u8)
        .collect();
    write_file(&path, &content);
    let other = directory.path().join("link.log");
    fs::hard_link(&path, &other).expect("hard link");
    let config = Config {
        in_memory_max: 1024,
        ..Config::default()
    };
    let mut harness = harness_with(config, directory.path());
    visit(&mut harness, &path);
    harness.keys("C-f C-f C-f X Y");
    harness.keys("C-x C-s");
    assert_eq!(harness.echo(), format!("Wrote {}", path.display()));
    let mut expected = content.clone();
    expected.splice(3..3, *b"XY");
    assert_eq!(fs::read(&path).expect("read"), expected);
    assert_eq!(harness.text().as_bytes(), expected.as_slice());
    assert_eq!(harness.buffer().text.piece_count(), 1, "one original piece");
    // The line index is rebuilt for the rewritten file.
    let buffer = harness.buffer().id;
    assert_eq!(harness.take_index_requests(), vec![buffer, buffer]);
}

#[test]
fn plan_save_decides_rename_or_in_place() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"abc\n");
    let identity = FileIdentity::of_path(&path).expect("stat");
    let text = Text::from_original(Sources::memory(Arc::from(&b"abc\n"[..])));
    let binding = FileBinding {
        path: path.clone(),
        identity: Some(identity),
        saving: false,
    };
    const USER: u32 = 1000;
    const OTHER: u32 = 1001;
    const ROOT: u32 = 0;
    let target = |nlink: u64, uid: u32, gid: u32, directory_writable: bool| TargetInfo {
        target: path.clone(),
        existing: Some(ExistingFile {
            identity,
            nlink,
            uid,
            gid,
            mode: 0o644,
            writable: true,
        }),
        directory_exists: true,
        directory_writable,
        new_file_gid: USER,
    };
    let is_rename = |info: TargetInfo, euid: u32| {
        matches!(
            plan_save(&binding, &text, &info, euid),
            SavePlan::Rename { .. }
        )
    };
    assert!(is_rename(target(1, USER, USER, true), USER));
    assert!(!is_rename(target(2, USER, USER, true), USER), "hardlinked");
    assert!(!is_rename(target(1, OTHER, USER, true), USER), "not owner");
    assert!(
        !is_rename(target(1, USER, OTHER, true), USER),
        "other group"
    );
    assert!(
        !is_rename(target(1, USER, USER, false), USER),
        "read-only dir"
    );
    assert!(
        is_rename(target(1, OTHER, OTHER, true), ROOT),
        "root chowns"
    );
    assert!(
        !is_rename(target(2, OTHER, OTHER, true), ROOT),
        "root, linked"
    );
    let new_file = TargetInfo {
        existing: None,
        ..target(1, USER, USER, true)
    };
    assert!(is_rename(new_file, USER));

    let probed = probe_target(&path).expect("probe");
    assert_eq!(probed.target, path);
    let existing = probed.existing.expect("exists");
    assert_eq!(existing.identity, identity);
    assert_eq!(existing.nlink, 1);
    assert!(existing.writable);
    assert!(probed.directory_writable);
}

#[test]
fn save_enospc_leaves_original_intact() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    let original = b"the original content\n".repeat(100);
    write_file(&path, &original);
    let identity = FileIdentity::of_path(&path).expect("stat");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    harness.keys("M-> m o r e");
    harness.set_save_sink(fail_after(1000));
    harness.keys("C-x C-s");
    assert_eq!(
        harness.echo(),
        format!("Write error: {ENOSPC_MESSAGE}, {}", path.display())
    );
    assert!(harness.signaled());
    assert_eq!(fs::read(&path).expect("read"), original);
    assert_eq!(FileIdentity::of_path(&path).expect("stat"), identity);
    assert!(harness.buffer().is_modified());
    assert_eq!(
        temp_entries(directory.path()),
        vec!["a.txt"],
        "temp removed"
    );
    assert!(!harness.buffer().file.as_ref().expect("visiting").saving);
    // The buffer is editable again and a later save succeeds.
    harness.clear_save_sink();
    harness.keys("!");
    harness.keys("C-x C-s");
    assert_eq!(harness.echo(), format!("Wrote {}", path.display()));
    let mut expected = original.clone();
    expected.extend_from_slice(b"more!");
    assert_eq!(fs::read(&path).expect("read"), expected);
}

#[test]
fn in_place_failure_keeps_buffer_modified() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"abcdef\n");
    fs::hard_link(&path, directory.path().join("b.txt")).expect("hard link");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    harness.keys("M-> x y z");
    harness.set_save_sink(fail_after(2));
    harness.keys("C-x C-s");
    assert_eq!(
        harness.echo(),
        format!(
            "Write error: {ENOSPC_MESSAGE}, {}; {} is left partly rewritten",
            path.display(),
            path.display()
        )
    );
    assert!(harness.buffer().is_modified());
    assert_eq!(harness.text(), "abcdef\nxyz");
}

#[test]
fn a_file_changed_after_planning_is_never_overwritten() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"planned against\n");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    harness.keys("x");
    harness.hold_saves();
    harness.keys("C-x C-s");
    let plan = harness.held_saves().remove(0);
    let SavePlan::Rename { text, .. } = &plan else {
        panic!("a plain file is renamed over: {plan:?}");
    };
    // An agent rewrites the file between the check and the write.
    write_file(&path, b"agent wrote this\n");
    let error = emx_core::io::save(&plan).expect_err("changed file refused");
    assert_eq!(
        error.to_string(),
        format!(
            "{} changed on disk while it was being saved; not saved",
            path.display()
        )
    );
    assert_eq!(fs::read(&path).expect("read"), b"agent wrote this\n");
    assert_eq!(temp_entries(directory.path()), vec!["a.txt"]);

    let info = probe_target(&path).expect("probe");
    let binding = harness.buffer().file.clone().expect("visiting");
    let in_place = emx_core::io::in_place_plan(&binding, text, &info);
    write_file(&path, b"and again, longer than before\n");
    assert!(matches!(
        emx_core::io::save(&in_place),
        Err(emx_core::io::SaveError::Changed(_))
    ));
    assert_eq!(
        fs::read(&path).expect("read"),
        b"and again, longer than before\n"
    );
    // The editor reports the refusal and keeps the buffer modified.
    harness.release_saves();
    assert_eq!(harness.echo(), error.to_string());
    assert!(harness.buffer().is_modified());
}

#[test]
fn save_permission_denied_keeps_buffer_modified() {
    assert_not_root();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("ro.txt");
    write_file(&path, b"r\n");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).expect("chmod");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    assert_eq!(harness.echo(), "Note: file is write protected");
    assert!(modeline(&harness).starts_with("-UU-:%%-"));
    harness.keys("x");
    assert_eq!(harness.echo(), "Buffer is read-only: #<buffer ro.txt>");
    harness.keys("C-x C-q x");
    assert!(harness.buffer().is_modified());
    harness.keys("C-x C-s");
    assert_eq!(
        harness.echo_line(),
        "File ro.txt is write-protected; try to save anyway? (yes or no) "
    );
    harness.type_text("no");
    harness.keys("RET");
    assert_eq!(
        harness.echo(),
        "Attempt to save to a file that you aren\u{2019}t allowed to write"
    );
    assert!(harness.buffer().is_modified());
    assert_eq!(fs::read(&path).expect("read"), b"r\n");

    // With the directory read-only too the save must be in place, which
    // the file's mode forbids.
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o555)).expect("chmod");
    harness.keys("C-x C-s");
    harness.type_text("yes");
    harness.keys("RET");
    let echo = harness.echo();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).expect("chmod");
    assert_eq!(
        echo,
        format!("Opening output file: Permission denied, {}", path.display())
    );
    assert!(harness.buffer().is_modified());
    assert_eq!(fs::read(&path).expect("read"), b"r\n");
}

#[test]
fn new_file_in_read_only_directory_is_reported() {
    assert_not_root();
    let directory = tempfile::tempdir().expect("tempdir");
    let locked = directory.path().join("locked");
    fs::create_dir(&locked).expect("mkdir");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).expect("chmod");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &locked.join("new.txt"));
    let visit_echo = harness.echo();
    harness.keys("C-x C-s");
    let save_echo = harness.echo();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("chmod");
    assert_eq!(visit_echo, "File not found and directory write-protected");
    assert_eq!(
        save_echo,
        format!("Directory {}/ write-protected", locked.display())
    );
}

#[test]
fn edits_rejected_while_saving() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"abc\n");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    harness.keys("x");
    harness.hold_saves();
    harness.keys("C-x C-s");
    assert_eq!(harness.echo(), format!("Saving file {}...", path.display()));
    harness.keys("y");
    assert_eq!(harness.echo(), "Buffer is being saved");
    assert!(harness.signaled());
    harness.keys("C-d");
    assert_eq!(harness.echo(), "Buffer is being saved");
    harness.keys("C-f");
    assert_eq!(harness.point(), 2, "motion still works");
    harness.release_saves();
    assert_eq!(harness.echo(), format!("Wrote {}", path.display()));
    assert_eq!(fs::read(&path).expect("read"), b"xabc\n");
    harness.keys("y");
    assert_eq!(harness.text(), "xaybc\n");
}

#[test]
fn save_over_newer_disk_asks() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"mine\n");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    harness.keys("x");
    write_file(&path, b"an agent rewrote this\n");
    harness.keys("C-x C-s");
    assert_eq!(
        harness.echo_line(),
        "a.txt has changed since visited or saved.  Save anyway? (yes or no) "
    );
    harness.type_text("maybe");
    harness.keys("RET");
    assert_eq!(harness.echo_line(), "Please answer yes or no.");
    harness.fire_timers();
    assert_eq!(
        harness.echo_line(),
        "a.txt has changed since visited or saved.  Save anyway? (yes or no) "
    );
    harness.type_text("no");
    harness.keys("RET");
    assert_eq!(harness.echo(), "Save not confirmed");
    assert_eq!(fs::read(&path).expect("read"), b"an agent rewrote this\n");
    assert!(harness.buffer().is_modified());
    harness.keys("C-x C-s");
    harness.type_text("YES");
    harness.keys("RET");
    assert_eq!(harness.echo(), format!("Wrote {}", path.display()));
    assert_eq!(fs::read(&path).expect("read"), b"xmine\n");
}

#[test]
fn large_save_offers_in_place() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, &[b'z'; 3000]);
    let config = Config {
        large_save_warn: 2048,
        ..Config::default()
    };
    let inode = fs::metadata(&path).expect("stat").ino();
    let mut harness = harness_with(config, directory.path());
    visit(&mut harness, &path);
    harness.keys("a C-x C-s");
    assert_eq!(
        harness.echo_line(),
        "Saving a.txt rewrites 2.9k and needs 2.9k free. (r)ewrite safely, (i)n place, (c)ancel? "
    );
    harness.keys("c");
    assert_eq!(harness.echo(), "Canceled");
    assert!(harness.buffer().is_modified());
    harness.keys("C-x C-s i");
    assert_eq!(harness.echo(), format!("Wrote {}", path.display()));
    assert_eq!(fs::metadata(&path).expect("stat").ino(), inode, "in place");
    harness.keys("b C-x C-s r");
    assert_ne!(fs::metadata(&path).expect("stat").ino(), inode, "renamed");
    assert_eq!(fs::read(&path).expect("read").len(), 3002);
}

#[test]
fn autosave_written_and_removed() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    let auto_save = directory.path().join("#a.txt#");
    write_file(&path, b"a\n");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("chmod");
    let config = Config {
        auto_save_interval_keys: 5,
        ..Config::default()
    };
    let mut harness = harness_with(config, directory.path());
    visit(&mut harness, &path);
    harness.keys("1 2 3 4 5");
    assert!(
        !auto_save.exists(),
        "Emacs auto-saves after more than the interval"
    );
    harness.keys("6");
    assert_eq!(fs::read(&auto_save).expect("auto-saved"), b"123456a\n");
    assert_eq!(mode(&auto_save), 0o640 | 0o600);
    assert_eq!(harness.echo(), "Auto-saving...done");
    assert_eq!(
        fs::read(&path).expect("read"),
        b"a\n",
        "the file is untouched"
    );
    harness.keys("C-x C-s");
    assert!(!auto_save.exists(), "removed by the save");
    assert_eq!(fs::read(&path).expect("read"), b"123456a\n");

    // Idle auto-save.
    harness.keys("7");
    harness.fire_timers();
    harness.fire_timers();
    assert_eq!(fs::read(&auto_save).expect("auto-saved"), b"1234567a\n");
}

#[test]
fn autosave_from_another_session_is_kept_and_recoverable() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    let auto_save = directory.path().join("#a.txt#");
    write_file(&path, b"old\n");
    write_file(&auto_save, b"recovered work\n");
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    fs::File::options()
        .write(true)
        .open(&path)
        .expect("open")
        .set_modified(past)
        .expect("set mtime");
    let config = Config {
        keys: [("C-c r".to_string(), "recover-this-file".to_string())].into(),
        ..Config::default()
    };
    let mut harness = harness_with(config, directory.path());
    visit(&mut harness, &path);
    assert_eq!(
        harness.echo(),
        "a.txt has auto save data; consider M-x recover-this-file"
    );
    harness.keys("C-c r");
    assert_eq!(
        harness.echo_line(),
        format!(
            "Recover auto save file {}? (yes or no) ",
            auto_save.display()
        )
    );
    harness.type_text("yes");
    harness.keys("RET");
    assert_eq!(harness.text(), "recovered work\n");
    assert!(harness.buffer().is_modified());
    assert_eq!(fs::read(&path).expect("read"), b"old\n");
    harness.keys("C-/");
    assert_eq!(harness.text(), "old\n", "recovery is undoable");
    harness.keys("C-? C-x C-s");
    assert_eq!(fs::read(&path).expect("read"), b"recovered work\n");
    assert!(!auto_save.exists());
    harness.keys("C-c r");
    assert_eq!(
        harness.echo(),
        format!("Auto save file {} does not exist", auto_save.display())
    );
}

#[test]
fn recover_refuses_a_stale_auto_save() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    let auto_save = directory.path().join("#a.txt#");
    write_file(&auto_save, b"stale\n");
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    fs::File::options()
        .write(true)
        .open(&auto_save)
        .expect("open")
        .set_modified(past)
        .expect("set mtime");
    write_file(&path, b"new\n");
    let config = Config {
        keys: [("C-c r".to_string(), "recover-this-file".to_string())].into(),
        ..Config::default()
    };
    let mut harness = harness_with(config, directory.path());
    visit(&mut harness, &path);
    assert_eq!(harness.echo(), "");
    harness.keys("C-c r");
    assert_eq!(
        harness.echo(),
        format!("Auto-save file {} not current", auto_save.display())
    );
    harness.keys("x C-x C-s");
    assert!(
        auto_save.exists(),
        "an auto-save this session did not write stays"
    );
}

#[test]
fn write_file_visits_the_new_name() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    let other = directory.path().join("other.txt");
    write_file(&path, b"hello\n");
    write_file(&other, b"u\n");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    harness.keys("C-x C-w");
    assert_eq!(
        harness.echo_line(),
        format!("Write file: {}/", directory.path().display())
    );
    harness.type_text("other.txt");
    harness.keys("RET");
    assert_eq!(
        harness.echo_line(),
        format!(
            "File \u{2018}{}\u{2019} exists; overwrite? (y or n) ",
            other.display()
        )
    );
    harness.keys("x");
    assert_eq!(
        harness.echo_line(),
        format!(
            "File \u{2018}{}\u{2019} exists; overwrite? (y or n)  [Please answer y or n]",
            other.display()
        )
    );
    harness.keys("y");
    assert_eq!(harness.echo(), format!("Wrote {}", other.display()));
    assert_eq!(fs::read(&other).expect("read"), b"hello\n");
    assert_eq!(harness.buffer().name, "other.txt");
    assert_eq!(
        harness.buffer().file.as_ref().expect("visiting").path,
        other
    );
    assert_eq!(fs::read(&path).expect("read"), b"hello\n");
}

#[test]
fn save_buffer_without_file_asks_for_one() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut harness = harness_in(directory.path());
    harness.keys("h i C-x C-s");
    assert_eq!(
        harness.echo_line(),
        format!("File to save in: {}/", directory.path().display())
    );
    harness.type_text("notes.txt");
    harness.keys("RET");
    let path = directory.path().join("notes.txt");
    assert_eq!(harness.echo(), format!("Wrote {}", path.display()));
    assert_eq!(fs::read(&path).expect("read"), b"hi");
    assert_eq!(harness.buffer().name, "notes.txt");
}

#[test]
fn save_some_buffers_asks_for_each() {
    let directory = tempfile::tempdir().expect("tempdir");
    let a = directory.path().join("a.txt");
    let b = directory.path().join("b.txt");
    let c = directory.path().join("c.txt");
    for path in [&a, &b, &c] {
        write_file(path, b"0\n");
    }
    let mut harness = harness_in(directory.path());
    for path in [&a, &b, &c] {
        visit(&mut harness, path);
        harness.keys("1");
    }
    harness.keys("C-x s");
    let question =
        |path: &PathBuf| format!("Save file {}? (y, n, !, ., q, C-f or C-h) ", path.display());
    // The current buffer first, then the others most recently shown first.
    assert_eq!(harness.echo_line(), question(&c));
    harness.keys("z");
    assert_eq!(harness.echo_line(), "Type C-h for help.");
    harness.fire_timers();
    assert_eq!(harness.echo_line(), question(&c));
    harness.keys("n");
    assert_eq!(harness.echo_line(), question(&b));
    harness.keys("y");
    assert_eq!(harness.echo_line(), question(&a));
    harness.keys("q");
    assert_eq!(harness.echo(), "");
    assert_eq!(fs::read(&b).expect("read"), b"10\n");
    assert_eq!(fs::read(&a).expect("read"), b"0\n");
    assert_eq!(fs::read(&c).expect("read"), b"0\n");
    harness.keys("C-x s !");
    assert_eq!(fs::read(&a).expect("read"), b"10\n");
    assert_eq!(fs::read(&c).expect("read"), b"10\n");
    assert_eq!(harness.echo(), "");
    harness.keys("C-x s");
    assert_eq!(harness.echo(), "(No files need saving)");
}

#[test]
fn kill_buffer_asks_when_modified() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"a\n");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    harness.keys("C-x k");
    assert_eq!(harness.echo_line(), "Kill buffer (default a.txt): ");
    harness.keys("RET");
    assert_eq!(harness.editor().buffer_names(), vec!["*scratch*"]);
    assert_eq!(harness.buffer().name, "*scratch*");

    visit(&mut harness, &path);
    harness.keys("1 C-x k RET");
    let question = "Buffer a.txt modified; kill anyway? (yes/no/save and then kill) ";
    assert_eq!(harness.echo_line(), question);
    harness.type_text("x");
    harness.keys("RET");
    assert_eq!(harness.echo_line(), format!("{question}x [No match]"));
    harness.keys("DEL");
    harness.type_text("no");
    harness.keys("RET");
    assert_eq!(harness.echo(), "");
    assert_eq!(harness.buffer().name, "a.txt");
    harness.keys("C-x k RET");
    harness.type_text("save and then kill");
    harness.keys("RET");
    assert_eq!(fs::read(&path).expect("read"), b"1a\n");
    assert_eq!(harness.editor().buffer_names(), vec!["*scratch*"]);
    visit(&mut harness, &path);
    harness.keys("2 C-x k RET");
    harness.type_text("yes");
    harness.keys("RET");
    assert_eq!(harness.editor().buffer_names(), vec!["*scratch*"]);
    assert_eq!(fs::read(&path).expect("read"), b"1a\n");
    harness.keys("C-x k");
    harness.type_text("nonesuch");
    harness.keys("RET");
    assert_eq!(
        harness.echo_line(),
        "Kill buffer (default *scratch*): nonesuch [No match]"
    );
}

#[test]
fn c_x_c_c_noserver_prompts_when_modified() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"a\n");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    harness.keys("1 C-x C-c");
    assert_eq!(
        harness.echo_line(),
        format!("Save file {}? (y, n, !, ., q, C-f or C-h) ", path.display())
    );
    harness.keys("n");
    assert_eq!(
        harness.echo_line(),
        "Modified buffers exist; exit anyway? (yes or no) "
    );
    harness.type_text("no");
    harness.keys("RET");
    assert!(!harness.shutdown_requested());
    assert_eq!(harness.echo(), "");
    harness.keys("C-x C-c n C-g");
    assert_eq!(harness.echo(), "Quit");
    assert!(!harness.shutdown_requested());
    harness.keys("C-x C-c q");
    harness.type_text("yes");
    harness.keys("RET");
    assert!(harness.shutdown_requested());
    assert_eq!(
        fs::read(&path).expect("read"),
        b"a\n",
        "exit without saving"
    );
}

#[test]
fn c_x_c_c_saves_then_exits() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"a\n");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    harness.keys("1 C-x C-c y");
    assert!(harness.shutdown_requested());
    assert_eq!(fs::read(&path).expect("read"), b"1a\n");

    let mut clean = harness_in(directory.path());
    clean.keys("x C-x C-c");
    assert!(clean.shutdown_requested(), "scratch edits do not count");
}

#[test]
fn c_x_c_c_does_not_exit_when_a_save_fails() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"a\n");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    harness.set_save_sink(fail_after(0));
    harness.keys("1 C-x C-c y");
    assert!(!harness.shutdown_requested());
    assert_eq!(
        harness.echo(),
        format!("Write error: {ENOSPC_MESSAGE}, {}", path.display())
    );
    assert_eq!(fs::read(&path).expect("read"), b"a\n");
}

#[test]
fn server_edit_without_clients_reports() {
    let mut harness = Harness::new();
    harness.keys("C-x #");
    assert_eq!(harness.echo(), "No server editing buffers exist");
}

#[test]
fn save_with_no_clients_rearms_the_daemon_grace_timer() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"a\n");
    let mut editor = emx_core::Editor::new(Config::default()).expect("valid config");
    editor.set_default_directory(directory.path());
    let client = emx_core::ClientId(1);
    editor.handle(Event::ClientAttached {
        client,
        cols: 80,
        rows: 24,
        kind: emx_proto::ClientKind::Tui,
    });
    let keys = |editor: &mut emx_core::Editor, kbd: &str| {
        for key in emx_types::parse_kbd(kbd).expect("kbd") {
            editor.handle(Event::Key(client, key));
        }
    };
    keys(&mut editor, "C-x C-f C-a C-k");
    for character in path.to_str().expect("utf-8").chars() {
        editor.handle(Event::Key(
            client,
            emx_types::Key {
                code: emx_types::KeyCode::Char(character),
                mods: emx_types::Mods::default(),
            },
        ));
    }
    keys(&mut editor, "RET");
    let (buffer, load_path) = editor
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Load { buffer, path } => Some((buffer, path)),
            _ => None,
        })
        .expect("load requested");
    let loaded = load_file(&load_path, editor.config()).map_err(|error| error.to_string());
    editor.handle(Event::FileLoaded {
        buffer,
        result: loaded,
    });
    keys(&mut editor, "x C-x C-s");
    let plan = editor
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Save { plan, .. } => Some(plan),
            _ => None,
        })
        .expect("save requested");
    editor.handle(Event::ClientDetached(client));
    assert!(
        !editor
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::SetTimer { .. })),
        "an unsaved buffer keeps the daemon"
    );
    let result = emx_core::io::save(&plan).map_err(|error| error.to_string());
    editor.handle(Event::SaveFinished { buffer, result });
    let timers: Vec<emx_core::TimerId> = editor
        .take_effects()
        .iter()
        .filter_map(|effect| match effect {
            Effect::SetTimer { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    assert!(!timers.is_empty(), "grace timer armed after the save");
    let mut shutdown = false;
    for timer in timers {
        editor.handle(Event::Timer(timer));
        shutdown |= editor.take_effects().contains(&Effect::Shutdown);
    }
    assert!(shutdown);
}

#[test]
fn buffer_is_read_only_and_marked_while_loading() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"abc\n");
    let mut editor = emx_core::Editor::new(Config::default()).expect("valid config");
    let client = emx_core::ClientId(1);
    editor.handle(Event::ClientAttached {
        client,
        cols: 80,
        rows: 24,
        kind: emx_proto::ClientKind::Tui,
    });
    for key in emx_types::parse_kbd("C-x C-f C-a C-k").expect("kbd") {
        editor.handle(Event::Key(client, key));
    }
    for character in path.to_str().expect("utf-8").chars() {
        editor.handle(Event::Key(
            client,
            emx_types::Key {
                code: emx_types::KeyCode::Char(character),
                mods: emx_types::Mods::default(),
            },
        ));
    }
    for key in emx_types::parse_kbd("RET x").expect("kbd") {
        editor.handle(Event::Key(client, key));
    }
    let row = |editor: &emx_core::Editor, row: u16| -> String {
        let grid = emx_core::render(editor, client).0;
        grid.row(row)
            .iter()
            .map(|cell| cell.text.as_str())
            .collect()
    };
    let modeline = row(&editor, 22);
    assert!(
        modeline.starts_with("-UUU:%%-  F1  a.txt") && modeline.contains("(Fundamental Loading)"),
        "{modeline}"
    );
    assert_eq!(
        row(&editor, 23).trim_end(),
        "Buffer is read-only: #<buffer a.txt>"
    );
    let (buffer, load_path) = editor
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Load { buffer, path } => Some((buffer, path)),
            _ => None,
        })
        .expect("load requested");
    let result = load_file(&load_path, editor.config()).map_err(|error| error.to_string());
    editor.handle(Event::FileLoaded { buffer, result });
    let modeline = row(&editor, 22);
    assert!(modeline.starts_with("-UU-:---  F1  a.txt"), "{modeline}");
    assert!(modeline.contains("(Fundamental) "), "{modeline}");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Random edits on a 1 MiB file served from the block cache, saved in
    /// place, leave exactly the buffer's bytes on disk.
    #[test]
    fn in_place_save_matches_reference(edits in proptest::collection::vec(
        (any::<u32>(), 0u32..70_000, proptest::collection::vec(any::<u8>(), 0..40_000), any::<bool>()),
        1..12,
    )) {
        const FILE_LEN: usize = 1 << 20;
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("big.bin");
        let content: Vec<u8> = (0..FILE_LEN).map(|index| (index * 7 % 251) as u8).collect();
        write_file(&path, &content);
        let config = Config { in_memory_max: 1024, ..Config::default() };
        let loaded = load_file(&path, &config).expect("load");
        let identity = loaded.identity;
        let mut text = Text::from_original(loaded.sources);
        for (position, delete, insert, insert_first) in edits {
            let at = u64::from(position) % (text.len() + 1);
            let end = (at + u64::from(delete)).min(text.len());
            if insert_first {
                text = text.insert(at, &insert).expect("insert");
                let at = at + insert.len() as u64;
                let end = (at + u64::from(delete)).min(text.len());
                text = text.delete(at..end).expect("delete");
            } else {
                text = text.delete(at..end).expect("delete");
                text = text.insert(at, &insert).expect("insert");
            }
        }
        let mut expected = Vec::new();
        text.read(0..text.len(), &mut expected).expect("read reference");
        let moves = emx_core::inplace::plan_in_place(&text, FILE_LEN as u64);
        let plan = SavePlan::InPlace { target: path.clone(), text, moves, expected: identity };
        let saved = emx_core::io::save(&plan).expect("in-place save");
        let on_disk = fs::read(&path).expect("read");
        prop_assert_eq!(on_disk.len(), expected.len());
        prop_assert!(on_disk == expected, "file differs from the buffer");
        prop_assert_eq!(saved, FileIdentity::of_path(&path).expect("stat"));
    }
}
