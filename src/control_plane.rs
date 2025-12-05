use anyhow::Result;
use std::sync::Arc;
use arc_swap::ArcSwap;
use crate::logging::{info, debug};
use tokio::signal;

use crate::routing::RouteTable;
use crate::config::PortailConfig;

/// Control Plane — shares the main Tokio runtime.
/// Handles configuration management and routing.
pub struct ControlPlane {
    config: PortailConfig,
    current_routes: Arc<ArcSwap<RouteTable>>,
}

impl ControlPlane {
    pub fn new(config: PortailConfig) -> Result<Self> {
        let current_routes = Arc::new(ArcSwap::from_pointee(RouteTable::new()));

        Ok(Self {
            config,
            current_routes,
        })
    }

    pub async fn start(&self) -> Result<()> {
        info!("Starting Control Plane");
        let route_table = self.config.to_route_table()
            .map_err(|e| anyhow::anyhow!("Failed to convert configuration to route table: {}", e))?;
        self.update_routes(route_table)?;
        info!("Routes loaded: {} HTTP routes, {} TCP routes",
              self.config.http_routes.len(), self.config.tcp_routes.len());
        info!("Control Plane started successfully");
        Ok(())
    }

    pub async fn wait_for_shutdown(&self) -> Result<()> {
        info!("Control Plane waiting for shutdown signal");
        signal::ctrl_c().await?;
        info!("Control Plane received shutdown signal");
        Ok(())
    }
    
    pub fn update_routes(&self, route_table: RouteTable) -> Result<()> {
        debug!("Updating route configuration");
        self.current_routes.store(Arc::new(route_table));
        debug!("Route configuration updated atomically");
        Ok(())
    }

    /// Get route table reference for data plane workers
    pub fn get_routes(&self) -> Arc<ArcSwap<RouteTable>> {
        self.current_routes.clone()
    }
}