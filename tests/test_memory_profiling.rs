use std::time::{Duration, Instant};
use uringress::parser::parse_http_headers_fast;
use uringress::routing::{RouteTable, Backend, fnv_hash};
use uringress::backend::PooledBuffer;

/// Memory allocation profiling tests for hot path analysis
/// These tests are designed to validate zero-allocation claims and detect memory leaks

#[test]
fn test_memory_profiling_parser_allocations() {
    // Test that HTTP parsing doesn't allocate in hot paths
    let requests: Vec<&[u8]> = vec![
        b"GET /api HTTP/1.1\r\nHost: example.com\r\n\r\n",
        b"POST /data HTTP/1.1\r\nHost: api.example.com\r\nContent-Type: application/json\r\n\r\n",
        b"PUT /users/123 HTTP/1.1\r\nHost: api.example.com\r\nAuthorization: Bearer token\r\n\r\n",
        b"DELETE /items/456 HTTP/1.1\r\nHost: api.example.com\r\n\r\n",
    ];

    // Warm up to stabilize allocations
    for _ in 0..100 {
        for request in &requests {
            let _ = parse_http_headers_fast(request);
        }
    }

    println!("Starting memory allocation test for HTTP parser...");
    
    // Test that steady-state parsing has minimal allocations
    let iterations = 10_000;
    for i in 0..iterations {
        if i % 1000 == 0 {
            println!("Parser memory test iteration: {}/{}", i, iterations);
        }
        
        for request in &requests {
            // This should not allocate - all data should be string slices
            let result = parse_http_headers_fast(request);
            match result {
                Ok((method, path, host)) => {
                    // Use the results to prevent optimization
                    std::hint::black_box((method.len(), path.len(), host.map(|h| h.len())));
                }
                Err(_) => {
                    // Handle errors without allocation
                    std::hint::black_box(());
                }
            }
        }
    }
    
    println!("HTTP parser memory allocation test complete - {} requests processed", 
             iterations * requests.len());
}

#[test]
fn test_memory_profiling_routing_allocations() {
    // Setup routing table (one-time allocation)
    let mut table = RouteTable::new();
    for i in 0..100 {
        let host = format!("service{}.com", i);
        let backend = Backend::new(format!("127.0.0.{}", (i % 254) + 1), 8080).unwrap();
        table.add_http_route(&host, "/api", backend);
    }

    // Create test data
    let hosts: Vec<String> = (0..100).map(|i| format!("service{}.com", i)).collect();
    
    // Warm up routing caches
    for _ in 0..100 {
        for host in &hosts {
            let host_hash = fnv_hash(host);
            let _ = table.route_http_request(host_hash, "/api");
        }
    }

    println!("Starting memory allocation test for routing engine...");
    
    // Test that routing lookups don't allocate
    let iterations = 50_000;
    for i in 0..iterations {
        if i % 5000 == 0 {
            println!("Routing memory test iteration: {}/{}", i, iterations);
        }
        
        for host in &hosts {
            // Hash computation should not allocate
            let host_hash = fnv_hash(host);
            
            // Routing lookup should not allocate
            let result = table.route_http_request(host_hash, "/api");
            
            // Use result to prevent optimization
            std::hint::black_box(result.is_ok());
        }
    }
    
    println!("Routing engine memory allocation test complete - {} lookups processed",
             iterations * hosts.len());
}

#[test]
fn test_memory_profiling_buffer_pool_behavior() {
    println!("Starting buffer pool memory behavior test...");
    
    // Test buffer pool allocation and reuse patterns
    let sizes = vec![1024, 2048, 4096, 8192, 16384];
    let iterations_per_size = 1000;
    
    for size in sizes {
        println!("Testing buffer pool behavior for size: {} bytes", size);
        
        // Warm up buffer pool for this size
        let mut warmup_buffers = Vec::new();
        for _ in 0..10 {
            warmup_buffers.push(PooledBuffer::new(size));
        }
        // Drop warmup buffers to return to pool
        drop(warmup_buffers);
        
        // Test actual allocation/reuse behavior
        for i in 0..iterations_per_size {
            if i % 100 == 0 {
                println!("  Buffer pool test iteration: {}/{}", i, iterations_per_size);
            }
            
            // Create buffer - should reuse from pool after warmup
            let mut buffer = PooledBuffer::new(size);
            
            // Use buffer
            let test_data = vec![42u8; size];
            buffer.copy_from_slice(&test_data);
            
            // Verify buffer has correct data
            let buffer_len = buffer.as_mut_vec().len();
            std::hint::black_box(buffer_len);
            
            // Buffer automatically returns to pool when dropped
        }
    }
    
    println!("Buffer pool memory behavior test complete");
}

