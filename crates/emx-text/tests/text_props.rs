use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use emx_text::{
    AddStore, BLOCK_SIZE, BlockCache, FileIdentity, LineIndex, LineLookup, NthNewline, Original,
    ReadError, Sources, Text, index_file,
};
use proptest::prelude::*;

const PROPTEST_CASES: u32 = 512;
const TEST_CACHE_BYTES: usize = 2 * BLOCK_SIZE;
const BLOCK: u64 = BLOCK_SIZE as u64;

#[derive(Debug, Clone)]
enum Op {
    Insert { position: u64, bytes: Vec<u8> },
    Delete { start: u64, length: u64 },
    Read { start: u64, length: u64 },
}

fn byte_run_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        3 => Just(b"\n".to_vec()),
        2 => Just(b"\r\n".to_vec()),
        1 => Just(vec![0xff, 0xfe]),
        1 => Just(vec![0xe2, 0x82]),
        4 => any::<u8>().prop_map(|byte| vec![byte]),
        4 => "[a-z ]{1,8}".prop_map(|text| text.into_bytes()),
    ]
}

fn bytes_strategy(max_runs: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(byte_run_strategy(), 0..max_runs).prop_map(|runs| runs.concat())
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => (any::<u64>(), bytes_strategy(6)).prop_map(|(position, bytes)| Op::Insert { position, bytes }),
        3 => (any::<u64>(), 0u64..40).prop_map(|(start, length)| Op::Delete { start, length }),
        2 => (any::<u64>(), 0u64..60).prop_map(|(start, length)| Op::Read { start, length }),
    ]
}

fn ops_strategy() -> impl Strategy<Value = (Vec<u8>, Vec<Op>)> {
    (
        bytes_strategy(40),
        prop::collection::vec(op_strategy(), 0..60),
    )
}

fn crlf_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(
        prop_oneof![
            Just(b"\r\n".to_vec()),
            "[a-z]{0,12}".prop_map(|text| text.into_bytes()),
            Just(vec![0xc3]),
        ],
        0..40,
    )
    .prop_map(|runs| runs.concat())
}

fn newline_count(bytes: &[u8]) -> u64 {
    bytes.iter().filter(|&&byte| byte == b'\n').count() as u64
}

fn memory_text(content: &[u8]) -> Text {
    Text::from_original(Sources::memory(Arc::from(content)))
}

fn file_text(path: &Path, content: &[u8]) -> Text {
    fs::write(path, content).expect("write fixture");
    let cache = BlockCache::open(path, TEST_CACHE_BYTES).expect("open cache");
    let index = LineIndex::new(cache.len());
    index_file(&cache, &index, &AtomicBool::new(false), &mut |_| {}).expect("index fixture");
    Text::from_original(Sources::file(cache, index))
}

fn read_all(text: &Text) -> Vec<u8> {
    let mut out = Vec::new();
    text.read(0..text.len(), &mut out).expect("read whole text");
    out
}

fn reference_line_start(reference: &[u8], line: u64) -> LineLookup {
    if line == 0 {
        return LineLookup::Found(0);
    }
    let newline_positions: Vec<usize> = memchr_positions(reference);
    match newline_positions.get(line as usize - 1) {
        Some(&position) => LineLookup::Found(position as u64 + 1),
        None => LineLookup::PastEnd,
    }
}

fn memchr_positions(reference: &[u8]) -> Vec<usize> {
    reference
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'\n')
        .map(|(position, _)| position)
        .collect()
}

fn clamp_range(start: u64, length: u64, total: u64) -> Range<u64> {
    let start = if total == 0 { 0 } else { start % (total + 1) };
    let end = (start + length).min(total);
    start..end
}

