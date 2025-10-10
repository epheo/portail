use uringress::routing::{
    RouteTable, Backend, HttpRouteRule, HttpFilter, HttpHeader, URLRewritePath,
    PathMatchType, HeaderMatch, BackendSelector, FnvRouterHasher,
};

#[test]
fn test_fnv_hash_consistency() {
    let host = "example.com";
    let hash1 = FnvRouterHasher::hash(host);
    let hash2 = FnvRouterHasher::hash(host);
    assert_eq!(hash1, hash2);
}

#[test]
fn test_fnv_hash_different_values() {
    let hash1 = FnvRouterHasher::hash("example.com");
    let hash2 = FnvRouterHasher::hash("different.com");
    assert_ne!(hash1, hash2);
}

#[test]
fn test_route_table_creation() {
    let table = RouteTable::new();
    assert!(table.http_routes.is_empty());
    assert!(table.tcp_routes.is_empty());
}

#[test]
fn test_route_table_http_routing() {
    let mut table = RouteTable::new();

    let backend = Backend::new("192.168.1.100".to_string(), 8080).unwrap();
    table.add_http_route("api.example.com", HttpRouteRule {
        path_match_type: PathMatchType::Prefix,
        path: "/v1/users".to_string(),
        header_matches: vec![],
        filters: vec![],
        backends: vec![backend],
    });

    let result = table.find_http_route("api.example.com", "/v1/users/123", &[]);
    assert!(result.is_ok());

    let rule = result.unwrap();
    let backend_ref = &rule.backends[0];
    assert_eq!(backend_ref.socket_addr.ip(), std::net::IpAddr::V4("192.168.1.100".parse().unwrap()));
    assert_eq!(backend_ref.socket_addr.port(), 8080);
}

#[test]
fn test_route_table_http_routing_no_match() {
    let table = RouteTable::new();

    let result = table.find_http_route("nonexistent.com", "/api/test", &[]);
    assert!(result.is_err());
}

#[test]
fn test_route_table_tcp_routing() {
    let mut table = RouteTable::new();

    let backends = vec![
        Backend::new("192.168.1.100".to_string(), 5432).unwrap(),
        Backend::new("192.168.1.101".to_string(), 5432).unwrap(),
    ];
    table.add_tcp_route(5432, backends);

    let result = table.find_tcp_backends(5432);
    assert!(result.is_ok());

    let backends = result.unwrap();
    let backend_ref = &backends[0];
    assert!(format!("{}", backend_ref.socket_addr.ip()).starts_with("192.168.1.1"));
    assert_eq!(backend_ref.socket_addr.port(), 5432);
}

#[test]
fn test_route_table_tcp_routing_no_match() {
    let table = RouteTable::new();

    let result = table.find_tcp_backends(9999);
    assert!(result.is_err());
}

#[test]
fn test_backend_creation() {
    let backend = Backend::new("127.0.0.1".to_string(), 9000).unwrap();
    assert_eq!(format!("{}", backend.socket_addr.ip()), "127.0.0.1");
    assert_eq!(backend.socket_addr.port(), 9000);
}

#[test]
fn test_path_matching_longest_prefix_first() {
    let mut table = RouteTable::new();

    // Add routes in reverse order to test sorting
    let backend1 = Backend::new("127.0.0.1".to_string(), 8080).unwrap();
    let backend2 = Backend::new("127.0.0.2".to_string(), 8080).unwrap();

    table.add_http_route("api.com", HttpRouteRule {
        path_match_type: PathMatchType::Prefix,
        path: "/api".to_string(),
        header_matches: vec![],
        filters: vec![],
        backends: vec![backend1],
    });
    table.add_http_route("api.com", HttpRouteRule {
        path_match_type: PathMatchType::Prefix,
        path: "/api/v1".to_string(),
        header_matches: vec![],
        filters: vec![],
        backends: vec![backend2],
    });

    // Should match longer prefix first
    let result = table.find_http_route("api.com", "/api/v1/users", &[]);
    assert!(result.is_ok());
    let rule = result.unwrap();
    let backend_ref = &rule.backends[0];
    assert_eq!(format!("{}", backend_ref.socket_addr.ip()), "127.0.0.2");

    // Should match shorter prefix for non-v1 paths
    let result = table.find_http_route("api.com", "/api/v2/users", &[]);
    assert!(result.is_ok());
    let rule = result.unwrap();
    let backend_ref = &rule.backends[0];
    assert_eq!(format!("{}", backend_ref.socket_addr.ip()), "127.0.0.1");
}

// --- Gateway API HTTPRoute spec coverage ---

