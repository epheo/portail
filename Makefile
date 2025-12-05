# Privileged tests need rustup available under sudo
# Resolve actual paths — env vars may be unset when rustup uses defaults
RUSTUP_HOME_RESOLVED := $(or $(RUSTUP_HOME),$(HOME)/.rustup)
CARGO_HOME_RESOLVED := $(or $(CARGO_HOME),$(HOME)/.cargo)

.PHONY: build test test-all test-integration bench

build:
	cargo build --release

test:
	cargo test --release

test-all:
	cargo test --release -- --ignored

test-integration:
	cargo test --release --test test_proxy_integration -- --ignored

bench:
	cargo bench
