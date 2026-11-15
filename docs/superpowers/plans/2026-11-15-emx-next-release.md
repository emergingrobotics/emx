# emx Next Release (M5-M7) Design and Implementation Plan

**Goal:** Build DESIGN.md milestones M5-M7: Linux terminal coverage (legacy key decoding for GNOME Terminal, Alacritty, xterm, the Linux console, tmux and screen), the file panel with editor-wide opt-in mouse support, and tree-sitter syntax highlighting with bracket matching, `TAB` indentation and `M-;` comments.

**Spec:** `VISION.md` and `DESIGN.md` (as revised 2026-11-15). Where this plan and the spec disagree, the spec wins. The first-release plan's Global Constraints (`docs/superpowers/plans/2026-11-13-emx-first-release.md`) still apply in full: Makefile-only builds, `#![forbid(unsafe_code)]`, `thiserror` in libraries, no `unwrap` on external input, strict config, named defaults, `make check` green at every commit, no skipped tests.

**Out of scope here** (VISION.md Non-Goals): sexp movement, fill, `C-h` help, running programs, version control commands, LSP, three-way merge, remote files, Konsole, macOS terminals (M8).

## Protocol

New messages and faces change the wire format, so `PROTOCOL_VERSION` goes from 1 to 2 with the first of them (M6). A client and server of different versions refuse each other with the existing version error; a user who upgrades restarts the server.

- `ClientMsg::Mouse { event: MouseEvent }`.
- `ServerMsg::Modes { mouse: bool }`: sent after `Welcome` to terminal clients and whenever mouse reporting is toggled. The client turns terminal mouse reporting on or off to match.
- New `Face` variants for the panel, syntax highlighting and paren matching.

## M5: Linux Terminal Coverage

### Decoder

`emx-term`'s `KittyDecoder` becomes `Decoder`, built with a `Protocol`:

- `Protocol::Kitty`: today's behavior, unchanged. Escape is `CSI 27 u`, so a lone ESC byte always starts a sequence and no timeout is needed.
- `Protocol::Legacy`: the encodings legacy terminals send.
  - **Meta is an ESC prefix.** ESC followed by a byte that does not open a sequence is delivered as the `ESC` key, and the following byte decodes on its own; the keymap already folds `ESC x` into `M-x` (Emacs `esc-map`). `ESC [` and `ESC O` open sequences as before.
  - **A lone or unfinished ESC waits for a timeout.** Bytes that may still become a sequence are held. `Decoder::timeout()` flushes them: the leading ESC becomes the `ESC` key and the rest decodes as plain input (so `ESC [` typed by hand is `M-[`, as in Emacs). The client calls it after `LEGACY_ESCAPE_TIMEOUT_MS` (50 ms) without input while the decoder holds an ESC.
  - **xterm `modifyOtherKeys`:** `CSI 27 ; mods ; code ~` decodes to the modified key. Level 1 is requested on entry, so xterm reports keys such as `C-%` and `C-M-%` that have no legacy byte.
  - **Linux console function keys:** `ESC [ [ A` through `ESC [ [ E` are F1-F5.
  - C0 bytes are Ctrl keys as today (0x08 is `C-h`, 0x00 is `C-@`, 0x1f is `C-_`).
  - Bracketed paste, `CSI`/`SS3` cursor and function keys with xterm modifiers, `CSI Z` and `CSI code ; mods u` decode in both modes.
- **SGR mouse reports** (`CSI < b ; x ; y M|m`) decode in both modes to `InputEvent::Mouse` (used from M6).

### Terminal setup

Raw mode, the alternate screen and bracketed paste are set up first; the probe then decides the protocol, and only then are protocol-specific modes set:

| Protocol | On entry | On exit |
|---|---|---|
| Kitty | push flags 5 (`CSI > 5 u`) | pop (`CSI < u`) |
| Legacy | `modifyOtherKeys` level 1 (`CSI > 4 ; 1 m`); save and set xterm `metaSendsEscape` (`CSI ? 1036 s`, `CSI ? 1036 h`) | reset `modifyOtherKeys` (`CSI > 4 m`); restore 1036 (`CSI ? 1036 r`) |

