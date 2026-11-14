//! Expected values come from GNU Emacs 29.3 running the same key sequences
//! (`emacs --batch -Q`, `execute-kbd-macro`), converted to 0-based offsets.

use std::ops::Range;
use std::sync::Arc;

use emx_core::{
    Buffer, BufferId, Change, ChangeRecorder, Config, InsertionType, UndoEntry, UndoError,
    UndoLimits,
};
use emx_text::{Sources, Text};

const SELF_INSERT: &str = "self-insert-command";
const UNDO: &str = "undo";
const NO_FURTHER: &str = "No further undo information";
const FOR_REGION: &str = "No further undo information for region";
const NO_REDO: &str = "No undone changes to redo";

#[derive(Default)]
struct Log {
    changes: Vec<Change>,
}

impl ChangeRecorder for Log {
    fn record(&mut self, _buffer: BufferId, change: &Change) {
        self.changes.push(change.clone());
    }
}

/// Plays the command loop's part: one undo boundary after every command
/// (including the one that opened the buffer) and `last-command` tracking.
/// Point is a plain position like Emacs's PT, which is not a buffer marker,
/// so deletions record no marker adjustment for it.
struct Session {
    buffer: Buffer,
    point: u64,
    last_command: Option<&'static str>,
    log: Log,
}

fn text_of(content: &str) -> Text {
    Text::from_original(Sources::memory(Arc::from(content.as_bytes())))
}

impl Session {
    fn new(content: &str, point: u64) -> Session {
        Session::with_limits(content, point, UndoLimits::from(&Config::default()))
    }

    fn with_limits(content: &str, point: u64, limits: UndoLimits) -> Session {
        let mut buffer = Buffer::new(
            BufferId::default(),
            "t".to_string(),
            text_of(content),
            limits,
        );
        buffer.undo.boundary(point);
        Session {
            buffer,
            point,
            last_command: None,
            log: Log::default(),
        }
    }

    fn point(&self) -> u64 {
        self.point
    }

    fn text(&self) -> String {
        let mut out = Vec::new();
        self.buffer
            .text
            .read(0..self.buffer.text.len(), &mut out)
            .expect("memory text reads");
        String::from_utf8(out).expect("tests use UTF-8")
    }

    fn finish(&mut self, command: &'static str) {
        let point = self.point();
        self.buffer.undo.boundary(point);
        self.last_command = Some(command);
    }

    fn goto(&mut self, pos: u64) {
        self.point = pos;
        self.finish("goto-char");
    }

    fn self_insert(&mut self, typed: &str) {
        for character in typed.chars() {
            let mut encoded = [0u8; 4];
            let bytes = character.encode_utf8(&mut encoded).as_bytes();
            self.buffer
                .amalgamate_last_change(self.last_command == Some(SELF_INSERT));
            let at = self.point();
            self.buffer
                .insert(at, bytes, &mut self.log)
                .expect("insert succeeds");
            self.point = at + bytes.len() as u64;
            self.finish(SELF_INSERT);
        }
    }

    /// `C-u N DEL`: a prefix argument of 2 or more does not amalgamate.
    fn delete_backward(&mut self, count: u64) {
        let end = self.point();
        self.buffer
            .delete_with_point(end - count..end, end, &mut self.log)
            .expect("delete succeeds");
        self.point = end - count;
        self.finish("delete-backward-char");
    }

    fn apply_undo(
        &mut self,
        result: Result<emx_core::UndoOutcome, UndoError>,
    ) -> Result<&'static str, String> {
        let outcome = result;
        // A failed undo still leaves `this-command` as `undo` in Emacs.
        if let Ok(outcome) = &outcome
            && let Some(point) = outcome.point
        {
            self.point = point;
        }
        self.finish(UNDO);
        outcome
            .map(|outcome| outcome.message)
            .map_err(|error| error.to_string())
    }

    fn undo(&mut self) -> Result<&'static str, String> {
        let continuing = self.last_command == Some(UNDO);
        let result = self.buffer.undo(1, continuing, &mut self.log);
        self.apply_undo(result)
    }

    fn undo_in_region(&mut self, region: Range<u64>) -> Result<&'static str, String> {
        let continuing = self.last_command == Some(UNDO);
        let result = self
            .buffer
            .undo_in_region(region, 1, continuing, &mut self.log);
        self.apply_undo(result)
    }

    fn undo_redo(&mut self) -> Result<&'static str, String> {
        let result = self.buffer.undo_redo(1, &mut self.log);
        self.apply_undo(result)
    }
}

