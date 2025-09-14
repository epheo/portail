use std::time::{Duration, Instant};
use uringress::uring_worker::unified_http_parser::UnifiedHttpParser;
use uringress::routing::{RouteTable, Backend, FnvRouterHasher, RouterHasher};

/// CPU profiling tests for hot path analysis
/// These tests are designed to be run with external profiling tools like flamegraph

#[test]
fn test_cpu_profiling_parser_hot_path() {
    // Create diverse HTTP requests to exercise different code paths
    let requests: Vec<&[u8]> = vec![
        b"GET /api/v1/users HTTP/1.1\r\nHost: api.example.com\r\nUser-Agent: curl/7.68.0\r\nAccept: application/json\r\n\r\n",
        b"POST /api/v1/data HTTP/1.1\r\nHost: api.example.com\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n",
        b"GET /health HTTP/1.1\r\nHost: health.example.com\r\n\r\n",
        b"PUT /api/v2/users/123 HTTP/1.1\r\nHost: api.example.com\r\nAuthorization: Bearer token123\r\nContent-Type: application/json\r\n\r\n",
        b"DELETE /api/v1/users/456 HTTP/1.1\r\nHost: api.example.com\r\nAuthorization: Bearer token456\r\n\r\n",
    ];

    let start = Instant::now();
    let iterations = 100_000; // High iteration count for profiling
    
    for _ in 0..iterations {
        for request in &requests {
            let result = UnifiedHttpParser::extract_routing_info(request);
            // Use black_box equivalent to prevent optimization
            let _ = std::hint::black_box(result);
        }
    }
    
    let duration = start.elapsed();
    println!("Parser hot path: {} iterations in {:?}", iterations * requests.len(), duration);
    println!("Average per request: {:?}", duration / ((iterations * requests.len()) as u32));
    
    // Performance assertion (should be under 1μs per request)
    let avg_per_request = duration / ((iterations * requests.len()) as u32);
    assert!(avg_per_request < Duration::from_micros(1), 
           "Parser taking too long: {:?} per request", avg_per_request);
}

#[test]
fn test_cpu_profiling_routing_hot_path() {
    // Setup routing table with realistic microservice routing
    let mut table = RouteTable::new();
    
    // Add 100 different hosts and paths
    for i in 0..100 {
        let host = format!("service{}.api.com", i);
        for j in 0..5 {
            let path = format!("/api/v{}/data", j + 1);
            let backend = Backend::new(format!("127.0.0.{}", (i % 255) + 1), 8080 + j as u16).unwrap();
            table.add_http_route(&host, &path, backend);
        }
    }
    
    // Create test data that exercises different routing paths
    let test_hosts = (0..100).map(|i| format!("service{}.api.com", i)).collect::<Vec<_>>();
    let test_paths = vec!["/api/v1/data", "/api/v2/data", "/api/v3/data", "/api/v4/data", "/api/v5/data"];
    
    let start = Instant::now();
    let iterations = 50_000;
    
    let hasher = FnvRouterHasher;
    for _ in 0..iterations {
        for (i, host) in test_hosts.iter().enumerate() {
            let path = test_paths[i % test_paths.len()];
            let host_hash = hasher.hash(host);
            let result = table.find_http_backend_pool(host_hash, path);
            let _ = std::hint::black_box(result);
        }
    }
    
    let duration = start.elapsed();
    println!("Routing hot path: {} iterations in {:?}", iterations * test_hosts.len(), duration);
    println!("Average per route: {:?}", duration / ((iterations * test_hosts.len()) as u32));
    
    // Performance assertion (should be under 500ns per route lookup)
    let avg_per_route = duration / ((iterations * test_hosts.len()) as u32);
    assert!(avg_per_route < Duration::from_nanos(500), 
           "Routing taking too long: {:?} per route", avg_per_route);
}

