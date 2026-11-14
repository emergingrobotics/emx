# Tier-1 Emacs parity contract

This is the parity contract for `emx`: every tier-1 command from
[`DESIGN.md`](../DESIGN.md) section 4 ("Emacs keybindings"), its default
binding, and the differential-test cases that pin its behavior against real
GNU Emacs.

## What "tier 1" means

`DESIGN.md` section 10 ("Testing") defines three tiers. Tier 1 is "the
everyday commands in section 4, which must match Emacs exactly." This file
enumerates that tier-1 set and maps each command to its coverage.

Every row below has **at least three** differential cases. Each case is one
JSON object in a `difftest/cases/*.jsonl` file: `{"name", "text", "keys",
"tier"}`, where `text` marks point with `|` (and an optional mark with `^`)
and `keys` is an Emacs `kbd` sequence. `make difftest` replays every case
through `emx` and through `emacs --batch -Q` (`difftest/emx-difftest.el`,
`execute-kbd-macro`) and compares buffer text, point, mark, mark-active
state, the newest kill-ring entries, the modified flag, and the error that
ended the macro. A tier-1 mismatch fails the suite.

As of this contract: **494 cases, all tier 1, `make difftest` 494/494.**

## Default bindings are authoritative in the keymap

Bindings below are the defaults in `crates/emx-core/src/keymap.rs`
(`DEFAULT_BINDINGS`). Keybindings are data and user-overridable via TOML
(DESIGN 4); this contract tracks the defaults.

## Case files

| File | Scope |
|---|---|
| `cases/m1-basic.jsonl` | Movement, editing, kill/yank, mark, undo fundamentals (M1). |
| `cases/m3-tier1.jsonl` | Sentences, word case, transpose, open/blank lines, rectangles, registers, goto-line, dabbrev, macros, mark ring. |
| `cases/m3-tier1-extra.jsonl` | Undo/undo-redo alternate bindings, `M-g M-g`, `C-x e` no-macro paths. |
| `cases/m3-search.jsonl` | isearch (forward/backward/regexp), query-replace (+regexp), occur. |
| `cases/m3-completion.jsonl` | `M-x`, `yank-pop` minibuffer (`M-y` browse). |
| `cases/m3-windows.jsonl` | Window split/delete/other, buffer switch/kill, buffer list. |
| `cases/m3-window-numbers.jsonl` | Cases whose messages name a window number (kept isolated; see the `.el` harness note). |
| `cases/m3-scroll.jsonl` | `C-v`, `M-v`, `C-l`. |
| `cases/m3-files.jsonl` | `find-file`, `save-buffer`, `write-file` (each case runs in a private temp directory on both sides). |

## Movement

| Binding | Command | Cases | Example case names |
|---|---|---|---|
| `C-f` | `forward-char` | 52 | `m1-basic`: forward-char-basic, forward-char-count, forward-char-negative |
| `C-b` | `backward-char` | 18 | `m1-basic`: backward-char-basic/-count/-bob-error |
| `C-n` | `next-line` | 31 | `m1-basic`: next-line-basic, next-line-goal-column, next-line-count |
| `C-p` | `previous-line` | 10 | `m1-basic`: previous-line-basic/-count/-goal-column |
| `M-f` | `forward-word` | 16 | `m1-basic`: forward-word-basic/-count/-negative |
| `M-b` | `backward-word` | 5 | `m1-basic`: backward-word-basic/-to-bob; `m3-search`: backward-word-count |
| `C-a` | `move-beginning-of-line` | 10 | `m1-basic`: beginning-of-line-basic/-count/-negative |
| `C-e` | `move-end-of-line` | 20 | `m1-basic`: end-of-line-basic/-count/-negative |
| `M-a` | `backward-sentence` | 8 | `m3-tier1`: backward-sentence-end/-mid/-twice |
| `M-e` | `forward-sentence` | 14 | `m3-tier1`: forward-sentence-second/-twice/-count |
| `C-v` | `scroll-up-command` | 3 | `m3-scroll`: scroll-up-small-buffer-errors, scroll-up-count-moves-point, scroll-up-at-eob-errors |
| `M-v` | `scroll-down-command` | 3 | `m3-scroll`: scroll-down-small-buffer-errors, scroll-down-at-eob-errors, scroll-down-count-errors |
| `M-<` | `beginning-of-buffer` | 9 | `m1-basic`: beginning-of-buffer-pushes-mark/-tenths/-mark-ring |
| `M->` | `end-of-buffer` | 16 | `m1-basic`: end-of-buffer-pushes-mark/-tenths |
| `M-g g`, `M-g M-g` | `goto-line` | 10 | `m3-tier1`: goto-line-3/-1/-mg; `m3-tier1-extra`: goto-line-mg-mg-line-3/-1 |
| `C-l` | `recenter-top-bottom` | 3 | `m3-scroll`: recenter-plain, recenter-cycle, recenter-argument |

