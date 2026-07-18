//! Zero-dependency process metrics.
//!
//! One static of relaxed `AtomicU64`s, rendered in Prometheus text exposition
//! format by the admin endpoint (`GET /metrics`). No labels, no registry, no
//! crates: a hot-path increment is a single relaxed atomic add, and a scrape
//! is a few hundred bytes of formatting. Histograms are fixed-bucket atomic
//! arrays: an observe is one clock read plus three relaxed adds; cumulation
//! and float formatting happen only at scrape time.

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
    access_log_dropped_total: "Access log lines dropped because the writer lagged or its sink failed",
    connections_accepted_total: "Client connections accepted across all listeners",
    tls_handshake_failures_total: "Client TLS handshakes that failed or timed out",
    upstream_connects_total: "Backend connections established",
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

// --- Latency histograms ----------------------------------------------------

/// Bucket bounds shared by every histogram. Log-scale from 10us (a pooled
/// local-backend hop) to 60s (past every default timeout: backendTimeout 30s,
/// so a timed-out request lands in a finite bucket, not just +Inf). Nanos for
/// the hot-path compare, the label strings precomputed for the scrape.
const LE_NANOS: [u64; 21] = [
    10_000,
    25_000,
    50_000,
    100_000,
    250_000,
    500_000,
    1_000_000,
    2_500_000,
    5_000_000,
    10_000_000,
    25_000_000,
    50_000_000,
    100_000_000,
    250_000_000,
    500_000_000,
    1_000_000_000,
    2_500_000_000,
    5_000_000_000,
    10_000_000_000,
    30_000_000_000,
    60_000_000_000,
];
const LE_LABELS: [&str; 21] = [
    "0.00001", "0.000025", "0.00005", "0.0001", "0.00025", "0.0005", "0.001", "0.0025", "0.005",
    "0.01", "0.025", "0.05", "0.1", "0.25", "0.5", "1", "2.5", "5", "10", "30", "60",
];

