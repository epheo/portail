//! Heap allocations on the request path, counted where a bench cannot see
//! them: per request, in a flood of the same request, and per connection,
//! in a churn of one-request connections.
//!
//! The counting allocator serves the whole test binary. Counts are per
//! thread, so tests running in parallel do not see each other's.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::{ArcSwap, ArcSwapOption};
use portail::config::{CertificateRef, PerformanceConfig};
use portail::proxy::health::HealthRegistry;
use portail::proxy::http_parser::{extract_routing_info, HeadError};
use portail::proxy::listener::{self, ListenerCtx, ServeConfig};
use portail::proxy::request_processor::{analyze_request, analyze_udp_request, RoutingResult};
use portail::proxy::tls::{build_server_config, TlsKind, TlsPlan};
use portail::routing::{
    Backend, BackendAction, BackendSelector, HttpRouteRule, PathMatchType, RouteTable,
};
use rustls::pki_types::CertificateDer;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

struct Counting;

thread_local! {
    // const and destructor-free: reading it never allocates.
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

fn count_one() {
    let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count_one();
        System.alloc(layout)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count_one();
        System.alloc_zeroed(layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count_one();
        System.realloc(ptr, layout, new_size)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Allocations `f` makes on this thread. It runs once before counting, so
/// lazy statics and log callsites settle first.
fn allocations<T>(mut f: impl FnMut() -> T) -> usize {
    std::hint::black_box(f());
    let before = ALLOCATIONS.with(Cell::get);
    std::hint::black_box(f());
    ALLOCATIONS.with(Cell::get) - before
}

/// `a.test` routes `/api` to one backend.
fn table() -> RouteTable {
    table_to("127.0.0.1:8001".parse().unwrap())
}

/// `a.test` routes `/api` to `backend`.
fn table_to(backend: SocketAddr) -> RouteTable {
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "a.test",
        HttpRouteRule::new(
            PathMatchType::Prefix,
            "/api".to_string(),
            vec![],
            vec![],
            vec![],
            vec![Backend {
                socket_addr: backend,
                weight: 1,
                filters: vec![],
                use_tls: false,
                server_name: String::new(),
                verify: None,
                action: BackendAction::Forward,
            }],
        ),
    );
    rt
}

const OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";

/// A backend answering every request with `OK`, on threads of its own, so
/// its allocations are never counted.
fn spawn_backend() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { return };
            std::thread::spawn(move || {
                let (mut seen, mut chunk) = (Vec::new(), [0u8; 4096]);
                while let Ok(n @ 1..) = sock.read(&mut chunk) {
                    seen.extend_from_slice(&chunk[..n]);
                    while let Some(end) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
                        seen.drain(..end + 4);
                        if sock.write_all(OK).is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    addr
}

/// `n` client connections to `proxy` from a thread of their own, each a
/// request, its whole response, and a close; then waits until the proxy
/// has let go of all of them.
async fn churn(proxy: SocketAddr, n: usize, active: &TaskTracker) {
    let client = std::thread::spawn(move || {
        for _ in 0..n {
            let mut sock = std::net::TcpStream::connect(proxy).unwrap();
            sock.write_all(b"GET /api HTTP/1.1\r\nHost: a.test\r\n\r\n")
                .unwrap();
            let (mut got, mut chunk) = (Vec::new(), [0u8; 4096]);
            while !got.ends_with(b"\r\n\r\nok") {
                let read = sock.read(&mut chunk).unwrap();
                assert!(read > 0, "{}", String::from_utf8_lossy(&got));
                got.extend_from_slice(&chunk[..read]);
            }
        }
    });
    while !client.is_finished() || !active.is_empty() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    client.join().unwrap();
}

/// Ten per connection, setup and teardown and the request it carries:
/// the task, two socket registrations, the read, scratch and response-head
/// buffers, the drain token and its watch, and two for the per-connection
/// pool taking back the backend connection.
/// Three more come from this test's setup: tokio boxes the connection
/// future in debug builds, and with one connection at a time the drain
/// token's parent and tokio's I/O driver each regrow a list.
#[test]
fn a_churned_connection_allocates_a_fixed_few_times() {
    const CONNS: usize = 200;
    let backend = spawn_backend();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let per_conn = rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = listener.local_addr().unwrap();
        let active = TaskTracker::new();
        let ctx = ListenerCtx {
            server_port: 80,
            routes: Arc::new(ArcSwap::from_pointee(table_to(backend))),
            cfg: ServeConfig::from_perf(&PerformanceConfig::default(), 64),
            plan: Default::default(),
            health: Arc::new(HealthRegistry::new()),
            selector: Arc::new(BackendSelector::new()),
            active_connections: active.clone(),
            shared_pool: None,
        };
        tokio::spawn(listener::serve(listener, ctx, CancellationToken::new()));
        // Lazy statics, metric slots and the runtime's own tables settle.
        churn(proxy, 20, &active).await;
        let before = ALLOCATIONS.with(Cell::get);
        churn(proxy, CONNS, &active).await;
        (ALLOCATIONS.with(Cell::get) - before) as f64 / CONNS as f64
    });
    assert!(
        per_conn.round() <= 13.0,
        "{per_conn} allocations per connection"
    );
}

/// What a client speaks to the proxy.
#[derive(Clone, Copy, PartialEq)]
enum Client {
    /// HTTP/1.1 over TCP.
    Plain,
    /// HTTP/1.1 over TLS.
    Tls,
    /// HTTP/2 over TLS.
    H2,
}

/// A certificate for `a.test` and the config that terminates with it,
/// HTTP/2 offered beside HTTP/1.1.
fn tls_config() -> (CertificateDer<'static>, Arc<rustls::ServerConfig>) {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["a.test".to_string()]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.crt"), cert.pem()).unwrap();
    std::fs::write(dir.path().join("a.key"), signing_key.serialize_pem()).unwrap();
    let refs = [CertificateRef {
        name: "a".to_string(),
        hostname: Some("a.test".to_string()),
        ..Default::default()
    }];
    let config = build_server_config(&refs, dir.path(), true).unwrap();
    (cert.der().clone(), Arc::new(config))
}

/// A proxy on this thread's runtime, routing to `backend`; with `tls`, it
/// terminates for `a.test`. Returns its address and its count of open
/// connections.
async fn proxy(
    backend: SocketAddr,
    tls: Option<Arc<rustls::ServerConfig>>,
) -> (SocketAddr, TaskTracker) {
    let plan = tls.map(|config| {
        let mut plan = TlsPlan::default();
        plan.terminate = Some(config);
        plan.add(Some("a.test"), TlsKind::Terminate);
        plan
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let active = TaskTracker::new();
    let perf = PerformanceConfig {
        http2: true,
        ..Default::default()
    };
    let ctx = ListenerCtx {
        server_port: 80,
        routes: Arc::new(ArcSwap::from_pointee(table_to(backend))),
        cfg: ServeConfig::from_perf(&perf, 64),
        plan: Arc::new(ArcSwapOption::from_pointee(plan)),
        health: Arc::new(HealthRegistry::new()),
        selector: Arc::new(BackendSelector::new()),
        active_connections: active.clone(),
        shared_pool: None,
    };
    tokio::spawn(listener::serve(listener, ctx, CancellationToken::new()));
    (addr, active)
}

/// `n` requests to `/api` on one connection to `proxy`, from a thread and a
/// runtime of their own: none of the client's allocations is counted.
fn requests(
    proxy: SocketAddr,
    client: Client,
    cert: CertificateDer<'static>,
    n: usize,
) -> std::thread::JoinHandle<()> {
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    async fn h1(mut conn: impl AsyncRead + AsyncWrite + Unpin, n: usize) {
        for _ in 0..n {
            conn.write_all(b"GET /api HTTP/1.1\r\nHost: a.test\r\n\r\n")
                .await
                .unwrap();
            let (mut got, mut chunk) = (Vec::new(), [0u8; 4096]);
            while !got.ends_with(b"\r\n\r\nok") {
                let read = conn.read(&mut chunk).await.unwrap();
                assert!(read > 0, "{}", String::from_utf8_lossy(&got));
                got.extend_from_slice(&chunk[..read]);
            }
        }
    }

    std::thread::spawn(move || {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let tcp = tokio::net::TcpStream::connect(proxy).await.unwrap();
            if client == Client::Plain {
                return h1(tcp, n).await;
            }
            let mut roots = rustls::RootCertStore::empty();
            roots.add(cert).unwrap();
            let mut config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let alpn: &[u8] = if client == Client::H2 {
                b"h2"
            } else {
                b"http/1.1"
            };
            config.alpn_protocols = vec![alpn.to_vec()];
            let name = rustls::pki_types::ServerName::try_from("a.test").unwrap();
            let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
                .connect(name, tcp)
                .await
                .unwrap();
            if client == Client::Tls {
                return h1(tls, n).await;
            }
            let (mut send, connection) = h2::client::handshake(tls).await.unwrap();
            tokio::spawn(connection);
            for _ in 0..n {
                send = send.ready().await.unwrap();
                let request = http::Request::get("https://a.test/api").body(()).unwrap();
                let (response, _) = send.send_request(request, true).unwrap();
                let response = response.await.unwrap();
                assert_eq!(response.status(), 200);
                let mut body = response.into_body();
                while let Some(chunk) = body.data().await {
                    let read = chunk.unwrap().len();
                    body.flow_control().release_capacity(read).unwrap();
                }
            }
        });
    })
}

/// Requests the long connection of [`per_request`] carries beyond the short one.
const EXTRA: usize = 200;

/// Allocations a TLS connection pays once can land on either side of a
/// window's edge: over 160 runs the difference moved by two at most. One
/// real allocation per request adds `EXTRA`.
const STRAY: usize = 4;

/// Allocations `EXTRA` requests cost on a connection already open, counted
/// on this thread, where `serve` put what answers at the address it returns.
/// Two connections carry different numbers of requests, and their difference
/// leaves out what each pays once: the handshake and the close.
fn per_request<F>(
    client: Client,
    serve: impl FnOnce(Option<Arc<rustls::ServerConfig>>) -> F,
) -> usize
where
    F: std::future::Future<Output = (SocketAddr, TaskTracker)>,
{
    const FEW: usize = 100;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (cert, config) = tls_config();
        let (addr, active) = serve((client != Client::Plain).then_some(config)).await;
        let mut counted = Vec::new();
        // The first connection only lets lazy statics, metric slots and the
        // runtime's own tables settle.
        for n in [FEW, FEW, FEW + EXTRA] {
            let before = ALLOCATIONS.with(Cell::get);
            let client = requests(addr, client, cert.clone(), n);
            while !client.is_finished() || !active.is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            client.join().unwrap();
            counted.push(ALLOCATIONS.with(Cell::get) - before);
        }
        // Every ceiling below also holds at zero, which is what a proxy
        // working on another thread would read.
        assert!(counted[1] > 0, "the counting thread saw no connection");
        counted[2].saturating_sub(counted[1])
    })
}

