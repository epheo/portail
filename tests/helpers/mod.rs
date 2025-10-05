use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Minimal TCP backend that accepts connections and writes canned HTTP responses.
pub struct TestBackend {
    pub addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TestBackend {
    /// Spawn a backend that responds to every HTTP request with `response_body`.
    /// The response includes proper Content-Length and Connection headers.
    pub fn spawn(response_body: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test backend");
        let addr = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let body = response_body.to_string();

        listener.set_nonblocking(true).unwrap();

        let handle = thread::spawn(move || {
            while !shutdown_clone.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let body = body.clone();
                        let shutdown = shutdown_clone.clone();
                        thread::spawn(move || {
                            handle_backend_connection(stream, &body, &shutdown);
                        });
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            shutdown,
            handle: Some(handle),
        }
    }

    /// Spawn a backend that sends a fixed-size binary payload (for large response tests).
    pub fn spawn_large(size: usize) -> Self {
        let body = "x".repeat(size);
        Self::spawn(&body)
    }
}

fn handle_backend_connection(mut stream: TcpStream, body: &str, shutdown: &AtomicBool) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let mut buf = [0u8; 8192];
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }

        let n = match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(_) => return,
        };

        // Wait until we see the end of HTTP headers
        let request = &buf[..n];
        if !request.windows(4).any(|w| w == b"\r\n\r\n") {
            continue;
        }

        let keep_alive = request_is_keepalive(request);

        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n{}",
            body.len(),
            if keep_alive { "keep-alive" } else { "close" },
            body
        );

        if stream.write_all(response.as_bytes()).is_err() {
            return;
        }

        if !keep_alive {
            return;
        }
    }
}

fn request_is_keepalive(request: &[u8]) -> bool {
    // HTTP/1.1 defaults to keep-alive unless Connection: close
    let req_str = std::str::from_utf8(request).unwrap_or("");
    if req_str.contains("HTTP/1.0") {
        return false;
    }
    !req_str.to_lowercase().contains("connection: close")
}

impl Drop for TestBackend {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // Connect to unblock the accept loop
        let _ = TcpStream::connect(self.addr);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Raw TCP echo backend — echoes bytes back without HTTP framing.
pub struct TcpEchoBackend {
    pub addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TcpEchoBackend {
    pub fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind echo backend");
        let addr = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        listener.set_nonblocking(true).unwrap();

        let handle = thread::spawn(move || {
            while !shutdown_clone.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let shutdown = shutdown_clone.clone();
                        thread::spawn(move || {
                            handle_echo_connection(stream, &shutdown);
                        });
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self { addr, shutdown, handle: Some(handle) }
    }
}

fn handle_echo_connection(mut stream: TcpStream, shutdown: &AtomicBool) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let mut buf = [0u8; 8192];
    loop {
        if shutdown.load(Ordering::Relaxed) { return; }
        let n = match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(_) => return,
        };
        if stream.write_all(&buf[..n]).is_err() { return; }
    }
}

impl Drop for TcpEchoBackend {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Send raw TCP data and read back the response.
pub fn tcp_roundtrip(addr: SocketAddr, data: &[u8], timeout: Duration) -> std::io::Result<Vec<u8>> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(data)?;

    let mut response = Vec::with_capacity(data.len());
    let mut buf = [0u8; 8192];
    while response.len() < data.len() {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut => break,
            Err(e) => return Err(e),
        }
    }
    Ok(response)
}

/// Handle to a running UringRess subprocess. Kills on drop.
pub struct UringRessProcess {
    child: Child,
    _config_file: tempfile::NamedTempFile,
    pub proxy_addr: SocketAddr,
}

impl UringRessProcess {
    /// Build a test config and spawn UringRess.
    /// `routes` maps (hostname, path_prefix) → backend SocketAddr.
    pub fn spawn(routes: &[(&str, &str, SocketAddr)], proxy_port: u16) -> Self {
        Self::spawn_with_tcp(routes, proxy_port, &[])
    }