Terminals ignore modes they do not know. The restore sequence also clears the screen before leaving the alternate screen, so the Linux console, which has no alternate screen, is not left showing the editor. The protocol in use is stored next to the existing `ACTIVE` flag so the panic hook restores the right modes.

### Probe

`probe_kitty` keeps its queries (`CSI ? u`, then primary device attributes). A flags reply before the attributes reply selects Kitty; anything else, including a terminal that answers neither, selects Legacy. The "unsupported terminal" exit is removed. `EMX_KEYBOARD=kitty|legacy` skips the probe (for terminals whose answer is wrong, or a Kitty terminal whose multiplexer forwards the protocol).

### Tests

- Decoder fixture tables in `crates/emx-term/tests/legacy.rs`: the bytes xterm, GNOME Terminal (VTE), Alacritty, the Linux console, tmux and screen send for each tier-1 key, from their documented encodings, decoded to the expected `Key`s. Property test: every byte is accounted for in Legacy mode as in Kitty mode; `timeout()` never loses bytes.
- PTY tests with a harness mode that answers only device attributes and encodes keys the legacy way (ESC-prefixed Meta, C0 controls).
- End-to-end tests that run `emx` inside real `tmux` and `screen` sessions (`send-keys`, `capture-pane` / `hardcopy`), typing, moving, saving and quitting. `tmux` and `screen` are required to run the test suite, like Emacs is for the differential tests.

## M6: File Panel and Mouse

### Face spans

A buffer can carry **face spans**: sorted, non-overlapping byte ranges with a `Face`, kept in step with edits the way markers are (an insertion inside a span extends it; a deletion shrinks or removes spans). Rendering looks a cell's face up in the spans after the search, region and escape faces. The panel uses spans for its markers in M6; syntax highlighting fills them in M7.

### The panel is a side window over a hidden buffer

The panel reuses windows and buffers instead of drawing a separate widget, so movement, scrolling, isearch, `C-x o` and rendering work in it unchanged:

- Each terminal client has at most one panel: a `Window` in its frame that is not part of the window tree, laid out in the leftmost `panel_width` columns, with a vertical border to its right. The window tree fills the remaining columns.
- The window shows a hidden, read-only buffer (`" *tree F<n>*"`; a leading space hides it from `C-x b` and the buffer list, as in Emacs), owned by the client and killed with the panel.
- `F8` and `M-x toggle-tree` open and close it. Opening selects it. `C-x o` cycles through it first. Closing it selects the most recently used other window.
- Width: `panel_width` (default 30 columns) in the config; dragging the border with the mouse changes it, and the last width is reused by the next panel opened in the session. The panel needs `PANEL_MIN_TEXT_COLS` (10) and leaves the windows `WINDOW_MIN_WIDTH`; a terminal too narrow for both refuses to open it with an error.

### Panel contents

- **Root:** the nearest ancestor of the client's working directory that contains `.git`, else the working directory.
- **Lines:** one per entry, indented two columns per level: a status column, then `▾ name/` or `▸ name/` for directories, `  name` for files. Directories sort before files, each alphabetically (byte order).
- **Status column:** `M` modified, `A` added, `?` untracked, `!` ignored (shown only when ignored entries are shown), `*` changed on disk since last viewed (DESIGN section 1), blank otherwise. A directory shows the strongest mark of anything beneath it.
- **Lazy listing:** only expanded directories are listed, by a worker (`Effect::ListTree` / `Event::TreeListed`), using the `ignore` crate so `.gitignore`, `.ignore` and git's excludes apply. Ignored entries are hidden until `i` (`tree-toggle-ignored`) shows them.
- **Git status:** a worker computes it with `gix` (no `git` binary) for the repository containing the root (`Effect::GitStatus` / `Event::GitStatusReady`), when the panel opens, on `g`, after a save, and after external changes settle.
- **Live updates:** expanded directories are watched (non-recursive) on local filesystems and listed again after a change settles; on network filesystems they are listed again when the panel is selected or on `g`.

### Panel keys (`tree-mode-map`)