#[test]
fn undo_restores_text_and_point() {
    // "abc def": C-e, C-u 3 DEL, C-b C-b, C-/
    let mut session = Session::new("abc def", 0);
    session.goto(7);
    session.delete_backward(3);
    assert_eq!(session.text(), "abc ");
    assert_eq!(session.point(), 4);
    session.goto(3);
    session.goto(2);
    assert_eq!(session.undo(), Ok("Undo"));
    assert_eq!(session.text(), "abc def");
    assert_eq!(session.point(), 7);
    assert!(!session.buffer.is_modified());
}

#[test]
fn undo_is_undoable() {
    // "xyz" with point at end: a b, C-/, C-b, C-/
    let mut session = Session::new("xyz", 3);
    session.self_insert("ab");
    assert_eq!(session.text(), "xyzab");
    assert_eq!(session.undo(), Ok("Undo"));
    assert_eq!(session.text(), "xyz");
    assert_eq!(session.point(), 3);
    session.goto(2);
    // Undoing an undo is announced as a redo.
    assert_eq!(session.undo(), Ok("Redo"));
    assert_eq!(session.text(), "xyzab");
    assert_eq!(session.point(), 3);
    assert!(session.buffer.is_modified());
}

#[test]
fn consecutive_undos_walk_back() {
    // a C-b b C-b c, then C-/ C-/ C-/ C-/
    let mut session = Session::new("", 0);
    session.self_insert("a");
    session.goto(0);
    session.self_insert("b");
    session.goto(0);
    session.self_insert("c");
    assert_eq!(session.text(), "cba");
    assert_eq!(session.point(), 1);

    assert_eq!(session.undo(), Ok("Undo"));
    assert_eq!((session.text().as_str(), session.point()), ("ba", 0));
    assert_eq!(session.undo(), Ok("Undo"));
    assert_eq!((session.text().as_str(), session.point()), ("a", 0));
    assert_eq!(session.undo(), Ok("Undo"));
    assert_eq!((session.text().as_str(), session.point()), ("", 0));
    assert!(!session.buffer.is_modified());
    assert_eq!(session.undo(), Err(NO_FURTHER.to_string()));
    assert_eq!((session.text().as_str(), session.point()), ("", 0));
}

#[test]
fn undo_redo_reverses_last_undo() {
    // a C-b b, C-/ C-/, C-? C-? C-?
    let mut session = Session::new("", 0);
    session.self_insert("a");
    session.goto(0);
    session.self_insert("b");
    assert_eq!(session.undo(), Ok("Undo"));
    assert_eq!(session.undo(), Ok("Undo"));
    assert_eq!((session.text().as_str(), session.point()), ("", 0));

    assert_eq!(session.undo_redo(), Ok("Redo"));
    assert_eq!((session.text().as_str(), session.point()), ("a", 0));
    assert_eq!(session.undo_redo(), Ok("Redo"));
    assert_eq!((session.text().as_str(), session.point()), ("ba", 0));
    assert_eq!(session.undo_redo(), Err(NO_REDO.to_string()));
    assert_eq!((session.text().as_str(), session.point()), ("ba", 0));

    // Redo after redo, then undo continues the chain: a C-b b C-/ C-/ C-? C-/
    let mut session = Session::new("", 0);
    session.self_insert("a");
    session.goto(0);
    session.self_insert("b");
    session.undo().expect("undo");
    session.undo().expect("undo");
    session.undo_redo().expect("redo");
    assert_eq!(session.undo(), Ok("Undo"));
    assert_eq!((session.text().as_str(), session.point()), ("", 0));
}

