/// Simplified eBPF Manager for UringRess
/// Manages SO_REUSEPORT program only - now using Rust eBPF implementation
/// Single codepath: eBPF functionality is required, no fallback behavior

use anyhow::{anyhow, Result};
use aya::{
    programs::SkReuseport,
    Ebpf,
};
use std::os::unix::io::{RawFd, BorrowedFd};
use tracing::{info, debug, warn};

/// Simplified eBPF manager for SO_REUSEPORT program only
/// Required component - no optional wrapping or fallback behavior
pub struct UnifiedEbpfManager {
    /// eBPF program container with loaded program and maps
    /// Programs accessed via bpf.program_mut() following upstream patterns
    bpf: Ebpf,
}

impl UnifiedEbpfManager {
    /// Create new simplified eBPF manager following upstream aya patterns
    /// Fails immediately if eBPF functionality unavailable
    pub fn new() -> Result<Self> {
        info!("Initializing eBPF manager - Rust SO_REUSEPORT program");
        
        // Load eBPF object from embedded bytecode (aya upstream pattern)
        let ebpf_bytes = aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/sk_reuseport"));
        info!("eBPF binary size: {} bytes", ebpf_bytes.len());
        
        let mut bpf = Ebpf::load(ebpf_bytes)
            .map_err(|e| {
                warn!("❌ Failed to load eBPF object");
                anyhow!("Failed to load eBPF program: {}. Binary size: {} bytes", e, ebpf_bytes.len())
            })?;
        
        info!("✅ eBPF object loaded successfully");
        
        // Load the SO_REUSEPORT program (upstream pattern)
        // First check available programs
        let available_programs: Vec<String> = bpf.programs().map(|(name, _)| name.to_string()).collect();
        info!("Available programs: {:?}", available_programs);
        
        {
            let prog: &mut SkReuseport = bpf
                .program_mut("select_socket")
                .ok_or_else(|| {
                    anyhow!("SO_REUSEPORT program 'select_socket' not found. Available: {:?}", available_programs)
                })?
                .try_into()
                .map_err(|e| anyhow!("Failed to convert to SkReuseport: {}", e))?;
            
            // Load program (upstream pattern)
            prog.load()
                .map_err(|e| anyhow!("Failed to load SkReuseport program: {}", e))?;
            
            info!("✅ SkReuseport program loaded successfully");
        }
        
        info!("✅ eBPF manager initialized successfully");
        
        Ok(Self { bpf })
    }
    
    // Program loading now happens during initialization in new()
    
    /// Attach SO_REUSEPORT program to specific socket following upstream patterns
    /// Called during socket setup in uring_worker.rs  
    pub fn attach_reuseport_program(&mut self, socket_fd: RawFd) -> Result<()> {
        debug!("Attaching SO_REUSEPORT program to socket fd: {}", socket_fd);
        
        // Get program reference (upstream pattern - fresh mutable reference each time)
        let program: &mut SkReuseport = self.bpf
            .program_mut("select_socket")
            .ok_or_else(|| anyhow!("SO_REUSEPORT program not found"))?
            .try_into()
            .map_err(|e| anyhow!("Failed to convert to SkReuseport: {}", e))?;
        
        // Attach to socket (upstream pattern)
        // SAFETY: socket_fd is a valid file descriptor from socket creation
        let borrowed_fd = unsafe { BorrowedFd::borrow_raw(socket_fd) };
        program.attach(borrowed_fd)
            .map_err(|e| anyhow!("Failed to attach SkReuseport to socket {}: {}", socket_fd, e))?;
        
        debug!("✅ SkReuseport program attached to socket: {}", socket_fd);
        Ok(())
    }
    
    // Stub methods for control plane compatibility during transition
    // These will be properly implemented once basic eBPF loading works
    
    pub fn update_route_table(&mut self, routes: &crate::routing::RouteTable) -> Result<()> {
        let http_count = routes.http_routes.len();
        let tcp_count = routes.tcp_routes.len();
        debug!("Route table update: {} HTTP, {} TCP routes (stub implementation)", http_count, tcp_count);
        Ok(())
    }
    
    pub fn update_protocol_worker_configs(&mut self, _configs: &[((std::net::IpAddr, u8), u32)]) -> Result<()> {
        debug!("Protocol worker config update (stub implementation)");
        Ok(())
    }
    
    pub fn update_worker_capabilities(&mut self, _capabilities: &[(u32, u8)]) -> Result<()> {
        debug!("Worker capabilities update (stub implementation)");
        Ok(())
    }
    
    pub fn update_runtime_configuration(&mut self, worker_count: u32) -> Result<()> {
        debug!("Runtime config update: {} workers (stub implementation)", worker_count);
        Ok(())
    }
    
    pub fn update_gateway_protocol_configs(&mut self, _configs: &[((std::net::IpAddr, u16), Vec<u8>)]) -> Result<()> {
        debug!("Gateway protocol config update (stub implementation)");
        Ok(())
    }
    
    pub fn update_listener_protocol_configs(&mut self, _configs: &[(std::net::IpAddr, u16, u8)]) -> Result<()> {
        debug!("Listener protocol config update (stub implementation)");
        Ok(())
    }
    
    pub fn get_worker_debug_metrics(&self, worker_id: u32) -> Result<super::maps::AddressAwareWorkerStats> {
        debug!("Getting debug metrics for worker {} (stub implementation)", worker_id);
        // Return a default struct for now
        Ok(super::maps::AddressAwareWorkerStats {
            total_packets: 0,
            total_bytes: 0,
            http_requests: 0,
            tcp_connections: 0,
            active_connections: 0,
            address_assignments: 0,
            last_selected_worker: 0,
            last_protocol: 0,
            last_dest_addr: 0,
            debug_flags: 0,
            selection_failures: 0,
        })
    }
    
    pub fn get_all_worker_debug_metrics(&self) -> Result<Vec<(u32, super::maps::AddressAwareWorkerStats)>> {
        debug!("Getting all worker debug metrics (stub implementation)");
        Ok(vec![])
    }
    
}

impl Drop for UnifiedEbpfManager {
    fn drop(&mut self) {
        info!("Shutting down UnifiedEbpfManager");
        
        // Note: In aya 0.13, XDP programs are detached automatically when dropped
        // SO_REUSEPORT programs are automatically detached when sockets close
        
        info!("UnifiedEbpfManager shutdown complete");
    }
}