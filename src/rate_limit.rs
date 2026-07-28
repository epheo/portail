//! Per-client token-bucket rate limiting, attached per listener.
//!
//! A `Limiter` exists per rate-limited listener and lives in a process-wide
//! registry keyed by `(published port, listener name)`: route-table swaps
//! (reconcile, DNS refresh, SIGHUP) re-attach the SAME limiter, so bucket
//! state survives rebuilds the way `metrics::host_requests_counter` slots do.
//! Rate changes update the existing limiter in place rather than replacing
//! it — editing a policy never resets who is currently throttled.
//!
//! The hot path is one `DashMap` read plus one CAS on the client's bucket:
//! a single `AtomicU64` packing (last-update ms, millitokens). No locks are
//! held across the refill computation, and an unlimited listener costs the
//! request exactly one `Option` null check.

use std::net::IpAddr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use dashmap::DashMap;

/// One token = 1000 millitokens; a request costs one token. Millitokens make
/// integer refill exact: at R req/s the bucket gains R millitokens per
/// millisecond, so no fractional accumulator is needed.
const MILLITOKENS_PER_REQUEST: u32 = 1000;

/// Largest expressible burst in whole tokens (u32 millitokens ceiling).
pub const MAX_BURST: u32 = u32::MAX / MILLITOKENS_PER_REQUEST;

/// Sweep cadence. Buckets refilled back to burst carry no information and
/// are dropped; only currently-throttled or recently-active clients occupy
/// memory between sweeps.
pub const SWEEP_INTERVAL_SECS: u64 = 60;

pub struct Limiter {
    /// Requests per second == millitokens per millisecond.
    rate: AtomicU32,
    /// Bucket capacity in millitokens.
    burst_mt: AtomicU32,
    /// Client key -> packed bucket. IPv4 keys by full address, IPv6 by /64
    /// (one prefix per subscriber; per-address v6 buckets are trivially
    /// evaded from a single /64).
    buckets: DashMap<u128, AtomicU64, fnv::FnvBuildHasher>,
    /// Bucket timestamps are u32 milliseconds since this instant. Wrapping
    /// subtraction stays correct across the 49-day wrap for any entry
    /// touched within the sweep interval, and the sweep evicts the rest.
    epoch: Instant,
}

impl Limiter {
    fn new(rps: u32, burst: u32) -> Self {
        Self {
            rate: AtomicU32::new(rps),
            burst_mt: AtomicU32::new(burst.saturating_mul(MILLITOKENS_PER_REQUEST)),
            buckets: DashMap::default(),
            epoch: Instant::now(),
        }
    }

    /// Update rate/burst in place, preserving all bucket state.
    fn reconfigure(&self, rps: u32, burst: u32) {
        self.rate.store(rps, Relaxed);
        self.burst_mt
            .store(burst.saturating_mul(MILLITOKENS_PER_REQUEST), Relaxed);
    }

    /// Whether `peer` may proceed. False means the caller should answer 429.
    pub fn check(&self, peer: IpAddr) -> bool {
        self.check_at(client_key(peer), self.now_ms())
    }

    fn now_ms(&self) -> u32 {
        self.epoch.elapsed().as_millis() as u32
    }

    fn check_at(&self, key: u128, now_ms: u32) -> bool {
        let rate = self.rate.load(Relaxed);
        let burst = self.burst_mt.load(Relaxed);
        if let Some(cell) = self.buckets.get(&key) {
            return consume(&cell, now_ms, rate, burst);
        }
        // First sighting since the last sweep: start from a full bucket.
        // entry() re-checks under the shard lock so a racing first request
        // from the same client lands in the Occupied arm.
        match self.buckets.entry(key) {
            dashmap::mapref::entry::Entry::Occupied(e) => consume(e.get(), now_ms, rate, burst),
            dashmap::mapref::entry::Entry::Vacant(v) => {
                let granted = burst >= MILLITOKENS_PER_REQUEST;
                let tokens = if granted {
                    burst - MILLITOKENS_PER_REQUEST
                } else {
                    burst
                };
                v.insert(AtomicU64::new(pack(now_ms, tokens)));
                granted
            }
        }
    }

