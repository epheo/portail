use anyhow::Result;
use std::sync::Arc;
use arc_swap::ArcSwap;
use crate::logging::{info, debug, warn};
use tokio::signal;

use crate::routing::RouteTable;
use crate::config::PortailConfig;
use crate::ebpf::{initialize_ebpf_system, UnifiedEbpfManager};

/// Control Plane — shares the main Tokio runtime.
/// Handles configuration management, eBPF coordination, and routing.
pub struct ControlPlane {
    pub ebpf_manager: UnifiedEbpfManager,
    config: PortailConfig,
    current_routes: Arc<ArcSwap<RouteTable>>,
}

impl ControlPlane {
    pub fn new(config: PortailConfig) -> Result<Self> {
        let ebpf_manager = initialize_ebpf_system()?;
        let current_routes = Arc::new(ArcSwap::from_pointee(RouteTable::new()));

        Ok(Self {
            ebpf_manager,
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
        info!("Updating route configuration");
        self.current_routes.store(Arc::new(route_table));
        info!("Route configuration updated atomically");
        Ok(())
    }

    /// Get route table reference for data plane workers
    pub fn get_routes(&self) -> Arc<ArcSwap<RouteTable>> {
        self.current_routes.clone()
    }

    /// Attach SO_REUSEPORT eBPF programs to worker sockets
    /// Coordinates eBPF program attachment across all workers for address-aware dispatch
    pub fn attach_worker_reuseport_programs(&mut self, worker_fds: &[(usize, u16, std::os::unix::io::RawFd)]) -> Result<()> {
        info!("Attaching SO_REUSEPORT eBPF programs to {} worker sockets", worker_fds.len());
        
        let mut attached_count = 0;
        let mut failed_count = 0;
        
        for &(worker_id, port, socket_fd) in worker_fds {
            match self.ebpf_manager.attach_reuseport_program(socket_fd) {
                Ok(()) => {
                    debug!("SO_REUSEPORT eBPF program attached to worker {} socket fd {} (port {})", worker_id, socket_fd, port);
                    attached_count += 1;
                }
                Err(e) => {
                    warn!("Failed to attach SO_REUSEPORT eBPF program to worker {} socket fd {} (port {}): {}",
                          worker_id, socket_fd, port, e);
                    failed_count += 1;
                }
            }
        }
        
        if attached_count == worker_fds.len() {
            info!("SO_REUSEPORT eBPF programs attached successfully: {} sockets", attached_count);
            
            self.ebpf_manager.populate_socket_maps(worker_fds)
                .map_err(|e| anyhow::anyhow!("Failed to populate socket maps: {}", e))?;
            
            info!("Runtime configuration handled by socket array with {} workers", worker_fds.len());
            
            info!("Address-aware packet distribution enabled for all workers");
            Ok(())
        } else {
            // Fail fast - no fallback to degraded operation
            Err(anyhow::anyhow!(
                "eBPF SO_REUSEPORT program attachment failed. \
                \nSuccessful attachments: {}/{} worker sockets (failed: {}). \
                \nSingle codepath architecture requires all eBPF programs to work correctly. \
                \nFailing fast instead of degraded operation. \
                \n \
                \nTroubleshooting: \
                \n  1. Check kernel eBPF support: sudo dmesg | grep -i bpf \
                \n  2. Verify eBPF programs compiled: ls -la target/ebpf/ \
                \n  3. Check capabilities: sudo setcap cap_net_admin,cap_bpf+ep target/debug/portail \
                \n  4. Run with privileges: sudo ./target/debug/portail",
                attached_count, worker_fds.len(), failed_count
            ))
        }
    }
}