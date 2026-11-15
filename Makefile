.DEFAULT_GOAL := help

TEST_ARGS ?=
FUZZ_SECONDS ?= 60
MUSL_TARGETS := x86_64-unknown-linux-musl aarch64-unknown-linux-musl
BENCH_SIZES := 10MB 100MB-line 1GB
BENCH_FULL_SIZES := $(BENCH_SIZES) 10GB

.PHONY: help build test run-tests difftest check fmt fuzz bench bench-full static clean publish-dry-run publish

help:
	@echo "Targets:"
	@echo "  build            Release build of emx, copied to bin/emx"
	@echo "  test             Run all workspace tests (TEST_ARGS to filter)"
	@echo "  run-tests        Run all tests (includes the differential suite)"
	@echo "  difftest         Run only the differential tests against Emacs"
	@echo "  check            fmt check + clippy -D warnings + run-tests"
	@echo "  fmt              Format all code"
	@echo "  fuzz             Run each fuzz target for FUZZ_SECONDS on nightly"
	@echo "  bench            Benchmarks on 10 MB and 1 GB corpora"
	@echo "  bench-full       Benchmarks on 10 MB, 1 GB and 10 GB corpora"
	@echo "  static           Static musl builds for x86_64 and aarch64"
	@echo "  clean            Remove build artifacts"
	@echo "  publish-dry-run  Validate crate packaging for crates.io"
	@echo "  publish          Publish crate to crates.io"

# Warnings fail the release build too, as clippy -D warnings fails check.
build:
	RUSTFLAGS="$${RUSTFLAGS:+$$RUSTFLAGS }-Dwarnings" cargo build --release -p emx
	mkdir -p bin
	cp target/release/emx bin/emx

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
# default allocator is too slow). zig is the cross C toolchain, installed as a
# Python module; --break-system-packages keeps it a user-space install on
# externally-managed interpreters (no sudo, no system packages touched).
static:
	rustup target add $(MUSL_TARGETS)
	cargo install cargo-zigbuild
	python3 -m pip install --user --break-system-packages ziglang
	for target in $(MUSL_TARGETS); do \
		echo cargo zigbuild --release -p emx --target $$target; \
		cargo zigbuild --release -p emx --target $$target || exit 1; \
		ls -l target/$$target/release/emx; \
	done

clean:
	cargo clean

publish-dry-run:
	cargo publish --dry-run --allow-dirty

publish:
	cargo publish
