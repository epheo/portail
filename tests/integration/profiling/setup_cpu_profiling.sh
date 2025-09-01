#!/bin/bash
# CPU Profiling Setup for UringRess
# Sets up flamegraph and perf tools for CPU performance analysis

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Function to print colored output
print_status() {
    local color=$1
    local message=$2
    echo -e "${color}${message}${NC}"
}

print_status $BLUE "=== CPU Profiling Setup ==="

# Detect script location and set paths
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
REPORTS_DIR="$SCRIPT_DIR/../reports/cpu"

# Create reports directory
mkdir -p "$REPORTS_DIR"

print_status $YELLOW "Checking dependencies..."

# Check for perf
if ! command -v perf &> /dev/null; then
    print_status $RED "ERROR: perf not found. Install with:"
    print_status $RED "  Ubuntu/Debian: sudo apt-get install linux-perf"
    print_status $RED "  RHEL/Fedora: sudo yum install perf"
    exit 1
fi

# Check for cargo flamegraph
if ! cargo flamegraph --help &> /dev/null; then
    print_status $YELLOW "Installing cargo flamegraph..."
    cargo install flamegraph
fi

# Check for Rust debug symbols capability
if ! rustc --print target-list | grep -q "$(rustc --version --verbose | grep host | cut -d' ' -f2)"; then
    print_status $RED "ERROR: Cannot determine Rust target"
    exit 1
fi

print_status $GREEN "✓ Dependencies OK"

print_status $YELLOW "Checking system permissions..."

# Check if we can use perf
if ! perf --version >/dev/null 2>&1; then
    print_status $RED "ERROR: Cannot run perf. You may need to:"
    print_status $RED "  1. Run with sudo"
    print_status $RED "  2. Set kernel.perf_event_paranoid: sudo sysctl kernel.perf_event_paranoid=1"
    print_status $RED "  3. Add user to perf_users group"
    exit 1
fi

# Check kernel perf_event_paranoid setting
PERF_PARANOID=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo "unknown")
if [ "$PERF_PARANOID" -gt 1 ]; then
    print_status $YELLOW "WARNING: kernel.perf_event_paranoid=$PERF_PARANOID (should be ≤1 for best results)"
    print_status $YELLOW "Consider running: sudo sysctl kernel.perf_event_paranoid=1"
fi

print_status $GREEN "✓ System permissions OK"

print_status $YELLOW "Building project with debug symbols..."

# Build project in release mode with debug symbols for profiling
cd "$PROJECT_ROOT"
export CARGO_PROFILE_RELEASE_DEBUG=true
cargo build --release

print_status $GREEN "✓ Build complete with debug symbols"

print_status $BLUE "CPU profiling setup complete!"
print_status $BLUE "Available profiling commands:"
print_status $BLUE "  - Basic flamegraph: ./run_flamegraph_analysis.sh"
print_status $BLUE "  - Detailed perf analysis: ./run_detailed_cpu_analysis.sh"
print_status $BLUE "  - Benchmark profiling: ./profile_benchmarks.sh"

print_status $BLUE "Reports will be saved to: $REPORTS_DIR"