# emx

A fast terminal editor with Emacs keybindings, built for humans working alongside coding agents: it watches files that agents, build tools, and `git` rewrite, shows what changed, never loses either side's work, and can itself be driven over a socket. It also handles multi-GB files and ships as one static binary, so it is a practical `emacs-nox` replacement on remote and constrained boxes.

## Install

emx runs on Linux. Building it needs Rust 1.88 or later; [rustup](https://rustup.rs) installs one. Distribution `cargo` packages are often older and fail with "this version of Cargo is older than the `2024` edition"; run `rustup update stable`, or use a static binary (below).

From crates.io:

```sh
cargo install --locked emx
```

This puts `emx` in `~/.cargo/bin`. To install into `~/.local/bin` instead, add `--root ~/.local`. Running the same command again upgrades to the newest release.

From a checkout of this repository:

```sh
make install                      # builds and copies emx to ~/.local/bin
make install-sys                  # builds and copies emx to /usr/local/bin (uses sudo)
```

### Other hosts

For a host without a new enough Rust, build static binaries, which run on any Linux of their architecture whatever its libc, and copy the one matching the host's `uname -m`:

```sh
make static
scp target/x86_64-unknown-linux-musl/release/emx  HOST:.local/bin/emx   # x86_64
scp target/aarch64-unknown-linux-musl/release/emx HOST:.local/bin/emx   # aarch64
```

`make deb` packages the same binaries as `.deb` files.

### Upgrading

New terminals attach to the session server that is already running, so a new emx takes effect once the old server exits. It exits by itself 3 seconds after its last terminal closes (`C-x C-c` in each), but stays up while any buffer has unsaved changes. To stop it at once, run `pkill -x emx`, which does not offer to save. Check the version with `emx --version`.

## Getting Started Fast

```sh
emx notes.txt            # open a file
emx src/main.rs:42       # open a file at line 42 (FILE:LINE[:COL])
export EDITOR="emx --wait"   # use emx for git commit messages
```

The first `emx` starts a session server in the background. Every later `emx` attaches to it and shares the same buffers, so a file open in one terminal is open in all of them. Use `emx --no-server FILE` to run a single standalone editor.

Keys follow Emacs: `C-x` means Control+x, `M-x` means Alt+x (or Esc then x).

| Keys | Action |
|---|---|
| `C-x C-f` | Open a file |
| `C-x C-s` | Save |
| `C-x s` | Save all modified buffers |
| `C-x C-c` | Quit (asks about unsaved buffers) |
| `C-z` | Suspend to the shell; `fg` resumes |
| `C-g` | Cancel the current command |
| `C-f` `C-b` `C-n` `C-p` | Move right, left, down, up (arrow keys work too) |
| `C-a` / `C-e` | Start / end of line |
| `M-<` / `M->` | Start / end of buffer |
| `C-v` / `M-v` | Page down / up |
| `M-g g` | Go to line |
| `C-s` / `C-r` | Incremental search forward / backward |
| `M-%` | Query replace |
| `C-SPC` | Set the mark (start a selection) |
| `C-w` / `M-w` / `C-y` | Cut / copy / paste |
| `C-k` | Cut to end of line |
| `DEL` / `Delete` | Delete the character before / under the cursor, or the selection if one is active |
| `C-/` | Undo |
| `C-x b` / `C-x C-b` | Switch buffer / list buffers |
| `C-x 2` / `C-x 3` / `C-x o` / `C-x 1` | Split below / split right / other window / close other windows |
| `F8` | Open or close the file panel |
| `C-x p f` | Find a file in the project |
| `TAB` | Indent the line (in a language file) |
| `M-;` | Add or go to a comment; comment or uncomment the region |
| `M-x` | Run any command by name |

Driving emx from a script or a coding agent:

```sh
emx open FILE:LINE       # show FILE in the most recently active emx terminal
emx goto 120             # move point in the current buffer
emx list-buffers
emx reload               # reread unmodified buffers from disk
emx save-all
emx events               # stream buffer, file and cursor events as JSON lines
emx diff-external FILE   # show how FILE changed on disk against its buffer
git diff FILE | emx apply-patch FILE   # apply a unified diff to FILE's open buffer
```

`emx apply-patch` puts the hunks into the open buffer, on top of any unsaved edits, as one change that `C-/` undoes. If any hunk does not apply, nothing changes and it exits with an error. It never saves.

Run `emx --help` or `emx help COMMAND` for details. Key bindings can be overridden in `~/.config/emx/config.toml`.

### File panel

`F8` opens a file tree on the left, rooted at the project (the nearest directory holding `.git`). Files ignored by git are hidden. The first column shows git status (`M` modified, `A` added, `?` untracked, rolled up to directories), and `*` marks files changed on disk since you last looked at them. The panel updates as files are created, deleted or renamed.

| Key | Action |
|---|---|
| `n` / `p` | Next / previous entry |
| `RET` | Open a file in the main window, or open/close a directory |
| `TAB` | Open/close a directory |
| `^` | Go to the parent directory (on the root line: make the parent the root) |
| `g` | Refresh, including git status |
| `i` | Show/hide ignored files |
| `+` / `R` / `d` | Create a directory / rename / delete (asks first) |
| `q` | Close the panel |

`C-x p f` lists every file in the project and opens the one you pick; type a few letters of the path in order (`smr` finds `src/main.rs`).

### Languages

Rust, C, Go, Python, JavaScript, Bash, JSON, TOML and YAML files are highlighted with tree-sitter, chosen by file name or a `#!` line. In those files `TAB` and `RET` indent by the language's rules and `M-;` comments. The matching paren of the one at point is highlighted in every file.

### Rectangles

`C-x r k` / `C-x r y` / `C-x r t` kill, yank and replace a rectangle; `C-x r M-w` copies it, `C-x r d` deletes it, `C-x r c` blanks it, `C-x r o` opens blank space in it, and `C-x r N` numbers its lines.

### Logs and binary files

`M-x ansi-color-mode` shows a log's ANSI color codes as colors and hides them; the file is unchanged. A file with NUL bytes in its first 8 KiB opens read-only as binary (the modeline shows `=`); `C-x C-q` makes it writable. Bytes that are not UTF-8 are shown as `\ooo` and saved unchanged; emx does not convert Latin-1 or UTF-16 files.

### Clipboard

Text you kill or copy also goes to the system clipboard through the terminal (OSC 52). Inside tmux, set `set -g set-clipboard on`. Turn it off with `osc52_clipboard = false`.

### Colors and per-file settings

```toml
theme = "light"                # or "dark" (the default)

[faces]                        # any face, as SGR parameters
keyword = "1;34"
comment = "2;37"

[file_types.md]                # by extension, or a whole file name
tab_width = 4
indent_tabs_mode = false
require_final_newline = true
delete_trailing_whitespace = true
```

`require_final_newline` and `delete_trailing_whitespace` can also be set for every file. The cleanups happen when you save, and `C-/` undoes them.

### Mouse

Mouse support is off by default, because turning it on takes over the terminal's own drag-to-select. Turn it on with `mouse = true` in the config, or for the session with `M-x xterm-mouse-mode`. Then a click moves the cursor or selects a window, a drag selects text, double and triple clicks select a word and a line, the right button extends the selection, the wheel scrolls the window under the pointer, and dragging a modeline or border resizes windows and the file panel. Hold Shift to use the terminal's own selection. Inside tmux, also `set -g mouse on`.

### Terminals

emx works in Kitty-protocol terminals (kitty, WezTerm, Ghostty, foot, recent Alacritty) and in legacy terminals: xterm, GNOME Terminal and other VTE terminals, Alacritty, the Linux console, tmux and GNU screen. It asks the terminal at startup which it is, and waits up to one second for the answer; a terminal or multiplexer that does not answer makes every start take that second. To skip the question, set `keyboard_encoding = "kitty"` or `"legacy"` in the config, or `EMX_KEYBOARD=kitty` or `EMX_KEYBOARD=legacy` in the environment.

On macOS, emx builds (`make macos`, or `cargo install` on a Mac) and uses the same keyboard handling in Terminal.app and iTerm2; set Option to act as Meta in the terminal's settings, or `M-` keys will not work. macOS is less tested than Linux.

In a legacy terminal, Alt sends an ESC prefix (emx turns this on in xterm), and a lone Escape takes effect after 50 ms. Some Ctrl combinations, such as `C-%`, only reach emx in xterm and in tmux with `extended-keys` on.

## Build and test

All builds and tests go through the Makefile. Run `make` to list targets.

```sh
make build        # release binary at bin/emx
make test         # all workspace tests
make check        # fmt check, clippy, tests
```

## Documentation

- [VISION.md](VISION.md): product vision
- [DESIGN.md](DESIGN.md): technical design

## License

MIT OR Apache-2.0
