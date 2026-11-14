//! The editor as the session server drives it: several clients, socket API
//! requests, chunked pastes and keys no terminal can type.

use std::fs;
use std::path::Path;

use emx_core::testing::Harness;
use emx_core::{ClientId, Config, Session};
use emx_proto::{
    BufferInfo, ClientKind, EditorEvent, Request, Response, ServerMsg, WireError, WirePath,
    error_code,
};
use emx_types::{Key, KeyCode, Mods};

const DELETE_CHARACTER: u32 = 0x7f;
const FIRST_NON_CONTROL: u32 = 0x20;

fn server() -> Harness {
    Harness::with_session(Config::default(), Session::Server)
}

fn server_with(config: Config) -> Harness {
    Harness::with_session(config, Session::Server)
}

fn wire(path: &Path) -> WirePath {
    WirePath::from(path)
}

fn open(path: &Path) -> Request {
    Request::Open {
        path: wire(path),
        line: None,
        col: None,
        wait: false,
    }
}

/// The result of request `id` among `messages`, if it was answered.
fn response(messages: &[ServerMsg], id: u64) -> Option<Result<Response, WireError>> {
    messages.iter().find_map(|message| match message {
        ServerMsg::Response {
            id: answered,
            result,
        } if *answered == id => Some(result.clone()),
        _ => None,
    })
}

fn answer(harness: &mut Harness, client: ClientId, id: u64) -> Result<Response, WireError> {
    let messages = harness.take_messages(client);
    response(&messages, id).unwrap_or_else(|| panic!("request {id} unanswered: {messages:?}"))
}

fn events(messages: &[ServerMsg]) -> Vec<EditorEvent> {
    messages
        .iter()
        .filter_map(|message| match message {
            ServerMsg::Event { event } => Some(event.clone()),
            _ => None,
        })
        .collect()
}

fn modeline(screen: &[String]) -> String {
    screen[screen.len() - 2].clone()
}

fn plain_char(character: char, mods: Mods) -> Key {
    Key {
        code: KeyCode::Char(character),
        mods,
    }
}

// ---- Keys no terminal can type ---------------------------------------------

/// Emacs reads a control character as the `C-<letter>` key: a raw `\n`
/// is `C-j`, `\x01` is `C-a`, DEL is `DEL`.
#[test]
fn ctrl_char_key_never_panics() {
    let modifier_sets = [
        Mods::default(),
        Mods {
            meta: true,
            ..Mods::default()
        },
        Mods {
            ctrl: true,
            ..Mods::default()
        },
        Mods {
            shift: true,
            ..Mods::default()
        },
    ];
    for code in (0..FIRST_NON_CONTROL).chain([DELETE_CHARACTER]) {
        let character = char::from_u32(code).expect("ASCII");
        for mods in modifier_sets {
            let mut harness = Harness::with_text("ab\ncd|");
            harness.key(plain_char(character, mods));
            harness.screen();
            harness.keys("C-g");
        }
    }
    let equivalents = [
        ('\u{1}', "C-a"),
        ('\n', "C-j"),
        ('\r', "RET"),
        ('\t', "TAB"),
        ('\u{1b}', "ESC"),
        ('\u{0}', "C-@"),
        ('\u{7f}', "DEL"),
    ];
    for (character, kbd) in equivalents {
        let mut raw = Harness::with_text("ab\ncd|");
        raw.key(plain_char(character, Mods::default()));
        let mut typed = Harness::with_text("ab\ncd|");
        typed.keys(kbd);
        assert_eq!(raw.text(), typed.text(), "{character:?} as {kbd}");
        assert_eq!(raw.point(), typed.point(), "{character:?} as {kbd}");
        assert_eq!(raw.echo(), typed.echo(), "{character:?} as {kbd}");
    }
}

#[test]
fn newline_in_echo_area_or_minibuffer_is_escaped() {
    let mut harness = Harness::new();
    harness.keys("C-x C-f");
    harness.paste(b"a\nb");
    let screen = harness.screen();
    assert!(
        screen.last().expect("echo row").contains("a^Jb"),
        "{screen:?}"
    );
}

// ---- Chunked paste -----------------------------------------------------------