    /// Build a test config with both HTTP and TCP routes, then spawn UringRess.
    /// `tcp_routes` maps (listener_name, proxy_port) → backend SocketAddr.
    pub fn spawn_with_tcp(
        routes: &[(&str, &str, SocketAddr)],
        proxy_port: u16,
        tcp_routes: &[(&str, u16, SocketAddr)],
    ) -> Self {
        let config = build_test_config_with_tcp(routes, proxy_port, tcp_routes);
        let config_file = tempfile::Builder::new()
            .suffix(".json")
            .tempfile()
            .expect("create temp config");
        std::fs::write(config_file.path(), config).expect("write temp config");

        let binary = cargo_bin_path();
        let mut child = Command::new(&binary)
            .arg("--config")
            .arg(config_file.path())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("Failed to spawn {:?}: {}", binary, e));

        let proxy_addr: SocketAddr = format!("127.0.0.1:{}", proxy_port).parse().unwrap();

        // Collect all ports to wait on (HTTP + TCP)
        let mut wait_addrs = vec![proxy_addr];
        for (_, tcp_port, _) in tcp_routes {
            let addr: SocketAddr = format!("127.0.0.1:{}", tcp_port).parse().unwrap();
            wait_addrs.push(addr);
        }

        // Wait for proxy to start accepting connections on all ports (up to 5s)
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        for wait_addr in &wait_addrs {
            loop {
                if std::time::Instant::now() > deadline {
                    if let Ok(Some(status)) = child.try_wait() {
                        let mut stderr = String::new();
                        if let Some(ref mut err) = child.stderr {
                            let _ = err.read_to_string(&mut stderr);
                        }
                        panic!(
                            "UringRess exited with {} before accepting connections.\nstderr: {}",
                            status, stderr
                        );
                    }
                    panic!("UringRess did not start accepting connections on {} within 5s", wait_addr);
                }
                if TcpStream::connect_timeout(wait_addr, Duration::from_millis(100)).is_ok() {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
        }

        Self {
            child,
            _config_file: config_file,
            proxy_addr,
        }
    }
}

impl Drop for UringRessProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Send a raw HTTP request and return the full response bytes.
pub fn http_request(addr: SocketAddr, request: &[u8]) -> std::io::Result<Vec<u8>> {
    http_request_timeout(addr, request, Duration::from_secs(5))
}

/// Send a raw HTTP request with a custom timeout.
pub fn http_request_timeout(
    addr: SocketAddr,
    request: &[u8],
    timeout: Duration,
) -> std::io::Result<Vec<u8>> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(request)?;

    let mut response = Vec::with_capacity(4096);
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // If we already have a complete HTTP response, stop
                if response_is_complete(&response) {
                    break;
                }
                // Otherwise this is a real timeout
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "read timed out waiting for complete response",
                ));
            }
            Err(e) => return Err(e),
        }
    }

    Ok(response)
}

/// Send multiple HTTP requests on the same TCP connection.
pub fn http_request_keepalive(
    addr: SocketAddr,
    requests: &[&[u8]],
) -> std::io::Result<Vec<Vec<u8>>> {
    let timeout = Duration::from_secs(5);
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let mut responses = Vec::new();

    for request in requests {
        stream.write_all(request)?;

        let mut response = Vec::with_capacity(4096);
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    response.extend_from_slice(&buf[..n]);
                    if response_is_complete(&response) {
                        break;
                    }
                }
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    if response_is_complete(&response) {
                        break;
                    }
                    return Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
                }
                Err(e) => return Err(e),
            }
        }
        responses.push(response);
    }

    Ok(responses)
}

/// Check if a raw HTTP response contains complete headers + content-length body.
fn response_is_complete(data: &[u8]) -> bool {
    let header_end = match data.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(pos) => pos + 4,
        None => return false,
    };

    let headers = std::str::from_utf8(&data[..header_end]).unwrap_or("");
    if let Some(cl_line) = headers.lines().find(|l| l.to_lowercase().starts_with("content-length:")) {
        if let Ok(cl) = cl_line.split(':').nth(1).unwrap_or("").trim().parse::<usize>() {
            return data.len() >= header_end + cl;
        }
    }

    // No content-length — assume complete if we have headers
    true
}