    /// Drop buckets that have refilled to capacity — an idle client's bucket
    /// is indistinguishable from no bucket at all.
    fn sweep(&self) {
        let now = self.now_ms();
        let rate = self.rate.load(Relaxed);
        let burst = self.burst_mt.load(Relaxed);
        self.buckets.retain(|_, cell| {
            let (last, tokens) = unpack(cell.load(Relaxed));
            refill(tokens, now.wrapping_sub(last), rate, burst) < burst
        });
    }

    #[cfg(test)]
    fn tracked_clients(&self) -> usize {
        self.buckets.len()
    }
}

impl std::fmt::Debug for Limiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Limiter")
            .field("rate", &self.rate.load(Relaxed))
            .field("burst_mt", &self.burst_mt.load(Relaxed))
            .field("clients", &self.buckets.len())
            .finish()
    }
}

fn pack(now_ms: u32, tokens: u32) -> u64 {
    (u64::from(now_ms) << 32) | u64::from(tokens)
}

fn unpack(state: u64) -> (u32, u32) {
    ((state >> 32) as u32, state as u32)
}

fn refill(tokens: u32, dt_ms: u32, rate: u32, burst: u32) -> u32 {
    // u64 arithmetic: dt (< 2^32) * rate (validated <= 10^6) cannot overflow.
    (u64::from(tokens) + u64::from(dt_ms) * u64::from(rate)).min(u64::from(burst)) as u32
}

fn consume(cell: &AtomicU64, now_ms: u32, rate: u32, burst: u32) -> bool {
    let mut cur = cell.load(Relaxed);
    loop {
        let (last, tokens) = unpack(cur);
        let filled = refill(tokens, now_ms.wrapping_sub(last), rate, burst);
        let (next, granted) = if filled >= MILLITOKENS_PER_REQUEST {
            (pack(now_ms, filled - MILLITOKENS_PER_REQUEST), true)
        } else {
            // Advance the timestamp WITH the partial refill folded in, so
            // accrual is never lost to a denied request.
            (pack(now_ms, filled), false)
        };
        match cell.compare_exchange_weak(cur, next, Relaxed, Relaxed) {
            Ok(_) => return granted,
            Err(observed) => cur = observed,
        }
    }
}

fn client_key(peer: IpAddr) -> u128 {
    match peer {
        IpAddr::V4(v4) => u128::from(u32::from(v4)),
        // /64 prefix; low bits zeroed. Cannot collide with the v4 space
        // (a v4 key has zero high bits, a routable v6 /64 does not).
        IpAddr::V6(v6) => u128::from(v6) & !0u128 << 64,
    }
}

// --- process-wide registry ---

type Registry = Mutex<std::collections::HashMap<(u16, String), Arc<Limiter>>>;

static REGISTRY: OnceLock<Registry> = OnceLock::new();

fn registry() -> &'static Registry {
    REGISTRY.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Get-or-create the limiter for a listener, updating its rate in place on
/// spec changes. Called at route-table build time only, never per request.
pub fn limiter_for(port: u16, listener: &str, rps: u32, burst: u32) -> Arc<Limiter> {
    let mut reg = registry().lock().unwrap();
    let limiter = reg
        .entry((port, listener.to_string()))
        .or_insert_with(|| Arc::new(Limiter::new(rps, burst)));
    limiter.reconfigure(rps, burst);
    Arc::clone(limiter)
}

