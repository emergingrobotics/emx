# emx First Release (M1-M4) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the `emx` first release (DESIGN.md milestones M1-M4): an Emacs-keybinding terminal editor with a session server and edit journal, agent-aware live reload, Emacs tier-1 command parity verified against real Emacs, and multi-GB file support.

**Architecture:** A Cargo workspace. `emx-text` is the byte-oriented text engine (persistent piece tree over an in-memory or `pread` block-cache original, line index, display measurement, streaming search). `emx-core` is a deterministic editor state machine: it consumes `Event`s, mutates state, emits `Effect`s (I/O requests, journal records, client messages), and renders per-client `Grid`s; it performs no thread or socket work itself, so it is fully testable headless. The `emx` binary runs the threads (editor loop, client sockets, I/O pool, watcher, indexer, search, journal writer) and the CLI. `emx-term` is the client-side terminal layer (Kitty keyboard decoding, screen diffing). `emx-proto` is the versioned JSON-lines protocol. `emx-types` holds types shared by all of them (keys, grid).

**Tech Stack:** Rust 1.95.0 (edition 2024), `rustix`, `notify`, `memchr`, `regex`, `regex-automata`, `regex-syntax`, `aho-corasick`, `unicode-segmentation`, `unicode-width`, `serde`/`serde_json`/`toml`, `thiserror`, `anyhow` (binary only), `clap`, `crossterm` (raw mode and size only), `similar`, `xxhash-rust`, `xattr`, `parking_lot`, `crossbeam-channel`, `slotmap`, `proptest`, `portable-pty`, `vt100`, `tempfile`, `mimalloc`. GNU Emacs 29.3 for differential tests.

**Spec:** `DESIGN.md` (technical design) and `VISION.md` (goals, ranked; decisions). Read both before any task. Where this plan and the spec disagree, the spec wins; report the conflict.

## Global Constraints

- Toolchain pinned in `rust-toolchain.toml`: `channel = "1.95.0"`, components `rustfmt`, `clippy`. Edition 2024 in every crate.
- Every build and test runs through the Makefile (`make build`, `make test`, `make run-tests`, `make check`). Never run `cargo` directly in a terminal; the Makefile is the interface. A single test is run with `make test TEST_ARGS="-p <crate> <filter>"`.
- Linux only (Ubuntu/Debian first). macOS is not targeted by this release; do not add macOS code paths.
- `#![forbid(unsafe_code)]` in every crate. System calls go through `rustix` (statfs, fadvise, flock, geteuid, SO_PEERCRED, fsync).
- Libraries use `thiserror` error enums; only the `emx` binary uses `anyhow`.
- No `unwrap()`/`expect()` on anything derived from external input (files, sockets, terminal, config, CLI). `expect("<invariant>")` is allowed only for internal invariants. Do not write forgiving code: validate inputs at boundaries and return errors; never silently fall back.
- Config is strict: `#[serde(deny_unknown_fields)]`; an unknown key or invalid value is a startup error naming the key.
- No magic numbers: every default below is a named `pub const` in `emx-core/src/config.rs` (or `emx-text` where noted) and is overridable in TOML.
- Comments explain why, never what. No commented-out code. No emoji anywhere.
- `make check` = `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` + all tests. It must pass at every commit.
- Dual license: `LICENSE-MIT`, `LICENSE-APACHE`; `license = "MIT OR Apache-2.0"` in every `Cargo.toml`.
- Never skip, ignore (`#[ignore]`), or stub a test. A test that cannot pass is a defect to fix or to report.

**Defaults (exact values; spec-derived or decided in DESIGN.md):**

| Constant | Value | Source |
|---|---|---|
| `IN_MEMORY_MAX` | 16 MiB | DESIGN 5 |
| `BLOCK_SIZE` (emx-text) | 64 KiB | plan |
| `BLOCK_CACHE_BYTES` | 64 MiB | plan |
| `SEARCH_MATCH_CAP` | 64 KiB | DESIGN 5 |
| `APPEND_TAIL_HASH_BYTES` | 64 KiB | DESIGN 3 |
| `SETTLE_WINDOW_MS` | 200 | plan (DESIGN 1 "fraction of a second") |
| `POLL_INTERVAL_MS` | 2000 | DESIGN 3 |
| `JOURNAL_FSYNC_MS` | 200 | DESIGN 2 |
| `JOURNAL_SNAPSHOT_MAX` | 64 MiB | DESIGN 2 |
| `LARGE_SAVE_WARN` | 512 MiB | plan |
| `DAEMON_GRACE_MS` | 3000 | plan (DESIGN 2 "short grace period") |
| `UNDO_LIMIT` / `UNDO_STRONG_LIMIT` / `UNDO_OUTER_LIMIT` | 160000 / 240000 / 24000000 | Emacs 29 defaults |
| `KILL_RING_MAX` | 120 | Emacs |
| `AUTO_SAVE_INTERVAL_KEYS` / `AUTO_SAVE_TIMEOUT_S` | 300 / 30 | Emacs |
| `TAB_WIDTH` | 8 | Emacs |
| `NEXT_SCREEN_CONTEXT_LINES` | 2 | Emacs |
| `AMALGAMATION_LIMIT` | 20 | Emacs `amalgamating-undo-limit` |
| `COLUMN_CHECKPOINT_INTERVAL` | 4096 bytes | plan |
| `LONG_LINE_THRESHOLD` | 4096 bytes | plan |
| `MAX_WIRE_MESSAGE` | 16 MiB | plan |
| `FRAME_QUEUE_DEPTH` | 8 | plan |

## Review Focus

1. **Save fails part-way (ENOSPC, EACCES, read-only directory, file deleted under us).** Expect: the original file is unchanged (rename path) or the error is reported with the buffer still modified (in-place path); never a silent half-write reported as success. Test owned by Task 10: `save_enospc_leaves_original_intact` (an injected writer that fails with `ENOSPC` after N bytes) and `save_permission_denied_keeps_buffer_modified`.
2. **Non-UTF-8 file names and paths with spaces/newlines.** Expect: open, save, list-buffers, and events round-trip the exact bytes. Test owned by Task 3: `wire_path_roundtrips_non_utf8` and Task 13: `cli_open_path_with_newline_and_invalid_utf8`.
3. **Tiny and degenerate terminal sizes (1x1, 2 rows, 0 columns reported, very wide).** Expect: no panic, renders what fits, splits refused with Emacs's "Window ... too small for splitting". Tests owned by Task 9 (`render_1x1_does_not_panic`, `render_zero_cols_is_error_free`) and Task 16 (`split_too_small_errors`).
4. **Two clients editing the same buffer at once.** Expect: each client's point and mark move correctly with the other's edits (markers), killing a buffer shown in another client's window switches that window to another buffer. Test owned by Task 13: `two_clients_same_buffer_points_shift`, `kill_buffer_shown_in_other_client`.
5. **Empty files, files without a trailing newline, files that are only newlines, CRLF files.** Expect: editing at end of buffer behaves like Emacs; every byte round-trips on save unchanged (CRLF shown as `^M` before end of line, as raw Emacs does with `no-conversion`). Tests owned by Task 4 (`roundtrip_crlf_and_no_trailing_newline` property case) and Task 8 differential cases `eob-*`.

---

## File Structure

```
Makefile                         build/test/check/fuzz/bench/difftest targets
Cargo.toml                       workspace
rust-toolchain.toml
LICENSE-MIT, LICENSE-APACHE, .gitignore, README.md
crates/emx-types/src/
  lib.rs
  key.rs                         Key, KeyCode, Mods; Emacs kbd parse/format
  grid.rs                        Grid, Cell, Face
crates/emx-text/src/
  lib.rs
  identity.rs                    FileIdentity
  source.rs                      AddStore, Original, Sources
  block_cache.rs                 BlockCache (pread, LRU, truncation)
  line_index.rs                  LineIndex, NthNewline, indexer
  piece_tree.rs                  persistent treap of pieces with summaries
  text.rs                        Text: high-level persistent text API
  display.rs                     DisplayUnit iteration, widths
  checkpoints.rs                 column checkpoints (M4)
  search/mod.rs                  Pattern, CompiledPattern, search_forward/backward
  search/emacs_regex.rs          Emacs regexp -> regex syntax translator
crates/emx-text/fuzz/            cargo-fuzz targets (nightly)
crates/emx-core/src/
  lib.rs
  config.rs                      Config + all default constants
  event.rs                       Event, Effect
  editor.rs                      Editor: buffers, clients, dispatch loop
  buffer.rs                      Buffer, markers, modified state
  marker.rs
  undo.rs                        Emacs-model undo list
  killring.rs
  keymap.rs                      Keymap, prefix maps, TOML overrides
  command/mod.rs                 command registry, CommandContext, prefix arg
  command/move.rs, edit.rs, kill.rs, mark.rs, files.rs, window.rs,
          search.rs, replace.rs, occur.rs, macro.rs, register.rs,
          rect.rs, dabbrev.rs, misc.rs
  minibuffer.rs                  minibuffer, prompts, history
  completion.rs                  vertical incremental completion
  isearch.rs
  window.rs                      window tree per client, layout
  render.rs                      Grid rendering, modeline, echo area
  io.rs                          open/save implementations (called by workers)
  inplace.rs                     in-place save planner (disk memmove)
  autosave.rs
  journal.rs                     journal format, writer, replay
  reload.rs                      external-change state machine (M2)
  diff.rs                        diff-external
  testing.rs                     Harness (headless, synchronous effects)
crates/emx-proto/src/lib.rs      Envelope, ClientMsg, ServerMsg, WirePath, framing
crates/emx-term/src/
  lib.rs
  kitty.rs                       KittyDecoder, InputEvent
  terminal.rs                    raw mode, setup/restore, kitty probe
  screen.rs                      Grid -> escape-sequence diff
src/                             the `emx` binary: the existing root package (crates.io name, publish metadata)
  main.rs                        clap CLI dispatch
  paths.rs                       socket dir, state dir, config path; ownership checks
  server/mod.rs                  accept loop, editor thread, lifetime
  server/client_conn.rs          per-client reader/writer threads
  server/workers.rs              I/O pool, search, indexer threads
  server/watcher.rs              notify + statfs + polling
  server/journal_writer.rs       batched fsync thread
  client.rs                      TUI client (socket or in-process)
  cli.rs                         open/list-buffers/reload/goto/save-all/events/diff-external
  noserver.rs                    in-process runner
tests/                           PTY and server integration tests (root package)
crates/emx-bench/src/main.rs     benchmark harness (M4)
difftest/emx-difftest.el         Emacs batch driver
difftest/cases/*.jsonl           differential cases
crates/emx-core/tests/difftest.rs  differential runner
```

