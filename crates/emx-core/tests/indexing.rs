//! Background line indexing (M4 task 21): a large file opens before it is
//! scanned, the modeline is honest about line numbers it does not yet know,
//! and a `goto-line` past the counted prefix waits for the indexer.

use std::fs::{self, File};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use emx_core::io::load_file;
use emx_core::testing::Harness;
use emx_core::{BufferId, Config, Event, PrefetchRequest};
use emx_text::{BLOCK_SIZE, BlockCache, Original, index_file};

/// One byte over the small-file threshold forces the block-cache path. The
/// fixtures here are a few hundred KiB, so a tiny limit keeps them off the
/// in-memory path.
const SMALL_IN_MEMORY_MAX: u64 = 1024;
/// Fixed-width lines make the byte offset of every line exact: line `i`
/// (0-based) starts at `i * LINE_WIDTH`.
const LINE_WIDTH: usize = 64;
/// `LINE_WIDTH` divides `BLOCK_SIZE`, so each full block holds exactly this
/// many newlines.
const LINES_PER_BLOCK: u64 = BLOCK_SIZE as u64 / LINE_WIDTH as u64;

fn config() -> Config {
    Config {
        in_memory_max: SMALL_IN_MEMORY_MAX,
        ..Config::default()
    }
}

/// `line_count` fixed-width lines, each a run of `a` ended by a newline.
fn fixed_width_lines(line_count: u64) -> Vec<u8> {
    let mut content = Vec::with_capacity(line_count as usize * LINE_WIDTH);
    for _ in 0..line_count {
        content.extend(std::iter::repeat_n(b'a', LINE_WIDTH - 1));
        content.push(b'\n');
    }
    content
}

/// Opens `path` in a fresh harness through the block-cache path, as `C-x C-f`
/// of a large file does.
fn open_large(directory: &Path, path: &Path) -> Harness {
    let mut harness = Harness::with_config(config());
    harness.set_default_directory(directory);
    harness.keys("C-x C-f C-a C-k");
    harness.type_text(path.to_str().expect("test paths are UTF-8"));
    harness.keys("C-j");
    harness
}

fn modeline(harness: &Harness) -> String {
    let screen = harness.screen();
    screen[screen.len() - 2].clone()
}

/// Fills blocks `[0, upto)` of the buffer's shared line index with the known
/// per-block newline count, then delivers the progress the background indexer
/// would, so the editor refreshes as the real server would on these events.
fn advance_index(harness: &mut Harness, buffer: BufferId, upto: u64, complete: bool) {
    let index = harness.buffer().text.sources().index.clone();
    let total = index.block_count();
    for block in 0..upto {
        if index.get_block(block).is_none() {
            index.set_block(block, LINES_PER_BLOCK as u32);
        }
    }
    harness.handle(Event::IndexProgress {
        buffer,
        indexed_blocks: upto,
        total_blocks: total,
        complete,
    });
}

/// The block cache behind the buffer's file-backed text.
fn buffer_cache(harness: &Harness) -> Arc<BlockCache> {
    match &harness.buffer().text.sources().original {
        Original::File(cache) => cache.clone(),
        Original::Memory(_) => panic!("fixture is file-backed"),
    }
}

