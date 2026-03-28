//! Proxy throughput benchmark — measures requests/sec through portail.
//!
//! Uses the same test infrastructure as integration tests:
//! spawns a real portail process + mock backend, sends N requests
//! on keepalive connections, and measures elapsed time.
//!
//! Run with: cargo bench --bench proxy_throughput

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const WARMUP_REQUESTS: usize = 1_000;
const BENCHMARK_REQUESTS: usize = 50_000;
const NUM_CONNECTIONS: usize = 8;
const PROXY_PORT: u16 = 29_100;
const FILTER_PROXY_PORT: u16 = 29_200;

fn main() {
    println!("=== Portail Proxy Throughput Benchmark ===\n");

    // --- Scenario 1: No-filter path (hot path) ---
    {
        let backend = spawn_backend("benchmark-ok");
        let proxy = spawn_portail_no_filters(backend.addr, PROXY_PORT);

        // Warmup
        run_keepalive_requests(proxy.addr, WARMUP_REQUESTS, 1);

        // Benchmark: single connection (serial latency)
        let (reqs, elapsed) = run_keepalive_requests(proxy.addr, BENCHMARK_REQUESTS, 1);
        let rps = reqs as f64 / elapsed.as_secs_f64();
        let avg_us = elapsed.as_micros() as f64 / reqs as f64;
        println!(
            "No-filter (1 conn):  {:>8} req/s  avg={:.0}µs  ({} reqs in {:.2}s)",
            rps as u64,
            avg_us,
            reqs,
            elapsed.as_secs_f64()
        );

        // Benchmark: parallel connections (throughput)
        let (reqs, elapsed) =
            run_keepalive_requests(proxy.addr, BENCHMARK_REQUESTS, NUM_CONNECTIONS);
        let rps = reqs as f64 / elapsed.as_secs_f64();
        let avg_us = elapsed.as_micros() as f64 / reqs as f64;
        println!(
            "No-filter ({} conn): {:>8} req/s  avg={:.0}µs  ({} reqs in {:.2}s)",
            NUM_CONNECTIONS,
            rps as u64,
            avg_us,
            reqs,
            elapsed.as_secs_f64()
        );
    }

    println!();

    // --- Scenario 2: Filter path (header mods + URL rewrite) ---
    {
        let backend = spawn_backend("filtered-ok");
        let proxy = spawn_portail_with_filters(backend.addr, FILTER_PROXY_PORT);

        // Warmup
        run_keepalive_requests(proxy.addr, WARMUP_REQUESTS, 1);

        // Benchmark: single connection
        let (reqs, elapsed) = run_keepalive_requests(proxy.addr, BENCHMARK_REQUESTS, 1);
        let rps = reqs as f64 / elapsed.as_secs_f64();
        let avg_us = elapsed.as_micros() as f64 / reqs as f64;
        println!(
            "Filter path (1 conn):  {:>8} req/s  avg={:.0}µs  ({} reqs in {:.2}s)",
            rps as u64,
            avg_us,
            reqs,
            elapsed.as_secs_f64()
        );

        // Benchmark: parallel connections
        let (reqs, elapsed) =
            run_keepalive_requests(proxy.addr, BENCHMARK_REQUESTS, NUM_CONNECTIONS);
        let rps = reqs as f64 / elapsed.as_secs_f64();
        let avg_us = elapsed.as_micros() as f64 / reqs as f64;
        println!(
            "Filter path ({} conn): {:>8} req/s  avg={:.0}µs  ({} reqs in {:.2}s)",
            NUM_CONNECTIONS,
            rps as u64,
            avg_us,
            reqs,
            elapsed.as_secs_f64()
        );
    }

    println!("\n=== Benchmark complete ===");
}

/// Run N requests spread across `num_connections` keepalive connections.
/// Returns (total_successful_requests, total_elapsed).
fn run_keepalive_requests(
    proxy_addr: SocketAddr,
    total_requests: usize,
    num_connections: usize,
) -> (usize, Duration) {
    let per_conn = total_requests / num_connections;
    let start = Instant::now();

    let handles: Vec<_> = (0..num_connections)
        .map(|_| thread::spawn(move || keepalive_burst(proxy_addr, per_conn)))
        .collect();

    let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    let elapsed = start.elapsed();
    (total, elapsed)
}

/// Send `count` HTTP/1.1 keepalive requests on a single TCP connection.
/// Returns number of successful request/response cycles.
fn keepalive_burst(addr: SocketAddr, count: usize) -> usize {
    let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(10))).ok();

    let request = b"GET /bench HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let mut read_buf = [0u8; 4096];
    let mut successes = 0;

    for _ in 0..count {
        if stream.write_all(request).is_err() {
            break;
        }

        // Read until we have a complete response
        let mut response = Vec::with_capacity(256);
        loop {
            match stream.read(&mut read_buf) {
                Ok(0) => return successes,
                Ok(n) => {
                    response.extend_from_slice(&read_buf[..n]);
                    if response_complete(&response) {
                        break;
                    }
                }
                Err(_) => return successes,
            }
        }

        if response.starts_with(b"HTTP/1.1 200") {
            successes += 1;
        }
    }

    successes
}

