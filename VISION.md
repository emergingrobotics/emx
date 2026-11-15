# Vision: An Emacs-Keybinding Terminal Editor for Working Alongside Coding Agents

Name: **`emx`**. License: dual MIT/Apache-2.0. Personal open-source project.

This document is the product vision. The technical design (data structures, server, protocol, architecture, testing, milestones) is in [DESIGN.md](DESIGN.md).

## One-liner

A fast terminal editor with Emacs keybindings, built for humans working alongside coding agents: it watches files that agents, build tools, and `git` rewrite, shows what changed, never loses either side's work, and can itself be driven over a socket. It also handles multi-GB files and ships as one static binary, so it is a practical `emacs-nox` replacement on remote and constrained boxes.

## Problem

### Editing next to agents

Coding agents and build tools rewrite files in bursts, often several files at once, while a human has the same files open. Today's editors treat that as an edge case:

- Changes land silently or behind a blocking "file changed on disk" prompt. There is no quick way to see *what* the agent changed, accept it, or roll it back.
- A buffer the human has modified and a file the agent has rewritten collide. The usual outcome is that one side's work is lost.
- Writers that rewrite a file in place (no temp-file-plus-rename) are seen half-written, and the editor reloads garbage.
- Agents and scripts have no simple, stable way to tell the editor "open this at line N", "reload these", or "tell me what the user is looking at".

### Emacs in the terminal

`emacs-nox` is the default terminal editor for many Emacs users, but:

- Startup with a real config is slow relative to `vi`/`nano`. `emacs -Q` starts quickly but gives up the config; `emacs --daemon` plus `emacsclient -t` starts instantly but keeps a full Emacs resident, which is heavy on small boxes.
- Large files (logs, dumps, minified JSON) bog it down. The `vlf` package exists but is chunked and awkward to use.
- `auto-revert-mode` is opt-in. It uses inotify where available (`auto-revert-use-notify`, Emacs 24.4+), but falls back to polling on network filesystems, and conflict handling for a modified buffer is a blocking prompt.
- No built-in project/file tree without extra packages and configuration.
- Its power comes from Elisp, which most of these use cases don't need but pay for in startup time and memory.

### Prior art

- **Modal editors** (Helix, Neovim, Kakoune) are fast but force a different muscle memory.
- **Small Emacs-style editors** (mg, zile, jed) are fast but lack large-file handling, a file tree, live reload, and modern conveniences.
- **`micro`** is the main fast, non-modal modern TUI editor, but does not use Emacs keybindings.
- **Rust Emacs-alikes** (`zee`, `remacs`, `emacs-ng`) and **`lem`** (Common Lisp): `zee` is the closest in spirit and was abandoned; `remacs` and `emacs-ng` aimed to port Emacs itself. Their histories are a warning about scope (see Risks).

## Vision

An agent rewrites six files while you have three of them open. Each buffer updates within a moment of the writes settling, the modeline and tree panel mark what changed, `emx diff-external` shows the hunks, and `C-/` takes any of it back. Nothing you had typed is lost. The agent itself can call `emx open src/main.rs:42` to show you where it is working.

The same binary, copied to a server or SBC, opens a 2 GB log over SSH instantly, navigates with `C-s`, `C-v`, `M-<`, `C-x C-f`, and follows the log as it grows. No config required, no scripting language, one static binary.

## Target Users

1. **Primary: developers working alongside coding agents.** On a workstation, with agents, build tools, and `git` rewriting files underneath them. They need to see, review, accept, and undo external changes without losing their own edits, and want the editor scriptable from the agent side.
2. **Secondary: Emacs users on remote and constrained boxes.** Servers, containers, Raspberry Pi / Orange Pi, and embedded Linux, where `emacs-nox` is too heavy or unavailable. They edit logs, configs, YAML/JSON, and source over SSH.

Platform: Linux-first (Ubuntu/Debian), macOS secondary. Windows is out of scope.

## Goals

Goals are ranked. When two conflict, the higher one wins.

