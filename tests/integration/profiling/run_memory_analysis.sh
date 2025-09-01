#!/bin/bash
# Run memory allocation analysis on UringRess
# Uses dhat and other tools to analyze memory allocation patterns and detect leaks

set -e

# Colors for output  
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

print_status() {
    local color=$1
    local message=$2
    echo -e "${color}${message}${NC}"
}

# Detect script location and set paths
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
REPORTS_DIR="$SCRIPT_DIR/../reports/memory"
TIMESTAMP=$(date +"%Y%m%d_%H%M%S")

# Create reports directory
mkdir -p "$REPORTS_DIR"

print_status $BLUE "=== UringRess Memory Analysis ==="

cd "$PROJECT_ROOT"

print_status $YELLOW "Checking memory analysis dependencies..."

# Check for valgrind (optional but useful)
if command -v valgrind &> /dev/null; then
    print_status $GREEN "✓ Valgrind found"
    HAS_VALGRIND=true
else
    print_status $YELLOW "⚠ Valgrind not found (optional)"
    HAS_VALGRIND=false
fi

# Check for heaptrack (optional)
if command -v heaptrack &> /dev/null; then
    print_status $GREEN "✓ Heaptrack found"
    HAS_HEAPTRACK=true
else
    print_status $YELLOW "⚠ Heaptrack not found (optional)"
    HAS_HEAPTRACK=false
fi

print_status $YELLOW "Building project with debug symbols for memory analysis..."

# Build with debug symbols and memory profiling features
export CARGO_PROFILE_RELEASE_DEBUG=true
cargo build --release

print_status $GREEN "✓ Build complete"

# Function to run memory profiling on a test
run_memory_test() {
    local test_name=$1
    local output_prefix=$2
    local description=$3
    
    print_status $YELLOW "Memory profiling: $description"
    
    # Run the test and capture memory usage
    echo "=== Memory Test: $description ===" > "$REPORTS_DIR/memory_${output_prefix}_${TIMESTAMP}.log"
    echo "Started: $(date)" >> "$REPORTS_DIR/memory_${output_prefix}_${TIMESTAMP}.log"
    echo "" >> "$REPORTS_DIR/memory_${output_prefix}_${TIMESTAMP}.log"
    
    # Run with memory monitoring using /usr/bin/time
    if /usr/bin/time -v cargo test --release "$test_name" --test test_memory_profiling -- --nocapture 2>&1 | tee -a "$REPORTS_DIR/memory_${output_prefix}_${TIMESTAMP}.log"; then
        print_status $GREEN "✓ $description - memory data captured"
    else
        print_status $RED "✗ $description - test failed"
    fi
    
    echo "" >> "$REPORTS_DIR/memory_${output_prefix}_${TIMESTAMP}.log"
    echo "Completed: $(date)" >> "$REPORTS_DIR/memory_${output_prefix}_${TIMESTAMP}.log"
}

# Run memory profiling tests
print_status $YELLOW "Running memory allocation tests..."

run_memory_test "test_memory_profiling_parser_allocations" "parser" "HTTP Parser Memory Analysis"
run_memory_test "test_memory_profiling_routing_allocations" "routing" "Routing Engine Memory Analysis"
run_memory_test "test_memory_profiling_buffer_pool_behavior" "buffers" "Buffer Pool Memory Analysis" 
run_memory_test "test_memory_profiling_fnv_hash_allocations" "hash" "FNV Hash Memory Analysis"
run_memory_test "test_memory_profiling_end_to_end_allocations" "e2e" "End-to-End Memory Analysis"

# Run comprehensive stress test
print_status $YELLOW "Running memory stress test..."

run_memory_test "test_memory_profiling_stress_test" "stress" "Comprehensive Memory Stress Test"

# Run heaptrack analysis if available
if [ "$HAS_HEAPTRACK" = true ]; then
    print_status $YELLOW "Running heaptrack analysis..."
    
    heaptrack --output "$REPORTS_DIR/heaptrack_${TIMESTAMP}.dat" \
              cargo test --release test_memory_profiling_stress_test --test test_memory_profiling -- --ignored --nocapture
    
    if [ $? -eq 0 ]; then
        print_status $GREEN "✓ Heaptrack data captured"
        
        # Generate heaptrack report
        heaptrack_print "$REPORTS_DIR/heaptrack_${TIMESTAMP}.dat" > "$REPORTS_DIR/heaptrack_report_${TIMESTAMP}.txt"
        print_status $GREEN "✓ Heaptrack report generated"
    else
        print_status $RED "✗ Heaptrack analysis failed"
    fi
