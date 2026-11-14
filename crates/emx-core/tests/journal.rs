//! The edit journal: every edit is recorded so a crashed session server
//! loses nothing. Tests drive the editor, run its journal operations through
//! a real `JournalWriter` in a temporary directory, "crash" (drop
//! everything), replay the directory and compare with the live buffer.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use emx_core::journal::{Header, JournalError};
use emx_core::journal::{
    JournalEdit, JournalId, JournalOp, JournalWriter, Record, RecoveredBuffer, render_edits,
    replay_dir,
};
use emx_core::testing::{Harness, apply_journal, fail_after, write_and_replay, write_journal};
use emx_core::{Config, Event, Session};
use emx_text::{FileIdentity, Original};
use proptest::prelude::*;

const JOURNAL_DIRECTORY: &str = "journal";
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
const PERMISSION_BITS: u32 = 0o7777;

fn write_file(path: &Path, content: &[u8]) {
    fs::write(path, content).expect("write fixture");
}

fn harness_in(config: Config, directory: &Path) -> Harness {
    let mut harness = Harness::with_config(config);
    harness.set_default_directory(directory);
    harness
}

fn visit(harness: &mut Harness, path: &Path) {
    harness.keys("C-x C-f C-a C-k");
    harness.type_text(path.to_str().expect("test paths are UTF-8"));
    harness.keys("RET");
}

fn bytes_of(text: &emx_text::Text) -> Vec<u8> {
    let mut bytes = Vec::new();
    text.read(0..text.len(), &mut bytes).expect("read text");
    bytes
}

fn journal_files(directory: &Path) -> Vec<PathBuf> {
    match fs::read_dir(directory) {
        Ok(entries) => entries
            .map(|entry| entry.expect("directory entry").path())
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("list journal directory: {error}"),
    }
}

fn only_journal_file(directory: &Path) -> PathBuf {
    let files = journal_files(directory);
    assert_eq!(files.len(), 1, "{files:?}");
    files[0].clone()
}

/// A config whose files above 1 KiB are read through the block cache and
/// whose journal bases above 1 KiB are the file's identity.
fn identity_config() -> Config {
    Config {
        in_memory_max: 1024,
        journal_snapshot_max: 1024,
        ..Config::default()
    }
}

fn big_content() -> Vec<u8> {
    (0..4096u32)
        .map(|index| {
            if index % 64 == 63 {
                b'\n'
            } else {
                b'a' + (index % 26) as u8
            }
        })
        .collect()
}

fn rebuilt(
    recovered: &[RecoveredBuffer],
) -> (String, Option<PathBuf>, Option<FileIdentity>, Vec<u8>) {
    assert_eq!(recovered.len(), 1, "{recovered:?}");
    match &recovered[0] {
        RecoveredBuffer::Rebuilt {
            name,
            path,
            identity,
            text,
            ..
        } => (name.clone(), path.clone(), *identity, bytes_of(text)),
        other => panic!("expected a rebuilt buffer, got {other:?}"),
    }
}

#[test]
fn replay_rebuilds_edits_on_snapshot() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"hello\nworld\n");
    let mut harness = harness_in(Config::default(), directory.path());
    visit(&mut harness, &path);
    harness.type_text("abc");
    harness.keys("C-n C-d C-d DEL C-e");
    harness.paste(b"pasted\n");
    harness.keys("C-/");
    harness.type_text("z");
    let ops = harness.journal_ops();
    assert!(
        ops.iter().any(|op| matches!(
            op,
            JournalOp::Record {
                record: Record::EpochSnapshot { .. },
                ..
            }
        )),
        "a small buffer's base is a snapshot: {ops:?}"
    );
    let recovered = write_and_replay(
        &directory.path().join(JOURNAL_DIRECTORY),
        ops,
        harness.editor().config(),
    )
    .expect("replay");
    let (name, recovered_path, identity, text) = rebuilt(&recovered);
    assert_eq!(name, "a.txt");
    assert_eq!(recovered_path, Some(path));
    assert_eq!(
        identity, None,
        "a snapshot does not vouch for the disk file"
    );
    assert_eq!(text, harness.text().as_bytes());
}

#[test]
fn unmodified_buffers_write_no_journal() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"hello\n");
    let mut harness = harness_in(Config::default(), directory.path());
    visit(&mut harness, &path);
    harness.keys("C-n C-e M-<");
    assert_eq!(harness.journal_ops(), Vec::new());
}

#[test]
fn replay_identity_base_matching_disk() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("big.txt");
    write_file(&path, &big_content());
    let mut harness = harness_in(identity_config(), directory.path());
    visit(&mut harness, &path);
    harness.type_text("start");
    harness.keys("C-n C-n C-k C-k M->");
    harness.type_text("end");
    let ops = harness.journal_ops();
    let identity = FileIdentity::of_path(&path).expect("stat");
    assert!(
        ops.contains(&JournalOp::Record {
            buffer: journal_id(&ops),
            record: Record::EpochIdentity { identity },
        }),
        "a buffer above journal_snapshot_max is based on its file: {ops:?}"
    );
    let recovered = write_and_replay(
        &directory.path().join(JOURNAL_DIRECTORY),
        ops,
        harness.editor().config(),
    )
    .expect("replay");
    let (name, recovered_path, recovered_identity, text) = rebuilt(&recovered);
    assert_eq!(name, "big.txt");
    assert_eq!(recovered_path, Some(path));
    assert_eq!(recovered_identity, Some(identity));
    assert_eq!(text, harness.text().as_bytes());
    match &recovered[0] {
        RecoveredBuffer::Rebuilt { text, .. } => assert!(
            matches!(text.sources().original, Original::File(_)),
            "the large file is read through the block cache again"
        ),
        other => panic!("{other:?}"),
    }
}

