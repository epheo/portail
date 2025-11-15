use portail::http_parser::{extract_routing_info, ConnectionType};

#[test]
fn test_parse_http_headers_fast() {
    let request = b"GET /api/v1/users HTTP/1.1\r\nHost: example.com\r\nUser-Agent: curl/7.68.0\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.path, "/api/v1/users");
    assert_eq!(result.host, "example.com");
}

#[test]
fn test_parse_http_headers_fast_post() {
    let request = b"POST /api/data HTTP/1.1\r\nHost: api.example.com\r\nContent-Type: application/json\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.path, "/api/data");
    assert_eq!(result.host, "api.example.com");
}

#[test]
fn test_parse_http_headers_fast_no_host() {
    let request = b"GET /api HTTP/1.1\r\nContent-Type: application/json\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.path, "/api");
    assert_eq!(result.host, "localhost"); // Default fallback when no Host header
}

#[test]
fn test_parse_http_headers_fast_complex_path() {
    let request = b"GET /api/v1/users/123?filter=active&sort=name HTTP/1.1\r\nHost: api.example.com\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.path, "/api/v1/users/123");
    assert_eq!(result.query_string, "filter=active&sort=name");
    assert_eq!(result.host, "api.example.com");
}

#[test]
fn test_parse_path_without_query_string() {
    let request = b"GET /api/users HTTP/1.1\r\nHost: example.com\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.path, "/api/users");
    assert_eq!(result.query_string, "");
}

#[test]
fn test_parse_path_with_empty_query_string() {
    let request = b"GET /api/users? HTTP/1.1\r\nHost: example.com\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.path, "/api/users");
    assert_eq!(result.query_string, "");
}

#[test]
fn test_parse_http_headers_fast_root_path() {
    let request = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.path, "/");
    assert_eq!(result.host, "example.com");
}

#[test]
fn test_parse_http_headers_fast_host_with_port() {
    let request = b"GET /api HTTP/1.1\r\nHost: example.com:8080\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.path, "/api");
    assert_eq!(result.host, "example.com"); // Normalized - port removed for Gateway API compliance
}

#[test]
fn test_parse_http_headers_fast_case_insensitive_host() {
    let request = b"GET /api HTTP/1.1\r\nHOST: example.com\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.path, "/api");
    assert_eq!(result.host, "example.com");
}

#[test]
fn test_parse_http_headers_fast_multiple_headers() {
    let request = b"GET /api HTTP/1.1\r\nUser-Agent: test\r\nHost: example.com\r\nAccept: application/json\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.path, "/api");
    assert_eq!(result.host, "example.com");
}

#[test]
fn test_parse_http_headers_fast_whitespace_in_headers() {
    let request = b"GET /api HTTP/1.1\r\nHost:    example.com   \r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.path, "/api");
    assert_eq!(result.host, "example.com");
}

#[test]
fn test_parse_http_headers_fast_different_methods() {
    let methods = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"];

    for method in &methods {
        let request = format!("{} /test HTTP/1.1\r\nHost: example.com\r\n\r\n", method);
        let result = extract_routing_info(request.as_bytes()).unwrap();
        assert_eq!(result.path, "/test");
        assert_eq!(result.host, "example.com");
    }
}

#[test]
fn test_parse_http_headers_fast_empty_request() {
    let request = b"";
    let result = extract_routing_info(request);
    match result {
        Ok(info) => {
            assert_eq!(info.path, "/");
            assert_eq!(info.host, "localhost");
        }
        Err(_) => {
            // Error is also acceptable for empty input
        }
    }
}

#[test]
fn test_parse_http_headers_fast_minimal_valid_request() {
    let request = b"GET / HTTP/1.1\r\n\r\n";
    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.path, "/");
    assert_eq!(result.host, "localhost");
}

#[test]
fn test_connection_close_header() {
    let request = b"GET /api HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.connection_type, ConnectionType::Close);
    assert_eq!(result.host, "example.com");
    assert_eq!(result.path, "/api");
}

#[test]
fn test_connection_close_with_trailing_whitespace() {
    let request = b"GET /api HTTP/1.1\r\nHost: example.com\r\nConnection: close \r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.connection_type, ConnectionType::Close);
    assert_eq!(result.host, "example.com");
    assert_eq!(result.path, "/api");
}

#[test]
fn test_connection_keep_alive_header() {
    let request = b"GET /api HTTP/1.1\r\nHost: example.com\r\nConnection: keep-alive\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.connection_type, ConnectionType::KeepAlive);
    assert_eq!(result.host, "example.com");
    assert_eq!(result.path, "/api");
}

#[test]
fn test_connection_default_behavior() {
    let request = b"GET /api HTTP/1.1\r\nHost: example.com\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.connection_type, ConnectionType::KeepAlive);
    assert_eq!(result.host, "example.com");
    assert_eq!(result.path, "/api");
}

#[test]
fn test_http10_default_close_behavior() {
    let request = b"GET /api HTTP/1.0\r\nHost: example.com\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.connection_type, ConnectionType::Close);
    assert_eq!(result.host, "example.com");
    assert_eq!(result.path, "/api");
}

#[test]
fn test_http11_default_keepalive_behavior() {
    let request = b"GET /api HTTP/1.1\r\nHost: example.com\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.connection_type, ConnectionType::KeepAlive);
    assert_eq!(result.host, "example.com");
    assert_eq!(result.path, "/api");
}

#[test]
fn test_apachebench_scenario() {
    let request = b"GET /index.html HTTP/1.0\r\nHost: localhost:8080\r\nUser-Agent: ApacheBench/2.3\r\nAccept: */*\r\n\r\n";

    let result = extract_routing_info(request).unwrap();
    assert_eq!(result.connection_type, ConnectionType::Close);
    assert_eq!(result.host, "localhost");
    assert_eq!(result.path, "/index.html");
}