1. **Never lose work.** Neither the user's edits nor an agent's writes are ever silently discarded. Every reload is undoable; every overwrite of newer disk content is a deliberate, warned action.
2. **Always responsive.** The UI thread never blocks: no input lag at any file size, rendering never waits on I/O, multi-GB files open instantly and scroll smoothly. Where exact Emacs behavior would block (for example, highlighting every match in a 2 GB file on each keystroke), `emx` deviates from Emacs.
3. **Emacs-native feel.** Keybindings, kill ring, mark ring, minibuffer, incremental search, universal argument, and undo behave as Emacs users expect, for the commands in the tier-1 list (DESIGN.md, Testing).
4. **Live disk sync.** Buffers reflect external changes automatically, including bursts of agent writes and logs that are appended to.
5. **Drivable from outside.** A stable, versioned socket API lets agents, scripts, and other TUIs open files, trigger reloads, and subscribe to events.
6. **File panel, zero-config, single static binary.** A toggleable tree panel with git status; sensible defaults with optional TOML config; no runtime dependencies, easy to `scp` anywhere.

## Non-Goals

- No Lisp, no embedded scripting language, no package ecosystem. Extensibility is TOML config plus the socket API.
- No Org-mode, Magit, Gnus, or other Emacs applications.
- Not a GUI editor and not an IDE: an editor only. Tree-sitter is used for highlighting, indentation, and comments; there is no LSP client, completion popup, diagnostics, or go-to-definition.
- No running programs: no `compile`, `grep`, `shell`, `eshell`, `term`, `gdb`, or `shell-command` (`M-!`).
- No version control: no `vc` (`C-x v`), `ediff`, or `smerge`, and no three-way merge of on-disk conflicts (the conflict UX is diff / reload / keep mine). The file panel's read-only git status markers stay.
- No remote file editing (Emacs's TRAMP). `emx` opens files through the local filesystem only; to edit on another machine, SSH to it and run `emx` there.
- No built-in help system: no `C-h` prefix, `describe-key`, `describe-function`, tutorial, or Info.
- No sexp movement (`C-M-f/b`) or filling (`M-q`).
- Not planned among Emacs's everyday commands: `zap-to-char` (`M-z`), `just-one-space` and `delete-horizontal-space` (`M-SPC`, `M-\`), `transpose-lines`, `upcase-region` and `downcase-region`, `indent-region`, `replace-string`, narrowing (`C-x n n`), `repeat` (`C-x z`), `find-alternate-file`, `insert-file`, bookmarks, `count-words`, `what-line`, and the `C-x 4` and `C-x 5` prefixes.
- No Vi/modal mode (may be revisited, but not a goal).
- No full Emacs-compatibility guarantee. Compatibility is defined by a tiered command list and measured against real Emacs (DESIGN.md, Testing).
- No buffer editing over the socket in v1. Agents change files on disk; a patch API comes later (DESIGN.md, section 2). A general edit API (ranges, cursor control, transactions) is not planned.
- No bidirectional text layout. RTL text is stored, displayed in logical order, and round-tripped correctly, but not visually reordered. Terminals handle bidi inconsistently, and editor-side reordering can double-apply.
- Windows support.

## Capabilities

What users get. Each item links to its design.

1. **Agent-aware external edits.** Buffers update once an agent's writes settle; changed files are marked; every reload is diffable and undoable; nothing the user typed is lost. ([DESIGN.md section 1](DESIGN.md#1-external-edits-and-agent-workflows))
2. **Persistent sessions and a socket API.** Sessions survive SSH drops and daemon crashes; several terminals can share one session; agents and scripts drive the editor with `emx open`, `emx reload`, `emx events`. ([section 2](DESIGN.md#2-session-server-and-external-control))
3. **Live reload from disk**, including logs that grow, atomic saves, truncation, and network filesystems. ([section 3](DESIGN.md#3-live-reload-from-disk))
4. **Emacs keybindings** for everyday editing, with vertical minibuffer completion and Emacs's undo model. ([section 4](DESIGN.md#4-emacs-keybindings))
5. **Large files are normal.** Multi-GB files open instantly, scroll smoothly, and search without loading into RAM. ([section 5](DESIGN.md#5-large-file-support))
6. **Side-panel file tree** in the terminal, with git status and change markers, and opt-in mouse support across the editor. ([section 6](DESIGN.md#6-file-viewer-panel))
7. **Correct, safe editing fundamentals:** Unicode, byte-exact round-trips of any file, escaped control characters, crash-safe saves, OSC 52 clipboard. ([section 7](DESIGN.md#7-editing-fundamentals))

## Success Criteria

- A developer can keep files open while an agent rewrites them, see every change, and undo any reload, with zero lost edits in the fuzzing suite and in daily use.
- Agents and scripts can open files, trigger reloads, and follow events through the socket API without knowing editor internals.
- An Emacs user can switch from `emacs-nox` and stay productive on the tier-1 command list without relearning, with the differential test pass rate as the measure.
- Opening and navigating a multi-GB log meets the first-paint and keypress-to-paint targets, including cold-cache runs on aarch64.
- Runs well on a Raspberry Pi or Orange Pi over a slow SSH link, from a supported terminal (Kitty-protocol terminals in the first release; tmux and screen in a later one).
- A daemon crash, killed mid-edit, restores every buffer with no lost edits.
- Ships as one static binary for x86_64 and aarch64, within the binary size budget.

## Releases

- **First release (milestones M1-M4):** core and session server, agent-aware live reload, Emacs tier-1 parity, and large-file support, on Kitty-protocol terminals. Dogfooding starts after M2.
- **Next release (M5-M7):** Linux terminal coverage (GNOME Terminal, Alacritty, xterm, the Linux console, tmux, screen), the file panel with mouse support, and tree-sitter syntax highlighting.
- **After that:** polish (including macOS terminals), then the agent patch API.

Full milestone contents are in [DESIGN.md section 12](DESIGN.md#12-milestones).

## Decisions

Resolved from the original risks list (2026-11-13).

| Topic | Decision |
|---|---|
| Primary user | Developers working alongside coding agents; remote/constrained-box Emacs users are secondary. |
| Goal ranking | Never lose work > always responsive > Emacs fidelity > live sync > drivable from outside > panel/config/binary. |
| Process model | Client/server from M1; `--no-server` is first-class. |
| Daemon lifetime | Exits when the last client detaches and no buffer has unsaved edits, after a grace period. |
| Crash durability | Per-buffer edit journal with batched fsync; a daemon crash loses nothing. |
| Edit API | Disk-only in v1; `emx apply-patch` in M9; no general edit API. |
| Minibuffer completion | Vertical incremental (fido-vertical / vertico style). |
| Bidirectional text | Out of scope; logical-order display, correct round-trip. |
| Terminals | Pluggable input decoding. First release: Kitty-protocol terminals only. Legacy Linux terminals and tmux/screen in M5; macOS in M8. |
| Network filesystems | Detect with `statfs`; poll open buffers only (2 s default); tree refreshes on focus. |
| Highlighting bounds | Off-thread, per-frame budget (~4 ms) plus file-size (50 MB) and line-length (10 KB) cutoffs. |
| First release | M1 through M4. |
| Name and license | `emx`; dual MIT/Apache-2.0; personal open-source project. |

## Risks and Open Questions

Product-level risks. Technical risks are in [DESIGN.md section 13](DESIGN.md#13-technical-risks).

- **Scope.** This is an Emacs-compatible editor, a large-file viewer, and an editor session server in one. `zee` and similar projects stalled on scope. Mitigation: the goal ranking, tiered command list, and agent-first milestone order decide what gets cut. The M1-M4 first release is large; watch it.
- **Kitty-only first release excludes tmux and screen.** Many SSH workflows run inside a multiplexer, where the Kitty protocol usually does not pass through. Secondary (remote-box) users in tmux are unsupported until M5, the first milestone of the next release.
- **Name availability.** Checked 2026-11-13: `emx` is unclaimed on crates.io, in Debian and Ubuntu packages, and in Homebrew (formula and cask); no editor named `emx` exists on GitHub. "EMX" also names the dead OS/2 GCC runtime, so search results are noisy. Reserve the crate name early.