fi

# Run valgrind analysis if available (optional - can be slow)
if [ "$HAS_VALGRIND" = true ]; then
    print_status $YELLOW "Running Valgrind memory leak detection (this may take a while)..."
    
    # Run a shorter test with valgrind to avoid excessive runtime
    timeout 300s valgrind --tool=memcheck \
                          --leak-check=full \
                          --show-leak-kinds=all \
                          --track-origins=yes \
                          --log-file="$REPORTS_DIR/valgrind_${TIMESTAMP}.log" \
                          cargo test --release test_memory_profiling_parser_allocations --test test_memory_profiling -- --nocapture \
                          2>&1 || {
        print_status $YELLOW "Valgrind analysis timed out (normal for comprehensive analysis)"
    }
    
    if [ -f "$REPORTS_DIR/valgrind_${TIMESTAMP}.log" ]; then
        print_status $GREEN "✓ Valgrind analysis saved"
    fi
fi

# Analyze buffer pool efficiency with benchmarks
print_status $YELLOW "Running buffer allocation benchmarks..."

timeout 120s cargo bench --bench allocation_bench 2>&1 | tee "$REPORTS_DIR/allocation_bench_${TIMESTAMP}.log" || {
    print_status $YELLOW "Allocation benchmark timed out (captured partial results)"
}

print_status $BLUE "=== Memory Analysis Complete ==="

# Generate summary report
SUMMARY_FILE="$REPORTS_DIR/memory_analysis_summary_${TIMESTAMP}.txt"
cat > "$SUMMARY_FILE" <<EOF
UringRess Memory Analysis Report
Generated: $(date)

=== Analysis Overview ===
This report analyzes memory allocation patterns, buffer pool efficiency,
and potential memory leaks in UringRess hot paths.

=== Generated Reports ===
$(ls -1 "$REPORTS_DIR"/*_${TIMESTAMP}.* | sed 's|.*/||' | sort)

=== Key Memory Metrics to Analyze ===

1. Parser Memory Analysis:
   - Should show zero allocations in steady state
   - String slices should reference original buffer
   - No dynamic string creation in hot paths

2. Routing Engine Memory Analysis:
   - Hash computation should not allocate
   - Route lookups should not allocate
   - Backend selection should not allocate

3. Buffer Pool Analysis:
   - High reuse rates after warmup (>90%)
   - Minimal new allocations in steady state
   - Appropriate pool size management

4. FNV Hash Analysis:
   - Zero allocations (operates on borrowed strings)
   - Consistent performance across string lengths

5. End-to-End Analysis:
   - Combined pipeline allocation behavior
   - Overall system memory efficiency

=== Performance Targets ===
- Zero allocations in HTTP parsing hot paths
- <0.01 allocations per request in steady state
- Buffer pool reuse rate >90%
- No memory leaks in stress tests
- Minimal heap growth during sustained operation

=== Analysis Tools Used ===
EOF

if [ "$HAS_HEAPTRACK" = true ]; then
    echo "- Heaptrack: Detailed heap allocation analysis" >> "$SUMMARY_FILE"
fi

if [ "$HAS_VALGRIND" = true ]; then
    echo "- Valgrind: Memory leak detection" >> "$SUMMARY_FILE"  
fi

cat >> "$SUMMARY_FILE" <<EOF
- GNU time: Memory usage monitoring
- Cargo benchmarks: Allocation performance
- Custom memory tests: Targeted analysis

=== Next Steps ===
1. Review memory test logs for allocation patterns
2. Check for unexpected allocations in hot paths
3. Validate buffer pool reuse efficiency
4. Compare with HAProxy memory usage (when available)
5. Optimize high-allocation code paths

=== Files Analysis Guide ===
- memory_*_${TIMESTAMP}.log: Detailed test outputs with memory stats
- heaptrack_report_${TIMESTAMP}.txt: Heap allocation analysis
- valgrind_${TIMESTAMP}.log: Memory leak detection results
- allocation_bench_${TIMESTAMP}.log: Buffer pool performance data

Report saved to: $SUMMARY_FILE
EOF

print_status $GREEN "Memory analysis complete!"
print_status $BLUE "Generated reports:"
ls -la "$REPORTS_DIR"/*_${TIMESTAMP}.* | while read -r line; do
    print_status $BLUE "  $(basename "$(echo "$line" | awk '{print $NF}')")"
done

print_status $BLUE "Memory analysis summary: $SUMMARY_FILE"
print_status $BLUE "📊 Memory profiling analysis complete! 📊"