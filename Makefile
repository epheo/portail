.PHONY: build test test-all test-integration bench fmt clippy conformance

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

# Creates its own kind cluster; needs the operator checkout (OPERATOR_DIR, default ../portail-operator).
conformance:
	cd conformance && go test -v -timeout 45m -run TestConformance -count=1
