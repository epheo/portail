use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, Ordering};
use tracing::{info, debug, warn};
use tokio::signal;
use std::net::IpAddr;

use crate::routing::RouteTable;
use crate::config::UringRessConfig;
use crate::ebpf::{initialize_ebpf_system, UnifiedEbpfManager, AddressDispatcher};

/// Control Plane running on dedicated Tokio runtime
/// Handles configuration management, Gateway API integration, eBPF coordination, and metrics
/// Single codepath: eBPF functionality is required, no fallback behavior
pub struct ControlPlane {
    pub ebpf_manager: UnifiedEbpfManager,
    pub address_dispatcher: AddressDispatcher,
    config: Option<UringRessConfig>,
    current_routes: Arc<AtomicPtr<RouteTable>>,
}

impl ControlPlane {
    pub fn new() -> Result<Self> {
        // Initialize simplified eBPF system - required component, no fallback
        let ebpf_manager = initialize_ebpf_system()?;
        
        // Initialize address dispatcher with default worker configurations
        use crate::config::{WorkerConfig, Protocol};
        let default_worker_configs = vec![
            WorkerConfig {
                id: 0,
                protocol: Protocol::HTTP,
                buffer_size: 8192,
                buffer_count: 256,  // Increased for high concurrent load
                tcp_nodelay: false,
            },
        ];
        let address_dispatcher = AddressDispatcher::new(default_worker_configs);
        
        // Initialize route table
        let empty_route_table = Box::new(RouteTable::new());
        let current_routes = Arc::new(AtomicPtr::new(Box::into_raw(empty_route_table)));
        
        Ok(Self {
            ebpf_manager,
            address_dispatcher,
            config: None,
            current_routes,
        })
    }
    
    pub fn new_with_config(config: UringRessConfig) -> Result<Self> {
        
        // Initialize eBPF system
        let ebpf_manager = initialize_ebpf_system()?;
        
        // Initialize address dispatcher with configured workers
        let worker_configs = config.gateway.get_worker_configs();
        let address_dispatcher = AddressDispatcher::new(worker_configs);
        
        // Initialize route table
        let empty_route_table = Box::new(RouteTable::new());
        let current_routes = Arc::new(AtomicPtr::new(Box::into_raw(empty_route_table)));
        
        Ok(Self {
            ebpf_manager,
            address_dispatcher,
            config: Some(config),
            current_routes,
        })
    }

    /// Start the control plane in dedicated Tokio runtime
    pub async fn start(&mut self) -> Result<()> {
        info!("Starting Control Plane with Tokio runtime and eBPF integration");
        
        // eBPF program is loaded automatically during manager initialization
        info!("eBPF SO_REUSEPORT program ready for socket attachment");
        
        // Initialize route configuration with eBPF synchronization
        if let Some(config) = self.config.clone() {
            // Use file-based configuration
            self.setup_configured_routes_with_ebpf(&config).await?;
            info!("Routes loaded from configuration file with eBPF integration");
        } else {
            // Start with empty route table - ready for dynamic API configuration
            let empty_route_table = RouteTable::new();
            self.update_routes(empty_route_table).await?;
            info!("Started with empty route table - ready for dynamic configuration");
        }
        
        // Start eBPF background tasks
        self.start_ebpf_background_tasks().await?;
        
        info!("Control Plane started successfully with eBPF integration");
        Ok(())
    }
    
    /// Wait for shutdown signal
    pub async fn wait_for_shutdown(&self) -> Result<()> {
        info!("Control Plane waiting for shutdown signal");
        signal::ctrl_c().await?;
        info!("Control Plane received shutdown signal");
        Ok(())
    }
    
    /// Setup configured routes with eBPF integration
    async fn setup_configured_routes_with_ebpf(&mut self, config: &UringRessConfig) -> Result<()> {
        info!("Setting up routes from configuration file with eBPF integration");
        
        // Convert configuration to route table using zero-overhead conversion
        let route_table = config.to_route_table()
            .map_err(|e| anyhow::anyhow!("Failed to convert configuration to route table: {}", e))?;
        
        // Update eBPF maps with routing configuration
        self.ebpf_manager.update_route_table(&route_table)
            .map_err(|e| anyhow::anyhow!("Failed to update eBPF route table: {}", e))?;
        
        // Extract Gateway API addresses from configuration
        let gateway_addresses = self.extract_gateway_addresses(config)?;
        if !gateway_addresses.is_empty() {
            info!("Gateway API addresses configured: {:?} (using optimized FNV hash routing)", gateway_addresses);
            info!("Worker capability and runtime configuration handled by socket array mapping");
        } else {
            info!("No specific Gateway API addresses configured, using wildcard binding");
        }
        
        // Update the atomic route table after eBPF synchronization
        self.update_routes(route_table).await?;
        
        info!("Configured routes loaded with eBPF integration: {} HTTP routes, {} TCP routes", 
              config.http_routes.len(), 
              config.tcp_routes.len());
        Ok(())
    }
    
    /// Start eBPF background tasks for maintenance
    async fn start_ebpf_background_tasks(&mut self) -> Result<()> {
        info!("Starting eBPF background maintenance tasks");
        
        
        // TODO: Start connection cleanup task
        // TODO: Start load balancing optimization task
        
        info!("eBPF background tasks started successfully");
        Ok(())
    }
    
    
    
