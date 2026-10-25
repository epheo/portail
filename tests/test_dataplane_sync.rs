//! `portail dataplane` against a stub controller: it serves what the
//! snapshots say, reports what it bound, follows versions, 204s and 404s,
//! rides out a controller outage and stops gracefully. The binary runs
//! with no cluster: no KUBERNETES_SERVICE_HOST, no API token.

#[path = "helpers/controller_stub.rs"]
mod controller_stub;
mod helpers;

use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use controller_stub::{free_port, http_snapshot, snapshot, ControllerStub, Mode, GATEWAY};
use helpers::*;
use portail::sync::{EndpointReport, Snapshot, Transport};

/// Generous for anything the data plane does at once (a bind, an apply,
/// a poll after an answer) on a loaded test host.
const SETTLE: Duration = Duration::from_secs(10);

/// A short hold: unchanged polls come back as 204 several times a second.
const SHORT_HOLD: Duration = Duration::from_millis(300);

/// The controller's real hold (`portail::sync::HOLD`).
const HOLD: Duration = portail::sync::HOLD;

struct DataPlane {
    process: PortailProcess,
    admin: SocketAddr,
    dir: tempfile::TempDir,
}

/// Spawn `portail dataplane` for [`GATEWAY`] against `stub`, presenting
/// token `token-1`, with `args` added.
fn start_dataplane(stub: &ControllerStub, args: &[&str]) -> DataPlane {
    let dir = tempfile::tempdir().unwrap();
    let ca = dir.path().join("ca.crt");
    std::fs::write(&ca, &stub.ca_pem).unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, "token-1").unwrap();
    let admin_port = free_port();
    let url = stub.url();
    let mut mode = vec![
        "dataplane",
        "--gateway",
        GATEWAY,
        "--controller-address",
        &url,
        "--controller-ca-file",
        ca.to_str().unwrap(),
        "--token-file",
        token.to_str().unwrap(),
    ];
    mode.extend_from_slice(args);
    let process = PortailProcess::start_kubernetes(&mode, None, admin_port);
    DataPlane {
        process,
        admin: SocketAddr::from(([127, 0, 0, 1], admin_port)),
        dir,
    }
}

impl DataPlane {
    fn token_file(&self) -> PathBuf {
        self.dir.path().join("token")
    }

    fn readyz(&self) -> Option<u16> {
        let request = b"GET /readyz HTTP/1.1\r\nHost: admin\r\n\r\n";
        http_request_timeout(self.admin, request, Duration::from_secs(1))
            .ok()
            .and_then(|response| extract_status(&response))
    }

    /// The value of an unlabeled series on /metrics.
    fn metric(&self, name: &str) -> Option<f64> {
        let request = b"GET /metrics HTTP/1.1\r\nHost: admin\r\n\r\n";
        let response = http_request(self.admin, request).ok()?;
        let body = String::from_utf8_lossy(extract_body(&response)).into_owned();
        let line = body
            .lines()
            .find(|l| l.split_whitespace().next() == Some(name))?;
        line.split_whitespace().nth(1)?.parse().ok()
    }

