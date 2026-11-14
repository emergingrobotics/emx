//! The search engine: Emacs regexp translation, forward and backward search
//! with Emacs semantics, chunk overlap, the match cap, cancellation and
//! reads through the block cache.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use emx_text::search::{
    CompiledPattern, Match, Pattern, PatternError, SEARCH_CHUNK, SearchError, captures,
    emacs_regex_to_rust, match_at, matches_in, regexp_quote, search_backward, search_forward,
};
use emx_text::{BLOCK_SIZE, BlockCache, LineIndex, ReadError, Sources, Text, index_file};

const CAP: usize = 64 * 1024;

fn text(content: &[u8]) -> Text {
    Text::from_original(Sources::memory(Arc::from(content)))
}

fn literal(needle: &str) -> CompiledPattern {
    CompiledPattern::compile(&Pattern::Literal {
        needle: needle.as_bytes().to_vec(),
        case_fold: false,
    })
    .expect("literal compiles")
}

fn regex(emacs: &str) -> CompiledPattern {
    CompiledPattern::compile(&Pattern::Regex {
        emacs: emacs.to_string(),
        case_fold: false,
    })
    .expect("regexp compiles")
}

fn folded_regex(emacs: &str) -> CompiledPattern {
    CompiledPattern::compile(&Pattern::Regex {
        emacs: emacs.to_string(),
        case_fold: true,
    })
    .expect("regexp compiles")
}

fn not_cancelled() -> AtomicBool {
    AtomicBool::new(false)
}

fn forward(content: &Text, pattern: &CompiledPattern, from: u64) -> Option<(u64, u64)> {
    search_forward(content, pattern, from, None, &not_cancelled(), CAP)
        .expect("in-memory search")
        .map(|found| (found.start, found.end))
}

fn backward(content: &Text, pattern: &CompiledPattern, point: u64) -> Option<(u64, u64)> {
    search_backward(content, pattern, point, None, &not_cancelled(), CAP)
        .expect("in-memory search")
        .map(|found| (found.start, found.end))
}

#[test]
fn emacs_regex_translation_table() {
    let any = r"(?:[^\n]|(?-u:[\x80-\xFF]))";
    let pairs: [(&str, String); 33] = [
        ("abc", "abc".into()),
        ("a.c", format!("a{any}c")),
        (r"\(ab\)", "(ab)".into()),
        (r"\(?:ab\)", "(?:ab)".into()),
        (r"a\|b", "a|b".into()),
        ("a*", "a*".into()),
        ("a+?", "a+?".into()),
        ("a**", "(?:a*)*".into()),
        ("*a", r"\*a".into()),
        ("^a", "(?m:^)a".into()),
        ("a^", r"a\^".into()),
        ("a$", "a(?m:$)".into()),
        ("$a", r"\$a".into()),
        (r"\(^a$\)", "((?m:^)a(?m:$))".into()),
        (r"a\{2\}", "a{2}".into()),
        (r"a\{2,\}", "a{2,}".into()),
        (r"a\{,3\}", "a{0,3}".into()),
        (r"a\{2,3\}", "a{2,3}".into()),
        (r"\{", r"\{".into()),
        (r"\{2\}", r"\{2\}".into()),
        (r"\`a\'", r"\Aa\z".into()),
        (r"\<a\>", r"\b{start}a\b{end}".into()),
        (r"\ba\B", r"\ba\B".into()),
        ("(a|b){c}", r"\(a\|b\)\{c\}".into()),
        ("[abc]", r"[\x{61}\x{62}\x{63}]".into()),
        ("[]a]", r"[\x{5D}\x{61}]".into()),
        ("[^a-z]", r"(?:[^\x{61}-\x{7A}]|(?-u:[\x80-\xFF]))".into()),
        ("[a-]", r"[\x{61}\x{2D}]".into()),
        (r"[\]", r"[\x{5C}]".into()),
        ("[[:digit:]x]", r"[0-9\x{78}]".into()),
        (r"\(?:a\)\{2\}*", "(?:(?:a){2})*".into()),
        (r"a\n", "an".into()),
        ("[z-a]", r"[^\x{0}-\x{10FFFF}]".into()),
    ];
    for (emacs, rust) in pairs {
        assert_eq!(
            emacs_regex_to_rust(emacs).as_deref(),
            Ok(rust.as_str()),
            "{emacs}"
        );
    }
}

