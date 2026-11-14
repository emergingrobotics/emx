//! Live reload (DESIGN sections 1 and 3): external changes to visited files
//! are settled, classified and applied as undoable changes, or flagged as
//! conflicts; nothing the user typed is ever lost.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use emx_core::journal::{JournalOp, Record, RecoveredBuffer};
use emx_core::reload::{Classification, ExternalState, Observation, classify, observe};
use emx_core::testing::{Harness, write_and_replay};
use emx_core::{ClientId, Config, Event, FileBinding, UndoEntry};
use emx_proto::{
    ClientKind, EditorEvent, ExternalKind, Request, Response, ServerMsg, WireError, WirePath,
    error_code,
};
use emx_text::{FileIdentity, Original};
use proptest::prelude::*;

const TAIL: u64 = emx_core::config::APPEND_TAIL_HASH_BYTES;
const CONFLICT_ECHO: &str =
    "a.txt changed on disk; M-x diff-external, M-x revert-buffer, M-x keep-mine";

fn reload_keys() -> Config {
    Config {
        keys: [
            ("C-c r", "revert-buffer"),
            ("C-c k", "keep-mine"),
            ("C-c d", "diff-external"),
            ("C-c f", "follow-edit-mode"),
        ]
        .into_iter()
        .map(|(key, command)| (key.to_string(), command.to_string()))
        .collect(),
        ..Config::default()
    }
}

fn harness_with(config: Config, directory: &Path) -> Harness {
    let mut harness = Harness::with_config(config);
    harness.set_default_directory(directory);
    harness
}

fn harness_in(directory: &Path) -> Harness {
    harness_with(reload_keys(), directory)
}

fn big_file_config() -> Config {
    Config {
        in_memory_max: 1024,
        ..reload_keys()
    }
}

fn visit(harness: &mut Harness, path: &Path) {
    harness.keys("C-x C-f C-a C-k");
    harness.type_text(path.to_str().expect("test paths are UTF-8"));
    harness.keys("RET");
}

fn modeline(harness: &Harness) -> String {
    let screen = harness.screen();
    screen[screen.len() - 2].clone()
}

/// Every external write gets its own mtime, a second apart, so a
/// same-size rewrite is never hidden by a coarse clock.
fn stamp(path: &Path, serial: u64) {
    let file = OpenOptions::new().write(true).open(path).expect("open");
    let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + serial);
    file.set_modified(time).expect("set mtime");
}

fn rewrite_in_place(path: &Path, content: &[u8], serial: u64) {
    fs::write(path, content).expect("rewrite");
    stamp(path, serial);
}

fn replace_by_rename(path: &Path, content: &[u8], serial: u64) {
    let temp = path.with_extension("tmp-agent");
    fs::write(&temp, content).expect("write temp");
    stamp(&temp, serial);
    fs::rename(&temp, path).expect("rename over");
}

fn append(path: &Path, content: &[u8], serial: u64) {
    let mut file = OpenOptions::new().append(true).open(path).expect("open");
    file.write_all(content).expect("append");
    drop(file);
    stamp(path, serial);
}

/// The watcher saw the file change and it stayed put for a settle window.
fn external_change(harness: &mut Harness, path: &Path) {
    harness.handle(Event::FileChanged {
        path: path.to_path_buf(),
    });
    harness.fire_timers();
}

fn external(harness: &Harness) -> ExternalState {
    harness
        .buffer()
        .file
        .as_ref()
        .expect("visits a file")
        .external
        .clone()
}

fn entries(harness: &Harness) -> Vec<UndoEntry> {
    harness.buffer().undo.entries()
}

fn has_reload_entry(harness: &Harness) -> bool {
    entries(harness)
        .iter()
        .any(|entry| matches!(entry, UndoEntry::Reload))
}

fn binding_for(path: &Path, content: &[u8]) -> (FileBinding, FileIdentity) {
    let identity = FileIdentity::of_path(path).expect("stat");
    let mut binding = FileBinding::new(path.to_path_buf(), Some(identity));
    binding.tail_hash = emx_core::reload::tail_hash_of(content, TAIL);
    (binding, identity)
}

#[test]
fn append_detected_only_when_tail_unchanged() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    fs::write(&path, b"one\ntwo\n").expect("write");
    let (binding, old) = binding_for(&path, b"one\ntwo\n");

    append(&path, b"three\n", 1);
    let obs = observe(&path, Some(old.size), TAIL).expect("observe");
    assert_eq!(
        classify(&binding, &old, &obs),
        Classification::Appended { new_len: 14 }
    );

    // Same inode, larger, but a byte before the old end changed.
    let file = OpenOptions::new().write(true).open(&path).expect("open");
    std::os::unix::fs::FileExt::write_all_at(&file, b"O", 0).expect("poke");
    drop(file);
    stamp(&path, 2);
    let obs = observe(&path, Some(old.size), TAIL).expect("observe");
    assert_eq!(classify(&binding, &old, &obs), Classification::Replaced);

    // The editor follows an append without reading the file again.
    fs::write(&path, b"one\ntwo\n").expect("write");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    harness.keys("M->");
    append(&path, b"three\n", 3);
    external_change(&mut harness, &path);
    assert_eq!(harness.text(), "one\ntwo\nthree\n");
    assert!(!harness.buffer().is_modified());
    assert_eq!(harness.point(), 14, "point at the end follows the append");
    assert!(
        entries(&harness)
            .iter()
            .any(|entry| matches!(entry, UndoEntry::Insertion { beg: 8, end: 14 })),
        "{:?}",
        entries(&harness)
    );
    assert!(
        !entries(&harness)
            .iter()
            .any(|entry| matches!(entry, UndoEntry::ReplaceAll { .. }))
    );
    assert_eq!(external(&harness), ExternalState::Clean);
    // A second append is recognized against the new tail.
    append(&path, b"four\n", 4);
    external_change(&mut harness, &path);
    assert_eq!(harness.text(), "one\ntwo\nthree\nfour\n");
    assert_eq!(harness.point(), 19);
}