    /// Wait until /readyz answers `status`.
    fn wait_readyz(&self, status: u16, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.readyz() != Some(status) {
            assert!(
                Instant::now() < deadline,
                "/readyz never answered {status} (last {:?}):\n{}",
                self.readyz(),
                self.process.output()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// GET `path` on the data plane's `port`: status and body.
fn get(port: u16, path: &str) -> Option<(u16, Vec<u8>)> {
    let request = format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
    let response =
        http_request(SocketAddr::from(([127, 0, 0, 1], port)), request.as_bytes()).ok()?;
    Some((extract_status(&response)?, extract_body(&response).to_vec()))
}

/// Wait until GET / on `port` answers 200 with `body`.
fn wait_serving(dp: &DataPlane, port: u16, body: &str) {
    let deadline = Instant::now() + SETTLE;
    loop {
        let got = get(port, "/");
        if got
            .as_ref()
            .is_some_and(|(s, b)| *s == 200 && b == body.as_bytes())
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "port {port} never served {body:?} (last {got:?}):\n{}",
            dp.process.output()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn refuses(port: u16) -> bool {
    TcpStream::connect(("127.0.0.1", port)).is_err()
}

fn applied(version: &str) -> impl Fn(&controller_stub::Recorded) -> bool + '_ {
    move |r| {
        r.request
            .report
            .as_ref()
            .is_some_and(|report| report.applied == version)
    }
}

#[test]
fn serves_a_snapshot_and_reports_its_binds() {
    let backend = TestBackend::spawn("from-backend");
    let port = free_port();
    let served = http_snapshot(port, backend.addr);
    let stub = ControllerStub::start(SHORT_HOLD, Mode::Serve(Some(served.clone())));
    let dp = start_dataplane(&stub, &[]);

    dp.wait_readyz(200, SETTLE);
    wait_serving(&dp, port, "from-backend");

    let first = &stub.requests()[0];
    assert_eq!(first.path, "/v1/sync/default/gw");
    assert!(first.request.have.is_empty() && first.request.report.is_none());
    let reported = stub
        .wait_for(0, SETTLE, applied(&served.version))
        .unwrap_or_else(|| panic!("no report of {}:\n{}", served.version, dp.process.output()));
    assert_eq!(reported.request.have, served.version);
    assert_eq!(
        reported.request.data_plane_version,
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(reported.authorization.as_deref(), Some("Bearer token-1"));
    let report = reported.request.report.unwrap();
    assert_eq!(report.apply_error, None);
    assert!(!report.leaving);
    let listeners: Vec<(&str, &[EndpointReport])> = report
        .listeners
        .iter()
        .map(|l| (l.name.as_str(), l.endpoints.as_slice()))
        .collect();
    assert_eq!(
        listeners,
        [(
            "http",
            &[EndpointReport {
                address: None,
                port,
                transport: Transport::Tcp,
                bound: true,
                error: None,
            }][..]
        )]
    );
}

/// A new version is applied; an unchanged one loops on 204s; a 404 closes
/// every listener and the data plane polls on, as a first sync, until the
/// Gateway is back.
#[test]
fn follows_versions_204s_and_404s() {
    let (a, b) = (TestBackend::spawn("a"), TestBackend::spawn("b"));
    let port = free_port();
    let first = http_snapshot(port, a.addr);
    let stub = ControllerStub::start(SHORT_HOLD, Mode::Serve(Some(first.clone())));
    let dp = start_dataplane(&stub, &[]);
    wait_serving(&dp, port, "a");

    let from = stub.requests().len();
    std::thread::sleep(SHORT_HOLD * 6);
    let unchanged = stub.requests()[from..]
        .iter()
        .filter(|r| r.request.have == first.version)
        .count();
    assert!(unchanged >= 3, "{unchanged} polls in six holds");
    assert_eq!(get(port, "/").unwrap().1, b"a");

    let second = http_snapshot(port, b.addr);
    stub.set(Mode::Serve(Some(second.clone())));
    wait_serving(&dp, port, "b");
    assert!(stub.wait_for(0, SETTLE, applied(&second.version)).is_some());

    stub.set(Mode::Gone);
    let deadline = Instant::now() + SETTLE;
    while !(refuses(port) && dp.readyz() == Some(503)) {
        assert!(
            Instant::now() < deadline,
            "still serving after a 404:\n{}",
            dp.process.output()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let after_gone = stub.requests().len();
    // Polls after a 404 are paced by the backoff from 1s: two come
    // within the first two ceilings.
    for i in 0..2 {
        let poll = stub
            .wait_for(after_gone + i, Duration::from_secs(1 + 2) + SETTLE, |_| {
                true
            })
            .unwrap_or_else(|| panic!("polling stopped after a 404:\n{}", dp.process.output()));
        assert!(
            poll.request.have.is_empty() && poll.request.report.is_none(),
            "{poll:?}"
        );
    }

    stub.set(Mode::Serve(Some(second)));
    dp.wait_readyz(200, HOLD + SETTLE);
    wait_serving(&dp, port, "b");
}

/// A 503 closes the connection, so the Service can pick a replica again,
/// and its Retry-After wins over the backoff.
#[test]
fn a_503_reconnects_after_its_retry_after() {
    const RETRY_AFTER: u64 = 2;
    let backend = TestBackend::spawn("ok");
    let port = free_port();
    let served = http_snapshot(port, backend.addr);
    let stub = ControllerStub::start(SHORT_HOLD, Mode::Serve(Some(served.clone())));
    let dp = start_dataplane(&stub, &[]);
    wait_serving(&dp, port, "ok");

    stub.set(Mode::Unavailable(Some(RETRY_AFTER)));
    let deadline = Instant::now() + SETTLE;
    let refused = loop {
        let answers = stub.answers();
        if let Some(a) = answers.iter().find(|a| a.status == 503) {
            break a.clone();
        }
        assert!(Instant::now() < deadline, "no 503 answered");
        std::thread::sleep(Duration::from_millis(20));
    };
    let next = stub
        .wait_for(0, SETTLE, |r| r.at > refused.at)
        .unwrap_or_else(|| panic!("no poll after the 503:\n{}", dp.process.output()));
    assert_ne!(
        next.connection, refused.connection,
        "the 503's connection was reused"
    );
    assert!(
        next.at - refused.at >= Duration::from_secs(RETRY_AFTER) - Duration::from_millis(100),
        "polled again {:?} after a Retry-After of {RETRY_AFTER}s",
        next.at - refused.at
    );
    assert_eq!(dp.readyz(), Some(200), "a 503 is no reason to stop serving");

    stub.set(Mode::Serve(Some(served)));
    wait_serving(&dp, port, "ok");
}

/// kubelet rotates the projected token: each poll reads the file anew.
#[test]
fn the_token_is_read_per_request() {
    let backend = TestBackend::spawn("ok");
    let port = free_port();
    let stub = ControllerStub::start(
        SHORT_HOLD,
        Mode::Serve(Some(http_snapshot(port, backend.addr))),
    );
    let dp = start_dataplane(&stub, &[]);
    wait_serving(&dp, port, "ok");

    std::fs::write(dp.token_file(), "token-2\n").unwrap();
    let from = stub.requests().len();
    let rotated = stub.wait_for(from, SETTLE, |r| {
        r.authorization.as_deref() == Some("Bearer token-2")
    });
    assert!(
        rotated.is_some(),
        "the rotated token was never sent:\n{}",
        dp.process.output()
    );
}

/// With no controller to answer, a new data plane is NotReady and binds
/// nothing; the controller's first snapshot makes it Ready.
#[test]
fn starts_not_ready_until_the_controller_answers() {
    let backend = TestBackend::spawn("ok");
    let port = free_port();
    let stub = ControllerStub::down(
        SHORT_HOLD,
        Mode::Serve(Some(http_snapshot(port, backend.addr))),
    );
    let dp = start_dataplane(&stub, &[]);

    dp.wait_readyz(503, SETTLE);
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(dp.readyz(), Some(503));
    assert!(refuses(port), "bound with no snapshot");

    stub.up();
    // The next poll comes after a backoff the failures so far grew.
    dp.wait_readyz(200, HOLD + SETTLE);
    wait_serving(&dp, port, "ok");
}

/// SIGTERM: the data plane reports leaving at once, keeps its listeners
/// and /readyz up for --shutdown-delay, then drains: new connections are
/// refused, the one in flight completes, and the process exits 0.
#[test]
fn sigterm_reports_leaving_serves_through_the_delay_then_drains() {
    const DELAY: Duration = Duration::from_secs(3);
    // In flight past the delay by as much again: the drain must wait.
    const SLOW: Duration = Duration::from_secs(6);
    let (backend, slow_arrived) = spawn_slow_backend(SLOW);
    let port = free_port();
    let stub = ControllerStub::start(HOLD, Mode::Serve(Some(http_snapshot(port, backend))));
    let mut dp = start_dataplane(&stub, &["--shutdown-delay", "3s"]);
    wait_serving(&dp, port, "ok");

    let from = stub.requests().len();
    let stopped = Instant::now();
    dp.process.terminate();

    let leave = stub.wait_for(from, Duration::from_secs(2), |r| {
        r.request
            .report
            .as_ref()
            .is_some_and(|report| report.leaving)
    });
    assert!(leave.is_some(), "no leave report:\n{}", dp.process.output());
    assert_eq!(dp.readyz(), Some(200), "NotReady during the delay");
    assert_eq!(
        get(port, "/").map(|(s, _)| s),
        Some(200),
        "not serving during the delay"
    );

    let slow = std::thread::spawn(move || {
        let request = b"GET /slow HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n";
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        http_request_timeout(addr, request, SLOW + SETTLE)
    });
    slow_arrived
        .recv_timeout(SETTLE)
        .expect("the slow request reached the backend");
    assert!(stopped.elapsed() < DELAY, "too slow to test the delay");

    // Half way between the drain starting and the slow request ending.
    std::thread::sleep((DELAY + SLOW / 2).saturating_sub(stopped.elapsed()));
    assert!(
        refuses(port),
        "still accepting after the delay:\n{}",
        dp.process.output()
    );

    let response = slow
        .join()
        .unwrap()
        .expect("the request in flight completes");
    assert_eq!(extract_status(&response), Some(200));
    let status = dp
        .process
        .wait_exit(SETTLE)
        .unwrap_or_else(|| panic!("no exit after the drain:\n{}", dp.process.output()));
    assert!(status.success(), "{status}:\n{}", dp.process.output());
    let output = dp.process.output();
    assert!(
        output.contains("Leave reported to the controller"),
        "{output}"
    );
    assert!(output.contains("Data plane shutdown complete"), "{output}");
}

/// A TCP and a UDP listener on one port both bind and both forward.
#[test]
fn tcp_and_udp_listeners_share_a_port() {
    let (tcp_echo, udp_echo) = (TcpEchoBackend::spawn(), UdpEchoBackend::spawn());
    let port = free_port();
    let route = |name: &str, backend: SocketAddr| {
        serde_json::json!({
            "parentRefs": [{"name": "gw", "sectionName": name}],
            "rules": [{"backendRefs": [{"name": backend.ip().to_string(), "port": backend.port()}]}],
        })
    };
    let served: Snapshot = snapshot(serde_json::json!({
        "gateway": {"name": "gw", "listeners": [
            {"name": "tcp", "protocol": "TCP", "port": port},
            {"name": "udp", "protocol": "UDP", "port": port},
        ]},
        "tcpRoutes": [route("tcp", tcp_echo.addr)],
        "udpRoutes": [route("udp", udp_echo.addr)],
    }));
    let stub = ControllerStub::start(SHORT_HOLD, Mode::Serve(Some(served.clone())));
    let dp = start_dataplane(&stub, &[]);
    dp.wait_readyz(200, SETTLE);

    let proxy = SocketAddr::from(([127, 0, 0, 1], port));
    assert_eq!(
        tcp_roundtrip(proxy, b"over tcp", SETTLE).unwrap(),
        b"over tcp"
    );
    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    let reply = udp_send_recv(&client, proxy, b"over udp", 10).unwrap();
    assert!(
        reply.ends_with(b"over udp"),
        "{:?}",
        String::from_utf8_lossy(&reply)
    );

    let report = stub
        .wait_for(0, SETTLE, applied(&served.version))
        .and_then(|r| r.request.report)
        .unwrap();
    let bound: Vec<(Transport, bool)> = report
        .listeners
        .iter()
        .flat_map(|l| l.endpoints.iter().map(|e| (e.transport, e.bound)))
        .collect();
    assert_eq!(bound, [(Transport::Tcp, true), (Transport::Udp, true)]);
}

/// Long enough for the poll backoff to reach its ceiling (the hold) and
/// the no-snapshot watchdog's time (the hold) to pass, with margin.
const OUTAGE: Duration = Duration::from_secs(2 * HOLD.as_secs() + 5);

/// The controller goes away after the first apply: under load the data
/// plane serves every request and stays Ready, and syncs again when the
/// controller is back.
#[test]
fn serves_through_a_controller_outage_under_load() {
    serves_through_an_outage(OUTAGE);
}

/// The acceptance run of the design (9.2): ten minutes.
/// `cargo test --release --test test_dataplane_sync -- --ignored`
#[test]
#[ignore]
fn serves_through_a_ten_minute_controller_outage_under_load() {
    serves_through_an_outage(Duration::from_secs(600));
}

fn serves_through_an_outage(outage: Duration) {
    let backend = TestBackend::spawn("ok");
    let port = free_port();
    let stub = ControllerStub::start(HOLD, Mode::Serve(Some(http_snapshot(port, backend.addr))));
    let dp = start_dataplane(&stub, &[]);
    wait_serving(&dp, port, "ok");
    assert_eq!(dp.metric("portail_controller_connected"), Some(1.0));

    stub.go_down();
    let load = std::thread::spawn(move || load::run(port, outage));
    let deadline = Instant::now() + outage;
    while Instant::now() < deadline {
        assert_eq!(
            dp.readyz(),
            Some(200),
            "NotReady in the outage:\n{}",
            dp.process.output()
        );
        std::thread::sleep(Duration::from_secs(1));
    }
    let (requests, successes, errors) = load.join().unwrap();
    assert!(successes > 0, "no load ran");
    assert!(
        errors.is_empty() && requests == successes,
        "{} of {requests} requests failed: {errors:?}",
        requests - successes
    );
    assert!(dp
        .process
        .output()
        .contains("Sync of Gateway default/gw failed"));
    assert_eq!(dp.metric("portail_controller_connected"), Some(0.0));
    let age = dp.metric("portail_config_age_seconds").unwrap();
    assert!(
        age >= outage.as_secs_f64(),
        "config age {age}s after the outage"
    );

    let from = stub.requests().len();
    stub.up();
    assert!(
        stub.wait_for(from, HOLD + SETTLE, |_| true).is_some(),
        "never polled again:\n{}",
        dp.process.output()
    );
    assert_eq!(get(port, "/").map(|(s, _)| s), Some(200));
}

mod load {
    use std::time::{Duration, Instant};

    use hyper_014::{Body, Method, Request, Uri};
    use rewrk_core::{
        Batch, HttpProtocol, Producer, ReWrkBenchmark, RequestBatch, Sample, SampleCollector,
    };

    /// Keep-alive connections, as clients of a Gateway hold them.
    const CONNECTIONS: usize = 4;
    /// Requests per batch, one batch per pause: a steady load, light
    /// enough to run for minutes beside other tests.
    const BATCH: usize = 50;
    const PAUSE: Duration = Duration::from_millis(10);

    /// GET / on 127.0.0.1:`port` for `duration`: requests sent,
    /// successes, errors.
    pub fn run(port: u16, duration: Duration) -> (usize, usize, Vec<String>) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let uri: Uri = format!("http://127.0.0.1:{port}/").parse().unwrap();
            let producer = Paced {
                duration,
                started: None,
            };
            let mut benchmark = ReWrkBenchmark::create(
                uri,
                CONNECTIONS,
                HttpProtocol::HTTP1,
                producer,
                Collector::default(),
            )
            .await
            .unwrap();
            benchmark.set_num_workers(1);
            benchmark.run().await;
            let collector = benchmark.consume_collector().await;
            let Some(total) = collector.samples.into_iter().reduce(|a, b| a + b) else {
                return (0, 0, Vec::new());
            };
            let errors = total.errors().iter().map(|e| format!("{e:?}")).collect();
            (
                total.total_requests(),
                total.total_successful_requests(),
                errors,
            )
        })
    }

    #[derive(Clone)]
    struct Paced {
        duration: Duration,
        started: Option<Instant>,
    }

    #[rewrk_core::async_trait]
    impl Producer for Paced {
        fn ready(&mut self) {
            self.started = Some(Instant::now());
        }

        async fn create_batch(&mut self) -> anyhow::Result<RequestBatch> {
            if self.started.expect("ready first").elapsed() >= self.duration {
                return Ok(RequestBatch::End);
            }
            tokio::time::sleep(PAUSE).await;
            let requests = (0..BATCH)
                .map(|_| {
                    Request::builder()
                        .method(Method::GET)
                        .uri(Uri::from_static("/"))
                        .body(Body::empty())
                })
                .collect::<Result<_, _>>()?;
            Ok(RequestBatch::Batch(Batch { tag: 0, requests }))
        }
    }

    #[derive(Default)]
    struct Collector {
        samples: Vec<Sample>,
    }

    #[rewrk_core::async_trait]
    impl SampleCollector for Collector {
        async fn process_sample(&mut self, sample: Sample) -> anyhow::Result<()> {
            self.samples.push(sample);
            Ok(())
        }
    }
}
