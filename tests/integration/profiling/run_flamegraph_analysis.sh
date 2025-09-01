#!/bin/bash
# Run flamegraph analysis on UringRess hot paths
# Generates CPU flamegraphs for performance bottleneck identification

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
REPORTS_DIR="$SCRIPT_DIR/../reports/cpu"
TIMESTAMP=$(date +"%Y%m%d_%H%M%S")

# Create reports directory
mkdir -p "$REPORTS_DIR"

print_status $BLUE "=== UringRess CPU Flamegraph Analysis ==="

cd "$PROJECT_ROOT"

# Enable debug symbols for profiling
export CARGO_PROFILE_RELEASE_DEBUG=true

# Ensure project is built with debug symbols
if [ ! -f "target/release/uringress" ]; then
    print_status $YELLOW "Building project with debug symbols..."
    cargo build --release
fi

print_status $YELLOW "Running CPU profiling tests with flamegraph..."

# Function to run flamegraph on a specific test
run_flamegraph_test() {
    local test_name=$1
    local output_name=$2
    local description=$3
    
    print_status $YELLOW "Profiling: $description"
    
    # Use cargo flamegraph (much simpler!)
    local svg_output="$REPORTS_DIR/flamegraph_${output_name}_${TIMESTAMP}.svg"
    
    # Run flamegraph with the Rust crate - correct syntax for integration tests
    print_status $BLUE "Running: cargo flamegraph --output $svg_output --test test_cpu_profiling -- $test_name --nocapture"
    if cargo flamegraph --output "$svg_output" --test test_cpu_profiling -- "$test_name" --nocapture; then
        print_status $GREEN "✓ $description flamegraph saved"
    else
        print_status $RED "✗ Failed to generate $description flamegraph"
        print_status $RED "Check the error output above for details"
    fi
}

# Generate flamegraphs for different hot paths - Library Functions
run_flamegraph_test "test_cpu_profiling_parser_hot_path" "parser" "HTTP Parser Hot Path"
run_flamegraph_test "test_cpu_profiling_routing_hot_path" "routing" "Routing Engine Hot Path" 
run_flamegraph_test "test_cpu_profiling_fnv_hash_hot_path" "hash" "FNV Hash Function Hot Path"
run_flamegraph_test "test_cpu_profiling_end_to_end_hot_path" "e2e" "End-to-End Request Processing"

print_status $YELLOW "Running integration profiling tests (CRITICAL BOTTLENECKS)..."

# Profile the integration tests that cover worker and backend modules
run_flamegraph_integration_test() {
    local test_name=$1
    local output_name=$2  
    local description=$3
    
    print_status $YELLOW "Profiling: $description"
    
    local svg_output="$REPORTS_DIR/flamegraph_integration_${output_name}_${TIMESTAMP}.svg"
    
    print_status $BLUE "Running: cargo flamegraph --output $svg_output --test test_integration_profiling -- $test_name --nocapture"
    if cargo flamegraph --output "$svg_output" --test test_integration_profiling -- "$test_name" --nocapture; then
        print_status $GREEN "✓ $description flamegraph saved"
    else
        print_status $RED "✗ Failed to generate $description flamegraph"
        print_status $RED "Check the error output above for details"
    fi
}

# Critical integration tests - these cover the REAL bottlenecks
run_flamegraph_integration_test "test_integration_profiling_backend_connection_pool" "backend_pool" "Backend Connection Pool (CRITICAL)"
run_flamegraph_integration_test "test_integration_profiling_buffer_pool_realistic" "buffer_realistic" "Realistic Buffer Pool Usage (CRITICAL)"
run_flamegraph_integration_test "test_integration_profiling_worker_metrics" "worker_metrics" "Worker Metrics Under Load (CRITICAL)"
run_flamegraph_integration_test "test_integration_profiling_full_pipeline_simulation" "full_pipeline" "Complete Request Pipeline (MOST CRITICAL)"

print_status $YELLOW "Running comprehensive stress test flamegraph..."

# Run the library-level stress test
run_flamegraph_test "test_cpu_profiling_stress_test" "stress" "Library CPU Stress Test"

# Run the integration-level stress test (MOST IMPORTANT)
run_flamegraph_integration_test "test_integration_profiling_stress_test" "integration_stress" "Integration Stress Test (ULTIMATE BOTTLENECK FINDER)"

print_status $YELLOW "Running benchmark flamegraphs..."

# Profile each benchmark suite
for benchmark in parsing_bench routing_bench end_to_end_bench; do
    print_status $YELLOW "Profiling benchmark: $benchmark"
    
    svg_output="$REPORTS_DIR/flamegraph_${benchmark}_${TIMESTAMP}.svg"
    print_status $BLUE "Running: timeout 60s cargo flamegraph --output $svg_output --bench $benchmark"
    timeout 60s cargo flamegraph --output "$svg_output" --bench "$benchmark" || {
        print_status $YELLOW "Benchmark $benchmark timed out or failed (normal for detailed profiling)"
    }
done

print_status $BLUE "=== Flamegraph Analysis Complete ==="
print_status $BLUE "Generated flamegraphs in: $REPORTS_DIR"

# List generated files
print_status $GREEN "Generated files:"
ls -la "$REPORTS_DIR"/flamegraph_*_${TIMESTAMP}.svg | while read -r line; do
    print_status $GREEN "  $(basename "$(echo "$line" | awk '{print $NF}')")"
done

print_status $BLUE "Open flamegraphs in browser to analyze CPU hotspots:"
print_status $BLUE "  firefox $REPORTS_DIR/flamegraph_*_${TIMESTAMP}.svg"

# Generate summary report
SUMMARY_FILE="$REPORTS_DIR/flamegraph_analysis_${TIMESTAMP}.txt"
cat > "$SUMMARY_FILE" <<EOF
UringRess CPU Flamegraph Analysis Report
Generated: $(date)

=== Analysis Overview ===
This report contains CPU flamegraphs for UringRess performance analysis.
Each flamegraph shows function call stacks and CPU time distribution.

=== Generated Flamegraphs ===
$(ls -1 "$REPORTS_DIR"/flamegraph_*_${TIMESTAMP}.svg | sed 's|.*/||' | sed "s/_${TIMESTAMP}.svg//" | sort)

=== How to Read Flamegraphs ===
1. Width = CPU time spent in function
2. Height = call stack depth
3. Color intensity = heat (red = hot)
4. Click on functions to zoom in
5. Search for specific functions using browser search

=== Key Areas to Analyze ===
1. parser flamegraph: Look for HTTP parsing bottlenecks
2. routing flamegraph: Check routing table lookup performance
3. hash flamegraph: Verify FNV hash function efficiency
4. e2e flamegraph: Identify end-to-end pipeline bottlenecks
5. stress_test flamegraph: Overall system performance profile
6. benchmark flamegraphs: Detailed analysis of specific workloads

=== Performance Optimization Targets ===
- Functions taking >10% of CPU time
- Deep call stacks (>20 levels)
- Allocation-heavy functions (malloc/free calls)
- System call overhead (syscall functions)
- Lock contention (mutex/lock functions)

=== Next Steps ===
1. Open flamegraphs in browser for visual analysis
2. Identify top CPU consumers
3. Compare with HAProxy flamegraphs (when available)
4. Focus optimization efforts on widest flame sections
5. Re-run after optimizations to measure improvements

Report saved to: $SUMMARY_FILE
EOF

print_status $GREEN "Analysis summary saved to: $SUMMARY_FILE"

print_status $BLUE "🔥 CPU Flamegraph analysis complete! 🔥"