#[test]
fn paste_chunks_accumulate() {
    let mut harness = Harness::new();
    let client = harness.client();
    harness.paste_chunk(client, b"hel", false);
    assert_eq!(
        harness.text(),
        "",
        "nothing is inserted before the last chunk"
    );
    harness.paste_chunk(client, b"lo\n", false);
    harness.paste_chunk(client, b"world", true);
    assert_eq!(harness.text(), "hello\nworld");
    assert_eq!(harness.kill_ring(), vec!["hello\nworld".to_string()]);
    harness.keys("C-/");
    assert_eq!(harness.text(), "", "the paste is one undoable change");
}

#[test]
fn oversized_paste_is_discarded_with_an_error() {
    let config = Config {
        in_memory_max: 8,
        ..Config::default()
    };
    let mut harness = Harness::with_config(config);
    let client = harness.client();
    harness.paste_chunk(client, b"12345", false);
    harness.paste_chunk(client, b"67890", false);
    harness.paste_chunk(client, b"x", true);
    assert_eq!(harness.text(), "");
    assert!(harness.kill_ring().is_empty());
    assert_eq!(
        harness.echo(),
        "Paste discarded: larger than 8 bytes (in_memory_max)"
    );
    harness.paste_chunk(client, b"ok", true);
    assert_eq!(harness.text(), "ok", "the next paste starts afresh");
}

// ---- Yank per client ---------------------------------------------------------

#[test]
fn yank_pop_per_client() {
    let mut harness = server();
    let first = harness.client();
    harness.type_text("one");
    harness.keys("C-a C-k");
    harness.type_text("two");
    harness.keys("C-a C-k");
    let second = harness.attach(ClientKind::Tui);
    harness.keys_as(first, "C-y");
    assert_eq!(harness.text(), "two");
    harness.keys_as(second, "C-e");
    harness.type_as(second, "X");
    // The mark is the buffer's: the other client moves it.
    harness.keys_as(second, "C-SPC");
    harness.keys_as(first, "M-y");
    assert_eq!(harness.text(), "oneX");
    assert_eq!(harness.point_of(first), 3);
    harness.keys_as(first, "M-y");
    assert_eq!(harness.text(), "twoX", "the ring wraps back to the newest");
}

#[test]
fn yank_pop_after_another_clients_insertion_at_the_yank_start() {
    let mut harness = server();
    let first = harness.client();
    harness.type_text("one");
    harness.keys("C-a C-k");
    harness.type_text("two");
    harness.keys("C-a C-k");
    let second = harness.attach(ClientKind::Tui);
    harness.keys_as(first, "C-y");
    // The second client's point is at 0, the yank's start.
    harness.type_as(second, "Y");
    assert_eq!(harness.text(), "Ytwo");
    harness.keys_as(first, "M-y");
    assert_eq!(harness.text(), "Yone");
}

// ---- Requests ----------------------------------------------------------------

#[test]
fn list_buffers_reports_every_buffer() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("notes.txt");
    fs::write(&path, "abc").expect("fixture");
    let mut harness = server();
    let tui = harness.client();
    let id = harness.request(tui, open(&path));
    assert_eq!(answer(&mut harness, tui, id), Ok(Response::Ok));
    harness.type_as(tui, "x");
    let cli = harness.attach(ClientKind::Cli);
    let id = harness.request(cli, Request::ListBuffers);
    let Ok(Response::Buffers(buffers)) = answer(&mut harness, cli, id) else {
        panic!("expected a buffer list");
    };
    assert!(buffers.contains(&BufferInfo {
        name: "notes.txt".to_string(),
        path: Some(wire(&path)),
        modified: true,
        size: 4,
    }));
    assert!(buffers.iter().any(|buffer| buffer.name == "*scratch*"));
}

#[test]
fn cli_open_displays_in_the_most_recently_active_terminal() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("shown.txt");
    fs::write(&path, "shown\n").expect("fixture");
    let mut harness = server();
    let first = harness.client();
    let second = harness.attach(ClientKind::Tui);
    harness.keys_as(first, "C-f");
    let cli = harness.attach(ClientKind::Cli);
    let id = harness.request(cli, open(&path));
    assert_eq!(answer(&mut harness, cli, id), Ok(Response::Ok));
    assert!(modeline(&harness.screen_of(first)).contains("shown.txt"));
    assert!(!modeline(&harness.screen_of(second)).contains("shown.txt"));
    harness.keys_as(second, "C-f");
    let other = directory.path().join("other.txt");
    let id = harness.request(cli, open(&other));
    assert_eq!(answer(&mut harness, cli, id), Ok(Response::Ok));
    assert!(modeline(&harness.screen_of(second)).contains("other.txt"));
}