fn backend(port: u16) -> Backend {
    Backend { socket_addr: format!("127.0.0.1:{}", port).parse().unwrap(), weight: 1 }
}

fn rule(path_type: PathMatchType, path: &str, port: u16) -> HttpRouteRule {
    HttpRouteRule {
        path_match_type: path_type,
        path: path.to_string(),
        header_matches: vec![],
        filters: vec![],
        backends: vec![backend(port)],
    }
}

// Path matching

#[test]
fn test_exact_path_match_wins_over_prefix() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", rule(PathMatchType::Exact, "/foo", 8001));
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/foo", 8002));

    let r = rt.find_http_route("example.com", "/foo", &[]).unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8001);
}

#[test]
fn test_longest_prefix_wins() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/api", 8001));
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/api/v1", 8002));

    let r = rt.find_http_route("example.com", "/api/v1/users", &[]).unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8002);
}

#[test]
fn test_root_prefix_catches_all() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", 8001));

    assert!(rt.find_http_route("example.com", "/anything/here", &[]).is_ok());
    assert!(rt.find_http_route("example.com", "/", &[]).is_ok());
}

#[test]
fn test_exact_path_no_subpath() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", rule(PathMatchType::Exact, "/foo", 8001));

    assert!(rt.find_http_route("example.com", "/foo", &[]).is_ok());
    assert!(rt.find_http_route("example.com", "/foo/bar", &[]).is_err());
}

#[test]
fn test_prefix_trailing_slash_semantics() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/foo/", 8001));
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/foo", 8002));

    // "/foo/bar" matches "/foo/" (longer prefix)
    let r = rt.find_http_route("example.com", "/foo/bar", &[]).unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8001);

    // "/foo" matches "/foo" (exact length prefix)
    let r = rt.find_http_route("example.com", "/foo", &[]).unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8002);
}

// Host matching

#[test]
fn test_exact_host_priority_over_wildcard() {
    let mut rt = RouteTable::new();
    rt.add_http_route("*.example.com", rule(PathMatchType::Prefix, "/", 8001));
    rt.add_http_route("specific.example.com", rule(PathMatchType::Prefix, "/", 8002));

    let r = rt.find_http_route("specific.example.com", "/", &[]).unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8002);
}

#[test]
fn test_wildcard_host_matching_new() {
    let mut rt = RouteTable::new();
    rt.add_http_route("*.example.com", rule(PathMatchType::Prefix, "/", 8001));

    let r = rt.find_http_route("foo.example.com", "/", &[]).unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8001);
}

#[test]
fn test_wildcard_no_match_bare_domain() {
    let mut rt = RouteTable::new();
    rt.add_http_route("*.example.com", rule(PathMatchType::Prefix, "/", 8001));

    // "example.com" has no subdomain to strip — should not match *.example.com
    assert!(rt.find_http_route("example.com", "/", &[]).is_err());
}

#[test]
fn test_case_insensitive_host() {
    let mut rt = RouteTable::new();
    rt.add_http_route("api.example.com", rule(PathMatchType::Prefix, "/", 8001));

    let r = rt.find_http_route("API.Example.COM", "/", &[]).unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8001);
}

#[test]
fn test_host_with_port_stripped() {
    // The http_parser strips port from Host header before routing,
    // so we test that the route table matches without port
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", 8001));

    let r = rt.find_http_route("example.com", "/", &[]).unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8001);
}

// Header matching

#[test]
fn test_single_header_match() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", HttpRouteRule {
        path_match_type: PathMatchType::Prefix,
        path: "/".to_string(),
        header_matches: vec![HeaderMatch { name: "x-env".to_string(), value: "canary".to_string() }],
        filters: vec![],
        backends: vec![backend(8001)],
    });
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", 8002));

    let headers = b"X-Env: canary\r\n";
    let r = rt.find_http_route("example.com", "/", headers).unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8001);
}

#[test]
fn test_multiple_headers_and_logic() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", HttpRouteRule {
        path_match_type: PathMatchType::Prefix,
        path: "/".to_string(),
        header_matches: vec![
            HeaderMatch { name: "x-env".to_string(), value: "canary".to_string() },
            HeaderMatch { name: "x-region".to_string(), value: "us-east".to_string() },
        ],
        filters: vec![],
        backends: vec![backend(8001)],
    });
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", 8002));

    // Both headers present → match
    let headers = b"X-Env: canary\r\nX-Region: us-east\r\n";
    let r = rt.find_http_route("example.com", "/", headers).unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8001);

    // Only one header → fallback
    let headers = b"X-Env: canary\r\n";
    let r = rt.find_http_route("example.com", "/", headers).unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8002);
}