fn journal_id(ops: &[JournalOp]) -> JournalId {
    match ops.first() {
        Some(JournalOp::Open { buffer, .. }) => *buffer,
        other => panic!("journal starts with Open, got {other:?}"),
    }
}

#[test]
fn replay_identity_mismatch_gives_edits_only() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("big.txt");
    write_file(&path, &big_content());
    let mut harness = harness_in(identity_config(), directory.path());
    visit(&mut harness, &path);
    harness.type_text("typed");
    harness.keys("C-n C-d C-d");
    let ops = harness.journal_ops();
    let journal = directory.path().join(JOURNAL_DIRECTORY);
    write_journal(&journal, ops).expect("write journal");
    // An agent rewrote the file after the crash.
    write_file(&path, b"something else entirely\n");
    let recovered = replay_dir(&journal, &identity_config()).expect("replay");
    assert_eq!(recovered.len(), 1);
    let RecoveredBuffer::EditsOnly {
        name,
        path: recovered_path,
        edits,
        ..
    } = &recovered[0]
    else {
        panic!("expected edits only, got {:?}", recovered[0]);
    };
    assert_eq!(name, "big.txt");
    assert_eq!(recovered_path.as_deref(), Some(path.as_path()));
    assert_eq!(
        edits,
        &vec![
            JournalEdit::Insert {
                at: 0,
                bytes: b"t".to_vec()
            },
            JournalEdit::Insert {
                at: 1,
                bytes: b"y".to_vec()
            },
            JournalEdit::Insert {
                at: 2,
                bytes: b"p".to_vec()
            },
            JournalEdit::Insert {
                at: 3,
                bytes: b"e".to_vec()
            },
            JournalEdit::Insert {
                at: 4,
                bytes: b"d".to_vec()
            },
            // Line 1 starts at 69 after "typed"; C-n keeps column 5.
            JournalEdit::Delete { at: 74, len: 1 },
            JournalEdit::Delete { at: 74, len: 1 },
        ]
    );

    // A new session shows them as a read-only buffer.
    let mut session = Harness::new();
    session.handle(Event::Recovered(recovered.clone()));
    let buffer = session
        .editor()
        .find_buffer("*recovered: big.txt*")
        .expect("recovered buffer exists");
    assert!(buffer.read_only);
    assert!(buffer.file.is_none());
    assert_eq!(
        bytes_of(&buffer.text),
        render_edits(
            "big.txt",
            "the file on disk changed after these edits were made",
            edits
        )
    );
    let rendered = String::from_utf8(bytes_of(&buffer.text)).expect("utf-8");
    assert!(
        rendered.contains("insert at 0, 1 bytes:\nt\n"),
        "{rendered}"
    );
    assert!(rendered.contains("delete at 74, 1 bytes\n"), "{rendered}");
}

#[test]
fn torn_tail_record_ignored() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"base\n");
    let mut harness = harness_in(Config::default(), directory.path());
    visit(&mut harness, &path);
    harness.type_text("ab");
    let before_last = harness.text();
    harness.type_text("c");
    let journal = directory.path().join(JOURNAL_DIRECTORY);
    write_journal(&journal, harness.journal_ops()).expect("write journal");
    let file = only_journal_file(&journal);
    let full = fs::read(&file).expect("read journal");

    // The last record ("c") was cut off part way through.
    for cut in 1..12 {
        fs::write(&file, &full[..full.len() - cut]).expect("truncate");
        let recovered = replay_dir(&journal, &Config::default()).expect("replay");
        assert_eq!(rebuilt(&recovered).3, before_last.as_bytes(), "cut {cut}");
    }

    // The last record is whole but its payload was not all written.
    let mut corrupt = full.clone();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xff;
    fs::write(&file, &corrupt).expect("corrupt");
    let recovered = replay_dir(&journal, &Config::default()).expect("replay");
    assert_eq!(rebuilt(&recovered).3, before_last.as_bytes());

    // A writer reopening the journal drops the torn tail before appending,
    // so later records are not hidden behind it.
    fs::write(&file, &full[..full.len() - 3]).expect("truncate");
    let mut writer = JournalWriter::new(&journal).expect("writer");
    let id = JournalId::from_file_name(file.file_name().expect("name")).expect("journal name");
    writer
        .apply(JournalOp::Open {
            buffer: id,
            header: emx_core::journal::Header {
                path: Some(path.clone()),
                name: "a.txt".to_string(),
            },
        })
        .expect("reopen");
    writer
        .apply(JournalOp::Record {
            buffer: id,
            record: Record::Insert {
                at: 2,
                bytes: b"Z".to_vec(),
            },
        })
        .expect("append");
    writer.flush_and_sync().expect("sync");
    let recovered = replay_dir(&journal, &Config::default()).expect("replay");
    assert_eq!(rebuilt(&recovered).3, b"abZbase\n");
}

