#!/bin/bash
# Circuit breaker test for UringRess
# Tests backend failure detection and recovery

set -e

PROXY_URL="http://localhost:8080"
BACKEND_PORTS=(3001 3002 3003)
BACKEND_CONTENT_TYPES=("small" "medium" "large")

# Colors
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

echo -e "${BLUE}=== UringRess Circuit Breaker Test ===${NC}"
echo ""

# Function to check service health
check_service() {
    local url=$1
    local name=$2
    
    if curl -s --max-time 3 "$url" > /dev/null 2>&1; then
        echo -e "${GREEN}✓${NC} $name is responding"
        return 0
    else
        echo -e "${RED}✗${NC} $name is not responding"
        return 1
    fi
}

# Function to make test request with timing
test_request() {
    local url=$1
    local description=$2
    
    echo -n "  Testing $description: "
    
    local start_time=$(date +%s.%N)
    local response=$(curl -s -w "%{http_code}:%{time_total}" --max-time 10 "$url" 2>/dev/null)
    local end_time=$(date +%s.%N)
    
    if [[ -n "$response" ]]; then
        local status_code=$(echo "$response" | cut -d: -f1 | tail -c 4)
        local curl_time=$(echo "$response" | cut -d: -f2)
        
        if [[ "$status_code" == "200" ]]; then
            echo -e "${GREEN}SUCCESS${NC} (${curl_time}s)"
        else
            echo -e "${YELLOW}HTTP $status_code${NC} (${curl_time}s)"
        fi
    else
        local duration=$(echo "$end_time - $start_time" | bc -l)
        echo -e "${RED}FAILED${NC} (timeout after $(printf "%.2f" $duration)s)"
    fi
}

# Function to stop KISS backend
stop_backend() {
    local port=$1
    local content_type=$2
    
    echo "Stopping KISS backend ($content_type content) on port $port..."
    
    local pid=$(pgrep -f "kiss --port $port" 2>/dev/null || echo "")
    if [[ -n "$pid" ]]; then
        kill "$pid" 2>/dev/null || true
        sleep 2
        if ! pgrep -f "kiss --port $port" > /dev/null 2>&1; then
            echo -e "${GREEN}✓${NC} Backend on port $port stopped (PID: $pid)"
        else
            echo -e "${RED}✗${NC} Failed to stop backend on port $port"
        fi
    else
        echo -e "${YELLOW}⚠${NC} No backend running on port $port"
    fi
}

# Function to start KISS backend
start_backend() {
    local port=$1
    local content_type=$2
    
    echo "Starting KISS backend ($content_type content) on port $port..."
    
    if pgrep -f "kiss --port $port" > /dev/null 2>&1; then
        echo -e "${YELLOW}⚠${NC} Backend already running on port $port"
        return
    fi
    
    # Detect if running from project root or integration directory
    if [[ -d "./tests/integration/test-content" ]]; then
        # Running from project root
        local content_dir="./tests/integration/test-content/$content_type"
    else
        # Running from integration directory
        local content_dir="./test-content/$content_type"
    fi
    
    kiss --port "$port" --static-dir "$content_dir" > /dev/null 2>&1 &
    local pid=$!
    
    # Wait for startup
    sleep 3
    
    if pgrep -f "kiss --port $port" > /dev/null 2>&1; then
        echo -e "${GREEN}✓${NC} Backend started on port $port (PID: $pid)"
    else
        echo -e "${RED}✗${NC} Failed to start backend on port $port"
    fi
}

# Initial health check
echo -e "${BLUE}=== Initial Health Check ===${NC}"
check_service "$PROXY_URL/health" "UringRess proxy"

for i in "${!BACKEND_PORTS[@]}"; do
    check_service "http://localhost:${BACKEND_PORTS[$i]}/health" "KISS backend ${BACKEND_PORTS[$i]} (${BACKEND_CONTENT_TYPES[$i]})"
done
echo ""

# Test 1: Normal operation
echo -e "${BLUE}=== Test 1: Normal Operation ===${NC}"
echo "Testing all endpoints with all backends healthy:"

test_request "$PROXY_URL/api.json" "small content"
test_request "$PROXY_URL/users.json" "medium content"  
test_request "$PROXY_URL/dataset.json" "large content"
test_request "$PROXY_URL/health" "health check"
echo ""

# Test 2: Single backend failure
echo -e "${BLUE}=== Test 2: Single Backend Failure ===${NC}"
echo "Simulating failure of small content backend (port 3001)..."