#[test]
fn append_to_file_backed_buffer_extends_the_original() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("big.log");
    let content: Vec<u8> = (0..5000u32)
        .flat_map(|line| format!("line {line}\n").into_bytes())
        .collect();
    fs::write(&path, &content).expect("write");
    let mut harness = harness_with(big_file_config(), directory.path());
    visit(&mut harness, &path);
    assert!(matches!(
        harness.buffer().text.sources().original,
        Original::File(_)
    ));
    append(&path, b"tail line\n", 1);
    external_change(&mut harness, &path);
    let mut expected = content.clone();
    expected.extend_from_slice(b"tail line\n");
    assert_eq!(harness.text().as_bytes(), expected.as_slice());
    assert_eq!(
        harness.buffer().text.piece_count(),
        2,
        "one more original piece"
    );
    assert!(!harness.buffer().is_modified());
    let Original::File(cache) = &harness.buffer().text.sources().original else {
        panic!("file-backed");
    };
    assert_eq!(
        cache.identity(),
        FileIdentity::of_path(&path).expect("stat")
    );
    harness.keys("C-/");
    assert_eq!(harness.text().as_bytes(), content.as_slice());
}

#[test]
fn same_size_rewrite_is_replaced() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    fs::write(&path, b"alpha\n").expect("write");
    stamp(&path, 1);
    let (binding, old) = binding_for(&path, b"alpha\n");
    rewrite_in_place(&path, b"omega\n", 2);
    let obs = observe(&path, Some(old.size), TAIL).expect("observe");
    let new = obs.identity.expect("exists");
    assert_eq!((new.ino, new.size), (old.ino, old.size));
    assert_eq!(classify(&binding, &old, &obs), Classification::Replaced);

    rewrite_in_place(&path, b"alpha\n", 3);
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    rewrite_in_place(&path, b"omega\n", 4);
    external_change(&mut harness, &path);
    assert_eq!(harness.text(), "omega\n");
    assert!(!harness.buffer().is_modified());
}

#[test]
fn inode_change_is_replaced() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    fs::write(&path, b"alpha\n").expect("write");
    let (binding, old) = binding_for(&path, b"alpha\n");
    // An atomic writer replaces the file with a longer one whose start is
    // the same: still not an append, because the inode changed.
    replace_by_rename(&path, b"alpha\nbeta\n", 1);
    let obs = observe(&path, Some(old.size), TAIL).expect("observe");
    assert_ne!(obs.identity.expect("exists").ino, old.ino);
    assert_eq!(classify(&binding, &old, &obs), Classification::Replaced);

    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    replace_by_rename(&path, b"alpha\nbeta\ngamma\n", 2);
    external_change(&mut harness, &path);
    assert_eq!(harness.text(), "alpha\nbeta\ngamma\n");
    assert!(has_reload_entry(&harness));
}

#[test]
fn own_save_not_reloaded() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    fs::write(&path, b"alpha\n").expect("write");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    harness.keys("x C-x C-s");
    assert_eq!(harness.echo(), format!("Wrote {}", path.display()));
    let saved = FileIdentity::of_path(&path).expect("stat");
    let binding = harness.buffer().file.clone().expect("file");
    assert_eq!(binding.own_save_identity, Some(saved));
    let obs = Observation {
        identity: Some(saved),
        tail_hash: None,
        end_hash: None,
        at: Instant::now(),
    };
    let stale = FileIdentity { size: 0, ..saved };
    assert_eq!(classify(&binding, &stale, &obs), Classification::OwnSave);

    let before = entries(&harness).len();
    external_change(&mut harness, &path);
    assert_eq!(harness.text(), "xalpha\n");
    assert_eq!(entries(&harness).len(), before, "nothing was reloaded");
    assert!(!has_reload_entry(&harness));
    assert_eq!(external(&harness), ExternalState::Clean);
}

#[test]
fn settle_waits_for_two_equal_observations() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    fs::write(&path, b"alpha\n").expect("write");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    rewrite_in_place(&path, b"half", 1);
    harness.handle(Event::FileChanged { path: path.clone() });
    assert!(matches!(external(&harness), ExternalState::Settling { .. }));
    assert_eq!(harness.text(), "alpha\n", "nothing read before it settles");
    // The writer is still going at the next look.
    rewrite_in_place(&path, b"half written, then done\n", 2);
    harness.fire_timers();
    assert!(matches!(external(&harness), ExternalState::Settling { .. }));
    assert_eq!(harness.text(), "alpha\n");
    harness.fire_timers();
    assert_eq!(harness.text(), "half written, then done\n");
    assert_eq!(external(&harness), ExternalState::Clean);
    // Exactly one reload for the whole burst.
    let reloads = entries(&harness)
        .iter()
        .filter(|entry| matches!(entry, UndoEntry::Reload))
        .count();
    assert_eq!(reloads, 1);
}

#[test]
fn unmodified_reload_preserves_point_by_line_context() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    let lines: Vec<String> = (0..10).map(|line| format!("line {line}\n")).collect();
    fs::write(&path, lines.concat()).expect("write");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    // Point on "line 5" at column 3.
    harness.keys("M-< C-n C-n C-n C-n C-n C-f C-f C-f");
    assert_eq!(harness.point(), 5 * 7 + 3);
    let mut edited = vec!["new top\n".to_string(), "another\n".to_string()];
    edited.extend(lines.iter().cloned());
    replace_by_rename(&path, edited.concat().as_bytes(), 1);
    external_change(&mut harness, &path);
    assert_eq!(
        harness.point(),
        16 + 5 * 7 + 3,
        "same line text, same column"
    );

    // The line itself changed: the same line number is kept.
    let mut rewritten = edited.clone();
    rewritten[7] = "LINE FIVE CHANGED\n".to_string();
    replace_by_rename(&path, rewritten.concat().as_bytes(), 2);
    external_change(&mut harness, &path);
    let line_start: usize = rewritten[..7].iter().map(String::len).sum();
    assert_eq!(harness.point(), line_start as u64 + 3);
}