/// Applies `op` to both `text` and `reference`, returning the new text.
fn apply(text: &Text, reference: &mut Vec<u8>, op: &Op) -> Result<Text, TestCaseError> {
    let total = reference.len() as u64;
    match op {
        Op::Insert { position, bytes } => {
            let at = position % (total + 1);
            let next = text.insert(at, bytes).expect("insert");
            reference.splice(at as usize..at as usize, bytes.iter().copied());
            Ok(next)
        }
        Op::Delete { start, length } => {
            let range = clamp_range(*start, *length, total);
            let next = text.delete(range.clone()).expect("delete");
            reference.drain(range.start as usize..range.end as usize);
            Ok(next)
        }
        Op::Read { start, length } => {
            let range = clamp_range(*start, *length, total);
            let mut out = Vec::new();
            text.read(range.clone(), &mut out).expect("read");
            prop_assert_eq!(
                &out[..],
                &reference[range.start as usize..range.end as usize]
            );
            let mut chunked = Vec::new();
            let mut expected_start = range.start;
            for chunk in text.chunks(range.clone()) {
                let chunk = chunk.expect("chunk");
                prop_assert_eq!(chunk.start, expected_start);
                prop_assert!(!chunk.bytes().is_empty());
                expected_start += chunk.bytes().len() as u64;
                chunked.extend_from_slice(chunk.bytes());
            }
            prop_assert_eq!(&chunked[..], &out[..]);
            Ok(text.clone())
        }
    }
}

fn check_matches(text: &Text, reference: &[u8]) -> Result<(), TestCaseError> {
    prop_assert_eq!(text.len(), reference.len() as u64);
    prop_assert_eq!(read_all(text), reference.to_vec());
    prop_assert_eq!(text.line_count(), Some(newline_count(reference) + 1));
    let piece_total: u64 = text.pieces().map(|(_, piece)| piece.len).sum();
    prop_assert_eq!(piece_total, reference.len() as u64);
    Ok(())
}

/// A query position: either anywhere, or within a few bytes of a block
/// boundary, where file-backed chunking changes.
#[derive(Debug, Clone, Copy)]
enum Probe {
    Anywhere(u64),
    NearBlockBoundary { block: u64, delta: i64 },
}

fn probe_strategy() -> impl Strategy<Value = Probe> {
    prop_oneof![
        any::<u64>().prop_map(Probe::Anywhere),
        (1u64..4, -2i64..=2).prop_map(|(block, delta)| Probe::NearBlockBoundary { block, delta }),
    ]
}

fn probes_strategy() -> impl Strategy<Value = Vec<Probe>> {
    prop::collection::vec(probe_strategy(), 1..4)
}

fn probe_position(probe: Probe, total: u64) -> u64 {
    match probe {
        Probe::Anywhere(value) => value % (total + 1),
        Probe::NearBlockBoundary { block, delta } => {
            (block * BLOCK).saturating_add_signed(delta).min(total)
        }
    }
}

fn check_queries(text: &Text, reference: &[u8], probe: Probe) -> Result<(), TestCaseError> {
    let total = reference.len() as u64;
    let position = probe_position(probe, total);
    let before = &reference[..position as usize];
    prop_assert_eq!(
        text.line_of(position).expect("line_of"),
        Some(newline_count(before))
    );
    let forward = reference[position as usize..]
        .iter()
        .position(|&byte| byte == b'\n')
        .map(|offset| position + offset as u64);
    prop_assert_eq!(
        text.find_newline_forward(position).expect("forward"),
        forward
    );
    let backward = before
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map(|offset| offset as u64);
    prop_assert_eq!(
        text.find_newline_backward(position).expect("backward"),
        backward
    );
    if position < total {
        prop_assert_eq!(
            text.byte_at(position).expect("byte_at"),
            reference[position as usize]
        );
    }
    Ok(())
}

