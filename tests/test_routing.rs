use uringress::routing::{
    RouteTable, Backend, HttpRouteRule, HttpFilter, HttpHeader, URLRewritePath,
    PathMatchType, HeaderMatch, QueryParamMatch, BackendSelector,
};

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
    table.add_http_route("api.example.com", HttpRouteRule::new(
        PathMatchType::Prefix, "/v1/users".to_string(), vec![], vec![], vec![],
        vec![backend],
    ));

    let result = table.find_http_route("api.example.com", "GET", "/v1/users/123", &[], "");
    assert!(result.is_ok());

    let rule = result.unwrap();
    let backend_ref = &rule.backends[0];
    assert_eq!(backend_ref.socket_addr.ip(), std::net::IpAddr::V4("192.168.1.100".parse().unwrap()));
    assert_eq!(backend_ref.socket_addr.port(), 8080);
}

#[test]
fn test_route_table_http_routing_no_match() {
    let table = RouteTable::new();

    let result = table.find_http_route("nonexistent.com", "GET", "/api/test", &[], "");
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

    let backend1 = Backend::new("127.0.0.1".to_string(), 8080).unwrap();
    let backend2 = Backend::new("127.0.0.2".to_string(), 8080).unwrap();

    table.add_http_route("api.com", HttpRouteRule::new(
        PathMatchType::Prefix, "/api".to_string(), vec![], vec![], vec![],
        vec![backend1],
    ));
    table.add_http_route("api.com", HttpRouteRule::new(
        PathMatchType::Prefix, "/api/v1".to_string(), vec![], vec![], vec![],
        vec![backend2],
    ));

    // Should match longer prefix first
    let result = table.find_http_route("api.com", "GET", "/api/v1/users", &[], "");
    assert!(result.is_ok());
    let rule = result.unwrap();
    let backend_ref = &rule.backends[0];
    assert_eq!(format!("{}", backend_ref.socket_addr.ip()), "127.0.0.2");

    // Should match shorter prefix for non-v1 paths
    let result = table.find_http_route("api.com", "GET", "/api/v2/users", &[], "");
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
    HttpRouteRule::new(path_type, path.to_string(), vec![], vec![], vec![], vec![backend(port)])
}

// Path matching

#[test]
fn test_exact_path_match_wins_over_prefix() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", rule(PathMatchType::Exact, "/foo", 8001));
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/foo", 8002));

    let r = rt.find_http_route("example.com", "GET", "/foo", &[], "").unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8001);
}

#[test]
fn test_longest_prefix_wins() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/api", 8001));
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/api/v1", 8002));

    let r = rt.find_http_route("example.com", "GET", "/api/v1/users", &[], "").unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8002);
}

#[test]
fn test_root_prefix_catches_all() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", 8001));

    assert!(rt.find_http_route("example.com", "GET", "/anything/here", &[], "").is_ok());
    assert!(rt.find_http_route("example.com", "GET", "/", &[], "").is_ok());
}

#[test]
fn test_exact_path_no_subpath() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", rule(PathMatchType::Exact, "/foo", 8001));

    assert!(rt.find_http_route("example.com", "GET", "/foo", &[], "").is_ok());
    assert!(rt.find_http_route("example.com", "GET", "/foo/bar", &[], "").is_err());
}

#[test]
fn test_prefix_trailing_slash_semantics() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/foo/", 8001));
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/foo", 8002));

    let r = rt.find_http_route("example.com", "GET", "/foo/bar", &[], "").unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8001);

    let r = rt.find_http_route("example.com", "GET", "/foo", &[], "").unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8002);
}

// Host matching

#[test]
fn test_exact_host_priority_over_wildcard() {
    let mut rt = RouteTable::new();
    rt.add_http_route("*.example.com", rule(PathMatchType::Prefix, "/", 8001));
    rt.add_http_route("specific.example.com", rule(PathMatchType::Prefix, "/", 8002));

    let r = rt.find_http_route("specific.example.com", "GET", "/", &[], "").unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8002);
}

#[test]
fn test_wildcard_host_matching_new() {
    let mut rt = RouteTable::new();
    rt.add_http_route("*.example.com", rule(PathMatchType::Prefix, "/", 8001));

    let r = rt.find_http_route("foo.example.com", "GET", "/", &[], "").unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8001);
}

#[test]
fn test_wildcard_no_match_bare_domain() {
    let mut rt = RouteTable::new();
    rt.add_http_route("*.example.com", rule(PathMatchType::Prefix, "/", 8001));

    assert!(rt.find_http_route("example.com", "GET", "/", &[], "").is_err());
}

#[test]
fn test_case_insensitive_host() {
    let mut rt = RouteTable::new();
    rt.add_http_route("api.example.com", rule(PathMatchType::Prefix, "/", 8001));

    let r = rt.find_http_route("API.Example.COM", "GET", "/", &[], "").unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8001);
}

