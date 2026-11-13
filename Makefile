.DEFAULT_GOAL := help

.PHONY: help build test run-tests check clean publish-dry-run publish

help:
	@echo "Targets:"
	@echo "  build            Build release binary"
	@echo "  test             Run tests"
	@echo "  run-tests        Alias for test"
	@echo "  check            cargo check + clippy + fmt check"
	@echo "  clean            Remove build artifacts"
	@echo "  publish-dry-run  Validate crate packaging for crates.io"
	@echo "  publish          Publish crate to crates.io"

build:
	cargo build --release

test:
	cargo test

run-tests: test

check:
	cargo check
	cargo clippy -- -D warnings
	cargo fmt -- --check

clean:
	cargo clean

publish-dry-run:
	cargo publish --dry-run --allow-dirty

publish:
	cargo publish