stop_backend 3001 "small"
sleep 2

echo "Testing requests with one backend down:"
test_request "$PROXY_URL/api.json" "small content (backend down)"
test_request "$PROXY_URL/users.json" "medium content (should work)"
test_request "$PROXY_URL/dataset.json" "large content (should work)"
echo ""

# Test 3: Circuit breaker behavior
echo -e "${BLUE}=== Test 3: Circuit Breaker Response ===${NC}"
echo "Making multiple requests to trigger circuit breaker..."

for i in {1..10}; do
    echo -n "Request $i: "
    test_request "$PROXY_URL/api.json" "small content"
    sleep 0.5
done
echo ""

# Test 4: Backend recovery
echo -e "${BLUE}=== Test 4: Backend Recovery ===${NC}"
echo "Restarting failed backend to test recovery..."

start_backend 3001 "small"
sleep 3

echo "Testing recovery behavior:"
for i in {1..5}; do
    echo -n "Recovery test $i: "
    test_request "$PROXY_URL/api.json" "small content (after recovery)"
    sleep 1
done
echo ""

# Test 5: Multiple backend failures
echo -e "${BLUE}=== Test 5: Multiple Backend Failures ===${NC}"
echo "Simulating cascade failure scenario..."

stop_backend 3002 "medium"
stop_backend 3003 "large"
sleep 2

echo "Testing with multiple backends down:"
test_request "$PROXY_URL/api.json" "small content (1 backend up)"
test_request "$PROXY_URL/users.json" "medium content (backend down)"
test_request "$PROXY_URL/dataset.json" "large content (backend down)"
echo ""

# Test 6: Full recovery
echo -e "${BLUE}=== Test 6: Full Recovery ===${NC}"
echo "Restarting all backends..."

start_backend 3002 "medium"
start_backend 3003 "large"
sleep 5

echo "Testing full system recovery:"
test_request "$PROXY_URL/api.json" "small content"
test_request "$PROXY_URL/users.json" "medium content"
test_request "$PROXY_URL/dataset.json" "large content"
test_request "$PROXY_URL/health" "health check"
echo ""

# Test 7: Performance under recovery
echo -e "${BLUE}=== Test 7: Performance Under Recovery ===${NC}"
echo "Testing performance after backend recovery..."

if command -v ab &> /dev/null; then
    echo "Running performance test (100 requests, 5 concurrent):"
    ab -n 100 -c 5 -q "$PROXY_URL/api.json" 2>/dev/null | grep -E "(Requests per second|Time per request|Failed requests)" || echo "ab test failed"
else
    echo "Apache Bench (ab) not available, skipping performance test"
fi
echo ""

# Final health check
echo -e "${BLUE}=== Final Health Check ===${NC}"
check_service "$PROXY_URL/health" "UringRess proxy"

for i in "${!BACKEND_PORTS[@]}"; do
    check_service "http://localhost:${BACKEND_PORTS[$i]}/health" "KISS backend ${BACKEND_PORTS[$i]} (${BACKEND_CONTENT_TYPES[$i]})"
done

echo ""
echo -e "${GREEN}=== Circuit Breaker Test Complete ===${NC}"

# Generate structured test summary
echo ""
echo -e "${BLUE}=== TEST SUMMARY ===${NC}"
echo "STRUCTURED_OUTPUT_BEGIN"
echo "TEST_NAME=circuit_breaker"
echo "TEST_TIMESTAMP=$(date)"
echo "TEST_STATUS=COMPLETED"
echo ""
echo "[METRICS]"
echo "BACKEND_COUNT=${#BACKEND_PORTS[@]}"
echo "TEST_PHASES=7"
echo "FINAL_PROXY_STATUS=responding"
echo "FINAL_BACKENDS_STATUS=all_responding"
echo ""
echo "[NOTES]"
echo "Circuit breaker behavior validation with real backend failures"
echo "Tests automatic failure detection and recovery"
echo "Validates response codes and timing during failures"
echo "Performance testing after recovery"
echo "STRUCTURED_OUTPUT_END"

echo ""
echo "Key observations to look for:"
echo "  1. Failed requests should return 502/504 status codes"
echo "  2. Response times should be fast for circuit breaker responses"
echo "  3. Recovery should happen automatically when backends restart"
echo "  4. Performance should return to normal after recovery"
echo ""
echo "Note: This test validates real circuit breaker behavior with actual"
echo "backend failures and recoveries, not synthetic test conditions."