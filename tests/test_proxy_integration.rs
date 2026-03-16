//! End-to-end proxy integration tests for Portail.
//!
//! These tests spawn a real Portail process and backend TCP servers,
//! send actual HTTP traffic through the proxy, and verify responses.
//!
//! Requirements:
//! - Portail binary must be built (`cargo build --release`)
//!
//! Run with: cargo test --release --test test_proxy_integration

mod helpers;

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::thread;
use std::time::Duration;

use helpers::{
    extract_body, extract_status, http_request, http_request_keepalive, http_request_timeout,
    tcp_roundtrip, TcpEchoBackend, TestBackend, InspectingBackend, PortailProcess,
};

/// Pick a proxy port unlikely to conflict. Each test uses a unique port.
fn proxy_port(offset: u16) -> u16 {
    19000 + offset
}

#[test]
fn test_http_proxy_roundtrip() {
    let backend = TestBackend::spawn("hello from backend");
    let port = proxy_port(1);
    let proxy = PortailProcess::spawn(
        &[("localhost", "/", backend.addr)],
        port,
    );

    let request = "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n".to_string();
    let response = http_request(proxy.proxy_addr, request.as_bytes())
        .expect("proxy should return a response");

    let status = extract_status(&response).expect("valid HTTP status");
    assert_eq!(status, 200, "expected 200 OK, got {}", status);

    let body = extract_body(&response);
    assert_eq!(
        std::str::from_utf8(body).unwrap().trim(),
        "hello from backend"
    );
}

#[test]
#[ignore]
fn test_http_keepalive_reuse() {
    let backend = TestBackend::spawn("keepalive-response");
    let port = proxy_port(2);
    let proxy = PortailProcess::spawn(
        &[("localhost", "/", backend.addr)],
        port,
    );

    let req = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let responses = http_request_keepalive(proxy.proxy_addr, &[req, req])
        .expect("both requests should succeed on same connection");

    assert_eq!(responses.len(), 2);
    for (i, resp) in responses.iter().enumerate() {
        let status = extract_status(resp).expect("valid HTTP status");
        assert_eq!(status, 200, "request {} got status {}", i, status);
        let body = extract_body(resp);
        assert_eq!(
            std::str::from_utf8(body).unwrap().trim(),
            "keepalive-response",
            "request {} body mismatch",
            i
        );
    }
}

#[test]
fn test_http_connection_close() {
    let backend = TestBackend::spawn("close-response");
    let port = proxy_port(3);
    let proxy = PortailProcess::spawn(
        &[("localhost", "/", backend.addr)],
        port,
    );

    let request = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let response =
        http_request(proxy.proxy_addr, request).expect("should get response before close");

    let status = extract_status(&response).expect("valid HTTP status");
    assert_eq!(status, 200);
    assert_eq!(
        std::str::from_utf8(extract_body(&response)).unwrap().trim(),
        "close-response"
    );
}

#[test]
fn test_multiple_backends() {
    let backend_a = TestBackend::spawn("backend-A");
    let backend_b = TestBackend::spawn("backend-B");
    let port = proxy_port(4);

    // Two different path prefixes → two different backends
    let proxy = PortailProcess::spawn(
        &[
            ("localhost", "/a", backend_a.addr),
            ("localhost", "/b", backend_b.addr),
        ],
        port,
    );

    let req_a = "GET /a HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n".to_string();
    let resp_a = http_request(proxy.proxy_addr, req_a.as_bytes()).expect("route /a");
    assert_eq!(
        std::str::from_utf8(extract_body(&resp_a)).unwrap().trim(),
        "backend-A"
    );

    let req_b = "GET /b HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n".to_string();
    let resp_b = http_request(proxy.proxy_addr, req_b.as_bytes()).expect("route /b");
    assert_eq!(
        std::str::from_utf8(extract_body(&resp_b)).unwrap().trim(),
        "backend-B"
    );
}

