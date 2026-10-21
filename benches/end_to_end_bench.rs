use criterion::{criterion_group, criterion_main, Criterion};
use portail::proxy::health::HealthRegistry;
use portail::proxy::http_parser::extract_routing_info;
use portail::proxy::request_processor::analyze_request;
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
        let backend =
            Backend::new(backend_addr.to_string(), 8080).expect("Failed to create backend");
        table.add_http_route(host, make_rule(path, backend));
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
                let result = table.find_http_route(
                    info.host,
                    info.method,
                    info.path,
                    info.header_data,
                    info.query_string,
                    80,
                );
                let _ = std::hint::black_box(result);
            }
        })
    });

    c.bench_function("e2e_single_auth_request", |b| {
        let request = b"POST /login HTTP/1.1\r\nHost: auth.api.com\r\nContent-Type: application/json\r\nContent-Length: 50\r\n\r\n";
        b.iter(|| {
            let info = extract_routing_info(request).unwrap();
            let result = table.find_http_route(info.host, info.method, info.path, info.header_data, info.query_string, 80);
            let _ = std::hint::black_box(result);
        })
    });

    c.bench_function("e2e_single_product_search", |b| {
        let request = b"GET /search?q=electronics&category=phones&sort=price HTTP/1.1\r\nHost: products.api.com\r\nAccept: application/json\r\nUser-Agent: mobile-app/2.1\r\n\r\n";
        b.iter(|| {
            let info = extract_routing_info(request).unwrap();
            let result = table.find_http_route(info.host, info.method, info.path, info.header_data, info.query_string, 80);
            let _ = std::hint::black_box(result);
        })
    });

    c.bench_function("e2e_health_check_processing", |b| {
        let request = b"GET /health HTTP/1.1\r\nHost: api.com\r\n\r\n";
        let health_backend =
            Backend::new("127.0.0.1".to_string(), 8080).expect("Failed to create health backend");
        let mut health_table = RouteTable::new();
        health_table.add_http_route("api.com", make_rule("/health", health_backend));

        b.iter(|| {
            let info = extract_routing_info(request).unwrap();
            let result = health_table.find_http_route(
                info.host,
                info.method,
                info.path,
                info.header_data,
                info.query_string,
                80,
            );
            let _ = std::hint::black_box(result);
        })
    });

    c.bench_function("e2e_404_route_processing", |b| {
        let request = b"GET /nonexistent HTTP/1.1\r\nHost: unknown.api.com\r\n\r\n";
        b.iter(|| {
            let info = extract_routing_info(request).unwrap();
            let result = table.find_http_route(
                info.host,
                info.method,
                info.path,
                info.header_data,
                info.query_string,
                80,
            );
            let _ = std::hint::black_box(result);
        })
    });
}

fn throughput_simulation(c: &mut Criterion) {
    let mut table = RouteTable::new();

    for i in 0..100 {
        let host = format!("service{}.api.com", i);
        let backend =
            Backend::new("127.0.0.1".to_string(), 8080).expect("Failed to create backend");
        table.add_http_route(&host, make_rule("/api", backend));
    }

    let popular_requests: Vec<_> = (0..20)
        .map(|i| {
            format!(
                "GET /api/data HTTP/1.1\r\nHost: service{}.api.com\r\n\r\n",
                i
            )
        })
        .collect();

    let regular_requests: Vec<_> = (20..100)
        .map(|i| {
            format!(
                "GET /api/data HTTP/1.1\r\nHost: service{}.api.com\r\n\r\n",
                i
            )
        })
        .collect();

    c.bench_function("throughput_simulation_1000_requests", |b| {
        b.iter(|| {
            for _ in 0..800 {
                for request in popular_requests.iter().take(4) {
                    let info = extract_routing_info(request.as_bytes()).unwrap();
                    let result = table.find_http_route(
                        info.host,
                        info.method,
                        info.path,
                        info.header_data,
                        info.query_string,
                        80,
                    );
                    let _ = std::hint::black_box(result);
                }
            }

            for request in regular_requests.iter().take(200) {
                let info = extract_routing_info(request.as_bytes()).unwrap();
                let result = table.find_http_route(
                    info.host,
                    info.method,
                    info.path,
                    info.header_data,
                    info.query_string,
                    80,
                );
                let _ = std::hint::black_box(result);
            }
        })
    });
}

fn latency_scenarios(c: &mut Criterion) {
    let mut table = RouteTable::new();
    let backend =
        Backend::new("127.0.0.1".to_string(), 8080).expect("Failed to create fast backend");
    table.add_http_route("fast.api.com", make_rule("/", backend));

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
                let result = table.find_http_route(
                    info.host,
                    info.method,
                    info.path,
                    info.header_data,
                    info.query_string,
                    80,
                );
                let _ = std::hint::black_box(result);
            })
        });
    }
}

