use uringress::routing::{RouteTable, Backend, fnv_hash};
use uringress::parser::parse_http_headers_fast;
use uringress::backend::PooledBuffer;

/// Isolated performance profiling tests that eliminate test framework overhead
/// These tests call UringRess functions directly for accurate CPU profiling

/// Direct FNV hash function profiling without test framework interference
#[no_mangle]
pub extern "C" fn profile_fnv_hash_direct() {
    let hosts = [
        "api.com",
        "users.service.company.com", 
        "microservice.internal.example.com",
        "long.subdomain.very.deep.hierarchy.enterprise.com",
        "backend.auth.api.production.company.com",
        "data.processing.ml.service.platform.com"
    ];
    
    // High iteration count for sustained profiling
    for _ in 0..2_000_000 {
        for host in &hosts {
            // Direct function call - no test framework overhead
            std::hint::black_box(fnv_hash(host));
        }
    }
}

/// Direct HTTP parsing profiling without test framework interference  
#[no_mangle]
pub extern "C" fn profile_http_parsing_direct() {
    let requests: Vec<&[u8]> = vec![
        b"GET / HTTP/1.1\r\nHost: api.com\r\n\r\n",
        b"POST /api/users HTTP/1.1\r\nHost: users.api.com\r\nContent-Type: application/json\r\nContent-Length: 156\r\n\r\n",
        b"GET /api/v1/data?filter=active&sort=created_at HTTP/1.1\r\nHost: data.service.com\r\nAuthorization: Bearer eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9\r\nUser-Agent: mobile-app/2.1.0\r\nAccept: application/json\r\n\r\n",
        b"PUT /api/orders/123 HTTP/1.1\r\nHost: orders.api.com\r\nContent-Type: application/json\r\nAuthorization: Bearer token123\r\nX-Request-ID: req-456\r\nContent-Length: 89\r\n\r\n",
        b"DELETE /api/cache/user:789 HTTP/1.1\r\nHost: cache.internal.com\r\nAuthorization: Bearer internal-token\r\n\r\n"
    ];
    
    // High iteration count for sustained profiling
    for _ in 0..1_000_000 {
        for request in &requests {
            // Direct function call - no test framework overhead
            let _ = std::hint::black_box(parse_http_headers_fast(request));
        }
    }
}

/// Direct route lookup profiling without test framework interference
#[no_mangle]
pub extern "C" fn profile_route_lookup_direct() {
    // Setup routing table once
    let mut table = RouteTable::new();
    
    // Create realistic route table with 1000 entries
    for i in 0..1000 {
        let host = format!("service{}.company.com", i);
        for j in 0..3 {
            let path = format!("/api/v{}", j + 1);
            let backend = Backend::new(
                format!("127.0.{}.{}", (i % 254) + 1, (j % 254) + 1), 
                8080
            ).unwrap();
            table.add_http_route(&host, &path, backend);
        }
    }
    
    // Pre-compute host hashes to isolate lookup performance
    let test_patterns: Vec<_> = (0..100).map(|i| {
        let host = format!("service{}.company.com", i);
        let host_hash = fnv_hash(&host);
        (host_hash, format!("/api/v{}/data", (i % 3) + 1))
    }).collect();
    
    // High iteration count for sustained profiling  
    for _ in 0..1_000_000 {
        for (host_hash, path) in &test_patterns {
            // Direct route lookup - no test framework overhead
            let _ = std::hint::black_box(table.route_http_request(*host_hash, path));
        }
    }
}

/// Direct buffer operations profiling without test framework interference
#[no_mangle] 
pub extern "C" fn profile_buffer_operations_direct() {
    let request_sizes = [512, 1024, 2048, 4096];
    let response_sizes = [1024, 4096, 8192, 16384];
    
    // Create realistic buffer data
    let dummy_request_data = vec![42u8; 4096];
    let dummy_response_data = vec![65u8; 16384];
    
    // High iteration count for sustained profiling
    for _ in 0..500_000 {
        for &req_size in &request_sizes {
            // Request buffer operations
            let mut req_buffer = PooledBuffer::new(req_size);
            let copy_len = req_size.min(dummy_request_data.len());
            req_buffer.copy_from_slice(&dummy_request_data[..copy_len]);
            std::hint::black_box(req_buffer.as_mut_vec().len());
            
            for &resp_size in &response_sizes {
                // Response buffer operations  
                let mut resp_buffer = PooledBuffer::new(resp_size);
                let copy_len = resp_size.min(dummy_response_data.len());
                resp_buffer.copy_from_slice(&dummy_response_data[..copy_len]);
                std::hint::black_box(resp_buffer.as_mut_vec().len());
            }
        }
    }
}

