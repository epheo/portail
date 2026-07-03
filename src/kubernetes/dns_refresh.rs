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
//! intentionally **not** cleared on swap: it is keyed by `socket_addr` and lives
//! outside the table, so surviving pods keep their passive-health status and new
//! pod IPs start optimistically healthy — matching the reconcile-driven swap.
//!
//! The swap is a **compare-and-swap** against the table this task snapshotted at
//! the start of the tick: the reconciler is a second, authoritative writer of the
//! same cell, so if it stores a fresher table during this task's (blocking)
//! re-resolve, the CAS leaves that table in place and this tick is dropped. That
//! keeps the task stateless and unable to clobber a just-applied config change.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::{ArcSwap, ArcSwapOption};
use tokio_util::sync::CancellationToken;

use crate::config::PortailConfig;
use crate::logging::{debug, info, warn};
use crate::metrics::METRICS;
use crate::routing::RouteTable;

/// Spawn the background DNS-refresh task.
///
/// `config` is the shared cell the Kubernetes reconciler publishes its last-built
/// config into. This task is spawned only in Kubernetes mode (the standalone
/// config-file path re-resolves on reload via `config_watcher`, not here), and it
/// no-ops on each tick until the first reconcile populates the cell. A zero
/// `interval` disables the task entirely.
pub fn spawn_dns_refresh(
    config: Arc<ArcSwapOption<PortailConfig>>,
    routes: Arc<ArcSwap<RouteTable>>,
    interval: Duration,
    shutdown: CancellationToken,
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

            let Some(cfg) = config.load_full() else {
                continue;
            };

            // Snapshot the live table we intend to replace, before the (slow)
            // re-resolve, so the CAS in `apply_refresh` can tell whether a
            // reconcile swapped a fresher table in while we were resolving.
            let current = routes.load_full();

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

            match apply_refresh(&routes, &current, built) {
                SwapOutcome::Swapped => {
                    METRICS.dns_refresh_swaps_total.inc();
                    info!("DNS refresh: resolved backend set changed; route table swapped")
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

/// Install `built` into `routes`, replacing the table snapshotted as `current`,
/// but only if (a) the resolved backend set actually changed and (b) no other
/// writer swapped in the meantime.
///
/// The compare-and-swap against `current` is what stops a slow re-resolve from
/// clobbering a fresh reconcile: if the reconciler stored a newer table while we
/// were resolving, the CAS leaves it in place and we drop our (now-stale) rebuild.
/// `current` is an immutable snapshot, so its `dns_signature` is stable; comparing
/// against it detects DNS drift relative to what was live at snapshot time. A
/// churny resolver that merely reorders records is a no-op (the signature is
/// order-independent).
fn apply_refresh(
    routes: &ArcSwap<RouteTable>,
    current: &Arc<RouteTable>,
    built: RouteTable,
) -> SwapOutcome {
    if built.dns_signature() == current.dns_signature() {
        return SwapOutcome::Unchanged;
    }
    let prev = routes.compare_and_swap(current, Arc::new(built));
    if Arc::ptr_eq(current, &prev) {
        SwapOutcome::Swapped
    } else {
        SwapOutcome::Superseded
    }
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

    #[test]
    fn unchanged_set_does_not_swap() {
        let live = Arc::new(table_with_tcp_backend("10.0.0.1"));
        let routes = ArcSwap::new(live.clone());
        // Re-resolve returns the same address set → no swap, same table instance.
        let built = table_with_tcp_backend("10.0.0.1");
        assert_eq!(apply_refresh(&routes, &live, built), SwapOutcome::Unchanged);
        assert!(Arc::ptr_eq(&routes.load_full(), &live));
    }

    #[test]
    fn changed_set_swaps() {
        let live = Arc::new(table_with_tcp_backend("10.0.0.1"));
        let routes = ArcSwap::new(live.clone());
        // A pod was replaced (different address) → swap.
        let built = table_with_tcp_backend("10.0.0.2");
        assert_eq!(apply_refresh(&routes, &live, built), SwapOutcome::Swapped);
        assert_eq!(
            routes.load().dns_signature(),
            table_with_tcp_backend("10.0.0.2").dns_signature()
        );
    }

    #[test]
    fn concurrent_reconcile_is_not_clobbered() {
        // The race: the DNS task snapshots `current`, then a reconcile stores a
        // fresher table, then the (stale) re-resolve tries to swap. The CAS must
        // leave the reconcile's table in place — never revert to the stale build.
        let snapshot = Arc::new(table_with_tcp_backend("10.0.0.1"));
        let routes = ArcSwap::new(snapshot.clone());

        // Reconcile lands during our rebuild: a different, fresher table.
        let reconciled = Arc::new(table_with_tcp_backend("10.0.0.9"));
        routes.store(reconciled.clone());

        // Our re-resolve (from the old config) differs from the snapshot but must
        // NOT overwrite the reconcile's table.
        let stale_built = table_with_tcp_backend("10.0.0.2");
        assert_eq!(
            apply_refresh(&routes, &snapshot, stale_built),
            SwapOutcome::Superseded
        );
        assert!(
            Arc::ptr_eq(&routes.load_full(), &reconciled),
            "reconcile's table must survive the stale DNS-refresh swap"
        );
    }
}