#[test]
fn reload_keeps_the_point_and_start_of_every_window_by_line_context() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    let lines: Vec<String> = (0..40).map(|line| format!("line {line:02}\n")).collect();
    fs::write(&path, lines.concat()).expect("write");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    let line_length = 8;
    // Upper window: point on "line 05" at column 3. Lower window: scrolled
    // so it starts at "line 18", point on "line 18" at column 2.
    harness.keys("C-n C-n C-n C-n C-n C-f C-f C-f C-x 2 C-x o");
    harness.keys("M-< C-u 1 8 C-n C-f C-f C-l C-l");
    assert_eq!(harness.point(), 18 * line_length + 2);
    assert_eq!(harness.screen()[12].trim_end(), "line 18");
    // A third window shows the file only through its history.
    let other = harness.attach(ClientKind::Tui);
    harness.keys_as(other, "C-x b RET");
    harness.keys_as(other, "C-x b RET");
    let edited = format!("new top\nanother\n{}", lines.concat());
    replace_by_rename(&path, edited.as_bytes(), 1);
    external_change(&mut harness, &path);
    assert_eq!(harness.text(), edited);
    let shift = 16;
    assert_eq!(harness.point(), shift + 18 * line_length + 2);
    assert_eq!(
        harness.screen()[12].trim_end(),
        "line 18",
        "the lower window still starts on the same line"
    );
    harness.keys("C-x o");
    assert_eq!(harness.point(), shift + 5 * line_length + 3);
    assert_eq!(harness.screen()[0].trim_end(), "line 00");
    // Showing the file again in the other client's window puts point back
    // where that window left it, moved with its line.
    harness.keys_as(other, "C-x b RET");
    assert_eq!(harness.point_of(other), shift + 5 * line_length + 3);
}

#[test]
fn modified_buffer_gets_conflict_not_reload() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    fs::write(&path, b"alpha\n").expect("write");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    harness.keys("x");
    replace_by_rename(&path, b"agent wrote this\n", 1);
    external_change(&mut harness, &path);
    assert_eq!(harness.text(), "xalpha\n", "never auto-reloaded");
    assert!(harness.buffer().is_modified());
    let disk = FileIdentity::of_path(&path).expect("stat");
    assert_eq!(external(&harness), ExternalState::Conflict { disk });
    assert_eq!(harness.echo(), CONFLICT_ECHO);
    assert!(
        modeline(&harness).contains("[CONFLICT]"),
        "{}",
        modeline(&harness)
    );

    // revert-buffer asks, then takes the disk version (undoably).
    harness.keys("C-c r");
    assert_eq!(
        harness.echo_line(),
        format!("Revert buffer from file {}? (yes or no) ", path.display())
    );
    harness.type_text("yes");
    harness.keys("RET");
    assert_eq!(harness.text(), "agent wrote this\n");
    assert!(!harness.buffer().is_modified());
    assert_eq!(external(&harness), ExternalState::Clean);
    harness.keys("C-/");
    assert_eq!(
        harness.text(),
        "xalpha\n",
        "the discarded edits are in undo"
    );
}

#[test]
fn reload_is_undoable() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    fs::write(&path, b"alpha\n").expect("write");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    replace_by_rename(&path, b"beta\n", 1);
    external_change(&mut harness, &path);
    assert_eq!(harness.text(), "beta\n");
    assert!(!harness.buffer().is_modified());
    harness.keys("C-/");
    assert_eq!(harness.text(), "alpha\n");
    assert!(harness.buffer().is_modified(), "older than the file now");
    assert!(
        harness
            .buffer()
            .file
            .as_ref()
            .expect("file")
            .content_predates_disk
    );
    // Undoing the undo brings the reloaded text back, unmodified.
    harness.keys("C-g C-/");
    assert_eq!(harness.text(), "beta\n");
    assert!(!harness.buffer().is_modified());
    assert!(
        !harness
            .buffer()
            .file
            .as_ref()
            .expect("file")
            .content_predates_disk
    );
}

#[test]
fn save_after_undoing_reload_warns() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    fs::write(&path, b"alpha\n").expect("write");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    replace_by_rename(&path, b"beta\n", 1);
    external_change(&mut harness, &path);
    harness.keys("C-/ C-x C-s");
    assert_eq!(
        harness.echo_line(),
        "Saving will overwrite newer changes on disk (made after a.txt was reloaded).  \
         Save anyway? (yes or no) "
    );
    harness.type_text("no");
    harness.keys("RET");
    assert_eq!(harness.echo(), "Save not confirmed");
    assert_eq!(fs::read(&path).expect("read"), b"beta\n");
    harness.keys("C-x C-s");
    harness.type_text("yes");
    harness.keys("RET");
    assert_eq!(harness.echo(), format!("Wrote {}", path.display()));
    assert_eq!(fs::read(&path).expect("read"), b"alpha\n");
    assert!(!harness.buffer().is_modified());
}

#[test]
fn keep_mine_suppresses_warning() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    fs::write(&path, b"alpha\n").expect("write");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    replace_by_rename(&path, b"beta\n", 1);
    external_change(&mut harness, &path);
    harness.keys("C-/ C-c k C-x C-s");
    assert_eq!(harness.echo(), format!("Wrote {}", path.display()));
    assert_eq!(fs::read(&path).expect("read"), b"alpha\n");

    // A conflict kept: the save does not ask whether to overwrite.
    harness.keys("y");
    replace_by_rename(&path, b"gamma\n", 2);
    external_change(&mut harness, &path);
    assert!(matches!(external(&harness), ExternalState::Conflict { .. }));
    harness.keys("C-c k");
    let disk = FileIdentity::of_path(&path).expect("stat");
    assert_eq!(external(&harness), ExternalState::KeepMine { disk });
    assert!(!modeline(&harness).contains("[CONFLICT]"));
    harness.keys("C-x C-s");
    assert_eq!(harness.echo(), format!("Wrote {}", path.display()));
    assert_eq!(fs::read(&path).expect("read"), b"yalpha\n");
    assert_eq!(external(&harness), ExternalState::Clean);
}

#[test]
fn diff_external_shows_hunks() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    fs::write(&path, b"one\ntwo\nthree\n").expect("write");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    harness.keys("C-c d");
    assert_eq!(harness.echo(), "No external change recorded for a.txt");
    replace_by_rename(&path, b"one\nTWO\nthree\n", 1);
    external_change(&mut harness, &path);
    harness.keys("C-c d");
    assert_eq!(harness.buffer().name, "*diff-external*");
    let diff = harness.text();
    assert!(diff.contains("@@ -1,3 +1,3 @@"), "{diff}");
    assert!(diff.contains("\n-two\n+TWO\n"), "{diff}");
    assert!(harness.buffer().read_only);

    // Too large to diff: refused with a message.
    let mut small = harness_with(
        Config {
            in_memory_max: 4,
            ..reload_keys()
        },
        directory.path(),
    );
    visit(&mut small, &path);
    replace_by_rename(&path, b"one\ntwo\nfour\n", 2);
    external_change(&mut small, &path);
    small.keys("C-c d");
    assert_eq!(
        small.echo(),
        "a.txt (before reload) is too large to diff (over 4 bytes)"
    );
}

