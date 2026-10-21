//! DNS-layer refresh of resolved backends (STRICT_DNS-style).
//!
//! Each portail pod serves a single Gateway and resolves backend Service FQDNs
//! to all of their A-records at config-build time
//! (see [`crate::routing::Backend::all_with_weight`]). Headless pods come and go
//! without any Gateway/Route/EndpointSlice change, so freshness is bounded by
//! DNS, not the apiserver: this task periodically re-runs `to_route_table()`
//! (which re-resolves every FQDN) and swaps the live route table **only when the
//! resolved address set actually changes**.
//!
//! It deliberately reuses the existing `Arc<ArcSwap<RouteTable>>` swap that
//! already backs config reload — a DNS change triggers the same atomic swap a
//! reconcile would, with no apiserver call and no hot-path change (every
//! resolved address is an ordinary fixed-`socket_addr` backend). Health state is
//! keyed by `socket_addr` and lives outside the table: a swap prunes the pods
//! that left it, so surviving pods keep their passive-health status and new or
//! recycled pod IPs start optimistically healthy, matching the reconcile-driven
//! swap.
//!
//! The swap is a **compare-and-swap** against the table this task snapshotted at
//! the start of the tick: the reconciler is a second, authoritative writer of the
//! same cell, so if it stores a fresher table during this task's (blocking)
//! re-resolve, the CAS leaves that table in place and this tick is dropped. That
//! keeps the task stateless and unable to clobber a just-applied config change.
//!
//! The config and the table built from it are read as one [`Published`] pair.
//! From two cells, a tick could pair an older config with a newer table and
//! CAS a table built from the older config into place.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::{ArcSwap, ArcSwapOption};
use tokio_util::sync::CancellationToken;

use crate::config::PortailConfig;
use crate::logging::{debug, info, warn};
use crate::metrics::METRICS;
use crate::proxy::health::HealthRegistry;
use crate::routing::RouteTable;

/// A config and the route table the reconciler built from it.
pub struct Published {
    pub config: Arc<PortailConfig>,
    pub table: Arc<RouteTable>,
}

/// Spawn the background DNS-refresh task.
///
/// `published` is the shared cell the Kubernetes reconciler publishes its
/// last-built config and table into. This task is spawned only in Kubernetes
/// mode (the standalone config-file path re-resolves on reload via
/// `config_watcher`, not here), and it no-ops on each tick until the first
/// reconcile populates the cell. A zero `interval` disables the task entirely.
///
/// `on_swap` runs after each swap: the backends' pods changed.
pub fn spawn_dns_refresh(
    published: Arc<ArcSwapOption<Published>>,
    routes: Arc<ArcSwap<RouteTable>>,
    health: Arc<HealthRegistry>,
    interval: Duration,
    shutdown: CancellationToken,
    on_swap: impl Fn() + Send + 'static,
) {
    if interval.is_zero() {
        info!("DNS refresh disabled (interval is zero)");
        return;
    }

    tokio::spawn(async move {
        info!("DNS refresh task started (every {:?})", interval);

        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // interval()'s first tick fires immediately; consume it so the first
        // re-resolve happens after a full interval, not at startup.
        ticker.tick().await;

        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.cancelled() => {
                    debug!("DNS refresh task shutting down");
                    break;
                }
            }

            // The pair's table is the one to replace: the CAS in
            // `apply_refresh` tells whether a reconcile swapped a fresher
            // table in while we were resolving.
            let Some(pair) = published.load_full() else {
                continue;
            };
            let cfg = pair.config.clone();

            // to_route_table() does blocking DNS resolution + regex compilation;
            // keep it off the async runtime. Each pod rebuilds only its own
            // Gateway's table, so this is cheap.
            // TODO(dns-refresh-cost): the common no-change tick still recompiles
            // every path/header regex just to discard the rebuild on a matching
            // signature. Split "re-resolve addresses" from "rebuild table" so an
            // unchanged address set skips regex compilation entirely.
            let built = match tokio::task::spawn_blocking(move || cfg.to_route_table()).await {
                Ok(Ok(table)) => table,
                Ok(Err(e)) => {
                    warn!(
                        "DNS refresh: route-table rebuild failed, keeping current table: {}",
                        e
                    );
                    continue;
                }
                Err(e) => {
                    warn!("DNS refresh: rebuild task panicked: {}", e);
                    continue;
                }
            };

            match apply_refresh(&published, &routes, &pair, built) {
                SwapOutcome::Swapped => {
                    health.prune(&routes.load());
                    METRICS.dns_refresh_swaps_total.inc();
                    info!("DNS refresh: resolved backend set changed; route table swapped");
                    on_swap();
                }
                SwapOutcome::Superseded => {
                    METRICS.dns_refresh_superseded_total.inc();
                    debug!(
                        "DNS refresh: reconcile swapped concurrently; dropping re-resolved table"
                    )
                }
                SwapOutcome::Unchanged => METRICS.dns_refresh_unchanged_total.inc(),
            }
        }
    });
}

