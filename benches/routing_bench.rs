use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use portail::proxy::health::HealthRegistry;
use portail::proxy::request_processor::analyze_udp_request;
use portail::routing::{Backend, BackendSelector, HttpRouteRule, PathMatchType, RouteTable};
use std::hint::black_box;
use std::time::Duration;

fn make_rule(path: &str, backend: Backend) -> HttpRouteRule {
    HttpRouteRule::new(
        PathMatchType::Prefix,
        path.to_string(),
        vec![],
        vec![],
        vec![],
        vec![backend],
    )
}

/// One HTTP lookup with every input opaque: a constant input lets the
/// compiler fold the lookup or hoist it out of the timing loop.
fn lookup<'a>(table: &'a RouteTable, host: &str, path: &str) -> Option<&'a HttpRouteRule> {
    black_box(table).find_http_route(
        black_box(host),
        black_box("GET"),
        black_box(path),
        black_box(&[]),
        black_box(""),
        black_box(443),
    )
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
        (10000, &large_table),
    ] {
        let host = format!("host{}.example.com", size / 2);
        group.bench_with_input(
            BenchmarkId::new("http_route_lookup", size),
            &table,
            |b, table| b.iter(|| lookup(table, &host, "/api/users")),
        );
    }
    group.finish();

    // What a 404 flood repeats: a host no listener serves, and a served
    // host's unrouted path.
    let mut miss_group = c.benchmark_group("route_miss");
    for (name, host, path) in [
        ("unknown_host", "nope.example.com", "/api/users"),
        ("unrouted_path", "host50.example.com", "/admin"),
    ] {
        miss_group.bench_function(name, |b| b.iter(|| lookup(&small_table, host, path)));
    }
    miss_group.finish();

    let mut complex_table = RouteTable::new();

    for path in ["/", "/api", "/api/v1", "/api/v1/users", "/api/v2"] {
        let backend = Backend::new("127.0.0.1".to_string(), 8080).unwrap();
        complex_table.add_http_route("api.com", make_rule(path, backend));
    }

    c.bench_function("path_matching_simple", |b| {
        b.iter(|| lookup(&complex_table, "api.com", "/"))
    });

    c.bench_function("path_matching_complex", |b| {
        b.iter(|| lookup(&complex_table, "api.com", "/api/v1/users/123/profile"))
    });

    let mut tcp_table = RouteTable::new();
    for port in 8000..9000 {
        let backends = vec![Backend::new("127.0.0.1".to_string(), port).unwrap()];
        tcp_table.add_tcp_route(port, backends);
    }

    c.bench_function("tcp_route_lookup", |b| {
        b.iter(|| black_box(&tcp_table).find_tcp_backends(black_box(8500)))
    });

    // Every backend healthy, as in steady state: what is timed is the
    // weighted round robin itself.
    let selector = BackendSelector::new();
    let health = HealthRegistry::new();

    // What each datagram of a flood at a port with no route costs: the
    // lookup miss and the throttled log line.
    c.bench_function("udp_route_miss", |b| {
        b.iter(|| analyze_udp_request(black_box(&tcp_table), &selector, black_box(9999), &health))
    });
    let mut selection = c.benchmark_group("backend_selection");

    let l4_backends: Vec<Backend> = (0..100)
        .map(|i| Backend::new("127.0.0.1".to_string(), 8000 + i).unwrap())
        .collect();
    selection.bench_function("l4_round_robin_100", |b| {
        b.iter(|| selector.select_l4_backend(black_box(9999), black_box(&l4_backends), &health))
    });

    let mut weighted_table = RouteTable::new();
    let weighted = (1..=4)
        .map(|w| Backend::with_weight("127.0.0.1".to_string(), 8000 + w as u16, w).unwrap())
        .collect();
    weighted_table.add_http_route(
        "weighted.com",
        HttpRouteRule::new(
            PathMatchType::Prefix,
            "/".to_string(),
            vec![],
            vec![],
            vec![],
            weighted,
        ),
    );
    // Taken back from the table, which precomputes the cumulative weights.
    let rule = weighted_table
        .find_http_route("weighted.com", "GET", "/", &[], "", 443)
        .unwrap();
    selection.bench_function("http_weighted_4", |b| {
        b.iter(|| selector.select_healthy_weighted_backend(black_box(rule), &health))
    });
    selection.finish();
}

fn memory_benchmark(c: &mut Criterion) {
    c.bench_function("route_table_creation", |b| {
        b.iter(|| black_box(RouteTable::new()))
    });

    c.bench_function("backend_creation", |b| {
        b.iter(|| black_box(Backend::new("127.0.0.1".to_string(), 8080).unwrap()))
    });

    c.bench_function("add_http_route", |b| {
        b.iter_batched(
            RouteTable::new,
            |mut table| {
                let backend = Backend::new("127.0.0.1".to_string(), 8080).unwrap();
                table.add_http_route("host.com", make_rule("/api", backend));
                black_box(table)
            },
            criterion::BatchSize::SmallInput,
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

/// A file config with one host and the given rules, as JSON.
fn route_config(rules: &[String]) -> portail::config::PortailConfig {
    let json = format!(
        r#"{{"gateway":{{"name":"g","listeners":[{{"name":"h","protocol":"HTTP","port":443}}]}},
            "httpRoutes":[{{"hostnames":["api.com"],"rules":[{}]}}]}}"#,
        rules.join(",")
    );
    serde_json::from_str(&json).unwrap()
}

fn config_benchmark(c: &mut Criterion) {
    // A regex path match is a full match (the pattern compiles anchored).
    let regex_table = route_config(&[
        r#"{"matches":[{"path":{"type":"RegularExpression","value":"/api/v[0-9]+/users/[0-9]+"}}],"backendRefs":[{"name":"127.0.0.1","port":8080}]}"#.to_string(),
    ])
    .to_route_table()
    .unwrap();
    for (name, path) in [
        ("path_matching_regex_hit", "/api/v2/users/42"),
        ("path_matching_regex_miss", "/web/static/app/index.html"),
    ] {
        c.bench_function(name, |b| b.iter(|| lookup(&regex_table, "api.com", path)));
    }

    // Control plane, not per request: one host's rules sort once per build.
    let many_rules: Vec<String> = (0..1000)
        .map(|i| {
            format!(
                r#"{{"matches":[{{"path":{{"type":"PathPrefix","value":"/p{i}"}}}}],"backendRefs":[{{"name":"127.0.0.1","port":8080}}]}}"#
            )
        })
        .collect();
    let many = route_config(&many_rules);
    c.bench_function("route_table_build_1000_rules", |b| {
        b.iter(|| black_box(&many).to_route_table().unwrap())
    });
}

criterion_group! {
    name = routing_benches;
    config = config();
    targets = routing_benchmark, memory_benchmark, config_benchmark
}
criterion_main!(routing_benches);