fn run_ops(
    initial: Text,
    initial_content: &[u8],
    ops: &[Op],
    probes: &[Probe],
) -> Result<(), TestCaseError> {
    let mut reference = initial_content.to_vec();
    let mut text = initial;
    let check_all = |text: &Text, reference: &[u8]| -> Result<(), TestCaseError> {
        check_matches(text, reference)?;
        for &probe in probes {
            check_queries(text, reference, probe)?;
        }
        Ok(())
    };
    check_all(&text, &reference)?;
    for op in ops {
        text = apply(&text, &mut reference, op)?;
        check_all(&text, &reference)?;
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(PROPTEST_CASES))]

    #[test]
    fn ops_match_reference((initial, ops) in ops_strategy(), probes in probes_strategy()) {
        run_ops(memory_text(&initial), &initial, &ops, &probes)?;
    }

    #[test]
    fn ops_match_reference_on_empty_text(ops in prop::collection::vec(op_strategy(), 0..60), probes in probes_strategy()) {
        let empty = Text::empty(Sources::memory(Arc::from(&b""[..])));
        run_ops(empty, b"", &ops, &probes)?;
    }

    #[test]
    fn ops_match_reference_on_file_original(
        pattern in bytes_strategy(30),
        repetitions in 0usize..5000,
        ops in prop::collection::vec(op_strategy(), 0..40),
        probes in probes_strategy(),
    ) {
        let directory = tempfile::tempdir().expect("tempdir");
        let content = pattern.repeat(repetitions);
        let text = file_text(&directory.path().join("original.txt"), &content);
        run_ops(text, &content, &ops, &probes)?;
    }

    #[test]
    fn persistence_old_versions_unchanged((initial, ops) in ops_strategy()) {
        let mut reference = initial.clone();
        let mut text = memory_text(&initial);
        let mut history = vec![(text.clone(), reference.clone())];
        for op in &ops {
            text = apply(&text, &mut reference, op)?;
            history.push((text.clone(), reference.clone()));
        }
        for (version, snapshot) in &history {
            check_matches(version, snapshot)?;
        }
    }

    #[test]
    fn roundtrip_crlf_and_no_trailing_newline(content in crlf_strategy()) {
        let text = memory_text(&content);
        prop_assert_eq!(read_all(&text), content.clone());
        prop_assert_eq!(text.line_count(), Some(newline_count(&content) + 1));
    }

    #[test]
    fn line_start_matches_reference((initial, ops) in ops_strategy(), line in 0u64..200) {
        let mut reference = initial.clone();
        let mut text = memory_text(&initial);
        for op in &ops {
            text = apply(&text, &mut reference, op)?;
        }
        prop_assert_eq!(text.line_start(line).expect("line_start"), reference_line_start(&reference, line));
    }

    #[test]
    fn line_start_matches_reference_on_file_original(
        pattern in bytes_strategy(30),
        repetitions in 0usize..5000,
        ops in prop::collection::vec(op_strategy(), 0..20),
        line in 0u64..20_000,
    ) {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut reference = pattern.repeat(repetitions);
        let mut text = file_text(&directory.path().join("original.txt"), &reference);
        for op in &ops {
            text = apply(&text, &mut reference, op)?;
        }
        prop_assert_eq!(text.line_start(line).expect("line_start"), reference_line_start(&reference, line));
    }
}

fn write_file(path: &Path, content: &[u8]) {
    let mut file = File::create(path).expect("create file");
    file.write_all(content).expect("write file");
    file.sync_all().expect("sync file");
}

/// Deterministic pseudo-random content with a newline roughly every 40 bytes.
fn generated_content(length: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    (0..length)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let value = (state >> 33) as u8;
            if value.is_multiple_of(40) {
                b'\n'
            } else {
                value
            }
        })
        .collect()
}

#[test]
fn block_cache_truncation_detected() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("three.bin");
    write_file(&path, &vec![b'x'; 3 * BLOCK_SIZE]);
    let cache = BlockCache::open(&path, TEST_CACHE_BYTES).expect("open");
    assert_eq!(cache.len(), 3 * BLOCK);
    let file = OpenOptions::new().write(true).open(&path).expect("reopen");
    file.set_len(BLOCK).expect("truncate");
    match cache.get(2) {
        Err(ReadError::Truncated {
            expected_len,
            actual_len,
        }) => {
            assert_eq!(expected_len, 3 * BLOCK);
            assert_eq!(actual_len, BLOCK);
        }
        other => panic!("expected Truncated, got {other:?}"),
    }
    assert!(cache.get(0).is_ok());
}