#[test]
fn saved_buffer_journal_removed() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"base\n");
    let journal = directory.path().join(JOURNAL_DIRECTORY);
    let mut writer = JournalWriter::new(&journal).expect("writer");
    let mut harness = harness_in(Config::default(), directory.path());
    visit(&mut harness, &path);
    harness.type_text("ab");
    apply_journal(&mut writer, harness.journal_ops()).expect("write journal");
    assert_eq!(journal_files(&journal).len(), 1);
    harness.keys("C-x C-s");
    let ops = harness.journal_ops();
    let id = match ops.as_slice() {
        [
            JournalOp::Record {
                buffer,
                record: Record::Saved { identity },
            },
            JournalOp::Close { buffer: closed },
        ] => {
            assert_eq!(buffer, closed);
            assert_eq!(*identity, FileIdentity::of_path(&path).expect("stat"));
            *buffer
        }
        other => panic!("expected Saved then Close, got {other:?}"),
    };
    apply_journal(&mut writer, ops).expect("write journal");
    assert_eq!(journal_files(&journal), Vec::<PathBuf>::new());
    assert_eq!(
        replay_dir(&journal, &Config::default()).expect("replay"),
        Vec::new()
    );

    // Editing again starts a fresh journal from the saved text.
    harness.type_text("c");
    let ops = harness.journal_ops();
    assert_eq!(journal_id(&ops), id);
    apply_journal(&mut writer, ops).expect("write journal");
    let recovered = replay_dir(&journal, &Config::default()).expect("replay");
    assert_eq!(rebuilt(&recovered).3, b"abcbase\n");
}

/// Text changed while the save ran: the buffer stays modified, so the
/// journal is kept and continues from a new epoch.
#[test]
fn edits_during_a_save_start_a_new_epoch() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"a\n");
    let journal = directory.path().join(JOURNAL_DIRECTORY);
    let mut writer = JournalWriter::new(&journal).expect("writer");
    let mut harness = harness_in(Config::default(), directory.path());
    visit(&mut harness, &path);
    harness.keys("x");
    harness.hold_saves();
    harness.keys("C-x C-s");
    // Simulates an edit slipping past the read-only guard.
    harness.buffer_mut().file.as_mut().expect("visiting").saving = false;
    harness.keys("C-e z");
    apply_journal(&mut writer, harness.journal_ops()).expect("write journal");
    harness.release_saves();
    assert!(harness.buffer().is_modified());
    let ops = harness.journal_ops();
    assert!(
        matches!(
            ops.as_slice(),
            [JournalOp::Record {
                record: Record::EpochSnapshot { .. },
                ..
            }]
        ),
        "{ops:?}"
    );
    apply_journal(&mut writer, ops).expect("write journal");
    let recovered = replay_dir(&journal, &Config::default()).expect("replay");
    assert_eq!(rebuilt(&recovered).3, b"xaz\n");
}

#[test]
fn killed_buffer_journal_removed() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"base\n");
    let journal = directory.path().join(JOURNAL_DIRECTORY);
    let mut harness = harness_in(Config::default(), directory.path());
    visit(&mut harness, &path);
    harness.type_text("ab");
    harness.keys("C-x k RET");
    harness.type_text("yes");
    harness.keys("RET");
    let ops = harness.journal_ops();
    assert!(
        matches!(
            &ops[ops.len() - 2..],
            [
                JournalOp::Record {
                    record: Record::Killed,
                    ..
                },
                JournalOp::Close { .. }
            ]
        ),
        "{ops:?}"
    );
    write_journal(&journal, ops).expect("write journal");
    assert_eq!(journal_files(&journal), Vec::<PathBuf>::new());
}

#[test]
fn journal_without_its_delete_is_reported_finished() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"base\n");
    let journal = directory.path().join(JOURNAL_DIRECTORY);
    let mut harness = harness_in(Config::default(), directory.path());
    visit(&mut harness, &path);
    harness.type_text("ab");
    harness.keys("C-x C-s");
    // The crash came after Saved was written but before the delete.
    let ops: Vec<JournalOp> = harness
        .journal_ops()
        .into_iter()
        .filter(|op| !matches!(op, JournalOp::Close { .. }))
        .collect();
    let id = journal_id(&ops);
    write_journal(&journal, ops).expect("write journal");
    let recovered = replay_dir(&journal, &Config::default()).expect("replay");
    assert_eq!(recovered, vec![RecoveredBuffer::Finished { journal: id }]);
    // The new session deletes it.
    let mut session = Harness::new();
    session.handle(Event::Recovered(recovered));
    assert_eq!(session.journal_ops(), vec![JournalOp::Close { buffer: id }]);
}

/// Recovering an auto-save file replaces the whole text as a reload does;
/// it and its undo each start an epoch.
#[test]
fn reload_and_its_undo_start_epochs() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"old\n");
    write_file(&directory.path().join("#a.txt#"), b"new on disk\n");
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
    let mut harness = harness_in(config, directory.path());
    visit(&mut harness, &path);
    harness.type_text("x");
    harness.keys("C-c r");
    harness.type_text("yes");
    harness.keys("RET");
    assert_eq!(harness.text(), "new on disk\n");
    harness.keys("C-e");
    harness.type_text("!");
    let epochs = |ops: &[JournalOp]| {
        ops.iter()
            .filter(|op| {
                matches!(
                    op,
                    JournalOp::Record {
                        record: Record::EpochSnapshot { .. } | Record::EpochIdentity { .. },
                        ..
                    }
                )
            })
            .count()
    };
    let ops = harness.journal_ops();
    assert_eq!(epochs(&ops), 2, "open and reload: {ops:?}");
    let journal = directory.path().join(JOURNAL_DIRECTORY);
    let mut writer = JournalWriter::new(&journal).expect("writer");
    apply_journal(&mut writer, ops).expect("write journal");
    let recovered = replay_dir(&journal, &Config::default()).expect("replay");
    assert_eq!(rebuilt(&recovered).3, b"new on disk!\n");
    harness.keys("C-/ C-/");
    assert_eq!(harness.text(), "xold\n");
    let ops = harness.journal_ops();
    assert_eq!(epochs(&ops), 1, "undo of the reload: {ops:?}");
    apply_journal(&mut writer, ops).expect("write journal");
    let recovered = replay_dir(&journal, &Config::default()).expect("replay");
    assert_eq!(rebuilt(&recovered).3, b"xold\n");
}

