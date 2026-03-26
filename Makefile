.PHONY: build test test-all test-integration bench fmt clippy

build:
	cargo build --release

test:
	cargo test --release

test-all:
	cargo test --release

test-integration:
	cargo test --release --test test_proxy_integration

bench:
	cargo bench

fmt:
	cargo fmt

clippy:
	cargo clippy --all-targets -- -D warnings