/// Fixed-bucket latency histogram. Buckets store per-bucket (non-cumulative)
/// counts; the Prometheus cumulative form is computed at scrape time so the
/// hot path never touches more than one bucket. Samples above the last bound
/// exist only in `count` (rendered as the +Inf bucket). Sum is kept in nanos:
/// u64 overflows after ~584 years of accumulated latency.
pub struct Histogram {
    buckets: [AtomicU64; LE_NANOS.len()],
    sum_nanos: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    const fn new() -> Self {
        Self {
            buckets: [const { AtomicU64::new(0) }; LE_NANOS.len()],
            sum_nanos: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    pub fn observe(&self, d: std::time::Duration) {
        let nanos = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        // Count before bucket: relaxed ordering gives a concurrent scrape no
        // cross-variable guarantee, but keeping the count add first makes the
        // common interleaving land the sample in +Inf rather than rendering a
        // cumulative bucket above the count (render clamps the rest).
        self.count.fetch_add(1, Relaxed);
        self.sum_nanos.fetch_add(nanos, Relaxed);
        if let Some(idx) = LE_NANOS.iter().position(|&le| nanos <= le) {
            self.buckets[idx].fetch_add(1, Relaxed);
        }
    }
}

/// Request-headers-complete to response flushed to the client, for proxied
/// HTTP requests. Upgrade tunnels are excluded: a WebSocket's lifetime is a
/// connection duration, not a request latency, and one long tunnel would
/// dominate the sum forever.
pub static REQUEST_DURATION: Histogram = Histogram::new();

/// Route match to backend response headers received: pool acquire, connect,
/// request forward (including the client's body), and backend processing.
pub static UPSTREAM_TTFB: Histogram = Histogram::new();

fn render_histogram(out: &mut String, name: &str, help: &str, h: &Histogram) {
    use std::fmt::Write;
    let _ = writeln!(out, "# HELP portail_{} {}", name, help);
    let _ = writeln!(out, "# TYPE portail_{} histogram", name);
    let mut cum = 0u64;
    for (label, bucket) in LE_LABELS.iter().zip(&h.buckets) {
        cum += bucket.load(Relaxed);
        let _ = writeln!(out, "portail_{}_bucket{{le=\"{}\"}} {}", name, label, cum);
    }
    // Clamp: a scrape racing an observe can see a bucket add whose count add
    // is not yet visible; +Inf (== count) must stay >= the last finite bucket.
    let count = h.count.load(Relaxed).max(cum);
    let _ = writeln!(out, "portail_{}_bucket{{le=\"+Inf\"}} {}", name, count);
    let _ = writeln!(
        out,
        "portail_{}_sum {}",
        name,
        h.sum_nanos.load(Relaxed) as f64 / 1e9
    );
    let _ = writeln!(out, "portail_{}_count {}", name, count);
}

// --- Labeled counter families ---------------------------------------------
//
// `portail_http_requests_total{host=...}` and the `upstream_connect_*`
// families replace their former unlabeled process-global counters: every
// one of those events has exactly one owner (the matched route's hostname,
// the configured backend), so the labeled series partition the old totals
// and `sum()` reproduces them. Label values come from CONFIG, never from
// client input: the matched route's configured hostname — not the raw Host
// header, which is attacker-controlled and unbounded — and the configured
// backend name:port — not the resolved pod IP, which churns with every DNS
// refresh. Entries live for the process lifetime, bounded by the set of
// hostnames/backends ever configured.

type LabeledFamily = Mutex<BTreeMap<String, Arc<AtomicU64>>>;

static HOST_REQUESTS: OnceLock<LabeledFamily> = OnceLock::new();
static BACKEND_CONNECT_ERRORS: OnceLock<LabeledFamily> = OnceLock::new();
static BACKEND_CONNECT_TIMEOUTS: OnceLock<LabeledFamily> = OnceLock::new();

fn family_counter(family: &'static OnceLock<LabeledFamily>, key: &str) -> Arc<AtomicU64> {
    let mut map = match family.get_or_init(Default::default).lock() {
        Ok(m) => m,
        Err(poisoned) => poisoned.into_inner(),
    };
    match map.get(key) {
        Some(counter) => counter.clone(),
        None => {
            let counter = Arc::new(AtomicU64::new(0));
            map.insert(key.to_string(), counter.clone());
            counter
        }
    }
}

/// Request counter for a route's configured hostname ("*" for catch-all
/// routes). Fetched once at route-table build and attached to each rule —
/// the per-request cost stays one relaxed add, no lookup, and the counter
/// survives table swaps (reconciles, DNS refreshes) monotonically.
pub fn host_requests_counter(host: &str) -> Arc<AtomicU64> {
    family_counter(&HOST_REQUESTS, host)
}

/// Attribute a backend connect failure (client saw 502) to the configured
/// backend. A registry lookup per call is fine on this path: connect
/// errors are rare by definition and already cost a log line + health hit.
pub fn record_backend_connect_error(backend: &str) {
    family_counter(&BACKEND_CONNECT_ERRORS, backend).fetch_add(1, Relaxed);
}

/// Attribute a backend connect timeout (client saw 504) to the configured
/// backend.
pub fn record_backend_connect_timeout(backend: &str) {
    family_counter(&BACKEND_CONNECT_TIMEOUTS, backend).fetch_add(1, Relaxed);
}

/// Prometheus label values must escape backslash, double-quote and newline.
/// Config-sourced hostnames and backend names never contain them in
/// practice, but a malformed config must corrupt one label, not the scrape.
fn escape_label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn render_labeled_family(
    out: &mut String,
    family: &'static OnceLock<LabeledFamily>,
    name: &str,
    label: &str,
    help: &str,
) {
    use std::fmt::Write;
    let map = match family.get_or_init(Default::default).lock() {
        Ok(m) => m,
        Err(poisoned) => poisoned.into_inner(),
    };
    if map.is_empty() {
        return;
    }
    let _ = writeln!(out, "# HELP portail_{} {}", name, help);
    let _ = writeln!(out, "# TYPE portail_{} counter", name);
    for (key, counter) in map.iter() {
        let _ = writeln!(
            out,
            "portail_{}{{{}=\"{}\"}} {}",
            name,
            label,
            escape_label(key),
            counter.load(Relaxed),
        );
    }
}

/// Per-listener stats, registered by each accept loop when it starts and kept
/// forever (counters must stay monotonic across a rebind of the same port).
/// The hot path touches only its own pre-registered atomics — one relaxed
/// add per event, the same cost class as the global counters that already
/// fire alongside; the registry Mutex is taken at spawn and scrape time only.
///
/// Keyed on (proto, published port): the data plane's real identity. On a
/// shared port (several SNI-scoped listeners on :443) the samples are the
/// port's aggregate — per-hostname attribution would cost a lookup per
/// request and unbounded label cardinality. Pool metrics stay global only:
/// a process-scoped pool serves every listener, so per-port pool numbers
/// would lie.
#[derive(Default)]
pub struct ListenerStats {
    /// TCP connections accepted / UDP datagrams received on this listener.
    pub accepted: AtomicU64,
    /// Client connections currently open on this listener.
    pub active: AtomicU64,
    /// HTTP requests dispatched to a backend from this listener.
    pub http_requests: AtomicU64,
    /// Client TLS handshakes that failed or timed out on this listener.
    pub tls_handshake_failures: AtomicU64,
    /// Backend connects that failed (client saw 502) for this listener's requests.
    pub upstream_connect_errors: AtomicU64,
    /// Backend connects that timed out (client saw 504) for this listener's requests.
    pub upstream_connect_timeouts: AtomicU64,
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
        .or_insert_with(|| Arc::new(ListenerStats::default()))
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
    // One HELP/TYPE block per series, one sample per registered listener.
    // UDP listeners render zeros for the TCP-only series; a constant zero
    // costs nothing and keeps the schema uniform across protos.
    type Get = fn(&ListenerStats) -> u64;
    let series: [(&str, &str, &str, Get); 7] = [
        (
            "listener_accepted_total",
            "Connections accepted (TCP) or datagrams received (UDP) per listener",
            "counter",
            |s| s.accepted.load(Relaxed),
        ),
        (
            "listener_active_connections",
            "Client connections currently open per listener",
            "gauge",
            |s| s.active.load(Relaxed),
        ),
        (
            "listener_http_requests_total",
            "HTTP requests dispatched to a backend per listener",
            "counter",
            |s| s.http_requests.load(Relaxed),
        ),
        (
            "listener_tls_handshake_failures_total",
            "Client TLS handshakes that failed or timed out per listener",
            "counter",
            |s| s.tls_handshake_failures.load(Relaxed),
        ),
        (
            "listener_upstream_connect_errors_total",
            "Backend connects that failed (client saw 502) per listener",
            "counter",
            |s| s.upstream_connect_errors.load(Relaxed),
        ),
        (
            "listener_upstream_connect_timeouts_total",
            "Backend connects that timed out (client saw 504) per listener",
            "counter",
            |s| s.upstream_connect_timeouts.load(Relaxed),
        ),
        (
            "listener_up",
            "Listener accept-loop liveness (1 = accepting)",
            "gauge",
            |s| s.up.load(Relaxed),
        ),
    ];
    for (name, help, kind, get) in series {
        let _ = writeln!(out, "# HELP portail_{} {}", name, help);
        let _ = writeln!(out, "# TYPE portail_{} {}", name, kind);
        for ((proto, port), stats) in map.iter() {
            let _ = writeln!(
                out,
                "portail_{}{{proto=\"{}\",port=\"{}\"}} {}",
                name,
                proto,
                port,
                get(stats),
            );
        }
    }
}

/// Render everything in Prometheus text exposition format. `ready` is served
/// as a gauge so dashboards can overlay readiness flaps on traffic.
pub fn render(ready: bool) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(4096);
    render_counters(&mut out);
    render_labeled_family(
        &mut out,
        &HOST_REQUESTS,
        "http_requests_total",
        "host",
        "HTTP requests dispatched to a backend, by the matched route's configured hostname",
    );
    render_labeled_family(
        &mut out,
        &BACKEND_CONNECT_ERRORS,
        "upstream_connect_errors_total",
        "backend",
        "Backend connects that failed (client saw 502), by configured backend",
    );
    render_labeled_family(
        &mut out,
        &BACKEND_CONNECT_TIMEOUTS,
        "upstream_connect_timeouts_total",
        "backend",
        "Backend connects that timed out (client saw 504), by configured backend",
    );
    render_listeners(&mut out);
    render_histogram(
        &mut out,
        "request_duration_seconds",
        "Proxied HTTP request duration, headers-complete to response flushed (upgrade tunnels excluded)",
        &REQUEST_DURATION,
    );
    render_histogram(
        &mut out,
        "upstream_ttfb_seconds",
        "Route match to backend response headers received (pool acquire, connect, request forward, backend processing)",
        &UPSTREAM_TTFB,
    );
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
    fn histogram_buckets_cumulate_and_sum() {
        use std::time::Duration;
        let h = Histogram::new();
        h.observe(Duration::from_micros(5)); // below first bound -> le=0.00001
        h.observe(Duration::from_micros(300)); // -> le=0.0005
        h.observe(Duration::from_millis(40)); // -> le=0.05
        h.observe(Duration::from_secs(120)); // above last bound -> +Inf only

        let mut out = String::new();
        render_histogram(&mut out, "test_seconds", "help", &h);

        assert!(out.contains("# TYPE portail_test_seconds histogram"));
        assert!(out.contains("portail_test_seconds_bucket{le=\"0.00001\"} 1"));
        // Cumulative: the 300us sample joins the 5us one at le=0.0005.
        assert!(out.contains("portail_test_seconds_bucket{le=\"0.0005\"} 2"));
        assert!(out.contains("portail_test_seconds_bucket{le=\"0.05\"} 3"));
        assert!(out.contains("portail_test_seconds_bucket{le=\"60\"} 3"));
        assert!(out.contains("portail_test_seconds_bucket{le=\"+Inf\"} 4"));
        assert!(out.contains("portail_test_seconds_count 4"));
        let sum: f64 = out
            .lines()
            .find(|l| l.starts_with("portail_test_seconds_sum "))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap();
        assert!((sum - 120.040305).abs() < 1e-6, "sum was {sum}");
        // Buckets never decrease and never exceed +Inf.
        let mut prev = 0u64;
        for line in out.lines().filter(|l| l.contains("_bucket")) {
            let v: u64 = line.split_whitespace().nth(1).unwrap().parse().unwrap();
            assert!(v >= prev, "non-monotonic bucket line: {line}");
            prev = v;
        }
    }

    #[test]
    fn request_histograms_render_in_scrape() {
        REQUEST_DURATION.observe(std::time::Duration::from_millis(2));
        UPSTREAM_TTFB.observe(std::time::Duration::from_millis(1));
        let text = render(true);
        assert!(text.contains("# TYPE portail_request_duration_seconds histogram"));
        assert!(text.contains("# TYPE portail_upstream_ttfb_seconds histogram"));
        assert!(text.contains("portail_request_duration_seconds_bucket{le=\"+Inf\"}"));
    }

    #[test]
    fn listener_registry_renders_and_flips_up() {
        let (stats, guard) = register_listener("tcp", 8123);
        stats.accepted.fetch_add(3, Relaxed);
        stats.http_requests.fetch_add(2, Relaxed);
        stats.upstream_connect_errors.fetch_add(1, Relaxed);
        let text = render(true);
        assert!(text.contains("portail_listener_accepted_total{proto=\"tcp\",port=\"8123\"} 3"));
        assert!(text.contains("portail_listener_up{proto=\"tcp\",port=\"8123\"} 1"));
        assert!(
            text.contains("portail_listener_http_requests_total{proto=\"tcp\",port=\"8123\"} 2")
        );
        assert!(text.contains(
            "portail_listener_upstream_connect_errors_total{proto=\"tcp\",port=\"8123\"} 1"
        ));
        assert!(text.contains("portail_listener_active_connections{proto=\"tcp\",port=\"8123\"} 0"));

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