`C-v`/`M-v` cases keep the whole buffer within the window on both sides, so
the compared outcome is the deterministic beginning/end-of-buffer signal and
point, which do not depend on the exact window height. `C-l` changes only
window-start, so its compared outcome (text, point, mark, kills) is
unchanged; the cases pin that it never errors or moves point.

## Editing

| Binding | Command | Cases | Example case names |
|---|---|---|---|
| `C-d` | `delete-char` | 7 | `m1-basic`: delete-char-basic/-count-kills/-eob-error |
| `M-d` | `kill-word` | 13 | `m3-tier1`: kill-word-basic/-mid/-count |
| `C-k` | `kill-line` | 35 | `m1-basic`: kill-line-basic/-eol/-count |
| `C-w` | `kill-region` | 13 | `m1-basic`: kill-region-basic/-no-mark-error/-empty |
| `M-w` | `kill-ring-save` | 5 | `m1-basic`: kill-ring-save-basic/-deactivates/-then-yank |
| `C-y` | `yank` | 16 | `m1-basic`: yank-twice/-universal-point-first/-empty-ring-error |
| `M-y` | `yank-pop` | 10 | `m1-basic`: yank-pop-cycle/-wraps/-negative |
| `C-/`, `C-_`, `C-x u` | `undo` | 31 | `m1-basic`: undo-c-x-u, undo-c-underscore, undo-count; `m3-tier1-extra`: undo-underscore-twice-errors, undo-cx-u-twice-errors, undo-cx-u-after-redo |
| `C-?`, `C-M-_` | `undo-redo` | 7 | `m1-basic`: undo-redo-c-question, undo-redo-c-m-underscore; `m3-tier1-extra`: undo-redo-question-twice-errors, undo-redo-cm-underscore-single |
| `M-u` | `upcase-word` | 11 | `m3-tier1`: upcase-word-basic/-count/-neg |
| `M-l` | `downcase-word` | 6 | `m3-tier1`: downcase-word-basic/-count/-mid |
| `M-c` | `capitalize-word` | 10 | `m3-tier1`: capitalize-word-basic/-upper/-mixed |
| `C-t` | `transpose-chars` | 9 | `m3-tier1`: transpose-chars-mid/-eol/-bol |
| `M-t` | `transpose-words` | 7 | `m3-tier1`: transpose-words-basic/-mid/-start |
| `C-o` | `open-line` | 18 | `m3-tier1`: open-line-basic/-count/-eol |
| `C-x C-o` | `delete-blank-lines` | 9 | `m3-tier1`: delete-blank-lines-many/-single/-nonblank |
| `M-/` | `dabbrev-expand` | 6 | `m3-tier1`: dabbrev-basic/-prefix/-forward |

## Mark and region

| Binding | Command | Cases | Example case names |
|---|---|---|---|
| `C-SPC` | `set-mark-command` | 26 | `m1-basic`: set-mark-then-move/-toggle-off/-toggle-on |
| `C-u C-SPC` | `set-mark-command` (pop mark ring) | 9 | `m1-basic`: set-mark-pop/-pop-twice; `m3-tier1`: mark-ring-pop, mark-ring-set-pop-twice |
| `C-x C-x` | `exchange-point-and-mark` | 6 | `m1-basic`: exchange-point-and-mark-basic/-twice/-no-mark-error |
| `C-x h` | `mark-whole-buffer` | 6 | `m3-tier1`: mark-whole-buffer/-empty/-start |
| `C-x r k` | `kill-rectangle` | 7 | `m3-tier1`: kill-rectangle-basic/-wide/-one-line |
| `C-x r y` | `yank-rectangle` | 4 | `m3-tier1`: yank-rectangle-basic/-inline/-into-text |
| `C-x r t` | `string-rectangle` | 4 | `m3-tier1`: string-rectangle-basic/-longer/-then-move |
| `C-x r s` | `copy-to-register` | 10 | `m3-tier1`: copy-to-register-noarg/-delete/-reversed |
| `C-x r i` | `insert-register` | 7 | `m3-tier1`: copy-insert-register/-arg, insert-register-twice |
| `C-x r SPC` | `point-to-register` | 3 | `m3-tier1`: point-jump-register, point-jump-after-edit, point-register-two |
| `C-x r j` | `jump-to-register` | 3 | `m3-tier1`: point-jump-register, point-jump-after-edit, point-register-two |

## Search

| Binding | Command | Cases | Example case names |
|---|---|---|---|
| `C-s` | `isearch-forward` | 32 | `m3-search`: isearch-exit-pushes-mark, isearch-repeat, isearch-wraps-after-failing |
| `C-r` | `isearch-backward` | 5 | `m3-search`: isearch-backward-closest-start/-repeat/-then-forward |
| `C-M-s` | `isearch-forward-regexp` | 3 | `m3-search`: isearch-regexp, isearch-regexp-fallback-star, isearch-regexp-alternative |
| `C-M-r` | `isearch-backward-regexp` | 4 | `m3-search`: isearch-regexp-backward, isearch-backward-regexp-digit/-repeat/-word |
| `M-%` | `query-replace` | 23 | `m3-search`: query-replace-bang/-quit-with-q/-dot |
| `C-M-%` | `query-replace-regexp` | 5 | `m3-search`: query-replace-regexp-groups/-whole-and-count/-empty-matches |
| `M-s o` | `occur` | 12 | `m3-search`: occur-buffer-contents/-next/-visit |

