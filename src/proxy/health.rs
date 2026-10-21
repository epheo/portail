//! Backend health tracking with passive failure detection and active TCP recovery probes.
//!
//! # Design
//!
//! Each backend (`SocketAddr`) has two atomics:
//! - `consecutive_failures`: incremented on each connect failure, reset on success
//! - `status`: one of HEALTHY / UNHEALTHY / PROBING (see constants)
//!
//! State machine:
//! ```text
//! HEALTHY --(>=3 failures)--> UNHEALTHY --(CAS wins)--> PROBING
//!    ^                                                     |
//!    +------ (probe TCP connect OK, or passive success) ---+
//! ```
//!
//! Exactly one probe task runs per unhealthy backend: the `UNHEALTHY->PROBING`
//! compare_exchange ensures only the winning caller spawns the task.
//!
//! A probe never gives up. Unhealthy backends receive no traffic, so no passive
//! failure can ever restart a probe cycle; a bounded probe that exhausted its
//! attempts used to park the backend at 503 until the process restarted, even
//! after it came back (a Service that gained its first endpoints hours later).
//! The probe instead backs off to `PROBE_MAX_INTERVAL` and keeps going until the
//! backend answers or a route-table swap prunes the entry, which aborts it.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use dashmap::DashMap;
use tokio::net::TcpStream;
use tokio::task::AbortHandle;

use crate::logging::{debug, info, warn};
use crate::routing::RouteTable;

// Status constants — module-private; callers use `is_healthy()` only.
const STATUS_HEALTHY: u8 = 0;
const STATUS_UNHEALTHY: u8 = 1;
/// Unhealthy AND a probe task is already running — prevents duplicate spawns.
const STATUS_PROBING: u8 = 2;

/// Number of consecutive failures before a backend is marked unhealthy.
pub const FAILURE_THRESHOLD: u32 = 3;
/// Delay before the first probe; doubles after every failed probe.
const PROBE_INITIAL_INTERVAL: Duration = Duration::from_secs(10);
/// Backoff ceiling. One TCP connect per parked backend per five minutes is the
/// cost of never abandoning one.
const PROBE_MAX_INTERVAL: Duration = Duration::from_secs(300);
/// Timeout for each individual probe TCP connect.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

fn next_probe_interval(current: Duration) -> Duration {
    current.saturating_mul(2).min(PROBE_MAX_INTERVAL)
}

struct BackendHealth {
    /// Reset to 0 on any success. Written with Relaxed — the threshold check
    /// is guarded by the status CAS which provides the necessary ordering.
    consecutive_failures: AtomicU32,
    /// Read with Acquire, written with Release to establish happens-before
    /// between probe success and subsequent `is_healthy` reads.
    status: AtomicU8,
    /// Aborted when the entry leaves the map, so a pruned backend is not
    /// probed forever and a recycled address starts without the old probe.
    probe: Mutex<Option<AbortHandle>>,
}

impl BackendHealth {
    fn healthy() -> Self {
        Self {
            consecutive_failures: AtomicU32::new(0),
            status: AtomicU8::new(STATUS_HEALTHY),
            probe: Mutex::new(None),
        }
    }
}

impl Drop for BackendHealth {
    fn drop(&mut self) {
        let probe = self.probe.get_mut().unwrap_or_else(PoisonError::into_inner);
        if let Some(probe) = probe.take() {
            probe.abort();
        }
    }
}

pub struct HealthRegistry {
    backends: DashMap<SocketAddr, BackendHealth>,
    /// When false, `backends` is empty and the per-request calls skip it:
    /// DashMap's `len()`/`is_empty()` read-lock every shard and `get()` locks
    /// one. Raised after every insert; lowered only by `prune` once the map is
    /// empty, which then looks again and re-raises it for a racing insert. So
    /// no entry outlives a lowered flag; a racing reader at worst sees a
    /// just-inserted entry as untracked, i.e. healthy, as it was before its
    /// first failure. Relaxed throughout: the flag publishes no data, the map's
    /// own locks order the entries.
    tracked: AtomicBool,
}

