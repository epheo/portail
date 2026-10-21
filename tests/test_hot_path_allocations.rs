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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use portail::config::PerformanceConfig;
use portail::proxy::health::HealthRegistry;
use portail::proxy::request_processor::{analyze_request, analyze_udp_request, RoutingResult};
use portail::proxy::worker::{run_worker, ListenerCtx, WorkerConfig};
use portail::routing::{
    Backend, BackendAction, BackendSelector, HttpRouteRule, PathMatchType, RouteTable,
};
use tokio_util::sync::CancellationToken;

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
async fn churn(proxy: SocketAddr, n: usize, active: &AtomicUsize) {
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
    while !client.is_finished() || active.load(Ordering::Acquire) > 0 {
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
        let active = Arc::new(AtomicUsize::new(0));
        let ctx = ListenerCtx {
            server_port: 80,
            routes: Arc::new(ArcSwap::from_pointee(table_to(backend))),
            cfg: WorkerConfig::from_perf(&PerformanceConfig::default(), 64),
            plan: Default::default(),
            health: Arc::new(HealthRegistry::new()),
            selector: Arc::new(BackendSelector::new()),
            active_connections: active.clone(),
            shared_pool: None,
        };
        tokio::spawn(run_worker(listener, ctx, CancellationToken::new()));
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