#[test]
fn test_backend_unreachable() {
    // Point to a port where nothing is listening
    let dead_addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let port = proxy_port(5);
    let proxy = PortailProcess::spawn(
        &[("localhost", "/", dead_addr)],
        port,
    );

    let request = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let result = http_request_timeout(proxy.proxy_addr, request, Duration::from_secs(3));

    match result {
        Ok(response) => {
            // Proxy might return an error page (502/503) or reset the connection.
            // Either is acceptable — the key requirement is "no hang".
            let status = extract_status(&response);
            if let Some(code) = status {
                assert!(
                    code >= 400,
                    "expected error status for unreachable backend, got {}",
                    code
                );
            }
        }
        Err(_) => {
            // Connection reset or timeout within our deadline is acceptable
        }
    }
}

#[test]
fn test_concurrent_requests() {
    let backend = TestBackend::spawn("concurrent-ok");
    let port = proxy_port(6);
    let proxy = PortailProcess::spawn(
        &[("localhost", "/", backend.addr)],
        port,
    );

    let proxy_addr = proxy.proxy_addr;
    let num_threads = 8;
    let requests_per_thread = 10;

    let failures: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    let handles: Vec<_> = (0..num_threads)
        .map(|t| {
            let failures = failures.clone();
            thread::spawn(move || {
                let mut successes = 0;
                for i in 0..requests_per_thread {
                    let request =
                        b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
                    match http_request(proxy_addr, request) {
                        Ok(response) => {
                            let status = extract_status(&response);
                            let body = extract_body(&response);
                            if status == Some(200) && body == b"concurrent-ok" {
                                successes += 1;
                            } else {
                                let body_str = String::from_utf8_lossy(body);
                                let msg = format!(
                                    "t{}r{}: status={:?} body_len={} body={:?}",
                                    t, i, status, body.len(),
                                    if body_str.len() > 200 { &body_str[..200] } else { &body_str }
                                );
                                failures.lock().unwrap().push(msg);
                            }
                        }
                        Err(e) => {
                            failures.lock().unwrap().push(format!("t{}r{}: err={}", t, i, e));
                        }
                    }
                }
                successes
            })
        })
        .collect();

    let total_successes: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    let total_requests = num_threads * requests_per_thread;

    let failure_details = failures.lock().unwrap();
    if !failure_details.is_empty() {
        eprintln!("=== Failure details ({} failures) ===", failure_details.len());
        for f in failure_details.iter() {
            eprintln!("  {}", f);
        }
    }

    // Allow some failures under concurrent load, but majority must succeed
    let success_rate = total_successes as f64 / total_requests as f64;
    assert!(
        success_rate > 0.9,
        "concurrent success rate too low: {}/{} ({:.1}%)",
        total_successes,
        total_requests,
        success_rate * 100.0
    );
}

#[test]
fn test_large_response_forwarding() {
    let size = 645 * 1024; // 645KB — matches HA frontend JS bundle size
    let backend = TestBackend::spawn_large(size);
    let port = proxy_port(7);
    let proxy = PortailProcess::spawn(
        &[("localhost", "/", backend.addr)],
        port,
    );

    // 20 rapid requests to catch intermittent TLS flush stalls (~5% rate)
    for i in 0..20 {
        let request = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        let response = http_request_timeout(proxy.proxy_addr, request, Duration::from_secs(10))
            .unwrap_or_else(|e| panic!("request {}: large response timed out: {}", i, e));

        let status = extract_status(&response).expect("valid HTTP status");
        assert_eq!(status, 200, "request {}: expected 200, got {}", i, status);

        let body = extract_body(&response);
        assert_eq!(
            body.len(),
            size,
            "request {}: expected {} bytes, got {} (partial delivery)",
            i, size, body.len()
        );
    }
}

// === Mixed Protocol Tests ===

