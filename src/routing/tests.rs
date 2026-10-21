use super::*;

use super::build::resolve_socket_addrs;

fn query_string_contains_param(query: &str, name: &str, value: &str) -> bool {
    find_query_param_value(query, name).is_some_and(|v| v == value)
}

fn backend(port: u16) -> Backend {
    Backend {
        socket_addr: format!("127.0.0.1:{}", port).parse().unwrap(),
        weight: 1,
        filters: vec![],
        use_tls: false,
        server_name: String::new(),
        action: BackendAction::Forward,
    }
}

fn rule(path_type: PathMatchType, path: &str, backends: Vec<Backend>) -> HttpRouteRule {
    HttpRouteRule::new(
        path_type,
        path.to_string(),
        vec![],
        vec![],
        vec![],
        backends,
    )
}

// --- STRICT_DNS multi-A resolution + refresh signature ---------------------
// Tests stay hermetic: IP literals only, never the system resolver.

#[test]
fn resolve_socket_addrs_ip_literal_is_single_and_resolver_free() {
    let addrs = resolve_socket_addrs("192.168.1.1", 443).unwrap();
    assert_eq!(addrs, vec!["192.168.1.1:443".parse().unwrap()]);
}

#[test]
fn all_with_weight_ip_literal_yields_one_weighted_backend() {
    let backends = Backend::all_with_weight("10.0.0.5".to_string(), 8080, 3).unwrap();
    assert_eq!(backends.len(), 1);
    assert_eq!(backends[0].socket_addr, "10.0.0.5:8080".parse().unwrap());
    assert_eq!(backends[0].weight, 3);
    // Original address is retained for TLS SNI.
    assert_eq!(backends[0].server_name, "10.0.0.5");
}

#[test]
fn dns_signature_is_order_independent() {
    // The resolver may return the same A-records in a different order; that
    // must not look like a change (no spurious table swap).
    let mut a = RouteTable::new();
    a.add_http_route(
        "h",
        rule(
            PathMatchType::Prefix,
            "/",
            vec![backend(1), backend(2), backend(3)],
        ),
    );
    let mut b = RouteTable::new();
    b.add_http_route(
        "h",
        rule(
            PathMatchType::Prefix,
            "/",
            vec![backend(3), backend(1), backend(2)],
        ),
    );
    assert_eq!(a.dns_signature(), b.dns_signature());
    // Empty table folds to 0.
    assert_eq!(RouteTable::new().dns_signature(), 0);
}

#[test]
fn dns_signature_changes_when_pod_set_changes() {
    let mut base = RouteTable::new();
    base.add_http_route(
        "h",
        rule(PathMatchType::Prefix, "/", vec![backend(1), backend(2)]),
    );
    // A pod scaled in (added address) changes the signature.
    let mut added = RouteTable::new();
    added.add_http_route(
        "h",
        rule(
            PathMatchType::Prefix,
            "/",
            vec![backend(1), backend(2), backend(3)],
        ),
    );
    assert_ne!(base.dns_signature(), added.dns_signature());
    // A pod replaced (same count, different address) changes the signature —
    // wrapping_add does not cancel like XOR would.
    let mut replaced = RouteTable::new();
    replaced.add_http_route(
        "h",
        rule(PathMatchType::Prefix, "/", vec![backend(1), backend(9)]),
    );
    assert_ne!(base.dns_signature(), replaced.dns_signature());
}

#[test]
fn dns_signature_covers_l4_and_tls_backends() {
    // DNS churn on a TCP/UDP/TLS backend must also trigger a refresh swap.
    let mut tcp_a = RouteTable::new();
    tcp_a.add_tcp_route(7000, vec![backend(1)]);
    let mut tcp_b = RouteTable::new();
    tcp_b.add_tcp_route(7000, vec![backend(2)]);
    assert_ne!(tcp_a.dns_signature(), tcp_b.dns_signature());

    let mut tls = RouteTable::new();
    tls.add_tls_route(0, "svc.example.com", vec![backend(1)]);
    assert_ne!(RouteTable::new().dns_signature(), tls.dns_signature());
}

#[test]
fn test_exact_path_matching() {
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "example.com",
        rule(PathMatchType::Exact, "/foo", vec![backend(8001)]),
    );
    rt.add_http_route(
        "example.com",
        rule(PathMatchType::Prefix, "/foo", vec![backend(8002)]),
    );

    let r = rt
        .find_http_route("example.com", "GET", "/foo", &[], "", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());

    let r = rt
        .find_http_route("example.com", "GET", "/foo/bar", &[], "", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8002".parse().unwrap());
}

