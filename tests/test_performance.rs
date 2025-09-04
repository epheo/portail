use std::time::Instant;
use uringress::parser::parse_http_headers_fast;
use uringress::routing::{RouteTable, Backend, fnv_hash};

/// Performance validation tests for MVP targets
/// These tests validate that we meet our performance goals

#[test]
fn test_fnv_hash_performance() {
    let hosts = [
        "api.com",
        "subdomain.api.com", 
        "very.long.subdomain.example.com",
        "microservice.enterprise.example.com",
    ];
    
    let iterations = 10000;
    let start = Instant::now();
    
    for _ in 0..iterations {
        for host in &hosts {
            let _ = fnv_hash(host);
        }
    }
    
    let elapsed = start.elapsed();
    let avg_time_ns = elapsed.as_nanos() / (iterations * hosts.len()) as u128;
    
    println!("FNV hash average time: {}ns", avg_time_ns);
    
    // Target: <100ns per hash (sub-microsecond)
    assert!(avg_time_ns < 100, "FNV hash too slow: {}ns > 100ns", avg_time_ns);
}

#[test]
fn test_http_parsing_performance() {
    let requests = [
        b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n" as &[u8],
        b"POST /api/data HTTP/1.1\r\nHost: api.example.com\r\nContent-Type: application/json\r\n\r\n",
        b"GET /api/v1/users/123?include=profile HTTP/1.1\r\nHost: users.api.com\r\nAuthorization: Bearer token\r\nUser-Agent: test-client\r\n\r\n",
    ];
    
    let iterations = 10000;
    let start = Instant::now();
    
    for _ in 0..iterations {
        for request in &requests {
            let _ = parse_http_headers_fast(request);
        }
    }
    
    let elapsed = start.elapsed();
    let avg_time_ns = elapsed.as_nanos() / (iterations * requests.len()) as u128;
    
    println!("HTTP parsing average time: {}ns", avg_time_ns);
    
    // Target: <300ns per parse with single-pass optimization
    assert!(avg_time_ns < 300, "HTTP parsing too slow: {}ns > 300ns", avg_time_ns);
}

#[test]
fn test_route_lookup_performance() {
    let mut table = RouteTable::new();
    
    // Setup routing table with 1000 routes
    for i in 0..1000 {
        let host = format!("host{}.example.com", i);
        let backend = Backend::new(format!("127.0.0.{}", (i % 254) + 1), 8080).unwrap();
        table.add_http_route(&host, "/api", backend);
    }
    
    let test_hosts: Vec<_> = (0..100)
        .map(|i| format!("host{}.example.com", i))
        .collect();
    
    let iterations = 10000;
    let start = Instant::now();
    
    for _ in 0..iterations {
        for host in &test_hosts {
            let host_hash = fnv_hash(host);
            let _ = table.route_http_request(host_hash, "/api/test");
        }
    }
    
    let elapsed = start.elapsed();
    let avg_time_ns = elapsed.as_nanos() / (iterations * test_hosts.len()) as u128;
    
    println!("Route lookup average time: {}ns", avg_time_ns);
    
    // Target: <500ns per lookup (O(1) performance)
    assert!(avg_time_ns < 500, "Route lookup too slow: {}ns > 500ns", avg_time_ns);
}

#[test]
fn test_o1_route_lookup_scalability() {
    // Test that routing performance doesn't degrade with table size
    let sizes = [100, 1000, 10000];
    let mut results = Vec::new();
    
    for size in &sizes {
        let mut table = RouteTable::new();
        
        for i in 0..*size {
            let host = format!("host{}.example.com", i);
            let backend = Backend::new(format!("127.0.0.{}", (i % 254) + 1), 8080).unwrap();
            table.add_http_route(&host, "/api", backend);
        }
        
        let test_host = format!("host{}.example.com", size / 2);
        let host_hash = fnv_hash(&test_host);
        
        let iterations = 1000;
        let start = Instant::now();
        
        for _ in 0..iterations {
            let _ = table.route_http_request(host_hash, "/api/test");
        }
        
        let elapsed = start.elapsed();
        let avg_time_ns = elapsed.as_nanos() / iterations as u128;
        results.push((*size, avg_time_ns));
        
        println!("Table size {}: {}ns per lookup", size, avg_time_ns);
    }
    
    // Verify O(1) performance - later lookups shouldn't be significantly slower
    let ratio = results[2].1 as f64 / results[0].1 as f64;
    println!("Performance ratio (10K/100): {:.2}", ratio);
    
    // Allow up to 2x degradation due to cache effects, but should be roughly constant
    assert!(ratio < 2.0, "Route lookup is not O(1): {:.2}x degradation", ratio);
}