#[test]
fn test_tcp_works_during_http_load() {
    let http_backend = TestBackend::spawn("http-ok");
    let echo = TcpEchoBackend::spawn();
    let http_port = proxy_port(16);
    let tcp_port = proxy_port(17);

    let proxy = PortailProcess::spawn_with_tcp(
        &[("localhost", "/", http_backend.addr)],
        http_port,
        &[("tcp-mixed", tcp_port, echo.addr)],
    );

    let tcp_addr: std::net::SocketAddr = format!("127.0.0.1:{}", tcp_port).parse().unwrap();

    // Verify TCP works before HTTP load
    let resp = tcp_roundtrip(tcp_addr, b"before-load", Duration::from_secs(5))
        .expect("TCP should work before HTTP load");
    assert_eq!(resp, b"before-load");

    // Start sustained HTTP load in background threads
    let http_addr = proxy.proxy_addr;
    let http_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let http_handles: Vec<_> = (0..4)
        .map(|_| {
            let done = http_done.clone();
            thread::spawn(move || {
                let mut count = 0u32;
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    let req = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
                    let _ = http_request_timeout(http_addr, req, Duration::from_secs(2));
                    count += 1;
                }
                count
            })
        })
        .collect();

    // Let HTTP load build up
    thread::sleep(Duration::from_millis(200));

    // TCP must still work while HTTP load is active
    for i in 0..5 {
        let msg = format!("during-load-{}", i);
        let resp = tcp_roundtrip(tcp_addr, msg.as_bytes(), Duration::from_secs(5))
            .unwrap_or_else(|e| panic!("TCP roundtrip {} failed during HTTP load: {}", i, e));
        assert_eq!(resp, msg.as_bytes(), "TCP echo mismatch during HTTP load (iteration {})", i);
    }

    // Stop HTTP load
    http_done.store(true, std::sync::atomic::Ordering::Relaxed);
    let total_http: u32 = http_handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(total_http > 0, "HTTP threads should have completed some requests");

    // TCP must still work after HTTP load stops
    let resp = tcp_roundtrip(tcp_addr, b"after-load", Duration::from_secs(5))
        .expect("TCP should work after HTTP load");
    assert_eq!(resp, b"after-load");
}

// === TCP Proxy Tests ===

#[test]
fn test_tcp_proxy_echo_roundtrip() {
    let echo = TcpEchoBackend::spawn();
    let http_port = proxy_port(10);
    let tcp_port = proxy_port(11);

    let _proxy = PortailProcess::spawn_with_tcp(
        &[],
        http_port,
        &[("tcp-echo", tcp_port, echo.addr)],
    );

    let tcp_addr: std::net::SocketAddr = format!("127.0.0.1:{}", tcp_port).parse().unwrap();

    // First message
    let response = tcp_roundtrip(tcp_addr, b"hello from client", Duration::from_secs(5))
        .expect("TCP echo should work");
    assert_eq!(response, b"hello from client", "first echo mismatch");

    // Second message on a new connection (persistent backend)
    let response = tcp_roundtrip(tcp_addr, b"second message", Duration::from_secs(5))
        .expect("TCP echo should work on second connection");
    assert_eq!(response, b"second message", "second echo mismatch");
}

#[test]
fn test_tcp_proxy_binary_data() {
    let echo = TcpEchoBackend::spawn();
    let http_port = proxy_port(12);
    let tcp_port = proxy_port(13);

    let _proxy = PortailProcess::spawn_with_tcp(
        &[],
        http_port,
        &[("tcp-bin", tcp_port, echo.addr)],
    );

    let tcp_addr: std::net::SocketAddr = format!("127.0.0.1:{}", tcp_port).parse().unwrap();

    // 4KB of binary data including null bytes, 0xFF, SSH-like patterns
    let mut data = vec![0u8; 4096];
    for (i, byte) in data.iter_mut().enumerate() {
        *byte = (i % 256) as u8;
    }
    // Include bytes that look like "HTTP" to verify no parsing corruption
    data[0..4].copy_from_slice(b"SSH-");
    data[100..104].copy_from_slice(&[0x00, 0xFF, 0x00, 0xFF]);

    let response = tcp_roundtrip(tcp_addr, &data, Duration::from_secs(5))
        .expect("binary TCP roundtrip should work");
    assert_eq!(response, data, "binary data corrupted through TCP proxy");
}