#[test]
fn test_wildcard_host_matching() {
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "*.example.com",
        rule(PathMatchType::Prefix, "/", vec![backend(9001)]),
    );
    rt.add_http_route(
        "specific.example.com",
        rule(PathMatchType::Prefix, "/", vec![backend(9002)]),
    );

    let r = rt
        .find_http_route("specific.example.com", "GET", "/", &[], "", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:9002".parse().unwrap());

    let r = rt
        .find_http_route("other.example.com", "GET", "/", &[], "", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:9001".parse().unwrap());

    assert!(rt
        .find_http_route("example.org", "GET", "/", &[], "", 0)
        .is_none());
}

#[test]
fn test_header_matching() {
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "example.com",
        HttpRouteRule::new(
            PathMatchType::Prefix,
            "/".to_string(),
            vec![HeaderMatch {
                name: "x-env".to_string(),
                matcher: ValueMatcher::Exact("canary".to_string()),
            }],
            vec![],
            vec![],
            vec![backend(7001)],
        ),
    );
    rt.add_http_route(
        "example.com",
        rule(PathMatchType::Prefix, "/", vec![backend(7002)]),
    );

    let headers = b"X-Env: canary\r\nAccept: */*\r\n";
    let r = rt
        .find_http_route("example.com", "GET", "/", headers, "", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:7001".parse().unwrap());

    let r = rt
        .find_http_route("example.com", "GET", "/", b"Accept: */*\r\n", "", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:7002".parse().unwrap());
}

#[test]
fn test_find_header_value() {
    let headers = b"Host: example.com\r\nX-Custom: hello\r\nContent-Type: text/plain\r\n";
    assert_eq!(find_header_value(headers, "host"), Some("example.com"));
    assert_eq!(find_header_value(headers, "x-custom"), Some("hello"));
    assert_eq!(
        find_header_value(headers, "content-type"),
        Some("text/plain")
    );
    assert_eq!(find_header_value(headers, "missing"), None);
}

#[test]
fn test_prefix_boundary_no_false_extension() {
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "example.com",
        rule(PathMatchType::Prefix, "/foo", vec![backend(8001)]),
    );
    rt.add_http_route(
        "example.com",
        rule(PathMatchType::Prefix, "/", vec![backend(8002)]),
    );

    // /foo/bar matches prefix /foo (boundary at /)
    let r = rt
        .find_http_route("example.com", "GET", "/foo/bar", &[], "", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());

    // /foobar must NOT match prefix /foo — falls through to /
    let r = rt
        .find_http_route("example.com", "GET", "/foobar", &[], "", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8002".parse().unwrap());

    // /foo exactly matches prefix /foo
    let r = rt
        .find_http_route("example.com", "GET", "/foo", &[], "", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());
}

#[test]
fn test_prefix_boundary_trailing_slash() {
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "example.com",
        rule(PathMatchType::Prefix, "/foo/", vec![backend(8001)]),
    );
    rt.add_http_route(
        "example.com",
        rule(PathMatchType::Prefix, "/", vec![backend(8002)]),
    );

    // /foo/bar matches prefix /foo/ (prefix ends with /)
    let r = rt
        .find_http_route("example.com", "GET", "/foo/bar", &[], "", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());

    // /foo/ matches prefix /foo/
    let r = rt
        .find_http_route("example.com", "GET", "/foo/", &[], "", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());
}

#[test]
fn test_weighted_backend() {
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "test.com",
        HttpRouteRule::new(
            PathMatchType::Prefix,
            "/".to_string(),
            vec![],
            vec![],
            vec![],
            vec![
                Backend {
                    socket_addr: "127.0.0.1:8001".parse().unwrap(),
                    weight: 3,
                    filters: vec![],
                    use_tls: false,
                    server_name: String::new(),
                    action: BackendAction::Forward,
                },
                Backend {
                    socket_addr: "127.0.0.1:8002".parse().unwrap(),
                    weight: 1,
                    filters: vec![],
                    use_tls: false,
                    server_name: String::new(),
                    action: BackendAction::Forward,
                },
            ],
        ),
    );
    let r = rt
        .find_http_route("test.com", "GET", "/", &[], "", 0)
        .unwrap();

    let health = crate::proxy::health::HealthRegistry::new();
    let selector = BackendSelector::new();
    let mut counts = [0u32; 2];
    for _ in 0..400 {
        let idx = selector
            .select_healthy_weighted_backend(r, &health)
            .unwrap();
        counts[idx] += 1;
    }
    assert_eq!(counts[0], 300);
    assert_eq!(counts[1], 100);
}

#[test]
fn test_method_matching() {
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "example.com",
        rule(PathMatchType::Prefix, "/", vec![backend(8001)]).with_method(Some("POST".to_string())),
    );
    rt.add_http_route(
        "example.com",
        rule(PathMatchType::Prefix, "/", vec![backend(8002)]),
    );

    // POST matches the method-constrained rule
    let r = rt
        .find_http_route("example.com", "POST", "/", &[], "", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());

    // GET skips the method-constrained rule, hits fallback
    let r = rt
        .find_http_route("example.com", "GET", "/", &[], "", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8002".parse().unwrap());

    // Case-insensitive
    let r = rt
        .find_http_route("example.com", "post", "/", &[], "", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());
}

