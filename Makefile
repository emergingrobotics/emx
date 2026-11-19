.DEFAULT_GOAL := help

TEST_ARGS ?=
FUZZ_SECONDS ?= 60
MUSL_TARGETS := x86_64-unknown-linux-musl aarch64-unknown-linux-musl
MACOS_TARGETS := x86_64-apple-darwin aarch64-apple-darwin
BENCH_SIZES := 10MB 100MB-line 1GB
BENCH_FULL_SIZES := $(BENCH_SIZES) 10GB
PREFIX ?= /usr/local
BINDIR ?= $(PREFIX)/bin
USER_PREFIX ?= $(HOME)/.local
GORELEASER_VERSION := v2.18.3
# Recursive, so cargo runs only for the targets that use it.
EMX_VERSION = $(shell cargo pkgid -p emx | sed 's/.*[#@]//')

.PHONY: help prereqs build install install-sys install-user uninstall test run-tests difftest check fmt fuzz bench bench-full static snapshot deb release macos clean publish-dry-run publish

help:
	@echo "Targets:"
	@echo "  prereqs          Install missing release tools (musl targets, cargo-zigbuild, zig, goreleaser)"
	@echo "  build            Release build of emx, copied to bin/emx"
	@echo "  install          Build and copy emx to BINDIR (PREFIX/bin, default /usr/local/bin); sudo if not writable"
	@echo "  install-sys      Alias for install"
	@echo "  install-user     Build and copy emx to ~/.local/bin (install with PREFIX=USER_PREFIX)"
	@echo "  uninstall        Remove emx from BINDIR (pass the same PREFIX used to install)"
	@echo "  test             Run all workspace tests (TEST_ARGS to filter)"
	@echo "  run-tests        Run all tests (includes the differential suite)"
	@echo "  difftest         Run only the differential tests against Emacs"
	@echo "  check            fmt check + clippy -D warnings + run-tests"
	@echo "  fmt              Format all code"
	@echo "  fuzz             Run each fuzz target for FUZZ_SECONDS on nightly"
	@echo "  bench            Benchmarks on 10 MB and 1 GB corpora"
	@echo "  bench-full       Benchmarks on 10 MB, 1 GB and 10 GB corpora"
	@echo "  static           Static musl builds for x86_64 and aarch64"
	@echo "  snapshot         Local GoReleaser run into dist/: tarballs, .debs, formula; publishes nothing"
	@echo "  deb              .deb packages (amd64, arm64) in dist/, via snapshot"
	@echo "  release          Publish GitHub release + .debs + Homebrew formula for the vX.Y.Z tag at HEAD"
	@echo "  macos            macOS builds (x86_64, arm64); needs SDKROOT set to a macOS SDK"
	@echo "  clean            Remove build artifacts"
	@echo "  publish-dry-run  Validate packaging of all crates for crates.io"
	@echo "  publish          Publish all crates to crates.io in dependency order"

# Installs only what is missing. rustup, python3 and go are base toolchains
# that need a system-level choice (rustup.rs, the distribution's python3,
# go.dev), so they are checked, not installed. zig is the cross C toolchain,
# installed as a Python module; --break-system-packages keeps it a user-space
# install on externally-managed interpreters (no sudo, no system packages
# touched). goreleaser is pinned: the Rust builder needs v2.5 or later.
prereqs:
	@for tool in rustup cargo python3 go; do \
		command -v $$tool >/dev/null || { echo "$$tool is required and not installed"; exit 1; }; \
	done
	rustup target add $(MUSL_TARGETS)
	@command -v cargo-zigbuild >/dev/null || { \
		echo cargo install --locked cargo-zigbuild; \
		cargo install --locked cargo-zigbuild; \
	}
	@python3 -c 'import ziglang' 2>/dev/null || { \
		echo python3 -m pip install --user --break-system-packages ziglang; \
		python3 -m pip install --user --break-system-packages ziglang; \
	}
	@command -v goreleaser >/dev/null || { \
		echo go install github.com/goreleaser/goreleaser/v2@$(GORELEASER_VERSION); \
		go install github.com/goreleaser/goreleaser/v2@$(GORELEASER_VERSION) || exit 1; \
		gobin=$$(go env GOBIN); gobin=$${gobin:-$$(go env GOPATH)/bin}; \
		command -v goreleaser >/dev/null || { echo "goreleaser installed to $$gobin, which is not on PATH"; exit 1; }; \
	}

# Warnings fail the release build too, as clippy -D warnings fails check.
build:
	RUSTFLAGS="$${RUSTFLAGS:+$$RUSTFLAGS }-Dwarnings" cargo build --release -p emx
	mkdir -p bin
	cp target/release/emx bin/emx

