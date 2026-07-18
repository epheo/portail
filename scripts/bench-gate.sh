#!/usr/bin/env bash
# Enforcing micro-bench regression gate. Run on owned hardware: the CI bench
# on shared GitHub runners is advisory only (see .github/workflows/bench.yml)
# because baseline-vs-PR runner variance there exceeds any honest threshold.
#
#   scripts/bench-gate.sh baseline   record this machine's baseline
#   scripts/bench-gate.sh check      compare against it, exit 1 on regression
#
# A bench fails when BOTH hold:
#   current > baseline * BENCH_GATE_THRESHOLD   (default 1.15)
#   current - baseline >= BENCH_GATE_MIN_DELTA_NS   (default 2)
# The absolute floor exists because bencher output is integer nanoseconds:
# the sub-ns benches print 0 or 1 ns/iter, where a ratio alone is
# meaningless (0 -> 1 ns is rounding, not a regression).
set -euo pipefail

cd "$(dirname "$0")/.."

BASELINE_FILE="${BENCH_GATE_BASELINE:-target/bench-baseline.txt}"
THRESHOLD="${BENCH_GATE_THRESHOLD:-1.15}"
MIN_DELTA_NS="${BENCH_GATE_MIN_DELTA_NS:-2}"

run_benches() {
    cargo bench --bench parsing_bench --bench routing_bench --bench end_to_end_bench \
        -- --output-format bencher
}

# bencher lines: "test <name> ... bench: <ns> ns/iter (+/- <mad>)"
extract() {
    awk '$1 == "test" && $4 == "bench:" { gsub(",", "", $5); print $2, $5 }'
}

case "${1:-}" in
baseline)
    mkdir -p "$(dirname "$BASELINE_FILE")"
    run_benches | tee "$BASELINE_FILE"
    echo "baseline written to $BASELINE_FILE"
    ;;
check)
    if [ ! -f "$BASELINE_FILE" ]; then
        echo "no baseline at $BASELINE_FILE - record one first: $0 baseline" >&2
        exit 2
    fi
    current=$(run_benches)
    printf '%s\n' "$current"
    fail=0
    while read -r name base_ns; do
        cur_ns=$(printf '%s\n' "$current" | extract | awk -v n="$name" '$1 == n { print $2 }')
        if [ -z "$cur_ns" ]; then
            echo "MISSING  $name (in baseline, not in current run)" >&2
            fail=1
            continue
        fi
        verdict=$(awk -v b="$base_ns" -v c="$cur_ns" -v t="$THRESHOLD" -v d="$MIN_DELTA_NS" \
            'BEGIN { print (c > b * t && c - b >= d) ? "FAIL" : "ok" }')
        if [ "$verdict" = "FAIL" ]; then
            echo "REGRESSED  $name: ${base_ns} -> ${cur_ns} ns/iter" >&2
            fail=1
        fi
    done < <(extract <"$BASELINE_FILE")
    if [ "$fail" -ne 0 ]; then
        echo "bench gate FAILED (threshold ${THRESHOLD}x, min delta ${MIN_DELTA_NS} ns)" >&2
        exit 1
    fi
    echo "bench gate ok (threshold ${THRESHOLD}x vs $BASELINE_FILE)"
    ;;
*)
    echo "usage: $0 {baseline|check}" >&2
    exit 2
    ;;
esac