#[test]
fn test_header_match_case_insensitive() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", HttpRouteRule {
        path_match_type: PathMatchType::Prefix,
        path: "/".to_string(),
        header_matches: vec![HeaderMatch { name: "x-env".to_string(), value: "canary".to_string() }],
        filters: vec![],
        backends: vec![backend(8001)],
    });
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", 8002));

    // Header name case doesn't matter
    let headers = b"x-env: canary\r\n";
    let r = rt.find_http_route("example.com", "/", headers).unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8001);
}

#[test]
fn test_header_match_fallback_to_no_header_rule() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", HttpRouteRule {
        path_match_type: PathMatchType::Prefix,
        path: "/".to_string(),
        header_matches: vec![HeaderMatch { name: "x-env".to_string(), value: "canary".to_string() }],
        filters: vec![],
        backends: vec![backend(8001)],
    });
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", 8002));

    // No matching headers → falls through to default rule
    let headers = b"X-Env: production\r\n";
    let r = rt.find_http_route("example.com", "/", headers).unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8002);
}

// Weighted backends

#[test]
fn test_weighted_round_robin_distribution() {
    let backends = vec![
        Backend { socket_addr: "127.0.0.1:8001".parse().unwrap(), weight: 3 },
        Backend { socket_addr: "127.0.0.1:8002".parse().unwrap(), weight: 1 },
    ];

    let mut selector = BackendSelector::new();
    let mut counts = [0u32; 2];
    for _ in 0..400 {
        let idx = selector.select_weighted_backend(42, &backends);
        counts[idx] += 1;
    }
    assert_eq!(counts[0], 300);
    assert_eq!(counts[1], 100);
}

#[test]
fn test_equal_weights() {
    let backends = vec![
        Backend { socket_addr: "127.0.0.1:8001".parse().unwrap(), weight: 1 },
        Backend { socket_addr: "127.0.0.1:8002".parse().unwrap(), weight: 1 },
    ];

    let mut selector = BackendSelector::new();
    let mut counts = [0u32; 2];
    for _ in 0..100 {
        let idx = selector.select_weighted_backend(42, &backends);
        counts[idx] += 1;
    }
    assert_eq!(counts[0], 50);
    assert_eq!(counts[1], 50);
}

#[test]
fn test_zero_weight_backend_never_selected() {
    let backends = vec![
        Backend { socket_addr: "127.0.0.1:8001".parse().unwrap(), weight: 1 },
        Backend { socket_addr: "127.0.0.1:8002".parse().unwrap(), weight: 0 },
    ];

    let mut selector = BackendSelector::new();
    for _ in 0..100 {
        let idx = selector.select_weighted_backend(42, &backends);
        assert_eq!(idx, 0, "zero-weight backend should never be selected");
    }
}

// Filter presence on rules

#[test]
fn test_route_carries_filters() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", HttpRouteRule {
        path_match_type: PathMatchType::Prefix,
        path: "/".to_string(),
        header_matches: vec![],
        filters: vec![HttpFilter::RequestHeaderModifier {
            add: vec![HttpHeader { name: "X-Added".to_string(), value: "yes".to_string() }],
            set: vec![],
            remove: vec![],
        }],
        backends: vec![backend(8001)],
    });

    let r = rt.find_http_route("example.com", "/", &[]).unwrap();
    assert_eq!(r.filters.len(), 1);
    assert!(matches!(&r.filters[0], HttpFilter::RequestHeaderModifier { .. }));
}

#[test]
fn test_url_rewrite_on_route_rule() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", HttpRouteRule {
        path_match_type: PathMatchType::Prefix,
        path: "/v1".to_string(),
        header_matches: vec![],
        filters: vec![HttpFilter::URLRewrite {
            hostname: None,
            path: Some(URLRewritePath::ReplacePrefixMatch("/v2".to_string())),
        }],
        backends: vec![backend(8001)],
    });

    let r = rt.find_http_route("example.com", "/v1/users", &[]).unwrap();
    assert_eq!(r.filters.len(), 1);
    assert!(matches!(&r.filters[0], HttpFilter::URLRewrite { .. }));
}

#[test]
fn test_request_mirror_on_route_rule() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", HttpRouteRule {
        path_match_type: PathMatchType::Prefix,
        path: "/".to_string(),
        header_matches: vec![],
        filters: vec![HttpFilter::RequestMirror {
            backend_addr: "127.0.0.1:9999".parse().unwrap(),
        }],
        backends: vec![backend(8001)],
    });

    let r = rt.find_http_route("example.com", "/", &[]).unwrap();
    assert_eq!(r.filters.len(), 1);
    assert!(matches!(&r.filters[0], HttpFilter::RequestMirror { .. }));
}
