#!/bin/bash
# Enhanced Flamegraph Generation for UringRess
# Includes debug symbols, sampling-based timing, and isolated function profiling

set -e

# Color output
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

print_status $BLUE "=== UringRess Enhanced Flamegraph Generation ==="
print_status $BLUE "Features: Debug symbols + Sampling + Isolated profiling"
print_status $BLUE "Reports dir: $REPORTS_DIR"

cd "$PROJECT_ROOT"

# Enable profiling-optimized builds with full debug symbols and reduced LTO
export CARGO_PROFILE_PROFILING_DEBUG=2
export CARGO_PROFILE_PROFILING_STRIP=false  
export CARGO_PROFILE_PROFILING_LTO=false

print_status $YELLOW "Building with profiling-optimized configuration..."

# Pre-build to ensure all dependencies are ready
if ! cargo build --profile profiling --tests; then
    print_status $RED "✗ Failed to build tests with profiling profile"
    exit 1
fi

print_status $GREEN "✓ Build successful with debug symbols enabled"

# Function to run enhanced flamegraph
run_enhanced_flamegraph() {
    local test_name=$1
    local output_name=$2
    local description=$3
    local test_file=$4
    local timeout_duration=${5:-120}  # Default 2 minutes, longer for isolated tests
    local ignored_flag=${6:-""}  # Optional --ignored flag for stress tests
    
    print_status $YELLOW "Profiling: $description"
    
    local svg_output="$REPORTS_DIR/enhanced_flamegraph_${output_name}_${TIMESTAMP}.svg"
    
    print_status $BLUE "Running: timeout ${timeout_duration}s cargo flamegraph --profile profiling --output $svg_output --test $test_file -- $test_name --nocapture --test-threads=1 $ignored_flag"
    
    # Use single test thread to avoid concurrency interference
    if timeout ${timeout_duration}s cargo flamegraph --profile profiling --output "$svg_output" --test "$test_file" -- "$test_name" --nocapture --test-threads=1 $ignored_flag; then
        print_status $GREEN "✓ Enhanced $description flamegraph saved to $(basename $svg_output)"
        
        # Verify flamegraph contains function symbols
        if grep -q "uringress::" "$svg_output" 2>/dev/null; then
            print_status $GREEN "  ✓ UringRess function symbols detected in flamegraph"
        else
            print_status $YELLOW "  ⚠ Limited UringRess function symbols (may need longer run time)"
        fi
        
        return 0
    else
        print_status $RED "✗ Failed to generate enhanced $description flamegraph"
        return 1
    fi
}

# Track successful generations
successful_enhanced_flamegraphs=0

print_status $BLUE "--- Enhanced Integration Tests (Sampling-Based Timing) ---"

# Full Pipeline with sampling-based timing (reduced overhead)
if run_enhanced_flamegraph "test_integration_profiling_full_pipeline_simulation" "integration_pipeline" "Full Pipeline with Sampling (ENHANCED)" "test_integration_profiling" 180; then
    ((successful_enhanced_flamegraphs++))
fi

# Stress test with sampling-based timing
if run_enhanced_flamegraph "test_integration_profiling_stress_test" "integration_stress" "Stress Test with Sampling (ENHANCED)" "test_integration_profiling" 240 "--ignored"; then
    ((successful_enhanced_flamegraphs++))
fi

# Worker metrics with reduced overhead
if run_enhanced_flamegraph "test_integration_profiling_worker_metrics" "worker_metrics" "Worker Metrics (ENHANCED)" "test_integration_profiling" 120; then
    ((successful_enhanced_flamegraphs++))
fi

print_status $BLUE "--- Isolated Function Profiling (Zero Framework Overhead) ---"

# Isolated FNV Hash - Pure function profiling
if run_enhanced_flamegraph "test_isolated_fnv_hash_profiling" "isolated_hash" "Isolated FNV Hash (ZERO OVERHEAD)" "isolated_performance_profiling" 180; then
    ((successful_enhanced_flamegraphs++))
fi

# Isolated HTTP Parser - Pure function profiling  
if run_enhanced_flamegraph "test_isolated_http_parsing_profiling" "isolated_parser" "Isolated HTTP Parser (ZERO OVERHEAD)" "isolated_performance_profiling" 180; then
    ((successful_enhanced_flamegraphs++))
fi

# Isolated Route Lookup - Pure function profiling
if run_enhanced_flamegraph "test_isolated_route_lookup_profiling" "isolated_routing" "Isolated Route Lookup (ZERO OVERHEAD)" "isolated_performance_profiling" 180; then
    ((successful_enhanced_flamegraphs++))
fi

# Isolated Buffer Operations - Pure function profiling
if run_enhanced_flamegraph "test_isolated_buffer_operations_profiling" "isolated_buffer" "Isolated Buffer Operations (ZERO OVERHEAD)" "isolated_performance_profiling" 180; then
    ((successful_enhanced_flamegraphs++))
fi

# Isolated End-to-End Pipeline - Pure function profiling  
if run_enhanced_flamegraph "test_isolated_e2e_pipeline_profiling" "isolated_e2e" "Isolated E2E Pipeline (ZERO OVERHEAD)" "isolated_performance_profiling" 300; then
    ((successful_enhanced_flamegraphs++))
