use anyhow::{Result, anyhow};
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
        use crate::config::{WorkerConfig, WorkerProtocol};
        let default_worker_configs = vec![
            WorkerConfig {
                id: 0,
                protocols: vec![WorkerProtocol::Http1, WorkerProtocol::Http2],
                buffer_size: 8192,
                buffer_count: 64,
                tcp_nodelay: false,
                tcp_keepalive: false,
                tcp_keepalive_idle: std::time::Duration::from_secs(7200),
                tcp_keepalive_interval: std::time::Duration::from_secs(75),
                tcp_keepalive_probes: 9,
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
        // Extract all unique interfaces from listeners, with fallback to gateway-level interface
        let interfaces = Self::extract_interfaces_from_config(&config)?;
        
        // Initialize simplified eBPF system (interface no longer needed)
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
            
            // Update specialization hints for eBPF routing optimization
            let protocol_worker_configs = self.address_dispatcher.generate_protocol_worker_configs(&gateway_addresses)
                .map_err(|e| anyhow::anyhow!("Failed to update specialization hints: {}", e))?;
            
            info!("eBPF protocol-worker configurations generated: {} mappings", 
                  protocol_worker_configs.len());
                  
            // Log the specific eBPF configurations for debugging
            for ((addr, protocol), worker_id) in &protocol_worker_configs {
                debug!("eBPF config: Address {} Protocol {} → Worker {}", 
                       addr, protocol, worker_id);
            }
            
            // Actually populate the eBPF maps with the generated configurations
            self.ebpf_manager.update_protocol_worker_configs(&protocol_worker_configs)
                .map_err(|e| anyhow::anyhow!("Failed to update eBPF protocol-worker maps: {}", e))?;
                
            // Generate and populate worker capability configurations
            let worker_capabilities = self.address_dispatcher.generate_worker_capability_configs()
                .map_err(|e| anyhow::anyhow!("Failed to generate worker capability configs: {}", e))?;
            
            if !worker_capabilities.is_empty() {
                info!("Updating eBPF worker capability configurations: {} workers", worker_capabilities.len());
                self.ebpf_manager.update_worker_capabilities(&worker_capabilities)
                    .map_err(|e| anyhow::anyhow!("Failed to update eBPF worker capability maps: {}", e))?;
                info!("Successfully updated eBPF worker capabilities - dynamic protocol validation enabled");
            } else {
                warn!("No worker capability configurations generated - eBPF worker validation may not work correctly");
            }
            
            // Update eBPF runtime configuration with worker count from YAML config
            let worker_count = self.address_dispatcher.get_worker_count();
            info!("Updating eBPF runtime configuration: worker_count={}", worker_count);
            self.ebpf_manager.update_runtime_configuration(worker_count)
                .map_err(|e| anyhow::anyhow!("Failed to update eBPF runtime configuration: {}", e))?;
            info!("Successfully updated eBPF runtime configuration - dynamic worker selection enabled");
                
            // Get Gateway API protocol configurations if any
            let gateway_configs = self.address_dispatcher.generate_gateway_protocol_configs();
            if !gateway_configs.is_empty() {
                self.ebpf_manager.update_gateway_protocol_configs(&gateway_configs)
                    .map_err(|e| anyhow::anyhow!("Failed to update eBPF gateway protocol maps: {}", e))?;
                info!("Updated eBPF maps with {} Gateway API protocol configurations", gateway_configs.len());
            }
            
            // Generate listener protocol configurations from YAML listeners
            let listener_configs = self.generate_listener_protocol_configs(config)
                .map_err(|e| anyhow::anyhow!("Failed to generate listener protocol configs: {}", e))?;
            if !listener_configs.is_empty() {
                self.ebpf_manager.update_listener_protocol_configs(&listener_configs)
                    .map_err(|e| anyhow::anyhow!("Failed to update eBPF listener protocol maps: {}", e))?;
                info!("Updated eBPF maps with {} listener protocol configurations", listener_configs.len());
            }
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
        
        // Note: Debug metrics monitoring disabled for cleaner production logs
        // self.start_continuous_debug_metrics_monitoring();
        
        // TODO: Start connection cleanup task
        // TODO: Start load balancing optimization task
        
        info!("eBPF background tasks started successfully");
        Ok(())
    }
    
    /// Start continuous debug metrics monitoring to troubleshoot worker selection issues
    /// This runs in the background and will catch connection attempts when they happen
    fn start_continuous_debug_metrics_monitoring(&self) {
        info!("Starting continuous eBPF debug metrics monitoring");
        
        // Create a channel to send metrics requests
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        
        // Clone what we need for the background task
        // We'll create a simplified interface to avoid borrowing the entire manager
        let ebpf_manager_for_metrics = std::ptr::addr_of!(self.ebpf_manager) as usize;
        
        // Spawn background monitoring task
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
            let mut last_packet_counts = std::collections::HashMap::<u32, u64>::new();
            
            loop {
                interval.tick().await;
                
                // SAFETY: This is a temporary workaround for the borrowing issue
                // In production, we'd use proper message passing or Arc<RwLock<>>
                let ebpf_manager = unsafe { &*(ebpf_manager_for_metrics as *const crate::ebpf::UnifiedEbpfManager) };
                
                // Try to read debug metrics from all workers
                match ebpf_manager.get_all_worker_debug_metrics() {
                    Ok(metrics) => {
                        if !metrics.is_empty() {
                            let mut new_activity = false;
                            let mut any_packets = false;
                            
                            // Check for new activity since last check
                            for (worker_id, stats) in &metrics {
                                let total_packets = stats.total_packets;
                                let last_count = last_packet_counts.get(worker_id).unwrap_or(&0);
                                
                                if total_packets > 0 {
                                    any_packets = true;
                                }
                                
                                if total_packets > *last_count {
                                    new_activity = true;
                                    last_packet_counts.insert(*worker_id, total_packets);
                                }
                            }
                            
                            // Show metrics if there's new activity OR if we have workers but no packets (debugging)
                            if new_activity || (!any_packets && metrics.len() > 0) {
                                let activity_type = if new_activity { "NEW Activity" } else { "DEBUG - All Workers (0 packets)" };
                                info!("🔍 eBPF {} - Debug Metrics ({} workers):", activity_type, metrics.len());
                                
                                for (worker_id, stats) in &metrics {
                                    // Copy fields to avoid packed struct alignment issues
                                    let total_packets = stats.total_packets;
                                    let last_selected_worker = stats.last_selected_worker;
                                    let last_protocol = stats.last_protocol;
                                    let last_dest_addr = stats.last_dest_addr;
                                    let debug_flags = stats.debug_flags;
                                    let selection_failures = stats.selection_failures;
                                    
                                    info!("  🔧 Worker {}: {} packets, last_worker={}, last_protocol={}, dest_addr={}, debug_flags=0x{:02x}, failures={}",
                                          worker_id, 
                                          total_packets, 
                                          last_selected_worker,
                                          last_protocol,
                                          std::net::Ipv4Addr::from(last_dest_addr.to_be_bytes()),
                                          debug_flags,
                                          selection_failures);
                                    
                                    // Log specific issues with clear indicators
                                    if debug_flags != 0 {
                                        if debug_flags & 0x01 != 0 { // DEBUG_FLAG_FALLBACK_USED
                                            warn!("⚠️  Worker {} using FALLBACK selection (possible routing issue)", worker_id);
                                        }
                                        if debug_flags & 0x04 != 0 { // DEBUG_FLAG_CAPABILITY_MISS
                                            warn!("❌ Worker {} capability lookup FAILED (check worker_capability_map)", worker_id);
                                        }
                                        if debug_flags & 0x08 != 0 { // DEBUG_FLAG_ADDRESS_MISS
                                            warn!("🎯 Worker {} address-specific mapping FAILED (check address_worker_map)", worker_id);
                                        }
                                    } else {
                                        info!("✅ Worker {} normal selection (no issues)", worker_id);
                                    }
                                    
                                    if selection_failures > 0 {
                                        warn!("💥 Worker {} has {} selection FAILURES - investigate eBPF program logic", 
                                              worker_id, selection_failures);
                                    }
                                }
                                
                                // Add separator for readability
                                info!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                            }
                        }
                    }
                    Err(e) => {
                        debug!("Could not read eBPF debug metrics: {} (metrics map may not be available)", e);
                    }
                }
            }
        });
        
        info!("✅ Continuous eBPF debug metrics monitoring started - will show activity when connections happen");
    }
    
    /// Extract all unique network interfaces from configuration
    fn extract_interfaces_from_config(config: &UringRessConfig) -> Result<Vec<String>> {
        let mut interfaces = std::collections::HashSet::new();
        
        // Collect interfaces from all listeners
        for listener in &config.gateway.listeners {
            if let Some(ref interface) = listener.interface {
                interfaces.insert(interface.clone());
            }
        }
        
        // If no per-listener interfaces specified, use gateway-level interface
        if interfaces.is_empty() {
            let fallback_interface = config.gateway.network_interface
                .clone()
                .unwrap_or_else(|| "lo".to_string());
            interfaces.insert(fallback_interface);
        }
        
        let interface_list: Vec<String> = interfaces.into_iter().collect();
        
        if interface_list.is_empty() {
            return Err(anyhow!("No network interfaces found in configuration"));
        }
        
        info!("Detected network interfaces: {:?}", interface_list);
        Ok(interface_list)
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
    
    /// Generate listener protocol configurations from YAML config
    /// Returns (address, port, protocol_mask) tuples for eBPF listener_protocol_map
    fn generate_listener_protocol_configs(&self, config: &UringRessConfig) -> Result<Vec<(std::net::IpAddr, u16, u8)>> {
        let mut listener_configs = Vec::new();
        
        for listener in &config.gateway.listeners {
            // Parse listener address
            let address = if let Some(ref addr_str) = listener.address {
                addr_str.parse::<std::net::IpAddr>()
                    .map_err(|e| anyhow::anyhow!("Invalid listener address {}: {}", addr_str, e))?
            } else {
                std::net::IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0)) // Default to wildcard
            };
            
            // Convert protocol from YAML to capability mask
            let protocol_mask = match listener.protocol {
                crate::config::Protocol::HTTP => {
                    // HTTP listeners support both HTTP1 and HTTP2
                    4 | 8  // PROTOCOL_HTTP1_BIT | PROTOCOL_HTTP2_BIT
                }
                crate::config::Protocol::HTTPS => {
                    // HTTPS also supports both HTTP1 and HTTP2 over TLS
                    4 | 8  // PROTOCOL_HTTP1_BIT | PROTOCOL_HTTP2_BIT  
                }
                crate::config::Protocol::TCP => {
                    2  // PROTOCOL_TCP_BIT
                }
                crate::config::Protocol::UDP => {
                    2  // Treat UDP as TCP capability for now
                }
            };
            
            listener_configs.push((address, listener.port, protocol_mask));
            
            debug!("Generated listener protocol config: {}:{} → protocol_mask=0x{:02x} ({:?})", 
                   address, listener.port, protocol_mask, listener.protocol);
        }
        
        info!("Generated {} listener protocol configurations from YAML", listener_configs.len());
        Ok(listener_configs)
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
    
    /// Get eBPF configuration statistics for monitoring
    /// Returns stats about current eBPF configuration state
    pub fn get_ebpf_config_stats(&self) -> crate::ebpf::AddressDistributionStats {
        self.address_dispatcher.get_config_stats()
    }
    
    /// Validate current eBPF configuration consistency
    /// Ensures all configurations are valid and consistent
    pub fn validate_ebpf_configuration(&self) -> Result<()> {
        self.address_dispatcher.validate_ebpf_configs()
    }
    
    /// Get debug metrics from eBPF worker_metrics map for troubleshooting
    pub fn get_worker_debug_metrics(&self, worker_id: u32) -> Result<crate::ebpf::maps::AddressAwareWorkerStats> {
        self.ebpf_manager.get_worker_debug_metrics(worker_id)
    }
    
    /// Get debug metrics from all active workers for comprehensive troubleshooting
    pub fn get_all_worker_debug_metrics(&self) -> Result<Vec<(u32, crate::ebpf::maps::AddressAwareWorkerStats)>> {
        self.ebpf_manager.get_all_worker_debug_metrics()
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
            
            // Update address dispatcher to track successful attachments
            self.address_dispatcher.set_ebpf_attachment_status(true);
            
            info!("Address-aware packet distribution enabled for all workers");
            Ok(())
        } else {
            // Fail fast - no fallback to degraded operation
            Err(anyhow::anyhow!(
                "eBPF SO_REUSEPORT program attachment failed. \
                \nSuccessful attachments: {}/{} worker sockets. \
                \nSingle codepath architecture requires all eBPF programs to work correctly. \
                \nFailing fast instead of degraded operation. \
                \n \
                \nTroubleshooting: \
                \n  1. Check kernel eBPF support: sudo dmesg | grep -i bpf \
                \n  2. Verify eBPF programs compiled: ls -la target/ebpf/ \
                \n  3. Check capabilities: sudo setcap cap_net_admin,cap_bpf+ep target/debug/uringress \
                \n  4. Run with privileges: sudo ./target/debug/uringress",
                attached_count, worker_fds.len()
            ))
        }
    }
}