#[test]
fn large_file_opens_without_reading_whole() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("huge.log");
    // A sparse ~1 GiB file: `set_len` punches a hole, so the file is a
    // gibibyte long but costs almost no disk, and its holes read back as
    // zero bytes (no newlines).
    const GIBIBYTE: u64 = 1 << 30;
    {
        let file = File::create(&path).expect("create");
        file.set_len(GIBIBYTE).expect("set length");
    }

    // Opening must not read or index the whole file: it returns at once with
    // a buffer whose index is empty.
    let loaded = load_file(&path, &config()).expect("load");
    assert!(
        loaded.needs_index,
        "a gibibyte file is read through the cache"
    );
    assert!(
        !loaded.sources.index.is_complete(),
        "opening does not wait for the line index"
    );
    assert_eq!(loaded.sources.index.file_len(), GIBIBYTE);
    let total_blocks = loaded.sources.index.block_count();
    assert_eq!(total_blocks, GIBIBYTE / BLOCK_SIZE as u64);

    // Mirror the server's event stream for this buffer. `FileLoaded` is
    // delivered when the open finishes; `IndexProgress` only afterwards, as
    // the background indexer runs. The terminal `complete` report is last.
    #[derive(PartialEq, Eq)]
    enum Step {
        FileLoaded,
        IndexProgress { complete: bool },
    }
    let mut log = vec![Step::FileLoaded];
    let cache = match &loaded.sources.original {
        Original::File(cache) => cache.clone(),
        Original::Memory(_) => panic!("a gibibyte file is file-backed"),
    };
    let index = loaded.sources.index.clone();
    let cancel = AtomicBool::new(false);
    let mut report = |_indexed_blocks: u64| {
        log.push(Step::IndexProgress { complete: false });
    };
    index_file(cache.as_ref(), index.as_ref(), &cancel, &mut report).expect("index");
    assert!(index.is_complete(), "the full scan counts every block");
    log.push(Step::IndexProgress { complete: true });

    let loaded_at = log
        .iter()
        .position(|step| *step == Step::FileLoaded)
        .expect("FileLoaded is logged");
    let complete_at = log
        .iter()
        .position(|step| *step == Step::IndexProgress { complete: true })
        .expect("a terminal progress is logged");
    assert!(
        loaded_at < complete_at,
        "the file opens before the index completes"
    );
}

#[test]
fn modeline_shows_l_unknown_then_number() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("lines.log");
    let line_count = 3 * LINES_PER_BLOCK;
    fs::write(&path, fixed_width_lines(line_count)).expect("write fixture");

    let mut harness = open_large(directory.path(), &path);
    let buffer = harness.buffer().id;

    // Jump to the end before any block is counted: its line number is not yet
    // known, so the modeline shows `L?`.
    harness.keys("M->");
    assert_eq!(harness.point(), line_count * LINE_WIDTH as u64);
    assert!(
        modeline(&harness).contains("L?"),
        "unknown line shows L?: {}",
        modeline(&harness)
    );

    // Once the whole file is indexed, the real line number appears. The file
    // ends in a newline, so point sits on the empty line after the last one.
    advance_index(&mut harness, buffer, 3, true);
    let expected_line = line_count + 1;
    let line_field = format!("L{expected_line}");
    assert!(
        modeline(&harness).contains(&line_field),
        "known line shows {line_field}: {}",
        modeline(&harness)
    );
    assert!(!modeline(&harness).contains("L?"));
}

#[test]
fn goto_line_pending_then_completes() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("lines.log");
    let line_count = 3 * LINES_PER_BLOCK;
    fs::write(&path, fixed_width_lines(line_count)).expect("write fixture");

    let mut harness = open_large(directory.path(), &path);
    let buffer = harness.buffer().id;

    // Count the first two blocks, leaving the third (holding the target line)
    // unknown.
    advance_index(&mut harness, buffer, 2, false);

    // Line 2500 lives in the uncounted third block: the jump must wait.
    let target_line = 2 * LINES_PER_BLOCK + 452;
    harness.keys("M-g g");
    harness.type_text(&target_line.to_string());
    harness.keys("RET");
    assert_eq!(
        harness.point(),
        0,
        "point does not move while the line waits"
    );
    assert!(
        harness
            .echo()
            .starts_with(&format!("Indexing to line {target_line}...")),
        "pending goto reports progress: {}",
        harness.echo()
    );

    // The indexer reaches the third block: the jump completes.
    advance_index(&mut harness, buffer, 3, true);
    assert_eq!(harness.point(), (target_line - 1) * LINE_WIDTH as u64);
    assert_eq!(harness.echo(), "", "the pending message is cleared");
}

#[test]
fn goto_line_cancel() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("lines.log");
    let line_count = 3 * LINES_PER_BLOCK;
    fs::write(&path, fixed_width_lines(line_count)).expect("write fixture");

    let mut harness = open_large(directory.path(), &path);
    let buffer = harness.buffer().id;
    advance_index(&mut harness, buffer, 2, false);

    let target_line = 2 * LINES_PER_BLOCK + 452;
    harness.keys("M-g g");
    harness.type_text(&target_line.to_string());
    harness.keys("RET");
    assert_eq!(harness.point(), 0);

    // C-g abandons the pending jump.
    harness.keys("C-g");
    assert_eq!(harness.echo(), "Quit");

    // Even once the indexer reaches the line, the cancelled jump does not fire.
    advance_index(&mut harness, buffer, 3, true);
    assert_eq!(harness.point(), 0, "a cancelled goto never moves point");
}