---

## Milestone M1: Core and Server

### Task 1: Workspace scaffolding, Makefile, git

**Existing state:** the repo already has the root package `emx` (version 0.0.1, crates.io metadata: description, license, repository, readme, keywords, categories), `src/main.rs` (hello world), `LICENSE-MIT`, `LICENSE-APACHE`, a Makefile with `help build test run-tests check clean publish-dry-run publish`, and `.gitignore`. Keep all of that metadata and the publish targets.

**Files:**
- Modify: `Cargo.toml` (add `[workspace]` with `members = ["crates/*"]`, `resolver = "3"`, `[workspace.package]` and `[workspace.dependencies]`; the root package stays the `emx` binary), `Makefile`, `src/main.rs`
- Create: `rust-toolchain.toml`, `README.md`, `crates/{emx-types,emx-text,emx-core,emx-proto,emx-term,emx-bench}/Cargo.toml` and `src/lib.rs` (`emx-bench`: `src/main.rs`)

**Interfaces:**
- Produces: workspace members `emx-types`, `emx-text`, `emx-core`, `emx-proto`, `emx-term`, `emx-bench` (binary); the root package `emx` (binary named `emx`).

- [ ] **Step 1:** `.gitignore` is already correct; leave it.
- [ ] **Step 2:** Makefile (keep `help` as the default goal and keep `publish-dry-run`/`publish`). Default target prints the target list. Targets: `build` (release build of `emx`, binary copied to `bin/emx`), `test` (`cargo test --workspace $(TEST_ARGS)`), `run-tests` (alias of `test` plus `difftest`), `difftest` (`cargo test -p emx-core --test difftest`), `check` (fmt check, clippy `-D warnings`, `run-tests`), `fmt`, `fuzz` (each fuzz target for `FUZZ_SECONDS ?= 60` on nightly), `bench` (10 MB and 1 GB corpora), `bench-full` (adds 10 GB), `static` (x86_64 and aarch64 musl via `cargo zigbuild`), `clean`. Every recipe echoes its command.
- [ ] **Step 3:** Each crate compiles with `#![forbid(unsafe_code)]`; `emx --version` prints `emx <CARGO_PKG_VERSION>`.
- [ ] **Step 4:** Run `make check`. Expected: passes with zero tests.
- [ ] **Step 5:** Commit `chore: scaffold emx workspace`.

### Task 2: Keys and grid types (`emx-types`)

**Files:** Create `crates/emx-types/src/key.rs`, `grid.rs`; tests inline.

**Interfaces:**
- Produces:
  - `pub struct Mods { pub ctrl: bool, pub meta: bool, pub shift: bool, pub super_: bool }`
  - `pub enum KeyCode { Char(char), Enter, Tab, Backspace, Escape, Delete, Insert, Up, Down, Left, Right, Home, End, PageUp, PageDown, F(u8) }`
  - `pub struct Key { pub code: KeyCode, pub mods: Mods }` (Eq, Hash, Serialize as its kbd string)
  - `pub fn parse_kbd(s: &str) -> Result<Vec<Key>, KbdError>` and `pub fn format_key(k: &Key) -> String` using Emacs `kbd` notation: `C-x`, `M-%`, `C-M-s`, `RET`, `SPC`, `TAB`, `DEL`, `ESC`, `<f8>`, `<up>`, `C-SPC`, `C-/`, `C-_`, `S-<left>`.
  - Normalization rule: `Char` with `shift` and an uppercase-able letter is stored as the uppercase char without `shift` (`S-a` == `A`); `C-` with an uppercase letter keeps `shift` (Emacs distinguishes `C-S-a`).
  - `pub enum Face { Default, ModeLine, ModeLineInactive, Region, IsearchMatch, LazyHighlight, MinibufferPrompt, EscapeGlyph, Error, Warning, ChangedMarker }`
  - `pub struct Cell { pub text: String, pub width: u8, pub face: Face }` (width 0 marks the trailing half of a wide char)
  - `pub struct Grid { pub cols: u16, pub rows: u16, pub cells: Vec<Cell>, pub cursor: Option<(u16, u16)> }` with `Grid::new(cols, rows)`, `set(col,row,cell)`, `row(r) -> &[Cell]`.

- [ ] **Step 1: Failing tests**

```rust
#[test] fn kbd_roundtrip() {
    for s in ["C-x C-f", "M-%", "C-M-s", "RET", "SPC", "TAB", "DEL", "<f8>", "C-SPC", "C-/", "C-_", "M-<", "C-x r t", "<up>"] {
        let keys = parse_kbd(s).unwrap();
        assert_eq!(keys.iter().map(format_key).collect::<Vec<_>>().join(" "), s);
    }
}
#[test] fn shift_letter_normalizes() { assert_eq!(parse_kbd("S-a").unwrap(), parse_kbd("A").unwrap()); }
#[test] fn bad_kbd_errors() { assert!(parse_kbd("C-").is_err()); assert!(parse_kbd("<nosuchkey>").is_err()); }
```
- [ ] **Step 2:** `make test TEST_ARGS="-p emx-types"` fails (not defined).
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Passes.
- [ ] **Step 5:** Commit `feat(types): keys and grid`.

### Task 3: Protocol (`emx-proto`)

**Files:** Create `crates/emx-proto/src/lib.rs`.

**Interfaces:**
- Produces:
  - `pub const PROTOCOL_VERSION: u32 = 1;` Every message on the wire is `{"v":1, "type": ..., ...}` (one JSON object per line, max `MAX_WIRE_MESSAGE` bytes; longer lines are a protocol error that closes the connection).
  - `pub enum WirePath` — serialized as a JSON string when the path is valid UTF-8, else as `{"bytes_b64": "..."}`; `From<&Path>`, `into_path_buf()`.
  - `pub enum ClientKind { Tui, Cli }`
  - `pub enum ClientMsg { Hello { kind: ClientKind, cwd: WirePath, cols: u16, rows: u16, wait_for: Option<Vec<WirePath>> }, Key { key: Key }, Paste { data_b64: String }, Resize { cols: u16, rows: u16 }, Request { id: u64, req: Request } }`
  - `pub enum Request { Open { path: WirePath, line: Option<u64>, col: Option<u64>, wait: bool }, ListBuffers, Reload { path: Option<WirePath> }, Goto { path: Option<WirePath>, line: u64, col: Option<u64> }, SaveAll, SubscribeEvents, DiffExternal { path: WirePath } }`
  - `pub enum ServerMsg { Welcome { server_pid: u32 }, Frame { full: bool, cols: u16, rows: u16, rows_changed: Vec<RowUpdate>, cursor: Option<(u16, u16)> }, Bell, Response { id: u64, result: Result<Response, WireError> }, Event { event: EditorEvent }, Exit { reason: String } }`
  - `pub enum EditorEvent { BufferOpened { path: Option<WirePath>, name: String }, BufferKilled {..}, BufferSaved {..}, ExternalChange { path: WirePath, kind: ExternalKind }, CursorMoved { client: u64, path: Option<WirePath>, line: Option<u64>, col: u64, offset: u64 }, ClientAttached { client: u64 }, ClientDetached { client: u64 } }`; `pub enum ExternalKind { Reloaded, Appended, Conflict, Deleted, Renamed { to: WirePath }, Truncated }`.
  - `pub fn write_msg<W: Write, T: Serialize>(w, &T) -> io::Result<()>`; `pub struct LineReader<R>` with `read_msg<T: DeserializeOwned>() -> Result<Option<T>, ProtoError>`; a message whose `v` is not `PROTOCOL_VERSION` is `ProtoError::Version { got, want }`.
  - Frame diffing helper: `pub fn diff_grids(old: Option<&Grid>, new: &Grid) -> ServerMsg` (changed rows only; full on size change).

- [ ] **Step 1: Failing tests:** `every_message_has_version`, `wrong_version_rejected`, `wire_path_roundtrips_non_utf8`, `oversized_line_rejected`, `diff_grids_only_changed_rows`, `serde_roundtrip_all_variants`.
- [ ] **Step 2-4:** fail, implement, pass.
- [ ] **Step 5:** Commit `feat(proto): versioned JSON-lines protocol`.

### Task 4: Text sources, line index, and persistent piece tree (`emx-text`)

**Files:** Create `identity.rs`, `source.rs`, `block_cache.rs`, `line_index.rs`, `piece_tree.rs`, `text.rs`; tests in `crates/emx-text/tests/text_props.rs`.

