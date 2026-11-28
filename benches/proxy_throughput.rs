//! Proxy throughput benchmark using rewrk-core + kiss backend.
//!
//! Spawns kiss (static file server) as backend, portail as proxy,
//! and uses rewrk-core as the load generator. The churn and TLS scenarios
//! bring their own clients: rewrk-core keeps its connections and speaks
//! neither TLS to a private certificate nor multiplexed HTTP/2.
//!
//! Prerequisites:
//!   - `kiss` binary in PATH (cargo install kiss)
//!   - `portail` binary built (cargo build --release)
//!
//! Run with: cargo bench --bench proxy_throughput
//!
//! Environment:
//!   - `PT_ONLY`: comma-separated scenarios to run, all by default: direct
//!     nf1 nf1_log nf4 f1 f4 pool4 churn_direct churn_conn churn_proc tls_h1
//!     h2_64x1 h2_8x8 h2_1x64 bulk_direct bulk_tcp bulk_upgrade echo_tcp
//!   - `PT_SECS`: seconds per scenario, 10 by default
//!   - `PORTAIL_BIN`: the portail binary under test, else the one built here
//!     (target/release, then target/debug)
//!   - `PT_PROXY_CPUS`, `PT_BACKEND_CPUS`: the CPUs the proxy and the
//!     backends are confined to, as a list such as `0-3,8`. The load
//!     generator runs on the bench's own: start it under `taskset`.
//!
//! Each proxy scenario prints the proxy's CPU time per request, or per GiB
//! relayed. Compare runs by that figure: req/s and Gbit/s also measure the
//! load generator, and on shared cores all three follow the scheduler.

