.PHONY: build test test-integration bench clippy fmt fmt-check

build:
	cargo build --release

test:
	cargo test --release

test-integration:
	cargo test --release --test test_proxy_integration

bench:
	cargo bench

clippy:
	cargo clippy --all-targets -- -D warnings

fmt:
	cargo fmt

fmt-check:
	cargo fmt --check