#[test]
fn test_tcp_proxy_eof_propagation() {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let echo = TcpEchoBackend::spawn();
    let http_port = proxy_port(14);
    let tcp_port = proxy_port(15);

    let _proxy = PortailProcess::spawn_with_tcp(
        &[],
        http_port,
        &[("tcp-eof", tcp_port, echo.addr)],
    );

    let tcp_addr: std::net::SocketAddr = format!("127.0.0.1:{}", tcp_port).parse().unwrap();
    let timeout = Duration::from_secs(5);

    let mut stream = TcpStream::connect_timeout(&tcp_addr, timeout).expect("connect");
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();

    stream.write_all(b"ping").expect("write");

    let mut buf = [0u8; 64];
    let mut response = Vec::new();
    while response.len() < 4 {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut
                || e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => panic!("read error: {}", e),
        }
    }
    assert_eq!(response, b"ping");

    // Close our side — connection should not hang
    drop(stream);

    // Verify we can establish a new connection (proxy didn't leak)
    thread::sleep(Duration::from_millis(100));
    let response2 = tcp_roundtrip(tcp_addr, b"after-eof", Duration::from_secs(5))
        .expect("new connection after EOF should work");
    assert_eq!(response2, b"after-eof");
}

// === Filter Integration Tests ===

fn extract_header_value<'a>(response: &'a [u8], name: &str) -> Option<&'a str> {
    let header_end = response.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let headers_str = std::str::from_utf8(&response[..header_end]).ok()?;
    for line in headers_str.lines().skip(1) {
        if let Some(colon) = line.find(':') {
            if line[..colon].eq_ignore_ascii_case(name) {
                return Some(line[colon + 1..].trim());
            }
        }
    }
    None
}

fn extract_request_path(raw_request: &[u8]) -> Option<String> {
    let first_line_end = raw_request.iter().position(|&b| b == b'\r')?;
    let first_line = std::str::from_utf8(&raw_request[..first_line_end]).ok()?;
    let path = first_line.split_whitespace().nth(1)?;
    Some(path.to_string())
}

fn extract_request_header_value(raw_request: &[u8], name: &str) -> Option<String> {
    let req_str = std::str::from_utf8(raw_request).ok()?;
    for line in req_str.lines().skip(1) {
        if let Some(colon) = line.find(':') {
            if line[..colon].eq_ignore_ascii_case(name) {
                return Some(line[colon + 1..].trim().to_string());
            }
        }
    }
    None
}

#[test]
fn test_url_rewrite_full_path_e2e() {
    let backend = InspectingBackend::spawn("ok");
    let port = proxy_port(20);
    let filter = r#"{"type": "URLRewrite", "urlRewrite": {"path": {"type": "ReplaceFullPath", "replaceFullPath": "/new"}}}"#;
    let proxy = PortailProcess::spawn_with_filters(
        &[("localhost", "/", backend.addr, filter)],
        port,
    );

    let request = b"GET /old HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let response = http_request(proxy.proxy_addr, request).expect("should get response");
    assert_eq!(extract_status(&response), Some(200));

    thread::sleep(Duration::from_millis(100));
    let requests = backend.received_requests();
    assert!(!requests.is_empty(), "backend should have received a request");
    let path = extract_request_path(&requests[0]).unwrap();
    assert_eq!(path, "/new", "backend should receive rewritten path");
}