#[test]
fn self_insert_amalgamates_20() {
    // 45 x's: Emacs groups the first 21 and the next 21, then 3.
    let mut session = Session::new("", 0);
    session.self_insert(&"x".repeat(45));
    assert_eq!(session.undo(), Ok("Undo"));
    assert_eq!(session.text(), "x".repeat(42));
    assert_eq!(session.point(), 42);
    assert_eq!(session.undo(), Ok("Undo"));
    assert_eq!(session.text(), "x".repeat(21));
    assert_eq!(session.point(), 21);
    assert_eq!(session.undo(), Ok("Undo"));
    assert_eq!(session.text(), "");
    assert_eq!(session.point(), 0);

    // 25 x's then C-/ leaves 21.
    let mut session = Session::new("", 0);
    session.self_insert(&"x".repeat(25));
    session.undo().expect("undo");
    assert_eq!(session.text(), "x".repeat(21));
    assert_eq!(session.point(), 21);

    // A different command in between breaks amalgamation: a b C-b c C-/
    let mut session = Session::new("", 0);
    session.self_insert("ab");
    session.goto(1);
    session.self_insert("c");
    session.undo().expect("undo");
    assert_eq!((session.text().as_str(), session.point()), ("ab", 1));
}

#[test]
fn undo_restores_unmodified_flag() {
    let mut session = Session::new("abc", 3);
    assert!(!session.buffer.is_modified());
    session.self_insert("x");
    assert!(session.buffer.is_modified());
    session.undo().expect("undo");
    assert_eq!(session.text(), "abc");
    assert!(!session.buffer.is_modified());

    // A save between changes: undoing back to the save point marks the buffer
    // unmodified; undoing past it (an obsolete save) leaves it modified.
    let mut session = Session::new("abc", 3);
    let first_save = emx_text::FileIdentity {
        dev: 1,
        ino: 2,
        size: 3,
        mtime_ns: 4,
    };
    let second_save = emx_text::FileIdentity {
        size: 4,
        mtime_ns: 5,
        ..first_save
    };
    session.buffer.file = Some(emx_core::FileBinding::new(
        "/tmp/f".into(),
        Some(first_save),
    ));
    session.self_insert("x");
    session.goto(0);
    session.buffer.file.as_mut().expect("bound").identity = Some(second_save);
    session.buffer.set_unmodified();
    session.self_insert("y");
    assert!(session.buffer.is_modified());
    session.undo().expect("undo");
    assert_eq!(session.text(), "abcx");
    assert!(!session.buffer.is_modified());
    session.undo().expect("undo");
    assert_eq!(session.text(), "abc");
    assert!(session.buffer.is_modified());
}

#[test]
fn undo_in_region_only_affects_region() {
    // "aaaa bbbb": X C-e Y C-a C-SPC C-f C-f C-/
    let mut session = Session::new("aaaa bbbb", 0);
    session.self_insert("X");
    session.goto(10);
    session.self_insert("Y");
    assert_eq!(session.text(), "Xaaaa bbbbY");
    session.goto(0);
    session.goto(2);
    assert_eq!(session.undo_in_region(0..2), Ok("Undo in region"));
    assert_eq!(session.text(), "aaaa bbbbY");
    assert_eq!(session.point(), 0);
    // Continuing ignores the region argument and walks the selective list.
    assert_eq!(session.undo_in_region(0..0), Err(FOR_REGION.to_string()));
    assert_eq!(session.text(), "aaaa bbbbY");
    // A plain undo afterwards undoes the region undo first.
    session.goto(0);
    assert_eq!(session.undo(), Ok("Redo"));
    assert_eq!(session.text(), "Xaaaa bbbbY");
    assert_eq!(session.point(), 0);
}