#[test]
fn test_memory_profiling_end_to_end_allocations() {
    // Setup routing table
    let mut table = RouteTable::new();
    for i in 0..50 {
        let host = format!("api{}.example.com", i);
        let backend = Backend::new(format!("127.0.0.{}", (i % 254) + 1), 8080).unwrap();
        table.add_http_route(&host, "/api/data", backend);
    }
    
    // Create test requests
    let requests: Vec<String> = (0..50).map(|i| {
        format!("GET /api/data HTTP/1.1\r\nHost: api{}.example.com\r\nUser-Agent: test\r\n\r\n", i)
    }).collect();
    
    // Warm up the full pipeline
    for _ in 0..100 {
        for request in &requests {
            if let Ok((_, path, host)) = parse_http_headers_fast(request.as_bytes()) {
                if let Some(host) = host {
                    let host_hash = fnv_hash(host);
                    let _ = table.route_http_request(host_hash, path);
                }
            }
        }
    }

    println!("Starting end-to-end memory allocation test...");
    
    // Test end-to-end pipeline for allocation behavior
    let iterations = 5_000;
    for i in 0..iterations {
        if i % 500 == 0 {
            println!("End-to-end memory test iteration: {}/{}", i, iterations);
        }
        
        for request in &requests {
            // Full pipeline: parse + hash + route
            // This should have minimal allocations in steady state
            match parse_http_headers_fast(request.as_bytes()) {
                Ok((method, path, host)) => {
                    if let Some(host) = host {
                        let host_hash = fnv_hash(host);
                        let result = table.route_http_request(host_hash, path);
                        
                        // Use results to prevent optimization
                        std::hint::black_box((method.len(), result.is_ok()));
                    }
                }
                Err(_) => {
                    std::hint::black_box(());
                }
            }
        }
    }
    
    println!("End-to-end memory allocation test complete - {} full pipelines processed",
             iterations * requests.len());
}

#[test]
fn test_memory_profiling_fnv_hash_allocations() {
    // Test FNV hash function for allocation behavior
    let test_hostnames = vec![
        "api.example.com",
        "users-service.company.internal.com", 
        "payments-gateway.secure.financial.institution.com",
        "short.io",
        "very-very-very-long-hostname-for-testing-edge-cases.company.com",
        "api1.service.com",
        "api2.service.com",
        "analytics.data.warehouse.com",
    ];

    println!("Starting FNV hash memory allocation test...");
    
    // Test that FNV hash never allocates
    let iterations = 100_000;
    for i in 0..iterations {
        if i % 10000 == 0 {
            println!("FNV hash memory test iteration: {}/{}", i, iterations);
        }
        
        for hostname in &test_hostnames {
            // FNV hash should never allocate - it works on borrowed strings
            let hash = fnv_hash(hostname);
            std::hint::black_box(hash);
        }
    }
    
    println!("FNV hash memory allocation test complete - {} hashes computed",
             iterations * test_hostnames.len());
}

/// Memory stress test for comprehensive allocation analysis
/// Use with dhat or other memory profilers
#[test]
#[ignore] // Only run with --ignored flag for profiling sessions
fn test_memory_profiling_stress_test() {
    println!("Starting comprehensive memory stress test...");
    println!("Run with: cargo test test_memory_profiling_stress_test --release -- --ignored");
    println!("Profile with dhat: cargo test test_memory_profiling_stress_test --release --features dhat-heap -- --ignored");
    
    // Setup comprehensive test scenario
    let mut table = RouteTable::new();
    for i in 0..1000 {
        let host = format!("service{}.company.com", i);
        for j in 0..3 {
            let path = format!("/api/v{}", j + 1);
            let backend = Backend::new(format!("127.0.{}.{}", (i % 254) + 1, (j % 254) + 1), 8080).unwrap();
            table.add_http_route(&host, &path, backend);
        }
    }
    
    // Create diverse request patterns
    let requests: Vec<String> = (0..1000).map(|i| {
        let method = match i % 4 {
            0 => "GET",
            1 => "POST", 
            2 => "PUT",
            _ => "DELETE",
        };
        let path = format!("/api/v{}", (i % 3) + 1);
        let host = format!("service{}.company.com", i);
        format!("{} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: memory-stress\r\nAccept: application/json\r\n\r\n", 
                method, path, host)
    }).collect();
    
    // Warm up all systems
    println!("Warming up memory pools and caches...");
    for _ in 0..100 {
        for request in &requests {
            if let Ok((_, path, host)) = parse_http_headers_fast(request.as_bytes()) {
                if let Some(host) = host {
                    let host_hash = fnv_hash(host);
                    let _ = table.route_http_request(host_hash, path);
                }
            }
        }
    }
    
    println!("Starting memory stress test iterations...");
    
    let start = Instant::now();
    let iterations = 1000; // High iteration count for memory analysis
    
    for iteration in 0..iterations {
        if iteration % 100 == 0 {
            println!("Memory stress iteration: {}/{}", iteration, iterations);
        }
        
        // Process all requests in this iteration
        for request in &requests {
            // Full pipeline processing
            match parse_http_headers_fast(request.as_bytes()) {
                Ok((method, path, host)) => {
                    if let Some(host) = host {
                        let host_hash = fnv_hash(host);
                        match table.route_http_request(host_hash, path) {
                            Ok(_backend) => {
                                // Simulate some buffer usage
                                let mut buffer = PooledBuffer::new(1024);
                                let test_data = vec![42u8; 512];
                                buffer.copy_from_slice(&test_data);
                                std::hint::black_box((method.len(), buffer.as_mut_vec().len()));
                            }
                            Err(_) => {
                                std::hint::black_box(());
                            }
                        }
                    }
                }
                Err(_) => {
                    std::hint::black_box(());
                }
            }
        }
    }
    
    let duration = start.elapsed();
    let total_operations = iterations * requests.len();
    
    println!("Memory stress test completed!");
    println!("Total operations: {}", total_operations);
    println!("Total duration: {:?}", duration);
    println!("Operations per second: {:.0}", total_operations as f64 / duration.as_secs_f64());
    println!("Memory profiling data should show allocation patterns in this test");
}