**Interfaces:**
- Produces:
  - `pub struct FileIdentity { pub dev: u64, pub ino: u64, pub size: u64, pub mtime_ns: i128 }` with `of_file(&File) -> io::Result<Self>`, `of_path(&Path) -> io::Result<Self>`.
  - `pub enum ReadError { Io(#[from] io::Error), Truncated { expected_len: u64, actual_len: u64 }, NotCached { block: u64 } }`
  - `pub struct AddStore` (append-only `parking_lot::RwLock<Vec<u8>>`): `new() -> Arc<Self>`, `append(&self, &[u8]) -> u64`, `len()`, `read_into(&self, off: u64, len: usize, out: &mut Vec<u8>)`.
  - `pub struct BlockCache`: `pub const BLOCK_SIZE: usize = 65536`; `open(path: &Path, capacity_bytes: usize) -> io::Result<Arc<Self>>`; `len() -> u64`; `identity() -> FileIdentity`; `get(block: u64) -> Result<Arc<[u8]>, ReadError>` (pread via `FileExt::read_at`; a short read where `len()` says the block is full returns `Truncated`); `try_get_cached(block) -> Option<Arc<[u8]>>`; `invalidate_from(offset: u64)`; `set_len(new_len: u64)` (for appends).
  - `pub enum Original { Memory(Arc<[u8]>), File(Arc<BlockCache>) }`
  - `pub struct LineIndex`: `new(file_len: u64) -> Arc<Self>`; `fully_indexed(content: &[u8]) -> Arc<Self>`; `block_count()`; `set_block(block: u64, newlines: u32)`; `get_block(block) -> Option<u32>`; `indexed_prefix_blocks() -> u64` (contiguous from 0); `is_complete() -> bool`; `extend(new_len: u64)`; `count_range(&self, cache: &BlockCache, range: Range<u64>) -> Result<Option<u64>, ReadError>`; `nth_newline(&self, cache, range, n: u64) -> Result<NthNewline, ReadError>` with `pub enum NthNewline { Found(u64), NotInRange { count: u64 }, Unknown }`.
  - `pub fn index_file(cache: &BlockCache, index: &LineIndex, cancel: &AtomicBool, progress: &mut dyn FnMut(u64 /* blocks done */)) -> Result<(), ReadError>` — reads with `read_at` directly (not through the cache, so viewport blocks are not evicted), calls `rustix::fs::fadvise(Sequential)` first.
  - `pub struct Sources { pub original: Original, pub add: Arc<AddStore>, pub index: Arc<LineIndex> }` (for `Memory` originals the index is fully populated at construction).
  - `pub enum PieceSource { Original, Add }`; `pub struct Piece { pub source: PieceSource, pub start: u64, pub len: u64, pub newlines: Option<u64> }`
  - `#[derive(Clone)] pub struct Text { /* Option<Arc<Node>> treap root + Arc<Sources> */ }` — persistent: every mutation returns a new `Text` sharing structure.
    - `Text::empty(sources: Arc<Sources>) -> Text`; `Text::from_original(sources: Arc<Sources>) -> Text`
    - `len(&self) -> u64`; `line_count(&self) -> Option<u64>` (newline count + 1; `None` while any piece is unindexed)
    - `insert(&self, at: u64, bytes: &[u8]) -> Result<Text, ReadError>`; `delete(&self, range: Range<u64>) -> Result<Text, ReadError>`
    - `read(&self, range: Range<u64>, out: &mut Vec<u8>) -> Result<(), ReadError>`; `read_cached(&self, range, out) -> Result<(), ReadError>` (returns `NotCached` instead of doing I/O)
    - `chunks(&self, range: Range<u64>) -> ChunkIter` yielding `Result<(u64, Arc<[u8]> /* or owned */), ReadError>`
    - `byte_at(&self, pos: u64) -> Result<u8, ReadError>`
    - `find_newline_forward(&self, from: u64) -> Result<Option<u64>, ReadError>`; `find_newline_backward(&self, before: u64) -> Result<Option<u64>, ReadError>` (memchr/memrchr per chunk)
    - `line_start(&self, line: u64) -> Result<LineLookup, ReadError>` where `pub enum LineLookup { Found(u64), PastEnd, Unknown }` (lines are 0-based internally; commands convert to 1-based)
    - `line_of(&self, pos: u64) -> Result<Option<u64>, ReadError>`
    - `refresh_newlines(&self) -> Text` (fills `None` piece counts from the index)
    - `pieces(&self) -> impl Iterator<Item = (u64, Piece)>` (buffer offset, piece) in order
    - `same_root(&self, other: &Text) -> bool` (Arc pointer equality, used to detect "unchanged since")
- Treap: node priority = `xxh3(node_serial)` for determinism; `split(at)` and `merge` are recursive with path copying; node summary = `{ bytes, newlines: Option<u64>, pieces }`. A newly typed character adjacent to the end of the previous `Add` piece whose bytes end at `AddStore::len()` extends that piece instead of creating a new one.

- [ ] **Step 1: Failing property tests** (proptest, 512 cases each, ops = random insert/delete/read against a `Vec<u8>` reference, bytes drawn from `[\x00-\xff]` weighted to include `\n`, `\r\n`, invalid UTF-8):

```rust
proptest! {
  #[test] fn ops_match_reference(ops in ops_strategy()) { /* after each op: text.len()==reference.len(); read(0..len)==reference; line_count()==Some(count('\n')+1) */ }
  #[test] fn persistence_old_versions_unchanged(ops in ops_strategy()) { /* keep every intermediate Text; at end each still reads its reference snapshot */ }
  #[test] fn roundtrip_crlf_and_no_trailing_newline(content in crlf_strategy()) { /* from_original(Memory(content)) then read == content */ }
  #[test] fn line_start_matches_reference(ops in ops_strategy(), line in 0u64..200) { /* LineLookup::Found(offset) equals reference computation, or PastEnd */ }
}
#[test] fn block_cache_truncation_detected() {
    // write 3 blocks, open cache, truncate file to 1 block, get(2) => Err(Truncated{expected_len, actual_len})
}
// line index: index_counts_match_memchr (random 3 MB file), nth_newline_across_blocks, count_range_partial_blocks, unknown_before_indexed
#[test] fn block_cache_survives_rename_replacement() {
    // open cache on a.txt, write new content to tmp, rename over a.txt; get(0) still returns old bytes (fd keeps old inode)
}
```
- [ ] **Step 2:** Run; fails.
- [ ] **Step 3:** Implement sources, cache (LRU by block with byte capacity; `Mutex<lru map>`), line index, treap, `Text`.
- [ ] **Step 4:** `make test TEST_ARGS="-p emx-text"` passes.
- [ ] **Step 5:** Commit `feat(text): persistent piece tree over memory or pread block cache`.

### Task 5: Display measurement (`emx-text`)

**Files:** Create `display.rs`; tests inline.

**Interfaces:**
- Produces:
  - `pub enum DisplayUnit { Grapheme { len: usize, width: u8 }, Tab, Newline, Control(u8), InvalidByte(u8) }` and `pub fn display_units(bytes: &[u8]) -> impl Iterator<Item = (usize, DisplayUnit)>`. Control chars (0x00-0x1f except `\t` `\n`, and 0x7f) render as `^X` (`^?` for 0x7f), width 2, face `EscapeGlyph`. Invalid UTF-8 bytes render as `\ooo` octal, width 4. Wide chars width 2 (`unicode-width`). Grapheme clusters via `unicode-segmentation` over valid runs.
  - `pub fn unit_width(unit: &DisplayUnit, column: usize, tab_width: usize) -> usize` (tab advances to next multiple of `tab_width`).

- [ ] **Step 1: Failing tests:** `display_control_and_invalid_bytes` (`b"a\x1b\xffb"` -> units `a`, `Control(0x1b)` width 2, `InvalidByte(0xff)` width 4, `b`), `display_wide_and_combining` (`"e\u{301}"` is one grapheme width 1; `"中"` width 2), `tab_advances_to_stop` (column 3 -> width 5).
- [ ] **Step 2-4:** fail, implement, pass.
- [ ] **Step 5:** Commit `feat(text): display units`.

### Task 6: Buffer, markers, and Emacs-model undo (`emx-core`)

**Files:** Create `config.rs` (constants table above + `Config` struct, TOML load), `buffer.rs`, `marker.rs`, `undo.rs`; tests inline and `tests/undo.rs`.