#[test]
fn undo_truncates_beyond_strong_limit() {
    // Truncation runs at each boundary the way Emacs's GC-time
    // truncate_undo_list does after every command. Sizes follow Emacs's
    // accounting: 16-byte cons cells, a 31-byte string header plus the bytes.
    let limits = UndoLimits {
        limit: 100,
        strong_limit: 150,
        outer_limit: 1_000_000,
        amalgamation_limit: emx_core::config::AMALGAMATION_LIMIT,
    };
    // x, C-u 200 DEL from the end, y: the deletion group alone exceeds the
    // strong limit, so the boundary after it cuts the oldest group ("x").
    let mut session = Session::with_limits(&"d".repeat(200), 0, limits);
    session.self_insert("x");
    session.goto(201);
    session.delete_backward(200);
    session.self_insert("y");
    assert_eq!((session.text().as_str(), session.point()), ("xy", 2));
    session.undo().expect("undo");
    assert_eq!((session.text().as_str(), session.point()), ("x", 1));
    session.undo().expect("undo");
    assert_eq!(session.text(), format!("x{}", "d".repeat(200)));
    assert_eq!(session.point(), 201);
    assert_eq!(session.undo(), Err(NO_FURTHER.to_string()));
    assert_eq!(session.text(), format!("x{}", "d".repeat(200)));
    assert!(!session.buffer.undo.take_outer_limit_warning());

    // With a 5-byte deletion the size at the second boundary (148) lies
    // between the limit and the strong limit: kept through that boundary,
    // the oldest group is cut.
    let mut session = Session::with_limits(&"d".repeat(200), 0, limits);
    session.self_insert("x");
    session.goto(6);
    session.delete_backward(5);
    session.self_insert("y");
    session.undo().expect("undo");
    session.undo().expect("undo");
    assert_eq!(session.text(), format!("x{}", "d".repeat(200)));
    assert_eq!(session.point(), 6);
    assert_eq!(session.undo(), Err(NO_FURTHER.to_string()));
    assert_eq!(session.text(), format!("x{}", "d".repeat(200)));
}

#[test]
fn undo_outer_limit_discards_and_warns() {
    let limits = UndoLimits {
        limit: 100,
        strong_limit: 150,
        outer_limit: 200,
        amalgamation_limit: emx_core::config::AMALGAMATION_LIMIT,
    };
    let mut session = Session::with_limits(&"d".repeat(300), 300, limits);
    session.delete_backward(300);
    assert!(session.buffer.undo.take_outer_limit_warning());
    assert!(!session.buffer.undo.take_outer_limit_warning());
    assert_eq!(session.undo(), Err(NO_FURTHER.to_string()));
    assert_eq!(session.text(), "");
}

#[test]
fn markers_shift_on_insert_and_delete() {
    let mut buffer = Buffer::new(
        BufferId::default(),
        "m".to_string(),
        text_of("0123456789"),
        UndoLimits::from(&Config::default()),
    );
    let mut log = Log::default();
    let before = buffer.create_marker(4, InsertionType::StayBefore);
    let after = buffer.create_marker(4, InsertionType::AdvanceAfter);
    let later = buffer.create_marker(8, InsertionType::StayBefore);
    let earlier = buffer.create_marker(2, InsertionType::AdvanceAfter);

    buffer.insert(4, b"ab", &mut log).expect("insert");
    buffer.undo.boundary(4);
    assert_eq!(buffer.marker_pos(before), 4);
    assert_eq!(buffer.marker_pos(after), 6);
    assert_eq!(buffer.marker_pos(later), 10);
    assert_eq!(buffer.marker_pos(earlier), 2);

    // Text is now "0123ab456789"; delete "3ab45" (3..8).
    let deleted = buffer.delete(3..8, &mut log).expect("delete");
    assert_eq!(deleted, b"3ab45");
    assert_eq!(buffer.marker_pos(before), 3);
    assert_eq!(buffer.marker_pos(after), 3);
    assert_eq!(buffer.marker_pos(later), 5);
    assert_eq!(buffer.marker_pos(earlier), 2);

    // Undo reinserts and restores markers that sat inside the deletion.
    buffer.undo.boundary(3);
    buffer.undo(1, false, &mut log).expect("undo");
    assert_eq!(buffer.marker_pos(before), 4);
    assert_eq!(buffer.marker_pos(after), 6);
    assert_eq!(buffer.marker_pos(later), 10);
    assert_eq!(buffer.marker_pos(earlier), 2);

    buffer.set_marker(later, 1_000);
    assert_eq!(buffer.marker_pos(later), buffer.text.len());
    buffer.delete_marker(later);

    assert!(matches!(
        log.changes.as_slice(),
        [
            Change::Insert { at: 4, .. },
            Change::Delete { at: 3, len: 5 },
            Change::Insert { at: 3, .. },
        ]
    ));
}