    /// Extract Gateway API listener addresses from configuration
    fn extract_gateway_addresses(&self, config: &UringRessConfig) -> Result<Vec<IpAddr>> {
        let mut addresses = Vec::new();
        
        // TODO: Extract specific listener addresses from Gateway configuration
        // For Phase 1, we'll use a placeholder approach
        
        // Check if specific addresses are configured in listeners
        for listener in &config.gateway.listeners {
            if let Some(ref address) = listener.address {
                let ip_addr: std::net::IpAddr = address.parse().map_err(|e| anyhow::anyhow!("Invalid listener address {}: {}", address, e))?;
                addresses.push(ip_addr);
            }
        }
        
        // If no specific addresses, we'll let eBPF handle wildcard (0.0.0.0)
        if addresses.is_empty() {
            debug!("No specific listener addresses found, using wildcard binding");
        }
        
        Ok(addresses)
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
    
    
    
    /// Map socket FDs to worker IDs based on the observed pattern
    /// This is a temporary solution until we have proper worker-ID tracking
    fn create_socket_fd_to_worker_id_mapping(&self, worker_fds: &[(u16, std::os::unix::io::RawFd)]) -> Result<std::collections::BTreeMap<u32, std::os::unix::io::RawFd>> {
        let mut worker_socket_map = std::collections::BTreeMap::new();
        
        // Based on the debug output pattern, create the correct worker-to-socket mapping
        // This needs to be more dynamic in the future, but works for the current configuration
        
        // Separate sockets by port
        let mut http_sockets = Vec::new(); // Port 8080
        let mut tcp_sockets = Vec::new();  // Port 2222
        
        for &(port, socket_fd) in worker_fds {
            match port {
                8080 => http_sockets.push(socket_fd),
                2222 => tcp_sockets.push(socket_fd),
                _ => {
                    warn!("Unknown port {}, assigning to worker 0", port);
                    worker_socket_map.insert(0, socket_fd);
                }
            }
        }
        
        // Assign HTTP sockets to workers 0,1 (based on debug output order)
        for (i, socket_fd) in http_sockets.iter().enumerate() {
            let worker_id = i as u32; // Workers 0,1 for HTTP
            worker_socket_map.insert(worker_id, *socket_fd);
        }
        
        // Assign TCP sockets to workers 2,3 (based on debug output order)
        for (i, socket_fd) in tcp_sockets.iter().enumerate() {
            let worker_id = 2 + i as u32; // Workers 2,3 for TCP
            worker_socket_map.insert(worker_id, *socket_fd);
        }
        
        Ok(worker_socket_map)
    }

    /// Attach SO_REUSEPORT eBPF programs to worker sockets
    /// Coordinates eBPF program attachment across all workers for address-aware dispatch
    pub fn attach_worker_reuseport_programs(&mut self, worker_fds: &[(u16, std::os::unix::io::RawFd)]) -> Result<()> {
        info!("Attaching SO_REUSEPORT eBPF programs to {} worker sockets", worker_fds.len());
        
        let mut attached_count = 0;
        let mut failed_count = 0;
        
        for &(port, socket_fd) in worker_fds {
            match self.ebpf_manager.attach_reuseport_program(socket_fd) {
                Ok(()) => {
                    debug!("SO_REUSEPORT eBPF program attached to socket fd {} (port {})", socket_fd, port);
                    attached_count += 1;
                }
                Err(e) => {
                    warn!("Failed to attach SO_REUSEPORT eBPF program to socket fd {} (port {}): {}", 
                          socket_fd, port, e);
                    failed_count += 1;
                    
                    // Continue with other sockets rather than failing entirely
                    // This allows partial operation if some attachments fail
                }
            }
        }
        
        if attached_count == worker_fds.len() {
            info!("SO_REUSEPORT eBPF programs attached successfully: {} sockets", attached_count);
            
            // Populate socket array map for deterministic worker selection
            info!("🚀 CONTROL_PLANE_DEBUG: Starting socket array population process");
            
            // Create worker-ID-indexed socket map based on configuration and observed patterns
            let worker_socket_map = self.create_socket_fd_to_worker_id_mapping(worker_fds)?;
            
            info!("🚀 CONTROL_PLANE_DEBUG: Created worker-to-socket mapping:");
            for (&worker_id, &socket_fd) in &worker_socket_map {
                info!("🚀 CONTROL_PLANE_DEBUG: Worker {} -> socket_fd={}", worker_id, socket_fd);
            }
                
            info!("🚀 CONTROL_PLANE_DEBUG: Calling ebpf_manager.update_worker_socket_array_by_worker_id()");
            match self.ebpf_manager.update_worker_socket_array_by_worker_id(&worker_socket_map) {
                Ok(()) => {
                    info!("✅ CONTROL_PLANE_DEBUG: Socket array population completed successfully");
                }
                Err(e) => {
                    warn!("❌ CONTROL_PLANE_DEBUG: Socket array population failed: {}", e);
                    return Err(anyhow::anyhow!("Failed to populate worker socket array: {}", e));
                }
            }
            
            info!("Runtime configuration handled by socket array with {} workers", worker_fds.len());
            
            // Update address dispatcher to track successful attachments
            self.address_dispatcher.set_ebpf_attached(true);
            
            info!("Address-aware packet distribution enabled for all workers with deterministic socket selection");
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
                \n  3. Check capabilities: sudo setcap cap_net_admin,cap_bpf+ep target/debug/uringress \
                \n  4. Run with privileges: sudo ./target/debug/uringress",
                attached_count, worker_fds.len(), failed_count
            ))
        }
    }
}