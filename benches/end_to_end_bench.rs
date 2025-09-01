use criterion::{black_box, criterion_group, criterion_main, Criterion};
use uringress::parser::parse_http_headers_fast;
use uringress::routing::{RouteTable, Backend, fnv_hash};
use std::time::Duration;

fn end_to_end_benchmark(c: &mut Criterion) {
    // Setup a realistic routing table
    let mut table = RouteTable::new();
    
    // Add typical microservice routes
    let services = [
        ("auth.api.com", "/login", "auth-service:8080"),
        ("auth.api.com", "/register", "auth-service:8080"),
        ("users.api.com", "/profile", "user-service:8080"),
        ("users.api.com", "/settings", "user-service:8080"),
        ("orders.api.com", "/create", "order-service:8080"),
        ("orders.api.com", "/history", "order-service:8080"),
        ("products.api.com", "/search", "product-service:8080"),
        ("products.api.com", "/details", "product-service:8080"),
        ("payments.api.com", "/process", "payment-service:8080"),
        ("analytics.api.com", "/reports", "analytics-service:8080"),
    ];
    
    for (host, path, backend_addr) in &services {
        let backend = Backend::new(backend_addr.to_string(), 8080).expect("Failed to create backend");
        table.add_http_route(host, path, backend);
    }
    
    // Typical HTTP requests
    let requests: &[&[u8]] = &[
        b"GET /login HTTP/1.1\r\nHost: auth.api.com\r\nUser-Agent: curl/7.68.0\r\n\r\n",
        b"POST /register HTTP/1.1\r\nHost: auth.api.com\r\nContent-Type: application/json\r\n\r\n",
        b"GET /profile HTTP/1.1\r\nHost: users.api.com\r\nAuthorization: Bearer token123\r\n\r\n",
        b"GET /search?q=laptop HTTP/1.1\r\nHost: products.api.com\r\nAccept: application/json\r\n\r\n",
        b"POST /process HTTP/1.1\r\nHost: payments.api.com\r\nContent-Type: application/json\r\n\r\n",
    ];
    
    // End-to-end processing: Parse + Route + Select Backend
    // Target: <10μs total (parse <1μs + route <1μs + backend selection <1μs + overhead <7μs)
    c.bench_function("e2e_http_request_processing", |b| {
        b.iter(|| {
            for request in requests {
                // Parse HTTP request
                let (method, path, host) = parse_http_headers_fast(request).unwrap();
                
                // Route request
                if let Some(host) = host {
                    let host_hash = fnv_hash(host);
                    let backend = table.route_http_request(host_hash, path);
                    black_box((method, backend));
                }
            }
        })
    });
    
    // Individual request processing
    c.bench_function("e2e_single_auth_request", |b| {
        let request = b"POST /login HTTP/1.1\r\nHost: auth.api.com\r\nContent-Type: application/json\r\nContent-Length: 50\r\n\r\n";
        b.iter(|| {
            let (method, path, host) = parse_http_headers_fast(request).unwrap();
            if let Some(host) = host {
                let host_hash = fnv_hash(host);
                let backend = table.route_http_request(host_hash, path);
                black_box((method, backend));
            }
        })
    });
    
    c.bench_function("e2e_single_product_search", |b| {
        let request = b"GET /search?q=electronics&category=phones&sort=price HTTP/1.1\r\nHost: products.api.com\r\nAccept: application/json\r\nUser-Agent: mobile-app/2.1\r\n\r\n";
        b.iter(|| {
            let (method, path, host) = parse_http_headers_fast(request).unwrap();
            if let Some(host) = host {
                let host_hash = fnv_hash(host);
                let backend = table.route_http_request(host_hash, path);
                black_box((method, backend));
            }
        })
    });
    
    // High-frequency scenarios
    c.bench_function("e2e_health_check_processing", |b| {
        let request = b"GET /health HTTP/1.1\r\nHost: api.com\r\n\r\n";
        let health_backend = Backend::new("health-service:8080".to_string(), 8080).expect("Failed to create health backend");
        let mut health_table = RouteTable::new();
        health_table.add_http_route("api.com", "/health", health_backend);
        
        b.iter(|| {
            let (method, path, host) = parse_http_headers_fast(request).unwrap();
            if let Some(host) = host {
                let host_hash = fnv_hash(host);
                let backend = health_table.route_http_request(host_hash, path);
                black_box((method, backend));
            }
        })
    });
    
    // Error path processing
    c.bench_function("e2e_404_route_processing", |b| {
        let request = b"GET /nonexistent HTTP/1.1\r\nHost: unknown.api.com\r\n\r\n";
        b.iter(|| {
            let (method, path, host) = parse_http_headers_fast(request).unwrap();
            if let Some(host) = host {
                let host_hash = fnv_hash(host);
                let backend = table.route_http_request(host_hash, path);
                black_box((method, backend));
            }
        })
    });
}