**Interfaces:**
- Consumes: `Text`, `Sources`, `ReadError`, `FileIdentity`.
- Produces:
  - `pub struct Config { pub keys: BTreeMap<String, String>, pub in_memory_max: u64, pub block_cache_bytes: u64, pub settle_window_ms: u64, pub poll_interval_ms: u64, pub journal_fsync_ms: u64, pub journal_snapshot_max: u64, pub large_save_warn: u64, pub search_match_cap: u64, pub keyboard_encoding: KeyboardEncoding /* only Kitty */, pub daemon_grace_ms: u64, pub undo_limit: u64, pub undo_strong_limit: u64, pub undo_outer_limit: u64 }`; `Config::load(path: Option<&Path>) -> Result<Config, ConfigError>`; `Default` uses the constants.
  - `slotmap::new_key_type! { pub struct BufferId; pub struct MarkerId; }`
  - `pub enum InsertionType { StayBefore, AdvanceAfter }`
  - `pub struct Buffer { pub id: BufferId, pub name: String, pub text: Text, pub file: Option<FileBinding>, pub read_only: bool, pub mark: Option<MarkerId>, pub mark_active: bool, pub mark_ring: VecDeque<MarkerId>, pub undo: UndoList, /* private: markers, modified tick vs save tick */ }`
  - `pub struct FileBinding { pub path: PathBuf, pub identity: Option<FileIdentity>, pub saving: bool }` (M2 adds external-change fields)
  - `Buffer::insert(&mut self, pos: u64, bytes: &[u8], recorder: &mut dyn ChangeRecorder) -> Result<(), EditError>`; `Buffer::delete(&mut self, range: Range<u64>, recorder) -> Result<Vec<u8>, EditError>`; `EditError::{ReadOnly, Saving, Read(ReadError), OutOfRange}`.
  - `pub trait ChangeRecorder { fn record(&mut self, buffer: BufferId, change: &Change); }` with `pub enum Change { Insert { at: u64, bytes: Vec<u8> }, Delete { at: u64, len: u64 }, ReplaceAll { text: Text } }` — the journal (Task 12) implements it.
  - `create_marker(pos, InsertionType) -> MarkerId`, `marker_pos(MarkerId) -> u64`, `set_marker(MarkerId, u64)`, `delete_marker(MarkerId)`; every insert/delete adjusts all markers (Emacs rules: a marker at the insertion point moves only if `AdvanceAfter`; markers inside a deleted range move to its start).
  - `is_modified() -> bool`, `set_unmodified()`.
  - `pub struct UndoList` implementing Emacs `buffer-undo-list` semantics: entries `Insertion { beg, end }`, `Deletion { text: Vec<u8>, pos: i64 /* negative = point was at end */ }`, `PointBefore(u64)`, `FirstChange { saved_identity: Option<FileIdentity> }` (the `(t . TIME)` entry), `MarkerAdjust`, `ReplaceAll { before: Text, after: Text }` (used by reload in M2), `Boundary`. Operations: `boundary()`, `record_insert`, `record_delete`, `undo(n, buffer) -> Result<..>` (primitive-undo), the `pending` list for consecutive undo, `undo_redo`, `undo_in_region(range)`, amalgamation for `self-insert-command` and `delete-char` (`AMALGAMATION_LIMIT` = 20), truncation per `undo_limit`/`undo_strong_limit`/`undo_outer_limit` exactly as Emacs `truncate_undo_list`.

- [ ] **Step 1: Failing tests** (`tests/undo.rs`): `undo_restores_text_and_point`, `undo_is_undoable` (undo, then a non-undo command breaks the chain, then undo redoes), `consecutive_undos_walk_back`, `undo_redo_reverses_last_undo`, `self_insert_amalgamates_20`, `undo_restores_unmodified_flag`, `undo_in_region_only_affects_region`, `undo_truncates_beyond_strong_limit`, `markers_shift_on_insert_and_delete`. Each asserts buffer text and point exactly.
- [ ] **Step 2-4:** fail, implement, pass. Emacs's `simple.el` (`undo`, `undo-more`, `undo-redo`, `undo-make-selective-list`) and `undo.c` are the reference algorithms; match them, including the "Undo" / "Undo in region" / "No further undo information" messages.
- [ ] **Step 5:** Commit `feat(core): buffer, markers, Emacs undo`.

### Task 7: Editor skeleton, events/effects, keymap, command loop, headless harness

**Files:** Create `event.rs`, `editor.rs`, `keymap.rs`, `command/mod.rs`, `killring.rs`, `testing.rs`.

**Interfaces:**
- Consumes: `emx-proto` (`Request`, `ServerMsg`, `EditorEvent`, `ClientKind`), `emx-types` (`Key`, `Grid`), Tasks 4-6.
- Produces:
  - `pub struct ClientId(pub u64)`; `slotmap::new_key_type! { pub struct WindowId; }`
  - `pub enum Event { ClientAttached { client: ClientId, cols: u16, rows: u16, kind: ClientKind }, ClientDetached(ClientId), Key(ClientId, Key), Paste(ClientId, Vec<u8>), Resize(ClientId, u16, u16), Request(ClientId, u64 /* id */, Request), FileLoaded { buffer: BufferId, result: Result<Loaded, String> }, SaveFinished { buffer: BufferId, result: Result<FileIdentity, String> }, BlockReady { buffer: BufferId }, IndexProgress { buffer: BufferId, blocks_done: u64, complete: bool }, SearchResult { token: u64, result: SearchOutcome }, FileObserved { buffer: BufferId, observation: Observation /* M2 */ }, Timer(TimerId), Recovered(Vec<RecoveredBuffer>) }`
  - `pub enum Effect { Send(ClientId, ServerMsg), Load { buffer: BufferId, path: PathBuf }, Save { buffer: BufferId, plan: SavePlan }, Prefetch { buffer: BufferId, block: u64 }, StartIndex { buffer: BufferId }, StartSearch { token: u64, job: SearchJob }, CancelSearch(u64), Watch(PathBuf), Unwatch(PathBuf), Journal(JournalOp), SetTimer { id: TimerId, after: Duration }, Release { client: ClientId, request: u64 } /* --wait */, Shutdown }`
  - `pub struct Editor` with `new(config: Config) -> Editor`, `handle(&mut self, ev: Event)`, `take_effects(&mut self) -> Vec<Effect>`, `render(&self, client: ClientId) -> (Grid, Vec<PrefetchRequest>)`, `has_unsaved(&self) -> bool`, `client_count(&self) -> usize`.
  - `pub struct Keymap` (trie: `HashMap<Key, Binding>`, `Binding::{Command(&'static str), Prefix(Keymap)}`); `Keymap::emacs_default() -> Keymap`; `apply_overrides(&mut self, keys: &BTreeMap<String, String>) -> Result<(), ConfigError>` (unknown command names are errors).
  - Command registry: `pub struct CommandSpec { pub name: &'static str, pub run: fn(&mut CommandContext) -> Result<(), CommandError> }`; `pub fn lookup(name: &str) -> Option<&'static CommandSpec>`; `CommandContext` exposes the editor, current client, window, buffer, `prefix_arg: PrefixArg` (`None`, `Raw(Vec of C-u count)`, `Numeric(i64)`, `Negative`), `last_command`, `this_command`, `message(..)`, `error(..)` (errors ring the bell and show the message; `C-g` raises `Quit`).
  - Universal argument per Emacs: `C-u` (4, 16, ...), `C-u <digits>`, `C-u -`, `M-<digit>`, `M--`, `C-<digit>`, `C--`.
  - `KillRing` (`KILL_RING_MAX`), `kill_new`, `kill_append(text, before: bool)`, `current_kill(n)`, yank pointer.
  - `testing::Harness`: `Harness::new() -> Harness` (80x24 client attached), `with_text(&str with '|' marking point, optional '^' marking mark)`, `keys(&mut self, kbd: &str)`, `text() -> String`, `point() -> u64`, `mark() -> Option<u64>`, `kill_ring() -> Vec<String>`, `echo() -> String`, `screen() -> Vec<String>`. The harness runs effects synchronously by calling the same `io`/search functions the server workers call.

- [ ] **Step 1: Failing tests:** `prefix_arg_forms` (`C-u C-u` = 16, `M-5` = 5, `C-u - ` negative), `unbound_key_message` (`C-c z` is unbound; echo shows `C-c z is undefined`), `override_rebinds` (`keys = { "C-c t" = "forward-char" }`), `unknown_command_override_errors`, `keyboard_quit_cancels_prefix`.
- [ ] **Step 2-4:** fail, implement, pass.
- [ ] **Step 5:** Commit `feat(core): editor state machine, keymap, command loop, harness`.

### Task 8: Basic movement and editing commands plus the differential-test harness

**Files:** Create `command/move.rs`, `edit.rs`, `kill.rs`, `mark.rs`; `difftest/emx-difftest.el`; `difftest/cases/m1-basic.jsonl`; `crates/emx-core/tests/difftest.rs`.