use std::ffi::OsStr;
use std::future::Future;
use std::net::{SocketAddr, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use hyper_014::{Body, Method, Request, Uri};
use rewrk_core::{
    Batch, HttpProtocol, Producer, ReWrkBenchmark, RequestBatch, Sample, SampleCollector,
};
use rustls::pki_types::CertificateDer;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const CONCURRENCY: usize = 64;
const NUM_WORKERS: usize = 4;
const REQUESTS_PER_BATCH: usize = 500;
const DEFAULT_BENCH_SECS: u64 = 10;

const PORTAIL_PORT: u16 = 29_100;
const PORTAIL_FILTER_PORT: u16 = 29_200;
const KISS_BASE_PORT: u16 = 29_300;
const PORTAIL_TLS_PORT: u16 = 29_400;
const PORTAIL_TCP_PORT: u16 = 29_500;
const SOURCE_PORT: u16 = 29_600;
const NUM_BACKENDS: u16 = 4;

/// Connections a bulk scenario downloads on at once.
const BULK_CONNS: usize = 8;
/// What the source writes and a bulk client reads at a time: large enough
/// that neither end caps what the relay between them can move.
const BULK_CHUNK: usize = 256 * 1024;

/// Connections of the echo scenario, one message in flight on each.
const ECHO_CONNS: usize = 64;
/// A chat frame: far below any relay buffer, where the bulk ones fill it.
const ECHO_MESSAGE: usize = 128;

/// The HTTPS listener's hostname: its certificate, the SNI and its route.
const TLS_HOST: &str = "bench.test";

/// What a TLS scenario keeps in flight.
#[derive(Clone, Copy)]
enum TlsLoad {
    /// One request per connection.
    H1 { conns: usize },
    /// `streams` requests per connection.
    H2 { conns: usize, streams: usize },
}

/// Each keeps `CONCURRENCY` requests in flight through one HTTPS listener,
/// so the rows differ by protocol and multiplexing alone.
const TLS_SCENARIOS: &[(&str, &str, TlsLoad)] = &[
    (
        "tls_h1",
        "TLS, HTTP/1.1 (64 connections)",
        TlsLoad::H1 { conns: CONCURRENCY },
    ),
    (
        "h2_64x1",
        "TLS, HTTP/2 (64 connections, 1 stream each)",
        TlsLoad::H2 {
            conns: CONCURRENCY,
            streams: 1,
        },
    ),
    (
        "h2_8x8",
        "TLS, HTTP/2 (8 connections, 8 streams each)",
        TlsLoad::H2 {
            conns: 8,
            streams: 8,
        },
    ),
    (
        "h2_1x64",
        "TLS, HTTP/2 (1 connection, 64 streams)",
        TlsLoad::H2 {
            conns: 1,
            streams: CONCURRENCY,
        },
    ),
];

fn main() {
    // rustls picks no provider on its own while the tree links two.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        println!("=== Portail Proxy Throughput Benchmark ===\n");

        let content_dir = PathBuf::from("/tmp/portail_bench_content");
        std::fs::create_dir_all(&content_dir).expect("create bench content dir");
        std::fs::write(content_dir.join("bench.txt"), "ok").expect("write bench file");

        // Start kiss backends
        let kiss_handles: Vec<ProcessHandle> = (0..NUM_BACKENDS)
            .map(|i| spawn_kiss(KISS_BASE_PORT + i, &content_dir))
            .collect();
        let backend_ports: Vec<u16> = (0..NUM_BACKENDS).map(|i| KISS_BASE_PORT + i).collect();

        // Baseline: direct to kiss (no proxy)
        if selected("direct") {
            println!("--- Direct to kiss (baseline, no proxy) ---");
            run_bench(KISS_BASE_PORT, "/bench.txt").await;
        }

        println!();

        // Scenario 1: No filters, single backend
        if selected("nf1") {
            let proxy = spawn_portail_no_filters(&backend_ports[..1], PORTAIL_PORT);
            println!("--- No-filter path (1 backend) ---");
            proxy.measure(run_bench(PORTAIL_PORT, "/bench.txt")).await;
            drop(proxy);
        }

        println!();

        // Scenario 1 with the access log on, to stdout as a Managed data
        // plane writes it: what a line costs, its sink /dev/null.
        if selected("nf1_log") {
            let proxy = spawn_portail_access_log(&backend_ports[..1], PORTAIL_PORT);
            println!("--- No-filter path (1 backend), access log to stdout ---");
            proxy.measure(run_bench(PORTAIL_PORT, "/bench.txt")).await;
            drop(proxy);
        }

        println!();

        // Scenario 2: No filters, multiple backends
        if selected("nf4") {
            let proxy = spawn_portail_no_filters(&backend_ports, PORTAIL_PORT);
            println!("--- No-filter path ({} backends) ---", NUM_BACKENDS);
            proxy.measure(run_bench(PORTAIL_PORT, "/bench.txt")).await;
            drop(proxy);
        }

        println!();

        // Scenario 3: With filters, single backend
        if selected("f1") {
            let proxy = spawn_portail_with_filters(&backend_ports[..1], PORTAIL_FILTER_PORT);
            println!("--- Filter path (1 backend) ---");
            proxy
                .measure(run_bench(PORTAIL_FILTER_PORT, "/bench.txt"))
                .await;
            drop(proxy);
        }

        println!();

        // Scenario 4: With filters, multiple backends
        if selected("f4") {
            let proxy = spawn_portail_with_filters(&backend_ports, PORTAIL_FILTER_PORT);
            println!("--- Filter path ({} backends) ---", NUM_BACKENDS);
            proxy
                .measure(run_bench(PORTAIL_FILTER_PORT, "/bench.txt"))
                .await;
            drop(proxy);
        }

        println!();

        // Scenario 5: keepalive load with the process-wide pool — the
        // contention guard. Compare against scenario 2: same load, pool
        // scope is the only variable.
        if selected("pool4") {
            let proxy = spawn_portail_pool(&backend_ports, PORTAIL_PORT, "process");
            println!(
                "--- No-filter path ({} backends), process-wide pool ---",
                NUM_BACKENDS
            );
            proxy.measure(run_bench(PORTAIL_PORT, "/bench.txt")).await;
            drop(proxy);
        }

        println!();

        // Churn scenarios: one request per connection, then close. rewrk-core
        // holds keepalive client connections — the best case for the
        // per-connection pool — so pool-scope differences only show here:
        // with churn, a per-connection pool gets zero backend reuse (every
        // request pays a fresh backend connect), while the process-wide pool
        // still reuses idle backend conns across client connections.
        if selected("churn_direct") {
            println!("--- Churn: direct to kiss (baseline) ---");
            run_churn_bench(KISS_BASE_PORT, "/bench.txt").await;
        }

        println!();

        if selected("churn_conn") {
            let proxy = spawn_portail_pool(&backend_ports, PORTAIL_PORT, "connection");
            println!(
                "--- Churn: proxy, per-connection pool ({} backends) ---",
                NUM_BACKENDS
            );
            proxy
                .measure(run_churn_bench(PORTAIL_PORT, "/bench.txt"))
                .await;
            drop(proxy);
        }

        println!();

        if selected("churn_proc") {
            let proxy = spawn_portail_pool(&backend_ports, PORTAIL_PORT, "process");
            println!(
                "--- Churn: proxy, process-wide pool ({} backends) ---",
                NUM_BACKENDS
            );
            proxy
                .measure(run_churn_bench(PORTAIL_PORT, "/bench.txt"))
                .await;
            drop(proxy);
        }

        for &(scenario, title, load) in TLS_SCENARIOS {
            if selected(scenario) {
                println!();
                let (proxy, cert) = spawn_portail_tls(&backend_ports[..1], PORTAIL_TLS_PORT);
                println!("--- {} ---", title);
                proxy
                    .measure(run_tls_bench(PORTAIL_TLS_PORT, "/bench.txt", cert, load))
                    .await;
                drop(proxy);
            }
        }

        // Bulk scenarios: bytes downloaded through the L4 relay, where the
        // HTTP ones count requests. The source is this process: its cost
        // falls on the load generator's CPUs, not on the proxy's.
        // A port each: an aborted source lets go of its own a moment later.
        if selected("bulk_direct") {
            println!();
            let source = spawn_source(SOURCE_PORT, false).await;
            println!("--- Bulk: direct from the source (baseline) ---");
            run_bulk_bench(SOURCE_PORT, false).await;
            source.abort();
        }

        if selected("bulk_tcp") {
            println!();
            let source = spawn_source(SOURCE_PORT + 1, false).await;
            let proxy = spawn_portail_tcp(SOURCE_PORT + 1, PORTAIL_TCP_PORT);
            println!("--- Bulk: TCP route ---");
            proxy
                .measure_bulk(run_bulk_bench(PORTAIL_TCP_PORT, false))
                .await;
            drop(proxy);
            source.abort();
        }

        if selected("bulk_upgrade") {
            println!();
            let source = spawn_source(SOURCE_PORT + 2, true).await;
            let proxy = spawn_portail_no_filters(&[SOURCE_PORT + 2], PORTAIL_PORT);
            println!("--- Bulk: HTTP upgrade tunnel ---");
            proxy.measure_bulk(run_bulk_bench(PORTAIL_PORT, true)).await;
            drop(proxy);
            source.abort();
        }

        // The relay again, by the message: what a chat or an RPC stream
        // pays, where the bulk scenarios measure it by the byte.
        if selected("echo_tcp") {
            println!();
            let echo = spawn_echo(SOURCE_PORT + 3).await;
            let proxy = spawn_portail_tcp(SOURCE_PORT + 3, PORTAIL_TCP_PORT + 1);
            println!(
                "--- Echo: TCP route ({} connections, {}-byte messages) ---",
                ECHO_CONNS, ECHO_MESSAGE
            );
            proxy.measure(run_echo_bench(PORTAIL_TCP_PORT + 1)).await;
            drop(proxy);
            echo.abort();
        }

        drop(kiss_handles);
        println!("\n=== Benchmark complete ===");
    });
}