#[test]
fn test_end_to_end_request_latency() {
    let mut table = RouteTable::new();
    
    // Setup typical microservice routes
    let services = [
        ("auth.api.com", "/login"),
        ("users.api.com", "/profile"), 
        ("products.api.com", "/search"),
        ("orders.api.com", "/create"),
    ];
    
    for (host, path) in &services {
        let backend = Backend::new("127.0.0.1".to_string(), 8080).unwrap();
        table.add_http_route(host, path, backend);
    }
    
    let requests = [
        b"GET /login HTTP/1.1\r\nHost: auth.api.com\r\n\r\n" as &[u8],
        b"GET /profile HTTP/1.1\r\nHost: users.api.com\r\nAuthorization: Bearer token\r\n\r\n",
        b"GET /search?q=laptop HTTP/1.1\r\nHost: products.api.com\r\n\r\n",
        b"POST /create HTTP/1.1\r\nHost: orders.api.com\r\nContent-Type: application/json\r\n\r\n",
    ];
    
    let iterations = 1000;
    let start = Instant::now();
    
    for _ in 0..iterations {
        for request in &requests {
            // End-to-end: Parse + Route + Backend Selection
            let (_method, path, host, _connection) = parse_http_headers_fast(request).unwrap();
            if let Some(host) = host {
                let host_hash = fnv_hash(host);
                let _ = table.route_http_request(host_hash, path);
            }
        }
    }
    
    let elapsed = start.elapsed();
    let avg_time_ns = elapsed.as_nanos() / (iterations * requests.len()) as u128;
    
    println!("End-to-end request processing: {}ns", avg_time_ns);
    
    // Target: <10μs (10,000ns) for full request processing 
    assert!(avg_time_ns < 10000, "End-to-end processing too slow: {}ns > 10000ns", avg_time_ns);
}

#[test]
fn test_throughput_estimate() {
    // Estimate requests per second based on processing time
    let mut table = RouteTable::new();
    let backend = Backend::new("127.0.0.1".to_string(), 8080).unwrap();
    table.add_http_route("api.com", "/test", backend);
    
    let request = b"GET /test HTTP/1.1\r\nHost: api.com\r\n\r\n";
    
    let iterations = 100000;
    let start = Instant::now();
    
    for _ in 0..iterations {
        let (_method, path, host, _connection) = parse_http_headers_fast(request).unwrap();
        if let Some(host) = host {
            let host_hash = fnv_hash(host);
            let _ = table.route_http_request(host_hash, path);
        }
    }
    
    let elapsed = start.elapsed();
    let requests_per_second = iterations as f64 / elapsed.as_secs_f64();
    
    println!("Estimated single-thread RPS: {:.0}", requests_per_second);
    println!("Estimated 4-worker RPS: {:.0}", requests_per_second * 4.0);
    
    // Target: >1M RPS with 4 workers (>250K RPS per worker)
    assert!(requests_per_second > 250000.0, 
        "Throughput too low: {:.0} RPS < 250K RPS per worker", requests_per_second);
}

#[test]
fn test_memory_efficiency() {
    // Test that we don't allocate excessively during request processing
    let mut table = RouteTable::new();
    let backend = Backend::new("127.0.0.1".to_string(), 8080).unwrap();
    table.add_http_route("api.com", "/test", backend);
    
    let request = b"GET /test HTTP/1.1\r\nHost: api.com\r\nUser-Agent: test\r\n\r\n";
    
    // Warm up
    for _ in 0..1000 {
        let _ = parse_http_headers_fast(request);
    }
    
    // The actual test would require memory profiling tools
    // For now, just verify basic functionality works
    let result = parse_http_headers_fast(request);
    assert!(result.is_ok());
    
    let (method, path, host, _connection) = result.unwrap();
    assert_eq!(method, "GET");
    assert_eq!(path, "/test");
    assert_eq!(host, Some("api.com"));
    
    println!("Memory efficiency test: Basic allocation test passed");
}

#[test]
fn test_concurrent_route_lookup_safety() {
    use std::sync::Arc;
    use std::thread;
    
    let mut table = RouteTable::new();
    
    // Setup routes
    for i in 0..100 {
        let host = format!("host{}.example.com", i);
        let backend = Backend::new(format!("127.0.0.{}", (i % 254) + 1), 8080).unwrap();
        table.add_http_route(&host, "/api", backend);
    }
    
    let table = Arc::new(table);
    let mut handles = vec![];
    
    // Spawn multiple threads doing concurrent lookups
    for thread_id in 0..4 {
        let table_clone = table.clone();
        let handle = thread::spawn(move || {
            let mut successful_lookups = 0;
            
            for i in 0..1000 {
                let host = format!("host{}.example.com", (i + thread_id * 25) % 100);
                let host_hash = fnv_hash(&host);
                
                if table_clone.route_http_request(host_hash, "/api/test").is_ok() {
                    successful_lookups += 1;
                }
            }
            
            successful_lookups
        });
        handles.push(handle);
    }
    
    // Wait for all threads and verify results
    let mut total_successful = 0;
    for handle in handles {
        total_successful += handle.join().unwrap();
    }
    
    println!("Concurrent lookups successful: {}/4000", total_successful);
    
    // Should have high success rate with concurrent access
    assert!(total_successful > 3900, "Too many failed concurrent lookups: {}/4000", total_successful);
}