#[test]
fn test_query_param_matching() {
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "example.com",
        HttpRouteRule::new(
            PathMatchType::Prefix,
            "/".to_string(),
            vec![],
            vec![QueryParamMatch {
                name: "version".to_string(),
                matcher: ValueMatcher::Exact("2".to_string()),
            }],
            vec![],
            vec![backend(8001)],
        ),
    );
    rt.add_http_route(
        "example.com",
        rule(PathMatchType::Prefix, "/", vec![backend(8002)]),
    );

    // Matching query param
    let r = rt
        .find_http_route("example.com", "GET", "/", &[], "version=2", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());

    // Missing query param -> fallback
    let r = rt
        .find_http_route("example.com", "GET", "/", &[], "", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8002".parse().unwrap());

    // Wrong value -> fallback
    let r = rt
        .find_http_route("example.com", "GET", "/", &[], "version=1", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8002".parse().unwrap());

    // Multiple params, one matches
    let r = rt
        .find_http_route("example.com", "GET", "/", &[], "foo=bar&version=2", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());
}

#[test]
fn test_multiple_query_params_and_logic() {
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "example.com",
        HttpRouteRule::new(
            PathMatchType::Prefix,
            "/".to_string(),
            vec![],
            vec![
                QueryParamMatch {
                    name: "a".to_string(),
                    matcher: ValueMatcher::Exact("1".to_string()),
                },
                QueryParamMatch {
                    name: "b".to_string(),
                    matcher: ValueMatcher::Exact("2".to_string()),
                },
            ],
            vec![],
            vec![backend(8001)],
        ),
    );
    rt.add_http_route(
        "example.com",
        rule(PathMatchType::Prefix, "/", vec![backend(8002)]),
    );

    // Both params present
    let r = rt
        .find_http_route("example.com", "GET", "/", &[], "a=1&b=2", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());

    // Only one param -> fallback
    let r = rt
        .find_http_route("example.com", "GET", "/", &[], "a=1", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8002".parse().unwrap());
}

#[test]
fn test_combined_method_path_header_query() {
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "example.com",
        HttpRouteRule::new(
            PathMatchType::Prefix,
            "/api".to_string(),
            vec![HeaderMatch {
                name: "x-env".to_string(),
                matcher: ValueMatcher::Exact("prod".to_string()),
            }],
            vec![QueryParamMatch {
                name: "v".to_string(),
                matcher: ValueMatcher::Exact("2".to_string()),
            }],
            vec![],
            vec![backend(8001)],
        )
        .with_method(Some("POST".to_string())),
    );
    rt.add_http_route(
        "example.com",
        rule(PathMatchType::Prefix, "/", vec![backend(8002)]),
    );

    let headers = b"X-Env: prod\r\n";

    // All match
    let r = rt
        .find_http_route("example.com", "POST", "/api/users", headers, "v=2", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());

    // Wrong method -> fallback
    let r = rt
        .find_http_route("example.com", "GET", "/api/users", headers, "v=2", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8002".parse().unwrap());

    // Wrong query -> fallback
    let r = rt
        .find_http_route("example.com", "POST", "/api/users", headers, "v=1", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8002".parse().unwrap());
}

#[test]
fn test_query_string_contains_param() {
    assert!(query_string_contains_param("a=1&b=2", "a", "1"));
    assert!(query_string_contains_param("a=1&b=2", "b", "2"));
    assert!(!query_string_contains_param("a=1&b=2", "c", "3"));
    assert!(!query_string_contains_param("", "a", "1"));
    assert!(!query_string_contains_param("a=1", "a", "2"));
}

#[test]
fn test_regex_header_matching() {
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "example.com",
        HttpRouteRule::new(
            PathMatchType::Prefix,
            "/".to_string(),
            vec![HeaderMatch {
                name: "x-env".to_string(),
                matcher: ValueMatcher::Regex(regex::Regex::new("^(canary|staging)$").unwrap()),
            }],
            vec![],
            vec![],
            vec![backend(7001)],
        ),
    );
    rt.add_http_route(
        "example.com",
        rule(PathMatchType::Prefix, "/", vec![backend(7002)]),
    );

    let headers = b"X-Env: canary\r\n";
    let r = rt
        .find_http_route("example.com", "GET", "/", headers, "", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:7001".parse().unwrap());

    let headers = b"X-Env: staging\r\n";
    let r = rt
        .find_http_route("example.com", "GET", "/", headers, "", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:7001".parse().unwrap());

    let headers = b"X-Env: production\r\n";
    let r = rt
        .find_http_route("example.com", "GET", "/", headers, "", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:7002".parse().unwrap());
}

#[test]
fn test_regex_query_param_matching() {
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "example.com",
        HttpRouteRule::new(
            PathMatchType::Prefix,
            "/".to_string(),
            vec![],
            vec![QueryParamMatch {
                name: "version".to_string(),
                matcher: ValueMatcher::Regex(regex::Regex::new(r"^\d+$").unwrap()),
            }],
            vec![],
            vec![backend(8001)],
        ),
    );
    rt.add_http_route(
        "example.com",
        rule(PathMatchType::Prefix, "/", vec![backend(8002)]),
    );

    let r = rt
        .find_http_route("example.com", "GET", "/", &[], "version=2", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());

    let r = rt
        .find_http_route("example.com", "GET", "/", &[], "version=abc", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8002".parse().unwrap());
}

#[test]
fn test_regex_path_matching() {
    let mut rt = RouteTable::new();
    let mut regex_rule = rule(
        PathMatchType::RegularExpression,
        r"^/api/v\d+/users$",
        vec![backend(8001)],
    );
    regex_rule.path_regex = Some(regex::Regex::new(r"^/api/v\d+/users$").unwrap());
    rt.add_http_route("example.com", regex_rule);
    rt.add_http_route(
        "example.com",
        rule(PathMatchType::Prefix, "/", vec![backend(8002)]),
    );

    let r = rt
        .find_http_route("example.com", "GET", "/api/v1/users", &[], "", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());

    let r = rt
        .find_http_route("example.com", "GET", "/api/v2/users", &[], "", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());

    // No match — falls to prefix
    let r = rt
        .find_http_route("example.com", "GET", "/api/v1/posts", &[], "", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8002".parse().unwrap());
}

#[test]
fn test_exact_beats_regex_precedence() {
    let mut rt = RouteTable::new();
    let mut regex_rule = rule(
        PathMatchType::RegularExpression,
        r"^/foo.*",
        vec![backend(8001)],
    );
    regex_rule.path_regex = Some(regex::Regex::new(r"^/foo.*").unwrap());
    rt.add_http_route("example.com", regex_rule);
    rt.add_http_route(
        "example.com",
        rule(PathMatchType::Exact, "/foo", vec![backend(8002)]),
    );

    // Exact match wins over regex
    let r = rt
        .find_http_route("example.com", "GET", "/foo", &[], "", 0)
        .unwrap();
    assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8002".parse().unwrap());
}

fn resolve_pt(rt: &RouteTable, sni: &str, port: u16) -> Option<std::net::SocketAddr> {
    let selector = BackendSelector::new();
    let health = crate::proxy::health::HealthRegistry::new();
    rt.resolve_tls_passthrough(Some(sni), port, None, &selector, &health)
        .ok()
}

#[test]
fn test_resolve_tls_passthrough_found() {
    let mut rt = RouteTable::new();
    rt.add_tcp_route(8443, vec![backend(9001)]);

    let addr = resolve_pt(&rt, "example.com", 8443);
    assert_eq!(addr, Some("127.0.0.1:9001".parse().unwrap()));
}

#[test]
fn test_resolve_tls_passthrough_no_route() {
    let rt = RouteTable::new();
    assert!(resolve_pt(&rt, "example.com", 8443).is_none());
}

/// A route with no backend at all was reported as having no positive
/// weight; the L4 path tells the two apart, and so does this.
#[test]
fn passthrough_miss_tells_no_backends_from_no_weight() {
    let mut weightless = backend(9001);
    weightless.weight = 0;
    let mut rt = RouteTable::new();
    rt.add_tls_route(0, "empty.example.com", vec![]);
    rt.add_tls_route(0, "weightless.example.com", vec![weightless]);
    rt.add_tcp_route(8443, vec![]);
    let (selector, health) = (
        BackendSelector::new(),
        crate::proxy::health::HealthRegistry::new(),
    );
    let miss = |sni| {
        rt.resolve_tls_passthrough(Some(sni), 8443, None, &selector, &health)
            .unwrap_err()
    };
    assert_eq!(miss("empty.example.com"), PassthroughMiss::NoBackends);
    assert_eq!(miss("weightless.example.com"), PassthroughMiss::NoWeight);
    assert_eq!(miss("tcp.example.com"), PassthroughMiss::NoBackends);
    assert_eq!(
        PassthroughMiss::NoBackends.to_string(),
        "no backends available"
    );
}

#[test]
fn test_tls_route_sni_exact_match() {
    let mut rt = RouteTable::new();
    rt.add_tls_route(0, "secure.example.com", vec![backend(9001)]);
    rt.add_tcp_route(8443, vec![backend(9999)]); // fallback

    let addr = resolve_pt(&rt, "secure.example.com", 8443);
    assert_eq!(addr, Some("127.0.0.1:9001".parse().unwrap()));
}

#[test]
fn test_tls_route_sni_wildcard_match() {
    let mut rt = RouteTable::new();
    rt.add_tls_route(0, "*.example.com", vec![backend(9002)]);
    rt.add_tcp_route(8443, vec![backend(9999)]); // fallback

    let addr = resolve_pt(&rt, "foo.example.com", 8443);
    assert_eq!(addr, Some("127.0.0.1:9002".parse().unwrap()));
}

#[test]
fn test_tls_route_sni_fallback_to_tcp() {
    let mut rt = RouteTable::new();
    rt.add_tls_route(0, "secure.example.com", vec![backend(9001)]);
    rt.add_tcp_route(8443, vec![backend(9999)]);

    // No SNI match -> falls back to TCP port route
    let addr = resolve_pt(&rt, "other.example.org", 8443);
    assert_eq!(addr, Some("127.0.0.1:9999".parse().unwrap()));
}

/// A TLSRoute attached to a listener on one port must not hijack matching
/// SNI on other ports — the original bug ignored the port entirely, so an
/// HTTPS-terminate connection on 443 would get passed through whenever any
/// TLSRoute elsewhere claimed its hostname.
#[test]
fn test_tls_route_scoped_to_listener_port() {
    let mut rt = RouteTable::new();
    rt.add_tls_route(9443, "svc.example.com", vec![backend(9001)]);

    assert!(resolve_pt(&rt, "svc.example.com", 9443).is_some());
    assert_eq!(
        resolve_pt(&rt, "svc.example.com", 9443),
        Some("127.0.0.1:9001".parse().unwrap())
    );

    // Same SNI on a different port: no passthrough.
    assert!(!resolve_pt(&rt, "svc.example.com", 443).is_some());
    assert_eq!(resolve_pt(&rt, "svc.example.com", 443), None);
}

/// Any-port (0) TLS routes still match every port — the file-config path.
#[test]
fn test_tls_route_any_port_scope_matches_all_ports() {
    let mut rt = RouteTable::new();
    rt.add_tls_route(0, "svc.example.com", vec![backend(9001)]);
    assert!(resolve_pt(&rt, "svc.example.com", 443).is_some());
    assert!(resolve_pt(&rt, "svc.example.com", 9443).is_some());
}

/// Wildcard TLS matching walks up domain labels like the HTTP wildcard
/// path — `*.example.com` also covers `a.b.example.com` (previously only
/// one label was stripped).
#[test]
fn test_tls_wildcard_matches_multi_label_subdomain() {
    let mut rt = RouteTable::new();
    rt.add_tls_route(0, "*.example.com", vec![backend(9002)]);
    assert!(resolve_pt(&rt, "a.b.example.com", 8443).is_some());
    assert_eq!(
        resolve_pt(&rt, "a.b.example.com", 8443),
        Some("127.0.0.1:9002".parse().unwrap())
    );
}

/// Passthrough rotates across backends instead of pinning the first.
#[test]
fn test_tls_passthrough_round_robins_backends() {
    let mut rt = RouteTable::new();
    rt.add_tls_route(0, "svc.example.com", vec![backend(9001), backend(9002)]);

    let selector = BackendSelector::new();
    let health = crate::proxy::health::HealthRegistry::new();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..4 {
        seen.insert(
            rt.resolve_tls_passthrough(Some("svc.example.com"), 8443, None, &selector, &health)
                .unwrap()
                .port(),
        );
    }
    assert_eq!(seen, [9001, 9002].into_iter().collect());
}

/// A passthrough listener's own routes and the routes without parentRefs
/// rank as one set: the exact hostname, then the longest wildcard, then
/// the catch-all. The listener's own scope was searched whole first, so
/// its catch-all took a host a shared route names exactly.
#[test]
fn tls_routes_rank_across_a_listeners_scope_and_the_shared_one() {
    let mut rt = RouteTable::new();
    rt.add_tls_route_for_listener(8443, None, "*", vec![backend(9001)]);
    rt.add_tls_route_for_listener(8443, None, "*.example.com", vec![backend(9002)]);
    rt.add_tls_route_for_listener(8443, None, "own.example.com", vec![backend(9003)]);
    rt.add_tls_route(0, "s.example.com", vec![backend(9004)]);
    rt.add_tls_route(0, "*.b.example.com", vec![backend(9005)]);
    rt.add_tls_route(0, "*.example.com", vec![backend(9006)]);
    rt.add_tls_route(0, "*", vec![backend(9007)]);
    for (sni, port) in [
        ("s.example.com", 9004),
        ("x.b.example.com", 9005),
        ("own.example.com", 9003),
        ("x.example.com", 9002),
        ("other.org", 9001),
    ] {
        assert_eq!(
            resolve_pt(&rt, sni, 8443).map(|a| a.port()),
            Some(port),
            "{sni}"
        );
    }
}

/// Selection rotates per route: hellos naming different hosts under one
/// wildcard route, or none at all on a catch-all, take turns. The counter
/// was keyed by the client's SNI, one per name a client chose to send.
#[test]
fn tls_passthrough_rotates_per_route_not_per_sni() {
    let mut rt = RouteTable::new();
    rt.add_tls_route_for_listener(
        8443,
        None,
        "*.example.com",
        vec![backend(9001), backend(9002)],
    );
    rt.add_tls_route_for_listener(8443, None, "*", vec![backend(9003), backend(9004)]);
    let selector = BackendSelector::new();
    let health = crate::proxy::health::HealthRegistry::new();
    let pick = |sni| {
        rt.resolve_tls_passthrough(sni, 8443, None, &selector, &health)
            .unwrap()
            .port()
    };
    let wildcard: Vec<_> = [
        "a.example.com",
        "b.example.com",
        "c.example.com",
        "d.example.com",
    ]
    .map(|sni| pick(Some(sni)))
    .into();
    assert_eq!(wildcard, [9001, 9002, 9001, 9002]);
    let catch_all: Vec<_> = [None, Some("x.org"), None, Some("y.org")].map(pick).into();
    assert_eq!(catch_all, [9003, 9004, 9003, 9004]);
}

/// Passthrough fails over: an unhealthy backend gets no connections while a
/// healthy one remains, and with every backend down it fails open instead of
/// refusing the connection.
#[test]
fn test_tls_passthrough_skips_unhealthy_backend() {
    let mut rt = RouteTable::new();
    rt.add_tls_route(0, "svc.example.com", vec![backend(9001), backend(9002)]);

    let selector = BackendSelector::new();
    let health = crate::proxy::health::HealthRegistry::new();
    mark_unhealthy(&health, 9001);
    for _ in 0..4 {
        let addr = rt
            .resolve_tls_passthrough(Some("svc.example.com"), 8443, None, &selector, &health)
            .unwrap();
        assert_eq!(addr.port(), 9002);
    }

    mark_unhealthy(&health, 9002);
    let mut seen = std::collections::HashSet::new();
    for _ in 0..4 {
        seen.insert(
            rt.resolve_tls_passthrough(Some("svc.example.com"), 8443, None, &selector, &health)
                .expect("all-unhealthy must fail open")
                .port(),
        );
    }
    assert_eq!(seen, [9001, 9002].into_iter().collect());
}

/// L4 selection honors backendRef weights: 3:1 over 400 picks gives
/// exactly 300:100, and weight-0 backends receive no traffic.
#[test]
fn test_l4_weighted_selection() {
    let mut b1 = backend(8001);
    b1.weight = 3;
    let b2 = backend(8002); // weight 1
    let mut b0 = backend(8003);
    b0.weight = 0;
    let backends = vec![b1, b2, b0];

    let health = crate::proxy::health::HealthRegistry::new();
    let selector = BackendSelector::new();
    let mut counts = std::collections::HashMap::new();
    for _ in 0..400 {
        let b = selector
            .select_l4_backend(7000, &backends, &health)
            .unwrap();
        *counts.entry(b.socket_addr.port()).or_insert(0u32) += 1;
    }
    assert_eq!(counts.get(&8001), Some(&300));
    assert_eq!(counts.get(&8002), Some(&100));
    assert_eq!(
        counts.get(&8003),
        None,
        "weight-0 backend must get no traffic"
    );
}

#[test]
fn test_equal_weights() {
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "test.com",
        HttpRouteRule::new(
            PathMatchType::Prefix,
            "/".to_string(),
            vec![],
            vec![],
            vec![],
            vec![
                Backend {
                    socket_addr: "127.0.0.1:8001".parse().unwrap(),
                    weight: 1,
                    filters: vec![],
                    use_tls: false,
                    server_name: String::new(),
                    action: BackendAction::Forward,
                },
                Backend {
                    socket_addr: "127.0.0.1:8002".parse().unwrap(),
                    weight: 1,
                    filters: vec![],
                    use_tls: false,
                    server_name: String::new(),
                    action: BackendAction::Forward,
                },
            ],
        ),
    );
    let r = rt
        .find_http_route("test.com", "GET", "/", &[], "", 0)
        .unwrap();

    let health = crate::proxy::health::HealthRegistry::new();
    let selector = BackendSelector::new();
    let mut counts = [0u32; 2];
    for _ in 0..100 {
        let idx = selector
            .select_healthy_weighted_backend(r, &health)
            .unwrap();
        counts[idx] += 1;
    }
    assert_eq!(counts[0], 50);
    assert_eq!(counts[1], 50);
}

#[test]
fn test_zero_weight_backend_never_selected() {
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "test.com",
        HttpRouteRule::new(
            PathMatchType::Prefix,
            "/".to_string(),
            vec![],
            vec![],
            vec![],
            vec![
                Backend {
                    socket_addr: "127.0.0.1:8001".parse().unwrap(),
                    weight: 1,
                    filters: vec![],
                    use_tls: false,
                    server_name: String::new(),
                    action: BackendAction::Forward,
                },
                Backend {
                    socket_addr: "127.0.0.1:8002".parse().unwrap(),
                    weight: 0,
                    filters: vec![],
                    use_tls: false,
                    server_name: String::new(),
                    action: BackendAction::Forward,
                },
            ],
        ),
    );
    let r = rt
        .find_http_route("test.com", "GET", "/", &[], "", 0)
        .unwrap();

    let health = crate::proxy::health::HealthRegistry::new();
    let selector = BackendSelector::new();
    for _ in 0..100 {
        let idx = selector
            .select_healthy_weighted_backend(r, &health)
            .unwrap();
        assert_eq!(idx, 0, "zero-weight backend should never be selected");
    }
}

fn weighted(port: u16, weight: u32) -> Backend {
    Backend {
        weight,
        ..backend(port)
    }
}

fn mark_unhealthy(health: &crate::proxy::health::HealthRegistry, port: u16) {
    let addr = format!("127.0.0.1:{}", port).parse().unwrap();
    for _ in 0..crate::proxy::health::FAILURE_THRESHOLD {
        health.record_failure(addr);
    }
}

#[test]
fn test_zero_weight_canary_with_unhealthy_primary_no_panic() {
    // Regression: healthy_count > 0 but healthy_weight == 0 hit `% 0`,
    // aborting the process. Weight-0 canary alive, primary down.
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "test.com",
        rule(
            PathMatchType::Prefix,
            "/",
            vec![weighted(8001, 0), weighted(8002, 1)],
        ),
    );
    let r = rt
        .find_http_route("test.com", "GET", "/", &[], "", 0)
        .unwrap();

    let health = crate::proxy::health::HealthRegistry::new();
    mark_unhealthy(&health, 8002);
    let selector = BackendSelector::new();
    // Fails open to the primary; the weight-0 canary stays excluded.
    for _ in 0..10 {
        assert_eq!(
            selector.select_healthy_weighted_backend(r, &health),
            Some(1)
        );
    }
}

#[test]
fn test_single_backend_weight_and_health_respected() {
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "zero.com",
        rule(PathMatchType::Prefix, "/", vec![weighted(8001, 0)]),
    );
    rt.add_http_route(
        "one.com",
        rule(PathMatchType::Prefix, "/", vec![weighted(8002, 1)]),
    );
    let health = crate::proxy::health::HealthRegistry::new();
    let selector = BackendSelector::new();

    let zero = rt
        .find_http_route("zero.com", "GET", "/", &[], "", 0)
        .unwrap();
    assert_eq!(
        selector.select_healthy_weighted_backend(zero, &health),
        None
    );

    let one = rt
        .find_http_route("one.com", "GET", "/", &[], "", 0)
        .unwrap();
    assert_eq!(
        selector.select_healthy_weighted_backend(one, &health),
        Some(0)
    );
    // A route's only backend is never parked by passive health.
    mark_unhealthy(&health, 8002);
    assert_eq!(
        selector.select_healthy_weighted_backend(one, &health),
        Some(0)
    );
}

/// Panic routing: with every positive-weight backend ejected, traffic spreads
/// over all of them by weight instead of answering 503. Passive health cannot
/// tell dead pods from a ClusterIP VIP whose endpoints are briefly gone.
#[test]
fn test_all_unhealthy_fails_open_by_weight() {
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "test.com",
        rule(
            PathMatchType::Prefix,
            "/",
            vec![weighted(8001, 3), weighted(8002, 1), weighted(8003, 0)],
        ),
    );
    let r = rt
        .find_http_route("test.com", "GET", "/", &[], "", 0)
        .unwrap();

    let health = crate::proxy::health::HealthRegistry::new();
    mark_unhealthy(&health, 8001);
    mark_unhealthy(&health, 8002);
    let selector = BackendSelector::new();
    let mut counts = [0u32; 3];
    for _ in 0..400 {
        let idx = selector
            .select_healthy_weighted_backend(r, &health)
            .expect("all-unhealthy must fail open");
        counts[idx] += 1;
    }
    assert_eq!(counts, [300, 100, 0]);
}