#[test]
fn write_file_records_the_new_name() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    let renamed = directory.path().join("b.txt");
    write_file(&path, b"base\n");
    let mut harness = harness_in(Config::default(), directory.path());
    visit(&mut harness, &path);
    harness.type_text("ab");
    harness.hold_saves();
    harness.keys("C-x C-w C-a C-k");
    harness.type_text(renamed.to_str().expect("utf-8"));
    harness.keys("RET");
    // The save is in flight when the server crashes.
    let recovered = write_and_replay(
        &directory.path().join(JOURNAL_DIRECTORY),
        harness.journal_ops(),
        &Config::default(),
    )
    .expect("replay");
    let (name, recovered_path, _, text) = rebuilt(&recovered);
    assert_eq!(name, "b.txt");
    assert_eq!(recovered_path, Some(renamed));
    assert_eq!(text, b"abbase\n");
}

#[test]
fn recovered_buffers_come_back_modified_and_journaled() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"base\n");
    let journal = directory.path().join(JOURNAL_DIRECTORY);
    let mut harness = harness_in(Config::default(), directory.path());
    visit(&mut harness, &path);
    harness.type_text("ab");
    let recovered =
        write_and_replay(&journal, harness.journal_ops(), &Config::default()).expect("replay");
    let RecoveredBuffer::Rebuilt { journal: id, .. } = &recovered[0] else {
        panic!("{recovered:?}");
    };
    let id = *id;

    let mut session = harness_in(Config::default(), directory.path());
    session.handle(Event::Recovered(recovered));
    let buffer = session.editor().find_buffer("a.txt").expect("recovered");
    assert!(buffer.is_modified());
    assert_eq!(bytes_of(&buffer.text), b"abbase\n");
    let binding = buffer.file.as_ref().expect("visits its file");
    assert_eq!(binding.path, path);
    // The new session takes over the journal at once, so a second crash
    // before any edit still loses nothing.
    let ops = session.journal_ops();
    assert_eq!(journal_id(&ops), id);
    write_journal(&journal, ops).expect("write journal");
    let again = replay_dir(&journal, &Config::default()).expect("replay");
    assert_eq!(rebuilt(&again).3, b"abbase\n");
    // Visiting the file shows the recovered buffer, not a fresh copy.
    visit(&mut session, &path);
    assert_eq!(session.text(), "abbase\n");
}

#[test]
fn unreadable_journal_is_reported_and_kept() {
    let directory = tempfile::tempdir().expect("tempdir");
    let journal = directory.path().join(JOURNAL_DIRECTORY);
    JournalWriter::new(&journal).expect("create directory");
    let file = journal.join(format!("{:032x}.jnl", 7u128));
    write_file(&file, b"this is not a journal at all");
    let recovered = replay_dir(&journal, &Config::default()).expect("replay");
    assert_eq!(recovered.len(), 1);
    let RecoveredBuffer::Unreadable {
        file: reported,
        reason,
    } = &recovered[0]
    else {
        panic!("{recovered:?}");
    };
    assert_eq!(reported, &file);
    assert!(!reason.is_empty());
    assert!(file.exists(), "an unreadable journal is never deleted");
    let mut session = Harness::new();
    session.handle(Event::Recovered(recovered));
    assert!(
        session
            .editor()
            .buffer_names()
            .iter()
            .any(|name| name.starts_with("*recovered: ")),
        "{:?}",
        session.editor().buffer_names()
    );
    assert_eq!(session.journal_ops(), Vec::new());
}

#[test]
fn missing_journal_directory_is_empty() {
    let directory = tempfile::tempdir().expect("tempdir");
    assert_eq!(
        replay_dir(&directory.path().join("none"), &Config::default()).expect("replay"),
        Vec::new()
    );
}

#[test]
fn writer_keeps_journals_private() {
    let directory = tempfile::tempdir().expect("tempdir");
    let journal = directory.path().join("state").join(JOURNAL_DIRECTORY);
    let mut harness = Harness::new();
    harness.type_text("scratch work");
    write_journal(&journal, harness.journal_ops()).expect("write journal");
    let mode =
        |path: &Path| fs::metadata(path).expect("stat").permissions().mode() & PERMISSION_BITS;
    assert_eq!(mode(&journal), PRIVATE_DIRECTORY_MODE);
    assert_eq!(mode(&only_journal_file(&journal)), PRIVATE_FILE_MODE);
    // Buffers without a file are journaled too.
    let recovered = replay_dir(&journal, &Config::default()).expect("replay");
    let (name, path, _, text) = rebuilt(&recovered);
    assert_eq!(name, "*scratch*");
    assert_eq!(path, None);
    assert_eq!(text, b"scratch work");

    fs::set_permissions(&journal, fs::Permissions::from_mode(0o755)).expect("chmod");
    assert!(
        JournalWriter::new(&journal).is_err(),
        "a journal directory others can read is refused"
    );
}

#[test]
fn record_for_unopened_journal_is_an_error() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut writer = JournalWriter::new(&directory.path().join(JOURNAL_DIRECTORY)).expect("writer");
    let result = writer.apply(JournalOp::Record {
        buffer: JournalId(1),
        record: Record::Killed,
    });
    assert!(result.is_err());
}

