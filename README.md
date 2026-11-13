# emx

A fast terminal editor with Emacs keybindings, built for humans working alongside coding agents: it watches files that agents, build tools, and `git` rewrite, shows what changed, never loses either side's work, and can itself be driven over a socket. It also handles multi-GB files and ships as one static binary, so it is a practical `emacs-nox` replacement on remote and constrained boxes.

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
