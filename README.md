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
```

Run `emx --help` or `emx help COMMAND` for details. Key bindings can be overridden in `~/.config/emx/config.toml`.

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