/// [`per_request`] through the proxy, in front of a backend.
fn per_proxied_request(client: Client) -> usize {
    let backend = spawn_backend();
    per_request(client, |tls| proxy(backend, tls))
}

#[test]
fn a_keepalive_request_allocates_nothing() {
    let n = per_proxied_request(Client::Plain);
    // Exact: a plain connection leaves nothing at a window's edge.
    assert_eq!(n, 0, "allocations over {EXTRA} requests");
}

/// Two, which TLS adds: the same request over plain TCP allocates nothing.
#[test]
fn a_keepalive_request_over_tls_allocates_twice() {
    let n = per_proxied_request(Client::Tls);
    assert!(
        n <= 2 * EXTRA + STRAY,
        "{n} allocations over {EXTRA} requests"
    );
}

/// Twenty in a debug build and nineteen in release, against two for the
/// same request over HTTP/1.1. The bridge gives each stream what a
/// connection gets: a task, an engine with its buffers and drain watch (the
/// churn count above), and a pipe to it. A ceiling to lower with the
/// bridge's cost, never to raise.
#[test]
fn an_h2_request_allocates_like_a_connection_of_its_own() {
    let n = per_proxied_request(Client::H2);
    assert!(
        n <= 20 * EXTRA + STRAY,
        "{n} allocations over {EXTRA} requests"
    );
}