#[test]
fn deleted_flagged_and_save_recreates() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    fs::write(&path, b"alpha\n").expect("write");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    fs::remove_file(&path).expect("delete");
    external_change(&mut harness, &path);
    assert_eq!(harness.text(), "alpha\n", "the buffer is kept");
    assert_eq!(external(&harness), ExternalState::Deleted);
    assert!(
        modeline(&harness).contains("[DELETED]"),
        "{}",
        modeline(&harness)
    );
    harness.keys("C-x C-s");
    assert_eq!(harness.echo(), format!("Wrote {}", path.display()));
    assert_eq!(fs::read(&path).expect("read"), b"alpha\n");
    assert_eq!(external(&harness), ExternalState::Clean);

    // A rename away is flagged with the new name.
    harness.resize(200, 24);
    let moved = directory.path().join("b.txt");
    fs::rename(&path, &moved).expect("rename");
    harness.handle(Event::FileRenamed {
        from: path.clone(),
        to: moved.clone(),
    });
    assert_eq!(external(&harness), ExternalState::Renamed(moved.clone()));
    assert!(
        modeline(&harness).contains(&format!("[MOVED to {}]", moved.display())),
        "{}",
        modeline(&harness)
    );
}

#[test]
fn truncated_file_backed_buffer_conflicts() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("big.log");
    let content: Vec<u8> = (0..20_000u32)
        .flat_map(|line| format!("line {line}\n").into_bytes())
        .collect();
    fs::write(&path, &content).expect("write");
    let mut harness = harness_with(big_file_config(), directory.path());
    visit(&mut harness, &path);
    harness.type_text("NOTE: keep me\n");
    let inode = fs::metadata(&path).expect("stat").ino();
    // copytruncate: same inode, cut to nothing, then new lines.
    let file = OpenOptions::new().write(true).open(&path).expect("open");
    file.set_len(0).expect("truncate");
    drop(file);
    append(&path, b"fresh line\n", 1);
    assert_eq!(fs::metadata(&path).expect("stat").ino(), inode);
    // Before the change is noticed, reading the vanished bytes is an error
    // message, never a panic.
    harness.keys("M-< C-n C-n C-e");
    assert!(
        harness.echo().contains("file truncated"),
        "{}",
        harness.echo()
    );
    external_change(&mut harness, &path);
    let disk = FileIdentity::of_path(&path).expect("stat");
    assert_eq!(external(&harness), ExternalState::Conflict { disk });
    assert!(modeline(&harness).contains("[CONFLICT]"));
    assert!(
        harness
            .echo()
            .contains("the text big.log read from the file is gone"),
        "{}",
        harness.echo()
    );
    let rescued = harness
        .buffer_text("*recovered: big.log*")
        .expect("rescue buffer");
    assert!(rescued.contains("NOTE: keep me\n"), "{rescued}");
    assert!(
        rescued.contains(&format!("{} bytes", content.len())),
        "{rescued}"
    );
    // Unmodified file-backed buffers simply reload.
    let mut clean = harness_with(big_file_config(), directory.path());
    fs::write(&path, &content).expect("write");
    visit(&mut clean, &path);
    let file = OpenOptions::new().write(true).open(&path).expect("open");
    file.set_len(100).expect("truncate");
    drop(file);
    stamp(&path, 2);
    external_change(&mut clean, &path);
    assert_eq!(clean.text().as_bytes(), &content[..100]);
    assert!(!clean.buffer().is_modified());
}

/// Carry-forward: an in-place rewrite (by another writer or by emx) of a
/// file whose old bytes undo entries still reference must never make those
/// entries read the new bytes.
#[test]
fn undo_of_reload_refused_after_in_place_rewrite() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("big.log");
    let content: Vec<u8> = (0..5000u32)
        .flat_map(|line| format!("old {line:05}\n").into_bytes())
        .collect();
    fs::write(&path, &content).expect("write");
    let mut harness = harness_with(big_file_config(), directory.path());
    visit(&mut harness, &path);
    let rewritten: Vec<u8> = content
        .iter()
        .map(|byte| if *byte == b'o' { b'N' } else { *byte })
        .collect();
    rewrite_in_place(&path, &rewritten, 1);
    external_change(&mut harness, &path);
    assert_eq!(harness.text().as_bytes(), rewritten.as_slice());
    harness.keys("C-/");
    assert_eq!(
        harness.echo(),
        "Cannot undo the reload: the file was rewritten on disk and the text before it is gone"
    );
    assert_eq!(harness.text().as_bytes(), rewritten.as_slice());
}

fn big_content(prefix: &str) -> Vec<u8> {
    (0..5000u32)
        .flat_map(|line| format!("{prefix} {line:05}\n").into_bytes())
        .collect()
}

const ORIGINAL_GONE: &str =
    "Cannot undo the reload: the file was rewritten on disk and the text before it is gone";

/// Fix 1: `emx reload` reads the file again without settling; an in-place
/// rewrite it picks up must retire the old text all the same.
#[test]
fn emx_reload_after_in_place_rewrite_retires_old_text() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("big.log");
    let content = big_content("old");
    fs::write(&path, &content).expect("write");
    let mut harness = harness_with(big_file_config(), directory.path());
    visit(&mut harness, &path);
    let rewritten = big_content("NEW");
    rewrite_in_place(&path, &rewritten, 1);
    let cli = harness.attach(ClientKind::Cli);
    let id = harness.request(
        cli,
        Request::Reload {
            path: Some(WirePath::from(path.as_path())),
        },
    );
    assert_eq!(answer(&mut harness, cli, id), Ok(Response::Ok));
    assert_eq!(harness.text().as_bytes(), rewritten.as_slice());
    harness.keys("C-/");
    assert_eq!(harness.echo(), ORIGINAL_GONE);
    assert_eq!(harness.text().as_bytes(), rewritten.as_slice());
}