fn response_complete(data: &[u8]) -> bool {
    let header_end = match data.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(pos) => pos + 4,
        None => return false,
    };
    let headers = std::str::from_utf8(&data[..header_end]).unwrap_or("");
    if let Some(cl_line) = headers
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-length:"))
    {
        if let Ok(cl) = cl_line
            .split(':')
            .nth(1)
            .unwrap_or("")
            .trim()
            .parse::<usize>()
        {
            return data.len() >= header_end + cl;
        }
    }
    true
}

// --- Infrastructure ---

struct BackendHandle {
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    _handle: thread::JoinHandle<()>,
}

impl Drop for BackendHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
    }
}

fn spawn_backend(body: &str) -> BackendHandle {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind backend");
    let addr = listener.local_addr().unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    let body = body.to_string();

    listener.set_nonblocking(true).unwrap();

    let handle = thread::spawn(move || {
        while !shutdown_clone.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let body = body.clone();
                    let sd = shutdown_clone.clone();
                    thread::spawn(move || backend_handler(stream, &body, &sd));
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(_) => break,
            }
        }
    });

    BackendHandle {
        addr,
        shutdown,
        _handle: handle,
    }
}

fn backend_handler(mut stream: TcpStream, body: &str, shutdown: &AtomicBool) {
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
    stream.set_nodelay(true).ok();
    let mut buf = [0u8; 4096];
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
        body.len(),
        body
    );
    let response_bytes = response.as_bytes();

    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        let n = match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(_) => return,
        };
        if !buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
            continue;
        }
        if stream.write_all(response_bytes).is_err() {
            return;
        }
    }
}

struct ProxyHandle {
    addr: SocketAddr,
    child: Child,
    _config: tempfile::NamedTempFile,
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_portail_no_filters(backend_addr: SocketAddr, port: u16) -> ProxyHandle {
    let config = format!(
        r#"{{
  "gateway": {{"name":"bench","listeners":[{{"name":"http","protocol":"HTTP","port":{}}}],"workerThreads":2}},
  "httpRoutes": [{{"parentRefs":[{{"name":"bench","sectionName":"http"}}],"hostnames":["localhost"],"rules":[{{"matches":[{{"path":{{"type":"PathPrefix","value":"/"}}}}],"backendRefs":[{{"name":"{}","port":{}}}]}}]}}],
  "observability": {{"logging":{{"level":"error","format":"pretty","output":"stderr"}}}},
  "performance": {{}}
}}"#,
        port,
        backend_addr.ip(),
        backend_addr.port()
    );

    spawn_portail_with_config(&config, port)
}

fn spawn_portail_with_filters(backend_addr: SocketAddr, port: u16) -> ProxyHandle {
    let config = format!(
        r#"{{
  "gateway": {{"name":"bench","listeners":[{{"name":"http","protocol":"HTTP","port":{}}}],"workerThreads":2}},
  "httpRoutes": [{{"parentRefs":[{{"name":"bench","sectionName":"http"}}],"hostnames":["localhost"],"rules":[{{
    "matches":[{{"path":{{"type":"PathPrefix","value":"/"}}}}],
    "filters":[
      {{"type":"RequestHeaderModifier","requestHeaderModifier":{{"add":[{{"name":"X-Bench","value":"true"}}],"remove":["Accept"]}}}},
      {{"type":"URLRewrite","urlRewrite":{{"hostname":"rewritten.bench"}}}}
    ],
    "backendRefs":[{{"name":"{}","port":{}}}]
  }}]}}],
  "observability": {{"logging":{{"level":"error","format":"pretty","output":"stderr"}}}},
  "performance": {{}}
}}"#,
        port,
        backend_addr.ip(),
        backend_addr.port()
    );

    spawn_portail_with_config(&config, port)
}

fn spawn_portail_with_config(config: &str, port: u16) -> ProxyHandle {
    let config_file = tempfile::Builder::new()
        .suffix(".json")
        .tempfile()
        .expect("temp");
    std::fs::write(config_file.path(), config).expect("write config");

    let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let binary = project_root.join("target/release/portail");
    let bin = if binary.exists() {
        binary
    } else {
        project_root.join("target/debug/portail")
    };

    let child = Command::new(&bin)
        .arg("--config")
        .arg(config_file.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {:?}: {}", bin, e));

    let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if Instant::now() > deadline {
            panic!("portail didn't start on port {}", port);
        }
        if TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }

    ProxyHandle {
        addr,
        child,
        _config: config_file,
    }
}