#[test]
fn a_route_miss_allocates_nothing() {
    let rt = table();
    for (host, path) in [("nope.test", "/api"), ("a.test", "/elsewhere")] {
        let n = allocations(|| rt.find_http_route(host, "GET", path, &[], "", 80));
        assert_eq!(n, 0, "{host}{path}");
    }
}

#[test]
fn a_404_allocates_nothing() {
    let (rt, sel, health) = (table(), BackendSelector::new(), HealthRegistry::new());
    let req = b"GET /api HTTP/1.1\r\nHost: nope.test\r\n\r\n";
    let n = allocations(|| {
        let result = analyze_request(&rt, &sel, req, 80, &health, false).unwrap();
        assert!(matches!(result, RoutingResult::LocalReply(_)));
    });
    assert_eq!(n, 0);
}

#[test]
fn a_routed_request_allocates_nothing() {
    let (rt, sel, health) = (table(), BackendSelector::new(), HealthRegistry::new());
    for req in [
        &b"GET /api/v1/users?page=2 HTTP/1.1\r\nHost: a.test\r\n\r\n"[..],
        // Dots and escapes, but no dot-segment: nothing to normalize.
        b"GET /api/.well-known/app.min.js?v=1.2 HTTP/1.1\r\nHost: a.test\r\n\r\n",
        b"GET /api/a%20b/%2e%2ex HTTP/1.1\r\nHost: a.test\r\n\r\n",
    ] {
        let n = allocations(|| {
            let result = analyze_request(&rt, &sel, req, 80, &health, false).unwrap();
            assert!(matches!(result, RoutingResult::HttpForward { .. }));
        });
        assert_eq!(n, 0, "{}", String::from_utf8_lossy(req));
    }
}