/// Returns the requests answered, as every load does: what a proxy's CPU
/// time is divided by.
async fn run_bench(port: u16, path: &str) -> u64 {
    let uri = Uri::builder()
        .scheme("http")
        .authority(format!("127.0.0.1:{}", port))
        .path_and_query(path)
        .build()
        .unwrap();

    let producer = BenchProducer::new(path, bench_duration());
    let collector = BenchCollector::default();

    let mut benchmarker =
        ReWrkBenchmark::create(uri, CONCURRENCY, HttpProtocol::HTTP1, producer, collector)
            .await
            .unwrap();
    benchmarker.set_num_workers(NUM_WORKERS);
    benchmarker.set_sample_window(Duration::from_secs(15));

    let wall_start = Instant::now();
    benchmarker.run().await;
    let wall_elapsed = wall_start.elapsed();

    let collector = benchmarker.consume_collector().await;
    collector.print_results(wall_elapsed)
}

/// Whether `PT_ONLY` leaves `scenario` in.
fn selected(scenario: &str) -> bool {
    std::env::var("PT_ONLY").map_or(true, |only| only.split(',').any(|s| s == scenario))
}

/// How long each scenario runs: `PT_SECS`, else `DEFAULT_BENCH_SECS`.
fn bench_duration() -> Duration {
    let secs = std::env::var("PT_SECS").ok().and_then(|s| s.parse().ok());
    Duration::from_secs(secs.unwrap_or(DEFAULT_BENCH_SECS))
}

/// Connection-churn load: N tasks each looping connect → one GET → read the
/// full response (Content-Length-framed) → close silently, for
/// `bench_duration()`. Hand-rolled because rewrk-core keeps its client
/// connections alive.
///
/// Deliberately does NOT send `Connection: close`: the proxy forwards the
/// client's header block to the backend, so an announced close makes the
/// BACKEND close after every response too — no backend conn survives to be
/// pooled, and pool scope can't matter. A silent close after the response is
/// the churn shape cross-client pooling actually targets.
async fn run_churn_bench(port: u16, path: &str) -> u64 {
    const CHURN_CONCURRENCY: usize = 64;
    let deadline = Instant::now() + bench_duration();
    let request = Arc::new(format!("GET {} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n", path));

    async fn one(port: u16, request: &str) -> Result<()> {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
        s.set_nodelay(true)?;
        // The drop that follows closes the client conn without announcing it.
        exchange(&mut s, request.as_bytes(), &mut Vec::with_capacity(1024)).await
    }

    let wall_start = Instant::now();
    let mut handles = Vec::with_capacity(CHURN_CONCURRENCY);
    for _ in 0..CHURN_CONCURRENCY {
        let request = request.clone();
        handles.push(tokio::spawn(async move {
            let (mut ok, mut errs) = (0u64, 0u64);
            while Instant::now() < deadline {
                match tokio::time::timeout(Duration::from_secs(5), one(port, &request)).await {
                    Ok(Ok(())) => ok += 1,
                    _ => errs += 1,
                }
            }
            (ok, errs)
        }));
    }

    let (mut ok, mut errs) = (0u64, 0u64);
    for h in handles {
        let (o, e) = h.await.unwrap();
        ok += o;
        errs += e;
    }
    let wall = wall_start.elapsed();
    println!(
        "  Requests:    {} ({:.0} conn+req/s)",
        ok,
        ok as f64 / wall.as_secs_f64()
    );
    println!("  Duration:    {:.2}s", wall.as_secs_f64());
    if errs > 0 {
        println!("  Errors:      {}", errs);
    }
    ok
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn content_length(headers: &[u8]) -> Option<usize> {
    for line in headers.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.len() > 15 && line[..15].eq_ignore_ascii_case(b"content-length:") {
            return std::str::from_utf8(&line[15..]).ok()?.trim().parse().ok();
        }
    }
    None
}

