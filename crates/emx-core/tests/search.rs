//! Incremental search, query-replace and occur (DESIGN sections 4 and 5).
//! Messages and positions follow Emacs 29.3's isearch.el and replace.el;
//! the differential cases in `difftest/cases/m3-search.jsonl` check the
//! same behaviour against Emacs itself.

use std::sync::atomic::AtomicBool;

use emx_core::testing::Harness;
use emx_core::{Config, Session};
use emx_proto::ClientKind;
use emx_text::{BLOCK_SIZE, BlockCache, LineIndex, Sources, Text, index_file};
use emx_types::Face;

/// A config whose `in_memory_max` makes every test text "large", so its
/// searches run on a worker.
fn small_in_memory_max() -> Config {
    Config {
        in_memory_max: 8,
        ..Config::default()
    }
}

fn faces_of_row(harness: &Harness, row: u16) -> Vec<Face> {
    harness
        .grid()
        .row(row)
        .iter()
        .map(|cell| cell.face)
        .collect()
}

#[test]
fn isearch_finds_and_exits_with_mark_saved() {
    let mut harness = Harness::with_text("|one foo two foo");
    harness.keys("C-s");
    assert_eq!(harness.echo(), "I-search: ");
    harness.keys("f o");
    assert_eq!(harness.echo(), "I-search: fo");
    assert_eq!(harness.point(), 6);
    harness.keys("o RET");
    assert_eq!(harness.point(), 7);
    assert_eq!(harness.mark(), Some(0));
    assert!(!harness.mark_active());
    assert_eq!(harness.echo(), "Mark saved where search started");
    // The search ring remembers the string for `C-s C-s`.
    harness.keys("C-s C-s");
    assert_eq!(harness.echo(), "I-search: foo");
    assert_eq!(harness.point(), 15);
}

#[test]
fn isearch_prompts_follow_emacs() {
    let mut harness = Harness::with_text("abc x|y abc");
    harness.keys("C-s a b c");
    assert_eq!(harness.echo(), "I-search: abc");
    assert_eq!(harness.point(), 10);
    harness.keys("C-s");
    assert_eq!(harness.echo(), "Failing I-search: abc");
    assert_eq!(
        harness.point(),
        10,
        "a failing search stays at the last match"
    );
    harness.keys("C-s");
    assert_eq!(harness.echo(), "Wrapped I-search: abc");
    assert_eq!(harness.point(), 3);
    harness.keys("C-s");
    // Past where it started: overwrapped.
    assert_eq!(harness.echo(), "Overwrapped I-search: abc");
    assert_eq!(harness.point(), 10);
    harness.keys("C-r");
    assert_eq!(harness.echo(), "Wrapped I-search backward: abc");
    assert_eq!(harness.point(), 7);
    harness.keys("RET");

    let mut harness = Harness::with_text("|xay xby");
    harness.keys("C-M-s x . y");
    assert_eq!(harness.echo(), "Regexp I-search: x.y");
    assert_eq!(harness.point(), 3);
    harness.keys("\\");
    assert_eq!(
        harness.echo(),
        "Regexp I-search: x.y\\ [Trailing backslash]"
    );
    harness.keys("DEL \\ (");
    assert_eq!(harness.echo(), "Regexp I-search: x.y\\( [incomplete input]");
    harness.keys("C-g");
    assert_eq!(harness.echo(), "Regexp I-search: x.y");

    let mut harness = Harness::with_text("|abc ABC");
    harness.keys("C-s A B D");
    assert_eq!(harness.echo(), "Failing case-sensitive I-search: ABD");
}