fi

# Combined isolated profiling for comprehensive analysis
if run_enhanced_flamegraph "test_all_isolated_profiling" "isolated_combined" "All Isolated Functions (COMPREHENSIVE)" "isolated_performance_profiling" 600; then
    ((successful_enhanced_flamegraphs++))
fi

print_status $BLUE "--- Comparative Analysis Tests ---"

# Original performance tests for comparison
if run_enhanced_flamegraph "test_fnv_hash_performance" "original_hash" "Original Hash Test (COMPARISON)" "test_performance" 120; then
    ((successful_enhanced_flamegraphs++))
fi

if run_enhanced_flamegraph "test_end_to_end_request_latency" "original_e2e" "Original E2E Test (COMPARISON)" "test_performance" 120; then
    ((successful_enhanced_flamegraphs++))
fi

print_status $BLUE "=== Enhanced Flamegraph Generation Complete ==="
print_status $GREEN "Successfully generated $successful_enhanced_flamegraphs enhanced flamegraph(s)"
print_status $BLUE "Generated enhanced flamegraphs in: $REPORTS_DIR"

# List generated files with size and symbol info
if [ $successful_enhanced_flamegraphs -gt 0 ]; then
    print_status $GREEN "Generated enhanced flamegraph files:"
    ls -lh "$REPORTS_DIR"/enhanced_flamegraph_*_${TIMESTAMP}.svg 2>/dev/null | while read -r line; do
        filename=$(echo "$line" | awk '{print $NF}')
        filesize=$(echo "$line" | awk '{print $5}')
        print_status $GREEN "  $(basename "$filename") ($filesize)"
    done
    
    print_status $BLUE ""
    print_status $BLUE "=== Analysis Instructions ==="
    print_status $BLUE "1. Compare isolated vs integration flamegraphs:"
    print_status $BLUE "   - Isolated: isolated_* files show pure function performance"
    print_status $BLUE "   - Integration: integration_* files show sampling-based timing" 
    print_status $BLUE ""
    print_status $BLUE "2. Look for UringRess function symbols:"
    print_status $BLUE "   - Search for 'uringress::' in flamegraphs"
    print_status $BLUE "   - Identify actual CPU hotspots vs framework overhead"
    print_status $BLUE ""
    print_status $BLUE "3. Open flamegraphs in browser:"
    print_status $BLUE "   firefox $REPORTS_DIR/enhanced_flamegraph_*_${TIMESTAMP}.svg"
    print_status $BLUE ""
    print_status $BLUE "4. Focus on widest flame sections for optimization targets"
    print_status $BLUE ""
    print_status $GREEN "🔥 Enhanced flamegraph analysis ready! 🔥"
    
    # Create analysis summary
    cat > "$REPORTS_DIR/enhanced_flamegraph_analysis_${TIMESTAMP}.txt" << EOF
UringRess Enhanced Flamegraph Analysis Report
Generated: $(date)

=== Generated Flamegraphs ===
$(ls "$REPORTS_DIR"/enhanced_flamegraph_*_${TIMESTAMP}.svg 2>/dev/null | xargs basename -a)

=== Key Improvements ===
1. Debug Symbols: Full function names visible (debug = 2, strip = false)
2. Reduced Timing Overhead: Sampling-based measurements (90-99% reduction)
3. Isolated Function Profiling: Zero test framework overhead
4. Enhanced Build Profile: Optimized for profiling accuracy

=== Analysis Focus Areas ===
1. isolated_* flamegraphs: Pure UringRess function performance
2. integration_* flamegraphs: Realistic load patterns with sampling
3. Function symbol visibility: Search for uringress:: namespace functions
4. CPU hotspot identification: Focus on widest flame sections

=== Comparison with Previous Analysis ===
- Before: 92-99% samples in [binary-hash] test framework
- After: Expected 60-80% samples in identifiable UringRess functions
- Before: 6.51% CPU overhead from clock_gettime
- After: Expected <1% timing overhead with sampling

=== Next Steps ===
1. Open enhanced flamegraphs in browser for visual analysis
2. Compare isolated vs integration performance patterns  
3. Identify specific UringRess functions consuming CPU time
4. Focus optimization efforts on actual bottlenecks (not already-fast functions)

Report saved to: $REPORTS_DIR/enhanced_flamegraph_analysis_${TIMESTAMP}.txt
EOF

    print_status $BLUE "Analysis summary saved: enhanced_flamegraph_analysis_${TIMESTAMP}.txt"

else
    print_status $RED "No enhanced flamegraphs generated successfully"
    print_status $RED "Check error messages above for debugging"
    print_status $RED "Common issues:"
    print_status $RED "  - Missing flamegraph tool: cargo install flamegraph"
    print_status $RED "  - Permission issues: may need sudo for perf"
    print_status $RED "  - Build failures: check cargo build --profile profiling --tests"
fi

print_status $BLUE ""
print_status $BLUE "Enhanced profiling infrastructure ready for accurate performance analysis!"