/// One HTTP/1.1 exchange on `s`: send `request`, read the whole response,
/// a 200 framed by Content-Length. An error answered fast must not count
/// as throughput. `buf` is scratch, reused from one exchange to the next.
async fn exchange<S>(s: &mut S, request: &[u8], buf: &mut Vec<u8>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    s.write_all(request).await?;
    buf.clear();
    let mut tmp = [0u8; 4096];
    let total = loop {
        let n = s.read(&mut tmp).await?;
        anyhow::ensure!(n > 0, "connection closed mid-response");
        buf.extend_from_slice(&tmp[..n]);
        if let Some(header_end) = find_subslice(buf, b"\r\n\r\n") {
            anyhow::ensure!(buf.starts_with(b"HTTP/1.1 200 "), "not a 200");
            let cl = content_length(&buf[..header_end])
                .ok_or_else(|| anyhow::anyhow!("no Content-Length"))?;
            break header_end + 4 + cl;
        }
    };
    while buf.len() < total {
        let n = s.read(&mut tmp).await?;
        anyhow::ensure!(n > 0, "connection closed mid-body");
        buf.extend_from_slice(&tmp[..n]);
    }
    Ok(())
}

// --- TLS load: HTTP/1.1 and HTTP/2 through one HTTPS listener ---

type TlsStream = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;

/// What one closed loop measured: each request's latency in microseconds,
/// and whether an error ended it early.
type LoopResult = (Vec<u32>, u64);

/// Closed-loop load over TLS for `bench_duration()`. Every connection is
/// open before the clock starts: latency and req/s hold no handshake. The
/// proxy's CPU window opens earlier and holds them all, one per connection.
async fn run_tls_bench(
    port: u16,
    path: &'static str,
    cert: CertificateDer<'static>,
    load: TlsLoad,
) -> u64 {
    let mut loops = Vec::new();
    let wall_start;
    match load {
        TlsLoad::H1 { conns } => {
            let client = tls_client(cert, b"http/1.1");
            let mut open = Vec::with_capacity(conns);
            for _ in 0..conns {
                open.push(tls_connect(&client, port).await);
            }
            wall_start = Instant::now();
            let deadline = wall_start + bench_duration();
            for tls in open {
                loops.push(tokio::spawn(h1_loop(tls, path, deadline)));
            }
        }
        TlsLoad::H2 { conns, streams } => {
            let client = tls_client(cert, b"h2");
            let mut open = Vec::with_capacity(conns);
            for _ in 0..conns {
                let tls = tls_connect(&client, port).await;
                let (send, connection) = h2::client::handshake(tls).await.expect("h2 handshake");
                tokio::spawn(connection);
                open.push(send);
            }
            wall_start = Instant::now();
            let deadline = wall_start + bench_duration();
            for send in open {
                for _ in 0..streams {
                    loops.push(tokio::spawn(h2_loop(send.clone(), path, deadline)));
                }
            }
        }
    }

    report(loops, wall_start).await
}

/// Wait for a closed-loop load's tasks and print what they measured.
/// Returns the exchanges completed.
async fn report(loops: Vec<tokio::task::JoinHandle<LoopResult>>, wall_start: Instant) -> u64 {
    let (mut latencies, mut errors) = (Vec::new(), 0);
    for task in loops {
        let (measured, failed) = task.await.unwrap();
        latencies.extend(measured);
        errors += failed;
    }
    let wall = wall_start.elapsed();

    latencies.sort_unstable();
    let requests = latencies.len() as u64;
    let at = |quantile: f64| {
        let rank = (latencies.len() as f64 * quantile) as usize;
        latencies
            .get(rank)
            .or(latencies.last())
            .copied()
            .unwrap_or(0)
    };
    println!(
        "  Requests:    {} ({:.0} req/s)",
        requests,
        requests as f64 / wall.as_secs_f64()
    );
    println!("  Duration:    {:.2}s", wall.as_secs_f64());
    println!(
        "  Latency:     p50={}us  p95={}us  p99={}us  max={}us",
        at(0.50),
        at(0.95),
        at(0.99),
        at(1.0)
    );
    if errors > 0 {
        println!("  Errors:      {}", errors);
    }
    requests
}

