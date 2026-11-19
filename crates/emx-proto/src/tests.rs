use super::*;
use emx_types::{Cell, Face, Grid, Key};
use std::ffi::OsString;
use std::io::Cursor;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

fn wire(path: &str) -> WirePath {
    WirePath::from(std::path::Path::new(path))
}

fn encode<T: Serialize>(message: &T) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_msg(&mut bytes, message).unwrap();
    bytes
}

fn roundtrip<T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(message: T) {
    let bytes = encode(&message);
    assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
    assert_eq!(bytes.last(), Some(&b'\n'));
    let mut reader = LineReader::new(Cursor::new(bytes));
    let decoded: T = reader.read_msg().unwrap().unwrap();
    assert_eq!(decoded, message);
    assert!(reader.read_msg::<T>().unwrap().is_none());
}

fn sample_key() -> Key {
    emx_types::parse_kbd("C-x").unwrap().remove(0)
}

fn client_messages() -> Vec<ClientMsg> {
    let requests = vec![
        Request::Open {
            path: wire("/a"),
            line: Some(3),
            col: None,
            wait: true,
        },
        Request::ListBuffers,
        Request::Reload { path: None },
        Request::Reload {
            path: Some(wire("/b")),
        },
        Request::Goto {
            path: Some(wire("/c")),
            line: 9,
            col: Some(2),
        },
        Request::SaveAll,
        Request::SubscribeEvents,
        Request::DiffExternal { path: wire("/d") },
    ];
    let mut messages = vec![
        ClientMsg::Hello {
            kind: ClientKind::Tui,
            cwd: wire("/w"),
            cols: 80,
            rows: 24,
            wait_for: Some(vec![wire("/x")]),
        },
        ClientMsg::Hello {
            kind: ClientKind::Cli,
            cwd: wire("/w"),
            cols: 1,
            rows: 1,
            wait_for: None,
        },
        ClientMsg::Key { key: sample_key() },
        ClientMsg::Mouse {
            event: emx_types::MouseEvent {
                kind: emx_types::MouseKind::Drag,
                button: emx_types::MouseButton::Left,
                col: 3,
                row: 4,
                mods: emx_types::Mods {
                    ctrl: true,
                    ..emx_types::Mods::default()
                },
                clicks: 2,
            },
        },
        ClientMsg::Paste {
            data_b64: "aGk=".to_string(),
            r#final: false,
        },
        ClientMsg::Paste {
            data_b64: String::new(),
            r#final: true,
        },
        ClientMsg::Redraw,
        ClientMsg::Resize {
            cols: 100,
            rows: 50,
        },
    ];
    for (id, req) in requests.into_iter().enumerate() {
        messages.push(ClientMsg::Request { id: id as u64, req });
    }
    messages
}

