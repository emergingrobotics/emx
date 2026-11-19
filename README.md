# emx

A fast terminal editor with Emacs keybindings, built for humans working alongside coding agents: it watches files that agents, build tools, and `git` rewrite, shows what changed, never loses either side's work, and can itself be driven over a socket. It also handles multi-GB files and ships as one static binary, so it is a practical `emacs-nox` replacement on remote and constrained boxes.

## Install

emx runs on Linux, on x86_64 and aarch64. Install it with Homebrew.

### Homebrew

```sh
brew tap gherlein/tap
brew trust gherlein/tap
brew install emx
```

Homebrew refuses to load formulae from a tap it does not trust, so the `brew trust` step is required. `brew upgrade emx` picks up new releases. The formula installs a prebuilt static binary that needs nothing else: no Rust toolchain, no libraries.

Prebuilt `.tar.gz` archives holding the bare binary are also on the [Releases](https://github.com/emergingrobotics/emx/releases) page.

### Cargo, if you already work in Rust

If you have a Rust toolchain and want to build emx yourself (to follow `main` between releases, or on macOS, which has no prebuilt binary yet), use cargo. It needs Rust 1.88 or later; distribution `cargo` packages are often older and fail with "this version of Cargo is older than the `2024` edition", so use [rustup](https://rustup.rs) and `rustup update stable`.

```sh
cargo install --locked emx                                                  # newest release on crates.io
cargo install --locked --git https://github.com/emergingrobotics/emx emx   # latest code on main
```

Either puts `emx` in `~/.cargo/bin`; add `--root ~/.local` to install into `~/.local/bin` instead. Running the same command again upgrades. If you also have the Homebrew copy, check which one runs with `type -a emx`.

### Debian packages

`make deb` builds `.deb` packages (`amd64` and `arm64`) of the static binary into `dist/`, which install with `sudo apt install ./dist/emx_X.Y.Z_linux_amd64.deb` and put `emx` in `/usr/bin`. We hope to publish Debian packages as a supported install route later.

### From a checkout

```sh
make install                      # builds and copies emx to /usr/local/bin (asks for sudo)
make install-user                 # builds and copies emx to ~/.local/bin (no sudo)
make install PREFIX=/opt/emx      # any other prefix; sudo only if PREFIX/bin is not writable
make uninstall                    # removes it again (pass the same PREFIX used to install)
```

`make install-sys` is an alias for `make install`. Only the copy runs under sudo, so do not run `sudo make install`: it would build as root and leave root-owned files in `target/`. A system-wide install puts `emx` on the path `sudo` searches, so `sudo emx` works. If an older `~/.local/bin/emx` is still there it shadows the system copy for you; remove it and check with `type -a emx`.

### Other hosts

For a host without a new enough Rust, build static binaries, which run on any Linux of their architecture whatever its libc, and copy the one matching the host's `uname -m`:

```sh
make static
scp target/x86_64-unknown-linux-musl/release/emx  HOST:.local/bin/emx   # x86_64
scp target/aarch64-unknown-linux-musl/release/emx HOST:.local/bin/emx   # aarch64
```

`make prereqs` installs the musl targets, cargo-zigbuild and zig these need, if missing.

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

Run `emx --help` or `emx help COMMAND` for details. Key bindings can be overridden in `~/.config/emx/config.toml`. `M-x` is Alt+x, or Esc then x (not both).

The config is read when the session server starts, not by each new `emx`: every later `emx` attaches to the running server. After changing it, close every emx (`C-x C-c`) and the server exits 3 seconds later, unless a buffer has unsaved changes; the next `emx` reads the new config.

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

Text you kill or copy also goes to the system clipboard through the terminal (OSC 52). Inside tmux, set `set -g set-clipboard on`. Turn it off with `osc52_clipboard = false`. A copy (`M-w`, or a mouse selection with `mouse_drag_copy_region`) flashes "Copied to clipboard" in the `success` face for `clipboard_notice_ms` (default 1500; 0 keeps it until the next key). A copy larger than `osc52_max_bytes` is not sent and says so in the `warning` face; it is still in the kill ring. emx cannot tell whether the terminal accepted the text: OSC 52 has no reply.

There are two pastes. `C-y` yanks from emx's kill ring, which every kill and copy (mouse copies included) goes to. The terminal's own paste key (Ctrl+Shift+V in kitty) pastes the system clipboard, which is also how text copied in other programs gets into emx.

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

Mouse support is off by default, because turning it on takes over the terminal's own drag-to-select. Turn it on in the config, or for the running session with `M-x xterm-mouse-mode`:

```toml
mouse = true
mouse_drag_copy_region = true   # a mouse selection copies, as in a terminal
```

With the mouse on, a click moves the cursor or selects a window, a drag selects text, double and triple clicks select a word and a line, the right button extends the selection, the wheel scrolls the window under the pointer, and dragging a modeline or border resizes windows and the file panel. Hold Shift to use the terminal's own selection. Inside tmux, also `set -g mouse on`. With `mouse_drag_copy_region = true` (Emacs `mouse-drag-copy-region`), releasing a drag, or a double or triple click, also copies the selection to the kill ring and the clipboard, as a terminal or multiplexer selection does, and flashes "Copied to clipboard" (see Clipboard).

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

### Releasing

Releases are built by [GoReleaser](https://goreleaser.com) from `.goreleaser.yaml`: static musl binaries for Linux x86_64 and aarch64, `.tar.gz` archives, `.deb` packages, checksums, and the Homebrew formula pushed to [gherlein/homebrew-tap](https://github.com/gherlein/homebrew-tap). There are no macOS binaries, because linking for macOS needs Apple's SDK.

```sh
make prereqs      # installs missing tools: musl targets, cargo-zigbuild, zig, goreleaser
make snapshot     # builds everything into dist/ and publishes nothing
```

To release, set the workspace `version` in `Cargo.toml`, commit and push, then tag that commit with the same version and run the release:

```sh
git tag -a vX.Y.Z -m "emx X.Y.Z"
git push origin vX.Y.Z
export GITHUB_TOKEN=$(gh auth token)
export HOMEBREW_TAP_TOKEN=$GITHUB_TOKEN
make release
```

`make release` refuses to run unless HEAD carries the `v` tag matching `Cargo.toml`, since that version is what `emx --version` reports. Both tokens are needed: without `HOMEBREW_TAP_TOKEN` the GitHub release is created but the formula push fails with `401 Bad credentials`. In that case do not run the release again; open a pull request to the tap with `dist/homebrew/Formula/emx.rb` instead.

## Documentation

- [VISION.md](VISION.md): product vision
- [DESIGN.md](DESIGN.md): technical design

## License

MIT OR Apache-2.0