/// A client that trusts the listener's own certificate and nothing else,
/// and offers `alpn` alone: the listener cannot pick the other protocol.
fn tls_client(cert: CertificateDer<'static>, alpn: &[u8]) -> tokio_rustls::TlsConnector {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert).expect("trust the bench certificate");
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![alpn.to_vec()];
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

async fn tls_connect(client: &tokio_rustls::TlsConnector, port: u16) -> TlsStream {
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect to the TLS listener");
    tcp.set_nodelay(true).expect("set TCP_NODELAY");
    let name = rustls::pki_types::ServerName::try_from(TLS_HOST).expect("server name");
    client.connect(name, tcp).await.expect("TLS handshake")
}

async fn h1_loop(mut tls: TlsStream, path: &str, deadline: Instant) -> LoopResult {
    let request = format!("GET {} HTTP/1.1\r\nHost: {}\r\n\r\n", path, TLS_HOST);
    let (mut latencies, mut buf) = (Vec::new(), Vec::with_capacity(1024));
    while Instant::now() < deadline {
        let sent = Instant::now();
        if exchange(&mut tls, request.as_bytes(), &mut buf)
            .await
            .is_err()
        {
            return (latencies, 1);
        }
        latencies.push(sent.elapsed().as_micros() as u32);
    }
    (latencies, 0)
}

async fn h2_loop(
    mut send: h2::client::SendRequest<bytes::Bytes>,
    path: &str,
    deadline: Instant,
) -> LoopResult {
    type Sender = h2::client::SendRequest<bytes::Bytes>;

    /// One GET on a stream of its own. Hands `send` back: waiting for a
    /// free stream consumes it.
    async fn one(send: Sender, uri: &str) -> Result<Sender> {
        let mut send = send.ready().await?;
        let request = http::Request::get(uri).body(())?;
        let (response, _) = send.send_request(request, true)?;
        let response = response.await?;
        anyhow::ensure!(response.status() == 200, "not a 200");
        let mut body = response.into_body();
        while let Some(chunk) = body.data().await {
            // Data read stays charged to the connection's window until
            // released: without this every stream stalls once 64 KiB of
            // bodies have been read.
            body.flow_control().release_capacity(chunk?.len())?;
        }
        Ok(send)
    }

    let uri = format!("https://{}{}", TLS_HOST, path);
    let mut latencies = Vec::new();
    while Instant::now() < deadline {
        let sent = Instant::now();
        send = match one(send, &uri).await {
            Ok(send) => send,
            Err(_) => return (latencies, 1),
        };
        latencies.push(sent.elapsed().as_micros() as u32);
    }
    (latencies, 0)
}

// --- Bulk load: bytes downloaded through the L4 relay ---

/// What an upgrade client asks and the source answers before the bytes.
const UPGRADE_REQUEST: &[u8] =
    b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: Upgrade\r\nUpgrade: bench\r\n\r\n";
const UPGRADE_RESPONSE: &[u8] =
    b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: bench\r\n\r\n";

/// A backend on this runtime that writes zeros to every connection until
/// its peer closes. With `upgrade`, it first reads an HTTP request and
/// answers 101, as the backend of an upgrade tunnel does.
async fn spawn_source(port: u16, upgrade: bool) -> tokio::task::JoinHandle<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind the bulk source");
    tokio::spawn(async move {
        loop {
            let Ok((mut conn, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                if upgrade {
                    let mut head = Vec::new();
                    while find_subslice(&head, b"\r\n\r\n").is_none() {
                        match conn.read_buf(&mut head).await {
                            Ok(n) if n > 0 => {}
                            _ => return,
                        }
                    }
                    if conn.write_all(UPGRADE_RESPONSE).await.is_err() {
                        return;
                    }
                }
                let zeros = vec![0u8; BULK_CHUNK];
                while conn.write_all(&zeros).await.is_ok() {}
            });
        }
    })
}

/// Download from `port` on `BULK_CONNS` connections for `bench_duration()`,
/// through an upgrade when `upgrade`. Returns the bytes read.
async fn run_bulk_bench(port: u16, upgrade: bool) -> u64 {
    async fn download(port: u16, upgrade: bool, deadline: Instant) -> Result<u64> {
        let mut conn = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
        let mut buf = Vec::with_capacity(BULK_CHUNK);
        let mut bytes = 0;
        if upgrade {
            conn.write_all(UPGRADE_REQUEST).await?;
            let head_end = loop {
                anyhow::ensure!(conn.read_buf(&mut buf).await? > 0, "closed before the 101");
                if let Some(end) = find_subslice(&buf, b"\r\n\r\n") {
                    break end + 4;
                }
            };
            anyhow::ensure!(buf.starts_with(b"HTTP/1.1 101 "), "not a 101");
            bytes = (buf.len() - head_end) as u64;
        }
        while Instant::now() < deadline {
            buf.clear();
            let read = conn.read_buf(&mut buf).await?;
            anyhow::ensure!(read > 0, "closed mid-download");
            bytes += read as u64;
        }
        Ok(bytes)
    }

    let wall_start = Instant::now();
    let deadline = wall_start + bench_duration();
    let downloads: Vec<_> = (0..BULK_CONNS)
        .map(|_| tokio::spawn(download(port, upgrade, deadline)))
        .collect();
    let (mut bytes, mut errors) = (0u64, 0u64);
    for download in downloads {
        match download.await.unwrap() {
            Ok(read) => bytes += read,
            Err(_) => errors += 1,
        }
    }
    let wall = wall_start.elapsed();
    println!(
        "  Transferred: {:.1} GiB ({:.1} Gbit/s)",
        bytes as f64 / GIB,
        bytes as f64 * 8.0 / 1e9 / wall.as_secs_f64()
    );
    println!("  Duration:    {:.2}s", wall.as_secs_f64());
    if errors > 0 {
        println!("  Errors:      {}", errors);
    }
    bytes
}

const GIB: f64 = (1u64 << 30) as f64;

// --- Echo load: small messages both ways through the L4 relay ---

/// A backend on this runtime that sends back what it reads.
async fn spawn_echo(port: u16) -> tokio::task::JoinHandle<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind the echo backend");
    tokio::spawn(async move {
        loop {
            let Ok((mut conn, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                // Nagle would hold each echo for the relay's delayed ACK.
                let _ = conn.set_nodelay(true);
                let mut buf = [0u8; ECHO_MESSAGE];
                while conn.read_exact(&mut buf).await.is_ok() {
                    if conn.write_all(&buf).await.is_err() {
                        return;
                    }
                }
            });
        }
    })
}