impl Default for HealthRegistry {
    fn default() -> Self {
        Self {
            backends: DashMap::new(),
            tracked: AtomicBool::new(false),
        }
    }
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop the state of every backend `table` no longer routes to, aborting
    /// its probe. Call after every swap of the live table: nothing else
    /// removes an entry, so under pod churn the map and its probes grew without
    /// bound, and a recycled pod IP inherited the old pod's ejection. Backends
    /// that stay keep their state. A request still running on the previous
    /// table can re-insert a pruned address; the next swap drops it again.
    pub fn prune(&self, table: &RouteTable) {
        if !self.tracked.load(Ordering::Relaxed) {
            return;
        }
        let routed = routed_addrs(table);
        self.backends.retain(|addr, _| routed.contains(addr));
        if self.backends.is_empty() {
            self.tracked.store(false, Ordering::Relaxed);
            // Not redundant: see `tracked`.
            if !self.backends.is_empty() {
                self.tracked.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Returns `true` if the backend is considered healthy.
    /// Backends that have never failed are implicitly healthy (lazy registration).
    #[inline]
    pub fn is_healthy(&self, addr: &SocketAddr) -> bool {
        if !self.tracked.load(Ordering::Relaxed) {
            return true;
        }
        match self.backends.get(addr) {
            None => true,
            Some(entry) => entry.status.load(Ordering::Acquire) == STATUS_HEALTHY,
        }
    }

    /// Called on successful `pool.acquire()` to reset failure state.
    /// No-op (zero allocation, no DashMap access) while no backend is tracked.
    pub fn record_success(&self, addr: SocketAddr) {
        if !self.tracked.load(Ordering::Relaxed) {
            return;
        }
        if let Some(entry) = self.backends.get(&addr) {
            let was_unhealthy = entry.status.load(Ordering::Acquire) != STATUS_HEALTHY;
            if was_unhealthy || entry.consecutive_failures.load(Ordering::Relaxed) > 0 {
                entry.consecutive_failures.store(0, Ordering::Relaxed);
                entry.status.store(STATUS_HEALTHY, Ordering::Release);
                if was_unhealthy {
                    crate::metrics::METRICS.backends_recovered_total.inc();
                }
                info!("Backend {} recovered (passive success)", addr);
            }
        }
        // Not registered = was always healthy, nothing to reset.
    }

    /// Called on `pool.acquire()` failure or timeout.
    ///
    /// Returns `true` if the caller should spawn a recovery probe via
    /// [`HealthRegistry::spawn_probe`]. Separating the decision from the
    /// spawn keeps this method synchronous and trivially testable.
    pub fn record_failure(&self, addr: SocketAddr) -> bool {
        let entry = self
            .backends
            .entry(addr)
            .or_insert_with(BackendHealth::healthy);
        // After the insert, never before: see `tracked`. Loaded first so a
        // failure storm does not keep invalidating the line every request reads.
        if !self.tracked.load(Ordering::Relaxed) {
            self.tracked.store(true, Ordering::Relaxed);
        }

        let new_count = entry.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        warn!("Backend {} failure #{}", addr, new_count);

        if new_count >= FAILURE_THRESHOLD {
            // Transition HEALTHY → UNHEALTHY if not already done.
            // We use CAS (not swap) so that STATUS_PROBING is never overwritten —
            // if a probe is already running, this CAS fails silently and we fall
            // through to the second CAS which will also fail, returning false.
            if entry
                .status
                .compare_exchange(
                    STATUS_HEALTHY,
                    STATUS_UNHEALTHY,
                    Ordering::Release,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                crate::metrics::METRICS
                    .backends_marked_unhealthy_total
                    .inc();
                warn!(
                    "Backend {} marked UNHEALTHY after {} consecutive failures",
                    addr, new_count
                );
            }

            // Exactly one caller wins UNHEALTHY → PROBING and should spawn a probe.
            // If status is already PROBING (probe running) or HEALTHY (recovered),
            // this CAS fails and we return false.
            // NOTE: the DashMap entry guard drops here (before the caller spawns),
            // so no shard lock is held across the async spawn boundary.
            return entry
                .status
                .compare_exchange(
                    STATUS_UNHEALTHY,
                    STATUS_PROBING,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_ok();
        }

        false
    }

    /// Spawn a background Tokio task that probes `addr` until it recovers.
    /// Call this only when [`record_failure`] returns `true`.
    pub fn spawn_probe(registry: Arc<Self>, addr: SocketAddr) {
        info!("Spawning recovery probe for backend {}", addr);
        let probe = tokio::spawn(Self::run_probe(Arc::clone(&registry), addr)).abort_handle();
        match registry.backends.get(&addr) {
            Some(entry) => {
                let mut slot = entry.probe.lock().unwrap_or_else(PoisonError::into_inner);
                // A probe from an earlier episode, still asleep after a passive
                // recovery, would otherwise keep probing beside this one.
                if let Some(stale) = slot.replace(probe) {
                    stale.abort();
                }
            }
            // Pruned since the failure that asked for this probe.
            None => probe.abort(),
        }
    }

    async fn run_probe(registry: Arc<Self>, addr: SocketAddr) {
        let mut interval = PROBE_INITIAL_INTERVAL;
        let mut attempt: u32 = 0;
        loop {
            tokio::time::sleep(interval).await;
            attempt += 1;

            // Pruned (its abort may not have landed yet), or an in-flight
            // request already recorded a passive success: nothing to probe.
            let still_probing = registry
                .backends
                .get(&addr)
                .is_some_and(|e| e.status.load(Ordering::Acquire) == STATUS_PROBING);
            if !still_probing {
                debug!(
                    "Probe for backend {} stopped: entry gone or already healthy",
                    addr
                );
                return;
            }

            debug!("Probe attempt {} for backend {}", attempt, addr);

            let connected = tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect(addr))
                .await
                .is_ok_and(|r| r.is_ok());

            if connected {
                if let Some(entry) = registry.backends.get(&addr) {
                    entry.consecutive_failures.store(0, Ordering::Relaxed);
                    // CAS, not store: a passive success may have flipped the
                    // status already, and the metric must count a recovery once.
                    if entry
                        .status
                        .compare_exchange(
                            STATUS_PROBING,
                            STATUS_HEALTHY,
                            Ordering::AcqRel,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                    {
                        crate::metrics::METRICS.backends_recovered_total.inc();
                        info!(
                            "Backend {} recovered after {} probe attempt(s)",
                            addr, attempt
                        );
                    }
                }
                return;
            }

            interval = next_probe_interval(interval);
            warn!(
                "Probe {} failed for backend {}; next probe in {:?}",
                attempt, addr, interval
            );
        }
    }
}

/// Every backend address `table` routes to. Must cover each backend list
/// `RouteTable::dns_signature` covers, or every swap would wipe the state of
/// the backends it misses.
fn routed_addrs(table: &RouteTable) -> HashSet<SocketAddr> {
    let http = table.http_scopes().flat_map(|scope| {
        scope
            .http_routes
            .values()
            .chain(scope.wildcard_http_routes.values())
            .chain(scope.catch_all_http_routes.iter())
            .flatten()
            .flat_map(|rule| &rule.backends)
    });
    let l4 = table
        .tcp_routes
        .values()
        .chain(table.udp_routes.values())
        .chain(
            table
                .tls_scopes
                .values()
                .flatten()
                .flat_map(crate::routing::TlsScope::backends),
        )
        .flatten();
    http.chain(l4).map(|b| b.socket_addr).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::{Backend, HttpRouteRule, PathMatchType};

    fn addr(port: u16) -> SocketAddr {
        format!("127.0.0.1:{}", port).parse().unwrap()
    }

    #[test]
    fn test_default_healthy() {
        let r = HealthRegistry::new();
        assert!(r.is_healthy(&addr(8080)));
    }

    #[test]
    fn test_record_success_no_prior_failure() {
        let r = HealthRegistry::new();
        r.record_success(addr(8080));
        assert!(r.is_healthy(&addr(8080)));
        // Should not create an entry (lazy registration)
        assert!(r.backends.get(&addr(8080)).is_none());
    }

    #[test]
    fn test_below_threshold_stays_healthy() {
        let r = HealthRegistry::new();
        let a = addr(8081);
        for _ in 0..(FAILURE_THRESHOLD - 1) {
            assert!(!r.record_failure(a));
        }
        assert!(r.is_healthy(&a));
    }

    #[tokio::test]
    async fn test_threshold_marks_unhealthy() {
        let r = HealthRegistry::new();
        let a = addr(8082);
        for _ in 0..(FAILURE_THRESHOLD - 1) {
            r.record_failure(a);
        }
        assert!(r.is_healthy(&a));

        let should_spawn = r.record_failure(a);
        assert!(!r.is_healthy(&a));
        // Exactly one caller should win the probe CAS
        assert!(should_spawn);
    }

    #[tokio::test]
    async fn test_only_one_probe_spawned() {
        let r = HealthRegistry::new();
        let a = addr(8083);
        // Cross the threshold
        for _ in 0..(FAILURE_THRESHOLD - 1) {
            r.record_failure(a);
        }
        let first = r.record_failure(a); // crosses threshold, CAS wins
        let second = r.record_failure(a); // status already PROBING, CAS fails
        assert!(first);
        assert!(!second);
    }

    #[test]
    fn test_success_resets_failure_count() {
        let r = HealthRegistry::new();
        let a = addr(8084);
        r.record_failure(a);
        r.record_failure(a);
        // Below threshold — still healthy
        assert!(r.is_healthy(&a));

        r.record_success(a);
        let count = r
            .backends
            .get(&a)
            .map(|e| e.consecutive_failures.load(Ordering::Relaxed))
            .unwrap_or(0);
        assert_eq!(count, 0);
    }

    #[test]
    fn test_tracked_flag_follows_entries() {
        let r = HealthRegistry::new();
        let a = addr(8086);
        r.record_success(a);
        assert!(
            !r.tracked.load(Ordering::Relaxed),
            "a success alone tracks nothing"
        );
        r.record_failure(a);
        assert!(r.tracked.load(Ordering::Relaxed));
        r.prune(&RouteTable::new());
        assert!(!r.tracked.load(Ordering::Relaxed));
        assert!(r.is_healthy(&a));
    }

    /// Whatever the interleaving, an entry left in the map must sit behind a
    /// raised flag, or an unhealthy backend would read healthy indefinitely.
    /// Each round races ONE failure against ONE prune, so no later failure
    /// can re-raise the flag and mask a bad interleaving.
    #[test]
    fn test_no_entry_survives_behind_lowered_flag() {
        use std::sync::Barrier;
        const ROUNDS: usize = 20_000;
        let r = Arc::new(HealthRegistry::new());
        let start = Arc::new(Barrier::new(3));
        let done = Arc::new(Barrier::new(3));
        let racer = |op: fn(&HealthRegistry)| {
            let (r, start, done) = (Arc::clone(&r), Arc::clone(&start), Arc::clone(&done));
            std::thread::spawn(move || {
                for _ in 0..ROUNDS {
                    start.wait();
                    op(&r);
                    done.wait();
                }
            })
        };
        let writer = racer(|r| {
            r.record_failure(addr(8087));
        });
        let pruner = racer(|r| r.prune(&RouteTable::new()));
        for _ in 0..ROUNDS {
            start.wait();
            done.wait();
            if !r.backends.is_empty() {
                assert!(r.tracked.load(Ordering::Relaxed));
            }
        }
        writer.join().unwrap();
        pruner.join().unwrap();
    }

    #[test]
    fn test_probe_interval_doubles_then_caps() {
        assert_eq!(
            next_probe_interval(PROBE_INITIAL_INTERVAL),
            PROBE_INITIAL_INTERVAL * 2
        );
        let mut d = PROBE_INITIAL_INTERVAL;
        for _ in 0..16 {
            d = next_probe_interval(d);
        }
        assert_eq!(d, PROBE_MAX_INTERVAL);
        assert_eq!(next_probe_interval(PROBE_MAX_INTERVAL), PROBE_MAX_INTERVAL);
    }

    /// Mark `a` unhealthy and win the probe CAS, as a worker would.
    fn park(r: &HealthRegistry, a: SocketAddr) {
        for _ in 0..(FAILURE_THRESHOLD - 1) {
            r.record_failure(a);
        }
        assert!(r.record_failure(a));
    }

    fn tcp_table(ports: &[u16]) -> RouteTable {
        let mut t = RouteTable::new();
        let backends = ports.iter().map(|&p| backend(p)).collect();
        t.add_tcp_route(7000, backends);
        t
    }

    fn backend(port: u16) -> Backend {
        Backend::new("127.0.0.1".into(), port).unwrap()
    }

    #[test]
    fn test_prune_drops_only_departed_backends() {
        let r = HealthRegistry::new();
        let (gone, kept) = (addr(8088), addr(8089));
        park(&r, gone);
        park(&r, kept);
        r.prune(&tcp_table(&[8089]));
        assert!(r.backends.get(&gone).is_none());
        assert!(
            !r.is_healthy(&kept),
            "a surviving backend keeps its ejection"
        );
        assert!(r.tracked.load(Ordering::Relaxed));
    }

    /// A pod IP recycled to a new pod must not inherit the old pod's ejection.
    #[test]
    fn test_recycled_address_starts_healthy() {
        let r = HealthRegistry::new();
        let a = addr(8090);
        park(&r, a);
        r.prune(&tcp_table(&[]));
        assert!(
            !r.tracked.load(Ordering::Relaxed),
            "an emptied map lowers the flag"
        );
        r.prune(&tcp_table(&[8090]));
        assert!(r.is_healthy(&a));
    }

    #[test]
    fn test_prune_keeps_backends_of_every_route_kind() {
        let mut t = RouteTable::new();
        for (host, port) in [
            ("a.example.com", 8101),
            ("*.example.net", 8102),
            ("*", 8103),
        ] {
            t.add_http_route(
                host,
                HttpRouteRule::new(
                    PathMatchType::Prefix,
                    "/".into(),
                    vec![],
                    vec![],
                    vec![],
                    vec![backend(port)],
                ),
            );
        }
        t.add_tcp_route(7001, vec![backend(8104)]);
        t.add_udp_route(7002, vec![backend(8105)]);
        t.add_tls_route(7003, "b.example.com", vec![backend(8106)]);
        t.add_tls_route(7003, "*.example.org", vec![backend(8107)]);

        let r = HealthRegistry::new();
        for port in 8101..=8108 {
            park(&r, addr(port));
        }
        r.prune(&t);
        for port in 8101..=8107 {
            assert!(!r.is_healthy(&addr(port)), "backend on {} was pruned", port);
        }
        assert!(r.backends.get(&addr(8108)).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn test_prune_aborts_probe() {
        let r = Arc::new(HealthRegistry::new());
        let a = addr(8091);
        park(&r, a);
        HealthRegistry::spawn_probe(Arc::clone(&r), a);
        r.prune(&RouteTable::new());
        // The sleeping probe holds the only other reference. Time cannot
        // advance while this task keeps yielding, so only an abort frees it.
        for _ in 0..10 {
            if Arc::strong_count(&r) == 1 {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("probe still running after its entry was pruned");
    }

    #[tokio::test(start_paused = true)]
    async fn test_probe_stops_when_entry_pruned() {
        let r = Arc::new(HealthRegistry::new());
        let a = addr(8085);
        park(&r, a);
        r.prune(&RouteTable::new());
        // Paused time auto-advances through the probe sleeps; the loop must
        // notice the missing entry and return instead of probing forever.
        tokio::time::timeout(
            PROBE_MAX_INTERVAL,
            HealthRegistry::run_probe(Arc::clone(&r), a),
        )
        .await
        .expect("probe loop must exit once its entry is gone");
    }

    #[tokio::test(start_paused = true)]
    async fn test_probe_recovers_backend_that_came_back_late() {
        // Bound but not listening: probes are refused, and no other process
        // can take the port before the backend comes back on it.
        let socket =
            socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
        socket
            .bind(&"127.0.0.1:0".parse::<SocketAddr>().unwrap().into())
            .unwrap();
        let a = socket.local_addr().unwrap().as_socket().unwrap();

        let r = Arc::new(HealthRegistry::new());
        park(&r, a);
        assert!(!r.is_healthy(&a));

        let probe = tokio::spawn(HealthRegistry::run_probe(Arc::clone(&r), a));

        // Let more probes fail than the old bounded loop allowed before the
        // backend finally appears.
        tokio::time::sleep(PROBE_MAX_INTERVAL * 40).await;
        assert!(!r.is_healthy(&a));
        assert!(!probe.is_finished(), "probe must not give up");

        socket.listen(1).unwrap();
        socket.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(socket.into()).unwrap();
        tokio::time::timeout(PROBE_MAX_INTERVAL * 2, probe)
            .await
            .expect("probe must succeed once the backend listens")
            .unwrap();
        drop(listener);
        assert!(r.is_healthy(&a));
    }
}