# Only the copy runs under sudo, so target/ and bin/ stay owned by the user
# (`sudo make install` would run cargo as root). sudo is used only when
# BINDIR is not writable, so a per-user PREFIX never prompts.
install: build
	@if install -d "$(BINDIR)" 2>/dev/null && [ -w "$(BINDIR)" ]; then \
		echo install -m 0755 bin/emx "$(BINDIR)/emx"; \
		install -m 0755 bin/emx "$(BINDIR)/emx"; \
	else \
		echo sudo install -D -m 0755 bin/emx "$(BINDIR)/emx"; \
		sudo install -D -m 0755 bin/emx "$(BINDIR)/emx"; \
	fi

install-sys: install

install-user:
	$(MAKE) install PREFIX=$(USER_PREFIX)

uninstall:
	@if [ -w "$(BINDIR)" ]; then \
		echo rm -f "$(BINDIR)/emx"; \
		rm -f "$(BINDIR)/emx"; \
	else \
		echo sudo rm -f "$(BINDIR)/emx"; \
		sudo rm -f "$(BINDIR)/emx"; \
	fi

test:
	cargo test $(if $(TEST_ARGS),$(TEST_ARGS),--workspace)

run-tests: test

difftest:
	cargo test -p emx-core --test difftest

check:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings
	$(MAKE) run-tests

fmt:
	cargo fmt --all

fuzz:
	rustup toolchain install nightly --profile minimal
	cargo install cargo-fuzz
	cd crates/emx-text && for target in $$(cargo +nightly fuzz list); do \
		echo cargo +nightly fuzz run $$target -- -max_total_time=$(FUZZ_SECONDS); \
		cargo +nightly fuzz run $$target -- -max_total_time=$(FUZZ_SECONDS) || exit 1; \
	done

# The benchmarks drive the real release binary; build it first and point
# emx-bench at it. Corpora are generated on local disk by emx-bench itself,
# never under this repository (it is on a network filesystem).
bench: build
	for size in $(BENCH_SIZES); do \
		echo cargo run --release -p emx-bench -- --size $$size; \
		EMX_BIN=$(CURDIR)/bin/emx cargo run --release -p emx-bench -- --size $$size || exit 1; \
	done

bench-full: build
	for size in $(BENCH_FULL_SIZES); do \
		echo cargo run --release -p emx-bench -- --size $$size; \
		EMX_BIN=$(CURDIR)/bin/emx cargo run --release -p emx-bench -- --size $$size || exit 1; \
	done

# Static musl builds use mimalloc as the global allocator (DESIGN 8: musl's
# default allocator is too slow).
static: prereqs
	for target in $(MUSL_TARGETS); do \
		echo cargo zigbuild --release -p emx --target $$target; \
		cargo zigbuild --release -p emx --target $$target || exit 1; \
		ls -l target/$$target/release/emx; \
	done

# Everything a release would publish, built into dist/ from the working tree
# (.goreleaser.yaml). A snapshot takes no version from git, so the Cargo.toml
# version is passed in.
snapshot: prereqs
	EMX_VERSION=$(EMX_VERSION) goreleaser release --snapshot --clean

deb: snapshot
	ls -l dist/*.deb

# Only from a clean checkout whose HEAD carries the vX.Y.Z tag matching
# Cargo.toml, which is the version `emx --version` reports. GITHUB_TOKEN
# publishes the release; HOMEBREW_TAP_TOKEN pushes the formula to
# gherlein/homebrew-tap (without it the release is created and the tap push
# fails with 401 Bad credentials).
release: prereqs
	@test -n "$$GITHUB_TOKEN" || { echo 'GITHUB_TOKEN is not set: export GITHUB_TOKEN=$$(gh auth token)'; exit 1; }
	@test -n "$$HOMEBREW_TAP_TOKEN" || { echo 'HOMEBREW_TAP_TOKEN is not set: export HOMEBREW_TAP_TOKEN=$$GITHUB_TOKEN'; exit 1; }
	@git tag --points-at HEAD | grep -qx "v$(EMX_VERSION)" || { \
		echo "HEAD must be tagged v$(EMX_VERSION) (the Cargo.toml version)"; exit 1; \
	}
	goreleaser release --clean

# Cross-builds for macOS with zig. Linking needs Apple's frameworks, so
# SDKROOT must point at a macOS SDK (on a Mac, `xcrun --show-sdk-path`).
macos: prereqs
	@test -n "$(SDKROOT)" || { echo "SDKROOT must name a macOS SDK"; exit 1; }
	rustup target add $(MACOS_TARGETS)
	for target in $(MACOS_TARGETS); do \
		echo cargo zigbuild --release -p emx --target $$target; \
		cargo zigbuild --release -p emx --target $$target || exit 1; \
	done

clean:
	cargo clean
	rm -rf dist

publish-dry-run:
	cargo publish --workspace --dry-run --allow-dirty

publish:
	cargo publish --workspace