/// Window point and start are not undo-tracked markers: Emacs PT is not a
/// marker, so a deletion around it records no `(MARKER . ADJUSTMENT)`.
#[test]
fn markers_without_undo_record_no_adjustment() {
    let mut buffer = Buffer::new(
        BufferId::default(),
        "w".to_string(),
        text_of("0123456789"),
        UndoLimits::from(&Config::default()),
    );
    let mut log = Log::default();
    buffer.undo.boundary(0);
    let window_point = buffer.create_marker_without_undo(5, InsertionType::StayBefore);
    let tracked = buffer.create_marker(6, InsertionType::StayBefore);
    buffer.delete(3..8, &mut log).expect("delete");
    assert_eq!(buffer.marker_pos(window_point), 3);
    let adjustments: Vec<i64> = buffer
        .undo
        .entries()
        .into_iter()
        .filter_map(|entry| match entry {
            UndoEntry::MarkerAdjust { marker, adjustment } => {
                assert_eq!(marker, tracked);
                Some(adjustment)
            }
            _ => None,
        })
        .collect();
    assert_eq!(adjustments, vec![-3]);
}

#[test]
fn undo_list_structure_matches_emacs() {
    // Emacs after "abc def" C-e, C-u 3 DEL, C-b C-b, C-/ (1-based):
    // ((5 . 8) nil ("def" . -5) 8 (t . 0)); the undo's own point record is
    // stripped so undoing it puts point at the change.
    let mut session = Session::new("abc def", 0);
    session.goto(7);
    session.delete_backward(3);
    session.goto(3);
    session.goto(2);
    session.undo().expect("undo");
    let entries = session.buffer.undo.entries();
    assert!(matches!(entries[0], UndoEntry::Boundary), "{entries:?}");
    assert!(
        matches!(
            &entries[1..],
            [
                UndoEntry::Insertion { beg: 4, end: 7 },
                UndoEntry::Boundary,
                UndoEntry::Deletion {
                    at: 4,
                    point_at_end: true,
                    ..
                },
                UndoEntry::PointBefore(7),
                UndoEntry::FirstChange {
                    saved_identity: None
                },
            ]
        ),
        "{entries:?}"
    );
}

#[test]
fn delete_backward_amalgamates_and_restores_point_at_end() {
    // "abcdef": C-e DEL DEL DEL C-a C-/ gives "abcdef" with point 6.
    let mut session = Session::new("abcdef", 0);
    session.goto(6);
    for _ in 0..3 {
        session
            .buffer
            .amalgamate_last_change(session.last_command == Some("delete-backward-char"));
        let end = session.point();
        session
            .buffer
            .delete_with_point(end - 1..end, end, &mut session.log)
            .expect("delete");
        session.point = end - 1;
        session.finish("delete-backward-char");
    }
    session.goto(0);
    assert_eq!(session.text(), "abc");
    assert_eq!(session.undo(), Ok("Undo"));
    assert_eq!((session.text().as_str(), session.point()), ("abcdef", 6));
    assert!(!session.buffer.is_modified());
}