#[test]
fn untranslatable_constructs_are_rejected_by_name() {
    for (emacs, construct, position) in [
        (r"\(a\)\1", r"\1", 5),
        (r"a\=", r"\=", 1),
        (r"\_<foo", r"\_<", 0),
        (r"\cg", r"\cg", 0),
        (r"\(?3:a\)", r"\(?3:", 0),
    ] {
        assert_eq!(
            emacs_regex_to_rust(emacs),
            Err(PatternError::Unsupported {
                construct: construct.to_string(),
                position
            }),
            "{emacs}"
        );
    }
}

#[test]
fn invalid_regexps_give_emacs_messages() {
    for (emacs, message, incomplete) in [
        (r"\(a", "Unmatched ( or \\(", true),
        (r"a\)", "Unmatched ) or \\)", true),
        ("[a", "Unmatched [ or [^", true),
        (r"a\", "Trailing backslash", false),
        (r"a\{2", "Unmatched \\{", true),
        (r"a\{3,2\}", "Invalid content of \\{\\}", false),
        ("[[:bogus:]]", "Invalid character class name", false),
    ] {
        let error = emacs_regex_to_rust(emacs).expect_err(emacs);
        assert_eq!(error.to_string(), message, "{emacs}");
        assert_eq!(error.is_incomplete(), incomplete, "{emacs}");
    }
}

#[test]
fn translations_match_like_emacs() {
    let content = text(b"foo_bar $x% a*b\nline (one)\n");
    // `_` is a symbol constituent and `$`, `%` are word constituents in
    // Emacs's standard syntax table.
    assert_eq!(forward(&content, &regex(r"\w+"), 0), Some((0, 3)));
    assert_eq!(forward(&content, &regex(r"\w+"), 3), Some((4, 7)));
    assert_eq!(forward(&content, &regex(r"\w+"), 7), Some((8, 11)));
    assert_eq!(forward(&content, &regex(r"\sw\s_"), 0), Some((2, 4)));
    assert_eq!(forward(&content, &regex(r"\s-+"), 11), Some((11, 12)));
    assert_eq!(forward(&content, &regex("a*b"), 12), Some((14, 15)));
    assert_eq!(forward(&content, &regex(r"a\*b"), 0), Some((12, 15)));
    assert_eq!(forward(&content, &regex("^line"), 0), Some((16, 20)));
    assert_eq!(forward(&content, &regex("b$"), 0), Some((14, 15)));
    assert_eq!(forward(&content, &regex("(one)"), 0), Some((21, 26)));
    assert_eq!(forward(&content, &regex(r"\'"), 0), Some((27, 27)));
    assert_eq!(forward(&content, &regex(r"[[:space:]]"), 0), Some((7, 8)));
    assert_eq!(forward(&content, &folded_regex("LINE"), 0), Some((16, 20)));
    assert_eq!(forward(&content, &regex("LINE"), 0), None);
}

#[test]
fn dot_matches_invalid_bytes_but_not_inside_characters() {
    let content = text(b"\xff\xc3\xa9\n");
    assert_eq!(forward(&content, &regex("."), 0), Some((0, 1)));
    assert_eq!(forward(&content, &regex("."), 1), Some((1, 3)));
    assert_eq!(forward(&content, &regex("[^a]"), 1), Some((1, 3)));
    assert_eq!(forward(&content, &regex("[^a]"), 0), Some((0, 1)));
}

#[test]
fn backward_search_emacs_semantics() {
    let content = text(b"aaa");
    assert_eq!(backward(&content, &literal("aa"), 3), Some((1, 3)));
    assert_eq!(backward(&content, &regex("aa"), 3), Some((1, 3)));
    assert_eq!(backward(&content, &regex("a+"), 3), Some((2, 3)));
    // The match must end at or before point, even when a longer one would
    // start closer.
    let content = text(b"abcabc");
    assert_eq!(backward(&content, &regex("abc"), 5), Some((0, 3)));
    assert_eq!(backward(&content, &regex("b.*"), 5), Some((4, 5)));
    assert_eq!(backward(&content, &literal("abc"), 2), None);
    // Assertions see the text after point.
    let content = text(b"ab\nab");
    assert_eq!(backward(&content, &regex("b$"), 5), Some((4, 5)));
    assert_eq!(backward(&content, &regex("a$"), 4), None);
    // Without a literal prefix every start is tried.
    let content = text(b"x1 22 333 y");
    assert_eq!(backward(&content, &regex("[0-9]+"), 11), Some((8, 9)));
    assert_eq!(backward(&content, &regex("[0-9]+"), 8), Some((7, 8)));
    assert_eq!(
        backward(&content, &regex(r"\(?:[0-9]\|x\)+"), 2),
        Some((1, 2))
    );
}

#[test]
fn backward_search_respects_its_bound() {
    let content = text(b"abab");
    let cancel = not_cancelled();
    let found = search_backward(&content, &regex("ab"), 4, Some(1), &cancel, CAP).expect("search");
    assert_eq!(found.map(|found| found.start), Some(2));
    let found = search_backward(&content, &regex("ab"), 3, Some(1), &cancel, CAP).expect("search");
    assert_eq!(found, None);
}

#[test]
fn forward_bound_limits_the_match_not_the_assertions() {
    let content = text(b"abcd");
    let cancel = not_cancelled();
    let found = search_forward(&content, &regex("b.*"), 0, Some(3), &cancel, CAP).expect("ok");
    assert_eq!(found.map(|found| found.range()), Some(1..3));
    let found = search_forward(&content, &regex("c$"), 0, Some(3), &cancel, CAP).expect("ok");
    assert_eq!(found, None, "$ sees the d after the bound");
}

#[test]
fn forward_match_across_chunk_boundary() {
    let boundary = usize::try_from(SEARCH_CHUNK).expect("chunk fits");
    let mut content = vec![b'x'; boundary - 3];
    content.extend_from_slice(b"needle");
    content.extend_from_slice(&[b'y'; 10]);
    let content = text(&content);
    let start = (boundary - 3) as u64;
    assert_eq!(
        forward(&content, &literal("needle"), 0),
        Some((start, start + 6))
    );
    assert_eq!(
        forward(&content, &regex("ne+dle"), 0),
        Some((start, start + 6))
    );
    assert_eq!(
        backward(&content, &literal("needle"), content.len()),
        Some((start, start + 6))
    );
    assert_eq!(
        backward(&content, &regex("ne+dle"), content.len()),
        Some((start, start + 6))
    );
}

#[test]
fn search_reads_a_file_through_the_block_cache() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("big.txt");
    let mut content = vec![b'.'; 3 * BLOCK_SIZE];
    let at = 2 * BLOCK_SIZE - 2;
    content[at..at + 5].copy_from_slice(b"HELLO");
    std::fs::write(&path, &content).expect("write fixture");
    let cache = BlockCache::open(&path, 2 * BLOCK_SIZE).expect("open cache");
    let index = LineIndex::new(cache.len());
    index_file(&cache, &index, &AtomicBool::new(false), &mut |_| {}).expect("index");
    let file_text = Text::from_original(Sources::file(cache, index));
    let at = at as u64;
    assert_eq!(
        forward(&file_text, &folded_regex("hello"), 0),
        Some((at, at + 5))
    );
    assert_eq!(
        backward(&file_text, &literal("HELLO"), file_text.len()),
        Some((at, at + 5))
    );

    // The file shrinks under the cache: the search fails with the read
    // error instead of panicking.
    std::fs::write(&path, b"short").expect("truncate");
    let shrunk = BlockCache::open(&path, 2 * BLOCK_SIZE).expect("reopen");
    drop(shrunk);
    let cache = match &file_text.sources().original {
        emx_text::Original::File(cache) => cache.clone(),
        emx_text::Original::Memory(_) => unreachable!("file original"),
    };
    cache.invalidate_from(0);
    let error = search_forward(
        &file_text,
        &literal("nowhere"),
        0,
        None,
        &not_cancelled(),
        CAP,
    )
    .expect_err("the file is gone");
    assert!(
        matches!(error, SearchError::Read(ReadError::Truncated { .. })),
        "{error:?}"
    );
}