#[test]
fn block_cache_survives_rename_replacement() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("a.txt");
    write_file(&path, b"old contents\n");
    let cache = BlockCache::open(&path, TEST_CACHE_BYTES).expect("open");
    let replacement = directory.path().join("a.txt.tmp");
    write_file(&replacement, b"brand new and longer contents\n");
    fs::rename(&replacement, &path).expect("rename");
    let block = cache.get(0).expect("get after rename");
    assert_eq!(&block[..], b"old contents\n");
    assert_ne!(
        cache.identity(),
        FileIdentity::of_path(&path).expect("identity")
    );
}

#[test]
fn block_cache_evicts_by_bytes_and_invalidates() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("four.bin");
    let content = generated_content(4 * BLOCK_SIZE, 7);
    write_file(&path, &content);
    let cache = BlockCache::open(&path, TEST_CACHE_BYTES).expect("open");
    for block in 0..3 {
        let data = cache.get(block).expect("get");
        let start = block as usize * BLOCK_SIZE;
        assert_eq!(&data[..], &content[start..start + BLOCK_SIZE]);
    }
    assert!(
        cache.try_get_cached(0).is_none(),
        "block 0 evicted by capacity"
    );
    assert!(cache.try_get_cached(1).is_some());
    assert!(cache.try_get_cached(2).is_some());
    cache.invalidate_from(2 * BLOCK + 5);
    assert!(cache.try_get_cached(2).is_none());
    assert!(cache.try_get_cached(1).is_some());
}

#[test]
fn block_cache_set_len_reads_appended_bytes() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("grow.txt");
    write_file(&path, b"first\n");
    let cache = BlockCache::open(&path, TEST_CACHE_BYTES).expect("open");
    assert_eq!(&cache.get(0).expect("get")[..], b"first\n");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open append");
    file.write_all(b"second\n").expect("append");
    cache.set_len(13);
    assert_eq!(cache.len(), 13);
    assert_eq!(
        &cache.get(0).expect("get after set_len")[..],
        b"first\nsecond\n"
    );
}

#[test]
fn file_identity_matches_between_file_and_path() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("id.txt");
    write_file(&path, b"12345");
    let from_file = FileIdentity::of_file(&File::open(&path).expect("open")).expect("of_file");
    let from_path = FileIdentity::of_path(&path).expect("of_path");
    assert_eq!(from_file, from_path);
    assert_eq!(from_file.size, 5);
}

#[test]
fn add_store_appends_and_reads() {
    let store = AddStore::new();
    assert_eq!(store.append(b"hello"), 0);
    assert_eq!(store.append(b" world"), 5);
    assert_eq!(store.len(), 11);
    let mut out = b">".to_vec();
    store.read_into(3, 5, &mut out);
    assert_eq!(out, b">lo wo");
}

#[test]
fn index_counts_match_memchr() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("three_mb.txt");
    let length = 3 * 1024 * 1024 + 123;
    let content = generated_content(length, 42);
    write_file(&path, &content);
    let cache = BlockCache::open(&path, TEST_CACHE_BYTES).expect("open");
    let index = LineIndex::new(cache.len());
    let mut progress_calls = Vec::new();
    index_file(&cache, &index, &AtomicBool::new(false), &mut |done| {
        progress_calls.push(done)
    })
    .expect("index");
    assert!(index.is_complete());
    assert_eq!(index.block_count(), length.div_ceil(BLOCK_SIZE) as u64);
    assert_eq!(index.indexed_prefix_blocks(), index.block_count());
    assert_eq!(progress_calls.last().copied(), Some(index.block_count()));
    for block in 0..index.block_count() {
        let start = block as usize * BLOCK_SIZE;
        let end = (start + BLOCK_SIZE).min(length);
        assert_eq!(
            index.get_block(block),
            Some(newline_count(&content[start..end]) as u32)
        );
    }
    assert!(
        cache.try_get_cached(0).is_none(),
        "indexer bypasses the cache"
    );
    let whole = index.count_range(&cache, 0..length as u64).expect("count");
    assert_eq!(whole, Some(newline_count(&content)));
    let memory_index = LineIndex::fully_indexed(&content);
    assert!(memory_index.is_complete());
    for block in 0..index.block_count() {
        assert_eq!(memory_index.get_block(block), index.get_block(block));
    }
}

