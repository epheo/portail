# UringRess

Kubernetes Gateway API Controller built for Linux io_uring environments.

## Currently Implemented Features

### Core Functionality
- **HTTP request routing** - Routes HTTP requests based on hostname and path
- **TCP connection forwarding** - Forwards TCP connections based on destination port
- **Load balancing** - Distributes traffic across multiple backend servers using round-robin
- **Multi-worker architecture** - Runs multiple worker threads for high concurrency
- **Connection pooling** - Reuses connections to backend servers for efficiency

### Traffic Handling
- **Static route configuration** - Routes are currently hardcoded for testing purposes
- **Health monitoring** - Tracks backend server availability
- **Error handling** - Returns appropriate HTTP error responses (502, 504, 500)
- **Graceful shutdown** - Cleanly stops all workers on termination signal

### Performance
- **High-performance I/O** - Uses Linux io_uring for fast network operations
- **Zero-copy processing** - Minimizes memory copying for better performance
- **Optimized routing** - Fast hostname and path matching algorithms

### Development & Testing
- **Performance benchmarks** - Measures routing, parsing, and end-to-end performance
- **Integration tests** - Validates complete request processing pipeline
- **Load testing scripts** - Tests performance under realistic traffic loads

## Quick Start

### Requirements
- Linux 5.10+ with io_uring support
- Rust 1.70+

### Build and Run
```bash
# Build the project
cargo build --release

# Run with default settings (4 workers)
cargo run --release

# Run with custom worker count
WORKER_THREADS=8 cargo run --release
```

### Testing
```bash
# Run all tests
cargo test --release

# Run benchmarks
cargo bench

# Run integration tests
./tests/integration/test_performance_realistic.sh
```

## Current Status

This is an MVP implementation with static configuration for testing and development. 

**Coming Next**: Kubernetes Gateway API integration to replace static configuration with dynamic CRD-based routing.

## License

Apache-2.0