/// End-to-end pipeline profiling without test framework interference
#[no_mangle]
pub extern "C" fn profile_e2e_pipeline_direct() {
    // Setup components once
    let mut table = RouteTable::new();
    for i in 0..50 {
        let host = format!("service{}.company.com", i);
        let backend = Backend::new(format!("127.0.0.{}", (i % 254) + 1), 8080).unwrap();
        table.add_http_route(&host, "/api", backend);
    }
    
    let requests: Vec<Vec<u8>> = (0..50).map(|i| {
        format!("GET /api/data?id={} HTTP/1.1\r\nHost: service{}.company.com\r\nUser-Agent: profiling-client\r\nAccept: application/json\r\n\r\n", 
                i, i % 50).into_bytes()
    }).collect();
    
    // High iteration count for sustained profiling
    for _ in 0..100_000 {
        for request in &requests {
            // Complete pipeline: Parse -> Hash -> Route -> Buffer
            if let Ok((method, path, host, _)) = parse_http_headers_fast(request) {
                if let Some(host) = host {
                    let host_hash = fnv_hash(host);
                    if let Ok(_backend) = table.route_http_request(host_hash, path) {
                        // Simulate buffer operations
                        let mut req_buffer = PooledBuffer::new(1024);
                        req_buffer.copy_from_slice(&request[..request.len().min(1024)]);
                        
                        let mut resp_buffer = PooledBuffer::new(2048);
                        let dummy_response = vec![72u8; 1500];
                        resp_buffer.copy_from_slice(&dummy_response);
                        
                        std::hint::black_box((method.len(), req_buffer.as_mut_vec().len(), resp_buffer.as_mut_vec().len()));
                    }
                }
            }
        }
    }
}

// Test functions that call the isolated profiling functions
// These allow the isolated functions to be called from cargo test

#[test]
fn test_isolated_fnv_hash_profiling() {
    println!("Running isolated FNV hash profiling...");
    profile_fnv_hash_direct();
    println!("Isolated FNV hash profiling complete");
}

#[test]
fn test_isolated_http_parsing_profiling() {
    println!("Running isolated HTTP parsing profiling...");
    profile_http_parsing_direct();
    println!("Isolated HTTP parsing profiling complete");
}

#[test]
fn test_isolated_route_lookup_profiling() {
    println!("Running isolated route lookup profiling...");
    profile_route_lookup_direct();
    println!("Isolated route lookup profiling complete");
}

#[test]
fn test_isolated_buffer_operations_profiling() {
    println!("Running isolated buffer operations profiling...");
    profile_buffer_operations_direct();
    println!("Isolated buffer operations profiling complete");
}

#[test]
fn test_isolated_e2e_pipeline_profiling() {
    println!("Running isolated end-to-end pipeline profiling...");
    profile_e2e_pipeline_direct();
    println!("Isolated end-to-end pipeline profiling complete");
}

/// Combined isolated profiling test for comprehensive analysis
#[test]
fn test_all_isolated_profiling() {
    println!("=== Running All Isolated Profiling Tests ===");
    
    println!("1. FNV Hash Performance...");
    profile_fnv_hash_direct();
    
    println!("2. HTTP Parsing Performance...");
    profile_http_parsing_direct();
    
    println!("3. Route Lookup Performance...");
    profile_route_lookup_direct();
    
    println!("4. Buffer Operations Performance...");
    profile_buffer_operations_direct();
    
    println!("5. End-to-End Pipeline Performance...");
    profile_e2e_pipeline_direct();
    
    println!("=== All Isolated Profiling Tests Complete ===");
}