#[test]
fn test_url_rewrite_prefix_replace_e2e() {
    let backend = InspectingBackend::spawn("ok");
    let port = proxy_port(21);
    let filter = r#"{"type": "URLRewrite", "urlRewrite": {"path": {"type": "ReplacePrefixMatch", "replacePrefixMatch": "/v2"}}}"#;
    let proxy = PortailProcess::spawn_with_filters(
        &[("localhost", "/v1", backend.addr, filter)],
        port,
    );

    let request = b"GET /v1/users HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let response = http_request(proxy.proxy_addr, request).expect("should get response");
    assert_eq!(extract_status(&response), Some(200));

    thread::sleep(Duration::from_millis(100));
    let requests = backend.received_requests();
    assert!(!requests.is_empty());
    let path = extract_request_path(&requests[0]).unwrap();
    assert_eq!(path, "/v2/users", "prefix /v1 should be replaced with /v2");
}

#[test]
fn test_url_rewrite_hostname_e2e() {
    let backend = InspectingBackend::spawn("ok");
    let port = proxy_port(22);
    let filter = r#"{"type": "URLRewrite", "urlRewrite": {"hostname": "rewritten.example.com"}}"#;
    let proxy = PortailProcess::spawn_with_filters(
        &[("localhost", "/", backend.addr, filter)],
        port,
    );

    let request = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let response = http_request(proxy.proxy_addr, request).expect("should get response");
    assert_eq!(extract_status(&response), Some(200));

    thread::sleep(Duration::from_millis(100));
    let requests = backend.received_requests();
    assert!(!requests.is_empty());
    let host = extract_request_header_value(&requests[0], "Host").unwrap();
    assert_eq!(host, "rewritten.example.com");
}

#[test]
fn test_request_header_modifier_e2e() {
    let backend = InspectingBackend::spawn("ok");
    let port = proxy_port(23);
    let filter = r#"{"type": "RequestHeaderModifier", "requestHeaderModifier": {"add": [{"name": "X-Added", "value": "yes"}], "remove": ["User-Agent"]}}"#;
    let proxy = PortailProcess::spawn_with_filters(
        &[("localhost", "/", backend.addr, filter)],
        port,
    );

    let request = b"GET / HTTP/1.1\r\nHost: localhost\r\nUser-Agent: test\r\nConnection: close\r\n\r\n";
    let response = http_request(proxy.proxy_addr, request).expect("should get response");
    assert_eq!(extract_status(&response), Some(200));

    thread::sleep(Duration::from_millis(100));
    let requests = backend.received_requests();
    assert!(!requests.is_empty());
    let req_str = String::from_utf8_lossy(&requests[0]);
    assert!(req_str.contains("X-Added: yes"), "X-Added header should be present");
    assert!(!req_str.to_lowercase().contains("user-agent"), "User-Agent should be removed");
}