/// `analyze_request` per request: parse, route lookup, weighted backend
/// selection and the invalid-backendRef check on the selected backend.
fn request_analysis(c: &mut Criterion) {
    let mut table = RouteTable::new();
    let backends = vec![
        Backend::with_weight("127.0.0.1".to_string(), 8080, 3).unwrap(),
        Backend::with_weight("127.0.0.1".to_string(), 8081, 1).unwrap(),
    ];
    table.add_http_route(
        "api.example.com",
        HttpRouteRule::new(
            PathMatchType::Prefix,
            "/".to_string(),
            vec![],
            vec![],
            vec![],
            backends,
        ),
    );
    let selector = BackendSelector::new();
    let health = HealthRegistry::new();
    let request = b"GET /users/42 HTTP/1.1\r\nHost: api.example.com\r\nAccept: */*\r\n\r\n";

    c.bench_function("e2e_analyze_request_weighted_backends", |b| {
        b.iter(|| {
            let result = analyze_request(
                &table,
                &selector,
                std::hint::black_box(request),
                std::hint::black_box(80),
                &health,
                false,
            );
            std::hint::black_box(result.is_ok())
        })
    });
}

/// `analyze_request` by path shape: only a dot-segment pays the rewrite
/// path normalization makes before the route match.
fn path_analysis(c: &mut Criterion) {
    let mut table = RouteTable::new();
    for path in ["/", "/admin", "/static"] {
        let backend = Backend::new("127.0.0.1".to_string(), 8080).unwrap();
        table.add_http_route("app.api.com", make_rule(path, backend));
    }
    let selector = BackendSelector::new();
    let health = HealthRegistry::new();

    for (name, request) in [
        (
            "e2e_analyze_path_clean",
            &b"GET /admin/users/42/profile?tab=1 HTTP/1.1\r\nHost: app.api.com\r\nAccept: */*\r\n\r\n"[..],
        ),
        (
            "e2e_analyze_path_dotted",
            b"GET /static/js/vendor.min.js?v=1.2 HTTP/1.1\r\nHost: app.api.com\r\nAccept: */*\r\n\r\n",
        ),
        (
            "e2e_analyze_path_dot_segments",
            b"GET /public/../admin/./users/42/profile?tab=1 HTTP/1.1\r\nHost: app.api.com\r\nAccept: */*\r\n\r\n",
        ),
    ] {
        c.bench_function(name, |b| {
            b.iter(|| {
                analyze_request(
                    black_box(&table),
                    &selector,
                    black_box(request),
                    black_box(80),
                    &health,
                    false,
                )
            })
        });
    }
}

/// Path modifiers per request: a redirect's Location (prefix join plus
/// the query join), and a URLRewrite's prefix join. A value's own query
/// was split off when its rule compiled: no request scans the value.
fn path_modifiers(c: &mut Criterion) {
    use portail::proxy::http_filters::extract_url_rewrite;
    use portail::routing::{HttpFilter, URLRewritePath};

    let mut table = RouteTable::new();
    let redirect = HttpFilter::RequestRedirect {
        scheme: Some("https".to_string()),
        hostname: None,
        port: None,
        path: Some(URLRewritePath::ReplacePrefixMatch("/new".into())),
        status_code: 302,
    };
    table.add_http_route(
        "app.api.com",
        HttpRouteRule::new(
            PathMatchType::Prefix,
            "/old".to_string(),
            vec![],
            vec![],
            vec![redirect],
            vec![],
        ),
    );
    let selector = BackendSelector::new();
    let health = HealthRegistry::new();
    let request = b"GET /old/users/42?tab=1 HTTP/1.1\r\nHost: app.api.com\r\nAccept: */*\r\n\r\n";
    c.bench_function("e2e_analyze_redirect_prefix_with_query", |b| {
        b.iter(|| {
            analyze_request(
                black_box(&table),
                &selector,
                black_box(&request[..]),
                black_box(80),
                &health,
                false,
            )
        })
    });

    let rewrite = [HttpFilter::URLRewrite {
        hostname: None,
        path: Some(URLRewritePath::ReplacePrefixMatch("/new".into())),
    }];
    c.bench_function("url_rewrite_prefix", |b| {
        b.iter(|| extract_url_rewrite(black_box(&rewrite), "/old", black_box("/old/users/42")))
    });

    // A full path, its query and a hostname: the rule's, borrowed.
    let rewrite = [HttpFilter::URLRewrite {
        hostname: Some("internal.api.com".to_string()),
        path: Some(URLRewritePath::ReplaceFullPath("/new?src=gw".into())),
    }];
    c.bench_function("url_rewrite_full_path", |b| {
        b.iter(|| extract_url_rewrite(black_box(&rewrite), "/old", black_box("/old/users/42")))
    });

    // A full-path redirect whose value carries a query, joined with the
    // request's: the '&' decision reads the value's last byte only.
    let mut table = RouteTable::new();
    let redirect = HttpFilter::RequestRedirect {
        scheme: None,
        hostname: None,
        port: None,
        path: Some(URLRewritePath::ReplaceFullPath("/login?next=home".into())),
        status_code: 302,
    };
    table.add_http_route(
        "app.api.com",
        HttpRouteRule::new(
            PathMatchType::Prefix,
            "/old".to_string(),
            vec![],
            vec![],
            vec![redirect],
            vec![],
        ),
    );
    c.bench_function("e2e_analyze_redirect_full_value_query", |b| {
        b.iter(|| {
            analyze_request(
                black_box(&table),
                &selector,
                black_box(&request[..]),
                black_box(80),
                &health,
                false,
            )
        })
    });
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
    targets = end_to_end_benchmark, throughput_simulation, latency_scenarios, request_analysis, path_analysis, path_modifiers
}
criterion_main!(e2e_benches);