#[test]
fn test_host_with_port_stripped() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", 8001));

    let r = rt.find_http_route("example.com", "GET", "/", &[], "").unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8001);
}

// Header matching

#[test]
fn test_single_header_match() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", HttpRouteRule::new(
        PathMatchType::Prefix, "/".to_string(),
        vec![HeaderMatch { name: "x-env".to_string(), value: "canary".to_string() }],
        vec![], vec![],
        vec![backend(8001)],
    ));
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", 8002));

    let headers = b"X-Env: canary\r\n";
    let r = rt.find_http_route("example.com", "GET", "/", headers, "").unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8001);
}

#[test]
fn test_multiple_headers_and_logic() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", HttpRouteRule::new(
        PathMatchType::Prefix, "/".to_string(),
        vec![
            HeaderMatch { name: "x-env".to_string(), value: "canary".to_string() },
            HeaderMatch { name: "x-region".to_string(), value: "us-east".to_string() },
        ],
        vec![], vec![],
        vec![backend(8001)],
    ));
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", 8002));

    let headers = b"X-Env: canary\r\nX-Region: us-east\r\n";
    let r = rt.find_http_route("example.com", "GET", "/", headers, "").unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8001);

    let headers = b"X-Env: canary\r\n";
    let r = rt.find_http_route("example.com", "GET", "/", headers, "").unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8002);
}

#[test]
fn test_header_match_case_insensitive() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", HttpRouteRule::new(
        PathMatchType::Prefix, "/".to_string(),
        vec![HeaderMatch { name: "x-env".to_string(), value: "canary".to_string() }],
        vec![], vec![],
        vec![backend(8001)],
    ));
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", 8002));

    let headers = b"x-env: canary\r\n";
    let r = rt.find_http_route("example.com", "GET", "/", headers, "").unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8001);
}

#[test]
fn test_header_match_fallback_to_no_header_rule() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", HttpRouteRule::new(
        PathMatchType::Prefix, "/".to_string(),
        vec![HeaderMatch { name: "x-env".to_string(), value: "canary".to_string() }],
        vec![], vec![],
        vec![backend(8001)],
    ));
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", 8002));

    let headers = b"X-Env: production\r\n";
    let r = rt.find_http_route("example.com", "GET", "/", headers, "").unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8002);
}

// Weighted backends

#[test]
fn test_weighted_round_robin_distribution() {
    let mut rt = RouteTable::new();
    rt.add_http_route("test.com", HttpRouteRule::new(
        PathMatchType::Prefix, "/".to_string(), vec![], vec![], vec![],
        vec![
            Backend { socket_addr: "127.0.0.1:8001".parse().unwrap(), weight: 3 },
            Backend { socket_addr: "127.0.0.1:8002".parse().unwrap(), weight: 1 },
        ],
    ));
    let r = rt.find_http_route("test.com", "GET", "/", &[], "").unwrap();

    let mut selector = BackendSelector::new();
    let mut counts = [0u32; 2];
    for _ in 0..400 {
        let idx = selector.select_weighted_backend(42, r);
        counts[idx] += 1;
    }
    assert_eq!(counts[0], 300);
    assert_eq!(counts[1], 100);
}

#[test]
fn test_equal_weights() {
    let mut rt = RouteTable::new();
    rt.add_http_route("test.com", HttpRouteRule::new(
        PathMatchType::Prefix, "/".to_string(), vec![], vec![], vec![],
        vec![
            Backend { socket_addr: "127.0.0.1:8001".parse().unwrap(), weight: 1 },
            Backend { socket_addr: "127.0.0.1:8002".parse().unwrap(), weight: 1 },
        ],
    ));
    let r = rt.find_http_route("test.com", "GET", "/", &[], "").unwrap();

    let mut selector = BackendSelector::new();
    let mut counts = [0u32; 2];
    for _ in 0..100 {
        let idx = selector.select_weighted_backend(42, r);
        counts[idx] += 1;
    }
    assert_eq!(counts[0], 50);
    assert_eq!(counts[1], 50);
}

#[test]
fn test_zero_weight_backend_never_selected() {
    let mut rt = RouteTable::new();
    rt.add_http_route("test.com", HttpRouteRule::new(
        PathMatchType::Prefix, "/".to_string(), vec![], vec![], vec![],
        vec![
            Backend { socket_addr: "127.0.0.1:8001".parse().unwrap(), weight: 1 },
            Backend { socket_addr: "127.0.0.1:8002".parse().unwrap(), weight: 0 },
        ],
    ));
    let r = rt.find_http_route("test.com", "GET", "/", &[], "").unwrap();

    let mut selector = BackendSelector::new();
    for _ in 0..100 {
        let idx = selector.select_weighted_backend(42, r);
        assert_eq!(idx, 0, "zero-weight backend should never be selected");
    }
}

