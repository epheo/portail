//! Proxy throughput benchmark using rewrk-core + kiss backend.
//!
//! Spawns kiss (static file server) as backend, portail as proxy,
//! and uses rewrk-core as the load generator.
//!
//! Prerequisites:
//!   - `kiss` binary in PATH (cargo install kiss)
//!   - `portail` binary built (cargo build --release)
//!
//! Run with: cargo bench --bench proxy_throughput

use std::net::{SocketAddr, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use anyhow::Result;
use hyper::{Body, Method, Request, Uri};
use rewrk_core::{
    Batch, HttpProtocol, Producer, ReWrkBenchmark, RequestBatch, Sample, SampleCollector,
};

const CONCURRENCY: usize = 64;
const NUM_WORKERS: usize = 4;
const REQUESTS_PER_BATCH: usize = 500;
const BENCH_DURATION: Duration = Duration::from_secs(10);

const PORTAIL_PORT: u16 = 29_100;
const PORTAIL_FILTER_PORT: u16 = 29_200;
const KISS_BASE_PORT: u16 = 29_300;
const NUM_BACKENDS: u16 = 4;

fn main() {
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
        {
            println!("--- Direct to kiss (baseline, no proxy) ---");
            run_bench(KISS_BASE_PORT, "/bench.txt").await;
        }

        println!();

        // Scenario 1: No filters, single backend
        {
            let proxy = spawn_portail_no_filters(&backend_ports[..1], PORTAIL_PORT);
            println!("--- No-filter path (1 backend) ---");
            run_bench(PORTAIL_PORT, "/bench.txt").await;
            drop(proxy);
        }

        println!();

        // Scenario 2: No filters, multiple backends
        {
            let proxy = spawn_portail_no_filters(&backend_ports, PORTAIL_PORT);
            println!("--- No-filter path ({} backends) ---", NUM_BACKENDS);
            run_bench(PORTAIL_PORT, "/bench.txt").await;
            drop(proxy);
        }

        println!();

        // Scenario 3: With filters, single backend
        {
            let proxy = spawn_portail_with_filters(&backend_ports[..1], PORTAIL_FILTER_PORT);
            println!("--- Filter path (1 backend) ---");
            run_bench(PORTAIL_FILTER_PORT, "/bench.txt").await;
            drop(proxy);
        }

        println!();

        // Scenario 4: With filters, multiple backends
        {
            let proxy = spawn_portail_with_filters(&backend_ports, PORTAIL_FILTER_PORT);
            println!("--- Filter path ({} backends) ---", NUM_BACKENDS);
            run_bench(PORTAIL_FILTER_PORT, "/bench.txt").await;
            drop(proxy);
        }

        drop(kiss_handles);
        println!("\n=== Benchmark complete ===");
    });
}

async fn run_bench(port: u16, path: &str) {
    let uri = Uri::builder()
        .scheme("http")
        .authority(format!("127.0.0.1:{}", port))
        .path_and_query(path)
        .build()
        .unwrap();

    let producer = BenchProducer::new(path, BENCH_DURATION);
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
    collector.print_results(wall_elapsed);
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
    fn print_results(&self, wall_time: Duration) {
        if self.samples.is_empty() {
            println!("  No samples collected");
            return;
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
            "  Latency:     p50={:.0}µs  p95={:.0}µs  p99={:.0}µs  max={:.0}µs",
            p50, p95, p99, max
        );

        if !merged.errors().is_empty() {
            println!("  Errors:      {}", merged.errors().len());
            for e in merged.errors().iter().take(3) {
                println!("    - {:?}", e);
            }
        }
    }
}

// --- Infrastructure: spawn kiss + portail ---

struct ProcessHandle {
    child: Child,
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Set PR_SET_PDEATHSIG so the child is killed when the parent dies.
/// This prevents orphan processes when the bench panics or is Ctrl+C'd.
fn kill_on_parent_death() -> impl FnMut() -> Result<(), std::io::Error> {
    || {
        // SAFETY: prctl with PR_SET_PDEATHSIG is async-signal-safe
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };
        Ok(())
    }
}

fn spawn_kiss(port: u16, content_dir: &PathBuf) -> ProcessHandle {
    let child = unsafe {
        Command::new("kiss")
            .arg("--port")
            .arg(port.to_string())
            .arg("--static-dir")
            .arg(content_dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .pre_exec(kill_on_parent_death())
            .spawn()
            .expect("spawn kiss (is it installed? cargo install kiss)")
    };

    wait_for_port(port);
    ProcessHandle { child }
}

fn backend_refs_json(ports: &[u16]) -> String {
    ports
        .iter()
        .map(|p| format!(r#"{{"name":"127.0.0.1","port":{}}}"#, p))
        .collect::<Vec<_>>()
        .join(",")
}

fn spawn_portail_no_filters(backend_ports: &[u16], port: u16) -> ProcessHandle {
    let config = format!(
        r#"{{
  "gateway": {{"name":"bench","listeners":[{{"name":"http","protocol":"HTTP","port":{}}}]}},
  "httpRoutes": [{{"parentRefs":[{{"name":"bench","sectionName":"http"}}],"hostnames":["127.0.0.1"],"rules":[{{"matches":[{{"path":{{"type":"PathPrefix","value":"/"}}}}],"backendRefs":[{}]}}]}}],
  "observability": {{"logging":{{"level":"error","format":"pretty","output":"stderr"}}}},
  "performance": {{}}
}}"#,
        port,
        backend_refs_json(backend_ports),
    );

    spawn_portail_with_config(&config, port)
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

    spawn_portail_with_config(&config, port)
}

fn spawn_portail_with_config(config: &str, port: u16) -> ProcessHandle {
    let config_file = tempfile::Builder::new()
        .suffix(".json")
        .tempfile()
        .expect("create temp config");
    std::fs::write(config_file.path(), config).expect("write config");

    let binary = portail_binary_path();
    let child = unsafe {
        Command::new(&binary)
            .arg("--config")
            .arg(config_file.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .pre_exec(kill_on_parent_death())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {:?}: {}", binary, e))
    };

    // Leak the config file so it lives as long as the process
    std::mem::forget(config_file);

    wait_for_port(port);
    ProcessHandle { child }
}

fn portail_binary_path() -> PathBuf {
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

fn wait_for_port(port: u16) {
    let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if Instant::now() > deadline {
            panic!("port {} not ready within 5s", port);
        }
        if TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