/// Send a message and read it back, on `ECHO_CONNS` connections through
/// `port` for `bench_duration()`. Returns the messages echoed.
async fn run_echo_bench(port: u16) -> u64 {
    async fn chat(mut conn: tokio::net::TcpStream, deadline: Instant) -> LoopResult {
        let (mut latencies, mut message) = (Vec::new(), [0x5a; ECHO_MESSAGE]);
        while Instant::now() < deadline {
            let sent = Instant::now();
            if conn.write_all(&message).await.is_err()
                || conn.read_exact(&mut message).await.is_err()
            {
                return (latencies, 1);
            }
            latencies.push(sent.elapsed().as_micros() as u32);
        }
        (latencies, 0)
    }

    let mut open = Vec::with_capacity(ECHO_CONNS);
    for _ in 0..ECHO_CONNS {
        let conn = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect to the TCP route");
        conn.set_nodelay(true).expect("TCP_NODELAY");
        open.push(conn);
    }
    let wall_start = Instant::now();
    let deadline = wall_start + bench_duration();
    let loops = open
        .into_iter()
        .map(|conn| tokio::spawn(chat(conn, deadline)))
        .collect();
    report(loops, wall_start).await
}

// --- Producer: generates request batches for a fixed duration ---

#[derive(Clone)]
struct BenchProducer {
    path: String,
    duration: Duration,
    started: Option<Instant>,
}

impl BenchProducer {
    fn new(path: &str, duration: Duration) -> Self {
        Self {
            path: path.to_string(),
            duration,
            started: None,
        }
    }
}

#[rewrk_core::async_trait]
impl Producer for BenchProducer {
    fn ready(&mut self) {
        self.started = Some(Instant::now());
    }

    async fn create_batch(&mut self) -> Result<RequestBatch> {
        let started = self.started.expect("ready() must be called first");
        if started.elapsed() >= self.duration {
            return Ok(RequestBatch::End);
        }

        let mut requests = Vec::with_capacity(REQUESTS_PER_BATCH);
        for _ in 0..REQUESTS_PER_BATCH {
            let uri = Uri::builder().path_and_query(&*self.path).build()?;
            let request = Request::builder()
                .method(Method::GET)
                .uri(uri)
                .body(Body::empty())?;
            requests.push(request);
        }

        Ok(RequestBatch::Batch(Batch { tag: 0, requests }))
    }
}

// --- Collector: aggregates samples and prints results ---

#[derive(Default)]
struct BenchCollector {
    samples: Vec<Sample>,
}

#[rewrk_core::async_trait]
impl SampleCollector for BenchCollector {
    async fn process_sample(&mut self, sample: Sample) -> Result<()> {
        self.samples.push(sample);
        Ok(())
    }
}

impl BenchCollector {
    fn print_results(&self, wall_time: Duration) -> u64 {
        if self.samples.is_empty() {
            println!("  No samples collected");
            return 0;
        }

        // Merge all samples
        let merged = self.samples.iter().cloned().reduce(|a, b| a + b).unwrap();

        let total_reqs = merged.total_successful_requests();
        let rps = total_reqs as f64 / wall_time.as_secs_f64();

        let lat = merged.latency();
        let p50 = lat.value_at_percentile(50.0);
        let p95 = lat.value_at_percentile(95.0);
        let p99 = lat.value_at_percentile(99.0);
        let max = lat.max();

        println!("  Requests:    {} ({:.0} req/s)", total_reqs, rps);
        println!("  Duration:    {:.2}s", wall_time.as_secs_f64());
        println!(
            "  Latency:     p50={:.0}us  p95={:.0}us  p99={:.0}us  max={:.0}us",
            p50, p95, p99, max
        );

        if !merged.errors().is_empty() {
            println!("  Errors:      {}", merged.errors().len());
            for e in merged.errors().iter().take(3) {
                println!("    - {:?}", e);
            }
        }
        total_reqs as u64
    }
}

// --- Infrastructure: spawn kiss + portail ---

