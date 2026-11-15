# emx

A fast terminal editor with Emacs keybindings, built for humans working alongside coding agents: it watches files that agents, build tools, and `git` rewrite, shows what changed, never loses either side's work, and can itself be driven over a socket. It also handles multi-GB files and ships as one static binary, so it is a practical `emacs-nox` replacement on remote and constrained boxes.

## Install

emx runs on Linux. You need a Rust toolchain recent enough for edition 2024 (1.85 or later); [rustup](https://rustup.rs) installs one.

From crates.io:

```sh
cargo install emx
```

This puts `emx` in `~/.cargo/bin`. To install into `~/.local/bin` instead, add `--root ~/.local`.

From a checkout of this repository:

```sh
make install                      # builds and copies emx to ~/.local/bin
make install-sys                  # builds and copies emx to /usr/local/bin (uses sudo)
```

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

### Mouse

Mouse support is off by default, because turning it on takes over the terminal's own drag-to-select. Turn it on with `mouse = true` in the config, or for the session with `M-x xterm-mouse-mode`. Then a click moves the cursor or selects a window, a drag selects text, double and triple clicks select a word and a line, the right button extends the selection, the wheel scrolls the window under the pointer, and dragging a modeline or border resizes windows and the file panel. Hold Shift to use the terminal's own selection. Inside tmux, also `set -g mouse on`.

### Terminals

emx works in Kitty-protocol terminals (kitty, WezTerm, Ghostty, foot, recent Alacritty) and in legacy terminals: xterm, GNOME Terminal and other VTE terminals, Alacritty, the Linux console, tmux and GNU screen. It asks the terminal at startup which it is. To skip the question, set `EMX_KEYBOARD=kitty` or `EMX_KEYBOARD=legacy`.

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