fn throughput_simulation(c: &mut Criterion) {
    // Simulate realistic request distribution
    let mut table = RouteTable::new();
    
    // Add backends for popular services
    for i in 0..100 {
        let host = format!("service{}.api.com", i);
        let backend = Backend::new(format!("backend{}", i), 8080).expect("Failed to create backend");
        table.add_http_route(&host, "/api", backend);
    }
    
    // Generate request distribution (80/20 rule - 20% of services handle 80% of traffic)
    let popular_requests: Vec<_> = (0..20)
        .map(|i| format!("GET /api/data HTTP/1.1\r\nHost: service{}.api.com\r\n\r\n", i))
        .collect();
    
    let regular_requests: Vec<_> = (20..100)
        .map(|i| format!("GET /api/data HTTP/1.1\r\nHost: service{}.api.com\r\n\r\n", i))
        .collect();
    
    // Simulate 1000 requests with realistic distribution
    c.bench_function("throughput_simulation_1000_requests", |b| {
        b.iter(|| {
            // Process 800 requests to popular services
            for _ in 0..800 {
                for request in popular_requests.iter().take(4) { // Cycle through top 4 services
                    let (method, path, host) = parse_http_headers_fast(request.as_bytes()).unwrap();
                    if let Some(host) = host {
                        let host_hash = fnv_hash(host);
                        let backend = table.route_http_request(host_hash, path);
                        black_box((method, backend));
                    }
                }
            }
            
            // Process 200 requests to regular services
            for request in regular_requests.iter().take(200) {
                let (method, path, host) = parse_http_headers_fast(request.as_bytes()).unwrap();
                if let Some(host) = host {
                    let host_hash = fnv_hash(host);
                    let backend = table.route_http_request(host_hash, path);
                    black_box((method, backend));
                }
            }
        })
    });
}

fn latency_scenarios(c: &mut Criterion) {
    let mut table = RouteTable::new();
    let backend = Backend::new("fast-service".to_string(), 8080).expect("Failed to create fast backend");
    table.add_http_route("fast.api.com", "/", backend);
    
    // Test various request complexities that might affect latency
    let scenarios = [
        ("simple_get", b"GET / HTTP/1.1\r\nHost: fast.api.com\r\n\r\n" as &[u8]),
        ("with_auth", b"GET / HTTP/1.1\r\nHost: fast.api.com\r\nAuthorization: Bearer very-long-jwt-token-here-that-might-slow-things-down\r\n\r\n"),
        ("with_cookies", b"GET / HTTP/1.1\r\nHost: fast.api.com\r\nCookie: session=abc123; prefs=xyz789; tracking=def456\r\n\r\n"),
        ("complex_path", b"GET /api/v2/users/12345/profile/settings/notifications?detailed=true HTTP/1.1\r\nHost: fast.api.com\r\n\r\n"),
    ];
    
    for (name, request) in &scenarios {
        c.bench_function(&format!("latency_scenario_{}", name), |b| {
            b.iter(|| {
                let (method, path, host) = parse_http_headers_fast(request).unwrap();
                if let Some(host) = host {
                    let host_hash = fnv_hash(host);
                    let backend = table.route_http_request(host_hash, path);
                    black_box((method, backend));
                }
            })
        });
    }
}

// Configure for high precision latency measurement
fn config() -> Criterion {
    Criterion::default()
        .warm_up_time(Duration::from_millis(1000))
        .measurement_time(Duration::from_secs(3))
        .sample_size(10000)
        .significance_level(0.01)
        .noise_threshold(0.02)
        .with_plots()
}

criterion_group! {
    name = e2e_benches;
    config = config();
    targets = end_to_end_benchmark, throughput_simulation, latency_scenarios
}
criterion_main!(e2e_benches);