#[test]
fn index_file_honours_cancel() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("cancel.txt");
    write_file(&path, &generated_content(3 * BLOCK_SIZE, 3));
    let cache = BlockCache::open(&path, TEST_CACHE_BYTES).expect("open");
    let index = LineIndex::new(cache.len());
    index_file(&cache, &index, &AtomicBool::new(true), &mut |_| {}).expect("index");
    assert!(!index.is_complete());
    assert_eq!(index.indexed_prefix_blocks(), 0);
}

#[test]
fn index_file_reports_truncation_without_panic() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("shrinks.txt");
    write_file(&path, &generated_content(4 * BLOCK_SIZE, 5));
    let cache = BlockCache::open(&path, TEST_CACHE_BYTES).expect("open");
    let index = LineIndex::new(cache.len());
    let file = OpenOptions::new().write(true).open(&path).expect("reopen");
    let mut truncate_after_first = Some(file);
    let result = index_file(&cache, &index, &AtomicBool::new(false), &mut |_| {
        if let Some(file) = truncate_after_first.take() {
            file.set_len(BLOCK + 10).expect("truncate");
        }
    });
    match result {
        Err(ReadError::Truncated {
            expected_len,
            actual_len,
        }) => {
            assert_eq!(expected_len, 4 * BLOCK);
            assert_eq!(actual_len, BLOCK + 10);
        }
        other => panic!("expected Truncated, got {other:?}"),
    }
}

#[test]
fn nth_newline_across_blocks() {
    let mut content = vec![b'a'; 3 * BLOCK_SIZE];
    let positions = [10usize, BLOCK_SIZE - 1, BLOCK_SIZE, 2 * BLOCK_SIZE + 500];
    for &position in &positions {
        content[position] = b'\n';
    }
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("nth.txt");
    write_file(&path, &content);
    let cache = BlockCache::open(&path, TEST_CACHE_BYTES).expect("open");
    let index = LineIndex::new(cache.len());
    index_file(&cache, &index, &AtomicBool::new(false), &mut |_| {}).expect("index");
    let all = 0..content.len() as u64;
    for (n, &position) in positions.iter().enumerate() {
        assert_eq!(
            index
                .nth_newline(&cache, all.clone(), n as u64)
                .expect("nth"),
            NthNewline::Found(position as u64)
        );
    }
    assert_eq!(
        index.nth_newline(&cache, all, 4).expect("nth"),
        NthNewline::NotInRange { count: 4 }
    );
    let middle = 11..(2 * BLOCK + 500);
    assert_eq!(
        index.nth_newline(&cache, middle.clone(), 0).expect("nth"),
        NthNewline::Found(BLOCK - 1)
    );
    assert_eq!(
        index.nth_newline(&cache, middle, 2).expect("nth"),
        NthNewline::NotInRange { count: 2 }
    );
}

#[test]
fn count_range_partial_blocks() {
    let content = generated_content(3 * BLOCK_SIZE + 77, 9);
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("partial.txt");
    write_file(&path, &content);
    let cache = BlockCache::open(&path, TEST_CACHE_BYTES).expect("open");
    let index = LineIndex::new(cache.len());
    index_file(&cache, &index, &AtomicBool::new(false), &mut |_| {}).expect("index");
    let ranges = [
        0..0,
        5..6,
        100..BLOCK - 3,
        BLOCK - 3..BLOCK + 3,
        17..3 * BLOCK + 70,
        BLOCK..2 * BLOCK,
        2 * BLOCK + 1..3 * BLOCK + 77,
    ];
    for range in ranges {
        let expected = newline_count(&content[range.start as usize..range.end as usize]);
        assert_eq!(
            index.count_range(&cache, range.clone()).expect("count"),
            Some(expected),
            "range {range:?}"
        );
    }
}

