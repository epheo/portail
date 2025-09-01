use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use uringress::routing::{RouteTable, Backend, fnv_hash};
use std::time::Duration;

fn routing_benchmark(c: &mut Criterion) {
    // Create routing table with various scales for performance testing
    let mut small_table = RouteTable::new();
    let mut medium_table = RouteTable::new();
    let mut large_table = RouteTable::new();
    
    // Small table: 100 routes (typical microservice)
    for i in 0..100 {
        let host = format!("host{}.example.com", i);
        let backend = Backend::new(format!("backend{}", i), 8080).unwrap();
        small_table.add_http_route(&host, "/api", backend);
    }
    
    // Medium table: 1,000 routes (medium service)
    for i in 0..1000 {
        let host = format!("host{}.example.com", i);
        let backend = Backend::new(format!("backend{}", i), 8080).unwrap();
        medium_table.add_http_route(&host, "/api", backend);
    }
    
    // Large table: 10,000 routes (large enterprise service)
    for i in 0..10000 {
        let host = format!("host{}.example.com", i);
        let backend = Backend::new(format!("backend{}", i), 8080).unwrap();
        large_table.add_http_route(&host, "/api", backend);
    }
    
    // FNV Hash Performance - Target: <10ns (sub-microsecond)
    c.bench_function("fnv_hash_short_host", |b| {
        b.iter(|| {
            let host = "api.com";
            black_box(fnv_hash(host))
        })
    });
    
    c.bench_function("fnv_hash_long_host", |b| {
        b.iter(|| {
            let host = "very.long.subdomain.example.com";
            black_box(fnv_hash(host))
        })
    });
    
    // Route Lookup Performance - Target: O(1) regardless of table size
    let mut group = c.benchmark_group("route_lookup_by_table_size");
    
    for (size, table) in [
        (100, &small_table),
        (1000, &medium_table), 
        (10000, &large_table)
    ] {
        group.bench_with_input(BenchmarkId::new("http_route_lookup", size), &table, |b, table| {
            let host_hash = fnv_hash(&format!("host{}.example.com", size / 2));
            b.iter(|| {
                black_box(table.route_http_request(host_hash, "/api/users"))
            })
        });
    }
    group.finish();
    
    // Path Matching Performance - Multiple path prefixes
    let mut complex_table = RouteTable::new();
    let backend = Backend::new("backend".to_string(), 8080).unwrap();
    
    // Add routes with different path complexities
    complex_table.add_http_route("api.com", "/", backend.clone());
    complex_table.add_http_route("api.com", "/api", backend.clone());
    complex_table.add_http_route("api.com", "/api/v1", backend.clone());
    complex_table.add_http_route("api.com", "/api/v1/users", backend.clone());
    complex_table.add_http_route("api.com", "/api/v2", backend.clone());
    
    c.bench_function("path_matching_simple", |b| {
        let host_hash = fnv_hash("api.com");
        b.iter(|| {
            black_box(complex_table.route_http_request(host_hash, "/"))
        })
    });
    
    c.bench_function("path_matching_complex", |b| {
        let host_hash = fnv_hash("api.com");
        b.iter(|| {
            black_box(complex_table.route_http_request(host_hash, "/api/v1/users/123/profile"))
        })
    });
    
    // TCP Route Lookup Performance
    let mut tcp_table = RouteTable::new();
    for port in 8000..9000 {
        let backends = vec![Backend::new(format!("backend{}", port), port).unwrap()];
        tcp_table.add_tcp_route(port, backends);
    }
    
    c.bench_function("tcp_route_lookup", |b| {
        b.iter(|| {
            black_box(tcp_table.route_tcp_request(8500))
        })
    });
    
    // Backend Selection Performance
    let backends = (0..100).map(|i| Backend::new(format!("backend{}", i), 8080).unwrap()).collect();
    tcp_table.add_tcp_route(9999, backends);
    
    c.bench_function("backend_selection_round_robin", |b| {
        b.iter(|| {
            black_box(tcp_table.route_tcp_request(9999))
        })
    });
}

fn memory_benchmark(c: &mut Criterion) {
    // Memory allocation benchmarks
    c.bench_function("route_table_creation", |b| {
        b.iter(|| {
            black_box(RouteTable::new())
        })
    });
    
    c.bench_function("backend_creation", |b| {
        b.iter(|| {
            black_box(Backend::new("test.com".to_string(), 8080).unwrap())
        })
    });
    
    c.bench_function("add_http_route", |b| {
        b.iter_batched(
            || RouteTable::new(),
            |mut table| {
                let backend = Backend::new("test.com".to_string(), 8080).unwrap();
                table.add_http_route("host.com", "/api", backend);
                black_box(table)
            },
            criterion::BatchSize::SmallInput
        )
    });
}

// Configure benchmarks for MVP performance targets
fn config() -> Criterion {
    Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2))
        .sample_size(1000)
        .with_plots()
}

criterion_group! {
    name = routing_benches;
    config = config();
    targets = routing_benchmark, memory_benchmark
}
criterion_main!(routing_benches);