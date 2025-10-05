# UringRess Integration Tests

This directory contains comprehensive integration tests for UringRess, providing realistic performance validation using actual KISS backends and real network I/O.

## Overview

The integration test suite validates UringRess performance characteristics under realistic conditions, moving beyond synthetic microbenchmarks to measure actual proxy behavior with real backends, network operations, and HTTP processing.

## Architecture

```
tests/integration/
├── start_uringress_test_environment.sh   # Main orchestration script
├── test_performance_realistic.sh         # End-to-end performance testing
├── run_concurrent_test.sh                # Concurrent connection validation
├── test_circuit_breaker.sh              # Backend failure/recovery testing
├── generate_performance_report.sh        # Performance report generator
├── lib/
│   └── reporting_functions.sh           # Shared reporting utilities
├── test-content/                         # Test content for KISS backends
│   ├── small/                           # Small responses (~1KB)
│   ├── medium/                          # Medium responses (~5KB)
│   └── large/                           # Large responses (~240KB)
├── reports/                              # Generated performance reports
└── README.md                            # This documentation
```

## Test Environment

### Components

1. **UringRess Proxy** (port 8080)
   - 4 worker threads with SO_REUSEPORT
   - io_uring-based event loops
   - FNV hash-based routing

2. **KISS Backends** (ports 3001-3003)
   - Port 3001: Small content (API responses)
   - Port 3002: Medium content (user lists, web pages)
   - Port 3003: Large content (datasets, file downloads)

3. **tmux Session Management**
   - Window 0: System monitoring (htop, network, processes, logs)
   - Window 1: KISS backend services
   - Window 2: UringRess proxy service
   - Window 3: Manual and automated testing

### Routing Configuration

UringRess is configured with static routes for testing:
- `localhost:8080/api.json` → Backend on port 3001 (small content)
- `localhost:8080/users.json` → Backend on port 3002 (medium content)  
- `localhost:8080/dataset.json` → Backend on port 3003 (large content)
- `localhost:8080/health` → Backend on port 3001 (health checks)

## Usage

### Quick Start

```bash
# From project root
tests/integration/start_uringress_test_environment.sh start

# Or from integration directory
cd tests/integration
./start_uringress_test_environment.sh start
```

### Complete Command Reference

#### Main Orchestration Script

```bash
# Start complete test environment
./start_uringress_test_environment.sh start

# Attach to tmux session for monitoring
./start_uringress_test_environment.sh attach

# Run automated test suite
./start_uringress_test_environment.sh test

# Check status of all services
./start_uringress_test_environment.sh status

# View UringRess logs
./start_uringress_test_environment.sh logs

# Stop all services and clean up
./start_uringress_test_environment.sh stop

# Show help
./start_uringress_test_environment.sh help
```

#### Individual Test Scripts

```bash
# Realistic performance testing
./test_performance_realistic.sh

# Concurrent connection testing (20 workers × 50 requests)
./run_concurrent_test.sh

# Circuit breaker validation
./test_circuit_breaker.sh
```

#### Performance Reporting

```bash
# Generate comprehensive performance report
./generate_performance_report.sh

# Run tests and generate fresh report
./generate_performance_report.sh --run-tests

# Display report on console only (no file output)
./generate_performance_report.sh --format console

# Save report to file only (no console output)
./generate_performance_report.sh --format file

# Custom output directory
./generate_performance_report.sh --output /path/to/reports
```

### tmux Navigation

Once attached to the tmux session:

- **Switch windows**: `Ctrl+b`, then `0-3`
- **Switch panes**: `Alt+h/j/k/l`
- **Resize panes**: `Alt+H/J/K/L`
- **Detach**: `Ctrl+b`, then `d`
- **Help**: `Ctrl+b`, then `?`

## Performance Reporting System

### Overview

The performance reporting system provides comprehensive analysis and clear visualization of UringRess performance characteristics. It implements zero-impact measurement by parsing existing test outputs rather than adding runtime measurement overhead.

### Features

- **Executive Summary**: Pass/fail status with performance grades (Excellent/Good/Fair/Poor)
- **Structured Output Parsing**: Extracts metrics from standardized test output blocks
- **Performance Grading**: Evaluates results against predefined targets from CLAUDE.md
- **Historical Tracking**: Saves performance data for trend analysis
- **Multiple Output Formats**: Console display, file output, or both
- **Cross-Directory Execution**: Works from project root or integration directory