#[test]
fn isearch_case_fold_follows_search_upper_case() {
    let mut harness = Harness::with_text("|Foo foo");
    harness.keys("C-s f o o");
    assert_eq!(harness.point(), 3, "lower case folds");
    harness.keys("RET");
    let mut harness = Harness::with_text("|foo Foo");
    harness.keys("C-s F o o");
    assert_eq!(harness.point(), 7, "an upper-case letter is case-sensitive");
    harness.keys("M-c");
    assert_eq!(harness.echo(), "I-search: Foo [case insensitive]");
    harness.keys("C-r C-r");
    assert_eq!(harness.point(), 0);
}

#[test]
fn isearch_del_undoes_the_last_step() {
    let mut harness = Harness::with_text("|fa fb fa");
    harness.keys("C-s f a");
    assert_eq!(harness.point(), 2);
    harness.keys("C-s");
    assert_eq!(harness.point(), 8);
    harness.keys("DEL");
    assert_eq!(harness.point(), 2);
    harness.keys("DEL");
    assert_eq!(harness.point(), 1);
    assert_eq!(harness.echo(), "I-search: f");
    harness.keys("DEL DEL");
    assert_eq!(harness.point(), 0);
    assert_eq!(harness.echo(), "I-search: ");
}

#[test]
fn isearch_c_w_yanks_the_next_word() {
    let mut harness = Harness::with_text("|hello world hello world");
    harness.keys("C-s h e");
    harness.keys("C-w");
    assert_eq!(harness.echo(), "I-search: hello");
    assert_eq!(harness.point(), 5);
    harness.keys("C-w");
    assert_eq!(harness.echo(), "I-search: hello world");
    harness.keys("C-s");
    assert_eq!(harness.point(), 23);
}

#[test]
fn isearch_c_g_removes_the_failing_part_then_aborts() {
    let mut harness = Harness::with_text("|ab ac");
    harness.keys("C-s a c x y");
    assert_eq!(harness.echo(), "Failing I-search: acxy");
    assert_eq!(harness.point(), 5);
    harness.keys("C-g");
    assert_eq!(harness.echo(), "I-search: ac");
    assert!(!harness.signaled());
    assert_eq!(harness.point(), 5);
    harness.keys("C-g");
    assert_eq!(harness.echo(), "Quit");
    assert!(harness.signaled());
    assert_eq!(harness.point(), 0, "back where the search started");
    assert_eq!(harness.mark(), None, "no mark is pushed");
    harness.keys("a");
    assert_eq!(harness.text(), "aab ac", "isearch ended");
}

#[test]
fn other_commands_exit_isearch_and_run() {
    let mut harness = Harness::with_text("|foo bar");
    harness.keys("C-s f o o C-f");
    assert_eq!(harness.point(), 4);
    assert_eq!(harness.mark(), Some(0));
    harness.keys("x");
    assert_eq!(harness.text(), "foo xbar");

    // A prefix argument keeps the search going.
    let mut harness = Harness::with_text("|aaaa b");
    harness.keys("C-s C-u 3 a");
    assert_eq!(harness.echo(), "I-search: aaa");
    assert_eq!(harness.point(), 3);
    harness.keys("M-x");
    assert!(
        harness.echo_line().starts_with("M-x "),
        "{}",
        harness.echo_line()
    );
}

#[test]
fn isearch_backward_finds_the_closest_start() {
    let mut harness = Harness::with_text("aaaa|");
    harness.keys("C-r a a");
    assert_eq!(harness.point(), 2);
    assert_eq!(harness.echo(), "I-search backward: aa");
    harness.keys("C-r");
    assert_eq!(harness.point(), 0);
    harness.keys("RET");
    assert_eq!(harness.mark(), Some(4));
}

