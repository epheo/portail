//! Heap allocations on the request path, counted where a bench cannot see
//! them: per request, in a flood of the same request.
//!
//! The counting allocator serves the whole test binary. Counts are per
//! thread, so tests running in parallel do not see each other's.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use portail::proxy::health::HealthRegistry;
use portail::proxy::request_processor::{analyze_request, RoutingResult};
use portail::routing::{
    Backend, BackendAction, BackendSelector, HttpRouteRule, PathMatchType, RouteTable,
};

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
                socket_addr: "127.0.0.1:8001".parse().unwrap(),
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
