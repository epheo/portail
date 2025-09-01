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
//! HEALTHY ──(≥3 failures)──► UNHEALTHY ──(CAS wins)──► PROBING
//!    ▲                                                      │
//!    └──────────────── (probe TCP connect OK) ──────────────┘
//!                                                      │
//!          UNHEALTHY ◄────── (probe exhausted) ─────────┘
//! ```
//!
//! Exactly one probe task runs per unhealthy backend: the `UNHEALTHY→PROBING`
//! compare_exchange ensures only the winning caller spawns the task.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use tokio::net::TcpStream;

use crate::logging::{debug, info, warn};

// Status constants — module-private; callers use `is_healthy()` only.
const STATUS_HEALTHY: u8 = 0;
const STATUS_UNHEALTHY: u8 = 1;
/// Unhealthy AND a probe task is already running — prevents duplicate spawns.
const STATUS_PROBING: u8 = 2;

/// Number of consecutive failures before a backend is marked unhealthy.
pub const FAILURE_THRESHOLD: u32 = 3;
/// Time between probe TCP connect attempts.
const PROBE_INTERVAL: Duration = Duration::from_secs(10);
/// Timeout for each individual probe TCP connect.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// After this many failed probe attempts, revert to UNHEALTHY so a future
/// passive failure can re-trigger a new probe cycle.
const MAX_PROBE_ATTEMPTS: u32 = 30;

struct BackendHealth {
    /// Reset to 0 on any success. Written with Relaxed — the threshold check
    /// is guarded by the status CAS which provides the necessary ordering.
    consecutive_failures: AtomicU32,
    /// Read with Acquire, written with Release to establish happens-before
    /// between probe success and subsequent `is_healthy` reads.
    status: AtomicU8,
}

impl BackendHealth {
    fn healthy() -> Self {
        Self {
            consecutive_failures: AtomicU32::new(0),
            status: AtomicU8::new(STATUS_HEALTHY),
        }
    }
}

pub struct HealthRegistry {
    backends: DashMap<SocketAddr, BackendHealth>,
}

impl Default for HealthRegistry {
    fn default() -> Self {
        Self {
            backends: DashMap::new(),
        }
    }
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Remove all tracked backend health state.
    /// Called on config reload to prevent stale entries from blocking new backends.
    pub fn clear(&self) {
        self.backends.clear();
    }

    /// Returns `true` if the backend is considered healthy.
    /// Backends that have never failed are implicitly healthy (lazy registration).
    #[inline]
    pub fn is_healthy(&self, addr: &SocketAddr) -> bool {
        match self.backends.get(addr) {
            None => true,
            Some(entry) => entry.status.load(Ordering::Acquire) == STATUS_HEALTHY,
        }
    }

    /// Called on successful `pool.acquire()` to reset failure state.
    /// No-op (zero allocation, no DashMap access) if no backends have ever failed.
    pub fn record_success(&self, addr: SocketAddr) {
        // Fast path: if no backend has ever failed, skip the DashMap lookup entirely.
        // DashMap::is_empty() is lock-free and O(1).
        if self.backends.is_empty() {
            return;
        }
        if let Some(entry) = self.backends.get(&addr) {
            if entry.consecutive_failures.load(Ordering::Relaxed) > 0
                || entry.status.load(Ordering::Acquire) != STATUS_HEALTHY
            {
                entry.consecutive_failures.store(0, Ordering::Relaxed);
                entry.status.store(STATUS_HEALTHY, Ordering::Release);
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
        tokio::spawn(async move {
            Self::run_probe(registry, addr).await;
        });
    }

    async fn run_probe(registry: Arc<Self>, addr: SocketAddr) {
        for attempt in 1..=MAX_PROBE_ATTEMPTS {
            tokio::time::sleep(PROBE_INTERVAL).await;

            debug!(
                "Probe attempt {}/{} for backend {}",
                attempt, MAX_PROBE_ATTEMPTS, addr
            );

            let connected = tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect(addr))
                .await
                .is_ok_and(|r| r.is_ok());

            if connected {
                info!(
                    "Backend {} recovered after {} probe attempt(s)",
                    addr, attempt
                );
                if let Some(entry) = registry.backends.get(&addr) {
                    entry.consecutive_failures.store(0, Ordering::Relaxed);
                    // Release: subsequent Acquire reads of STATUS_HEALTHY will see
                    // all prior stores (including the failure counter reset).
                    entry.status.store(STATUS_HEALTHY, Ordering::Release);
                }
                return;
            }

            warn!(
                "Probe {}/{} failed for backend {}",
                attempt, MAX_PROBE_ATTEMPTS, addr
            );
        }

        // All attempts exhausted — revert PROBING → UNHEALTHY so a future
        // passive failure can trigger a fresh probe cycle.
        warn!(
            "Backend {} probe exhausted after {} attempts",
            addr, MAX_PROBE_ATTEMPTS
        );
        if let Some(entry) = registry.backends.get(&addr) {
            let _ = entry.status.compare_exchange(
                STATUS_PROBING,
                STATUS_UNHEALTHY,
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
