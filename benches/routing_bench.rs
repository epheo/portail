use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};
use portail::routing::{RouteTable, Backend, HttpRouteRule, PathMatchType};
use std::time::Duration;

fn make_rule(path: &str, backend: Backend) -> HttpRouteRule {
    HttpRouteRule::new(PathMatchType::Prefix, path.to_string(), vec![], vec![], vec![], vec![backend])
}

fn routing_benchmark(c: &mut Criterion) {
    let mut small_table = RouteTable::new();
    let mut medium_table = RouteTable::new();
    let mut large_table = RouteTable::new();

    for i in 0..100 {
        let host = format!("host{}.example.com", i);
        let backend = Backend::new("127.0.0.1".to_string(), 8080).unwrap();
        small_table.add_http_route(&host, make_rule("/api", backend));
    }

    for i in 0..1000 {
        let host = format!("host{}.example.com", i);
        let backend = Backend::new("127.0.0.1".to_string(), 8080).unwrap();
        medium_table.add_http_route(&host, make_rule("/api", backend));
    }

    for i in 0..10000 {
        let host = format!("host{}.example.com", i);
        let backend = Backend::new("127.0.0.1".to_string(), 8080).unwrap();
        large_table.add_http_route(&host, make_rule("/api", backend));
    }

    let mut group = c.benchmark_group("route_lookup_by_table_size");

    for (size, table) in [
        (100, &small_table),
        (1000, &medium_table),
        (10000, &large_table)
    ] {
        let host = format!("host{}.example.com", size / 2);
        group.bench_with_input(BenchmarkId::new("http_route_lookup", size), &table, |b, table| {
            b.iter(|| {
                std::hint::black_box(table.find_http_route(&host, "GET", "/api/users", &[], ""))
            })
        });
    }
    group.finish();

    let mut complex_table = RouteTable::new();

    for path in ["/", "/api", "/api/v1", "/api/v1/users", "/api/v2"] {
        let backend = Backend::new("127.0.0.1".to_string(), 8080).unwrap();
        complex_table.add_http_route("api.com", make_rule(path, backend));
    }

    c.bench_function("path_matching_simple", |b| {
        b.iter(|| {
            std::hint::black_box(complex_table.find_http_route("api.com", "GET", "/", &[], ""))
        })
    });

    c.bench_function("path_matching_complex", |b| {
        b.iter(|| {
            std::hint::black_box(complex_table.find_http_route("api.com", "GET", "/api/v1/users/123/profile", &[], ""))
        })
    });

    let mut tcp_table = RouteTable::new();
    for port in 8000..9000 {
        let backends = vec![Backend::new("127.0.0.1".to_string(), port).unwrap()];
        tcp_table.add_tcp_route(port, backends);
    }

    c.bench_function("tcp_route_lookup", |b| {
        b.iter(|| {
            std::hint::black_box(tcp_table.find_tcp_backends(8500))
        })
    });

    let backends = (0..100).map(|_| Backend::new("127.0.0.1".to_string(), 8080).unwrap()).collect();
    tcp_table.add_tcp_route(9999, backends);

    c.bench_function("backend_selection_round_robin", |b| {
        b.iter(|| {
            std::hint::black_box(tcp_table.find_tcp_backends(9999))
        })
    });
}

fn memory_benchmark(c: &mut Criterion) {
    c.bench_function("route_table_creation", |b| {
        b.iter(|| {
            std::hint::black_box(RouteTable::new())
        })
    });

    c.bench_function("backend_creation", |b| {
        b.iter(|| {
            std::hint::black_box(Backend::new("127.0.0.1".to_string(), 8080).unwrap())
        })
    });

    c.bench_function("add_http_route", |b| {
        b.iter_batched(
            || RouteTable::new(),
            |mut table| {
                let backend = Backend::new("127.0.0.1".to_string(), 8080).unwrap();
                table.add_http_route("host.com", make_rule("/api", backend));
                std::hint::black_box(table)
            },
            criterion::BatchSize::SmallInput
        )
    });
}

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