#[test]
fn exiting_closes_every_journal() {
    let mut harness = Harness::new();
    harness.type_text("scratch");
    let id = journal_id(&harness.journal_ops());
    harness.keys("C-x C-c");
    assert!(harness.shutdown_requested());
    assert_eq!(harness.journal_ops(), vec![JournalOp::Close { buffer: id }]);
}

/// Goal 1: an in-place save that fails part way leaves the buffer's own
/// file corrupt; the typed text is rescued into a read-only buffer.
#[test]
fn damaged_buffer_edits_are_rescued() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("big.log");
    let content: Vec<u8> = (0..200_000u32)
        .map(|index| b'a' + (index % 26) as u8)
        .collect();
    write_file(&path, &content);
    fs::hard_link(&path, directory.path().join("alias.log")).expect("hard link");
    let mut harness = harness_in(
        Config {
            in_memory_max: 1024,
            ..Config::default()
        },
        directory.path(),
    );
    visit(&mut harness, &path);
    harness.type_text("xy");
    harness.keys("C-f C-d");
    // The server applies journal operations before it starts a save.
    let journal = directory.path().join(JOURNAL_DIRECTORY);
    let mut writer = JournalWriter::new(&journal).expect("writer");
    apply_journal(&mut writer, harness.journal_ops()).expect("write journal");
    harness.set_save_sink(fail_after(100));
    harness.keys("C-x C-s i");
    assert_eq!(
        harness.echo(),
        format!(
            "Write error: No space left on device, {}; {} is left partly rewritten; buffer big.log read that file and is damaged; its edits are in *recovered: big.log*",
            path.display(),
            path.display()
        )
    );
    let rescued = harness
        .editor()
        .find_buffer("*recovered: big.log*")
        .expect("rescue buffer");
    assert!(rescued.read_only);
    let expected = render_edits(
        "big.log",
        "an in-place save left the file partly rewritten",
        &[
            JournalEdit::Insert {
                at: 0,
                bytes: b"x".to_vec(),
            },
            JournalEdit::Insert {
                at: 1,
                bytes: b"y".to_vec(),
            },
            JournalEdit::Delete { at: 3, len: 1 },
        ],
    );
    assert_eq!(bytes_of(&rescued.text), expected);
    // The rescue is journaled at once.
    apply_journal(&mut writer, harness.journal_ops()).expect("write journal");
    let recovered = replay_dir(&journal, &Config::default()).expect("replay");
    assert!(
        recovered.iter().any(|buffer| matches!(
            buffer,
            RecoveredBuffer::Rebuilt { name, text, .. }
                if name == "*recovered: big.log*" && bytes_of(text) == expected
        )),
        "{recovered:?}"
    );
}

#[derive(Debug, Clone)]
enum Action {
    Type(String),
    Keys(&'static str),
    Save,
}

fn action() -> impl Strategy<Value = Action> {
    prop_oneof![
        4 => "[a-z ]{1,6}".prop_map(Action::Type),
        3 => prop::sample::select(vec![
            "C-d", "DEL", "C-f", "C-b", "C-n", "C-p", "M-<", "M->", "C-k", "C-y",
            "C-/", "C-u 3 C-d", "C-a", "C-e", "RET",
        ])
        .prop_map(Action::Keys),
        1 => Just(Action::Save),
    ]
}

/// What a journal file should replay to when cut at or after a record end.
#[derive(Debug, Clone, PartialEq)]
enum Expected {
    Finished,
    Text(Vec<u8>),
}

/// The state the journal describes, maintained from the operations alone.
#[derive(Debug, Default)]
struct Model {
    open: bool,
    finished: bool,
    text: Option<Vec<u8>>,
    /// End offset of each record in the journal file, with the state it
    /// leaves.
    boundaries: Vec<(u64, Expected)>,
}

impl Model {
    fn apply(&mut self, op: &JournalOp, path: &Path) {
        match op {
            JournalOp::Open { .. } => {
                if !self.open {
                    self.open = true;
                    self.finished = false;
                    self.text = None;
                    self.boundaries.clear();
                }
            }
            JournalOp::Close { .. } => {
                self.open = false;
                self.text = None;
                self.boundaries.clear();
            }
            JournalOp::Record { record, .. } => match record {
                Record::Header(_) => {}
                Record::EpochSnapshot { content } => {
                    self.text = Some(bytes_of(content));
                    self.finished = false;
                }
                Record::EpochIdentity { identity } => {
                    assert_eq!(
                        FileIdentity::of_path(path).expect("stat"),
                        *identity,
                        "an identity base is the file on disk when it is taken"
                    );
                    self.text = Some(fs::read(path).expect("read base"));
                    self.finished = false;
                }
                Record::Insert { at, bytes } => {
                    let text = self.text.as_mut().expect("edit after a base");
                    let at = usize::try_from(*at).expect("small");
                    text.splice(at..at, bytes.iter().copied());
                }
                Record::Delete { at, len } => {
                    let text = self.text.as_mut().expect("edit after a base");
                    let at = usize::try_from(*at).expect("small");
                    let len = usize::try_from(*len).expect("small");
                    text.drain(at..at + len);
                }
                Record::Saved { .. } | Record::Killed => self.finished = true,
            },
        }
    }

    fn expected(&self) -> Expected {
        match (&self.text, self.finished) {
            (Some(text), false) => Expected::Text(text.clone()),
            _ => Expected::Finished,
        }
    }