#[test]
fn change_between_undos_breaks_the_chain() {
    // Emacs `undo--last-change-was-undo-p` fails when something edited the
    // buffer after the last undo, so the next undo starts over and undoes
    // that edit first.
    let mut session = Session::new("", 0);
    session.self_insert("a");
    session.goto(0);
    session.self_insert("b");
    session.undo().expect("undo");
    assert_eq!(session.text(), "a");
    session
        .buffer
        .insert(1, b"Z", &mut session.log)
        .expect("insert");
    session.buffer.undo.boundary(0);
    assert_eq!(session.undo(), Ok("Undo"));
    assert_eq!(session.text(), "a");
}

#[test]
fn replace_all_is_undoable() {
    let mut session = Session::new("old text", 4);
    let replacement = text_of("brand new");
    session
        .buffer
        .replace_all(replacement, &mut session.log)
        .expect("replace");
    session.buffer.set_unmodified();
    session.finish("revert-buffer");
    assert_eq!(session.text(), "brand new");
    assert_eq!(session.undo(), Ok("Undo"));
    assert_eq!(session.text(), "old text");
    assert_eq!(session.point(), 4);
    assert_eq!(session.undo_redo(), Ok("Redo"));
    assert_eq!(session.text(), "brand new");
    assert!(matches!(
        session.log.changes.as_slice(),
        [
            Change::ReplaceAll { .. },
            Change::ReplaceAll { .. },
            Change::ReplaceAll { .. }
        ]
    ));
}

#[test]
fn undo_in_read_only_buffer_is_refused() {
    let mut session = Session::new("", 0);
    session.self_insert("a");
    session.buffer.read_only = true;
    assert_eq!(session.undo(), Err("Buffer is read-only".to_string()));
    assert_eq!(session.text(), "a");
}

#[test]
fn edits_out_of_range_are_rejected() {
    let mut session = Session::new("abc", 0);
    assert!(matches!(
        session.buffer.insert(4, b"x", &mut session.log),
        Err(emx_core::EditError::OutOfRange)
    ));
    assert!(matches!(
        session.buffer.delete(2..5, &mut session.log),
        Err(emx_core::EditError::OutOfRange)
    ));
    assert!(session.log.changes.is_empty());
    assert!(session.buffer.undo.entries().is_empty());
}

#[test]
fn dropping_a_long_undo_list_does_not_overflow_the_stack() {
    let limits = UndoLimits {
        limit: u64::MAX,
        strong_limit: u64::MAX,
        outer_limit: u64::MAX,
        amalgamation_limit: emx_core::config::AMALGAMATION_LIMIT,
    };
    let mut session = Session::with_limits("", 0, limits);
    for _ in 0..300_000 {
        session
            .buffer
            .insert(0, b"x", &mut session.log)
            .expect("insert");
    }
    session.finish("many-inserts");
    drop(session);
}

#[test]
fn outer_limit_warning_names_the_variable() {
    assert_eq!(
        emx_core::outer_limit_warning("notes.txt"),
        "Buffer `notes.txt' undo info was discarded because it exceeded `undo-outer-limit'"
    );
}

#[test]
fn reload_undo_survives_edit_and_undo() {
    // Reload, type x, C-/ (removes x, leaving equal text under a new root),
    // C-/ again: the pre-reload text comes back with point where it was.
    let before_reload = emx_text::FileIdentity {
        dev: 1,
        ino: 2,
        size: 8,
        mtime_ns: 3,
    };
    let mut session = Session::new("old text", 4);
    session.buffer.file = Some(emx_core::FileBinding::new(
        "/tmp/reloaded".into(),
        Some(before_reload),
    ));
    session
        .buffer
        .replace_all(text_of("brand new"), &mut session.log)
        .expect("replace");
    // The reload adopts the new disk version, as M2's reload will.
    session.buffer.file.as_mut().expect("bound").identity = Some(emx_text::FileIdentity {
        size: 9,
        mtime_ns: 4,
        ..before_reload
    });
    session.buffer.set_unmodified();
    session.point = 0;
    session.finish("revert-buffer");
    session.goto(5);
    session.self_insert("x");
    assert_eq!(session.text(), "brandx new");
    assert_eq!(session.undo(), Ok("Undo"));
    assert_eq!((session.text().as_str(), session.point()), ("brand new", 5));
    assert_eq!(session.undo(), Ok("Undo"));
    assert_eq!((session.text().as_str(), session.point()), ("old text", 4));
    // The pre-reload text no longer matches the disk.
    assert!(session.buffer.is_modified());
}