fn server_messages() -> Vec<ServerMsg> {
    let info = BufferInfo {
        name: "n".into(),
        path: Some(wire("/p")),
        modified: true,
        size: 7,
    };
    let row = RowUpdate {
        row: 0,
        cells: vec![Cell::blank()],
    };
    let events = vec![
        EditorEvent::BufferOpened {
            path: None,
            name: "scratch".into(),
        },
        EditorEvent::BufferKilled {
            path: Some(wire("/k")),
            name: "k".into(),
        },
        EditorEvent::BufferSaved {
            path: Some(wire("/s")),
            name: "s".into(),
        },
        EditorEvent::ExternalChange {
            path: wire("/e"),
            kind: ExternalKind::Reloaded,
        },
        EditorEvent::ExternalChange {
            path: wire("/e"),
            kind: ExternalKind::Appended,
        },
        EditorEvent::ExternalChange {
            path: wire("/e"),
            kind: ExternalKind::Conflict,
        },
        EditorEvent::ExternalChange {
            path: wire("/e"),
            kind: ExternalKind::Deleted,
        },
        EditorEvent::ExternalChange {
            path: wire("/e"),
            kind: ExternalKind::Renamed { to: wire("/f") },
        },
        EditorEvent::ExternalChange {
            path: wire("/e"),
            kind: ExternalKind::Truncated,
        },
        EditorEvent::CursorMoved {
            client: 1,
            path: Some(wire("/c")),
            line: Some(2),
            col: 3,
            offset: 4,
        },
        EditorEvent::CursorMoved {
            client: 1,
            path: None,
            line: None,
            col: 0,
            offset: 0,
        },
        EditorEvent::ClientAttached { client: 5 },
        EditorEvent::ClientDetached { client: 5 },
    ];
    let mut messages = vec![
        ServerMsg::Welcome { server_pid: 42 },
        ServerMsg::Frame {
            full: true,
            cols: 1,
            rows: 1,
            rows_changed: vec![row],
            cursor: Some((0, 0)),
        },
        ServerMsg::Frame {
            full: false,
            cols: 1,
            rows: 1,
            rows_changed: vec![],
            cursor: None,
        },
        ServerMsg::Bell,
        ServerMsg::Modes { mouse: true },
        ServerMsg::Response {
            id: 1,
            result: Ok(Response::Ok),
        },
        ServerMsg::Response {
            id: 2,
            result: Ok(Response::Buffers(vec![info])),
        },
        ServerMsg::Response {
            id: 3,
            result: Ok(Response::Diff("--- a".into())),
        },
        ServerMsg::Response {
            id: 4,
            result: Err(WireError {
                code: "no_such_buffer".into(),
                message: "gone".into(),
            }),
        },
        ServerMsg::Exit {
            reason: "bye".into(),
        },
    ];
    messages.extend(events.into_iter().map(|event| ServerMsg::Event { event }));
    messages
}

#[test]
fn every_message_has_version() {
    for message in client_messages() {
        let value: serde_json::Value = serde_json::from_slice(&encode(&message)).unwrap();
        assert_eq!(value["v"], PROTOCOL_VERSION, "{message:?}");
        assert!(value["type"].is_string(), "{message:?}");
    }
    for message in server_messages() {
        let value: serde_json::Value = serde_json::from_slice(&encode(&message)).unwrap();
        assert_eq!(value["v"], PROTOCOL_VERSION, "{message:?}");
        assert!(value["type"].is_string(), "{message:?}");
    }
}