struct ProcessHandle {
    child: Child,
    /// The certificates the process serves, removed once it is gone.
    _cert_dir: Option<tempfile::TempDir>,
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl ProcessHandle {
    /// Run `load`, then print what each of its requests cost this process.
    async fn measure(&self, load: impl Future<Output = u64>) {
        let (requests, (user, system)) = self.cpu_during(load).await;
        if requests == 0 {
            return;
        }
        let each = |seconds: f64| seconds * 1e6 / requests as f64;
        println!(
            "  Proxy CPU:   user={:.2}us  sys={:.2}us per request  (peak RSS {} MiB)",
            each(user),
            each(system),
            self.peak_rss_kib() / 1024
        );
    }

    /// Run `load`, then print what each GiB it moved cost this process.
    async fn measure_bulk(&self, load: impl Future<Output = u64>) {
        let (bytes, (user, system)) = self.cpu_during(load).await;
        if bytes == 0 {
            return;
        }
        let each = |seconds: f64| seconds * 1e3 / (bytes as f64 / GIB);
        println!(
            "  Proxy CPU:   user={:.1}ms  sys={:.1}ms per GiB  (peak RSS {} MiB)",
            each(user),
            each(system),
            self.peak_rss_kib() / 1024
        );
    }

    /// What `load` returns, and the user and system CPU seconds this
    /// process used while it ran.
    async fn cpu_during<T>(&self, load: impl Future<Output = T>) -> (T, (f64, f64)) {
        let before = self.cpu_seconds();
        let done = load.await;
        let after = self.cpu_seconds();
        (done, (after.0 - before.0, after.1 - before.1))
    }

    /// The user and system CPU time the process has used so far.
    fn cpu_seconds(&self) -> (f64, f64) {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", self.child.id()))
            .expect("read the proxy's /proc stat");
        // The command name, second of the line, may hold spaces: count
        // from its closing parenthesis. utime and stime are fields 14 and
        // 15 of the line, in clock ticks.
        let (_, after_name) = stat.rsplit_once(") ").expect("a command name in stat");
        // Its cost would be read as zero for the rest of the run.
        assert!(
            !after_name.starts_with('Z'),
            "the measured process left during the run"
        );
        let mut ticks = after_name
            .split(' ')
            .skip(11)
            .map(|field| field.parse::<f64>().expect("a tick count"));
        // SAFETY: sysconf reads a constant and touches no memory of ours.
        let per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
        let mut seconds = || ticks.next().expect("utime and stime") / per_second;
        (seconds(), seconds())
    }

    fn peak_rss_kib(&self) -> u64 {
        std::fs::read_to_string(format!("/proc/{}/status", self.child.id()))
            .expect("read the proxy's /proc status")
            .lines()
            .find_map(|line| line.strip_prefix("VmHWM:"))
            .and_then(|kib| kib.trim().trim_end_matches(" kB").parse().ok())
            .expect("VmHWM in status")
    }
}

/// The CPUs `var` lists, such as `0-3,8`; `None` when it is unset. A list
/// that does not parse stops the bench: a run believed pinned and not is
/// worse than no run.
fn cpus(var: &str) -> Option<libc::cpu_set_t> {
    let list = std::env::var(var).ok()?;
    let cpu = |s: &str| {
        s.parse::<usize>()
            .unwrap_or_else(|_| panic!("{}={}: not a CPU list", var, list))
    };
    // SAFETY: an all-zero cpu_set_t is the empty set.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    for range in list.split(',') {
        let (first, last) = range.split_once('-').unwrap_or((range, range));
        for n in cpu(first)..=cpu(last) {
            // SAFETY: CPU_SET indexes the set's array, so a CPU past its
            // size panics, here in the parent.
            unsafe { libc::CPU_SET(n, &mut set) };
        }
    }
    Some(set)
}

/// Runs in the child before exec. The child dies with the bench, which a
/// panic or Ctrl+C would otherwise orphan, and stays on `cpus` when given.
fn child_setup(cpus: Option<libc::cpu_set_t>) -> impl FnMut() -> Result<(), std::io::Error> {
    move || {
        // SAFETY: prctl and sched_setaffinity are async-signal-safe.
        unsafe {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            if let Some(set) = &cpus {
                let size = std::mem::size_of::<libc::cpu_set_t>();
                if libc::sched_setaffinity(0, size, set) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
        }
        Ok(())
    }
}

fn spawn_kiss(port: u16, content_dir: &PathBuf) -> ProcessHandle {
    assert_port_free(port);
    let mut child = unsafe {
        Command::new("kiss")
            .arg("--port")
            .arg(port.to_string())
            .arg("--static-dir")
            .arg(content_dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .pre_exec(child_setup(cpus("PT_BACKEND_CPUS")))
            .spawn()
            .expect("spawn kiss (is it installed? cargo install kiss)")
    };

    wait_for_port(port, &mut child);
    ProcessHandle {
        child,
        _cert_dir: None,
    }
}

fn backend_refs_json(ports: &[u16]) -> String {
    ports
        .iter()
        .map(|p| format!(r#"{{"name":"127.0.0.1","port":{}}}"#, p))
        .collect::<Vec<_>>()
        .join(",")
}

fn spawn_portail_no_filters(backend_ports: &[u16], port: u16) -> ProcessHandle {
    spawn_portail_pool(backend_ports, port, "connection")
}

fn spawn_portail_access_log(backend_ports: &[u16], port: u16) -> ProcessHandle {
    spawn_portail_pool_with(
        backend_ports,
        port,
        "connection",
        &[OsStr::new("--access-log=-")],
    )
}

fn spawn_portail_pool(backend_ports: &[u16], port: u16, pool_scope: &str) -> ProcessHandle {
    spawn_portail_pool_with(backend_ports, port, pool_scope, &[])
}

fn spawn_portail_pool_with(
    backend_ports: &[u16],
    port: u16,
    pool_scope: &str,
    args: &[&OsStr],
) -> ProcessHandle {
    let config = format!(
        r#"{{
  "gateway": {{"name":"bench","listeners":[{{"name":"http","protocol":"HTTP","port":{}}}]}},
  "httpRoutes": [{{"parentRefs":[{{"name":"bench","sectionName":"http"}}],"hostnames":["127.0.0.1"],"rules":[{{"matches":[{{"path":{{"type":"PathPrefix","value":"/"}}}}],"backendRefs":[{}]}}]}}],
  "observability": {{"logging":{{"level":"error","format":"pretty","output":"stderr"}}}},
  "performance": {{"backendPoolScope":"{}"}}
}}"#,
        port,
        backend_refs_json(backend_ports),
        pool_scope,
    );

    spawn_portail_with_config(&config, port, args)
}

fn spawn_portail_with_filters(backend_ports: &[u16], port: u16) -> ProcessHandle {
    let config = format!(
        r#"{{
  "gateway": {{"name":"bench","listeners":[{{"name":"http","protocol":"HTTP","port":{}}}]}},
  "httpRoutes": [{{"parentRefs":[{{"name":"bench","sectionName":"http"}}],"hostnames":["127.0.0.1"],"rules":[{{
    "matches":[{{"path":{{"type":"PathPrefix","value":"/"}}}}],
    "filters":[
      {{"type":"RequestHeaderModifier","requestHeaderModifier":{{"add":[{{"name":"X-Bench","value":"true"}}],"remove":["Accept"]}}}},
      {{"type":"URLRewrite","urlRewrite":{{"hostname":"rewritten.bench"}}}}
    ],
    "backendRefs":[{}]
  }}]}}],
  "observability": {{"logging":{{"level":"error","format":"pretty","output":"stderr"}}}},
  "performance": {{}}
}}"#,
        port,
        backend_refs_json(backend_ports),
    );

    spawn_portail_with_config(&config, port, &[])
}

