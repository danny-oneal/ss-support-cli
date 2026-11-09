# ss — developer entry points.
# `make help` lists targets; `make check` is what CI runs.

.DEFAULT_GOAL := build

## build: compile the workspace in release mode
.PHONY: build
build:
	cargo build --workspace --release

## test: run the full test suite
.PHONY: test
test:
	cargo test --workspace

## lint: rustfmt check, then clippy with warnings as errors
.PHONY: lint
lint:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings

## check: everything CI runs — fmt, clippy, tests
.PHONY: check
check: lint test

## msrv: type-check on the minimum supported Rust (1.88)
.PHONY: msrv
msrv:
	cargo +1.88 check --workspace --all-targets --locked

## contract: S3 contract suite against MinIO (needs SS_S3_TEST_ENDPOINT, e.g. http://127.0.0.1:9000)
.PHONY: contract
contract:
	@test -n "$(SS_S3_TEST_ENDPOINT)" || { echo "set SS_S3_TEST_ENDPOINT"; exit 2; }
	cargo test -p ss aws::s3::minio -- --nocapture

## clean: remove build artifacts
.PHONY: clean
clean:
	cargo clean

## help: list targets
.PHONY: help
help:
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/^## /  /'