/// Once per datagram on a UDP port with no route, spoofed ones included,
/// and once per connection on a TCP one.
#[test]
fn an_unrouted_l4_port_allocates_nothing() {
    let (rt, sel, health) = (table(), BackendSelector::new(), HealthRegistry::new());
    let tls_hello = [0x16, 0x03, 0x01, 0x00, 0xf4, 0x01, 0x00, 0x00];
    let n = allocations(|| {
        let udp = analyze_udp_request(&rt, &sel, 5353, &health).unwrap();
        let tcp = analyze_request(&rt, &sel, &tls_hello, 5432, &health, false).unwrap();
        assert!(matches!(udp, RoutingResult::CloseConnection));
        assert!(matches!(tcp, RoutingResult::CloseConnection));
    });
    assert_eq!(n, 0);
}

#[test]
fn a_refused_dot_segment_allocates_nothing() {
    let (rt, sel, health) = (table(), BackendSelector::new(), HealthRegistry::new());
    let req = b"GET /public/..;/api HTTP/1.1\r\nHost: a.test\r\n\r\n";
    let n = allocations(|| {
        let result = analyze_request(&rt, &sel, req, 80, &health, false).unwrap();
        assert!(matches!(result, RoutingResult::LocalReply(r) if r.status == 400));
    });
    assert_eq!(n, 0);
}

#[test]
fn an_unparseable_head_allocates_nothing() {
    let (rt, sel, health) = (table(), BackendSelector::new(), HealthRegistry::new());
    let heads: [(&[u8], HeadError); 3] = [
        (b"NOT A REQUEST LINE\r\n\r\n", HeadError::RequestLine),
        (
            b"GET /\xff HTTP/1.1\r\nHost: a.test\r\n\r\n",
            HeadError::RequestLineNotUtf8,
        ),
        (b"GET /api HTTP/1.1\r\nAccept: */*\r\n\r\n", HeadError::Host),
    ];
    for (head, refusal) in heads {
        assert_eq!(extract_routing_info(head).err(), Some(refusal));
        let n = allocations(|| {
            let result = analyze_request(&rt, &sel, head, 80, &health, false).unwrap();
            assert!(matches!(result, RoutingResult::LocalReply(r) if r.status == 400));
        });
        assert_eq!(n, 0, "{refusal}");
    }
}

#[test]
fn a_path_to_normalize_allocates_once() {
    let (rt, sel, health) = (table(), BackendSelector::new(), HealthRegistry::new());
    let req = b"GET /public/%2e%2e/api/./users?page=2 HTTP/1.1\r\nHost: a.test\r\n\r\n";
    let n = allocations(|| {
        let result = analyze_request(&rt, &sel, req, 80, &health, false).unwrap();
        assert!(matches!(result, RoutingResult::HttpForward { .. }));
    });
    assert!(n <= 1, "{n} allocations");
}

/// What a URLRewrite rule costs each request it takes: a full-path value,
/// its query and the new hostname are the rule's, borrowed. Each request
/// cloned the path and hostname, and took a count on the query.
#[test]
fn a_full_path_rewrite_borrows_the_rules_values() {
    use portail::proxy::http_filters::extract_url_rewrite;
    use portail::routing::{HttpFilter, URLRewritePath};
    let filters = [HttpFilter::URLRewrite {
        hostname: Some("new.test".to_string()),
        path: Some(URLRewritePath::ReplaceFullPath("/new?from=rule".into())),
    }];
    let n = allocations(|| extract_url_rewrite(&filters, "/api", "/api/users").is_some());
    assert_eq!(n, 0);

    let HttpFilter::URLRewrite {
        path: Some(URLRewritePath::ReplaceFullPath(value)),
        ..
    } = &filters[0]
    else {
        unreachable!()
    };
    let query = value.query.as_ref().unwrap();
    let rewrite = extract_url_rewrite(&filters, "/api", "/api/users");
    assert_eq!(
        std::sync::Arc::strong_count(query),
        1,
        "a count per request"
    );
    drop(rewrite);
}

