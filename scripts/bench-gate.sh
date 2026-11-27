#!/usr/bin/env bash
# Enforcing micro-bench regression gate. Run on owned hardware: the CI bench
# on shared GitHub runners is advisory only (see .github/workflows/bench.yml)
# because baseline-vs-PR runner variance there exceeds any honest threshold.
#
#   scripts/bench-gate.sh baseline   record this machine's baseline
#   scripts/bench-gate.sh check      compare against it, exit 1 on regression
#
# BENCH_GATE_MEASURE picks the unit (default instructions):
#   instructions  user-space instructions retired per iteration, read from
#                 the PMU. Code placement alone moves wall time by 16-24%
#                 on byte-identical code (follow-up 28); it moves no
#                 instruction. Needs perf_event_paranoid <= 2 and a PMU,
#                 so no VM without counter passthrough.
#   time          wall time per iteration, the fallback without a PMU.
#
# The instruction defaults come from seven processes of one binary: 66 of
# 72 benches never moved by one instruction, four drifted at most 3.5%,
# and layout-only rebuilds moved none of the 66. An edit off the measured
# path can still change its code (fat LTO re-decides inlining): one to
# main() alone moved 11 benches by -4.7% to +2.4%. Re-record after it.
# 1.10x leaves five points over that drift.
#
# A bench fails when BOTH hold:
#   current > baseline * BENCH_GATE_THRESHOLD
#   current - baseline >= BENCH_GATE_MIN_DELTA
# The absolute floor exists because bencher output is an integer: a bench
# of a few units rounds by a whole one, where a ratio alone is meaningless.
set -euo pipefail

cd "$(dirname "$0")/.."

MEASURE="${BENCH_GATE_MEASURE:-instructions}"
case "$MEASURE" in
instructions)
    UNIT=inst
    DEFAULT_THRESHOLD=1.10
    DEFAULT_MIN_DELTA=2
    # Each settles at a level set per process, so a run of the same
    # binary fails either one: route_table_creation 1,648 to 2,356 inst,
    # parse_malformed_request 392 or 664.
    SKIP='^(route_table_creation|parse_malformed_request)$'
    ;;
time)
    UNIT=ns
    DEFAULT_THRESHOLD=1.15
    DEFAULT_MIN_DELTA=2
    SKIP='^$'
    ;;
*)
    echo "BENCH_GATE_MEASURE must be instructions or time, got '$MEASURE'" >&2
    exit 2
    ;;
esac
BASELINE_FILE="${BENCH_GATE_BASELINE:-target/bench-baseline-$MEASURE.txt}"
THRESHOLD="${BENCH_GATE_THRESHOLD:-$DEFAULT_THRESHOLD}"
MIN_DELTA="${BENCH_GATE_MIN_DELTA:-$DEFAULT_MIN_DELTA}"

run_benches() {
    BENCH_MEASURE="$MEASURE" cargo bench \
        --bench parsing_bench --bench sniff_bench --bench routing_bench --bench end_to_end_bench \
        -- --output-format bencher
}

# bencher lines: "test <name> ... bench: <value> <unit>/iter (+/- <sd>)"
extract() {
    awk '$1 == "test" && $4 == "bench:" { gsub(",", "", $5); sub("/iter", "", $6); print $2, $5, $6 }'
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
    units=$(extract <"$BASELINE_FILE" | awk '{ print $3 }' | sort -u | paste -sd, -)
    if [ "$units" != "$UNIT" ]; then
        echo "baseline $BASELINE_FILE is in '$units', this run would be in $UNIT - record one first: BENCH_GATE_MEASURE=$MEASURE $0 baseline" >&2
        exit 2
    fi
    current=$(run_benches)
    printf '%s\n' "$current"
    fail=0
    while read -r name base _; do
        if [[ "$name" =~ $SKIP ]]; then
            continue
        fi
        cur=$(printf '%s\n' "$current" | extract | awk -v n="$name" '$1 == n { print $2 }')
        if [ -z "$cur" ]; then
            echo "MISSING  $name (in baseline, not in current run)" >&2
            fail=1
            continue
        fi
        verdict=$(awk -v b="$base" -v c="$cur" -v t="$THRESHOLD" -v d="$MIN_DELTA" \
            'BEGIN { print (c > b * t && c - b >= d) ? "FAIL" : "ok" }')
        if [ "$verdict" = "FAIL" ]; then
            echo "REGRESSED  $name: ${base} -> ${cur} ${UNIT}/iter" >&2
            fail=1
        fi
    done < <(extract <"$BASELINE_FILE")
    if [ "$fail" -ne 0 ]; then
        echo "bench gate FAILED (threshold ${THRESHOLD}x, min delta ${MIN_DELTA} ${UNIT})" >&2
        exit 1
    fi
    echo "bench gate ok (${MEASURE}: threshold ${THRESHOLD}x, min delta ${MIN_DELTA} ${UNIT} vs $BASELINE_FILE)"
    ;;
*)
    echo "usage: $0 {baseline|check}" >&2
    exit 2
    ;;
esac