/// portail with one TCP listener whose route forwards to `backend_port`.
fn spawn_portail_tcp(backend_port: u16, port: u16) -> ProcessHandle {
    let config = format!(
        r#"{{
  "gateway": {{"name":"bench","listeners":[{{"name":"tcp","protocol":"TCP","port":{}}}]}},
  "tcpRoutes": [{{"parentRefs":[{{"name":"bench","sectionName":"tcp"}}],"rules":[{{"backendRefs":[{}]}}]}}],
  "observability": {{"logging":{{"level":"error","format":"pretty","output":"stderr"}}}},
  "performance": {{}}
}}"#,
        port,
        backend_refs_json(&[backend_port]),
    );

    spawn_portail_with_config(&config, port, &[])
}

/// portail with one HTTPS listener that serves HTTP/1.1 and HTTP/2, and the
/// certificate a client has to trust to reach it.
fn spawn_portail_tls(backend_ports: &[u16], port: u16) -> (ProcessHandle, CertificateDer<'static>) {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec![TLS_HOST.to_string()])
            .expect("generate the bench certificate");
    let cert_dir = tempfile::tempdir().expect("create cert dir");
    std::fs::write(cert_dir.path().join("bench.crt"), cert.pem()).expect("write cert");
    std::fs::write(
        cert_dir.path().join("bench.key"),
        signing_key.serialize_pem(),
    )
    .expect("write key");

    let config = format!(
        r#"{{
  "gateway": {{"name":"bench","listeners":[{{"name":"https","protocol":"HTTPS","port":{},
    "tls":{{"mode":"Terminate","certificateRefs":[{{"name":"bench","hostname":"{}"}}]}}}}]}},
  "httpRoutes": [{{"parentRefs":[{{"name":"bench","sectionName":"https"}}],"hostnames":["{}"],"rules":[{{"matches":[{{"path":{{"type":"PathPrefix","value":"/"}}}}],"backendRefs":[{}]}}]}}],
  "observability": {{"logging":{{"level":"error","format":"pretty","output":"stderr"}}}},
  "performance": {{}}
}}"#,
        port,
        TLS_HOST,
        TLS_HOST,
        backend_refs_json(backend_ports),
    );

    let args = [
        OsStr::new("--cert-dir"),
        cert_dir.path().as_os_str(),
        OsStr::new("--http2"),
    ];
    let mut proxy = spawn_portail_with_config(&config, port, &args);
    proxy._cert_dir = Some(cert_dir);
    (proxy, cert.der().clone())
}

fn spawn_portail_with_config(config: &str, port: u16, args: &[&OsStr]) -> ProcessHandle {
    let config_file = tempfile::Builder::new()
        .suffix(".json")
        .tempfile()
        .expect("create temp config");
    std::fs::write(config_file.path(), config).expect("write config");

    let binary = portail_binary_path();
    assert_port_free(port);
    let mut child = unsafe {
        Command::new(&binary)
            .arg("--config")
            .arg(config_file.path())
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .pre_exec(child_setup(cpus("PT_PROXY_CPUS")))
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {:?}: {}", binary, e))
    };

    // Leak the config file so it lives as long as the process
    std::mem::forget(config_file);

    wait_for_port(port, &mut child);
    ProcessHandle {
        child,
        _cert_dir: None,
    }
}

fn portail_binary_path() -> PathBuf {
    if let Some(binary) = std::env::var_os("PORTAIL_BIN") {
        return PathBuf::from(binary);
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let release = root.join("target/release/portail");
    if release.exists() {
        return release;
    }
    let debug = root.join("target/debug/portail");
    if debug.exists() {
        return debug;
    }
    panic!("portail binary not found — run cargo build --release");
}

/// A process left listening by another run would pass for the one spawned
/// next, which fails to bind, and be measured in its place.
fn assert_port_free(port: u16) {
    let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
    assert!(
        TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_err(),
        "port {} is taken before its process is spawned: one left from another run?",
        port
    );
}

fn wait_for_port(port: u16, child: &mut Child) {
    let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().expect("the spawned process's status") {
            panic!(
                "the process for port {} left before listening: {}",
                port, status
            );
        }
        if Instant::now() > deadline {
            panic!("port {} not ready within 5s", port);
        }
        if TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
