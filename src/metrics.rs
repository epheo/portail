//! Zero-dependency process metrics.
//!
//! One static of relaxed `AtomicU64`s, rendered in Prometheus text exposition
//! format by the admin endpoint (`GET /metrics`). No labels, no registry, no
//! crates: a hot-path increment is a single relaxed atomic add, and a scrape
//! is a few hundred bytes of formatting. Histograms are deliberately out of
//! scope for now.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

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

/// Render everything in Prometheus text exposition format. `ready` is served
/// as a gauge so dashboards can overlay readiness flaps on traffic.
pub fn render(ready: bool) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(4096);
    render_counters(&mut out);
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
}