/// Makes the file unreadable, so looking at it fails, until `restore`.
fn unreadable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    assert_ne!(
        rustix::process::geteuid().as_raw(),
        0,
        "permission tests need a non-root user: root bypasses file modes"
    );
    fs::set_permissions(path, fs::Permissions::from_mode(0o000)).expect("chmod");
}

fn make_readable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).expect("chmod");
}

/// Fix 2: a look that fails while the file settles is retried at the next
/// settle window instead of leaving the buffer settling for ever.
#[test]
fn failed_observation_while_settling_is_retried() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    fs::write(&path, b"alpha\n").expect("write");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    replace_by_rename(&path, b"beta\n", 1);
    harness.handle(Event::FileChanged { path: path.clone() });
    assert!(matches!(external(&harness), ExternalState::Settling { .. }));
    unreadable(&path);
    harness.fire_timers();
    assert!(
        harness.echo().starts_with("Checking a.txt on disk failed"),
        "{}",
        harness.echo()
    );
    make_readable(&path);
    harness.fire_timers();
    harness.fire_timers();
    assert_eq!(harness.text(), "beta\n");
    assert_eq!(external(&harness), ExternalState::Clean);
}

/// Fix 2: a confirmed revert whose look at the file fails is reported to
/// whoever asked and forgotten, never carried out later by surprise.
#[test]
fn failed_observation_fails_a_pending_revert() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    fs::write(&path, b"alpha\n").expect("write");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    harness.keys("x");
    unreadable(&path);
    harness.keys("C-c r");
    harness.type_text("yes");
    harness.keys("RET");
    assert!(
        harness.echo().starts_with("Checking a.txt on disk failed"),
        "{}",
        harness.echo()
    );
    make_readable(&path);
    harness.handle(Event::FileChanged { path: path.clone() });
    harness.fire_timers();
    assert_eq!(harness.text(), "xalpha\n", "the failed revert stays undone");
    assert!(harness.buffer().is_modified());
}

/// Fix 3: a file-backed buffer saved after an append it has not seen yet:
/// the old bytes are intact, so the confirmed save goes ahead.
#[test]
fn save_after_unsettled_append_proceeds() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("big.log");
    let content = big_content("old");
    fs::write(&path, &content).expect("write");
    let mut harness = harness_with(big_file_config(), directory.path());
    visit(&mut harness, &path);
    harness.keys("X");
    append(&path, b"appended\n", 1);
    harness.keys("C-x C-s");
    assert_eq!(
        harness.echo_line(),
        "big.log has changed since visited or saved.  Save anyway? (yes or no) "
    );
    harness.type_text("yes");
    harness.keys("RET");
    assert_eq!(harness.echo(), format!("Wrote {}", path.display()));
    let mut expected = b"X".to_vec();
    expected.extend_from_slice(&content);
    assert_eq!(fs::read(&path).expect("read"), expected);
}

/// Fix 3: after an unsettled in-place rewrite the old bytes are gone; the
/// save is refused, the buffer conflicts, and its text is rescued.
#[test]
fn save_after_unsettled_rewrite_conflicts() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("big.log");
    let content = big_content("old");
    fs::write(&path, &content).expect("write");
    let mut harness = harness_with(big_file_config(), directory.path());
    visit(&mut harness, &path);
    harness.type_text("MINE ");
    let rewritten = big_content("NEW");
    rewrite_in_place(&path, &rewritten, 1);
    harness.keys("C-x C-s");
    harness.type_text("yes");
    harness.keys("RET");
    assert_eq!(
        harness.echo(),
        "big.log was rewritten on disk under its buffer; not saved: retry the save"
    );
    assert_eq!(
        fs::read(&path).expect("read"),
        rewritten,
        "the agent's file is intact"
    );
    let disk = FileIdentity::of_path(&path).expect("stat");
    assert_eq!(external(&harness), ExternalState::Conflict { disk });
    let rescued = harness
        .buffer_text("*recovered: big.log*")
        .expect("rescue buffer");
    assert!(rescued.contains("MINE "), "{rescued}");
}

/// Fix 4: killing a buffer whose `emx reload` is reading answers the
/// request instead of leaving the CLI waiting.
#[test]
fn killing_a_buffer_answers_its_pending_reload() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    fs::write(&path, b"alpha\n").expect("write");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    let cli = harness.attach(ClientKind::Cli);
    harness.hold_loads();
    let id = harness.request(
        cli,
        Request::Reload {
            path: Some(WirePath::from(path.as_path())),
        },
    );
    harness.keys("C-x k RET");
    assert_eq!(
        answer(&mut harness, cli, id),
        Err(WireError {
            code: error_code::RELOAD_FAILED.to_string(),
            message: "a.txt: the buffer was killed while it was read".to_string(),
        })
    );
    harness.release_loads();
    assert!(harness.take_messages(cli).is_empty());
}

/// Fix 5: an in-place save that another edit overtook still retires every
/// text that read the rewritten inode.
#[test]
fn overtaken_in_place_write_retires_old_text() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("big.log");
    let alias = directory.path().join("alias.log");
    let small = directory.path().join("small.txt");
    fs::write(&path, big_content("old")).expect("write");
    fs::hard_link(&path, &alias).expect("hard link");
    fs::hard_link(&path, directory.path().join("third.log")).expect("link");
    fs::write(&small, b"small\n").expect("write");
    let mut harness = harness_with(big_file_config(), directory.path());
    visit(&mut harness, &alias);
    let fresh: Vec<u8> = b"fresh\n".repeat(400);
    replace_by_rename(&alias, &fresh, 1);
    external_change(&mut harness, &alias);
    visit(&mut harness, &small);
    harness.keys("C-x C-w C-a C-k");
    harness.type_text(path.to_str().expect("utf-8"));
    harness.keys("RET");
    harness.hold_saves();
    harness.keys("y");
    assert_eq!(harness.held_saves().len(), 1);
    // An edit slips past the read-only guard while the save runs.
    harness.buffer_mut().file.as_mut().expect("visiting").saving = false;
    harness.keys("z");
    harness.release_saves();
    assert_eq!(fs::read(&path).expect("read"), b"small\n");
    visit(&mut harness, &alias);
    harness.keys("C-/");
    assert_eq!(harness.echo(), ORIGINAL_GONE);
    assert_eq!(harness.text().as_bytes(), fresh.as_slice());
}

