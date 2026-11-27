.PHONY: build test test-all test-integration bench bench-baseline bench-check fmt clippy manifests conformance

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
# Instruction counts by default; BENCH_GATE_MEASURE=time without a PMU.
bench-baseline:
	scripts/bench-gate.sh baseline

# Enforcing perf gate: fails past 1.10x the local baseline's instructions
# per iteration. CI's shared-runner bench is advisory; this is the gate
# that counts.
bench-check:
	scripts/bench-gate.sh check

fmt:
	cargo fmt

clippy:
	cargo clippy --all-targets -- -D warnings

manifests:
	scripts/check-manifests.sh

# Creates its own kind cluster and installs deploy/conformance on it.
# PROGRAMMED_SAMPLES=N also times N Gateway creates to Programmed first.
conformance:
	cd conformance && go test -v -timeout 45m -run TestConformance -count=1