#[test]
fn wrong_version_rejected() {
    // A peer from before version 2 (an old server still running after an
    // upgrade) is refused, not misread.
    let line = b"{\"v\":1,\"type\":\"bell\"}\n".to_vec();
    let mut reader = LineReader::new(Cursor::new(line));
    match reader.read_msg::<ServerMsg>() {
        Err(ProtoError::Version { got: 1, want: 2 }) => {}
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn missing_version_rejected() {
    let mut reader = LineReader::new(Cursor::new(b"{\"type\":\"bell\"}\n".to_vec()));
    assert!(matches!(
        reader.read_msg::<ServerMsg>(),
        Err(ProtoError::Invalid(_))
    ));
}

#[test]
fn wire_path_roundtrips_non_utf8() {
    let raw = vec![b'/', b't', 0xff, 0xfe, b'x'];
    let original = PathBuf::from(OsString::from_vec(raw.clone()));
    let wire_path = WirePath::from(original.as_path());
    let json = serde_json::to_value(&wire_path).unwrap();
    assert!(json.get("bytes_b64").is_some());
    let decoded: WirePath = serde_json::from_value(json).unwrap();
    assert_eq!(decoded.into_path_buf(), original);

    let utf8 = wire("/ok");
    assert_eq!(
        serde_json::to_value(&utf8).unwrap(),
        serde_json::json!("/ok")
    );
    assert_eq!(utf8.into_path_buf(), PathBuf::from("/ok"));
}

#[test]
fn wire_path_rejects_bad_base64() {
    let result: Result<WirePath, _> = serde_json::from_str("{\"bytes_b64\":\"!!!\"}");
    assert!(result.is_err());
}

#[test]
fn oversized_line_rejected() {
    let mut line = vec![b'x'; 64];
    line.push(b'\n');
    let mut reader = LineReader::with_limit(Cursor::new(line), 16);
    assert!(matches!(
        reader.read_msg::<ServerMsg>(),
        Err(ProtoError::Oversized { limit: 16 })
    ));

    let mut unterminated = LineReader::with_limit(Cursor::new(vec![b'x'; 64]), 16);
    assert!(matches!(
        unterminated.read_msg::<ServerMsg>(),
        Err(ProtoError::Oversized { limit: 16 })
    ));
}

#[test]
fn unterminated_short_line_is_truncated() {
    let mut reader = LineReader::new(Cursor::new(b"{\"v\":1".to_vec()));
    assert!(matches!(
        reader.read_msg::<ServerMsg>(),
        Err(ProtoError::Truncated)
    ));
}

fn text_row(text: &str, cols: u16) -> Vec<Cell> {
    (0..cols)
        .map(|_| Cell {
            text: text.into(),
            width: 1,
            face: Face::Default,
        })
        .collect()
}

fn grid_with_rows(rows: &[&str], cols: u16) -> Grid {
    let mut grid = Grid::new(cols, rows.len() as u16);
    for (row, text) in rows.iter().enumerate() {
        for (col, cell) in text_row(text, cols).into_iter().enumerate() {
            grid.set(col as u16, row as u16, cell);
        }
    }
    grid
}

#[test]
fn diff_grids_only_changed_rows() {
    let old = grid_with_rows(&["a", "b", "c"], 4);
    let mut new = old.clone();
    for (col, cell) in text_row("z", 4).into_iter().enumerate() {
        new.set(col as u16, 1, cell);
    }
    new.cursor = Some((2, 1));
    match diff_grids(Some(&old), &new) {
        ServerMsg::Frame {
            full,
            cols,
            rows,
            rows_changed,
            cursor,
        } => {
            assert!(!full);
            assert_eq!((cols, rows), (4, 3));
            assert_eq!(rows_changed.len(), 1);
            assert_eq!(rows_changed[0].row, 1);
            assert_eq!(rows_changed[0].cells, text_row("z", 4));
            assert_eq!(cursor, Some((2, 1)));
        }
        other => panic!("unexpected {other:?}"),
    }
    match diff_grids(Some(&new), &new) {
        ServerMsg::Frame {
            full, rows_changed, ..
        } => {
            assert!(!full);
            assert!(rows_changed.is_empty());
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn diff_grids_full_when_none_or_resized() {
    let new = grid_with_rows(&["a", "b"], 3);
    let smaller = grid_with_rows(&["a"], 3);
    for old in [None, Some(&smaller)] {
        match diff_grids(old, &new) {
            ServerMsg::Frame {
                full, rows_changed, ..
            } => {
                assert!(full);
                assert_eq!(rows_changed.len(), 2);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
fn frame_validate_rejects_long_rows() {
    let good = diff_grids(None, &grid_with_rows(&["a"], 3));
    good.validate(3).unwrap();
    let bad = ServerMsg::Frame {
        full: false,
        cols: 3,
        rows: 1,
        rows_changed: vec![RowUpdate {
            row: 0,
            cells: text_row("a", 4),
        }],
        cursor: None,
    };
    assert!(matches!(bad.validate(3), Err(ProtoError::Invalid(_))));
    let out_of_range = ServerMsg::Frame {
        full: false,
        cols: 3,
        rows: 1,
        rows_changed: vec![RowUpdate {
            row: 1,
            cells: text_row("a", 3),
        }],
        cursor: None,
    };
    assert!(matches!(
        out_of_range.validate(3),
        Err(ProtoError::Invalid(_))
    ));
    ServerMsg::Bell.validate(3).unwrap();
}

#[test]
fn serde_roundtrip_all_variants() {
    for message in client_messages() {
        roundtrip(message);
    }
    for message in server_messages() {
        roundtrip(message);
    }
}

#[test]
fn paste_final_flag_is_named_final_on_the_wire() {
    let mut line = Vec::new();
    write_msg(
        &mut line,
        &ClientMsg::Paste {
            data_b64: "aGk=".to_string(),
            r#final: true,
        },
    )
    .expect("write");
    let text = String::from_utf8(line).expect("json");
    assert!(text.contains("\"final\":true"), "{text}");
}