#[test]
fn own_in_place_save_retires_old_text_and_updates_identity() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("big.log");
    let alias = directory.path().join("alias.log");
    let content: Vec<u8> = (0..5000u32)
        .flat_map(|line| format!("old {line:05}\n").into_bytes())
        .collect();
    fs::write(&path, &content).expect("write");
    fs::hard_link(&path, &alias).expect("hard link");
    let mut harness = harness_with(big_file_config(), directory.path());
    // The alias buffer reloads after an agent replaces its name: its undo
    // keeps the old inode's text, which the other name still visits.
    visit(&mut harness, &alias);
    let fresh: Vec<u8> = b"fresh\n".repeat(400);
    replace_by_rename(&alias, &fresh, 1);
    external_change(&mut harness, &alias);
    assert_eq!(harness.text().as_bytes(), fresh.as_slice());
    // Now big.log, the old inode's last name, is saved in place.
    fs::hard_link(&path, directory.path().join("third.log")).expect("link");
    visit(&mut harness, &path);
    harness.keys("X C-x C-s i");
    assert_eq!(harness.echo(), format!("Wrote {}", path.display()));
    let Original::File(cache) = &harness.buffer().text.sources().original else {
        panic!("file-backed");
    };
    assert_eq!(
        cache.identity(),
        FileIdentity::of_path(&path).expect("stat"),
        "the cache knows the version it reads"
    );
    // Back in the alias buffer, undoing the reload must not read the
    // rewritten inode at its old offsets.
    visit(&mut harness, &alias);
    harness.keys("C-/");
    assert_eq!(
        harness.echo(),
        "Cannot undo the reload: the file was rewritten on disk and the text before it is gone"
    );
    assert_eq!(harness.text().as_bytes(), fresh.as_slice());
}

#[test]
fn reloads_are_journaled_so_recovery_is_exact() {
    let directory = tempfile::tempdir().expect("tempdir");
    let journals = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    fs::write(&path, b"alpha\n").expect("write");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    harness.journal_ops();
    replace_by_rename(&path, b"beta\n", 1);
    external_change(&mut harness, &path);
    // The buffer is the file again: nothing to recover.
    let ops = harness.journal_ops();
    assert!(
        ops.iter().all(|op| !matches!(
            op,
            JournalOp::Record {
                record: Record::Insert { .. },
                ..
            }
        )),
        "{ops:?}"
    );
    harness.keys("C-/");
    let mut ops = harness.journal_ops();
    assert!(
        ops.iter().any(|op| matches!(
            op,
            JournalOp::Record {
                record: Record::EpochSnapshot { .. },
                ..
            }
        )),
        "undoing a reload starts an epoch: {ops:?}"
    );
    harness.keys("z");
    ops.extend(harness.journal_ops());
    let recovered =
        write_and_replay(&journals.path().join("journal"), ops, &reload_keys()).expect("replay");
    let texts: Vec<String> = recovered
        .iter()
        .filter_map(|buffer| match buffer {
            RecoveredBuffer::Rebuilt {
                text,
                path: Some(rebuilt),
                ..
            } if *rebuilt == path => {
                let mut bytes = Vec::new();
                text.read(0..text.len(), &mut bytes).expect("read");
                Some(String::from_utf8(bytes).expect("utf-8"))
            }
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["zalpha\n".to_string()]);
}

#[test]
fn follow_edit_mode_moves_to_the_first_change() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    let lines: Vec<String> = (0..60).map(|line| format!("line {line}\n")).collect();
    fs::write(&path, lines.concat()).expect("write");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    harness.keys("C-c f");
    assert_eq!(harness.echo(), "Follow-Edit mode enabled");
    let mut edited = lines.clone();
    edited[40] = "changed by the agent\n".to_string();
    replace_by_rename(&path, edited.concat().as_bytes(), 1);
    external_change(&mut harness, &path);
    let start: usize = edited[..40].iter().map(String::len).sum();
    assert_eq!(harness.point(), start as u64);
    harness.keys("C-c f");
    assert_eq!(harness.echo(), "Follow-Edit mode disabled");
}

fn answer(harness: &mut Harness, client: ClientId, id: u64) -> Result<Response, WireError> {
    harness
        .take_messages(client)
        .into_iter()
        .find_map(|message| match message {
            ServerMsg::Response {
                id: answered,
                result,
            } if answered == id => Some(result),
            _ => None,
        })
        .unwrap_or_else(|| panic!("request {id} unanswered"))
}

#[test]
fn socket_api_reload_diff_and_events_use_the_reload_path() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    fs::write(&path, b"one\ntwo\n").expect("write");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    let cli = harness.attach(ClientKind::Cli);
    let id = harness.request(cli, Request::SubscribeEvents);
    assert_eq!(answer(&mut harness, cli, id), Ok(Response::Ok));
    let wire = WirePath::from(path.as_path());
    let id = harness.request(cli, Request::DiffExternal { path: wire.clone() });
    assert_eq!(
        answer(&mut harness, cli, id),
        Err(WireError {
            code: error_code::NO_EXTERNAL_CHANGE.to_string(),
            message: "no external change recorded".to_string(),
        })
    );
    // `emx reload` goes through the same reload: undoable, point kept.
    fs::write(&path, b"zero\none\ntwo\n").expect("write");
    let id = harness.request(
        cli,
        Request::Reload {
            path: Some(wire.clone()),
        },
    );
    let messages = harness.take_messages(cli);
    let reply = messages.iter().find_map(|message| match message {
        ServerMsg::Response {
            id: answered,
            result,
        } if *answered == id => Some(result.clone()),
        _ => None,
    });
    assert_eq!(reply, Some(Ok(Response::Ok)));
    assert!(messages.iter().any(|message| matches!(
        message,
        ServerMsg::Event {
            event: EditorEvent::ExternalChange {
                kind: ExternalKind::Reloaded,
                ..
            }
        }
    )));
    assert_eq!(harness.text(), "zero\none\ntwo\n");
    assert!(has_reload_entry(&harness));
    let id = harness.request(cli, Request::DiffExternal { path: wire });
    let Ok(Response::Diff(diff)) = answer(&mut harness, cli, id) else {
        panic!("a diff");
    };
    assert!(diff.contains("+zero\n"), "{diff}");
}