#[test]
fn lazy_highlight_marks_visible_matches() {
    let mut harness = Harness::with_text("|ab ab ab");
    harness.keys("C-s a b");
    let faces = faces_of_row(&harness, 0);
    assert_eq!(&faces[0..2], &[Face::IsearchMatch, Face::IsearchMatch]);
    assert_eq!(faces[2], Face::Default);
    assert_eq!(&faces[3..5], &[Face::LazyHighlight, Face::LazyHighlight]);
    assert_eq!(&faces[6..8], &[Face::LazyHighlight, Face::LazyHighlight]);
    // Every window of the client showing the buffer highlights.
    harness.keys("RET C-x 2 C-s a b");
    let lower = faces_of_row(&harness, 12);
    assert_eq!(&lower[0..2], &[Face::LazyHighlight, Face::LazyHighlight]);
    harness.keys("RET");
    assert_eq!(
        faces_of_row(&harness, 0)[0],
        Face::Default,
        "gone after the search"
    );
}

#[test]
fn large_buffers_search_on_a_worker() {
    let mut harness = Harness::with_config(small_in_memory_max());
    harness.set_text("|one two three two");
    harness.hold_searches();
    harness.keys("C-s t w");
    assert_eq!(harness.held_searches(), vec![false]);
    assert_eq!(harness.echo(), "I-search: t [searching...]");
    assert_eq!(harness.point(), 0);
    // Keys typed meanwhile wait for the search.
    harness.keys("o RET");
    harness.release_searches();
    assert_eq!(harness.point(), 7);
    assert_eq!(harness.mark(), Some(0), "the queued RET ended the search");

    // `C-g` cancels the search: it fails, and a second `C-g` quits.
    let mut harness = Harness::with_config(small_in_memory_max());
    harness.set_text("|one two three two");
    harness.hold_searches();
    harness.keys("C-s z");
    harness.keys("C-g");
    assert_eq!(harness.held_searches(), vec![true]);
    assert_eq!(harness.echo(), "I-search: ");
    harness.keys("C-g");
    assert!(harness.signaled());
    harness.release_searches();
    assert_eq!(harness.point(), 0);
}

#[test]
fn match_reaching_the_cap_is_reported() {
    let config = Config {
        search_match_cap: 4,
        ..Config::default()
    };
    let mut harness = Harness::with_config(config);
    harness.set_text("|aaaaaaaaaa");
    harness.keys("C-M-s a +");
    assert_eq!(harness.point(), 4);
    assert_eq!(
        harness.echo(),
        "Regexp I-search: a+ [Match truncated at search_match_cap]"
    );
}

#[test]
fn search_read_errors_are_shown_not_fatal() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("big.txt");
    std::fs::write(&path, vec![b'x'; 3 * BLOCK_SIZE]).expect("write fixture");
    let cache = BlockCache::open(&path, 4 * BLOCK_SIZE).expect("open cache");
    let index = LineIndex::new(cache.len());
    index_file(&cache, &index, &AtomicBool::new(false), &mut |_| {}).expect("index");
    let text = Text::from_original(Sources::file(cache.clone(), index));
    let mut harness = Harness::new();
    harness.buffer_mut().text = text;
    // Another process truncates the file; the cache must read it again.
    std::fs::write(&path, b"x").expect("truncate");
    cache.invalidate_from(0);
    harness.keys("C-s y");
    assert!(
        harness
            .echo()
            .starts_with("Failing I-search: y [file truncated"),
        "{}",
        harness.echo()
    );
    harness.keys("C-g C-g");
    assert_eq!(harness.point(), 0);
}

#[test]
fn another_clients_edits_during_isearch_are_safe() {
    let mut harness = Harness::with_session(Config::default(), Session::Server);
    let first = harness.client();
    let second = harness.attach(ClientKind::Tui);
    harness.keys_as(first, "a b c SPC a b c");
    harness.keys_as(first, "M-< C-s a b c C-s");
    assert_eq!(harness.point_of(first), 7);
    // The other client deletes everything the search knows about.
    harness.keys_as(second, "M-< C-k");
    assert_eq!(harness.point_of(first), 0);
    harness.keys_as(first, "DEL DEL");
    harness.keys_as(second, "x y z");
    harness.keys_as(first, "C-g C-g");
    assert_eq!(harness.point_of(first), 0);
    assert!(harness.echo_of(second).is_empty());
}