#[test]
fn test_response_header_modifier_e2e() {
    // Use a TestBackend that adds X-Internal header in its response
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let backend_addr = listener.local_addr().unwrap();
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();

    listener.set_nonblocking(true).unwrap();
    let _handle = thread::spawn(move || {
        while !shutdown_clone.load(std::sync::atomic::Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf);
                    let resp = "HTTP/1.1 200 OK\r\nX-Internal: secret\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
                    let _ = stream.write_all(resp.as_bytes());
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    let port = proxy_port(24);
    let filter = r#"{"type": "ResponseHeaderModifier", "responseHeaderModifier": {"remove": ["X-Internal"]}}"#;
    let proxy = PortailProcess::spawn_with_filters(
        &[("localhost", "/", backend_addr, filter)],
        port,
    );

    let request = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let response = http_request(proxy.proxy_addr, request).expect("should get response");
    assert_eq!(extract_status(&response), Some(200));

    let resp_str = String::from_utf8_lossy(&response);
    assert!(!resp_str.contains("X-Internal"), "X-Internal should be removed from response");

    shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
}

#[test]
fn test_request_redirect_e2e() {
    let port = proxy_port(25);
    let filter = r#"{"type": "RequestRedirect", "requestRedirect": {"hostname": "other.com", "statusCode": 301}}"#;
    // Redirect rules don't need backend_refs, but our helper always adds one.
    // The proxy should return redirect before contacting any backend.
    let dead_addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let proxy = PortailProcess::spawn_with_filters(
        &[("localhost", "/", dead_addr, filter)],
        port,
    );

    let request = b"GET /page HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let response = http_request(proxy.proxy_addr, request).expect("should get redirect");
    assert_eq!(extract_status(&response), Some(301));

    let location = extract_header_value(&response, "Location");
    assert!(location.is_some(), "redirect should have Location header");
    assert!(location.unwrap().contains("other.com"), "Location should contain new hostname");
}

#[test]
fn test_request_mirror_e2e() {
    let primary = InspectingBackend::spawn("primary-ok");
    let mirror = InspectingBackend::spawn("mirror-ok");
    let port = proxy_port(26);

    let filter = format!(
        r#"{{"type": "RequestMirror", "requestMirror": {{"backendRef": {{"name": "{}", "port": {}}}}}}}"#,
        mirror.addr.ip(), mirror.addr.port()
    );
    let proxy = PortailProcess::spawn_with_filters(
        &[("localhost", "/", primary.addr, &filter)],
        port,
    );

    let request = b"GET /test HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let response = http_request(proxy.proxy_addr, request).expect("should get response");
    assert_eq!(extract_status(&response), Some(200));
    assert_eq!(std::str::from_utf8(extract_body(&response)).unwrap().trim(), "primary-ok");

    // Give mirror time to receive the fire-and-forget request
    thread::sleep(Duration::from_millis(500));
    let mirror_requests = mirror.received_requests();
    assert!(!mirror_requests.is_empty(), "mirror backend should have received the request");
}

#[test]
fn test_filter_combination_rewrite_plus_header_mod() {
    let backend = InspectingBackend::spawn("ok");
    let port = proxy_port(27);
    let filters = r#"{"type": "URLRewrite", "urlRewrite": {"path": {"type": "ReplaceFullPath", "replaceFullPath": "/new"}}}, {"type": "RequestHeaderModifier", "requestHeaderModifier": {"add": [{"name": "X-Rewritten", "value": "true"}]}}"#;
    let proxy = PortailProcess::spawn_with_filters(
        &[("localhost", "/", backend.addr, filters)],
        port,
    );

    let request = b"GET /old HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let response = http_request(proxy.proxy_addr, request).expect("should get response");
    assert_eq!(extract_status(&response), Some(200));

    thread::sleep(Duration::from_millis(100));
    let requests = backend.received_requests();
    assert!(!requests.is_empty());
    let path = extract_request_path(&requests[0]).unwrap();
    assert_eq!(path, "/new");
    let req_str = String::from_utf8_lossy(&requests[0]);
    assert!(req_str.contains("X-Rewritten: true"));
}

#[test]
fn test_exact_vs_prefix_path_e2e() {
    let backend_exact = InspectingBackend::spawn("exact");
    let backend_prefix = InspectingBackend::spawn("prefix");
    let port = proxy_port(28);

    // Use two separate routes: exact /foo and prefix /foo
    // We need a custom config for this since both go to different backends
    let config = format!(
        r#"{{
  "gateway": {{
    "name": "test-gateway",
    "listeners": [{{"name": "http", "protocol": "HTTP", "port": {}}}],
    "workerThreads": 1
  }},
  "httpRoutes": [{{
    "parentRefs": [{{"name": "test-gateway", "sectionName": "http"}}],
    "hostnames": ["localhost"],
    "rules": [
      {{
        "matches": [{{"path": {{"type": "Exact", "value": "/foo"}}}}],
        "backendRefs": [{{"name": "{}", "port": {}}}]
      }},
      {{
        "matches": [{{"path": {{"type": "PathPrefix", "value": "/foo"}}}}],
        "backendRefs": [{{"name": "{}", "port": {}}}]
      }}
    ]
  }}],
  "observability": {{"logging": {{"level": "debug", "format": "pretty", "output": "stderr"}}}},
  "performance": {{}}
}}"#,
        port,
        backend_exact.addr.ip(), backend_exact.addr.port(),
        backend_prefix.addr.ip(), backend_prefix.addr.port()
    );

    let config_file = tempfile::Builder::new()
        .suffix(".json")
        .tempfile()
        .expect("create temp config");
    std::fs::write(config_file.path(), config).expect("write temp config");

    let binary = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/release/portail");
    let debug_binary = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/debug/portail");
    let bin = if binary.exists() { binary } else { debug_binary };

    let mut child = std::process::Command::new(&bin)
        .arg("--config")
        .arg(config_file.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("Failed to spawn {:?}: {}", bin, e));

    let proxy_addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if std::time::Instant::now() > deadline { panic!("timeout waiting for proxy"); }
        if std::net::TcpStream::connect_timeout(&proxy_addr, Duration::from_millis(100)).is_ok() { break; }
        thread::sleep(Duration::from_millis(50));
    }

    // GET /foo → exact match → backend_exact
    let req = b"GET /foo HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let resp = http_request(proxy_addr, req).expect("exact request");
    assert_eq!(std::str::from_utf8(extract_body(&resp)).unwrap().trim(), "exact");

    // GET /foo/bar → prefix match → backend_prefix
    let req = b"GET /foo/bar HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let resp = http_request(proxy_addr, req).expect("prefix request");
    assert_eq!(std::str::from_utf8(extract_body(&resp)).unwrap().trim(), "prefix");

    let _ = child.kill();
    let _ = child.wait();
}

