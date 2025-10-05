#!/bin/bash
# Realistic performance test for UringRess
# Measures actual end-to-end performance with real backends

set -e

PROXY_URL="http://localhost:8080"
WARMUP_REQUESTS=100
TEST_DURATION=30

# Colors
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

echo -e "${BLUE}=== UringRess Realistic Performance Test ===${NC}"
echo ""

# Function to check dependencies
check_dependencies() {
    local missing_tools=()
    
    for tool in curl ab wrk bc; do
        if ! command -v "$tool" &> /dev/null; then
            missing_tools+=("$tool")
        fi
    done
    
    if [[ ${#missing_tools[@]} -gt 0 ]]; then
        echo -e "${YELLOW}Warning: Missing tools: ${missing_tools[*]}${NC}"
        echo "Some tests may be skipped. Install them for full testing."
        echo ""
    fi
}

# Function to check UringRess availability
check_uringress() {
    echo "Checking UringRess availability..."
    
    if ! curl -s --max-time 3 "$PROXY_URL/health" > /dev/null; then
        echo -e "${RED}ERROR: UringRess not responding at $PROXY_URL${NC}"
        echo "Start the test environment:"
        echo "  From project root: tests/integration/start_uringress_test_environment.sh start"
        echo "  From integration dir: ./start_uringress_test_environment.sh start"
        exit 1
    fi
    
    echo -e "${GREEN}✓ UringRess is responding${NC}"
    echo ""
}

# Function to run warmup
run_warmup() {
    echo -e "${BLUE}=== Warmup Phase ===${NC}"
    echo "Running $WARMUP_REQUESTS warmup requests to stabilize performance..."
    
    for endpoint in "api.json" "users.json" "health"; do
        echo -n "  Warming up $endpoint: "
        local success=0
        for ((i=1; i<=WARMUP_REQUESTS/3; i++)); do
            if curl -s --max-time 3 "$PROXY_URL/$endpoint" > /dev/null 2>&1; then
                success=$((success + 1))
            fi
        done
        echo "$success/$(($WARMUP_REQUESTS/3)) successful"
    done
    echo -e "${GREEN}✓ Warmup completed${NC}"
    echo ""
}

# Function to test latency
test_latency() {
    echo -e "${BLUE}=== Latency Test ===${NC}"
    echo "Measuring request latency for different content sizes..."
    
    # Test small content (API response)
    echo -n "Small content (api.json): "
    local times=()
    for ((i=1; i<=20; i++)); do
        local time_output=$(curl -s -w "%{time_total}" --max-time 5 "$PROXY_URL/api.json" -o /dev/null 2>/dev/null)
        if [[ -n "$time_output" ]]; then
            times+=("$time_output")
        fi
    done
    
    if [[ ${#times[@]} -gt 0 ]]; then
        local avg_time=$(printf '%s\n' "${times[@]}" | awk '{sum+=$1} END {print sum/NR}')
        echo "${avg_time}s average (${#times[@]}/20 successful)"
    else
        echo -e "${RED}Failed${NC}"
    fi
    
    # Test medium content
    echo -n "Medium content (users.json): "
    times=()
    for ((i=1; i<=10; i++)); do
        local time_output=$(curl -s -w "%{time_total}" --max-time 10 "$PROXY_URL/users.json" -o /dev/null 2>/dev/null)
        if [[ -n "$time_output" ]]; then
            times+=("$time_output")
        fi
    done
    
    if [[ ${#times[@]} -gt 0 ]]; then
        local avg_time=$(printf '%s\n' "${times[@]}" | awk '{sum+=$1} END {print sum/NR}')
        echo "${avg_time}s average (${#times[@]}/10 successful)"
    else
        echo -e "${RED}Failed${NC}"
    fi
    
    # Test large content
    echo -n "Large content (dataset.json): "
    times=()
    for ((i=1; i<=5; i++)); do
        local time_output=$(curl -s -w "%{time_total}" --max-time 15 "$PROXY_URL/dataset.json" -o /dev/null 2>/dev/null)
        if [[ -n "$time_output" ]]; then
            times+=("$time_output")
        fi
    done
    
    if [[ ${#times[@]} -gt 0 ]]; then
        local avg_time=$(printf '%s\n' "${times[@]}" | awk '{sum+=$1} END {print sum/NR}')
        echo "${avg_time}s average (${#times[@]}/5 successful)"
    else
        echo -e "${RED}Failed${NC}"
    fi
    
    echo ""
}

# Function to test throughput with Apache Bench
test_throughput_ab() {
    if ! command -v ab &> /dev/null; then
        echo -e "${YELLOW}Apache Bench (ab) not available, skipping AB throughput test${NC}"
        return
    fi
    
    echo -e "${BLUE}=== Throughput Test (Apache Bench) ===${NC}"
    
    # Test different concurrency levels
    for concurrency in 1 10 25 50; do
        echo "Testing with $concurrency concurrent connections (1000 requests):"
        
        local ab_output=$(timeout 30s ab -n 1000 -c $concurrency -k "$PROXY_URL/api.json" 2>&1)
        local ab_exit_code=$?
        
        if [[ $ab_exit_code -eq 0 ]]; then
            echo "$ab_output" | grep -E "(Requests per second|Time per request|Failed requests|Transfer rate)" | sed 's/^/  /'
        else
            echo -e "  ${RED}Test failed${NC}"
        fi
        echo ""
    done
}

# Function to test throughput with wrk
test_throughput_wrk() {
    if ! command -v wrk &> /dev/null; then
        echo -e "${YELLOW}wrk not available, skipping wrk throughput test${NC}"
        return
    fi
    
    echo -e "${BLUE}=== Throughput Test (wrk) ===${NC}"
    echo "Running sustained load test for ${TEST_DURATION}s with 4 threads, 50 connections:"
    
    wrk -t 4 -c 50 -d ${TEST_DURATION}s --latency "$PROXY_URL/api.json" | sed 's/^/  /'
    echo ""
}

# Function to test mixed workload
test_mixed_workload() {
    echo -e "${BLUE}=== Mixed Workload Test ===${NC}"
    echo "Testing realistic mixed traffic patterns..."
    
    # Create a temporary Lua script for wrk
    cat > /tmp/mixed_workload.lua <<'EOF'
-- Mixed workload script for wrk
local counter = 0
local requests = {
    "/api.json",
    "/users.json", 
    "/health",
    "/api.json",
    "/api.json"  -- Weight API calls higher
}

request = function()
    counter = counter + 1
    local path = requests[(counter % #requests) + 1]
    return wrk.format("GET", path)
end
EOF
    
    if command -v wrk &> /dev/null; then
        echo "Running mixed workload (API:Users:Health = 3:1:1) for 20s:"
        wrk -t 2 -c 20 -d 20s -s /tmp/mixed_workload.lua --latency "$PROXY_URL" | sed 's/^/  /'
        rm -f /tmp/mixed_workload.lua
    else
        echo -e "${YELLOW}wrk not available, simulating mixed workload with curl${NC}"
        
        local start_time=$(date +%s)
        local end_time=$((start_time + 20))
        local request_count=0
        local success_count=0
        
        while [[ $(date +%s) -lt $end_time ]]; do
            local endpoints=("api.json" "users.json" "health" "api.json" "api.json")
            local endpoint=${endpoints[$((request_count % 5))]}
            
            if curl -s --max-time 2 "$PROXY_URL/$endpoint" > /dev/null 2>&1; then
                success_count=$((success_count + 1))
            fi
            request_count=$((request_count + 1))
            
            sleep 0.05  # 20 RPS per process
        done
        
        local actual_duration=$(($(date +%s) - start_time))
        local rps=$(echo "scale=2; $request_count / $actual_duration" | bc -l)
        local success_rate=$(echo "scale=2; $success_count * 100 / $request_count" | bc -l)
        
        echo "  Requests: $request_count"
        echo "  Successful: $success_count"
        echo "  Success rate: ${success_rate}%"
        echo "  RPS: $rps"
    fi
    echo ""
}

# Function to test memory usage
test_memory_usage() {
    echo -e "${BLUE}=== Memory Usage Test ===${NC}"
    echo "Monitoring UringRess memory usage during load..."
    
    local uringress_pid=$(pgrep -f "target/release/uringress" 2>/dev/null || pgrep -f "/uringress --config" 2>/dev/null || echo "")
    if [[ -z "$uringress_pid" ]]; then
        echo -e "${RED}UringRess process not found${NC}"
        return
    fi
    
    # Get initial memory usage
    local initial_memory=$(ps -p "$uringress_pid" -o rss= 2>/dev/null || echo "0")
    echo "Initial memory usage: ${initial_memory} KB"
    
    # Run load test while monitoring memory
    echo "Running load test and monitoring memory..."
    
    if command -v ab &> /dev/null; then
        # Start memory monitoring in background
        (
            for i in {1..30}; do
                local current_memory=$(ps -p "$uringress_pid" -o rss= 2>/dev/null || echo "0")
                echo "$i $current_memory" >> /tmp/memory_usage.log
                sleep 1
            done
        ) &
        local monitor_pid=$!
        
        # Run load test
        timeout 30s ab -n 3000 -c 20 -k "$PROXY_URL/api.json" > /dev/null 2>&1
        
        # Stop monitoring
        kill $monitor_pid 2>/dev/null || true
        wait $monitor_pid 2>/dev/null || true
        
        # Analyze memory usage
        if [[ -f /tmp/memory_usage.log ]]; then
            local max_memory=$(awk '{if($2>max) max=$2} END {print max}' /tmp/memory_usage.log)
            local final_memory=$(tail -1 /tmp/memory_usage.log | awk '{print $2}')
            local memory_increase=$((max_memory - initial_memory))
            
            echo "Maximum memory usage: ${max_memory} KB"
            echo "Final memory usage: ${final_memory} KB"
            echo "Memory increase during load: ${memory_increase} KB"
            
            rm -f /tmp/memory_usage.log
        fi
    else
        echo -e "${YELLOW}Apache Bench not available, skipping memory test${NC}"
    fi
    echo ""
}

# Function to compare with direct backend
test_proxy_overhead() {
    echo -e "${BLUE}=== Proxy Overhead Analysis ===${NC}"
    echo "Comparing direct backend vs proxy performance..."
    
    # Test direct backend
    echo -n "Direct backend (port 3001): "
    local backend_times=()
    for ((i=1; i<=10; i++)); do
        local time_output=$(curl -s -w "%{time_total}" --max-time 5 "http://localhost:3001/api.json" -o /dev/null 2>/dev/null)
        if [[ -n "$time_output" ]]; then
            backend_times+=("$time_output")
        fi
    done
    
    local backend_avg=0
    if [[ ${#backend_times[@]} -gt 0 ]]; then
        backend_avg=$(printf '%s\n' "${backend_times[@]}" | awk '{sum+=$1} END {print sum/NR}')
        echo "${backend_avg}s average"
    else
        echo -e "${RED}Failed${NC}"
    fi
    
    # Test through proxy
    echo -n "Through proxy (port 8080): "
    local proxy_times=()
    for ((i=1; i<=10; i++)); do
        local time_output=$(curl -s -w "%{time_total}" --max-time 5 "$PROXY_URL/api.json" -o /dev/null 2>/dev/null)
        if [[ -n "$time_output" ]]; then
            proxy_times+=("$time_output")
        fi
    done
    
    local proxy_avg=0
    if [[ ${#proxy_times[@]} -gt 0 ]]; then
        proxy_avg=$(printf '%s\n' "${proxy_times[@]}" | awk '{sum+=$1} END {print sum/NR}')
        echo "${proxy_avg}s average"
    else
        echo -e "${RED}Failed${NC}"
    fi
    
    # Calculate overhead
    if [[ ${#backend_times[@]} -gt 0 && ${#proxy_times[@]} -gt 0 ]]; then
        local overhead=$(echo "scale=6; $proxy_avg - $backend_avg" | bc -l)
        local overhead_percent=$(echo "scale=2; ($overhead / $backend_avg) * 100" | bc -l)
        echo "Proxy overhead: ${overhead}s (${overhead_percent}%)"
        
        # Evaluation
        local overhead_ms=$(echo "$overhead * 1000" | bc -l)
        if (( $(echo "$overhead_ms < 1" | bc -l) )); then
            echo -e "${GREEN}✓ EXCELLENT: <1ms proxy overhead${NC}"
        elif (( $(echo "$overhead_ms < 5" | bc -l) )); then
            echo -e "${GREEN}✓ GOOD: <5ms proxy overhead${NC}"
        elif (( $(echo "$overhead_ms < 10" | bc -l) )); then
            echo -e "${YELLOW}⚠ ACCEPTABLE: <10ms proxy overhead${NC}"
        else
            echo -e "${RED}✗ HIGH: >10ms proxy overhead${NC}"
        fi
    fi
    echo ""
}

# Main test execution
main() {
    check_dependencies
    check_uringress
    run_warmup
    test_latency
    test_throughput_ab
    test_throughput_wrk
    test_mixed_workload
    test_memory_usage
    test_proxy_overhead
    
    echo -e "${GREEN}=== Realistic Performance Test Complete ===${NC}"
}

main "$@"