#[test]
fn unknown_before_indexed() {
    let content = generated_content(3 * BLOCK_SIZE, 11);
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("lazy.txt");
    write_file(&path, &content);
    let cache = BlockCache::open(&path, TEST_CACHE_BYTES).expect("open");
    let index = LineIndex::new(cache.len());
    assert_eq!(index.block_count(), 3);
    assert!(!index.is_complete());
    assert_eq!(index.count_range(&cache, 0..10).expect("count"), None);

    let first_block_newlines = newline_count(&content[..BLOCK_SIZE]);
    index.set_block(0, first_block_newlines as u32);
    index.set_block(2, newline_count(&content[2 * BLOCK_SIZE..]) as u32);
    assert_eq!(index.indexed_prefix_blocks(), 1);
    assert_eq!(index.get_block(1), None);
    assert_eq!(
        index.count_range(&cache, 0..BLOCK).expect("count"),
        Some(first_block_newlines)
    );
    assert_eq!(
        index.count_range(&cache, 0..BLOCK + 1).expect("count"),
        None
    );
    let first_position = content
        .iter()
        .position(|&byte| byte == b'\n')
        .expect("newline");
    assert_eq!(
        index.nth_newline(&cache, 0..3 * BLOCK, 0).expect("nth"),
        NthNewline::Found(first_position as u64)
    );
    assert_eq!(
        index
            .nth_newline(&cache, 0..3 * BLOCK, first_block_newlines)
            .expect("nth"),
        NthNewline::Unknown
    );

    let sources = Sources::file(cache.clone(), index.clone());
    let text = Text::from_original(sources);
    assert_eq!(text.line_count(), None);
    assert_eq!(
        text.line_start(1).expect("line_start"),
        LineLookup::Found(first_position as u64 + 1)
    );
    assert_eq!(
        text.line_start(first_block_newlines + 1)
            .expect("line_start"),
        LineLookup::Unknown
    );
    assert_eq!(
        text.line_of(BLOCK).expect("line_of"),
        Some(first_block_newlines)
    );
    assert_eq!(text.line_of(BLOCK + 1).expect("line_of"), None);

    index.set_block(
        1,
        newline_count(&content[BLOCK_SIZE..2 * BLOCK_SIZE]) as u32,
    );
    assert!(index.is_complete());
    assert_eq!(text.line_count(), None, "old text keeps its unknown piece");
    let refreshed = text.refresh_newlines();
    assert_eq!(refreshed.line_count(), Some(newline_count(&content) + 1));
    assert_eq!(read_all(&refreshed), content);
}

#[test]
fn line_index_extend_reopens_partial_last_block() {
    let index = LineIndex::fully_indexed(b"ab\ncd");
    assert!(index.is_complete());
    index.extend(BLOCK + 4);
    assert_eq!(index.block_count(), 2);
    assert_eq!(index.get_block(0), None);
    assert_eq!(index.get_block(1), None);
    assert_eq!(index.indexed_prefix_blocks(), 0);
}

#[test]
fn read_cached_never_does_io() {
    let content = generated_content(2 * BLOCK_SIZE + 9, 13);
    let directory = tempfile::tempdir().expect("tempdir");
    let text = file_text(&directory.path().join("cached.txt"), &content);
    let mut out = Vec::new();
    match text.read_cached(BLOCK + 2..BLOCK + 8, &mut out) {
        Err(ReadError::NotCached { block }) => assert_eq!(block, 1),
        other => panic!("expected NotCached, got {other:?}"),
    }
    assert!(out.is_empty());
    let mut warm = Vec::new();
    text.read(BLOCK + 2..BLOCK + 8, &mut warm).expect("read");
    text.read_cached(BLOCK + 2..BLOCK + 8, &mut out)
        .expect("read_cached");
    assert_eq!(out, &content[BLOCK_SIZE + 2..BLOCK_SIZE + 8]);

    let edited = text.insert(3, b"typed").expect("insert");
    let mut typed = Vec::new();
    edited
        .read_cached(3..8, &mut typed)
        .expect("add bytes need no io");
    assert_eq!(typed, b"typed");
}

