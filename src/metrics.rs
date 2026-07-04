//! Zero-dependency process metrics.
//!
//! One static of relaxed `AtomicU64`s, rendered in Prometheus text exposition
//! format by the admin endpoint (`GET /metrics`). No labels, no registry, no
//! crates: a hot-path increment is a single relaxed atomic add, and a scrape
//! is a few hundred bytes of formatting. Histograms are deliberately out of
//! scope for now.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};

pub struct Counter(AtomicU64);

impl Counter {
    pub fn inc(&self) {
        self.0.fetch_add(1, Relaxed);
    }
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Relaxed);
    }
    pub fn get(&self) -> u64 {
        self.0.load(Relaxed)
    }
}

/// Live gauge — incremented and decremented as state comes and goes.
pub struct Gauge(AtomicU64);

impl Gauge {
    pub fn inc(&self) {
        self.0.fetch_add(1, Relaxed);
    }
    pub fn dec(&self) {
        self.0.fetch_sub(1, Relaxed);
    }
    pub fn get(&self) -> u64 {
        self.0.load(Relaxed)
    }
}

macro_rules! metrics {
    ($($name:ident: $help:literal,)+) => {
        pub struct Metrics {
            $(pub $name: Counter,)+
            /// Client connections currently open (mirrors the drain counter).
            pub active_connections: Gauge,
        }

        pub static METRICS: Metrics = Metrics {
            $($name: Counter(AtomicU64::new(0)),)+
            active_connections: Gauge(AtomicU64::new(0)),
        };

        fn render_counters(out: &mut String) {
            use std::fmt::Write;
            $(
                let _ = write!(
                    out,
                    concat!(
                        "# HELP portail_", stringify!($name), " ", $help, "\n",
                        "# TYPE portail_", stringify!($name), " counter\n",
                        "portail_", stringify!($name), " {}\n",
                    ),
                    METRICS.$name.get(),
                );
            )+
        }
    };
}

metrics! {
    connections_accepted_total: "Client connections accepted across all listeners",
    tls_handshake_failures_total: "Client TLS handshakes that failed or timed out",
    http_requests_total: "HTTP requests dispatched to a backend",
    upstream_connects_total: "Backend connections established",
    upstream_connect_errors_total: "Backend connects that failed (client saw 502)",
    upstream_connect_timeouts_total: "Backend connects that timed out (client saw 504)",
    pool_hits_total: "Backend pool checkouts served by an idle connection",
    pool_misses_total: "Backend pool checkouts that had to open a new connection",
    pool_stale_discards_total: "Idle pool connections found dead at checkout and discarded",
    pool_overflow_drops_total: "Connections dropped at check-in because the pool was full",
    pool_stale_retries_total: "Requests replayed on a fresh connection after a pooled one died on send",
    dns_refresh_swaps_total: "DNS refresh passes that installed a changed backend set",
    dns_refresh_superseded_total: "DNS refresh passes dropped because a reconcile swapped concurrently",
    dns_refresh_unchanged_total: "DNS refresh passes with an unchanged backend set",
    backends_marked_unhealthy_total: "Backend health transitions into UNHEALTHY",
    backends_recovered_total: "Backend health recoveries (probe or passive)",
    reconcile_runs_total: "Gateway reconcile passes that ran the apply path",
    reconcile_skips_total: "Gateway reconcile passes short-circuited by the fingerprint gate",
}

/// Per-listener stats, registered by each accept loop when it starts and kept
/// forever (counters must stay monotonic across a rebind of the same port).
/// The hot path touches only its own pre-registered atomics; the registry
/// Mutex is taken at spawn and scrape time only.
pub struct ListenerStats {
    /// TCP connections accepted / UDP datagrams received on this listener.
    pub accepted: AtomicU64,
    /// 1 while the accept loop is alive, 0 once it has exited. This is the
    /// outside-observable answer to "is anything listening on this port" —
    /// the signal that was missing every time a gateway VIP refused while
    /// the process looked healthy.
    up: AtomicU64,
}

/// Flips the listener's `up` gauge to 0 when the accept loop exits, on every
/// exit path (shutdown, error, cancellation). A panic skips the Drop only
/// under `panic = "abort"`, where the whole scrape endpoint dies with it.
pub struct ListenerUpGuard(Arc<ListenerStats>);

impl Drop for ListenerUpGuard {
    fn drop(&mut self) {
        self.0.up.store(0, Relaxed);
    }
}