#[test]
fn cli_open_without_a_terminal_client_fails() {
    let mut harness = server();
    let tui = harness.client();
    harness.detach(tui);
    let cli = harness.attach(ClientKind::Cli);
    let id = harness.request(cli, open(Path::new("/tmp/anything")));
    assert_eq!(
        answer(&mut harness, cli, id),
        Err(WireError {
            code: error_code::NO_CLIENT.to_string(),
            message: "no emx client is attached; run 'emx FILE' in a terminal first".to_string(),
        })
    );
}

#[test]
fn open_goes_to_line_and_column() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("lines.txt");
    fs::write(&path, "a\nbcd\nef\n").expect("fixture");
    let mut harness = server();
    let tui = harness.client();
    let id = harness.request(
        tui,
        Request::Open {
            path: wire(&path),
            line: Some(2),
            col: Some(3),
            wait: false,
        },
    );
    assert_eq!(answer(&mut harness, tui, id), Ok(Response::Ok));
    assert_eq!(harness.point_of(tui), 4);
    let id = harness.request(
        tui,
        Request::Open {
            path: wire(&path),
            line: Some(2),
            col: Some(40),
            wait: false,
        },
    );
    assert_eq!(answer(&mut harness, tui, id), Ok(Response::Ok));
    assert_eq!(
        harness.point_of(tui),
        5,
        "a column past the end stops there"
    );
    let id = harness.request(
        tui,
        Request::Open {
            path: wire(&path),
            line: Some(99),
            col: None,
            wait: false,
        },
    );
    assert_eq!(answer(&mut harness, tui, id), Ok(Response::Ok));
    assert_eq!(
        harness.point_of(tui),
        9,
        "a line past the end goes to the end"
    );
}

#[test]
fn open_of_a_directory_fails_with_the_load_error() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut harness = server();
    let tui = harness.client();
    let id = harness.request(tui, open(directory.path()));
    let Err(error) = answer(&mut harness, tui, id) else {
        panic!("a directory cannot be visited");
    };
    assert_eq!(error.code, error_code::OPEN_FAILED);
}

#[test]
fn open_wait_released_by_server_edit_after_saving() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("COMMIT_EDITMSG");
    fs::write(&path, "").expect("fixture");
    let mut harness = server();
    let tui = harness.client();
    let cli = harness.attach(ClientKind::Cli);
    let id = harness.request(
        cli,
        Request::Open {
            path: wire(&path),
            line: None,
            col: None,
            wait: true,
        },
    );
    assert_eq!(response(&harness.take_messages(cli), id), None, "held");
    harness.type_as(tui, "fix");
    harness.keys_as(tui, "C-x #");
    assert!(
        harness
            .echo_line_of(tui)
            .starts_with(&format!("Save file {}? (y or n)", path.display()))
    );
    assert_eq!(
        response(&harness.take_messages(cli), id),
        None,
        "still held"
    );
    harness.keys_as(tui, "y");
    assert_eq!(answer(&mut harness, cli, id), Ok(Response::Ok));
    assert_eq!(fs::read_to_string(&path).expect("saved"), "fix");
    assert!(
        harness.buffer_text("COMMIT_EDITMSG").is_none(),
        "a buffer opened for the wait is killed when done"
    );
}

#[test]
fn server_edit_keeps_a_buffer_that_existed_before_the_wait() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("kept.txt");
    fs::write(&path, "kept").expect("fixture");
    let mut harness = server();
    let tui = harness.client();
    let id = harness.request(tui, open(&path));
    assert_eq!(answer(&mut harness, tui, id), Ok(Response::Ok));
    let cli = harness.attach(ClientKind::Cli);
    let id = harness.request(
        cli,
        Request::Open {
            path: wire(&path),
            line: None,
            col: None,
            wait: true,
        },
    );
    harness.keys_as(tui, "C-x #");
    assert_eq!(answer(&mut harness, cli, id), Ok(Response::Ok));
    assert_eq!(harness.buffer_text("kept.txt").as_deref(), Some("kept"));
}