#[test]
fn typing_coalesces_into_one_add_piece() {
    let text = memory_text(b"0123456789");
    let mut current = text.clone();
    for (offset, byte) in b"hello\nworld".iter().enumerate() {
        current = current.insert(4 + offset as u64, &[*byte]).expect("insert");
    }
    assert_eq!(read_all(&current), b"0123hello\nworld456789");
    assert_eq!(current.pieces().count(), 3);
    assert_eq!(current.line_count(), Some(2));
    assert_eq!(read_all(&text), b"0123456789");
    assert_eq!(text.pieces().count(), 1);
}

#[test]
fn same_root_tracks_identity() {
    let text = memory_text(b"abc");
    let copy = text.clone();
    assert!(text.same_root(&copy));
    let unchanged = text.insert(1, b"").expect("empty insert");
    assert!(text.same_root(&unchanged));
    let changed = text.insert(1, b"x").expect("insert");
    assert!(!text.same_root(&changed));
    let deleted = changed.delete(1..2).expect("delete");
    assert!(!changed.same_root(&deleted));
    assert_eq!(read_all(&deleted), b"abc");
}

#[test]
fn shared_add_store_and_original_bytes() {
    let sources = Sources::memory(Arc::from(&b"line one\nline two\n"[..]));
    assert!(matches!(sources.original, Original::Memory(_)));
    assert!(sources.index.is_complete());
    let text = Text::from_original(sources.clone());
    let edited = text.insert(9, b"inserted\n").expect("insert");
    assert_eq!(sources.add.len(), 9);
    assert_eq!(
        edited.line_start(2).expect("line_start"),
        LineLookup::Found(18)
    );
    assert_eq!(
        edited.line_start(4).expect("line_start"),
        LineLookup::PastEnd
    );
    assert_eq!(
        edited.line_start(3).expect("line_start"),
        LineLookup::Found(27)
    );
}

#[test]
fn newline_search_crosses_block_boundaries_in_file_original() {
    let mut content = vec![b'x'; 3 * BLOCK_SIZE];
    content[BLOCK_SIZE - 1] = b'\n';
    content[2 * BLOCK_SIZE] = b'\n';
    let directory = tempfile::tempdir().expect("tempdir");
    let text = file_text(&directory.path().join("boundary.txt"), &content);
    assert_eq!(
        text.find_newline_backward(BLOCK).expect("backward"),
        Some(BLOCK - 1)
    );
    assert_eq!(
        text.find_newline_backward(BLOCK - 1).expect("backward"),
        None
    );
    assert_eq!(
        text.find_newline_backward(3 * BLOCK).expect("backward"),
        Some(2 * BLOCK)
    );
    assert_eq!(
        text.find_newline_backward(2 * BLOCK).expect("backward"),
        Some(BLOCK - 1)
    );
    assert_eq!(
        text.find_newline_forward(BLOCK).expect("forward"),
        Some(2 * BLOCK)
    );
    assert_eq!(
        text.find_newline_forward(BLOCK - 1).expect("forward"),
        Some(BLOCK - 1)
    );
    assert_eq!(
        text.find_newline_forward(2 * BLOCK + 1).expect("forward"),
        None
    );
    assert_eq!(
        text.line_start(2).expect("line_start"),
        LineLookup::Found(2 * BLOCK + 1)
    );
    assert_eq!(text.line_of(2 * BLOCK + 1).expect("line_of"), Some(2));
    let chunk_starts: Vec<u64> = text
        .chunks(BLOCK - 2..2 * BLOCK + 2)
        .map(|chunk| chunk.expect("chunk").start)
        .collect();
    assert_eq!(chunk_starts, vec![BLOCK - 2, BLOCK, 2 * BLOCK]);
}