| Key | Command | Action |
|---|---|---|
| `n` / `p`, `C-n` / `C-p` | `next-line` / `previous-line` | move |
| `RET` | `tree-open` | file: show it in the most recently used other window and select that window; directory: expand or collapse |
| `TAB` | `tree-toggle` | expand or collapse a directory |
| `^` | `tree-up` | move to the parent directory's line, or make the root's parent the root |
| `g` | `tree-refresh` | list again and recompute git status |
| `i` | `tree-toggle-ignored` | show or hide ignored entries |
| `+` | `tree-create-directory` | prompt for a name, create it |
| `R` | `tree-rename` | prompt for a new name, rename (a buffer visiting the file follows it) |
| `d` | `tree-delete` | ask `Delete NAME? (yes or no)`, delete a file or an empty directory |
| `q` | `toggle-tree` | close the panel |

File operations run on a worker and report errors in the echo area. Non-empty directories are not deleted.

### Quick-open

`C-x p f` (`project-find-file`, as in Emacs) lists every file under the panel root (gitignore applied, capped at `QUICK_OPEN_MAX_FILES`, 100 000) on a worker, then reads one with the vertical minibuffer completion, matching flexibly (the typed characters in order). `C-x C-f` is unchanged.

### Mouse

Mouse reporting is opt-in: `mouse = true` in the config, or `M-x xterm-mouse-mode` to toggle it for the session (all clients). When on, the client enables button-event tracking with SGR reports (`CSI ? 1002 h`, `CSI ? 1006 h`); the restore sequence always turns them off. Shift-drag keeps the terminal's own selection in xterm, VTE terminals and Alacritty; tmux needs `set -g mouse on` to forward reports (documented).

The client counts clicks (`DOUBLE_CLICK_MS` 500, same cell) and sends `MouseEvent { kind, button, col, row, mods, clicks }`; the core stays free of wall-clock time. Following Emacs `mouse.el`:

| Input | Where | Action |
|---|---|---|
| left press | window text | select the window, move point there, deactivate the mark; a drag that follows sets the mark at the press and moves point with the pointer (`mouse-drag-region`) |
| left double / triple click | window text | select the word / line |
| right press | window text | set the region from point to there (`mouse-save-then-kill` without the kill) |
| wheel | any window | scroll that window `MOUSE_WHEEL_LINES` (5) lines without selecting it; point stays unless it would leave the window |
| left press | modeline | select the window; a drag moves the boundary below it |
| left drag | vertical border | move the boundary between side-by-side windows |
| left drag | panel border | resize the panel |
| left press | panel line | move to the line; a file opens as `RET`; a directory toggles |
| anything | echo area, other buttons | ignored |

Drags clamp to window minimum sizes. While the minibuffer is active, clicks outside it are ignored (as an Emacs `-nw` minibuffer keeps focus).

## M7: Syntax Highlighting

### Languages

Bundled grammars (each a `tree-sitter-<lang>` crate with its own highlight query): Rust, C, Go, Python, JavaScript, Bash, JSON, TOML, YAML. The language is chosen from the file name's extension, then from a `#!` line (`bash`, `sh`, `python`, `node`) for files without one. The modeline shows the mode name (`Rust`, `Python`, ...) instead of `Fundamental`. Buffers in no bundled language behave exactly as before, so the Emacs differential tests are unaffected.

### Highlighting

- A worker owns one parser and syntax tree per highlighted buffer. After edits the editor sends a snapshot of the text with the edits since the last parse (`Effect::Highlight`); the worker re-parses incrementally and runs the language's highlight query over the visible range plus `HIGHLIGHT_MARGIN_BYTES` (64 KiB) on each side, returning face spans (`Event::Highlighted`).
- Results carry the buffer's change count; stale results are dropped. Between results, existing spans shift with edits, so typing does not flicker.
- Limits from DESIGN section 5: no highlighting above `highlight_max_file` (50 MB) or for lines over `highlight_max_line` (10 KB); both configurable.
- Captures map to faces modeled on Emacs font-lock: `Comment`, `String`, `Keyword`, `Type`, `Function`, `Variable`, `Constant`, `Builtin`, `Preprocessor`, with the 16 ANSI colors.