type ListenerRegistry = Mutex<BTreeMap<(&'static str, u16), Arc<ListenerStats>>>;

static LISTENERS: OnceLock<ListenerRegistry> = OnceLock::new();

fn listeners() -> &'static ListenerRegistry {
    LISTENERS.get_or_init(Default::default)
}

/// Register (or re-activate after a rebind) the stats slot for a listener,
/// keyed on the PUBLISHED port — the Gateway listener identity, not the bound
/// targetPort. Returns the stats handle plus the RAII guard that marks the
/// listener down when the accept loop exits.
pub fn register_listener(proto: &'static str, port: u16) -> (Arc<ListenerStats>, ListenerUpGuard) {
    let mut map = match listeners().lock() {
        Ok(m) => m,
        Err(poisoned) => poisoned.into_inner(),
    };
    let stats = map
        .entry((proto, port))
        .or_insert_with(|| {
            Arc::new(ListenerStats {
                accepted: AtomicU64::new(0),
                up: AtomicU64::new(0),
            })
        })
        .clone();
    stats.up.store(1, Relaxed);
    (stats.clone(), ListenerUpGuard(stats))
}

fn render_listeners(out: &mut String) {
    use std::fmt::Write;
    let map = match listeners().lock() {
        Ok(m) => m,
        Err(poisoned) => poisoned.into_inner(),
    };
    if map.is_empty() {
        return;
    }
    out.push_str(
        "# HELP portail_listener_accepted_total Connections accepted (TCP) or datagrams received (UDP) per listener\n\
         # TYPE portail_listener_accepted_total counter\n",
    );
    for ((proto, port), stats) in map.iter() {
        let _ = writeln!(
            out,
            "portail_listener_accepted_total{{proto=\"{}\",port=\"{}\"}} {}",
            proto,
            port,
            stats.accepted.load(Relaxed),
        );
    }
    out.push_str(
        "# HELP portail_listener_up Listener accept-loop liveness (1 = accepting)\n\
         # TYPE portail_listener_up gauge\n",
    );
    for ((proto, port), stats) in map.iter() {
        let _ = writeln!(
            out,
            "portail_listener_up{{proto=\"{}\",port=\"{}\"}} {}",
            proto,
            port,
            stats.up.load(Relaxed),
        );
    }
}

/// Render everything in Prometheus text exposition format. `ready` is served
/// as a gauge so dashboards can overlay readiness flaps on traffic.
pub fn render(ready: bool) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(4096);
    render_counters(&mut out);
    render_listeners(&mut out);
    let _ = write!(
        out,
        "# HELP portail_active_connections Client connections currently open\n\
         # TYPE portail_active_connections gauge\n\
         portail_active_connections {}\n\
         # HELP portail_ready Whether the data plane has bound its listener ports\n\
         # TYPE portail_ready gauge\n\
         portail_ready {}\n",
        METRICS.active_connections.get(),
        ready as u8,
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_is_valid_exposition_text() {
        METRICS.pool_hits_total.inc();
        METRICS.active_connections.inc();
        let text = render(true);

        // Every metric line references a declared metric with HELP+TYPE.
        assert!(text.contains("# TYPE portail_pool_hits_total counter"));
        assert!(text.contains("# TYPE portail_active_connections gauge"));
        assert!(text.contains("portail_ready 1"));
        let hits: u64 = text
            .lines()
            .find(|l| l.starts_with("portail_pool_hits_total "))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap();
        assert!(hits >= 1);
        // Text format: every non-comment line is "name value".
        for line in text.lines() {
            if !line.starts_with('#') {
                assert_eq!(line.split_whitespace().count(), 2, "bad line: {line}");
            }
        }
        METRICS.active_connections.dec();
    }

    #[test]
    fn listener_registry_renders_and_flips_up() {
        let (stats, guard) = register_listener("tcp", 8123);
        stats.accepted.fetch_add(3, Relaxed);
        let text = render(true);
        assert!(text.contains("portail_listener_accepted_total{proto=\"tcp\",port=\"8123\"} 3"));
        assert!(text.contains("portail_listener_up{proto=\"tcp\",port=\"8123\"} 1"));

        drop(guard);
        let text = render(true);
        assert!(text.contains("portail_listener_up{proto=\"tcp\",port=\"8123\"} 0"));

        // A rebind re-activates the same slot; the counter stays monotonic.
        let (_stats2, _guard2) = register_listener("tcp", 8123);
        let text = render(true);
        assert!(text.contains("portail_listener_up{proto=\"tcp\",port=\"8123\"} 1"));
        assert!(text.contains("portail_listener_accepted_total{proto=\"tcp\",port=\"8123\"} 3"));
    }
}