#[test]
fn test_l4_all_unhealthy_fails_open_by_weight() {
    // The weight-0 backend is healthy but must still get nothing.
    let backends = vec![weighted(8001, 3), weighted(8002, 1), weighted(8003, 0)];
    let health = crate::proxy::health::HealthRegistry::new();
    mark_unhealthy(&health, 8001);
    mark_unhealthy(&health, 8002);
    let selector = BackendSelector::new();
    let mut counts = std::collections::HashMap::new();
    for _ in 0..400 {
        let b = selector
            .select_l4_backend(7000, &backends, &health)
            .expect("all-unhealthy must fail open");
        *counts.entry(b.socket_addr.port()).or_insert(0u32) += 1;
    }
    assert_eq!(counts.get(&8001), Some(&300));
    assert_eq!(counts.get(&8002), Some(&100));
    assert_eq!(counts.get(&8003), None);

    let zero = vec![weighted(8004, 0)];
    assert!(selector.select_l4_backend(7001, &zero, &health).is_none());
}

#[test]
fn test_all_zero_weight_backends_return_none() {
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "test.com",
        rule(
            PathMatchType::Prefix,
            "/",
            vec![weighted(8001, 0), weighted(8002, 0)],
        ),
    );
    let r = rt
        .find_http_route("test.com", "GET", "/", &[], "", 0)
        .unwrap();

    let health = crate::proxy::health::HealthRegistry::new();
    let selector = BackendSelector::new();
    assert_eq!(selector.select_healthy_weighted_backend(r, &health), None);
}