// Filter presence on rules

#[test]
fn test_route_carries_filters() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", HttpRouteRule::new(
        PathMatchType::Prefix, "/".to_string(), vec![], vec![],
        vec![HttpFilter::RequestHeaderModifier {
            add: vec![HttpHeader { name: "X-Added".to_string(), value: "yes".to_string() }],
            set: vec![],
            remove: vec![],
        }],
        vec![backend(8001)],
    ));

    let r = rt.find_http_route("example.com", "GET", "/", &[], "").unwrap();
    assert_eq!(r.filters.len(), 1);
    assert!(r.has_filters);
    assert!(matches!(&r.filters[0], HttpFilter::RequestHeaderModifier { .. }));
}

#[test]
fn test_url_rewrite_on_route_rule() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", HttpRouteRule::new(
        PathMatchType::Prefix, "/v1".to_string(), vec![], vec![],
        vec![HttpFilter::URLRewrite {
            hostname: None,
            path: Some(URLRewritePath::ReplacePrefixMatch("/v2".to_string())),
        }],
        vec![backend(8001)],
    ));

    let r = rt.find_http_route("example.com", "GET", "/v1/users", &[], "").unwrap();
    assert_eq!(r.filters.len(), 1);
    assert!(matches!(&r.filters[0], HttpFilter::URLRewrite { .. }));
}

#[test]
fn test_request_mirror_on_route_rule() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", HttpRouteRule::new(
        PathMatchType::Prefix, "/".to_string(), vec![], vec![],
        vec![HttpFilter::RequestMirror {
            backend_addr: "127.0.0.1:9999".parse().unwrap(),
        }],
        vec![backend(8001)],
    ));

    let r = rt.find_http_route("example.com", "GET", "/", &[], "").unwrap();
    assert_eq!(r.filters.len(), 1);
    assert!(matches!(&r.filters[0], HttpFilter::RequestMirror { .. }));
}

// Pre-computed fields

#[test]
fn test_has_filters_precomputed() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", 8001));

    let r = rt.find_http_route("example.com", "GET", "/", &[], "").unwrap();
    assert!(!r.has_filters);
}

#[test]
fn test_cumulative_weights_precomputed() {
    let mut rt = RouteTable::new();
    rt.add_http_route("test.com", HttpRouteRule::new(
        PathMatchType::Prefix, "/".to_string(), vec![], vec![], vec![],
        vec![
            Backend { socket_addr: "127.0.0.1:8001".parse().unwrap(), weight: 3 },
            Backend { socket_addr: "127.0.0.1:8002".parse().unwrap(), weight: 1 },
        ],
    ));
    let r = rt.find_http_route("test.com", "GET", "/", &[], "").unwrap();
    assert_eq!(r.total_weight, 4);
    assert_eq!(r.cumulative_weights, vec![3, 4]);
}

// Method and query param matching (integration tests)

#[test]
fn test_method_match_integration() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com",
        rule(PathMatchType::Prefix, "/", 8001).with_method(Some("DELETE".to_string())));
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", 8002));

    let r = rt.find_http_route("example.com", "DELETE", "/", &[], "").unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8001);

    let r = rt.find_http_route("example.com", "GET", "/", &[], "").unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8002);
}

#[test]
fn test_query_param_match_integration() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", HttpRouteRule::new(
        PathMatchType::Prefix, "/search".to_string(), vec![],
        vec![QueryParamMatch { name: "format".to_string(), value: "json".to_string() }],
        vec![], vec![backend(8001)],
    ));
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/search", 8002));

    let r = rt.find_http_route("example.com", "GET", "/search", &[], "format=json&q=test").unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8001);

    let r = rt.find_http_route("example.com", "GET", "/search", &[], "q=test").unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8002);
}

#[test]
fn test_method_plus_query_plus_header_integration() {
    let mut rt = RouteTable::new();
    rt.add_http_route("example.com", HttpRouteRule::new(
        PathMatchType::Prefix, "/api".to_string(),
        vec![HeaderMatch { name: "x-version".to_string(), value: "2".to_string() }],
        vec![QueryParamMatch { name: "debug".to_string(), value: "true".to_string() }],
        vec![], vec![backend(8001)],
    ).with_method(Some("POST".to_string())));
    rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", 8002));

    let headers = b"X-Version: 2\r\n";
    let r = rt.find_http_route("example.com", "POST", "/api", headers, "debug=true").unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8001);

    // Wrong method
    let r = rt.find_http_route("example.com", "GET", "/api", headers, "debug=true").unwrap();
    assert_eq!(r.backends[0].socket_addr.port(), 8002);
}
