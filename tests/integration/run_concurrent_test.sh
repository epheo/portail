#!/bin/bash
# Concurrent connection test for Portail
# Tests how well the proxy handles multiple simultaneous connections

set -e

# Configuration
CONCURRENT_WORKERS=20
REQUESTS_PER_WORKER=50
PROXY_URL="http://localhost:8080"
TEST_ENDPOINTS=("api.json" "users.json" "health")

# Colors
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

echo -e "${BLUE}=== Portail Concurrent Connection Test ===${NC}"
echo "Configuration:"
echo "  Workers: $CONCURRENT_WORKERS"
echo "  Requests per worker: $REQUESTS_PER_WORKER"
echo "  Total requests: $((CONCURRENT_WORKERS * REQUESTS_PER_WORKER))"
echo "  Endpoints: ${TEST_ENDPOINTS[*]}"
echo ""

# Function to run worker
run_worker() {
    local worker_id=$1
    local requests=$2
    local start_time=$(date +%s.%N)
    local success_count=0
    local error_count=0
    
    for ((i=1; i<=requests; i++)); do
        local endpoint=${TEST_ENDPOINTS[$((RANDOM % ${#TEST_ENDPOINTS[@]}))]}
        local url="${PROXY_URL}/${endpoint}"

        local status_code=$(curl -s -o /dev/null -w "%{http_code}" --max-time 2 "$url" 2>/dev/null)
        if [[ "$status_code" == "200" ]]; then
            success_count=$((success_count + 1))
        else
            error_count=$((error_count + 1))
        fi
    done
    
    local end_time=$(date +%s.%N)
    local duration=$(echo "$end_time - $start_time" | bc -l)
    local rps=$(echo "scale=2; $requests / $duration" | bc -l)
    
    echo "Worker $worker_id: $success_count success, $error_count errors, ${rps} RPS"
    
    # Return success count for aggregation
    echo "$success_count" > /tmp/worker_${worker_id}_result
}

# Check if Portail is responding
echo "Checking Portail availability..."
if ! curl -s --max-time 3 "${PROXY_URL}/health" > /dev/null; then
    echo -e "${RED}ERROR: Portail not responding at ${PROXY_URL}${NC}"
    echo "Make sure the test environment is running:"
    echo "  From project root: tests/integration/start_portail_test_environment.sh start"
    echo "  From integration dir: ./start_portail_test_environment.sh start"
    exit 1
fi
echo -e "${GREEN}✓ Portail is responding${NC}"
echo ""

# Start concurrent workers
echo -e "${BLUE}Starting $CONCURRENT_WORKERS concurrent workers...${NC}"
start_time=$(date +%s.%N)

# Start all workers in background
for worker_id in $(seq 1 $CONCURRENT_WORKERS); do
    run_worker $worker_id $REQUESTS_PER_WORKER &
done

# Wait for all workers to complete
echo "Waiting for all workers to complete..."
wait

end_time=$(date +%s.%N)
total_duration=$(echo "$end_time - $start_time" | bc -l)

# Aggregate results
total_success=0
total_requests=$((CONCURRENT_WORKERS * REQUESTS_PER_WORKER))

for worker_id in $(seq 1 $CONCURRENT_WORKERS); do
    if [[ -f /tmp/worker_${worker_id}_result ]]; then
        worker_success=$(cat /tmp/worker_${worker_id}_result)
        total_success=$((total_success + worker_success))
        rm -f /tmp/worker_${worker_id}_result
    fi
done

total_errors=$((total_requests - total_success))
success_rate=$(echo "scale=2; $total_success * 100 / $total_requests" | bc -l)
overall_rps=$(echo "scale=2; $total_requests / $total_duration" | bc -l)

# Results
echo ""
echo -e "${BLUE}=== Test Results ===${NC}"
echo "Total requests: $total_requests"
echo "Successful: $total_success"
echo "Errors: $total_errors"
echo "Success rate: ${success_rate}%"
echo "Total duration: $(printf "%.2f" $total_duration)s"
echo "Overall RPS: $overall_rps"
echo ""

# Evaluation
if (( $(echo "$success_rate >= 95" | bc -l) )); then
    echo -e "${GREEN}✓ EXCELLENT: >95% success rate under concurrent load${NC}"
elif (( $(echo "$success_rate >= 90" | bc -l) )); then
    echo -e "${YELLOW}⚠ GOOD: >90% success rate under concurrent load${NC}"
elif (( $(echo "$success_rate >= 80" | bc -l) )); then
    echo -e "${YELLOW}⚠ FAIR: >80% success rate under concurrent load${NC}"
else
    echo -e "${RED}✗ POOR: <80% success rate under concurrent load${NC}"
fi

if (( $(echo "$overall_rps >= 1000" | bc -l) )); then
    echo -e "${GREEN}✓ HIGH THROUGHPUT: >1000 RPS achieved${NC}"
elif (( $(echo "$overall_rps >= 500" | bc -l) )); then
    echo -e "${YELLOW}⚠ MODERATE THROUGHPUT: >500 RPS achieved${NC}"
else
    echo -e "${RED}✗ LOW THROUGHPUT: <500 RPS achieved${NC}"
fi

