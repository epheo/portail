#!/bin/bash
# UringRess tmux Test Environment
# Realistic performance testing using KISS backends

set -e

SESSION_NAME="uringress-test"
TMUX_CONF="/tmp/uringress-test.conf"
KISS_BINARY="kiss"

# Detect script location and set absolute paths
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [[ "$SCRIPT_DIR" == */tests/integration ]]; then
    # Script is in integration directory
    PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
else
    # Legacy: running from project root directly (shouldn't happen now)
    PROJECT_ROOT="$SCRIPT_DIR"
fi

# Use absolute paths for all binaries and directories
URINGRESS_BINARY="$PROJECT_ROOT/target/release/uringress"
TEST_CONTENT_DIR="$SCRIPT_DIR/test-content"
INTEGRATION_DIR="$SCRIPT_DIR"

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

# tmux configuration for our test session
create_tmux_config() {
    cat > ${TMUX_CONF} <<EOF
# Status bar config for monitoring
set -g status-interval 1
set -g status-left-length 30
set -g status-right-length 150
set -g status-left '#[fg=green]#S #[fg=yellow]#I:#P '
set -g status-right '#[fg=cyan]CPU: #(top -bn1 | grep "Cpu(s)" | cut -d "," -f1 | cut -d ":" -f2 | xargs)#[default] | #[fg=green]%H:%M:%S'

# Window titles
set -g automatic-rename on
set -g allow-rename on

# Easy pane navigation
bind -n M-h select-pane -L
bind -n M-l select-pane -R
bind -n M-k select-pane -U
bind -n M-j select-pane -D

# Resize panes
bind -n M-H resize-pane -L 5
bind -n M-L resize-pane -R 5
bind -n M-K resize-pane -U 5
bind -n M-J resize-pane -D 5
EOF
}

# Function to check dependencies
check_dependencies() {
    print_status $BLUE "Checking dependencies..."
    
    # Check tmux
    if ! command -v tmux &> /dev/null; then
        print_status $RED "ERROR: tmux not found. Please install tmux."
        exit 1
    fi
    
    # Check KISS binary
    if ! command -v ${KISS_BINARY} &> /dev/null; then
        print_status $RED "ERROR: KISS binary not found in PATH"
        print_status $YELLOW "Please ensure KISS is built and available as '${KISS_BINARY}' command"
        print_status $YELLOW "You can download it from: https://github.com/epheo/kiss"
        exit 1
    fi
    
    # Check UringRess binary
    if [[ ! -f ${URINGRESS_BINARY} ]]; then
        print_status $RED "ERROR: UringRess binary not found at ${URINGRESS_BINARY}"
        print_status $YELLOW "Please build UringRess first: cargo build --release"
        exit 1
    fi
    
    # Check other required tools
    for tool in curl ab htop ss jq; do
        if ! command -v $tool &> /dev/null; then
            print_status $YELLOW "WARNING: $tool not found. Some tests may not work."
        fi
    done
    
    print_status $GREEN "✓ Dependencies check completed"
}