/// Periodic maintenance: evict refilled buckets, then drop limiters no route
/// table references anymore (policy or listener removed). A limiter dropped
/// by a lost race with a table rebuild is re-created on the next build with
/// empty buckets — harmless.
pub fn spawn_sweeper() {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(SWEEP_INTERVAL_SECS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let mut reg = registry().lock().unwrap();
            for limiter in reg.values() {
                limiter.sweep();
            }
            reg.retain(|_, l| Arc::strong_count(l) > 1);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    const K: u128 = 1;

    #[test]
    fn burst_then_sustained_rate() {
        let l = Limiter::new(2, 3);
        // Full burst up front.
        assert!(l.check_at(K, 0));
        assert!(l.check_at(K, 0));
        assert!(l.check_at(K, 0));
        assert!(!l.check_at(K, 0));
        // 2 rps refill: one token every 500ms.
        assert!(!l.check_at(K, 400));
        assert!(l.check_at(K, 500));
        assert!(!l.check_at(K, 600));
        assert!(l.check_at(K, 1000));
    }

    #[test]
    fn denied_requests_keep_partial_accrual() {
        let l = Limiter::new(1, 1);
        assert!(l.check_at(K, 0));
        // 999ms accrues 999 millitokens across denied probes; the accrual
        // must carry so the token lands at 1000ms sharp.
        for t in (100..=900).step_by(100) {
            assert!(!l.check_at(K, t));
        }
        assert!(l.check_at(K, 1000));
    }

    #[test]
    fn refill_caps_at_burst() {
        let l = Limiter::new(1000, 5);
        assert!(l.check_at(K, 0));
        // A long idle refills to burst, not beyond.
        for _ in 0..5 {
            assert!(l.check_at(K, 3_600_000));
        }
        assert!(!l.check_at(K, 3_600_000));
    }

    #[test]
    fn clients_are_independent() {
        let l = Limiter::new(1, 1);
        assert!(l.check_at(1, 0));
        assert!(!l.check_at(1, 0));
        assert!(l.check_at(2, 0));
    }

    #[test]
    fn ipv6_keys_by_slash64() {
        let a: IpAddr = Ipv6Addr::new(0x2001, 0xdb8, 1, 2, 0, 0, 0, 1).into();
        let b: IpAddr = Ipv6Addr::new(0x2001, 0xdb8, 1, 2, 0xffff, 0, 0, 2).into();
        let c: IpAddr = Ipv6Addr::new(0x2001, 0xdb8, 1, 3, 0, 0, 0, 1).into();
        assert_eq!(client_key(a), client_key(b));
        assert_ne!(client_key(a), client_key(c));
        let v4: IpAddr = Ipv4Addr::new(203, 0, 113, 9).into();
        assert_ne!(client_key(v4), client_key(a));
    }

    #[test]
    fn timestamp_wrap_is_transparent() {
        let l = Limiter::new(2, 2);
        let before_wrap = u32::MAX - 100;
        assert!(l.check_at(K, before_wrap));
        assert!(l.check_at(K, before_wrap));
        assert!(!l.check_at(K, before_wrap));
        // 500ms later, across the u32 wrap: one token accrued.
        assert!(l.check_at(K, before_wrap.wrapping_add(500)));
        assert!(!l.check_at(K, before_wrap.wrapping_add(500)));
    }

    #[test]
    fn sweep_drops_only_refilled_buckets() {
        let l = Limiter::new(1, 10);
        assert!(l.check_at(1, 0)); // refills by t=1000
        assert!(l.check_at(2, 0));
        for _ in 0..9 {
            l.check_at(2, 900); // pinned near empty at t=900
        }
        assert_eq!(l.tracked_clients(), 2);
        // Freeze time via check_at's clock: sweep uses now_ms(), so emulate
        // by reconfigure-free direct retain at a chosen instant.
        let now = 2000u32;
        let rate = l.rate.load(Relaxed);
        let burst = l.burst_mt.load(Relaxed);
        l.buckets.retain(|_, cell| {
            let (last, tokens) = unpack(cell.load(Relaxed));
            refill(tokens, now.wrapping_sub(last), rate, burst) < burst
        });
        assert_eq!(l.tracked_clients(), 1);
    }

    #[test]
    fn reconfigure_preserves_buckets() {
        let l = Limiter::new(1, 1);
        assert!(l.check_at(K, 0));
        assert!(!l.check_at(K, 0));
        l.reconfigure(1, 5);
        // Bucket survives: still empty, not reset to the new burst.
        assert!(!l.check_at(K, 0));
    }

    #[test]
    fn zero_burst_denies_everything() {
        let l = Limiter::new(1, 0);
        assert!(!l.check_at(K, 0));
        assert!(!l.check_at(K, 1_000_000));
    }

    #[test]
    fn registry_reattaches_same_limiter() {
        let a = limiter_for(443, "https-test-reattach", 2, 60);
        assert!(a.check(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7))));
        let b = limiter_for(443, "https-test-reattach", 4, 60);
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(b.rate.load(Relaxed), 4);
    }
}