/// Extract the response body from a raw HTTP response.
pub fn extract_body(response: &[u8]) -> &[u8] {
    match response.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(pos) => &response[pos + 4..],
        None => &[],
    }
}

/// Extract the HTTP status code from a raw response.
pub fn extract_status(response: &[u8]) -> Option<u16> {
    let first_line = response.split(|&b| b == b'\r').next()?;
    let status_str = std::str::from_utf8(first_line).ok()?;
    // "HTTP/1.1 200 OK" → "200"
    status_str.split_whitespace().nth(1)?.parse().ok()
}

fn build_test_config_with_tcp(
    routes: &[(&str, &str, SocketAddr)],
    proxy_port: u16,
    tcp_routes: &[(&str, u16, SocketAddr)],
) -> String {
    let mut backend_refs = Vec::new();
    let mut hostnames: Vec<String> = Vec::new();

    for (hostname, path, backend_addr) in routes {
        if !hostnames.contains(&hostname.to_string()) {
            hostnames.push(hostname.to_string());
        }
        backend_refs.push(format!(
            r#"{{
                "matches": [{{"path": {{"type": "PathPrefix", "value": "{}"}}}}],
                "backend_refs": [{{"name": "{}", "port": {}}}]
            }}"#,
            path,
            backend_addr.ip(),
            backend_addr.port()
        ));
    }

    let hostnames_json: Vec<String> = hostnames.iter().map(|h| format!("\"{}\"", h)).collect();

    // Build TCP listener entries and route entries
    let mut tcp_listener_entries = Vec::new();
    let mut tcp_route_entries = Vec::new();
    for (name, port, backend_addr) in tcp_routes {
        tcp_listener_entries.push(format!(
            r#"{{"name": "{}", "protocol": "TCP", "port": {}}}"#,
            name, port
        ));
        tcp_route_entries.push(format!(
            r#"{{
      "parent_refs": [{{"name": "test-gateway", "section_name": "{}"}}],
      "rules": [{{"backend_refs": [{{"name": "{}", "port": {}}}]}}]
    }}"#,
            name, backend_addr.ip(), backend_addr.port()
        ));
    }

    let mut listeners = format!(
        r#"{{"name": "http", "protocol": "HTTP", "port": {}}}"#,
        proxy_port
    );
    for entry in &tcp_listener_entries {
        listeners.push_str(", ");
        listeners.push_str(entry);
    }

    let http_routes_section = if routes.is_empty() {
        "[]".to_string()
    } else {
        format!(
            r#"[{{
      "parent_refs": [{{"name": "test-gateway", "section_name": "http"}}],
      "hostnames": [{}],
      "rules": [{}]
    }}]"#,
            hostnames_json.join(", "),
            backend_refs.join(", ")
        )
    };

    format!(
        r#"{{
  "gateway": {{
    "name": "test-gateway",
    "listeners": [{}],
    "worker_threads": 1
  }},
  "http_routes": {},
  "tcp_routes": [{}],
  "observability": {{
    "logging": {{"level": "debug", "format": "pretty", "output": "stderr"}}
  }},
  "performance": {{
    "keep_alive_timeout": "5s"
  }}
}}"#,
        listeners,
        http_routes_section,
        tcp_route_entries.join(", ")
    )
}

fn cargo_bin_path() -> PathBuf {
    // Look for the binary in target/release or target/debug
    let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let release_bin = project_root.join("target/release/uringress");
    if release_bin.exists() {
        return release_bin;
    }
    let debug_bin = project_root.join("target/debug/uringress");
    if debug_bin.exists() {
        return debug_bin;
    }
    panic!(
        "UringRess binary not found. Build with `cargo build --release` first.\n\
         Looked in: {:?} and {:?}",
        release_bin, debug_bin
    );
}