### Usage Examples

```bash
# Quick report from existing test data
./generate_performance_report.sh

# Run fresh tests and generate report
./generate_performance_report.sh --run-tests

# Console-only output (CI/CD friendly)
./generate_performance_report.sh --format console

# Save report to custom location
./generate_performance_report.sh --output ./my-reports

# Complete workflow: test + report + file output
./generate_performance_report.sh --run-tests --format both
```


### Data Storage and Trends

- **Report Files**: `reports/performance_report_YYYYMMDD_HHMMSS.txt`
- **Data Files**: `reports/performance_data_YYYYMMDD_HHMMSS.conf`
- **Trend Analysis**: Detects historical data for future trend analysis features

### Zero-Impact Design

The reporting system follows a zero-impact measurement philosophy:

1. **Post-Test Analysis**: Parses completed test outputs, never adds runtime overhead
2. **Existing Tool Integration**: Works with curl, ab, wrk outputs without modification
3. **Optional Reporting**: Tests run normally; reporting is a separate step

## Test Types

### 1. Realistic Performance Testing

**Script**: `test_performance_realistic.sh`

**Purpose**: Measures actual end-to-end performance characteristics

**Tests**:
- **Latency Analysis**: Request latency for different content sizes
- **Throughput Testing**: Apache Bench and wrk load testing
- **Mixed Workload**: Realistic traffic patterns (API:Users:Health = 3:1:1)
- **Memory Usage**: Memory consumption during load
- **Proxy Overhead**: Direct backend vs proxy performance comparison

**Sample Output**:
```
=== Latency Test ===
Small content (api.json): 0.0003s average (20/20 successful)
Medium content (users.json): 0.0003s average (10/10 successful)
Large content (dataset.json): 0.0003s average (5/5 successful)

=== Throughput Test ===
Testing with 50 concurrent connections:
  Requests per second: 32,507 [#/sec]
  Time per request: 1.538 [ms] (mean)

=== Proxy Overhead Analysis ===
Direct backend: 0.0004s average
Through proxy: 0.0007s average
Proxy overhead: 0.0003s (75%)
✓ EXCELLENT: <1ms proxy overhead
```

### 2. Concurrent Connection Testing

**Script**: `run_concurrent_test.sh`

**Purpose**: Validates performance under high concurrent load

**Configuration**:
- 20 concurrent workers
- 50 requests per worker (1000 total requests)
- Random endpoint selection
- Real network I/O measurement

**Success Criteria**:
- **>95% success rate**: Excellent
- **>90% success rate**: Good
- **>1000 RPS**: High throughput

**Sample Output**:
```
=== Test Results ===
Total requests: 1000
Successful: 1000
Errors: 0
Success rate: 100.00%
Total duration: 0.95s
Overall RPS: 1051.25

✓ EXCELLENT: >95% success rate under concurrent load
✓ HIGH THROUGHPUT: >1000 RPS achieved
```

### 3. Circuit Breaker Testing

**Script**: `test_circuit_breaker.sh`

**Purpose**: Validates backend failure detection and recovery

**Test Scenarios**:
1. **Normal Operation**: All backends healthy
2. **Single Backend Failure**: One backend down
3. **Circuit Breaker Response**: Multiple requests to failed backend
4. **Backend Recovery**: Restart failed backend
5. **Multiple Backend Failures**: Cascade failure scenario
6. **Full Recovery**: All backends restored
7. **Performance Under Recovery**: Post-recovery performance

**Key Observations**:
- Failed requests return 502/504 status codes
- Fast circuit breaker responses
- Automatic recovery when backends restart
- Performance returns to normal after recovery

## Performance Characteristics

### Measured Performance (Typical Results)

- **Latency**: 0.3-0.7ms end-to-end
- **Throughput**: 30,000+ RPS with multiple concurrent connections
- **Success Rate**: 100% under normal conditions
- **Proxy Overhead**: <1ms additional latency
- **Memory Usage**: Stable under load

### Performance Targets

Based on MVP architecture goals:

