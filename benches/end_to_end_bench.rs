use criterion::{criterion_group, criterion_main, Criterion};
use uringress::http_parser::extract_routing_info;
use uringress::routing::{RouteTable, Backend};
use std::time::Duration;

fn end_to_end_benchmark(c: &mut Criterion) {
    let mut table = RouteTable::new();

    let services = [
        ("auth.api.com", "/login", "127.0.0.1"),
        ("auth.api.com", "/register", "127.0.0.1"),
        ("users.api.com", "/profile", "127.0.0.1"),
        ("users.api.com", "/settings", "127.0.0.1"),
        ("orders.api.com", "/create", "127.0.0.1"),
        ("orders.api.com", "/history", "127.0.0.1"),
        ("products.api.com", "/search", "127.0.0.1"),
        ("products.api.com", "/details", "127.0.0.1"),
        ("payments.api.com", "/process", "127.0.0.1"),
        ("analytics.api.com", "/reports", "127.0.0.1"),
    ];

    for (host, path, backend_addr) in &services {
        let backend = Backend::new(backend_addr.to_string(), 8080).expect("Failed to create backend");
        table.add_http_route(host, path, vec![backend]);
    }

    let requests: &[&[u8]] = &[
        b"GET /login HTTP/1.1\r\nHost: auth.api.com\r\nUser-Agent: curl/7.68.0\r\n\r\n",
        b"POST /register HTTP/1.1\r\nHost: auth.api.com\r\nContent-Type: application/json\r\n\r\n",
        b"GET /profile HTTP/1.1\r\nHost: users.api.com\r\nAuthorization: Bearer token123\r\n\r\n",
        b"GET /search?q=laptop HTTP/1.1\r\nHost: products.api.com\r\nAccept: application/json\r\n\r\n",
        b"POST /process HTTP/1.1\r\nHost: payments.api.com\r\nContent-Type: application/json\r\n\r\n",
    ];

    c.bench_function("e2e_http_request_processing", |b| {
        b.iter(|| {
            for request in requests {
                let info = extract_routing_info(request).unwrap();
                let backend = table.find_http_backends(info.host, info.path);
                let _ = std::hint::black_box(backend);
            }
        })
    });

    c.bench_function("e2e_single_auth_request", |b| {
        let request = b"POST /login HTTP/1.1\r\nHost: auth.api.com\r\nContent-Type: application/json\r\nContent-Length: 50\r\n\r\n";
        b.iter(|| {
            let info = extract_routing_info(request).unwrap();
            let backend = table.find_http_backends(info.host, info.path);
            let _ = std::hint::black_box(backend);
        })
    });

    c.bench_function("e2e_single_product_search", |b| {
        let request = b"GET /search?q=electronics&category=phones&sort=price HTTP/1.1\r\nHost: products.api.com\r\nAccept: application/json\r\nUser-Agent: mobile-app/2.1\r\n\r\n";
        b.iter(|| {
            let info = extract_routing_info(request).unwrap();
            let backend = table.find_http_backends(info.host, info.path);
            let _ = std::hint::black_box(backend);
        })
    });

    c.bench_function("e2e_health_check_processing", |b| {
        let request = b"GET /health HTTP/1.1\r\nHost: api.com\r\n\r\n";
        let health_backend = Backend::new("127.0.0.1".to_string(), 8080).expect("Failed to create health backend");
        let mut health_table = RouteTable::new();
        health_table.add_http_route("api.com", "/health", vec![health_backend]);

        b.iter(|| {
            let info = extract_routing_info(request).unwrap();
            let backend = health_table.find_http_backends(info.host, info.path);
            let _ = std::hint::black_box(backend);
        })
    });

    c.bench_function("e2e_404_route_processing", |b| {
        let request = b"GET /nonexistent HTTP/1.1\r\nHost: unknown.api.com\r\n\r\n";
        b.iter(|| {
            let info = extract_routing_info(request).unwrap();
            let backend = table.find_http_backends(info.host, info.path);
            let _ = std::hint::black_box(backend);
        })
    });
}

fn throughput_simulation(c: &mut Criterion) {
    let mut table = RouteTable::new();

    for i in 0..100 {
        let host = format!("service{}.api.com", i);
        let backend = Backend::new("127.0.0.1".to_string(), 8080).expect("Failed to create backend");
        table.add_http_route(&host, "/api", vec![backend]);
    }

    let popular_requests: Vec<_> = (0..20)
        .map(|i| format!("GET /api/data HTTP/1.1\r\nHost: service{}.api.com\r\n\r\n", i))
        .collect();

    let regular_requests: Vec<_> = (20..100)
        .map(|i| format!("GET /api/data HTTP/1.1\r\nHost: service{}.api.com\r\n\r\n", i))
        .collect();

    c.bench_function("throughput_simulation_1000_requests", |b| {
        b.iter(|| {
            for _ in 0..800 {
                for request in popular_requests.iter().take(4) {
                    let info = extract_routing_info(request.as_bytes()).unwrap();
                    let backend = table.find_http_backends(info.host, info.path);
                    let _ = std::hint::black_box(backend);
                }
            }

            for request in regular_requests.iter().take(200) {
                let info = extract_routing_info(request.as_bytes()).unwrap();
                let backend = table.find_http_backends(info.host, info.path);
                let _ = std::hint::black_box(backend);
            }
        })
    });
}

fn latency_scenarios(c: &mut Criterion) {
    let mut table = RouteTable::new();
    let backend = Backend::new("127.0.0.1".to_string(), 8080).expect("Failed to create fast backend");
    table.add_http_route("fast.api.com", "/", vec![backend]);

    let scenarios = [
        ("simple_get", b"GET / HTTP/1.1\r\nHost: fast.api.com\r\n\r\n" as &[u8]),
        ("with_auth", b"GET / HTTP/1.1\r\nHost: fast.api.com\r\nAuthorization: Bearer very-long-jwt-token-here-that-might-slow-things-down\r\n\r\n"),
        ("with_cookies", b"GET / HTTP/1.1\r\nHost: fast.api.com\r\nCookie: session=abc123; prefs=xyz789; tracking=def456\r\n\r\n"),
        ("complex_path", b"GET /api/v2/users/12345/profile/settings/notifications?detailed=true HTTP/1.1\r\nHost: fast.api.com\r\n\r\n"),
    ];

    for (name, request) in &scenarios {
        c.bench_function(&format!("latency_scenario_{}", name), |b| {
            b.iter(|| {
                let info = extract_routing_info(request).unwrap();
                let backend = table.find_http_backends(info.host, info.path);
                let _ = std::hint::black_box(backend);
            })
        });
    }
}

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