#[test]
fn reload_undo_refuses_when_text_differs() {
    // Only a change that bypassed the undo list can make the text differ
    // from the reloaded text; undo must refuse rather than discard it.
    let mut session = Session::new("abc", 0);
    session
        .buffer
        .replace_all(text_of("xyz"), &mut session.log)
        .expect("replace");
    session.finish("revert-buffer");
    // Swap the text behind undo's back, keeping the length.
    session.buffer.text = text_of("xyq");
    assert_eq!(
        session.undo(),
        Err("Buffer text differs from the reloaded text; cannot undo the reload".to_string())
    );
    assert_eq!(session.text(), "xyq");
}

#[test]
fn amalgamation_limit_comes_from_config() {
    let config = Config {
        amalgamation_limit: 2,
        ..Config::default()
    };
    let mut session = Session::with_limits("", 0, UndoLimits::from(&config));
    session.self_insert("xxxxx");
    session.undo().expect("undo");
    assert_eq!((session.text().as_str(), session.point()), ("xxx", 3));
}

#[test]
fn replace_all_counts_the_text_it_keeps() {
    // A ReplaceAll entry keeps the replaced text alive, which Emacs would
    // count as a deleted string: a reload of a large in-memory file is
    // charged its size, so the strong limit cuts the groups before it. A
    // fixed per-entry charge would keep them all.
    let limits = UndoLimits {
        limit: 1000,
        strong_limit: 1500,
        outer_limit: 1_000_000,
        amalgamation_limit: emx_core::config::AMALGAMATION_LIMIT,
    };
    let mut session = Session::with_limits("", 0, limits);
    session.self_insert("x");
    for content in ["r".repeat(10), "n".repeat(3000), "tiny".to_string()] {
        session
            .buffer
            .replace_all(text_of(&content), &mut session.log)
            .expect("replace");
        session.finish("revert-buffer");
    }
    assert_eq!(session.undo(), Ok("Undo"));
    assert_eq!(session.text(), "n".repeat(3000));
    assert_eq!(session.undo(), Err(NO_FURTHER.to_string()));
    assert_eq!(session.text(), "n".repeat(3000));

    // Small replaced texts are all kept.
    let mut session = Session::with_limits("", 0, limits);
    session.self_insert("x");
    for content in ["r".repeat(10), "n".repeat(30), "tiny".to_string()] {
        session
            .buffer
            .replace_all(text_of(&content), &mut session.log)
            .expect("replace");
        session.finish("revert-buffer");
    }
    for expected in ["n".repeat(30), "r".repeat(10), "x".to_string()] {
        assert_eq!(session.undo(), Ok("Undo"));
        assert_eq!(session.text(), expected);
    }

    // Texts sharing one original (an edit undone and redone) cost nothing
    // more than the entry.
    let shared = text_of("same original");
    let mut session = Session::with_limits("", 0, limits);
    session.buffer.text = shared.clone();
    session
        .buffer
        .replace_all(shared.delete(0..5).expect("delete"), &mut session.log)
        .expect("replace");
    session.finish("revert-buffer");
    let sizes: Vec<UndoEntry> = session.buffer.undo.entries();
    assert!(
        sizes
            .iter()
            .any(|entry| matches!(entry, UndoEntry::ReplaceAll { .. }))
    );
    assert_eq!(session.undo(), Ok("Undo"));
    assert_eq!(session.text(), "same original");
}