    /// The state after the last record that ends at or before `cut`.
    fn expected_at(&self, cut: u64) -> Expected {
        self.boundaries
            .iter()
            .rev()
            .find(|(end, _)| *end <= cut)
            .map_or(Expected::Finished, |(_, expected)| expected.clone())
    }
}

fn replayed(recovered: &[RecoveredBuffer]) -> Expected {
    match recovered {
        [RecoveredBuffer::Finished { .. }] => Expected::Finished,
        [RecoveredBuffer::Rebuilt { text, .. }] => Expected::Text(bytes_of(text)),
        other => panic!("unexpected replay {other:?}"),
    }
}

fn run_replay_equals_live_buffer(
    initial: Vec<u8>,
    actions: Vec<Action>,
    identity_bases: bool,
    cut: prop::sample::Index,
) -> Result<(), TestCaseError> {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    let journal = directory.path().join(JOURNAL_DIRECTORY);
    let config = if identity_bases {
        Config {
            in_memory_max: 16,
            journal_snapshot_max: 16,
            ..Config::default()
        }
    } else {
        Config::default()
    };
    write_file(&path, &initial);
    let mut harness = harness_in(config.clone(), directory.path());
    visit(&mut harness, &path);
    let mut writer = JournalWriter::new(&journal).expect("writer");
    let mut model = Model::default();
    let mut journal_file: Option<PathBuf> = None;
    for action in actions {
        match action {
            Action::Type(text) => harness.type_text(&text),
            Action::Keys(keys) => harness.keys(keys),
            Action::Save => harness.keys("C-x C-s"),
        }
        for op in harness.journal_ops() {
            model.apply(&op, &path);
            if let JournalOp::Open { buffer, .. } = &op {
                journal_file = Some(journal.join(buffer.file_name()));
            }
            let closes = matches!(op, JournalOp::Close { .. });
            writer.apply(op).expect("apply");
            if !closes {
                let file = journal_file.as_ref().expect("opened");
                let end = fs::metadata(file).expect("stat journal").len();
                model.boundaries.push((end, model.expected()));
            }
        }
        if harness.buffer().is_modified() {
            prop_assert!(model.open, "a modified buffer is journaled");
        }
        if model.open {
            prop_assert_eq!(
                model.expected(),
                Expected::Text(harness.text().into_bytes()),
                "the journal describes the live buffer"
            );
        }
    }
    writer.flush_and_sync().expect("sync");
    drop(writer);

    let recovered = replay_dir(&journal, &config).expect("replay");
    if !model.open {
        prop_assert_eq!(recovered, Vec::new());
        return Ok(());
    }
    prop_assert_eq!(
        replayed(&recovered),
        Expected::Text(harness.text().into_bytes())
    );

    let file = journal_file.expect("journal opened");
    let full = fs::read(&file).expect("read journal");
    let cut_at = cut.index(full.len() + 1);
    fs::write(&file, &full[..cut_at]).expect("truncate journal");
    let recovered = replay_dir(&journal, &config).expect("replay");
    prop_assert_eq!(replayed(&recovered), model.expected_at(cut_at as u64));
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn replay_equals_live_buffer(
        initial in "[a-z\n]{0,40}",
        mut actions in prop::collection::vec(action(), 0..24),
        identity_bases in any::<bool>(),
        cut in any::<prop::sample::Index>(),
    ) {
        // End with an edit so there is always a journal to cut.
        actions.push(Action::Type("q".to_string()));
        run_replay_equals_live_buffer(initial.into_bytes(), actions, identity_bases, cut)?;
    }
}

/// Replays a journal whose base file changed: the edits come back only as
/// a `*recovered: big.txt*` listing. Returns the replay and the journal id.
fn recover_edits_only(directory: &Path) -> (Vec<RecoveredBuffer>, JournalId) {
    let path = directory.join("big.txt");
    write_file(&path, &big_content());
    let mut harness = harness_in(identity_config(), directory);
    visit(&mut harness, &path);
    harness.type_text("typed");
    let ops = harness.journal_ops();
    let id = journal_id(&ops);
    let journal = directory.join(JOURNAL_DIRECTORY);
    // kill -9: the journal is all that is left.
    write_journal(&journal, ops).expect("write journal");
    write_file(&path, b"rewritten by an agent\n");
    let recovered = replay_dir(&journal, &identity_config()).expect("replay");
    assert!(
        matches!(recovered.as_slice(), [RecoveredBuffer::EditsOnly { .. }]),
        "{recovered:?}"
    );
    (recovered, id)
}

#[test]
fn rescued_text_keeps_the_daemon_and_its_journal() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (recovered, id) = recover_edits_only(directory.path());
    let journal = directory.path().join(JOURNAL_DIRECTORY);
    let mut writer = JournalWriter::new(&journal).expect("writer");
    let mut session = Harness::with_session(Config::default(), Session::Server);
    session.handle(Event::Recovered(recovered));
    assert!(session.editor().has_unsaved());
    let client = session.client();
    session.handle(Event::ClientDetached(client));
    session.fire_timers();
    assert!(
        !session.shutdown_requested(),
        "rescued text keeps the daemon"
    );
    apply_journal(&mut writer, session.journal_ops()).expect("write journal");
    assert_eq!(only_journal_file(&journal), journal.join(id.file_name()));
    let again = replay_dir(&journal, &Config::default()).expect("replay");
    assert_eq!(rebuilt(&again).0, "*recovered: big.txt*");
}