- **FNV Hash**: <100ns per hash operation
- **HTTP Parsing**: <1μs per request
- **Route Lookup**: <500ns per lookup (O(1) performance)
- **End-to-End Processing**: <10μs total
- **Target Throughput**: >1M RPS with 4 workers (>250K per worker)

## Dependencies

### Required Tools

- **tmux**: Session management
- **kiss**: KISS static file server ([GitHub](https://github.com/epheo/kiss))
- **curl**: HTTP testing
- **ab** (apache2-utils): Load testing
- **htop**: System monitoring
- **ss**: Network monitoring
- **awk**: Mathematical calculations (replaces bc for better compatibility)

### Optional Tools

- **wrk**: Advanced HTTP benchmarking
- **jq**: JSON processing

### Installation Example (Ubuntu/Debian)

```bash
# Install system dependencies
sudo apt update
sudo apt install tmux curl apache2-utils htop iproute2 gawk

# Install KISS from source
git clone https://github.com/epheo/kiss.git
cd kiss
cargo build --release
sudo cp target/release/kiss /usr/local/bin/

# Install wrk (optional)
sudo apt install wrk
```

## Comparison with Synthetic Tests

### Why Integration Tests Matter

Traditional microbenchmarks (`tests/test_performance.rs`) measure isolated function performance in perfect conditions:

```rust
// Synthetic: CPU function calls only
for _ in 0..iterations {
    let _ = fnv_hash(host);  // ~50ns
    let _ = parse_http_headers_fast(request);  // ~800ns
    let _ = table.route_http_request(host_hash, path);  // ~200ns
}
```

Integration tests measure **real-world performance**:

```bash
# Realistic: Actual network I/O, syscalls, memory allocation
curl http://localhost:8080/api.json  # ~700μs total
```

### Performance Gap Analysis

| Component | Synthetic | Realistic | Overhead |
|-----------|-----------|-----------|----------|
| FNV Hash | 50ns | - | - |
| HTTP Parse | 800ns | - | - |
| Route Lookup | 200ns | - | - |
| **Total CPU** | **~1μs** | - | - |
| **Network I/O** | - | ~700μs | **700x** |

The 700x difference demonstrates why integration testing is essential for understanding actual performance characteristics.

## Troubleshooting

### Common Issues

1. **"KISS binary not found"**
   ```bash
   # Install KISS and ensure it's in PATH
   which kiss
   ```

2. **"UringRess not responding"**
   ```bash
   # Check if UringRess is built
   ls -la target/release/uringress
   cargo build --release
   ```

3. **Port conflicts**
   ```bash
   # Check for existing services
   ss -tlnp | grep -E "(8080|300[123])"
   ```

4. **tmux session issues**
   ```bash
   # Kill existing sessions
   tmux kill-session -t uringress-test
   ```

### Debug Commands

```bash
# Check service status
./start_uringress_test_environment.sh status

# View logs
./start_uringress_test_environment.sh logs

# Manual endpoint testing
curl -v http://localhost:8080/health
curl -v http://localhost:3001/health  # Direct backend
```

## Development Workflow

### Adding New Tests

1. Create test script in `tests/integration/`
2. Make executable: `chmod +x your_test.sh`
3. Follow existing patterns for error handling and output formatting
4. Update this README with test description

### Test Content Management

Test content is committed under `test-content/`:

- **Custom endpoints**: Add files to `test-content/{size}/` directories
- **New content types**: Extend KISS backend configuration

### CI/CD Integration

Integration tests can be automated in CI:

```yaml
# Example GitHub Actions workflow
- name: Run Integration Tests
  run: |
    cargo build --release
    tests/integration/start_uringress_test_environment.sh start
    sleep 10
    tests/integration/test_performance_realistic.sh
    tests/integration/run_concurrent_test.sh
    tests/integration/start_uringress_test_environment.sh stop
```

## Contributing

When adding new integration tests:

1. **Follow naming convention**: `test_feature_name.sh`
2. **Include error handling**: Check dependencies and service availability
3. **Provide clear output**: Use colored output and status indicators
4. **Document thoroughly**: Update this README with test descriptions
5. **Test both execution contexts**: Project root and integration directory

## Related Documentation

- [KISS Project](https://github.com/epheo/kiss): Backend static file server