**Interfaces:**
- Consumes: Harness, Buffer, KillRing, UndoList.
- Produces commands (Emacs names, bound as in Emacs): `forward-char` `backward-char` `next-line` `previous-line` (logical lines in M1; visual in Task 22) `move-beginning-of-line` `move-end-of-line` `forward-word` `backward-word` `beginning-of-buffer` `end-of-buffer` (both push mark, "Mark set") `self-insert-command` `newline` `delete-char` `delete-backward-char` `kill-line` `kill-region` `kill-ring-save` `yank` `yank-pop` `set-mark-command` (incl. `C-u C-SPC` pop) `exchange-point-and-mark` `undo` (`C-/`, `C-_`, `C-x u`) `undo-redo` (`C-?`, `C-M-_`) `keyboard-quit` `transient-mark-mode` behavior on (region active after `C-SPC` + motion, deactivated by edits and `C-g`).
- Word syntax = Emacs `standard-syntax-table` word constituents (Unicode letters and digits); `_` is a symbol constituent, not a word constituent.
- Differential case format (JSON lines): `{"name": "kill-line-eol", "text": "ab|c\ndef", "keys": "C-k C-k", "tier": 1}` where `|` marks point and optional `^` marks mark. Output compared: `{"text", "point", "mark", "mark_active", "kill_ring": [first 3]}`.
- `emx-difftest.el`: `emacs --batch -Q -l difftest/emx-difftest.el --cases FILE --out FILE`; per case: fresh buffer in `fundamental-mode`, `(transient-mark-mode 1)`, insert text, set point/mark, `(execute-kbd-macro (kbd KEYS))` inside `condition-case` (errors recorded as `{"error": "..."}` and compared to emx's error message), emit JSON with 1-based Emacs positions converted to 0-based byte offsets.
- `tests/difftest.rs` runs Emacs once over all `difftest/cases/*.jsonl`, runs each case through `Harness`, and fails listing every mismatching case with both outputs. Tier-1 pass rate must be 100%.
- Window-dependent commands (`C-v`, `M-v`, `C-l`, visual `C-n`) are not differential cases (batch Emacs has no real window); they get unit tests against Emacs-documented behavior in Task 9.

- [ ] **Step 1:** Write `m1-basic.jsonl` with at least 80 cases covering every command above, including `eob-*` cases (empty buffer, no trailing newline, only newlines, CRLF text), kill appending (`C-k C-k`, `M-d M-d`), `C-y M-y M-y`, prefix args (`C-u 3 C-f`, `M-- C-k`), undo chains. Write the runner. Run `make difftest`: fails (commands missing).
- [ ] **Step 2:** Implement commands until `make difftest` passes 100%.
- [ ] **Step 3:** Commit `feat(core): basic Emacs editing commands with differential tests against Emacs`.

### Task 9: Window rendering, modeline, echo area, scrolling

**Files:** Create `window.rs`, `render.rs`, `command/misc.rs` (scroll, recenter).

**Interfaces:**
- Produces:
  - Per-client `Frame { root: WindowTree, selected: WindowId, minibuffer: MinibufferState, echo: Option<EchoMessage>, cols, rows }`; `Window { buffer: BufferId, point: MarkerId, start: MarkerId, start_row_skip: u32, hscroll: u32 }` (point and start are markers in the buffer so other clients' edits move them).
  - `render(&Editor, ClientId) -> (Grid, Vec<PrefetchRequest>)`: each window draws text from `start` using `display_units`, wraps long lines with `\` in the last column (Emacs continuation glyph; line truncation is not offered in this release), then draws its modeline. Modeline format: `-UUU:**-  NAME   POS L<line>   (Fundamental)` matching Emacs: `**` modified, `%%` read-only, `--` clean; `POS` is `All`/`Top`/`Bot`/`NN%`; `L?` when the line number is unknown; the external-change and conflict badges are added in M2.
  - Point visibility: after every command, if point is outside the window, recenter (Emacs `scroll-conservatively` 0).
  - Commands: `scroll-up-command` (`C-v`), `scroll-down-command` (`M-v`) by window height minus `NEXT_SCREEN_CONTEXT_LINES`, with Emacs's "End of buffer"/"Beginning of buffer" errors; `recenter-top-bottom` (`C-l`) cycling middle, top, bottom.
  - Rendering only reads cached bytes (`read_cached`); a `NotCached` block renders as blank rows and returns a `PrefetchRequest`.

- [ ] **Step 1: Failing tests:** `render_basic_screen` (exact `screen()` lines for a 20x6 client including the modeline string), `wrap_with_backslash`, `control_char_rendered_escaped` (no raw ESC byte ever appears in any `Cell.text`), `render_1x1_does_not_panic`, `render_zero_cols_is_error_free`, `c_v_scrolls_by_height_minus_2`, `c_l_cycles_middle_top_bottom`, `point_moving_offscreen_recenters`, `notcached_block_requests_prefetch`.
- [ ] **Step 2-4:** fail, implement, pass.
- [ ] **Step 5:** Commit `feat(core): windows, rendering, modeline, scrolling`.

### Task 10: File open and save (`io.rs`, `inplace.rs`, `autosave.rs`, `command/files.rs`) with the basic minibuffer

**Files:** Create `minibuffer.rs` (line editing, history, `y-or-n-p`, `yes-or-no-p`, plain string reads), `io.rs`, `inplace.rs`, `autosave.rs`, `command/files.rs`.

**Interfaces:**
- Produces:
  - `pub fn load_file(path: &Path, config: &Config) -> Result<Loaded, LoadError>`: canonicalizes symlinks for saving but keeps the visited name; `size <= in_memory_max` reads fully (`Original::Memory`, index complete); otherwise `Original::File(BlockCache)` with an empty `LineIndex` and an `Effect::StartIndex`. Nonexistent file: empty buffer, "(New file)" message, as Emacs.
  - Buffers are created immediately in a loading state (read-only, modeline shows `Loading`), filled on `FileLoaded`.
  - `pub enum SavePlan { Rename { target: PathBuf, text: Text }, InPlace { target: PathBuf, text: Text, moves: Vec<Move> } }`; `pub fn plan_save(binding, text, metadata, euid) -> SavePlan` choosing `InPlace` when `nlink > 1`, the user is not the owner and not root, or the directory is not writable; else `Rename`.
  - `pub fn save_rename(plan) -> Result<FileIdentity, SaveError>`: write temp in the same directory, copy mode, xattrs (`xattr` crate, includes POSIX ACLs), `fchown` when euid is 0, `fsync` file, `rename`, `fsync` directory.
  - `pub fn plan_in_place(text: &Text, file_len: u64) -> Vec<Move>` and `pub fn save_in_place(plan) -> Result<FileIdentity, SaveError>` implementing the DESIGN 5 disk memmove: left-shifting original pieces ascending, right-shifting pieces descending (each copied back to front in `BLOCK_SIZE` chunks), unshifted pieces skipped, then `Add` bytes, then `set_len`, then `fsync`. Reads bypass the block cache; the cache is invalidated after.
  - Large-save prompt when `Rename` and size > `LARGE_SAVE_WARN`: `Saving NAME rewrites SIZE and needs SIZE free. (r)ewrite safely, (i)n place, (c)ancel?`
  - The buffer is read-only (`EditError::Saving`) from save start to `SaveFinished`.
  - Save over newer disk content (identity on disk differs from `binding.identity`) asks `NAME has changed since visited or saved.  Save anyway? (yes or no)` (Emacs wording).
  - Commands: `find-file` (`C-x C-f`, plain minibuffer read with default directory; completion arrives in Task 17), `save-buffer` (`C-x C-s`, "Wrote PATH"), `write-file` (`C-x C-w`), `save-some-buffers` (`C-x s`, y/n/!/q), `kill-buffer` (`C-x k`, asks when modified), `save-buffers-kill-terminal` (`C-x C-c`: offers to save modified buffers; in server mode then detaches; in `--no-server` mode asks `Modified buffers exist; exit anyway? (yes or no)` if any remain), `server-edit` (`C-x #`), `recover-this-file`.
  - Auto-save: `#NAME#` in the file's directory after `AUTO_SAVE_INTERVAL_KEYS` edits or `AUTO_SAVE_TIMEOUT_S` idle, only for buffers at or below `in_memory_max`; deleted on save; `recover-this-file` restores it.

- [ ] **Step 1: Failing tests** (tempdir-based): `open_small_reads_memory`, `open_large_uses_block_cache` (set `in_memory_max` to 1 KiB in test config), `save_rename_preserves_mode_and_xattr`, `save_symlink_writes_target_keeps_link`, `save_hardlinked_uses_in_place`, `in_place_plan_orders_moves` (unit test over synthetic piece layouts: left shift, right shift, mixed, unshifted), `in_place_save_matches_reference` (proptest: random edits on a 1 MiB file saved in place equal `Text::read` output), `save_enospc_leaves_original_intact` (rename path with an injected writer that fails after N bytes: original bytes and identity unchanged, buffer still modified, echo shows the error), `save_permission_denied_keeps_buffer_modified`, `edits_rejected_while_saving`, `save_over_newer_disk_asks`, `autosave_written_and_removed`, `c_x_c_c_noserver_prompts_when_modified`.
- [ ] **Step 2-4:** fail, implement, pass.
- [ ] **Step 5:** Commit `feat(core): open and save with rename and in-place policies, auto-save`.

### Task 11: Terminal layer (`emx-term`)

**Files:** Create `kitty.rs`, `terminal.rs`, `screen.rs`.

**Interfaces:**
- Produces:
  - `pub enum InputEvent { Key(Key), Paste(Vec<u8>), KittyFlags(u32), PrimaryDeviceAttributes, Unknown(Vec<u8>) }`
  - `pub struct KittyDecoder`; `feed(&mut self, bytes: &[u8]) -> Vec<InputEvent>`; decodes `CSI code ; mods u`, `CSI 1 ; mods A-D/H/F`, `CSI n ~` function keys, legacy single bytes for plain text (flag 1 sends plain text keys as UTF-8), bracketed paste `CSI 200~ ... CSI 201~`, replies `CSI ? flags u` and `CSI ? ... c`. Modifier encoding: value - 1 bitfield (shift 1, alt 2 -> `meta`, ctrl 4, super 8). Partial sequences are buffered across `feed` calls.
  - `pub struct Terminal` (RAII): enter raw mode (crossterm), alternate screen, hide cursor during draws, bracketed paste on, push Kitty flags `CSI > 1 u`; `Drop` and a panic hook restore everything (`CSI < u`, paste off, main screen, cooked mode).
  - `pub fn probe_kitty(input: &mut impl Read, output: &mut impl Write, timeout: Duration) -> Result<bool, TermError>`: writes `CSI ? u` then `CSI c`; true if a flags reply arrives before the DA reply. On false the client exits with: `emx: this terminal does not support the Kitty keyboard protocol. Supported in this release: kitty, WezTerm, Ghostty, foot.`
  - `pub struct Screen { current: Option<Grid> }`; `draw(&mut self, next: &Grid, out: &mut Vec<u8>)` emits minimal cursor moves, SGR per `Face` (fixed default theme), wide-char handling, and final cursor placement.

- [ ] **Step 1: Failing tests:** `decode_ctrl_meta_combos` (`\x1b[120;5u` = `C-x`, `\x1b[37;3u` = `M-%`, `\x1b[115;7u` = `C-M-s`, `\x1b[32;5u` = `C-SPC`, `\x1b[47;5u` = `C-/`), `decode_plain_utf8_text`, `decode_split_sequence_across_feeds`, `decode_bracketed_paste`, `probe_detects_support_and_absence`, `screen_diff_minimal` (second identical draw emits no cell output), `screen_never_emits_raw_control_from_cells`.
- [ ] **Step 2-4:** fail, implement, pass.
- [ ] **Step 5:** Commit `feat(term): Kitty keyboard decoding and screen diffing`.

### Task 12: Journal (`journal.rs`)

**Files:** Create `crates/emx-core/src/journal.rs`; tests `tests/journal.rs`.

**Interfaces:**
- Consumes: `Change`, `ChangeRecorder`, `FileIdentity`, `Text`.
- Produces:
  - Format: one file per buffer, `<journal_dir>/<buffer-uuid>.jnl`; records are length-prefixed (`u32` LE length + `u32` LE CRC32 + bincode-free hand-written little-endian payload). Record kinds: `Header { path: Option<PathBuf bytes>, name }`, `EpochSnapshot { content }`, `EpochIdentity { identity }`, `Insert { at, bytes }`, `Delete { at, len }`, `Saved { identity }`, `Killed`.
  - `pub enum JournalOp { Open { buffer, header }, Record { buffer, record }, Close { buffer } }` (emitted as `Effect::Journal`).
  - `Editor` implements `ChangeRecorder` by pushing `JournalOp::Record`; a new epoch is emitted on open, save, reload, and undo of a reload (snapshot when `len <= journal_snapshot_max`, identity otherwise).
  - `pub fn replay_dir(dir: &Path, config: &Config) -> Result<Vec<RecoveredBuffer>, JournalError>`: reads each journal, stops at the first record with a bad CRC or truncated length (a torn tail write) and keeps everything before it; rebuilds content from the last epoch base plus records; identity-only base that no longer matches disk yields `RecoveredBuffer::EditsOnly { name, edits: Vec<(u64, Vec<u8> or delete len)> }` rendered as the read-only `*recovered: NAME*` buffer.
  - `pub struct JournalWriter` (used by the server thread in Task 13): `apply(op)`, `flush_and_sync()`.

- [ ] **Step 1: Failing tests:** `replay_rebuilds_edits_on_snapshot`, `replay_identity_base_matching_disk`, `replay_identity_mismatch_gives_edits_only`, `torn_tail_record_ignored`, `saved_buffer_journal_removed`, proptest `replay_equals_live_buffer` (random edits, saves, and a truncation of the journal at a random byte: replay equals the buffer state at the last complete record).
- [ ] **Step 2-4:** fail, implement, pass.
- [ ] **Step 5:** Commit `feat(core): crash-safe edit journal with replay`.

### Task 13: Server, client, `--no-server`, CLI (`emx` binary)

**Files:** Create `src/paths.rs`, `src/server/*`, `src/client.rs`, `src/cli.rs`, `src/noserver.rs`, modify `src/main.rs`; tests `tests/{pty_basic.rs, server.rs, cli.rs, crash.rs, paths.rs}` with a shared `tests/common/mod.rs` PTY harness (`portable-pty` + `vt100`; the harness answers `CSI ? u` with `CSI ? 1 u` and `CSI c` with `CSI ? 62 c`, and encodes keys with the Kitty encoding).

**Interfaces:**
- Consumes: everything above.
- Produces:
  - `paths::socket_dir(euid: u32) -> Result<PathBuf, PathError>`: `EMX_SOCKET_DIR` if set (still subject to the ownership check), else `/run/user/<euid>/emx` when `/run/user/<euid>` exists, else `/tmp/emx-<euid>`; creates it with mode `0700`; refuses (error naming the path, owner, and mode) unless owned by `euid` with mode exactly `0700`. `XDG_RUNTIME_DIR` is never read.
  - `paths::state_dir()` (`$XDG_STATE_HOME/emx` or `~/.local/state/emx`), `journal_dir(mode)` (`server/` or `noserver-<pid>/`), `config_path()` (`$XDG_CONFIG_HOME/emx/config.toml` or `~/.config/emx/config.toml`).
  - Server: single instance via `flock` on `<socket_dir>/server.lock`; stale socket removed only while holding the lock; accept loop checks `SO_PEERCRED` uid == euid; per client a reader thread (decode `ClientMsg` -> `Event`) and a writer thread (bounded channel of depth `FRAME_QUEUE_DEPTH`; when full, frames are dropped and the next frame is sent `full: true`); the editor thread runs `Editor::handle` and dispatches effects to the I/O pool (4 threads), indexer, search workers, journal writer (batched `fsync` every `JOURNAL_FSYNC_MS` or on idle), and watcher (M2). On start it replays journals (`Event::Recovered`). Lifetime: when `client_count() == 0` and `!has_unsaved()`, exit after `DAEMON_GRACE_MS` unless a client attaches. `SIGTERM`/`SIGINT`: flush and sync the journal, exit.
  - Client (`emx [FILES]`): connect (auto-start `emx server --detach` with `setsid`, stdio to `/dev/null`, log to `state_dir/server.log`, wait up to 2 s for the socket); probe Kitty; send `Hello` and files as `Open` requests; forward keys/paste/resize; draw frames; exit on `Exit`.
  - `emx --wait FILE`: TUI client released by `C-x #` or killing the buffer. `emx open FILE[:LINE[:COL]] [--wait]`: CLI client; displays in the most recently active TUI client; with no TUI client attached it fails with `emx: no emx client is attached; run 'emx FILE' in a terminal first`. `FILE:LINE:COL` parsing treats a suffix as line/col only when it parses as numbers and the full path does not exist.
  - `emx list-buffers [--json]`, `emx reload [FILE]`, `emx goto [FILE:]LINE[:COL]`, `emx save-all`, `emx events` (JSON lines to stdout until interrupted), `emx diff-external FILE` (M2 fills the response; in M1 it returns an error `no external change recorded`). With no server running, these exit 1 with `emx: no server running`.
  - `emx --no-server [FILES]`: same editor, in-process channel transport, journal in `noserver-<pid>/`; on start recovers orphaned `noserver-*` journals whose lock is not held.

- [ ] **Step 1: Failing tests:**
  - `paths.rs`: `socket_dir_rejects_wrong_mode` (0755 dir -> error), `socket_dir_rejects_foreign_owner` (inject owner uid != euid via a test seam `check_dir(meta, euid)`), `xdg_runtime_dir_ignored`.
  - `pty_basic.rs`: `type_and_save_noserver` (spawn `emx --no-server f.txt`, type `hello`, `C-x C-s`, file contains `hello`), `unsupported_terminal_exits_with_message` (harness does not answer the probe), `modeline_shows_modified`.
  - `server.rs`: `autostart_and_attach`, `detach_reattach_keeps_unsaved_edits`, `two_clients_same_buffer_points_shift`, `kill_buffer_shown_in_other_client`, `server_exits_when_idle_and_clean` (within `DAEMON_GRACE_MS` + 1 s), `server_stays_with_unsaved`, `peer_uid_checked` (unit-level: handshake rejects a mismatched uid via the seam).
  - `cli.rs`: `open_wait_released_by_c_x_hash`, `list_buffers_json`, `goto_moves_point`, `save_all_writes`, `events_stream_reports_open_and_save`, `cli_open_path_with_newline_and_invalid_utf8`, `cli_without_server_errors`.
  - `crash.rs`: `sigkill_server_mid_edit_recovers_everything` (type 500 characters across two buffers, `kill -9` the server, start a client: both buffers restored modified with identical text), `noserver_crash_recovered_by_next_noserver`.
- [ ] **Step 2-4:** fail, implement, pass (`make test`).
- [ ] **Step 5:** `make check`. Commit `feat: session server, client, CLI, no-server mode (M1)`. Tag `m1`.

---

## Milestone M2: External Edits and Live Reload

### Task 14: Reload state machine (`reload.rs`)

**Files:** Create `crates/emx-core/src/reload.rs`; extend `FileBinding`, `UndoList`, `render.rs` (badges); tests `tests/reload.rs`.

**Interfaces:**
- Produces:
  - `pub struct Observation { pub identity: Option<FileIdentity> /* None = missing */, pub tail_hash: Option<u64>, pub at: Instant }`
  - `FileBinding` gains `external: ExternalState` with `pub enum ExternalState { Clean, Settling { first_seen: Instant, last: Observation }, Conflict { disk: FileIdentity }, Deleted, Renamed(PathBuf), KeepMine { disk: FileIdentity } }`, `previous_text: Option<Text>` (content before the last reload, for diff-external), `changed_since_viewed: bool`, `tail_hash: u64` (xxh3 of the `APPEND_TAIL_HASH_BYTES` before EOF, recomputed on load, save, reload, append), `own_save_identity: Option<FileIdentity>`, `polled: bool`.
  - `pub fn classify(binding: &FileBinding, old: &FileIdentity, obs: &Observation) -> Classification` with `pub enum Classification { Unchanged, OwnSave, Appended { new_len: u64 }, Replaced, Truncated, Deleted }`. Append rule exactly: same `ino` and `dev`, `size` grew, `tail_hash` of the bytes before the old EOF unchanged.
  - Settle rule: on a watcher event or poll difference, enter `Settling`; at each `SETTLE_WINDOW_MS` timer re-observe; settled when two consecutive observations have identical identity (size, mtime, inode). Before applying a reload to a modified buffer, re-observe once more.
  - Unmodified buffer + settled `Replaced`: load new content (I/O effect), then replace text with an undo entry `ReplaceAll { before, after }`; preserve each window point and start by (line number, column, the line's text): search for the same line text within +-50 lines of the old line, nearest first; else same line number clamped.
  - `Appended`: extend original (`BlockCache::set_len` / reload small in-memory files fully), add an `Original` piece, extend the line index; windows whose point was at EOB stay at EOB (follow).
  - Modified buffer + external change: `Conflict`; modeline badge `[CONFLICT]`, echo `NAME changed on disk; M-x diff-external, M-x revert-buffer, M-x keep-mine`. Never auto-reload.
  - Undoing a `ReplaceAll` sets `content_predates_disk = true`; a later save asks `Saving will overwrite newer changes on disk (made after NAME was reloaded).  Save anyway? (yes or no)`.
  - Commands: `revert-buffer` (reload discarding mine; asks `Revert buffer from file PATH? (yes or no)` when modified), `keep-mine` (`KeepMine`; next save writes without the newer-disk prompt), `diff-external` (opens `*diff-external*` with a unified diff of `previous_text` vs current via `similar`; refuses above `IN_MEMORY_MAX` with a message), `follow-edit-mode` (toggle; on reload moves the viewport to the first changed hunk).
  - Deleted file: buffer kept, badge `[DELETED]`, saving recreates it. Rename detected via watcher rename events: badge `[MOVED to NEW]`.
  - Truncation of a `File`-backed buffer: unmodified -> reload; modified with `Original` pieces past the new length -> `Conflict` plus a warning that the referenced original bytes are gone, and `ReadError::Truncated` from reads is surfaced as an error message, never a panic.

- [ ] **Step 1: Failing tests:** `append_detected_only_when_tail_unchanged`, `same_size_rewrite_is_replaced`, `inode_change_is_replaced`, `own_save_not_reloaded`, `settle_waits_for_two_equal_observations`, `unmodified_reload_preserves_point_by_line_context`, `modified_buffer_gets_conflict_not_reload`, `reload_is_undoable`, `save_after_undoing_reload_warns`, `keep_mine_suppresses_warning`, `diff_external_shows_hunks`, `deleted_flagged_and_save_recreates`, `truncated_file_backed_buffer_conflicts`, proptest `no_content_lost` (random interleavings of user edits, external writes (rewrite, append, truncate, delete), reloads, undos, keep-mine, saves: at every step, every user-typed byte is either in the buffer, in the undo list, or was on disk at a save the user confirmed — Goal 1).
- [ ] **Step 2-4:** fail, implement, pass.
- [ ] **Step 5:** Commit `feat(core): external change state machine with undoable reloads`.

### Task 15: Watcher, polling, notifications, change markers, events (server side)

**Files:** Create `src/server/watcher.rs`; extend editor notifications and `EditorEvent` emission; tests `tests/reload_e2e.rs`.

**Interfaces:**
- Produces:
  - Watcher thread: `notify::RecommendedWatcher` watching the parent directory of each visited file (non-recursive), reference-counted per directory; events filtered by file name and forwarded as `Event::FileObserved` after a `stat` in the watcher thread (never on the editor thread).
  - Filesystem kind via `rustix::fs::statfs` `f_type`: NFS `0x6969`, SMB `0x517B`, CIFS `0xFF534D42`, SMB2 `0xFE534D42`, FUSE `0x65735546`, 9P `0x01021997` -> polled: `stat` every `POLL_INTERVAL_MS` on an I/O thread for open buffers on that filesystem only; modeline shows `[poll]`.
  - Multi-file bursts: reloads applied within one settle round are summarized in one echo message `Reloaded N files changed on disk: a.rs, b.rs, ...` (truncated to the echo width) and one `EditorEvent` per file.
  - `changed_since_viewed` set on reload of a buffer not displayed in any window; cleared when displayed; modeline badge `[ext]`.
  - Notifications are echo-area messages only; nothing in this task opens a prompt.
  - `diff-external` CLI returns the unified diff text.

- [ ] **Step 1: Failing e2e tests** (server mode, real files): `agent_burst_writes_coalesce` (20 writes in 100 ms -> one reload), `in_place_writer_never_shows_partial` (writer writes half, sleeps 50 ms, writes rest: buffer only ever shows old or complete new content), `atomic_rename_save_reloads`, `append_follows_tail`, `git_checkout_multi_file_single_summary` (5 files rewritten together -> one summary message), `own_save_no_reload_loop`, `conflict_badge_on_modified`, `delete_then_recreate`, `events_stream_reports_external_change`, `diff_external_cli_prints_diff`, `polling_used_when_fs_is_network` (seam: force `FsKind::Network` for a tempdir).
- [ ] **Step 2-4:** fail, implement, pass.
- [ ] **Step 5:** `make check`. Commit `feat: live reload, watcher, polling, notifications (M2)`. Tag `m2`.

---

## Milestone M3: Emacs Tier-1 Parity

### Task 16: Windows and buffer list

**Files:** `command/window.rs`, `window.rs` extensions.

**Interfaces:**
- Produces: `split-window-below` (`C-x 2`), `split-window-right` (`C-x 3`), `delete-window` (`C-x 0`), `delete-other-windows` (`C-x 1`), `other-window` (`C-x o`, with prefix arg), `switch-to-buffer` (`C-x b`), `list-buffers` (`C-x C-b`, `*Buffer List*` with Emacs columns `CRM Buffer Size Mode File`, RET visits), window-local point/start per Emacs. Each window draws its own modeline; inactive windows use `ModeLineInactive`. Too-small splits error `Window #<window N> too small for splitting` (Emacs wording; minimum height 4 lines incl. modeline, width 10 columns).

- [ ] **Step 1: Failing tests:** `split_and_cycle`, `windows_on_same_buffer_have_independent_points`, `delete_other_windows`, `split_too_small_errors`, `buffer_list_visits`.
- [ ] **Step 2-4:** fail, implement, pass.
- [ ] **Step 5:** Commit `feat(core): windows and buffer list`.

### Task 17: Vertical incremental completion

**Files:** `completion.rs`, `minibuffer.rs` extensions.

**Interfaces:**
- Produces: `pub trait CompletionSource { fn candidates(&self, input: &str) -> Vec<Candidate>; }` with sources for files (directory listing of the input's directory, directories suffixed `/`), buffers (most recent first, current buffer last), commands (all registered names). Matching: `flex` style like `fido-mode` (characters in order, scored: prefix > substring > flex). Display: up to 10 candidates, one per line, above the prompt line; selected candidate highlighted; `C-n`/`C-p`/`<down>`/`<up>` move; `RET` accepts the selected candidate (or the input if there are none); `C-j`/`M-j` accept the literal input; `DEL` at a `/` boundary in file completion goes up one directory; `M-p`/`M-n` history per prompt kind; `C-g` aborts. Used by `find-file`, `write-file`, `switch-to-buffer`, `kill-buffer`, `execute-extended-command` (`M-x`).

- [ ] **Step 1: Failing tests:** `file_completion_lists_and_narrows`, `ret_selects_top_candidate`, `c_j_accepts_literal_new_file`, `del_at_slash_goes_up`, `buffer_completion_order`, `m_x_runs_command`, `history_m_p`.
- [ ] **Step 2-4:** fail, implement, pass.
- [ ] **Step 5:** Commit `feat(core): vertical incremental completion`.

### Task 18: Search engine, isearch, query-replace, occur

**Files:** `emx-text/src/search/{mod.rs, emacs_regex.rs}`, `emx-core/src/isearch.rs`, `command/{search.rs, replace.rs, occur.rs}`.

**Interfaces:**
- Produces:
  - `pub enum Pattern { Literal { needle: Vec<u8>, case_fold: bool }, Regex { emacs: String, case_fold: bool } }`; `pub struct CompiledPattern`; `CompiledPattern::compile(&Pattern) -> Result<Self, PatternError>`; `pub fn emacs_regex_to_rust(src: &str) -> Result<String, PatternError>` (DESIGN 5 list; `PatternError::Unsupported { construct, position }`).
  - `pub struct Match { pub start: u64, pub end: u64, pub capped: bool }`
  - `pub fn search_forward(text: &Text, pat: &CompiledPattern, from: u64, bound: Option<u64>, cancel: &AtomicBool, match_cap: usize) -> Result<Option<Match>, SearchError>` (chunked with `match_cap` overlap; literal case-sensitive uses `memmem::Finder`).
  - `pub fn search_backward(text, pat, point: u64, bound, cancel, match_cap) -> Result<Option<Match>, SearchError>` with the DESIGN 5 Emacs semantics (closest start before point whose anchored match ends at or before point; `memmem::FinderRev` for literals; literal-prefix candidates via `regex-syntax` extraction, else every position).
  - `pub fn matches_in(text, pat, range, match_cap) -> Result<Vec<Match>, SearchError>` (lazy highlight of the visible region only).
  - Case fold: Emacs `case-fold-search` t with `search-upper-case`: any uppercase character in the input makes the search case-sensitive.
  - isearch (`C-s`, `C-r`, `C-M-s`, `C-M-r`): prompts `I-search: `, `Failing I-search: `, `Wrapped I-search: `, `Overwrapped`, regexp variants `Regexp I-search: `; `C-s` repeats/wraps; `DEL` undoes the last isearch step; `C-w` yanks the next word; `RET` exits leaving mark at start ("Mark saved where search started"); `C-g` once removes the failing part, twice (or when not failing) aborts back to the start; other commands exit and run. Searches over buffers larger than `IN_MEMORY_MAX` run as `Effect::StartSearch` with the echo showing `[searching...]`; results arrive as `Event::SearchResult`; `C-g` cancels.
  - `query-replace` (`M-%`) and `query-replace-regexp` (`C-M-%`): prompts and responses `y`/`SPC`, `n`/`DEL`, `!`, `q`/`RET`, `.`, `^`, `C-g`; `\N` and `\&` in regexp replacements; final message `Replaced N occurrences`; one undo group per replacement as in Emacs.
  - `occur` (`M-s o`): `*Occur*` buffer `N matches for "PAT" in buffer: NAME` with `LINE:text` rows; `RET` visits.

- [ ] **Step 1: Failing tests:** emacs_regex translation table test (30 pairs), `backward_search_emacs_semantics` (`aaa`/`aa` -> start 1), `forward_match_across_chunk_boundary`, `match_longer_than_cap_is_flagged` (`Match` has `pub capped: bool`; a match reaching the cap is returned with `capped: true` and the command shows `Match truncated at search_match_cap`), `cancel_stops_search`, isearch harness tests, `query_replace_bang_replaces_rest`, `occur_lists_matches`.
- [ ] **Step 2-4:** fail, implement, pass.
- [ ] **Step 5:** Commit `feat: search engine, isearch, query-replace, occur`.

### Task 19: Remaining tier-1 editing commands

**Files:** `command/{edit.rs, kill.rs, mark.rs, macro.rs, register.rs, rect.rs, dabbrev.rs, misc.rs}`.

**Interfaces:**
- Produces (Emacs names and bindings): `backward-sentence` `forward-sentence` (`M-a`/`M-e`, `sentence-end-double-space` t), `kill-word` `backward-kill-word` (`M-d`, `M-DEL`), `upcase-word` `downcase-word` `capitalize-word` (`M-u/l/c`), `transpose-chars` (`C-t`), `transpose-words` (`M-t`), `open-line` (`C-o`), `delete-blank-lines` (`C-x C-o`), `mark-whole-buffer` (`C-x h`), mark ring pop (`C-u C-SPC`), `dabbrev-expand` (`M-/`, repeat cycles; search bounded to `IN_MEMORY_MAX` bytes around point), `indent-for-tab-command` in fundamental mode = `indent-relative` (`TAB`), `goto-line` (`M-g g`, `M-g M-g`), `copy-to-register` `insert-register` `point-to-register` `jump-to-register` (`C-x r s/i/SPC/j`), `kill-rectangle` `yank-rectangle` `string-rectangle` (`C-x r k/y/t`), `kmacro-start-macro` `kmacro-end-macro` `kmacro-end-and-call-macro` (`C-x (`, `C-x )`, `C-x e`, then `e` repeats), `keyboard-quit` aborting everything including macro definition.

- [ ] **Step 1:** Add differential cases to `difftest/cases/m3-tier1.jsonl` for every command above (at least 150 cases, including prefix args and edge positions). Run `make difftest`: fails.
- [ ] **Step 2:** Implement until 100% pass.
- [ ] **Step 3:** Commit `feat(core): tier-1 Emacs editing commands`.

### Task 20: Differential suite completion and tier-1 list

**Files:** `difftest/TIER1.md` (the tier-1 command list: every command in DESIGN 4 with its binding and the case files covering it), `difftest/cases/*.jsonl`.

- [ ] **Step 1:** Write `TIER1.md`. Add cases until every listed command has at least 3 cases (isearch, query-replace, M-x, and minibuffer-driven commands included via keyboard macros; file commands use a temp directory passed to both sides).
- [ ] **Step 2:** `make difftest`: total cases >= 250, 100% pass. Fix emx (never the expected Emacs output).
- [ ] **Step 3:** `make check`. Commit `test: tier-1 differential suite (M3)`. Tag `m3`.

---

## Milestone M4: Large Files

### Task 21: Background indexer, unknown line numbers, pending goto

**Files:** `src/server/workers.rs` (indexer thread), `emx-core` editor and render updates.

**Interfaces:**
- Produces: `Effect::StartIndex` spawns `index_file` on its own thread with progress every 256 blocks as `Event::IndexProgress`; on progress the editor calls `Text::refresh_newlines`. Modeline shows `L?` while the point's line is unknown and `Indexing NN%` badge. `M->` jumps instantly. `goto-line N` with N beyond the indexed prefix enters a pending state showing `Indexing to line N... NN%` and completes when reached; `C-g` cancels. Cold start: `index_file` uses `fadvise(Sequential)`.

- [ ] **Step 1: Failing tests:** `large_file_opens_without_reading_whole` (1 GiB sparse-ish generated file, first frame before indexing completes; open completes in < 50 ms in debug-independent terms: assert `FileLoaded` before any `IndexProgress{complete:true}`), `modeline_shows_l_unknown_then_number`, `goto_line_pending_then_completes`, `goto_line_cancel`.
- [ ] **Step 2-4:** fail, implement, pass.
- [ ] **Step 5:** Commit `feat: background line indexing and pending goto`.

### Task 22: Column checkpoints, long-line windowing, visual line motion

**Files:** `emx-text/src/checkpoints.rs`, `render.rs`, `command/move.rs`.

**Interfaces:**
- Produces: `pub struct ColumnCheckpoints` (per-line cache keyed by line-start offset, LRU-bounded to 1024 lines): checkpoints every `COLUMN_CHECKPOINT_INTERVAL` bytes at grapheme boundaries storing `(byte_offset, display_column)`; `column_of(text, line_start, pos) -> Result<u64, ReadError>` and `pos_at_column(text, line_start, col) -> Result<u64, ReadError>` reading at most one interval of bytes beyond the nearest checkpoint. Lines >= `LONG_LINE_THRESHOLD` use checkpoints; a window's `start_row_skip` lets it begin mid-line; rendering a 100 MB single-line file reads only the visible window's bytes. `next-line`/`previous-line` move by screen (visual) lines as Emacs `line-move-visual` t, keeping the goal column.

- [ ] **Step 1: Failing tests:** `checkpoints_match_full_scan` (proptest on random long lines with wide chars and tabs), `render_100mb_single_line_reads_bounded_bytes` (instrumented `Sources` counting bytes read <= 2 * window cells * 4 + interval), `visual_line_motion_in_wrapped_line`, `goal_column_preserved`.
- [ ] **Step 2-4:** fail, implement, pass.
- [ ] **Step 5:** Commit `feat: long-line windowing with column checkpoints`.

### Task 23: Tail mode, never-blocking render, save progress

**Files:** `command/misc.rs`, `editor.rs`, `render.rs`, server workers.

**Interfaces:**
- Produces: `auto-revert-tail-mode` (Emacs name; toggles; modeline `Tail`): on `Appended`, every window on the buffer whose point is at EOB scrolls to keep EOB visible. Prefetch: `PrefetchRequest`s from render become `Effect::Prefetch` on the I/O pool; `BlockReady` triggers re-render. Saves of `File`-backed buffers report progress in the modeline (`Saving NN%`).

- [ ] **Step 1: Failing tests:** `tail_mode_follows_appends` (e2e: append lines to a 200 MB file every 50 ms; visible last line updates), `render_never_reads_uncached` (instrumented source panics if `read` (not `read_cached`) is called from `render`), `save_progress_shown`.
- [ ] **Step 2-4:** fail, implement, pass.
- [ ] **Step 5:** Commit `feat: tail mode, prefetching render, save progress`.

### Task 24: Benchmarks, fuzzing, static builds

**Files:** `crates/emx-bench/src/main.rs`, `crates/emx-text/fuzz/fuzz_targets/{piece_tree.rs, kitty_decode.rs, journal_replay.rs, emacs_regex.rs}`, `bench/README.md` (results), Makefile targets.

**Interfaces:**
- Produces: `emx-bench` generating corpora under `bench/corpus/` (10 MB, 1 GB, optional 10 GB logs with realistic line lengths, plus a 100 MB single-line file), driving `emx --no-server` in a PTY and measuring: startup to first frame, open-to-first-paint per corpus, keypress-to-paint (median and p99 of 200 `C-n` presses), scroll (`C-v` x 100), search throughput (`C-s` for a needle at EOF), reload latency (external append to updated frame), RSS from `/proc/<pid>/status`. Cold-cache passes drop the corpus file from the page cache with `fadvise(DontNeed)` (no root needed). Output: markdown table appended to `bench/README.md` with targets from DESIGN 8 and pass/fail per target.
- Fuzz targets on nightly (`rustup toolchain install nightly`, `cargo install cargo-fuzz`), `make fuzz FUZZ_SECONDS=60` runs each.
- `make static`: `rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl`, `cargo install cargo-zigbuild`, zig via `python3 -m pip install --user ziglang`; builds both with `mimalloc` as the global allocator; prints sizes. aarch64 binaries are built but cannot be run here (no aarch64 hardware or qemu); report that.

- [ ] **Step 1:** Implement the bench and fuzz targets; run `make bench` and `make fuzz`; fix every crash found (add a regression test per crash).
- [ ] **Step 2:** `make static`; record binary sizes in `bench/README.md`.
- [ ] **Step 3:** `make check`. Commit `perf: benchmarks, fuzzing, static builds (M4)`. Tag `m4`.

---

## Final Verification (before reviews)

- [ ] `make check` passes (fmt, clippy -D warnings, all unit, property, PTY, server, crash, and differential tests).
- [ ] `make fuzz` ran every target for 60 s with no crash.
- [ ] `make bench` results recorded with pass/fail against DESIGN 8 targets; misses are reported, not hidden.
- [ ] Manual smoke in a real Kitty terminal is not possible headless; the PTY suite stands in for it, and that limitation is reported.