#[test]
fn exit_asks_about_rescued_text_and_keeps_its_journal() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (recovered, id) = recover_edits_only(directory.path());
    let journal = directory.path().join(JOURNAL_DIRECTORY);
    let mut writer = JournalWriter::new(&journal).expect("writer");
    let mut session = Harness::new();
    session.handle(Event::Recovered(recovered));
    session.keys("C-x C-c");
    assert_eq!(
        session.echo_line(),
        "Modified buffers exist; exit anyway? (yes or no) "
    );
    assert!(!session.shutdown_requested());
    session.type_text("yes");
    session.keys("RET");
    assert!(session.shutdown_requested());
    let ops = session.journal_ops();
    assert!(
        !ops.contains(&JournalOp::Close { buffer: id }),
        "the rescued text's journal is never closed at exit: {ops:?}"
    );
    apply_journal(&mut writer, ops).expect("write journal");
    assert_eq!(only_journal_file(&journal), journal.join(id.file_name()));
}

#[test]
fn killing_rescued_text_asks_then_deletes_its_journal() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (recovered, _) = recover_edits_only(directory.path());
    let journal = directory.path().join(JOURNAL_DIRECTORY);
    let mut writer = JournalWriter::new(&journal).expect("writer");
    let mut session = Harness::new();
    session.handle(Event::Recovered(recovered));
    session.keys("C-x k");
    session.type_text("*recovered: big.txt*");
    session.keys("RET");
    assert_eq!(
        session.echo_line(),
        "Buffer *recovered: big.txt* holds recovered text saved nowhere else; kill anyway? (yes or no) "
    );
    session.type_text("yes");
    session.keys("RET");
    assert!(
        session
            .editor()
            .find_buffer("*recovered: big.txt*")
            .is_none()
    );
    apply_journal(&mut writer, session.journal_ops()).expect("write journal");
    assert_eq!(journal_files(&journal), Vec::<PathBuf>::new());
}

#[test]
fn saving_rescued_text_to_a_file_ends_its_journal() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (recovered, _) = recover_edits_only(directory.path());
    let journal = directory.path().join(JOURNAL_DIRECTORY);
    let mut writer = JournalWriter::new(&journal).expect("writer");
    let mut session = harness_in(Config::default(), directory.path());
    session.handle(Event::Recovered(recovered));
    session.keys("C-x k");
    session.type_text("*scratch*");
    session.keys("RET");
    // The most recent other buffer is shown in place of the killed
    // *scratch*, as in Emacs; the recovered buffer is switched to.
    assert_eq!(session.buffer().name, "*Messages*");
    session.keys("C-x b");
    session.type_text("*recovered: big.txt*");
    session.keys("RET");
    assert_eq!(session.buffer().name, "*recovered: big.txt*");
    let saved = directory.path().join("rescued.txt");
    session.keys("C-x C-w C-a C-k");
    session.type_text(saved.to_str().expect("utf-8"));
    session.keys("RET");
    assert!(!session.editor().has_unsaved());
    apply_journal(&mut writer, session.journal_ops()).expect("write journal");
    assert_eq!(journal_files(&journal), Vec::<PathBuf>::new());
    assert!(
        String::from_utf8(fs::read(&saved).expect("read"))
            .expect("utf-8")
            .contains("insert at 0, 1 bytes:\nt\n")
    );
}

#[test]
fn zero_filled_tail_ends_the_journal() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"base\n");
    let mut harness = harness_in(Config::default(), directory.path());
    visit(&mut harness, &path);
    harness.type_text("ab");
    let ops = harness.journal_ops();
    let id = journal_id(&ops);
    let journal = directory.path().join(JOURNAL_DIRECTORY);
    write_journal(&journal, ops).expect("write journal");
    let file = only_journal_file(&journal);
    let good = fs::read(&file).expect("read journal");
    let mut zeroed = good.clone();
    zeroed.extend(std::iter::repeat_n(0u8, 4096));
    fs::write(&file, &zeroed).expect("append zeros");
    let recovered = replay_dir(&journal, &Config::default()).expect("replay");
    assert_eq!(rebuilt(&recovered).3, b"abbase\n");

    write_journal(
        &journal,
        vec![
            JournalOp::Open {
                buffer: id,
                header: Header {
                    path: Some(path.clone()),
                    name: "a.txt".to_string(),
                },
            },
            JournalOp::Record {
                buffer: id,
                record: Record::Insert {
                    at: 2,
                    bytes: b"c".to_vec(),
                },
            },
        ],
    )
    .expect("reopen and append");
    let reopened = fs::read(&file).expect("read journal");
    assert_eq!(
        &reopened[..good.len()],
        good.as_slice(),
        "the zeros are cut"
    );
    assert!(reopened.len() < good.len() + 4096, "the zeros are cut");
    let recovered = replay_dir(&journal, &Config::default()).expect("replay");
    assert_eq!(rebuilt(&recovered).3, b"abcbase\n");
}

#[test]
fn failed_write_poisons_the_journal_until_a_new_epoch() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("big.log");
    let content: Vec<u8> = (0..200_000u32)
        .map(|index| b'a' + (index % 26) as u8)
        .collect();
    write_file(&path, &content);
    let config = Config {
        in_memory_max: 1024,
        ..Config::default()
    };
    let mut harness = harness_in(config.clone(), directory.path());
    visit(&mut harness, &path);
    harness.type_text("a");
    let ops = harness.journal_ops();
    let id = journal_id(&ops);
    let journal = directory.path().join(JOURNAL_DIRECTORY);
    let mut writer = JournalWriter::new(&journal).expect("writer");
    // The snapshot cannot be read: the file was truncated under the buffer.
    write_file(&path, b"");
    let mut results = ops.into_iter().map(|op| writer.apply(op));
    assert!(results.next().expect("open").is_ok());
    assert!(matches!(
        results.next().expect("snapshot"),
        Err(JournalError::Write { .. })
    ));
    assert!(matches!(
        results.next().expect("insert"),
        Err(JournalError::Poisoned { journal }) if journal == id
    ));
    drop(results);
    harness.type_text("b");
    for op in harness.journal_ops() {
        assert!(
            matches!(writer.apply(op), Err(JournalError::Poisoned { .. })),
            "no record lands after the gap"
        );
    }

    write_file(&path, &content);
    harness.journal_write_failed(id, "Read error");
    assert_eq!(
        harness.echo(),
        "Journal write failed for big.log; edits are unprotected until it recovers: Read error"
    );
    harness.type_text("c");
    apply_journal(&mut writer, harness.journal_ops()).expect("restarted journal");
    let recovered = replay_dir(&journal, &config).expect("replay");
    assert_eq!(rebuilt(&recovered).3, harness.text().as_bytes());
    assert!(harness.text().starts_with("abc"));
}