#[derive(Debug, Clone)]
enum Step {
    Type { at_end: bool },
    Rewrite(Vec<u8>),
    Replace(Vec<u8>),
    Append(Vec<u8>),
    Truncate(u8),
    Delete,
    Settle,
    Undo,
    KeepMine,
    Save,
    Revert,
}

fn step() -> impl Strategy<Value = Step> {
    let content = proptest::collection::vec(prop_oneof![Just(b'a'), Just(b'\n')], 0..12);
    prop_oneof![
        3 => any::<bool>().prop_map(|at_end| Step::Type { at_end }),
        1 => content.clone().prop_map(Step::Rewrite),
        1 => content.clone().prop_map(Step::Replace),
        1 => content.prop_map(Step::Append),
        1 => any::<u8>().prop_map(Step::Truncate),
        1 => Just(Step::Delete),
        2 => Just(Step::Settle),
        2 => Just(Step::Undo),
        1 => Just(Step::KeepMine),
        1 => Just(Step::Save),
        1 => Just(Step::Revert),
    ]
}

/// Answers every yes-or-no question with yes: the user confirmed.
fn confirm(harness: &mut Harness) {
    while harness.echo_line().ends_with("(yes or no) ") {
        harness.type_text("yes");
        harness.keys("RET");
    }
}