#[test]
fn open_wait_released_by_killing_the_buffer() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("waited.txt");
    fs::write(&path, "w").expect("fixture");
    let mut harness = server();
    let tui = harness.client();
    let cli = harness.attach(ClientKind::Cli);
    let id = harness.request(
        cli,
        Request::Open {
            path: wire(&path),
            line: None,
            col: None,
            wait: true,
        },
    );
    assert_eq!(response(&harness.take_messages(cli), id), None);
    harness.keys_as(tui, "C-x k RET");
    assert_eq!(answer(&mut harness, cli, id), Ok(Response::Ok));
}

#[test]
fn server_edit_without_waiting_clients_says_so() {
    let mut harness = server();
    harness.keys("C-x #");
    assert_eq!(harness.echo(), "No server editing buffers exist");
}

#[test]
fn reload_reverts_an_unmodified_buffer_undoably() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("agent.txt");
    fs::write(&path, "old\n").expect("fixture");
    let mut harness = server();
    let tui = harness.client();
    let id = harness.request(tui, open(&path));
    assert_eq!(answer(&mut harness, tui, id), Ok(Response::Ok));
    fs::write(&path, "new content\n").expect("agent writes");
    let cli = harness.attach(ClientKind::Cli);
    let id = harness.request(
        cli,
        Request::Reload {
            path: Some(wire(&path)),
        },
    );
    assert_eq!(answer(&mut harness, cli, id), Ok(Response::Ok));
    assert_eq!(
        harness.buffer_text("agent.txt").as_deref(),
        Some("new content\n")
    );
    assert!(!harness.buffer_modified("agent.txt"));
    harness.keys_as(tui, "C-/");
    assert_eq!(harness.buffer_text("agent.txt").as_deref(), Some("old\n"));
}

#[test]
fn reload_refuses_a_modified_buffer() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("mine.txt");
    fs::write(&path, "disk\n").expect("fixture");
    let mut harness = server();
    let tui = harness.client();
    let id = harness.request(tui, open(&path));
    assert_eq!(answer(&mut harness, tui, id), Ok(Response::Ok));
    harness.type_as(tui, "typed ");
    let cli = harness.attach(ClientKind::Cli);
    let id = harness.request(
        cli,
        Request::Reload {
            path: Some(wire(&path)),
        },
    );
    assert_eq!(
        answer(&mut harness, cli, id),
        Err(WireError {
            code: error_code::BUFFER_MODIFIED.to_string(),
            message: "buffer modified".to_string(),
        })
    );
    assert_eq!(
        harness.buffer_text("mine.txt").as_deref(),
        Some("typed disk\n")
    );
}

#[test]
fn reload_of_a_file_no_buffer_visits_fails() {
    let mut harness = server();
    let cli = harness.attach(ClientKind::Cli);
    let id = harness.request(
        cli,
        Request::Reload {
            path: Some(WirePath::Text("/nonexistent/emx".to_string())),
        },
    );
    let Err(error) = answer(&mut harness, cli, id) else {
        panic!("nothing to reload");
    };
    assert_eq!(error.code, error_code::NO_BUFFER);
}

#[test]
fn reload_without_a_file_reverts_every_unmodified_file_buffer() {
    let directory = tempfile::tempdir().expect("tempdir");
    let first = directory.path().join("first.txt");
    let second = directory.path().join("second.txt");
    fs::write(&first, "1").expect("fixture");
    fs::write(&second, "2").expect("fixture");
    let mut harness = server();
    let tui = harness.client();
    for path in [&first, &second] {
        let id = harness.request(tui, open(path));
        assert_eq!(answer(&mut harness, tui, id), Ok(Response::Ok));
    }
    fs::write(&first, "one").expect("rewrite");
    fs::write(&second, "two").expect("rewrite");
    let cli = harness.attach(ClientKind::Cli);
    let id = harness.request(cli, Request::Reload { path: None });
    assert_eq!(answer(&mut harness, cli, id), Ok(Response::Ok));
    assert_eq!(harness.buffer_text("first.txt").as_deref(), Some("one"));
    assert_eq!(harness.buffer_text("second.txt").as_deref(), Some("two"));
}