#[test]
fn isearch_occur_lists_the_search_string_and_keeps_searching() {
    // Covers what the removed `isearch-occur` difftest case checked: batch
    // Emacs in slow-terminal isearch mode restores the window configuration
    // after `M-s o`, which interactive Emacs does not, so the behaviour is
    // proven here instead of against a batch Emacs.
    let mut harness = Harness::with_text("|foo\nbar foo\n");
    harness.keys("C-s f o o");
    assert_eq!(harness.echo(), "I-search: foo");
    assert_eq!(harness.point(), 3);
    harness.keys("M-s o");
    assert_eq!(
        harness.buffer_text("*Occur*").as_deref(),
        Some("2 matches for \"foo\" in buffer: *scratch*\n      1:foo\n      2:bar foo\n")
    );
    assert_eq!(
        harness.buffer().name,
        "*scratch*",
        "occur does not leave isearch"
    );
    // The search is still live: repeating it moves to the next match.
    harness.keys("C-s");
    assert_eq!(harness.echo(), "I-search: foo");
    assert_eq!(harness.point(), 11);
    harness.keys("RET C-x o");
    assert_eq!(harness.buffer().name, "*Occur*");
}

#[test]
fn query_replace_bang_replaces_rest() {
    let mut harness = Harness::with_text("|a b a b a");
    harness.keys("M-%");
    assert_eq!(harness.echo_line(), "Query replace: ");
    harness.keys("a RET");
    assert_eq!(harness.echo_line(), "Query replace a with: ");
    harness.keys("x RET");
    assert_eq!(harness.echo(), "Query replacing a with x: (? for help) ");
    assert_eq!(harness.point(), 1);
    harness.keys("y");
    assert_eq!(harness.text(), "x b a b a");
    harness.keys("n");
    assert_eq!(harness.point(), 9);
    harness.keys("!");
    assert_eq!(harness.text(), "x b a b x");
    assert_eq!(harness.echo(), "Replaced 2 occurrences");
    assert_eq!(harness.mark(), Some(0));
    // The next query-replace offers the last pair as the default.
    harness.keys("M-<");
    harness.keys("M-%");
    assert_eq!(
        harness.echo_line(),
        "Query replace (default a \u{2192} x): "
    );
    harness.keys("RET");
    assert_eq!(harness.point(), 5);
    harness.keys("q");
    assert_eq!(harness.echo(), "Replaced 0 occurrences");
}

#[test]
fn query_replace_each_answer_is_an_undo_group() {
    let mut harness = Harness::with_text("|a a a");
    harness.keys("M-% a RET b RET y y q");
    assert_eq!(harness.text(), "b b a");
    harness.keys("C-/");
    assert_eq!(harness.text(), "b a a");
    let mut harness = Harness::with_text("|a a a");
    harness.keys("M-% a RET b RET y ^");
    assert_eq!(harness.point(), 1);
    harness.keys("n .");
    assert_eq!(harness.text(), "b b a");
    assert_eq!(harness.echo(), "Replaced 2 occurrences");
}

#[test]
fn query_replace_regexp_groups_and_case() {
    let mut harness = Harness::with_text("|key=value Name=x");
    harness.keys("C-M-% \\ ( \\ w + \\ ) = \\ ( \\ w + \\ ) RET \\ 2 = \\ 1 SPC \\ & RET !");
    assert_eq!(harness.text(), "value=key key=value x=Name Name=x");
    assert_eq!(harness.echo(), "Replaced 2 occurrences");

    let mut harness = Harness::with_text("|foo Foo FOO");
    harness.keys("M-% f o o RET b a r RET !");
    assert_eq!(harness.text(), "bar Bar BAR");
    let mut harness = Harness::with_text("|foo Foo FOO");
    harness.keys("M-% F o o RET b a r RET !");
    assert_eq!(harness.text(), "foo bar FOO", "upper case in FROM is exact");
}