#[test]
fn modeline_badge_shows_indexing_percent() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("lines.log");
    let line_count = 3 * LINES_PER_BLOCK;
    fs::write(&path, fixed_width_lines(line_count)).expect("write fixture");

    let mut harness = open_large(directory.path(), &path);
    let buffer = harness.buffer().id;

    // One of three blocks counted: the modeline carries the badge at 33%.
    advance_index(&mut harness, buffer, 1, false);
    assert!(
        modeline(&harness).contains("[Indexing 33%]"),
        "indexing badge shows the counted fraction: {}",
        modeline(&harness)
    );

    // Once the whole file is indexed, the badge is gone.
    advance_index(&mut harness, buffer, 3, true);
    assert!(
        !modeline(&harness).contains("Indexing"),
        "the badge clears when indexing completes: {}",
        modeline(&harness)
    );
}

#[test]
fn goto_line_index_failure_cancels_pending() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("lines.log");
    let line_count = 3 * LINES_PER_BLOCK;
    fs::write(&path, fixed_width_lines(line_count)).expect("write fixture");

    let mut harness = open_large(directory.path(), &path);
    let buffer = harness.buffer().id;
    advance_index(&mut harness, buffer, 2, false);

    let target_line = 2 * LINES_PER_BLOCK + 452;
    harness.keys("M-g g");
    harness.type_text(&target_line.to_string());
    harness.keys("RET");
    assert_eq!(harness.point(), 0);

    // The indexer fails: the pending jump is abandoned instead of waiting
    // forever for a line that will never be counted.
    harness.handle(Event::IndexFinished {
        buffer,
        result: Err("i/o error".to_string()),
    });
    assert_eq!(harness.echo(), "Cannot reach line: indexing failed");

    // A later block becoming cached must not revive the cancelled jump.
    harness.handle(Event::BlockReady {
        buffer,
        block: 2,
        result: Ok(()),
    });
    assert_eq!(harness.point(), 0, "a failed-index goto never moves point");
}

#[test]
fn goto_line_completion_prefetches_uncached_block() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("lines.log");
    // Five blocks: the target line sits in a middle block that opening does
    // not cache (only the first, shown, block and the tail block are).
    let line_count = 5 * LINES_PER_BLOCK;
    fs::write(&path, fixed_width_lines(line_count)).expect("write fixture");

    let mut harness = open_large(directory.path(), &path);
    let buffer = harness.buffer().id;
    advance_index(&mut harness, buffer, 2, false);

    let target_line = 3 * LINES_PER_BLOCK + 500;
    let target_block = (target_line - 1) * LINE_WIDTH as u64 / BLOCK_SIZE as u64;
    harness.keys("M-g g");
    harness.type_text(&target_line.to_string());
    harness.keys("RET");
    assert_eq!(harness.point(), 0);
    harness.take_prefetches();

    // Indexing completes, but the block holding the line is not cached. The
    // autonomous progress path must not read it on the editor thread: it asks
    // for a prefetch and keeps waiting rather than blocking.
    advance_index(&mut harness, buffer, 5, true);
    let prefetches = harness.take_prefetches();
    assert!(
        prefetches.contains(&PrefetchRequest {
            buffer,
            block: target_block,
        }),
        "the uncached landing block is prefetched: {prefetches:?}"
    );
    assert_eq!(
        harness.point(),
        0,
        "the jump waits for the block, not an I/O stall"
    );

    // Once that block is cached, the block-ready event completes the jump.
    let cache = buffer_cache(&harness);
    cache.get(target_block).expect("read the landing block");
    harness.handle(Event::BlockReady {
        buffer,
        block: target_block,
        result: Ok(()),
    });
    assert_eq!(harness.point(), (target_line - 1) * LINE_WIDTH as u64);
}
