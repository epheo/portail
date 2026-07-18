.PHONY: build test test-all test-integration bench bench-baseline bench-check fmt clippy conformance

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

# Record this machine's micro-bench baseline (owned hardware only).
bench-baseline:
	scripts/bench-gate.sh baseline

# Enforcing perf gate: fails past 1.15x the local baseline. CI's
# shared-runner bench is advisory; this is the gate that counts.
bench-check:
	scripts/bench-gate.sh check

fmt:
	cargo fmt

clippy:
	cargo clippy --all-targets -- -D warnings

# Creates its own kind cluster; needs the operator checkout (OPERATOR_DIR, default ../portail-operator).
conformance:
	cd conformance && go test -v -timeout 45m -run TestConformance -count=1