#[test]
fn goto_moves_point_in_the_most_recent_terminal() {
    let mut harness = server();
    let tui = harness.client();
    harness.type_text("ab");
    harness.keys("RET");
    harness.type_text("cdef");
    let cli = harness.attach(ClientKind::Cli);
    let id = harness.request(
        cli,
        Request::Goto {
            path: None,
            line: 2,
            col: Some(3),
        },
    );
    assert_eq!(answer(&mut harness, cli, id), Ok(Response::Ok));
    assert_eq!(harness.point_of(tui), 5);
}

#[test]
fn save_all_writes_every_modified_file_buffer() {
    let directory = tempfile::tempdir().expect("tempdir");
    let first = directory.path().join("a.txt");
    let second = directory.path().join("b.txt");
    fs::write(&first, "a").expect("fixture");
    fs::write(&second, "b").expect("fixture");
    let mut harness = server();
    let tui = harness.client();
    for path in [&first, &second] {
        let id = harness.request(tui, open(path));
        assert_eq!(answer(&mut harness, tui, id), Ok(Response::Ok));
        harness.type_as(tui, "+");
    }
    let cli = harness.attach(ClientKind::Cli);
    let id = harness.request(cli, Request::SaveAll);
    assert_eq!(answer(&mut harness, cli, id), Ok(Response::Ok));
    assert_eq!(fs::read_to_string(&first).expect("read"), "+a");
    assert_eq!(fs::read_to_string(&second).expect("read"), "+b");
    assert!(!harness.editor().has_unsaved());
}

#[test]
fn save_all_never_overwrites_a_file_changed_on_disk() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("raced.txt");
    fs::write(&path, "v1").expect("fixture");
    let mut harness = server();
    let tui = harness.client();
    let id = harness.request(tui, open(&path));
    assert_eq!(answer(&mut harness, tui, id), Ok(Response::Ok));
    harness.type_as(tui, "mine ");
    fs::write(&path, "agent's version").expect("agent writes");
    let cli = harness.attach(ClientKind::Cli);
    let id = harness.request(cli, Request::SaveAll);
    let Err(error) = answer(&mut harness, cli, id) else {
        panic!("a changed file needs the user's confirmation");
    };
    assert_eq!(error.code, error_code::SAVE_FAILED);
    assert!(
        error
            .message
            .contains("raced.txt has changed since visited or saved"),
        "{}",
        error.message
    );
    assert_eq!(fs::read_to_string(&path).expect("read"), "agent's version");
}

#[test]
fn events_report_buffers_and_terminal_clients() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("watched.txt");
    fs::write(&path, "w").expect("fixture");
    let mut harness = server();
    let tui = harness.client();
    let cli = harness.attach(ClientKind::Cli);
    let id = harness.request(cli, Request::SubscribeEvents);
    assert_eq!(answer(&mut harness, cli, id), Ok(Response::Ok));
    let second = harness.attach(ClientKind::Tui);
    let id = harness.request(tui, open(&path));
    assert_eq!(answer(&mut harness, tui, id), Ok(Response::Ok));
    harness.type_as(tui, "x");
    harness.keys_as(tui, "C-x C-s");
    harness.keys_as(tui, "C-x k RET");
    harness.detach(second);
    let reported: Vec<EditorEvent> = events(&harness.take_messages(cli))
        .into_iter()
        .filter(|event| !matches!(event, EditorEvent::CursorMoved { .. }))
        .collect();
    let name = "watched.txt".to_string();
    assert_eq!(
        reported,
        vec![
            EditorEvent::ClientAttached { client: second.0 },
            EditorEvent::BufferOpened {
                path: Some(wire(&path)),
                name: name.clone(),
            },
            EditorEvent::BufferSaved {
                path: Some(wire(&path)),
                name: name.clone(),
            },
            EditorEvent::BufferKilled {
                path: Some(wire(&path)),
                name,
            },
            EditorEvent::ClientDetached { client: second.0 },
        ]
    );
}