#[test]
fn test_zero_weight_backend_skipped_on_slow_path() {
    // Slow path (one unhealthy sibling): weight-0 backend must be skipped
    // by the walk, not just by the fast-path prefix sums.
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "test.com",
        rule(
            PathMatchType::Prefix,
            "/",
            vec![weighted(8001, 0), weighted(8002, 1), weighted(8003, 1)],
        ),
    );
    let r = rt
        .find_http_route("test.com", "GET", "/", &[], "", 0)
        .unwrap();

    let health = crate::proxy::health::HealthRegistry::new();
    mark_unhealthy(&health, 8003);
    let selector = BackendSelector::new();
    for _ in 0..10 {
        assert_eq!(
            selector.select_healthy_weighted_backend(r, &health),
            Some(1)
        );
    }
}

/// An invalid backendRef's Reject entry keeps exactly its configured share
/// while a sibling is unhealthy: the unhealthy share moves to the healthy
/// forwarding backends only, evenly, never to the Reject entry.
#[test]
fn test_reject_share_is_exact_while_a_sibling_is_unhealthy() {
    let backends = vec![
        weighted(8001, 1),
        weighted(8002, 1),
        Backend::rejecting(1),
        weighted(8003, 1),
    ];
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "test.com",
        rule(PathMatchType::Prefix, "/", backends.clone()),
    );
    let r = rt
        .find_http_route("test.com", "GET", "/", &[], "", 0)
        .unwrap();
    let health = crate::proxy::health::HealthRegistry::new();
    mark_unhealthy(&health, 8001);

    let selector = BackendSelector::new();
    let mut http = [0u32; 4];
    let mut l4 = [0u32; 4];
    for _ in 0..800 {
        http[selector
            .select_healthy_weighted_backend(r, &health)
            .unwrap()] += 1;
        let b = selector
            .select_l4_backend(7000, &backends, &health)
            .unwrap();
        let i = backends.iter().position(|c| std::ptr::eq(c, b)).unwrap();
        l4[i] += 1;
    }
    assert_eq!(http, [0, 300, 200, 300]);
    assert_eq!(l4, [0, 300, 200, 300]);
}

