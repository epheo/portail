.PHONY: build test test-all test-integration bench fmt clippy conformance conformance-kind

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

conformance:
	./deploy-conformance.sh --no-kind

conformance-kind:
	./deploy-conformance.sh
