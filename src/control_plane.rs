use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, Ordering};
use tracing::info;
use tokio::signal;

use crate::routing::RouteTable;
use crate::config::UringRessConfig;

/// Control Plane running on dedicated Tokio runtime
/// Handles configuration management, Gateway API integration (future), and metrics
pub struct ControlPlane {
    pub config_manager: Arc<ConfigManager>,
    config: Option<UringRessConfig>,
    api_port: Option<u16>,
    api_disabled: bool,
}

impl ControlPlane {
    pub fn new() -> Result<Self> {
        let config_manager = Arc::new(ConfigManager::new());
        
        Ok(Self {
            config_manager,
            config: None,
            api_port: None,
            api_disabled: true,
        })
    }
    
    pub fn new_with_config(config: UringRessConfig, api_port: Option<u16>, api_disabled: bool) -> Result<Self> {
        let config_manager = Arc::new(ConfigManager::new());
        
        Ok(Self {
            config_manager,
            config: Some(config),
            api_port,
            api_disabled,
        })
    }

    /// Start the control plane in dedicated Tokio runtime
    pub async fn start(&self) -> Result<()> {
        info!("Starting Control Plane with Tokio runtime");
        
        // Initialize route configuration
        if let Some(ref config) = self.config {
            // Use file-based configuration
            self.setup_configured_routes(config).await?;
            info!("Routes loaded from configuration file");
        } else {
            // Start with empty route table - ready for dynamic API configuration
            let empty_route_table = RouteTable::new();
            self.config_manager.update_routes(empty_route_table).await?;
            info!("Started with empty route table - ready for dynamic configuration");
        }
        
        // TODO: Start API server if enabled
        if !self.api_disabled {
            if let Some(port) = self.api_port {
                info!("API server would start on port {} (not implemented yet)", port);
            }
        }
        
        // TODO: Future Gateway API integration
        // self.start_gateway_api_watchers().await?;
        
        info!("Control Plane started successfully");
        Ok(())
    }
    
    /// Wait for shutdown signal
    pub async fn wait_for_shutdown(&self) -> Result<()> {
        info!("Control Plane waiting for shutdown signal");
        signal::ctrl_c().await?;
        info!("Control Plane received shutdown signal");
        Ok(())
    }
    
    async fn setup_configured_routes(&self, config: &UringRessConfig) -> Result<()> {
        info!("Setting up routes from configuration file");
        
        // Convert configuration to route table using zero-overhead conversion
        let route_table = config.to_route_table()
            .map_err(|e| anyhow::anyhow!("Failed to convert configuration to route table: {}", e))?;
        
        // Update the atomic route table
        self.config_manager.update_routes(route_table).await?;
        
        info!("Configured routes loaded: {} HTTP routes, {} TCP routes", 
              config.http_routes.len(), 
              config.tcp_routes.len());
        Ok(())
    }
}

/// Configuration manager with atomic route table updates
/// Shared between control plane and data plane via Arc<AtomicPtr<RouteTable>>
pub struct ConfigManager {
    current_routes: Arc<AtomicPtr<RouteTable>>,
    // Future: update_channel: broadcast::Sender<ConfigUpdate>,
    // Future: version: AtomicU64,
}

impl ConfigManager {
    pub fn new() -> Self {
        let empty_route_table = Box::new(RouteTable::new());
        let current_routes = Arc::new(AtomicPtr::new(Box::into_raw(empty_route_table)));
        
        Self {
            current_routes,
        }
    }

    /// Atomically update route table (called from control plane)
    pub async fn update_routes(&self, route_table: RouteTable) -> Result<()> {
        info!("Updating route configuration");
        
        // Atomic route table update - zero-copy for data plane readers
        let new_table = Box::new(route_table);
        let old_ptr = self.current_routes.swap(Box::into_raw(new_table), Ordering::AcqRel);
        
        // Clean up old table
        if !old_ptr.is_null() {
            // SAFETY: old_ptr was returned by a previous swap operation and is guaranteed
            // to be a valid pointer to a RouteTable that was allocated using Box::into_raw.
            // The null check ensures we don't double-free.
            unsafe {
                let _ = Box::from_raw(old_ptr);
            }
        }
        
        info!("Route configuration updated atomically");
        Ok(())
    }

    /// Get route table reference for data plane workers
    pub fn get_routes(&self) -> Arc<AtomicPtr<RouteTable>> {
        self.current_routes.clone()
    }
}

// Future structures for Gateway API integration
#[allow(dead_code)]
pub struct GatewayApiController {
    // kube_client: Client,
    // gateway_watcher: Watcher<Gateway>,
    // http_route_watcher: Watcher<HTTPRoute>,
    // tcp_route_watcher: Watcher<TCPRoute>,
}

#[allow(dead_code)]
pub enum ConfigUpdate {
    Routes,
    Gateways,
    Backends,
}