// === POST Body Forwarding Tests ===

#[test]
fn test_post_body_forwarded() {
    let backend = InspectingBackend::spawn("ok");
    let port = proxy_port(30);
    let proxy = PortailProcess::spawn(
        &[("localhost", "/", backend.addr)],
        port,
    );

    // 4KB JSON body — fits in the initial 64KB buffer
    let body = format!("{{\"data\": \"{}\"}}", "x".repeat(4000));
    let request = format!(
        "POST /api/query HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body,
    );
    let response = http_request(proxy.proxy_addr, request.as_bytes())
        .expect("POST should return a response");

    let status = extract_status(&response).expect("valid HTTP status");
    assert_eq!(status, 200, "expected 200 OK, got {}", status);

    thread::sleep(Duration::from_millis(200));
    let requests = backend.received_requests();
    assert!(!requests.is_empty(), "backend should have received a request");

    // Verify the full body was forwarded
    let received = &requests[0];
    let header_end = received.windows(4).position(|w| w == b"\r\n\r\n")
        .expect("should find header boundary") + 4;
    let received_body = &received[header_end..];
    assert_eq!(
        received_body.len(), body.len(),
        "backend received {} body bytes, expected {}",
        received_body.len(), body.len()
    );
    assert_eq!(
        std::str::from_utf8(received_body).unwrap(), body,
        "body content mismatch"
    );
}

#[test]
fn test_post_large_body_forwarded() {
    let backend = InspectingBackend::spawn("ok");
    let port = proxy_port(31);
    let proxy = PortailProcess::spawn(
        &[("localhost", "/", backend.addr)],
        port,
    );

    // 128KB body — exceeds the 64KB initial buffer, requires body relay
    let body = "x".repeat(128 * 1024);
    let request = format!(
        "POST /api/ds/query HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body,
    );
    let response = http_request_timeout(proxy.proxy_addr, request.as_bytes(), Duration::from_secs(10))
        .expect("large POST should return a response (not timeout)");

    let status = extract_status(&response).expect("valid HTTP status");
    assert_eq!(status, 200, "expected 200 OK, got {}", status);

    thread::sleep(Duration::from_millis(200));
    let requests = backend.received_requests();
    assert!(!requests.is_empty(), "backend should have received a request");

    let received = &requests[0];
    let header_end = received.windows(4).position(|w| w == b"\r\n\r\n")
        .expect("should find header boundary") + 4;
    let received_body = &received[header_end..];
    assert_eq!(
        received_body.len(), body.len(),
        "backend received {} body bytes, expected {} (128KB)",
        received_body.len(), body.len()
    );
}