/// Linux may drop the pages of a failed `fsync` and report the next one
/// clean, so the journal cannot be trusted after one: it is poisoned, and
/// the restart the editor emits empties it before the new epoch.
#[test]
fn sync_failure_starts_the_journal_afresh() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let directory = tempfile::tempdir().expect("tempdir");
    let journal = directory.path().join(JOURNAL_DIRECTORY);
    let fail = Arc::new(AtomicBool::new(true));
    let failing = fail.clone();
    let mut writer = JournalWriter::with_sync(
        &journal,
        Box::new(move |file: &fs::File| {
            if failing.swap(false, Ordering::SeqCst) {
                return Err(std::io::Error::from_raw_os_error(5));
            }
            file.sync_data()
        }),
    )
    .expect("writer");
    let mut harness = Harness::new();
    harness.type_text(&"x".repeat(1000));
    let ops = harness.journal_ops();
    let id = journal_id(&ops);
    for op in ops {
        writer.apply(op).expect("apply");
    }
    let failures = writer.sync();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].journal, Some(id));
    assert!(!fail.load(Ordering::SeqCst), "the failure was injected");
    harness.type_text("y");
    for op in harness.journal_ops() {
        assert!(matches!(
            writer.apply(op),
            Err(JournalError::Poisoned { journal }) if journal == id
        ));
    }
    assert!(writer.sync().is_empty(), "the retried sync succeeds");

    harness.journal_write_failed(id, "Input/output error");
    harness.type_text("z");
    apply_journal(&mut writer, harness.journal_ops()).expect("restarted journal");
    let recovered = replay_dir(&journal, &Config::default()).expect("replay");
    assert_eq!(rebuilt(&recovered).3, harness.text().as_bytes());
    let size = fs::metadata(journal.join(id.file_name()))
        .expect("journal file")
        .len();
    let text = harness.text().len() as u64;
    assert!(
        size < 2 * text,
        "only the new epoch is in the file ({size} bytes for {text} bytes of text)"
    );
}

/// Restarting after a failed sync must not destroy records that were
/// already durable: the new journal replaces the old one only once its
/// header and epoch are on disk.
#[test]
fn restart_replaces_the_journal_only_once_the_new_one_is_durable() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    let directory = tempfile::tempdir().expect("tempdir");
    let journal = directory.path().join(JOURNAL_DIRECTORY);
    // Fail the syncs numbered here (1-based): the batch sync, then the
    // sync of the replacement.
    let calls = Arc::new(AtomicU32::new(0));
    let counter = calls.clone();
    let mut writer = JournalWriter::with_sync(
        &journal,
        Box::new(move |file: &fs::File| {
            let call = counter.fetch_add(1, Ordering::SeqCst) + 1;
            if call == 1 || call == 2 {
                return Err(std::io::Error::from_raw_os_error(5));
            }
            file.sync_data()
        }),
    )
    .expect("writer");
    let mut harness = Harness::new();
    harness.type_text("durable");
    let ops = harness.journal_ops();
    let id = journal_id(&ops);
    apply_journal_unsynced(&mut writer, ops);
    assert_eq!(writer.sync().len(), 1, "the batch sync fails");
    let old_file = journal.join(id.file_name());
    let old = fs::read(&old_file).expect("old journal");
    let replay_old = || {
        let recovered = replay_dir(&journal, &Config::default()).expect("replay");
        rebuilt(&recovered).3.to_vec()
    };
    assert_eq!(replay_old(), b"durable");

    harness.type_text("!");
    harness.journal_ops();
    harness.journal_write_failed(id, "Input/output error");
    let mut restart = harness.journal_ops().into_iter();
    let open = restart.next().expect("header");
    assert!(matches!(open, JournalOp::Open { .. }));
    writer.apply(open).expect("the header goes to the new file");
    assert_eq!(fs::read(&old_file).expect("old journal"), old, "untouched");
    assert_eq!(replay_old(), b"durable", "a crash now replays the old one");
    let epoch = restart.next().expect("epoch");
    assert!(
        writer.apply(epoch).is_err(),
        "the replacement's sync fails, so it does not replace"
    );
    assert_eq!(fs::read(&old_file).expect("old journal"), old, "untouched");

    harness.journal_write_failed(id, "Input/output error");
    apply_journal(&mut writer, harness.journal_ops()).expect("second restart");
    assert_eq!(replay_old(), b"durable!");
    let names: Vec<String> = fs::read_dir(&journal)
        .expect("journal dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(names, vec![id.file_name()], "no replacement file is left");
}

fn apply_journal_unsynced(writer: &mut JournalWriter, ops: Vec<JournalOp>) {
    for op in ops {
        writer.apply(op).expect("apply");
    }
}