/// Real workload integration test for runtime flamegraph generation
/// WARNING: This test profiles the CLIENT (curl) side, not the server!
/// For server-side profiling, use: tests/integration/profiling/profile_server_runtime.sh
#[test]
fn test_runtime_flamegraph_with_real_workload() {
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Duration;
    use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
    use std::path::Path;
    
    println!("=== Starting Real Workload Runtime Flamegraph Test ===");
    
    // Ensure profiling binary exists (avoid cargo compilation during profiling)
    let binary_path = "./target/profiling/uringress";
    if !Path::new(binary_path).exists() {
        panic!("UringRess profiling binary not found at {}\nBuild it first: cargo build --profile profiling", binary_path);
    }
    
    // Start pre-built UringRess binary directly (NO cargo run - avoids compilation profiling)
    println!("1. Starting pre-built UringRess server...");
    let mut server = Command::new(binary_path)
        .args(&["--config", "examples/development.yaml"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to start pre-built UringRess server");
    
    // Give server time to start
    thread::sleep(Duration::from_secs(3));
    
    // Check if server is responding
    for i in 0..10 {
        if let Ok(response) = std::process::Command::new("curl")
            .args(&["-s", "--connect-timeout", "1", "http://localhost:8080/health"])
            .output() 
        {
            if response.status.success() {
                println!("✓ UringRess server is ready");
                break;
            }
        }
        if i == 9 {
            server.kill().unwrap();
            panic!("UringRess server failed to start");
        }
        thread::sleep(Duration::from_millis(500));
    }
    
    // Generate realistic HTTP workload for sustained profiling
    println!("2. Generating realistic HTTP workload for 30 seconds...");
    
    let running = Arc::new(AtomicBool::new(true));
    let mut handles = vec![];
    
    // Multiple concurrent load generators
    for worker_id in 0..4 {
        let running_clone = running.clone();
        let handle = thread::spawn(move || {
            let endpoints = [
                "http://localhost:8080/api.json",
                "http://localhost:8080/users.json", 
                "http://localhost:8080/health",
                "http://localhost:8080/dataset.json",
            ];
            
            let mut requests = 0;
            while running_clone.load(Ordering::Relaxed) {
                for endpoint in &endpoints {
                    if !running_clone.load(Ordering::Relaxed) { break; }
                    
                    // Make HTTP request
                    let _ = std::process::Command::new("curl")
                        .args(&["-s", "--max-time", "2", endpoint])
                        .output();
                    
                    requests += 1;
                    
                    // Brief pause to maintain realistic load pattern
                    thread::sleep(Duration::from_millis(10));
                }
            }
            println!("Worker {} completed {} requests", worker_id, requests);
        });
        handles.push(handle);
    }
    
    // Run workload for 30 seconds to get substantial profiling data
    thread::sleep(Duration::from_secs(30));
    
    println!("3. Stopping workload generation...");
    running.store(false, Ordering::Relaxed);
    
    // Wait for all workers to finish
    for handle in handles {
        handle.join().unwrap();
    }
    
    println!("4. Stopping UringRess server...");
    server.kill().unwrap();
    let _ = server.wait();
    
    println!("✓ Real workload runtime flamegraph test completed successfully");
    println!(""); 
    println!("⚠️  WARNING: This test profiles CLIENT-SIDE operations (curl), not server!");
    println!("");
    println!("This test exercises:");
    println!("- Real network I/O operations FROM CLIENT PERSPECTIVE");
    println!("- HTTP request generation and response parsing"); 
    println!("- Client connection management");
    println!("- Concurrent client request processing");
    println!("- Realistic client load patterns and timing");
    println!("");
    println!("❌ INCORRECT: This will profile curl, not UringRess server:");
    println!("cargo flamegraph --profile profiling --output tests/integration/reports/cpu/runtime_workload.svg \\");
    println!("  --test isolated_performance_profiling -- test_runtime_flamegraph_with_real_workload --nocapture");
    println!("");
    println!("✅ CORRECT: For server-side profiling use:");
    println!("tests/integration/profiling/profile_server_runtime.sh");
}