#[test]
fn test_cpu_profiling_end_to_end_hot_path() {
    // Setup routing table
    let mut table = RouteTable::new();
    for i in 0..50 {
        let host = format!("api{}.example.com", i);
        let backend = Backend::new(format!("127.0.0.{}", (i % 255) + 1), 8080).unwrap();
        table.add_http_route(&host, "/api", backend);
    }
    
    // Create requests that exercise full pipeline: parse + route + select backend
    let requests = (0..50).map(|i| {
        format!("GET /api/data HTTP/1.1\r\nHost: api{}.example.com\r\nUser-Agent: test\r\n\r\n", i)
    }).collect::<Vec<_>>();
    
    let start = Instant::now();
    let iterations = 10_000;
    
    for _ in 0..iterations {
        for request in &requests {
            // Parse request
            let info = UnifiedHttpParser::extract_routing_info(request.as_bytes()).unwrap();
            let (path, host) = (info.path, Some(info.host));
            
            // Route request
            if let Some(host) = host {
                let hasher = FnvRouterHasher;
                let host_hash = hasher.hash(host);
                let result = table.find_http_backend_pool(host_hash, path);
                let _ = std::hint::black_box(result);
            }
        }
    }
    
    let duration = start.elapsed();
    println!("End-to-end hot path: {} iterations in {:?}", iterations * requests.len(), duration);
    println!("Average per request: {:?}", duration / ((iterations * requests.len()) as u32));
    
    // Performance assertion (should be under 10μs per end-to-end request)
    let avg_per_request = duration / ((iterations * requests.len()) as u32);
    assert!(avg_per_request < Duration::from_micros(10), 
           "End-to-end taking too long: {:?} per request", avg_per_request);
}

#[test]
fn test_cpu_profiling_fnv_hash_hot_path() {
    // Test FNV hash performance with different string lengths and patterns
    let test_strings = vec![
        "api.example.com",
        "very-long-hostname.microservice.company.internal.com",
        "short.io",
        "api1.example.com",
        "api2.example.com", 
        "users-service.internal.company.com",
        "payments-gateway.secure.financial.com",
        "analytics-collector.data.company.com",
    ];
    
    let start = Instant::now();
    let iterations = 1_000_000; // Very high iteration count for hash function
    
    let hasher = FnvRouterHasher;
    for _ in 0..iterations {
        for hostname in &test_strings {
            let hash = hasher.hash(hostname);
            std::hint::black_box(hash);
        }
    }
    
    let duration = start.elapsed();
    println!("FNV hash hot path: {} iterations in {:?}", iterations * test_strings.len(), duration);
    println!("Average per hash: {:?}", duration / ((iterations * test_strings.len()) as u32));
    
    // Performance assertion (should be under 100ns per hash)
    let avg_per_hash = duration / ((iterations * test_strings.len()) as u32);
    assert!(avg_per_hash < Duration::from_nanos(100), 
           "FNV hash taking too long: {:?} per hash", avg_per_hash);
}

/// Profiling helper function that runs CPU-intensive workload
/// This can be used with external profiling tools like `perf record`
#[test]
#[ignore] // Only run with --ignored flag for profiling sessions
fn test_cpu_profiling_stress_test() {
    println!("Starting CPU profiling stress test...");
    println!("Run this test with: cargo test test_cpu_profiling_stress_test --release -- --ignored");
    println!("Profile with: perf record --call-graph dwarf cargo test test_cpu_profiling_stress_test --release -- --ignored");
    
    // Setup comprehensive test scenario
    let mut table = RouteTable::new();
    for i in 0..1000 {
        let host = format!("service{}.company.com", i);
        for j in 0..3 {
            let path = format!("/api/v{}", j + 1);
            let backend = Backend::new(format!("127.0.0.{}", (i % 255) + 1), 8080 + j as u16).unwrap();
            table.add_http_route(&host, &path, backend);
        }
    }
    
    // Create diverse request patterns
    let requests = (0..1000).map(|i| {
        let method = if i % 4 == 0 { "GET" } else if i % 4 == 1 { "POST" } else if i % 4 == 2 { "PUT" } else { "DELETE" };
        let path = format!("/api/v{}", (i % 3) + 1);
        let host = format!("service{}.company.com", i);
        format!("{} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: stress-test\r\nAccept: application/json\r\n\r\n", 
                method, path, host)
    }).collect::<Vec<_>>();
    
    let start = Instant::now();
    let iterations = 1000; // Lower iterations but more comprehensive per iteration
    
    for iteration in 0..iterations {
        if iteration % 100 == 0 {
            println!("Profiling iteration: {}/{}", iteration, iterations);
        }
        
        for request in &requests {
            // Full pipeline: parse + route + select backend
            match UnifiedHttpParser::extract_routing_info(request.as_bytes()) {
                Ok(info) => {
                    let hasher = FnvRouterHasher;
                    let host_hash = hasher.hash(info.host);
                    let result = table.find_http_backend_pool(host_hash, info.path);
                    let _ = std::hint::black_box(result);
                }
                Err(e) => {
                    std::hint::black_box(e);
                }
            }
        }
    }
    
    let duration = start.elapsed();
    let total_operations = iterations * requests.len();
    println!("CPU profiling stress test completed!");
    println!("Total operations: {}", total_operations);
    println!("Total duration: {:?}", duration);
    println!("Average per operation: {:?}", duration / total_operations as u32);
    println!("Operations per second: {:.0}", total_operations as f64 / duration.as_secs_f64());
}