### Paren matching

`show-paren-mode` (on by default, as in Emacs 28+): when point is before an opening or after a closing `()`, `[]` or `{}`, both are drawn in `ShowParenMatch`, or the one at point in `ShowParenMismatch` when the other does not match. The scan reads only cached text, skips comments and strings using the face spans, and stops after `SHOW_PAREN_MAX_DISTANCE` (100 KiB).

### Indentation and comments

Per-language rules (no Lisp, no tree queries):

- **Indentation** (`TAB` = `indent-for-tab-command`, and `RET` via electric indentation in language buffers): the previous non-blank line's indentation, plus one level when it ends with an opener (`{`, `[`, `(`, or `:` in Python and YAML), minus one level when the current line starts with a closer. Width per language: Rust, C and Python 4; JavaScript, Bash, JSON, YAML and TOML 2; Go indents with a tab. `indent_tabs` follows the language (Go true, others false).
- **Comments** (`M-;` = `comment-dwim`): with an active region, comment the lines, or uncomment them if every non-blank line is commented. Without a region, move to the line's comment if it has one, else add one at `comment-column` (32), or one space after the code if it reaches past that. Comment syntax per language: `//` (Rust, C, Go, JavaScript), `#` (Python, Bash, TOML, YAML); JSON has none and reports an error.

## Defaults

| Constant | Value |
|---|---|
| `LEGACY_ESCAPE_TIMEOUT_MS` | 50 |
| `PANEL_WIDTH` | 30 |
| `PANEL_MIN_TEXT_COLS` | 10 |
| `QUICK_OPEN_MAX_FILES` | 100 000 |
| `DOUBLE_CLICK_MS` | 500 |
| `MOUSE_WHEEL_LINES` | 5 |
| `HIGHLIGHT_MARGIN_BYTES` | 64 KiB |
| `HIGHLIGHT_MAX_FILE` | 50 MB |
| `HIGHLIGHT_MAX_LINE` | 10 KB |
| `SHOW_PAREN_MAX_DISTANCE` | 100 KiB |
| `COMMENT_COLUMN` | 32 |

## Review Focus

1. **ESC in legacy mode:** a lone Escape, `M-x` as `ESC x`, `ESC ESC ESC`, and sequences split across reads must decode the same whatever the read boundaries; the timeout must never drop or reorder bytes.
2. **Panel with splits and resizes:** opening and closing the panel, resizing the terminal below the minimum, and deleting windows must keep every window within its minimum size, never panic, and keep `C-x o` order stable.
3. **Mouse coordinates:** clicks on borders, modelines, the echo area, the panel and wide characters must land where they are drawn; a click past the end of a line goes to the end of that line.
4. **Highlight staleness:** an edit between a highlight request and its result must never apply spans to the wrong text.
5. **Language buffers keep Emacs behavior for everything else:** only `TAB`, `RET`'s indentation and `M-;` change.

## Tasks

M5 (one commit each, `make check` green):

1. `Decoder` with `Protocol`; legacy ESC-prefix handling and `timeout()`; `modifyOtherKeys`, console F-keys; SGR mouse decoding; property and fixture tests.
2. Terminal setup split around the probe; protocol-aware restore; `EMX_KEYBOARD`; client escape timeout; the unsupported-terminal exit removed; PTY tests in legacy mode.
3. tmux and screen end-to-end tests; README and DESIGN terminal notes.

M6:

4. Face spans in buffers and rendering.
5. Protocol 2: mouse messages, `Modes`, client mouse modes and click counting.
6. Core mouse handling in windows: point, drag region, word/line selection, wheel, modeline and border drags.
7. Panel layout in the frame; toggle; `C-x o`; width and border drag.
8. Tree model, listing worker with `ignore`, panel keys, file operations, live updates.
9. Git status with `gix`; quick-open.

M7:

10. Tree-sitter worker, language detection, highlight spans, mode names.
11. Paren matching.
12. Indentation and `RET` electric indentation; `comment-dwim`.
13. Docs: README, DESIGN milestone status.