/// The bytes of `text` that can still be read: pieces whose file was
/// rewritten on disk are skipped, the rest is kept.
fn readable(text: &emx_text::Text) -> String {
    let mut out = Vec::new();
    for (offset, piece) in text.pieces() {
        let mut bytes = Vec::new();
        if text.read(offset..offset + piece.len, &mut bytes).is_ok() {
            out.extend_from_slice(&bytes);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn undo_list_holds(harness: &Harness, token: &str) -> bool {
    entries(harness).iter().any(|entry| match entry {
        UndoEntry::Deletion { text, .. } => String::from_utf8_lossy(text).contains(token),
        UndoEntry::ReplaceAll { before, after } => [before, after]
            .iter()
            .any(|text| readable(text).contains(token)),
        _ => false,
    })
}

/// `*recovered: a.txt*` buffers hold text rescued from a buffer whose file
/// was rewritten under it.
fn rescued_holds(harness: &Harness, token: &str) -> bool {
    harness
        .editor()
        .buffer_names()
        .iter()
        .filter(|name| name.starts_with("*recovered: a.txt*"))
        .any(|name| {
            harness
                .buffer_text(name)
                .is_some_and(|text| text.contains(token))
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Goal 1: at every step, every token the user typed is in the buffer,
    /// in the undo list, or was on disk at a save the user confirmed.
    #[test]
    fn no_content_lost(steps in proptest::collection::vec(step(), 1..24)) {
        check_no_content_lost(reload_keys(), steps)?;
    }

    /// The same with every file read through the block cache, so in-place
    /// rewrites retire caches, undo refuses lost text and buffers are
    /// rescued into `*recovered: a.txt*`, which counts as a location too.
    #[test]
    fn no_content_lost_file_backed(steps in proptest::collection::vec(step(), 1..24)) {
        let config = Config {
            in_memory_max: 2,
            ..reload_keys()
        };
        check_no_content_lost(config, steps)?;
    }
}

fn check_no_content_lost(config: Config, steps: Vec<Step>) -> Result<(), TestCaseError> {
    let directory = tempfile::tempdir().expect("tempdir");
    let path: PathBuf = directory.path().join("a.txt");
    fs::write(&path, b"base\n").expect("write");
    let mut harness = harness_with(config, directory.path());
    visit(&mut harness, &path);
    let mut typed: Vec<String> = Vec::new();
    let mut saved: Vec<Vec<u8>> = Vec::new();
    for (serial, step) in steps.into_iter().enumerate() {
        let serial = serial as u64 + 1;
        match step {
            Step::Type { at_end } => {
                let token = format!("<u{serial}>");
                harness.keys(if at_end { "C-g M->" } else { "C-g M-<" });
                harness.type_text(&token);
                // Typing into text whose file vanished may fail; only
                // text the buffer took counts as typed.
                if readable(&harness.buffer().text).contains(&token) {
                    typed.push(token);
                }
            }
            Step::Rewrite(content) => {
                if path.exists() {
                    rewrite_in_place(&path, &content, serial);
                } else {
                    fs::write(&path, &content).expect("create");
                    stamp(&path, serial);
                }
                harness.handle(Event::FileChanged { path: path.clone() });
            }
            Step::Replace(content) => {
                replace_by_rename(&path, &content, serial);
                harness.handle(Event::FileChanged { path: path.clone() });
            }
            Step::Append(content) => {
                if path.exists() {
                    append(&path, &content, serial);
                    harness.handle(Event::FileChanged { path: path.clone() });
                }
            }
            Step::Truncate(length) => {
                if path.exists() {
                    let file = OpenOptions::new().write(true).open(&path).expect("open");
                    let size = file.metadata().expect("stat").len();
                    file.set_len(u64::from(length).min(size)).expect("truncate");
                    drop(file);
                    stamp(&path, serial);
                    harness.handle(Event::FileChanged { path: path.clone() });
                }
            }
            Step::Delete => {
                if path.exists() {
                    fs::remove_file(&path).expect("delete");
                    harness.handle(Event::FileChanged { path: path.clone() });
                }
            }
            Step::Settle => harness.fire_timers(),
            Step::Undo => harness.keys("C-g C-/"),
            Step::KeepMine => harness.keys("C-g C-c k"),
            Step::Save => {
                harness.keys("C-g C-x C-s");
                confirm(&mut harness);
                if harness.echo() == format!("Wrote {}", path.display()) {
                    saved.push(fs::read(&path).expect("read saved"));
                }
            }
            Step::Revert => {
                harness.keys("C-g C-c r");
                confirm(&mut harness);
                harness.fire_timers();
            }
        }
        let text = readable(&harness.buffer().text);
        for token in &typed {
            let kept = text.contains(token.as_str())
                || undo_list_holds(&harness, token)
                || rescued_holds(&harness, token)
                || saved
                    .iter()
                    .any(|content| String::from_utf8_lossy(content).contains(token.as_str()));
            prop_assert!(kept, "{token} lost; buffer {text:?}");
        }
    }
    Ok(())
}

// ---- Watching, notifications and markers (Task 15) ------------------------

#[test]
fn watches_follow_visited_files() {
    let directory = tempfile::tempdir().expect("tempdir");
    let a = directory.path().join("a.txt");
    let b = directory.path().join("b.txt");
    fs::write(&a, "a\n").expect("write");
    fs::write(&b, "b\n").expect("write");
    let mut harness = harness_in(directory.path());
    assert!(harness.watched().is_empty(), "*scratch* visits no file");
    visit(&mut harness, &a);
    visit(&mut harness, &b);
    visit(&mut harness, &a);
    assert_eq!(
        harness.watched().iter().cloned().collect::<Vec<_>>(),
        vec![a.clone(), b.clone()],
        "each path once"
    );
    harness.keys("C-x k RET");
    assert_eq!(
        harness.watched().iter().cloned().collect::<Vec<_>>(),
        vec![b.clone()],
        "the killed buffer's file is let go"
    );
}

#[test]
fn polled_file_shows_poll_badge_and_notice() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    fs::write(&path, "a\n").expect("write");
    let mut harness = harness_in(directory.path());
    harness.handle(Event::FilePolled {
        path: path.clone(),
        notice: Some("not watched yet".to_string()),
    });
    assert_eq!(harness.echo(), "", "a path not watched is ignored");
    visit(&mut harness, &path);
    assert!(
        !modeline(&harness).contains("[poll]"),
        "{}",
        modeline(&harness)
    );
    let notice = "Polling files in /x: cannot watch it (No such file or directory)";
    harness.handle(Event::FilePolled {
        path: path.clone(),
        notice: Some(notice.to_string()),
    });
    assert!(
        modeline(&harness).contains("[poll]"),
        "{}",
        modeline(&harness)
    );
    assert_eq!(harness.echo(), notice);
}

#[test]
fn ext_marker_only_on_buffers_not_shown() {
    let directory = tempfile::tempdir().expect("tempdir");
    let a = directory.path().join("a.txt");
    let b = directory.path().join("b.txt");
    fs::write(&a, "a one\n").expect("write");
    fs::write(&b, "b one\n").expect("write");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &a);
    visit(&mut harness, &b);
    rewrite_in_place(&a, b"a two\n", 1);
    rewrite_in_place(&b, b"b two\n", 2);
    harness.handle(Event::FileChanged { path: a.clone() });
    harness.handle(Event::FileChanged { path: b.clone() });
    harness.fire_timers();
    harness.fire_timers();
    assert_eq!(harness.buffer_text("a.txt").as_deref(), Some("a two\n"));
    assert_eq!(harness.buffer_text("b.txt").as_deref(), Some("b two\n"));
    assert!(!modeline(&harness).contains("[ext]"), "b.txt is shown");
    visit(&mut harness, &a);
    assert!(
        modeline(&harness).contains("[ext]"),
        "a.txt changed unseen: {}",
        modeline(&harness)
    );
    harness.keys("C-f");
    assert!(!modeline(&harness).contains("[ext]"), "seen now");
}

#[test]
fn reloads_of_one_round_are_summarized_once() {
    let directory = tempfile::tempdir().expect("tempdir");
    let names = ["c.txt", "a.txt", "b.txt"];
    let mut harness = harness_in(directory.path());
    let paths: Vec<PathBuf> = names
        .iter()
        .map(|name| {
            let path = directory.path().join(name);
            fs::write(&path, format!("{name} before\n")).expect("write");
            visit(&mut harness, &path);
            path
        })
        .collect();
    for (serial, path) in (1..).zip(&paths) {
        replace_by_rename(path, b"after\n", serial);
        harness.handle(Event::FileChanged { path: path.clone() });
    }
    // The second look settles every file, and they are reloaded; the
    // round's timer is then still to come.
    harness.fire_timers();
    assert_eq!(harness.echo(), "", "the round is still open");
    harness.fire_timers();
    assert_eq!(
        harness.echo(),
        "Reloaded 3 files changed on disk: a.txt, b.txt, c.txt"
    );
    for name in names {
        assert_eq!(harness.buffer_text(name).as_deref(), Some("after\n"));
    }
}

#[test]
fn a_single_reload_stays_silent() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    fs::write(&path, "before\n").expect("write");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    let before = harness.echo();
    replace_by_rename(&path, b"after\n", 1);
    external_change(&mut harness, &path);
    harness.fire_timers();
    harness.fire_timers();
    assert_eq!(harness.buffer_text("a.txt").as_deref(), Some("after\n"));
    assert_eq!(harness.echo(), before, "nothing new is said");
}

#[test]
fn a_new_file_buffer_shows_the_file_from_the_top_once_it_appears() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("later.txt");
    let mut harness = harness_in(directory.path());
    visit(&mut harness, &path);
    fs::write(&path, "appeared\nsecond\n").expect("create");
    external_change(&mut harness, &path);
    harness.fire_timers();
    assert_eq!(
        harness.buffer_text("later.txt").as_deref(),
        Some("appeared\nsecond\n")
    );
    let screen = harness.screen();
    assert_eq!(screen[0].trim_end(), "appeared", "{screen:?}");
    assert_eq!(harness.point(), 0);
}

#[test]
fn summary_keeps_the_conflicts_of_its_round() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut harness = harness_in(directory.path());
    let names = ["a.txt", "b.txt", "x.txt"];
    let paths: Vec<PathBuf> = names
        .iter()
        .map(|name| {
            let path = directory.path().join(name);
            fs::write(&path, format!("{name} before\n")).expect("write");
            visit(&mut harness, &path);
            path
        })
        .collect();
    // x.txt is modified and then hidden: only the echo tells of its
    // conflict.
    harness.type_text("mine ");
    visit(&mut harness, &paths[0]);
    for (serial, path) in (1..).zip(&paths) {
        replace_by_rename(path, b"after\n", serial);
        harness.handle(Event::FileChanged { path: path.clone() });
    }
    harness.fire_timers();
    harness.fire_timers();
    assert_eq!(
        harness.echo(),
        "Reloaded 2 files changed on disk: a.txt, b.txt; 1 conflict: x.txt"
    );
    assert_eq!(
        harness.buffer_text("x.txt").as_deref(),
        Some("mine x.txt before\n")
    );
}