#[test]
fn query_replace_other_keys_exit_and_run() {
    let mut harness = Harness::with_text("|a a");
    harness.keys("M-% a RET b RET C-f");
    assert_eq!(harness.point(), 2);
    assert_eq!(harness.text(), "a a");
    harness.keys("C-g");
    let mut harness = Harness::with_text("|a a");
    harness.keys("M-% a RET b RET C-g");
    assert!(harness.signaled());
    assert_eq!(harness.echo(), "Quit");
    harness.keys("z");
    assert_eq!(
        harness.text(),
        "az a",
        "point stays after the match asked about"
    );
}

#[test]
fn query_replace_in_the_active_region() {
    let mut harness = Harness::with_text("a a| a a");
    harness.keys("C-SPC C-e M-%");
    assert_eq!(harness.echo_line(), "Query replace in region: ");
    harness.keys("a RET b RET !");
    assert_eq!(harness.text(), "a a b b");
    assert!(!harness.mark_active());
}

#[test]
fn occur_lists_matches() {
    let mut harness = Harness::with_text("|foo bar\nbaz\nfoo foo\nzap foo\n");
    harness.keys("M-s o");
    assert_eq!(harness.echo_line(), "List lines matching regexp: ");
    harness.keys("f o o RET");
    assert_eq!(harness.echo(), "Searched 1 buffer; 4 matches for \"foo\"");
    assert_eq!(
        harness.buffer_text("*Occur*").as_deref(),
        Some(
            "4 matches in 3 lines for \"foo\" in buffer: *scratch*\n      1:foo bar\n      3:foo foo\n      4:zap foo\n"
        )
    );
    assert_eq!(
        harness.buffer().name,
        "*scratch*",
        "the list is not selected"
    );
    harness.keys("C-x o");
    assert_eq!(harness.buffer().name, "*Occur*");
    // Like Emacs, `M-n` stops at the end of each match.
    harness.keys("M-n M-n");
    assert_eq!(harness.point(), 79);
    harness.keys("RET");
    assert_eq!(harness.buffer().name, "*scratch*");
    assert_eq!(harness.point(), 12);
    // The default is the last regexp.
    harness.keys("M-s o");
    assert_eq!(
        harness.echo_line(),
        "List lines matching regexp (default foo): "
    );
    harness.keys("C-g");
}

#[test]
fn occur_context_lines_and_no_matches() {
    let mut harness = Harness::with_text("|a1\nfoo\nb2\nc3\nd4\nfoo x\ne5\nf6\ng7\nh8\nfoo\n");
    harness.keys("M-1 M-s o f o o RET");
    assert_eq!(
        harness.buffer_text("*Occur*").as_deref(),
        Some(
            "3 matches for \"foo\" in buffer: *scratch*\n       :a1\n      2:foo\n       :b2\n-------\n       :d4\n      6:foo x\n       :e5\n-------\n       :h8\n     11:foo\n"
        )
    );
    harness.keys("M-s o z z z RET");
    assert_eq!(harness.echo(), "Searched 1 buffer; no matches for \"zzz\"");
    assert_eq!(harness.buffer_text("*Occur*"), None, "the list is killed");
}

#[test]
fn occur_on_a_large_buffer_runs_on_a_worker() {
    let mut harness = Harness::with_config(small_in_memory_max());
    harness.set_text("|xx\nyy\nxx\n");
    harness.hold_searches();
    harness.keys("M-s o x RET");
    assert_eq!(harness.echo(), "Occur [searching...]");
    // The editor keeps working meanwhile.
    harness.keys("C-e");
    assert_eq!(harness.point(), 2);
    harness.release_searches();
    assert_eq!(
        harness.buffer_text("*Occur*").as_deref(),
        Some("4 matches in 2 lines for \"x\" in buffer: *scratch*\n      1:xx\n      3:xx\n")
    );
}