#[test]
fn match_longer_than_cap_is_flagged() {
    let content = text(&[b'a'; 100]);
    let cancel = not_cancelled();
    let found = search_forward(&content, &regex("a+"), 0, None, &cancel, 16).expect("search");
    assert_eq!(
        found,
        Some(Match {
            start: 0,
            end: 16,
            capped: true
        })
    );
    let found = search_forward(&content, &regex("a+"), 0, None, &cancel, 100).expect("search");
    assert_eq!(
        found,
        Some(Match {
            start: 0,
            end: 100,
            capped: false
        })
    );
    // Across a chunk: the window ends at the cap past the chunk.
    let long = text(&vec![
        b'a';
        usize::try_from(SEARCH_CHUNK).expect("fits") + 100
    ]);
    let found = search_forward(&long, &regex("a+"), 5, None, &cancel, 64).expect("search");
    assert_eq!(
        found,
        Some(Match {
            start: 5,
            end: 69,
            capped: true
        })
    );
    let found = search_backward(&content, &regex("a+"), 100, None, &cancel, 16).expect("search");
    assert_eq!(found.map(|found| found.start), Some(99));
}

#[test]
fn cancel_stops_search() {
    let content = text(&vec![
        b'z';
        3 * usize::try_from(SEARCH_CHUNK).expect("fits")
    ]);
    let cancel = AtomicBool::new(true);
    for pattern in [literal("q"), regex("q+")] {
        assert!(matches!(
            search_forward(&content, &pattern, 0, None, &cancel, CAP),
            Err(SearchError::Cancelled)
        ));
        assert!(matches!(
            search_backward(&content, &pattern, content.len(), None, &cancel, CAP),
            Err(SearchError::Cancelled)
        ));
    }
}