# Function to setup test content
setup_test_content() {
    print_status $BLUE "Setting up test content directories..."
    
    # Create content directories
    mkdir -p ${TEST_CONTENT_DIR}/{small,medium,large}
    
    # Small responses (< 1KB) - API-like responses
    cat > ${TEST_CONTENT_DIR}/small/api.json <<EOF
{
  "status": "ok",
  "service": "api",
  "timestamp": "$(date -Iseconds)",
  "data": {
    "id": 1,
    "name": "test-service",
    "version": "1.0.0"
  }
}
EOF
    
    cat > ${TEST_CONTENT_DIR}/small/health <<EOF
{
  "status": "healthy",
  "uptime": "1h",
  "version": "1.0.0",
  "checks": {
    "database": "ok",
    "cache": "ok",
    "memory": "ok"
  }
}
EOF
    
    echo '<html><head><title>Backend 1 - Small</title></head><body><h1>Backend 1 (Port 3001)</h1><p>Small test page served through UringRess proxy</p><p style="color: blue;">This is served by KISS backend #1</p></body></html>' > ${TEST_CONTENT_DIR}/small/index.html
    
    # Medium responses (1-10KB) - Typical web pages/API responses
    cat > ${TEST_CONTENT_DIR}/medium/users.json <<EOF
{
  "users": [
$(for i in {1..50}; do echo "    {\"id\": $i, \"name\": \"user$i\", \"email\": \"user$i@example.com\", \"active\": $([ $((i % 2)) -eq 0 ] && echo true || echo false)}$([ $i -lt 50 ] && echo ',')"; done)
  ],
  "meta": {
    "total": 50,
    "page": 1,
    "per_page": 50,
    "has_more": false
  }
}
EOF
    
    cat > ${TEST_CONTENT_DIR}/medium/page.html <<EOF
<!DOCTYPE html>
<html>
<head>
    <title>Backend 2 - Medium</title>
    <style>
        body { font-family: Arial, sans-serif; margin: 40px; }
        .content { max-width: 800px; margin: 0 auto; }
        .data { background: #f5f5f5; padding: 20px; margin: 20px 0; }
        .backend-info { background: #e8f5e8; border: 2px solid green; padding: 15px; margin: 20px 0; }
    </style>
</head>
<body>
    <div class="content">
        <h1>Backend 2 - Medium Content Test Page</h1>
        <div class="backend-info">
            <h2>Backend Information</h2>
            <p><strong>Backend:</strong> #2 (Port 3002)</p>
            <p><strong>Content Type:</strong> Medium-sized responses</p>
        </div>
        <p>This page tests medium-sized responses through the UringRess proxy.</p>
        <div class="data">
$(for i in {1..20}; do echo "            <p>Sample data line $i with some content to increase page size.</p>"; done)
        </div>
    </div>
</body>
</html>
EOF
    
    echo '<html><head><title>Backend 2 - Medium</title></head><body><h1>Backend 2 (Port 3002)</h1><p>Medium test page served through UringRess proxy</p><p style="color: green;">This is served by KISS backend #2</p></body></html>' > ${TEST_CONTENT_DIR}/medium/index.html
    
    # Large responses (100KB+) - File downloads, large datasets
    cat > ${TEST_CONTENT_DIR}/large/dataset.json <<EOF
{
  "dataset": "large_test_data",
  "description": "Large dataset for testing proxy performance with big responses",
  "data": "$(head -c 80000 /dev/urandom | base64 -w 0)",
  "size_bytes": 80000,
  "generated_at": "$(date -Iseconds)"
}
EOF
    
    # Create a large text file
    cat > ${TEST_CONTENT_DIR}/large/large-file.txt <<EOF
Large File Content Test
=======================

This file contains a significant amount of text data to test how UringRess handles
large response streaming and memory usage.

$(for i in {1..1000}; do echo "Line $i: This is a test line with some content to make the file larger. Lorem ipsum dolor sit amet, consectetur adipiscing elit."; done)

End of large file.
EOF
    
    echo '<html><head><title>Backend 3 - Large</title></head><body><h1>Backend 3 (Port 3003)</h1><p>Large test page served through UringRess proxy</p><p style="color: red;">This is served by KISS backend #3</p></body></html>' > ${TEST_CONTENT_DIR}/large/index.html
    
    print_status $GREEN "✓ Test content created successfully"
    
    # Show content sizes
    print_status $BLUE "Content sizes:"
    du -sh ${TEST_CONTENT_DIR}/* | while read size dir; do
        echo "  $(basename $dir): $size"
    done
}

# Function to create tmux session with multiple panes
create_tmux_session() {
    print_status $BLUE "Creating tmux session: ${SESSION_NAME}"
    
    # Kill existing session if it exists
    tmux kill-session -t ${SESSION_NAME} 2>/dev/null || true
    
    # Create tmux config
    create_tmux_config
    
    # Change to project root for consistent paths
    cd "$PROJECT_ROOT"
    
    # Create new session with first window for monitoring
    tmux -f ${TMUX_CONF} new-session -d -s ${SESSION_NAME} -n "monitor"
    
    # Split monitor window into 4 panes for system monitoring
    tmux split-window -h -t ${SESSION_NAME}:monitor
    tmux split-window -v -t ${SESSION_NAME}:monitor.0
    tmux split-window -v -t ${SESSION_NAME}:monitor.2
    
    # Setup monitoring commands
    tmux send-keys -t ${SESSION_NAME}:monitor.0 "echo 'System Monitor' && htop" Enter
    tmux send-keys -t ${SESSION_NAME}:monitor.1 "echo 'Network Connections' && watch -n1 'ss -tlnp | grep -E \"(8080|300[123])\"'" Enter
    tmux send-keys -t ${SESSION_NAME}:monitor.2 "echo 'Process Monitor' && watch -n1 'ps aux | grep -E \"(uringress|kiss)\" | head -10'" Enter
    tmux send-keys -t ${SESSION_NAME}:monitor.3 "echo 'UringRess Logs' && echo 'Waiting for UringRess to start...' && sleep 2 && tail -f /tmp/uringress-test.log 2>/dev/null || echo 'Log file not yet created'" Enter
    
    # Create KISS backends window
    tmux new-window -t ${SESSION_NAME} -n "kiss-backends"
    tmux split-window -h -t ${SESSION_NAME}:kiss-backends
    tmux split-window -v -t ${SESSION_NAME}:kiss-backends.0
    
    # Start KISS backends in separate panes
    tmux send-keys -t ${SESSION_NAME}:kiss-backends.0 "echo 'KISS Backend 1 - Small Content (Port 3001)' && echo 'Starting...' && ${KISS_BINARY} --port 3001 --static-dir ${TEST_CONTENT_DIR}/small" Enter
    tmux send-keys -t ${SESSION_NAME}:kiss-backends.1 "echo 'KISS Backend 2 - Medium Content (Port 3002)' && echo 'Starting...' && ${KISS_BINARY} --port 3002 --static-dir ${TEST_CONTENT_DIR}/medium" Enter  
    tmux send-keys -t ${SESSION_NAME}:kiss-backends.2 "echo 'KISS Backend 3 - Large Content (Port 3003)' && echo 'Starting...' && ${KISS_BINARY} --port 3003 --static-dir ${TEST_CONTENT_DIR}/large" Enter
    
    # Create UringRess window
    tmux new-window -t ${SESSION_NAME} -n "uringress"
    
    # Wait for KISS backends to start
    print_status $BLUE "Waiting for KISS backends to start..."
    sleep 5
    
    # Verify KISS backends are running
    local all_ready=true
    for port in 3001 3002 3003; do
        if curl -s --connect-timeout 2 http://localhost:${port}/health > /dev/null 2>&1; then
            print_status $GREEN "✓ KISS backend on port ${port} is ready"
        else
            print_status $RED "✗ KISS backend on port ${port} not responding"
            all_ready=false
        fi
    done
    
    if [[ ${all_ready} != true ]]; then
        print_status $YELLOW "Some backends are not ready, but continuing..."
    fi
    
    # Start UringRess
    print_status $BLUE "Starting UringRess..."
    tmux send-keys -t ${SESSION_NAME}:uringress "echo 'UringRess Proxy Server' && echo 'Starting with configuration from development.yaml...' && cd '$PROJECT_ROOT' && '$URINGRESS_BINARY' --config '$PROJECT_ROOT/examples/development.yaml' 2>&1 | tee /tmp/uringress-test.log" Enter
    
    # Create testing window
    tmux new-window -t ${SESSION_NAME} -n "testing"
    tmux split-window -h -t ${SESSION_NAME}:testing
    
    # Setup testing panes
    tmux send-keys -t ${SESSION_NAME}:testing.0 "echo 'Manual Testing Pane'" Enter
    tmux send-keys -t ${SESSION_NAME}:testing.0 "echo 'Available endpoints:'" Enter
    tmux send-keys -t ${SESSION_NAME}:testing.0 "echo '  Small:  curl http://localhost:8080/api.json'" Enter
    tmux send-keys -t ${SESSION_NAME}:testing.0 "echo '  Medium: curl http://localhost:8080/users.json'" Enter
    tmux send-keys -t ${SESSION_NAME}:testing.0 "echo '  Large:  curl http://localhost:8080/dataset.json'" Enter
    tmux send-keys -t ${SESSION_NAME}:testing.0 "echo '  Health: curl http://localhost:8080/health'" Enter
    tmux send-keys -t ${SESSION_NAME}:testing.0 "echo ''" Enter
    
    tmux send-keys -t ${SESSION_NAME}:testing.1 "echo 'Automated Testing Pane'" Enter
    tmux send-keys -t ${SESSION_NAME}:testing.1 "echo 'Run: tests/integration/start_uringress_test_environment.sh test'" Enter
    tmux send-keys -t ${SESSION_NAME}:testing.1 "echo ''" Enter
    
    # Select the monitoring window by default
    tmux select-window -t ${SESSION_NAME}:monitor
    
    print_status $GREEN "✓ tmux session created successfully!"
    print_status $BLUE ""
    print_status $BLUE "Session layout:"
    print_status $BLUE "  - Window 0 (monitor): System monitoring (htop, network, processes, logs)"
    print_status $BLUE "  - Window 1 (kiss-backends): Three KISS backend services"
    print_status $BLUE "  - Window 2 (uringress): UringRess proxy service"  
    print_status $BLUE "  - Window 3 (testing): Manual and automated testing"
    print_status $BLUE ""
    print_status $GREEN "To attach: tmux attach-session -t ${SESSION_NAME}"
    print_status $BLUE "To detach: Ctrl+b, then d"
    print_status $BLUE "To switch windows: Ctrl+b, then window number (0-3)"
    print_status $BLUE "To switch panes: Alt+h/j/k/l"
}

# Function to run automated tests
run_automated_tests() {
    local session_target="${SESSION_NAME}:testing.1"
    
    print_status $BLUE "Running automated tests..."
    
    # Check if session exists
    if ! tmux has-session -t ${SESSION_NAME} 2>/dev/null; then
        print_status $RED "ERROR: tmux session '${SESSION_NAME}' not found. Start the environment first."
        exit 1
    fi
    
    # Wait for UringRess to start
    print_status $BLUE "Waiting for UringRess to be ready..."
    local retries=0
    while ! curl -s --connect-timeout 2 http://localhost:8080/health > /dev/null 2>&1; do
        sleep 2
        retries=$((retries + 1))
        if [[ $retries -gt 15 ]]; then
            print_status $RED "ERROR: UringRess not responding after 30 seconds"
            return 1
        fi
    done
    
    print_status $GREEN "✓ UringRess is ready"
    
    # Clear testing pane and start tests
    tmux send-keys -t ${session_target} "clear" Enter
    
    # Test 1: Health check
    tmux send-keys -t ${session_target} "echo '=== Test 1: Health checks ==='" Enter
    tmux send-keys -t ${session_target} "curl -s -w 'Status: %{http_code}, Time: %{time_total}s\\n' http://localhost:8080/health" Enter
    sleep 3
    
    # Test 2: Small content through proxy
    tmux send-keys -t ${session_target} "echo ''" Enter
    tmux send-keys -t ${session_target} "echo '=== Test 2: Small content proxy ==='" Enter
    tmux send-keys -t ${session_target} "curl -s -w 'Status: %{http_code}, Time: %{time_total}s, Size: %{size_download} bytes\\n' http://localhost:8080/api.json" Enter
    sleep 3
    
    # Test 3: Medium content
    tmux send-keys -t ${session_target} "echo ''" Enter
    tmux send-keys -t ${session_target} "echo '=== Test 3: Medium content proxy ==='" Enter
    tmux send-keys -t ${session_target} "curl -s -w 'Status: %{http_code}, Time: %{time_total}s, Size: %{size_download} bytes\\n' http://localhost:8080/users.json > /dev/null" Enter
    sleep 3
    
    # Test 4: Large content
    tmux send-keys -t ${session_target} "echo ''" Enter
    tmux send-keys -t ${session_target} "echo '=== Test 4: Large content proxy ==='" Enter
    tmux send-keys -t ${session_target} "curl -s -w 'Status: %{http_code}, Time: %{time_total}s, Size: %{size_download} bytes\\n' http://localhost:8080/dataset.json > /dev/null" Enter
    sleep 5
    
    # Test 5: Performance test with ab
    if command -v ab &> /dev/null; then
        tmux send-keys -t ${session_target} "echo ''" Enter
        tmux send-keys -t ${session_target} "echo '=== Test 5: Performance test (1000 requests, 10 concurrent) ==='" Enter
        tmux send-keys -t ${session_target} "ab -n 1000 -c 10 -q http://localhost:8080/api.json | grep -E '(Requests per second|Time per request)'" Enter
        sleep 15
    fi
    
    # Test 6: Concurrent connections
    tmux send-keys -t ${session_target} "echo ''" Enter
    tmux send-keys -t ${session_target} "echo '=== Test 6: Concurrent connections ==='" Enter
    tmux send-keys -t ${session_target} "'$INTEGRATION_DIR/run_concurrent_test.sh'" Enter
    
    print_status $GREEN "✓ Automated tests launched in tmux session"
    print_status $BLUE "Attach to see results: tmux attach-session -t ${SESSION_NAME}"
}

# Function to show status
show_status() {
    print_status $BLUE "=== UringRess Test Environment Status ==="
    
    # tmux session
    print_status $BLUE "\\n--- tmux Session ---"
    if tmux list-sessions 2>/dev/null | grep -q ${SESSION_NAME}; then
        print_status $GREEN "✓ Session '${SESSION_NAME}' is running"
        tmux list-windows -t ${SESSION_NAME} 2>/dev/null | while read line; do
            echo "  $line"
        done
    else
        print_status $RED "✗ Session '${SESSION_NAME}' not running"
    fi
    
    # Processes
    print_status $BLUE "\\n--- Processes ---"
    if pgrep -f "uringress" > /dev/null; then
        print_status $GREEN "✓ UringRess is running (PID: $(pgrep -f "uringress"))"
    else
        print_status $RED "✗ UringRess not running"
    fi
    
    for port in 3001 3002 3003; do
        if pgrep -f "kiss --port ${port}" > /dev/null; then
            print_status $GREEN "✓ KISS backend on port ${port} is running (PID: $(pgrep -f "kiss --port ${port}"))"
        else
            print_status $RED "✗ KISS backend on port ${port} not running"
        fi
    done
    
    # Network ports
    print_status $BLUE "\\n--- Network Ports ---"
    if ss -tlnp | grep -q ":8080"; then
        print_status $GREEN "✓ UringRess listening on port 8080"
    else
        print_status $RED "✗ Nothing listening on port 8080"
    fi
    
    for port in 3001 3002 3003; do
        if ss -tlnp | grep -q ":${port}"; then
            print_status $GREEN "✓ KISS backend listening on port ${port}"
        else
            print_status $RED "✗ Nothing listening on port ${port}"
        fi
    done
    
    # Service health
    print_status $BLUE "\\n--- Service Health ---"
    if curl -s --connect-timeout 2 http://localhost:8080/health > /dev/null 2>&1; then
        print_status $GREEN "✓ UringRess health check OK"
    else
        print_status $RED "✗ UringRess health check failed"
    fi
    
    for port in 3001 3002 3003; do
        if curl -s --connect-timeout 2 http://localhost:${port}/health > /dev/null 2>&1; then
            print_status $GREEN "✓ KISS backend ${port} health check OK"
        else
            print_status $RED "✗ KISS backend ${port} health check failed"
        fi
    done
}

# Function to stop environment
stop_environment() {
    print_status $BLUE "Stopping test environment..."
    
    # Kill tmux session
    if tmux has-session -t ${SESSION_NAME} 2>/dev/null; then
        tmux kill-session -t ${SESSION_NAME}
        print_status $GREEN "✓ tmux session stopped"
    fi
    
    # Kill processes
    pkill -f "kiss --port" 2>/dev/null || true
    pkill -f "uringress" 2>/dev/null || true
    print_status $GREEN "✓ Processes stopped"
    
    # Clean up files
    rm -f /tmp/uringress-test.log ${TMUX_CONF}
    print_status $GREEN "✓ Temporary files cleaned"
    
    print_status $GREEN "✓ Environment stopped successfully"
}

# Function to show usage
show_usage() {
    cat <<EOF
${BLUE}UringRess tmux Test Environment${NC}

${GREEN}Usage:${NC} $0 [command]

${GREEN}Commands:${NC}
    ${YELLOW}start${NC}     - Start the complete test environment
    ${YELLOW}attach${NC}    - Attach to existing tmux session
    ${YELLOW}stop${NC}      - Stop all services and kill tmux session
    ${YELLOW}status${NC}    - Show status of all services
    ${YELLOW}test${NC}      - Run automated tests (session must be running)
    ${YELLOW}logs${NC}      - Show UringRess logs

${GREEN}Examples:${NC}
    $0 start          # Start everything
    $0 attach         # Attach to running session
    $0 test           # Run automated tests
    $0 status         # Check what's running
    $0 stop           # Clean shutdown

${GREEN}tmux Navigation:${NC}
    Ctrl+b, 0-3       # Switch between windows
    Alt+h/j/k/l       # Switch between panes
    Alt+H/J/K/L       # Resize panes
    Ctrl+b, d         # Detach from session
    Ctrl+b, ?         # Show tmux help

${GREEN}Windows:${NC}
    0. monitor        # System monitoring
    1. kiss-backends  # KISS backend services
    2. uringress      # UringRess proxy
    3. testing        # Manual and automated testing

${GREEN}Integration Directory:${NC}
    This script can be run from project root or tests/integration/
    Paths are automatically adjusted based on execution location.
EOF
}

# Main script logic
main() {
    local command=${1:-"start"}
    
    case ${command} in
        "start")
            check_dependencies
            setup_test_content
            create_tmux_session
            
            print_status $GREEN ""
            print_status $GREEN "🚀 Test environment started successfully!"
            print_status $BLUE ""
            print_status $BLUE "Next steps:"
            print_status $BLUE "  1. ${YELLOW}$0 attach${NC} (or tmux attach-session -t ${SESSION_NAME})"
            print_status $BLUE "  2. Use Ctrl+b then 0-3 to switch between windows"
            print_status $BLUE "  3. Use Alt+h/j/k/l to switch between panes"  
            print_status $BLUE "  4. Run: ${YELLOW}$0 test${NC} (in another terminal for automated tests)"
            print_status $BLUE ""
            ;;
            
        "attach")
            if tmux has-session -t ${SESSION_NAME} 2>/dev/null; then
                tmux attach-session -t ${SESSION_NAME}
            else
                print_status $RED "ERROR: Session '${SESSION_NAME}' not found. Start the environment first."
                exit 1
            fi
            ;;
            
        "stop")
            stop_environment
            ;;
            
        "status")
            show_status
            ;;
            
        "test")
            run_automated_tests
            ;;
            
        "logs")
            if [[ -f /tmp/uringress-test.log ]]; then
                tail -f /tmp/uringress-test.log
            else
                print_status $RED "Log file not found. Is UringRess running?"
                exit 1
            fi
            ;;
            
        "help"|"-h"|"--help")
            show_usage
            ;;
            
        *)
            print_status $RED "Unknown command: $command"
            show_usage
            exit 1
            ;;
    esac
}

main "$@"