/// Many tunnels on one worker, each end sending and reading in bursts: the
/// relay allocates nothing per read. Reads change size with every burst; a
/// tunnel that owned its buffer would reallocate it at each change.
#[test]
fn a_relay_under_many_paced_tunnels_does_not_allocate_per_read() {
    const TUNNELS: usize = 32;
    const COUNTED: u64 = 1024 * 1024 * 1024;
    // The scenario's pacing: each end rests after every burst.
    const REST: Duration = Duration::from_micros(400);

    // Tunnels that have delivered their first bytes, bytes delivered, and
    // senders that could not connect or send: one of those would leave the
    // waits below waiting.
    let (delivered, mut seen) = tokio::sync::watch::channel((0usize, 0u64, 0usize));
    let delivered = Arc::new(delivered);
    let failed = delivered.clone();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Reads a burst, then rests: on threads of its own, as the senders are,
    // so only the proxy's allocations are counted.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let backend = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { return };
            let delivered = delivered.clone();
            std::thread::spawn(move || {
                let mut chunk = vec![0u8; 256 * 1024];
                let (mut first, mut unsaid) = (true, 0u64);
                while let Ok(n @ 1..) = sock.read(&mut chunk) {
                    unsaid += n as u64;
                    // Said a mebibyte at a time: each word wakes the test's
                    // task on the proxy's thread, between two relays.
                    if first || unsaid >= 1024 * 1024 {
                        delivered.send_modify(|(tunnels, bytes, _)| {
                            *tunnels += usize::from(std::mem::take(&mut first));
                            *bytes += std::mem::take(&mut unsaid);
                        });
                    }
                    std::thread::sleep(REST);
                }
            });
        }
    });

    let mut table = RouteTable::new();
    table.add_tcp_route(
        80,
        vec![Backend {
            socket_addr: backend,
            weight: 1,
            filters: vec![],
            use_tls: false,
            server_name: String::new(),
            verify: None,
            action: BackendAction::Forward,
        }],
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let allocations = rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = listener.local_addr().unwrap();
        let ctx = ListenerCtx {
            server_port: 80,
            routes: Arc::new(ArcSwap::from_pointee(table)),
            cfg: ServeConfig::from_perf(&PerformanceConfig::default(), 64),
            plan: Default::default(),
            health: Arc::new(HealthRegistry::new()),
            selector: Arc::new(BackendSelector::new()),
            active_connections: Default::default(),
            shared_pool: None,
        };
        tokio::spawn(listener::serve(listener, ctx, CancellationToken::new()));
        for _ in 0..TUNNELS {
            let (stop, failed) = (stop.clone(), failed.clone());
            std::thread::spawn(move || {
                let stopped = || stop.load(std::sync::atomic::Ordering::Relaxed);
                let chunk = vec![7u8; 64 * 1024];
                let sent = std::net::TcpStream::connect(proxy).and_then(|mut sock| {
                    while !stopped() {
                        sock.write_all(&chunk)?;
                        std::thread::sleep(REST);
                    }
                    Ok(())
                });
                if sent.is_err() && !stopped() {
                    failed.send_modify(|(_, _, failed)| *failed += 1);
                }
            });
        }
        // Every tunnel is open, and each direction has borrowed once.
        let warm = seen
            .wait_for(|(tunnels, _, failed)| *tunnels == TUNNELS || *failed > 0)
            .await;
        let from = warm.unwrap().1;
        let before = ALLOCATIONS.with(Cell::get);
        let moved = seen
            .wait_for(|(_, bytes, failed)| *bytes >= from + COUNTED || *failed > 0)
            .await;
        assert_eq!(moved.unwrap().2, 0, "a sender could not connect or send");
        ALLOCATIONS.with(Cell::get) - before
    });
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    // A relay that allocates per read makes 16 a mebibyte at the least.
    let most = (COUNTED / (1024 * 1024)) as usize;
    assert!(
        allocations < most,
        "{allocations} allocations for {most} MiB"
    );
}