#[test]
fn lazy_highlight_finds_every_match_in_a_range() {
    let content = text(b"foo bar foo baz foo");
    let found: Vec<(u64, u64)> = matches_in(&content, &literal("foo"), 2..19, CAP)
        .expect("search")
        .into_iter()
        .map(|found| (found.start, found.end))
        .collect();
    assert_eq!(found, vec![(8, 11), (16, 19)]);
    let empty: Vec<u64> = matches_in(&content, &regex("^"), 0..19, CAP)
        .expect("search")
        .into_iter()
        .map(|found| found.start)
        .collect();
    assert_eq!(empty, vec![0]);
}

#[test]
fn groups_and_anchored_matches() {
    let content = text(b"key=value");
    let pattern = regex(r"\(\w+\)=\(\w+\)\(x\)?");
    let found = forward(&content, &pattern, 0).expect("match");
    let found = Match {
        start: found.0,
        end: found.1,
        capped: false,
    };
    assert_eq!(pattern.group_count(), 4);
    assert_eq!(
        captures(&content, &pattern, &found).expect("groups"),
        vec![Some(0..9), Some(0..3), Some(4..9), None]
    );
    let at = match_at(&content, &regex("=v"), 3, None, CAP).expect("match");
    assert_eq!(at.map(|found| found.range()), Some(3..5));
    assert_eq!(
        match_at(&content, &regex("v"), 3, None, CAP).expect("ok"),
        None
    );
    assert_eq!(regexp_quote("a.b*[c]^$\\"), r"a\.b\*\[c]\^\$\\");
}
