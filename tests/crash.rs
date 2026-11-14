//! A daemon crash loses nothing (DESIGN section 2): edits are journaled as
//! they are made and replayed by the next server.

mod common;

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use common::{RawClient, TestEnv, WAIT};
use emx_core::Config;
use emx_core::journal::{RecoveredBuffer, replay_dir};
use emx_proto::{BufferInfo, ClientKind, Request, Response, WirePath};

const LINES_PER_BUFFER: usize = 5;
/// With its newline, each typed line is 50 characters: 250 per buffer,
/// 500 in all.
const LINE_CHARACTERS: usize = 49;
const POLL: Duration = Duration::from_millis(20);

const ALPHABET: usize = 26;

/// Lines of letters, starting `shift` letters into the alphabet.
fn typed_lines(shift: usize) -> Vec<String> {
    (0..LINES_PER_BUFFER)
        .map(|line| {
            (0..LINE_CHARACTERS)
                .map(|column| {
                    let offset = (line * LINE_CHARACTERS + column + shift) % ALPHABET;
                    char::from(b'a' + offset as u8)
                })
                .collect()
        })
        .collect()
}

fn type_lines(client: &mut RawClient, lines: &[String]) {
    for line in lines {
        client.type_text(line);
        client.keys("RET");
    }
}

/// The journal holds `expected` for every path, as a replay sees it now.
fn journaled(journal: &Path, expected: &[(&Path, String)]) -> bool {
    let Ok(recovered) = replay_dir(journal, &Config::default()) else {
        return false;
    };
    expected.iter().all(|(path, text)| {
        recovered.iter().any(|buffer| match buffer {
            RecoveredBuffer::Rebuilt {
                path: Some(journaled),
                text: content,
                ..
            } if journaled == path => {
                let mut bytes = Vec::new();
                content
                    .read(0..content.len(), &mut bytes)
                    .is_ok_and(|()| bytes == text.as_bytes())
            }
            _ => false,
        })
    })
}

#[test]
fn sigkill_server_mid_edit_recovers_everything() {
    let env = TestEnv::new();
    let first = env.file("first.txt", "alpha\n");
    let second = env.file("second.txt", "beta\n");
    let first_lines = typed_lines(0);
    let second_lines = typed_lines(13);
    let mut server = env.start_server();
    let mut client = env.connect(ClientKind::Tui);
    client.open(&first);
    type_lines(&mut client, &first_lines);
    client.open(&second);
    type_lines(&mut client, &second_lines);
    client.wait_text(&second_lines[LINES_PER_BUFFER - 1]);
    let expected_first = format!("{}\nalpha\n", first_lines.join("\n"));
    let expected_second = format!("{}\nbeta\n", second_lines.join("\n"));
    // The journal thread writes each edit as it comes; wait until it has
    // written the last one, then crash.
    let journal = env.journal_dir();
    let expected = [
        (first.as_path(), expected_first.clone()),
        (second.as_path(), expected_second.clone()),
    ];
    let deadline = Instant::now() + WAIT;
    while !journaled(&journal, &expected) {
        assert!(Instant::now() < deadline, "the journal never caught up");
        thread::sleep(POLL);
    }
    server.kill9();
    drop(client);

    let _restarted = env.start_server();
    let mut client = env.connect(ClientKind::Tui);
    let Ok(Response::Buffers(buffers)) = client.call(Request::ListBuffers) else {
        panic!("expected a buffer list");
    };
    for (path, text) in &expected {
        let name = path
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        assert!(
            buffers.contains(&BufferInfo {
                name,
                path: Some(WirePath::from(*path)),
                modified: true,
                size: text.len() as u64,
            }),
            "{buffers:?}"
        );
    }
    client.open(&first);
    let screen = client.wait_text(&first_lines[0]);
    let shown: Vec<&str> = screen[..=LINES_PER_BUFFER]
        .iter()
        .map(|row| row.trim_end())
        .collect();
    let mut wanted: Vec<&str> = first_lines.iter().map(String::as_str).collect();
    wanted.push("alpha");
    assert_eq!(shown, wanted);
    assert!(client.modeline().contains("**"), "{}", client.modeline());
    client.open(&second);
    let screen = client.wait_text(&second_lines[0]);
    let shown: Vec<&str> = screen[..=LINES_PER_BUFFER]
        .iter()
        .map(|row| row.trim_end())
        .collect();
    let mut wanted: Vec<&str> = second_lines.iter().map(String::as_str).collect();
    wanted.push("beta");
    assert_eq!(shown, wanted);
    assert_eq!(
        std::fs::read_to_string(&first).expect("read"),
        "alpha\n",
        "the files on disk were never touched"
    );
}