#[test]
fn cursor_events_are_throttled_per_client() {
    let mut harness = server();
    let tui = harness.client();
    harness.type_text("abcdef");
    let cli = harness.attach(ClientKind::Cli);
    let id = harness.request(cli, Request::SubscribeEvents);
    assert_eq!(answer(&mut harness, cli, id), Ok(Response::Ok));
    for _ in 0..4 {
        harness.keys_as(tui, "C-b");
    }
    let moves = |messages: &[ServerMsg]| -> Vec<EditorEvent> {
        events(messages)
            .into_iter()
            .filter(|event| matches!(event, EditorEvent::CursorMoved { .. }))
            .collect()
    };
    assert_eq!(
        moves(&harness.take_messages(cli)),
        vec![EditorEvent::CursorMoved {
            client: tui.0,
            path: None,
            line: Some(1),
            col: 6,
            offset: 5,
        }],
        "the first move is reported at once, the rest wait"
    );
    harness.fire_timers();
    assert_eq!(
        moves(&harness.take_messages(cli)),
        vec![EditorEvent::CursorMoved {
            client: tui.0,
            path: None,
            line: Some(1),
            col: 3,
            offset: 2,
        }],
        "the latest position follows the interval"
    );
    harness.fire_timers();
    assert!(moves(&harness.take_messages(cli)).is_empty());
}

#[test]
fn diff_external_has_nothing_recorded_yet() {
    let mut harness = server();
    let cli = harness.attach(ClientKind::Cli);
    let id = harness.request(
        cli,
        Request::DiffExternal {
            path: WirePath::Text("/tmp/x".to_string()),
        },
    );
    assert_eq!(
        answer(&mut harness, cli, id),
        Err(WireError {
            code: error_code::NO_EXTERNAL_CHANGE.to_string(),
            message: "no external change recorded".to_string(),
        })
    );
}

#[test]
fn quitting_a_terminal_client_detaches_it_and_keeps_unsaved_buffers() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("left.txt");
    fs::write(&path, "x").expect("fixture");
    let mut harness = server_with(Config::default());
    let tui = harness.client();
    let id = harness.request(tui, open(&path));
    assert_eq!(answer(&mut harness, tui, id), Ok(Response::Ok));
    harness.type_as(tui, "unsaved ");
    harness.keys_as(tui, "C-x C-c");
    harness.keys_as(tui, "n");
    let messages = harness.take_messages(tui);
    assert!(
        messages
            .iter()
            .any(|message| matches!(message, ServerMsg::Exit { .. })),
        "{messages:?}"
    );
    assert!(harness.editor().has_unsaved());
    assert!(!harness.shutdown_requested());
}

#[test]
fn every_client_frame_is_numbered_from_two() {
    let mut harness = server();
    let second = harness.attach(ClientKind::Tui);
    assert!(modeline(&harness.screen()).starts_with("-UUU:@---  F2  *scratch*"));
    assert!(modeline(&harness.screen_of(second)).starts_with("-UUU:@---  F3  *scratch*"));
}

#[test]
fn a_waiting_client_that_leaves_releases_nothing_and_leaves_no_trace() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("left.txt");
    fs::write(&path, "x").expect("fixture");
    let mut harness = server();
    let tui = harness.client();
    let cli = harness.attach(ClientKind::Cli);
    harness.request(
        cli,
        Request::Open {
            path: wire(&path),
            line: None,
            col: None,
            wait: true,
        },
    );
    harness.detach(cli);
    harness.keys_as(tui, "C-x #");
    assert_eq!(harness.echo_of(tui), "No server editing buffers exist");
    let again = harness.attach(ClientKind::Cli);
    let id = harness.request(
        again,
        Request::Open {
            path: wire(&path),
            line: None,
            col: None,
            wait: true,
        },
    );
    harness.keys_as(tui, "C-x #");
    assert_eq!(answer(&mut harness, again, id), Ok(Response::Ok));
    assert!(
        harness.buffer_text("left.txt").is_some(),
        "the buffer existed before this wait, so it stays"
    );
}

#[test]
fn journal_failure_is_shown_at_once() {
    let mut harness = server();
    harness.type_text("a");
    let id = harness
        .journal_ops()
        .iter()
        .find_map(|op| match op {
            emx_core::journal::JournalOp::Open { buffer, .. } => Some(*buffer),
            _ => None,
        })
        .expect("the edit opened the journal");
    harness.take_sent();
    harness.journal_write_failed(id, "disk full");
    let frames = harness.take_sent();
    assert!(
        frames
            .iter()
            .any(|message| matches!(message, ServerMsg::Frame { .. })),
        "the warning is drawn without waiting for input"
    );
}