/// With every forwarding backend down, panic routing keeps each one's own
/// share; the Reject entry still gets only its own.
#[test]
fn test_reject_share_is_exact_when_every_forwarding_backend_is_down() {
    let backends = vec![weighted(8001, 3), Backend::rejecting(1)];
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "test.com",
        rule(PathMatchType::Prefix, "/", backends.clone()),
    );
    let r = rt
        .find_http_route("test.com", "GET", "/", &[], "", 0)
        .unwrap();
    let health = crate::proxy::health::HealthRegistry::new();
    mark_unhealthy(&health, 8001);

    let selector = BackendSelector::new();
    let mut http = [0u32; 2];
    let mut l4 = [0u32; 2];
    for _ in 0..400 {
        http[selector
            .select_healthy_weighted_backend(r, &health)
            .unwrap()] += 1;
        let b = selector
            .select_l4_backend(7000, &backends, &health)
            .unwrap();
        l4[usize::from(b.action == BackendAction::Reject)] += 1;
    }
    assert_eq!(http, [300, 100]);
    assert_eq!(l4, [300, 100]);
}

#[test]
fn test_cumulative_weights_precomputed() {
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "test.com",
        HttpRouteRule::new(
            PathMatchType::Prefix,
            "/".to_string(),
            vec![],
            vec![],
            vec![],
            vec![
                Backend {
                    socket_addr: "127.0.0.1:8001".parse().unwrap(),
                    weight: 3,
                    filters: vec![],
                    use_tls: false,
                    server_name: String::new(),
                    action: BackendAction::Forward,
                },
                Backend {
                    socket_addr: "127.0.0.1:8002".parse().unwrap(),
                    weight: 1,
                    filters: vec![],
                    use_tls: false,
                    server_name: String::new(),
                    action: BackendAction::Forward,
                },
            ],
        ),
    );
    let r = rt
        .find_http_route("test.com", "GET", "/", &[], "", 0)
        .unwrap();
    assert_eq!(r.total_weight, 4);
    assert_eq!(r.cumulative_weights, vec![3, 4]);
}