/// Outcome of one DNS-refresh swap attempt.
#[derive(Debug, PartialEq, Eq)]
enum SwapOutcome {
    /// Resolved backend set unchanged since the snapshot — no swap.
    Unchanged,
    /// Re-resolved table installed.
    Swapped,
    /// A concurrent writer (the reconciler) swapped first — re-resolve dropped.
    Superseded,
}

/// Install `built`, re-resolved from `pair.config`, in place of `pair.table`,
/// but only if (a) the resolved backend set actually changed and (b) no other
/// writer swapped in the meantime.
///
/// The compare-and-swap against `pair.table` is what stops a slow re-resolve
/// from clobbering a fresh reconcile: if the reconciler stored a newer table
/// while we were resolving, the CAS leaves it in place and we drop our
/// (now-stale) rebuild. The pair is an immutable snapshot, so its
/// `dns_signature` is stable; comparing against it detects DNS drift relative
/// to what was live at snapshot time. A churny resolver that merely reorders
/// records is a no-op (the signature is order-independent).
fn apply_refresh(
    published: &ArcSwapOption<Published>,
    routes: &ArcSwap<RouteTable>,
    pair: &Arc<Published>,
    built: RouteTable,
) -> SwapOutcome {
    if built.dns_signature() == pair.table.dns_signature() {
        return SwapOutcome::Unchanged;
    }
    let built = Arc::new(built);
    let prev = routes.compare_and_swap(&pair.table, built.clone());
    if !Arc::ptr_eq(&pair.table, &prev) {
        return SwapOutcome::Superseded;
    }
    // The next tick must compare against the table now live. A reconcile
    // that published since keeps its newer pair.
    published.compare_and_swap(
        &Some(pair.clone()),
        Some(Arc::new(Published {
            config: pair.config.clone(),
            table: built,
        })),
    );
    SwapOutcome::Swapped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_with_tcp_backend(ip: &str) -> RouteTable {
        let mut t = RouteTable::new();
        t.add_tcp_route(
            7000,
            vec![crate::routing::Backend::new(ip.to_string(), 8001).unwrap()],
        );
        t
    }

    /// The cells as a reconcile leaves them: `table` live and published with
    /// the config it was built from.
    fn reconciled(
        ip: &str,
    ) -> (
        ArcSwapOption<Published>,
        ArcSwap<RouteTable>,
        Arc<Published>,
    ) {
        let pair = Arc::new(Published {
            config: Arc::new(PortailConfig::default()),
            table: Arc::new(table_with_tcp_backend(ip)),
        });
        (
            ArcSwapOption::new(Some(pair.clone())),
            ArcSwap::new(pair.table.clone()),
            pair,
        )
    }

    #[test]
    fn unchanged_set_does_not_swap() {
        let (published, routes, pair) = reconciled("10.0.0.1");
        // Re-resolve returns the same address set → no swap, same table instance.
        let built = table_with_tcp_backend("10.0.0.1");
        assert_eq!(
            apply_refresh(&published, &routes, &pair, built),
            SwapOutcome::Unchanged
        );
        assert!(Arc::ptr_eq(&routes.load_full(), &pair.table));
    }

    #[test]
    fn changed_set_swaps() {
        let (published, routes, pair) = reconciled("10.0.0.1");
        // A pod was replaced (different address) → swap.
        let built = table_with_tcp_backend("10.0.0.2");
        assert_eq!(
            apply_refresh(&published, &routes, &pair, built),
            SwapOutcome::Swapped
        );
        let live = routes.load_full();
        assert_eq!(
            live.dns_signature(),
            table_with_tcp_backend("10.0.0.2").dns_signature()
        );
        // The pair follows, so the next tick compares against the live table.
        let now = published.load_full().unwrap();
        assert!(Arc::ptr_eq(&now.table, &live));
        assert!(Arc::ptr_eq(&now.config, &pair.config));
    }

    /// A swap tells the controller: the backends' pods changed.
    #[tokio::test]
    async fn swap_runs_on_swap() {
        let config: PortailConfig = serde_json::from_str(
            r#"{
                "gateway": {"name": "g", "listeners": [{"name": "h", "protocol": "HTTP", "port": 80}]},
                "httpRoutes": [{
                    "parentRefs": [{"name": "g", "sectionName": "h"}],
                    "hostnames": ["a.example"],
                    "rules": [{"backendRefs": [{"name": "127.0.0.1", "port": 8001}]}]
                }]
            }"#,
        )
        .unwrap();
        // The live table resolved elsewhere: the next tick's rebuild differs.
        let pair = Arc::new(Published {
            config: Arc::new(config),
            table: Arc::new(table_with_tcp_backend("10.0.0.1")),
        });
        let routes = Arc::new(ArcSwap::new(pair.table.clone()));
        let (swapped, mut on_swap) = tokio::sync::mpsc::unbounded_channel();
        let shutdown = CancellationToken::new();
        spawn_dns_refresh(
            Arc::new(ArcSwapOption::new(Some(pair))),
            routes,
            Arc::new(HealthRegistry::new()),
            Duration::from_millis(10),
            shutdown.clone(),
            move || {
                let _ = swapped.send(());
            },
        );
        let called = tokio::time::timeout(Duration::from_secs(5), on_swap.recv()).await;
        shutdown.cancel();
        assert!(matches!(called, Ok(Some(()))));
    }

    /// A reconcile stores its table, then its pair. A tick reading between
    /// the two sees the old pair, whose table is no longer live. With config
    /// and table in two cells, that tick read the old config with the new
    /// table and swapped a table built from the old config into place.
    #[test]
    fn concurrent_reconcile_is_not_clobbered() {
        // The race: the DNS task reads its pair, then a reconcile stores a
        // fresher table, then the (stale) re-resolve tries to swap. The CAS must
        // leave the reconcile's table in place — never revert to the stale build.
        let (published, routes, snapshot) = reconciled("10.0.0.1");

        // Reconcile lands during our rebuild: a different, fresher table.
        let reconciled = Arc::new(table_with_tcp_backend("10.0.0.9"));
        routes.store(reconciled.clone());

        // Our re-resolve (from the old config) differs from the snapshot but must
        // NOT overwrite the reconcile's table.
        let stale_built = table_with_tcp_backend("10.0.0.2");
        assert_eq!(
            apply_refresh(&published, &routes, &snapshot, stale_built),
            SwapOutcome::Superseded
        );
        assert!(
            Arc::ptr_eq(&routes.load_full(), &reconciled),
            "reconcile's table must survive the stale DNS-refresh swap"
        );
    }
}
