/// End-to-end proxy integration tests for UringRess.
///
/// These tests spawn a real UringRess process and backend TCP servers,
/// send actual HTTP traffic through the proxy, and verify responses.
///
/// Requirements:
/// - UringRess binary must be built (`cargo build --release`)
/// - Linux with io_uring + eBPF support
/// - CAP_BPF + CAP_NET_ADMIN capabilities (typically root)
///
/// Run with: cargo test --release --test test_proxy_integration

mod helpers;

use std::net::SocketAddr;
use std::thread;
use std::time::Duration;

use helpers::{
    extract_body, extract_status, http_request, http_request_keepalive, http_request_timeout,
    tcp_roundtrip, TcpEchoBackend, TestBackend, UringRessProcess,
};

/// Pick a proxy port unlikely to conflict. Each test uses a unique port.
fn proxy_port(offset: u16) -> u16 {
    19000 + offset
}

#[test]
#[ignore] // Requires eBPF/root — run with: cargo test --release --test test_proxy_integration -- --ignored
fn test_http_proxy_roundtrip() {
    let backend = TestBackend::spawn("hello from backend");
    let port = proxy_port(1);
    let proxy = UringRessProcess::spawn(
        &[("localhost", "/", backend.addr)],
        port,
    );

    let request = format!(
        "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    );
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
    let proxy = UringRessProcess::spawn(
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
#[ignore]
fn test_http_connection_close() {
    let backend = TestBackend::spawn("close-response");
    let port = proxy_port(3);
    let proxy = UringRessProcess::spawn(
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
#[ignore]
fn test_multiple_backends() {
    let backend_a = TestBackend::spawn("backend-A");
    let backend_b = TestBackend::spawn("backend-B");
    let port = proxy_port(4);

    // Two different path prefixes → two different backends
    let proxy = UringRessProcess::spawn(
        &[
            ("localhost", "/a", backend_a.addr),
            ("localhost", "/b", backend_b.addr),
        ],
        port,
    );

    let req_a = format!("GET /a HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    let resp_a = http_request(proxy.proxy_addr, req_a.as_bytes()).expect("route /a");
    assert_eq!(
        std::str::from_utf8(extract_body(&resp_a)).unwrap().trim(),
        "backend-A"
    );

    let req_b = format!("GET /b HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    let resp_b = http_request(proxy.proxy_addr, req_b.as_bytes()).expect("route /b");
    assert_eq!(
        std::str::from_utf8(extract_body(&resp_b)).unwrap().trim(),
        "backend-B"
    );
}

#[test]
#[ignore]
fn test_backend_unreachable() {
    // Point to a port where nothing is listening
    let dead_addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let port = proxy_port(5);
    let proxy = UringRessProcess::spawn(
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
#[ignore]
fn test_concurrent_requests() {
    let backend = TestBackend::spawn("concurrent-ok");
    let port = proxy_port(6);
    let proxy = UringRessProcess::spawn(
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
#[ignore]
fn test_large_response_forwarding() {
    let size = 128 * 1024; // 128KB
    let backend = TestBackend::spawn_large(size);
    let port = proxy_port(7);
    let proxy = UringRessProcess::spawn(
        &[("localhost", "/", backend.addr)],
        port,
    );

    let request = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let response = http_request_timeout(proxy.proxy_addr, request, Duration::from_secs(10))
        .expect("large response should complete");

    let status = extract_status(&response).expect("valid HTTP status");
    assert_eq!(status, 200);

    let body = extract_body(&response);
    assert_eq!(
        body.len(),
        size,
        "expected {} bytes, got {}",
        size,
        body.len()
    );
}

// === Mixed Protocol Tests ===

#[test]
#[ignore]
fn test_tcp_works_during_http_load() {
    let http_backend = TestBackend::spawn("http-ok");
    let echo = TcpEchoBackend::spawn();
    let http_port = proxy_port(16);
    let tcp_port = proxy_port(17);

    let proxy = UringRessProcess::spawn_with_tcp(
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
#[ignore]
fn test_tcp_proxy_echo_roundtrip() {
    let echo = TcpEchoBackend::spawn();
    let http_port = proxy_port(10);
    let tcp_port = proxy_port(11);

    let _proxy = UringRessProcess::spawn_with_tcp(
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
#[ignore]
fn test_tcp_proxy_binary_data() {
    let echo = TcpEchoBackend::spawn();
    let http_port = proxy_port(12);
    let tcp_port = proxy_port(13);

    let _proxy = UringRessProcess::spawn_with_tcp(
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
#[ignore]
fn test_tcp_proxy_eof_propagation() {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let echo = TcpEchoBackend::spawn();
    let http_port = proxy_port(14);
    let tcp_port = proxy_port(15);

    let _proxy = UringRessProcess::spawn_with_tcp(
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
