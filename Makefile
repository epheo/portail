# Privileged tests need rustup available under sudo
# Resolve actual paths — env vars may be unset when rustup uses defaults
RUSTUP_HOME_RESOLVED := $(or $(RUSTUP_HOME),$(HOME)/.rustup)
CARGO_HOME_RESOLVED := $(or $(CARGO_HOME),$(HOME)/.cargo)
SUDO_ENV := sudo env PATH="$(PATH)" RUSTUP_HOME="$(RUSTUP_HOME_RESOLVED)" CARGO_HOME="$(CARGO_HOME_RESOLVED)"

.PHONY: build test test-all test-integration bench

build:
	cargo build --release

test:
	cargo test --release

test-all:
	$(SUDO_ENV) cargo test --release -- --ignored

test-integration:
	$(SUDO_ENV) cargo test --release --test test_proxy_integration -- --ignored

bench:
	$(SUDO_ENV) cargo bench