// === WebSocket Upgrade Tests ===

/// Minimal WebSocket-like backend: accepts the upgrade, sends 101, then echoes raw data.
fn spawn_websocket_backend() -> (SocketAddr, std::sync::Arc<std::sync::atomic::AtomicBool>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();

    listener.set_nonblocking(true).unwrap();
    thread::spawn(move || {
        while !shutdown_clone.load(std::sync::atomic::Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
                    stream.set_nonblocking(false).ok();

                    // Read the upgrade request
                    let mut buf = [0u8; 4096];
                    let mut accum = Vec::new();
                    loop {
                        let n = match stream.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(_) => break,
                        };
                        accum.extend_from_slice(&buf[..n]);
                        if accum.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }

                    // Verify it's an upgrade request
                    let req_str = String::from_utf8_lossy(&accum);
                    if !req_str.to_lowercase().contains("upgrade") {
                        let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n");
                        continue;
                    }

                    // Send 101 Switching Protocols
                    let response = "HTTP/1.1 101 Switching Protocols\r\n\
                                    Upgrade: websocket\r\n\
                                    Connection: Upgrade\r\n\r\n";
                    if stream.write_all(response.as_bytes()).is_err() {
                        continue;
                    }

                    // Echo loop: read data and echo it back (raw, no WS framing)
                    loop {
                        let n = match stream.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(_) => break,
                        };
                        if stream.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });

    (addr, shutdown)
}

#[test]
fn test_websocket_upgrade_through_proxy() {
    let (backend_addr, shutdown) = spawn_websocket_backend();
    let port = proxy_port(32);
    let proxy = PortailProcess::spawn(
        &[("localhost", "/", backend_addr)],
        port,
    );

    // Connect to the proxy and send a WebSocket upgrade request
    let proxy_addr = proxy.proxy_addr;
    let mut stream = std::net::TcpStream::connect_timeout(&proxy_addr, Duration::from_secs(5))
        .expect("connect to proxy");
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let upgrade_request = "GET /api/live/ws HTTP/1.1\r\n\
                           Host: localhost\r\n\
                           Upgrade: websocket\r\n\
                           Connection: Upgrade\r\n\
                           Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                           Sec-WebSocket-Version: 13\r\n\r\n";
    stream.write_all(upgrade_request.as_bytes()).expect("send upgrade request");

    // Read the 101 response
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("timeout waiting for 101 response, got {} bytes: {:?}",
                response.len(), String::from_utf8_lossy(&response));
        }
        match stream.read(&mut buf) {
            Ok(0) => panic!("connection closed before 101 response"),
            Ok(n) => {
                response.extend_from_slice(&buf[..n]);
                if response.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => panic!("read error waiting for 101: {}", e),
        }
    }

    let status = extract_status(&response).expect("valid HTTP status in upgrade response");
    assert_eq!(status, 101, "expected 101 Switching Protocols, got {}", status);

    // Now the connection should be in raw bidirectional mode.
    // Send some data and verify it echoes back.
    let test_data = b"hello websocket through proxy";
    stream.write_all(test_data).expect("write test data after upgrade");

    let mut echo_buf = [0u8; 256];
    let mut echo_response = Vec::new();
    let echo_deadline = std::time::Instant::now() + Duration::from_secs(3);
    while echo_response.len() < test_data.len() {
        if std::time::Instant::now() > echo_deadline {
            break;
        }
        match stream.read(&mut echo_buf) {
            Ok(0) => break,
            Ok(n) => echo_response.extend_from_slice(&echo_buf[..n]),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(_) => break,
        }
    }

    assert_eq!(
        echo_response, test_data,
        "data should echo through WebSocket upgrade, got {:?}",
        String::from_utf8_lossy(&echo_response)
    );

    drop(stream);
    shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
}
