use uringress::parser::parse_http_headers_fast;

#[test]
fn test_parse_http_headers_fast() {
    let request = b"GET /api/v1/users HTTP/1.1\r\nHost: example.com\r\nUser-Agent: curl/7.68.0\r\n\r\n";
    
    let result = parse_http_headers_fast(request).unwrap();
    assert_eq!(result.0, "GET");
    assert_eq!(result.1, "/api/v1/users");
    assert_eq!(result.2, Some("example.com"));
}

#[test]
fn test_parse_http_headers_fast_post() {
    let request = b"POST /api/data HTTP/1.1\r\nHost: api.example.com\r\nContent-Type: application/json\r\n\r\n";
    
    let result = parse_http_headers_fast(request).unwrap();
    assert_eq!(result.0, "POST");
    assert_eq!(result.1, "/api/data");
    assert_eq!(result.2, Some("api.example.com"));
}

#[test]
fn test_parse_http_headers_fast_no_host() {
    let request = b"GET /api HTTP/1.1\r\nContent-Type: application/json\r\n\r\n";
    
    let result = parse_http_headers_fast(request).unwrap();
    assert_eq!(result.0, "GET");
    assert_eq!(result.1, "/api");
    assert_eq!(result.2, None);
}

#[test]
fn test_parse_http_headers_fast_complex_path() {
    let request = b"GET /api/v1/users/123?filter=active&sort=name HTTP/1.1\r\nHost: api.example.com\r\n\r\n";
    
    let result = parse_http_headers_fast(request).unwrap();
    assert_eq!(result.0, "GET");
    assert_eq!(result.1, "/api/v1/users/123?filter=active&sort=name");
    assert_eq!(result.2, Some("api.example.com"));
}

#[test]
fn test_parse_http_headers_fast_root_path() {
    let request = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
    
    let result = parse_http_headers_fast(request).unwrap();
    assert_eq!(result.0, "GET");
    assert_eq!(result.1, "/");
    assert_eq!(result.2, Some("example.com"));
}

#[test]
fn test_parse_http_headers_fast_host_with_port() {
    let request = b"GET /api HTTP/1.1\r\nHost: example.com:8080\r\n\r\n";
    
    let result = parse_http_headers_fast(request).unwrap();
    assert_eq!(result.0, "GET");
    assert_eq!(result.1, "/api");
    assert_eq!(result.2, Some("example.com:8080"));
}

#[test]
fn test_parse_http_headers_fast_case_insensitive_host() {
    let request = b"GET /api HTTP/1.1\r\nHOST: example.com\r\n\r\n";
    
    let result = parse_http_headers_fast(request).unwrap();
    assert_eq!(result.0, "GET");
    assert_eq!(result.1, "/api");
    assert_eq!(result.2, Some("example.com"));
}

#[test]
fn test_parse_http_headers_fast_multiple_headers() {
    let request = b"GET /api HTTP/1.1\r\nUser-Agent: test\r\nHost: example.com\r\nAccept: application/json\r\n\r\n";
    
    let result = parse_http_headers_fast(request).unwrap();
    assert_eq!(result.0, "GET");
    assert_eq!(result.1, "/api");
    assert_eq!(result.2, Some("example.com"));
}

#[test]
fn test_parse_http_headers_fast_whitespace_in_headers() {
    let request = b"GET /api HTTP/1.1\r\nHost:    example.com   \r\n\r\n";
    
    let result = parse_http_headers_fast(request).unwrap();
    assert_eq!(result.0, "GET");
    assert_eq!(result.1, "/api");
    assert_eq!(result.2, Some("example.com"));
}

#[test]
fn test_parse_http_headers_fast_different_methods() {
    let methods = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"];
    
    for method in &methods {
        let request = format!("{} /test HTTP/1.1\r\nHost: example.com\r\n\r\n", method);
        let result = parse_http_headers_fast(request.as_bytes()).unwrap();
        assert_eq!(result.0, *method);
        assert_eq!(result.1, "/test");
        assert_eq!(result.2, Some("example.com"));
    }
}

#[test]
fn test_parse_http_headers_fast_empty_request() {
    let request = b"";
    let result = parse_http_headers_fast(request);
    assert!(result.is_err());
}

#[test]
fn test_parse_http_headers_fast_invalid_request_line() {
    let request = b"INVALID\r\n\r\n";
    let result = parse_http_headers_fast(request);
    assert!(result.is_err());
}

#[test]
fn test_parse_http_headers_fast_missing_path() {
    let request = b"GET HTTP/1.1\r\nHost: example.com\r\n\r\n";
    let result = parse_http_headers_fast(request);
    assert!(result.is_err());
}

#[test]
fn test_parse_http_headers_fast_missing_version() {
    let request = b"GET /api\r\nHost: example.com\r\n\r\n";
    let result = parse_http_headers_fast(request);
    assert!(result.is_err());
}