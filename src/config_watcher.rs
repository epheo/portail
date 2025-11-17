//! Hot configuration reload via SIGHUP signal.
//!
//! When the process receives SIGHUP, the configuration file is re-read,
//! validated, converted to a RouteTable, and swapped atomically via ArcSwap.
//! Only routes are reloaded — listener/port changes require a restart.
//!
//! If the new configuration is invalid, the error is logged and the
//! existing routes remain active (no disruption).

use std::path::PathBuf;
use std::sync::Arc;
use arc_swap::ArcSwap;
use anyhow::Result;

use crate::config::PortailConfig;
use crate::logging::{info, warn, error};
use crate::routing::RouteTable;

/// Spawn a background task that listens for SIGHUP and reloads routes.
///
/// This should only be called in standalone (non-Kubernetes) mode.
/// In Kubernetes mode, the controller reconciler handles route updates.
pub async fn watch_config(
    config_path: PathBuf,
    routes: Arc<ArcSwap<RouteTable>>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    info!("Config watcher started — send SIGHUP to reload routes from {:?}", config_path);

    let mut sighup = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to register SIGHUP handler: {}. Hot reload disabled.", e);
            return;
        }
    };

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!("Config watcher shutting down");
                return;
            }
            _ = sighup.recv() => {
                info!("SIGHUP received — reloading configuration from {:?}", config_path);
                match reload_routes(&config_path, &routes) {
                    Ok(()) => info!("Route configuration reloaded successfully"),
                    Err(e) => error!("Config reload failed (keeping existing routes): {}", e),
                }
            }
        }
    }
}

/// Re-read the config file, validate it, convert to RouteTable, and swap atomically.
fn reload_routes(
    config_path: &PathBuf,
    routes: &Arc<ArcSwap<RouteTable>>,
) -> Result<()> {
    let config = PortailConfig::load_from_file(config_path)?;
    let route_table = config.to_route_table()?;

    let http_count = route_table.http_routes.len() + route_table.wildcard_http_routes.len();
    let tcp_count = route_table.tcp_routes.len();
    let udp_count = route_table.udp_routes.len();

    routes.store(Arc::new(route_table));

    info!("Routes reloaded: {} HTTP, {} TCP, {} UDP", http_count, tcp_count, udp_count);
    Ok(())
}