Lazy highlighting of non-selected matches (DESIGN 4) is a render concern,
not part of the compared outcome, so it is not pinned by these cases.

## Files, buffers, windows

| Binding | Command | Cases | Example case names |
|---|---|---|---|
| `C-x C-f` | `find-file` | 7 | `m3-files`: find-file-new-is-empty, find-file-new-then-type, find-file-two-files |
| `C-x C-s` | `save-buffer` | 5 | `m3-files`: save-buffer-clears-modified, save-buffer-no-changes, save-buffer-new-unmodified-writes |
| `C-x C-w` | `write-file` | 4 | `m3-files`: write-file-clears-modified, write-file-then-edit, write-file-then-save |
| `C-x b` | `switch-to-buffer` | 14 | `m3-windows`: switch-to-new-buffer, switch-to-buffer-default-returns, switch-to-buffer-by-name |
| `C-x k` | `kill-buffer` | 4 | `m3-windows`: kill-buffer-shows-previous-buffer, kill-buffer-current-default, kill-buffer-by-name |
| `C-x 2` | `split-window-below` | 18 | `m3-windows`: window-points-independent, split-below-new-window-too-small |
| `C-x 3` | `split-window-right` | 6 | `m3-windows`: other-window-negative, split-right-new-window-too-small |
| `C-x 0` | `delete-window` | 3 | `m3-windows`: delete-window-selects-most-recent, delete-window-sole-error, delete-window-other-keeps-edit |
| `C-x 1` | `delete-other-windows` | 3 | `m3-windows`: delete-other-windows-keeps-selected, delete-other-windows-after-splits, delete-other-windows-sole-noop |
| `C-x o` | `other-window` | 32 | `m3-windows`: other-window-count/-count-wraps/-sole |

Each `m3-files` case runs in a private, empty temporary directory, created
per case on both the `emx` side (`run_emx` in `crates/emx-core/tests/difftest.rs`)
and the Emacs side (`emx-difftest--run` in `difftest/emx-difftest.el`), so a
file command neither reads nor overwrites another case's files nor the
repository, and both editors start each case from an identical empty
directory. The compared outcome is buffer state (text, point, mark, kills,
modified, error), not the bytes on disk. `save-buffer` is exercised only on
file-visiting buffers (opened with `find-file` first): in batch Emacs,
`save-buffer` on a buffer with no file does not read the `File to save in:`
minibuffer prompt it reads interactively, so that path is a batch-vs-
interactive divergence and is not driven here (interactive emx behavior is
covered by unit tests in `crates/emx-core/tests/`).

## Minibuffer completion

Vertical incremental completion (DESIGN 4) is the input method for
`find-file`, `switch-to-buffer`, and `execute-extended-command`. It is
pinned through those commands' cases (`m3-files`, `m3-windows`,
`m3-completion`) and, for its own machinery (flex matching, history,
candidate navigation), by the unit tests in
`crates/emx-core/tests/completion.rs`. The differential harness compares
buffer state, which does not include the transient candidate list.

## Universal argument, macros, quit

| Binding | Command | Cases | Example case names |
|---|---|---|---|
| `C-u` | `universal-argument` | 73 | `m1-basic`: forward-char-count/-universal; `m3-scroll`: scroll-up-count-moves-point |
| `C-x (` | `kmacro-start-macro` | 4 | `m3-tier1`: macro-define-only, macro-define-move, macro-abort-define |
| `C-x )` | `kmacro-end-macro` | 3 | `m3-tier1`: macro-define-only, macro-define-move, macro-end-not-defining |
| `C-x e` | `kmacro-end-and-call-macro` | 3 | `m3-tier1`: macro-call-no-macro; `m3-tier1-extra`: macro-call-no-macro-count, macro-call-no-macro-twice |
| `M-x` | `execute-extended-command` | 5 | `m3-completion`: execute-extended-command-prefix-argument/-kill-line/-forward-word |
| `C-g` | `keyboard-quit` | 10 | `m1-basic`: keyboard-quit-deactivates; `m3-tier1`: keyboard-quit-mark/-prefix/-plain |

**Keyboard-macro replay is a documented batch-vs-interactive divergence.**
Inside `emacs --batch -Q`, `execute-kbd-macro` does not record a nested
macro, so `C-x ( ... C-x )` followed by `C-x e` reports "No kbd macro has
been defined" rather than replaying. The differential cases therefore
exercise macro *definition* (`C-x (`, `C-x )`) and the *no-macro* error path
of `C-x e`; actual replay is covered by unit tests in
`crates/emx-core/tests/macros.rs`. This matches the divergence handling
established in earlier milestones (macro replay, isearch-occur).