#[test]
fn test_hostnames_intersect() {
    // exact vs exact
    assert!(hostnames_intersect("example.com", "example.com"));
    assert!(!hostnames_intersect("example.com", "other.com"));
    // wildcard vs exact, both directions; never the bare parent
    assert!(hostnames_intersect("*.example.com", "foo.example.com"));
    assert!(hostnames_intersect("foo.example.com", "*.example.com"));
    assert!(!hostnames_intersect("*.example.com", "example.com"));
    assert!(!hostnames_intersect("*.example.com", "foo.other.com"));
    assert!(!hostnames_intersect("*.example.com", "badexample.com"));
    // wildcard vs wildcard: equal or nested, label boundary required
    assert!(hostnames_intersect("*.example.com", "*.example.com"));
    assert!(hostnames_intersect("*.example.com", "*.sub.example.com"));
    assert!(hostnames_intersect("*.sub.example.com", "*.example.com"));
    assert!(!hostnames_intersect("*.example.com", "*.other.com"));
    assert!(!hostnames_intersect("*.example.com", "*.badexample.com"));
}

/// What decides a port's sniff: its TCP route, and HTTP routes in its own
/// scopes or the any-port (0) ones.
#[test]
fn port_route_kinds_include_the_any_port_scope() {
    let mut rt = RouteTable::new();
    rt.add_tcp_route(25, vec![backend(2525)]);
    rt.add_http_route_for_listener(80, None, "a.test", rule(PathMatchType::Prefix, "/", vec![]));
    assert!(rt.has_tcp_route(25) && !rt.has_http_routes(25));
    assert!(rt.has_http_routes(80) && !rt.has_tcp_route(80));

    rt.add_http_route("b.test", rule(PathMatchType::Prefix, "/", vec![]));
    assert!(rt.has_http_routes(25));
}
