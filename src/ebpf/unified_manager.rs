/// Simplified eBPF Manager for UringRess
/// Manages SO_REUSEPORT program only - now using Rust eBPF implementation
/// Single codepath: eBPF functionality is required, no fallback behavior

use anyhow::{anyhow, Result};
use aya::{
    maps::ReusePortSockArray,
    programs::SkReuseport,
    Ebpf,
};
use std::os::unix::io::{RawFd, AsRawFd, BorrowedFd};
use tracing::{info, debug, warn};

/// Simple wrapper for RawFd that implements AsRawFd for ReusePortSockArray::set()
/// This allows us to pass raw file descriptors to the socket array map
struct SocketFdWrapper(RawFd);

impl AsRawFd for SocketFdWrapper {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

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
                .program_mut("address_aware_worker_selector")
                .ok_or_else(|| {
                    anyhow!("SO_REUSEPORT program 'address_aware_worker_selector' not found. Available: {:?}", available_programs)
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
            .program_mut("address_aware_worker_selector")
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
    
    
    
    
    
    /// Update socket array map with worker socket file descriptors using worker-ID-based indexing
    /// This ensures worker N's socket is placed at index N in the socket array for correct selection
    pub fn update_worker_socket_array_by_worker_id(&mut self, worker_socket_map: &std::collections::BTreeMap<u32, RawFd>) -> Result<()> {
        info!("🔧 SOCKET_ARRAY_DEBUG: Starting worker-ID-based socket array population");
        
        // Log the worker-to-socket mapping
        for (&worker_id, &socket_fd) in worker_socket_map {
            info!("🔧 SOCKET_ARRAY_DEBUG: Worker {} -> socket_fd={}", worker_id, socket_fd);
        }
        
        // Get worker_socket_array using correct aya-rs ReusePortSockArray pattern
        info!("🔧 SOCKET_ARRAY_DEBUG: Attempting to get worker_socket_array map");
        let mut worker_socket_array: ReusePortSockArray<_> = ReusePortSockArray::try_from(
            self.bpf.map_mut("worker_socket_array")
                .ok_or_else(|| {
                    warn!("❌ SOCKET_ARRAY_DEBUG: worker_socket_array map not found!");
                    anyhow!("worker_socket_array not found")
                })?
        ).map_err(|e| {
            warn!("❌ SOCKET_ARRAY_DEBUG: Failed to convert worker_socket_array: {}", e);
            anyhow!("Failed to convert worker_socket_array: {}", e)
        })?;
        
        info!("✅ SOCKET_ARRAY_DEBUG: Successfully obtained ReusePortSockArray map");
        
        // Populate socket array using worker IDs as indices
        let mut successful_insertions: u32 = 0;
        for (&worker_id, &socket_fd) in worker_socket_map {
            info!("🔧 SOCKET_ARRAY_DEBUG: Processing Worker {} socket_fd={}", worker_id, socket_fd);
            
            if socket_fd <= 0 {
                warn!("❌ SOCKET_ARRAY_DEBUG: Invalid socket FD {} for worker {}, skipping", socket_fd, worker_id);
                continue;
            }
            
            if worker_id >= 8 {
                warn!("❌ SOCKET_ARRAY_DEBUG: Worker ID {} exceeds socket array size (8), skipping", worker_id);
                continue;
            }
            
            info!("✅ SOCKET_ARRAY_DEBUG: Socket FD {} is valid, creating wrapper", socket_fd);
            
            // Create AsRawFd wrapper for the raw file descriptor
            let socket_wrapper = SocketFdWrapper(socket_fd);
            
            info!("🔧 SOCKET_ARRAY_DEBUG: Calling socket_array.set({}, socket_fd={}, flags=0)", worker_id, socket_fd);
            
            // Use worker ID directly as the index in the socket array
            match worker_socket_array.set(worker_id, &socket_wrapper, 0) {
                Ok(()) => {
                    info!("✅ SOCKET_ARRAY_DEBUG: Successfully added Worker {} socket fd={} at index={}", 
                          worker_id, socket_fd, worker_id);
                    successful_insertions += 1;
                }
                Err(e) => {
                    warn!("❌ SOCKET_ARRAY_DEBUG: Failed to add Worker {} socket {} at index {}: {}", 
                          worker_id, socket_fd, worker_id, e);
                    return Err(anyhow!("Failed to add Worker {} socket {} at index {}: {}", 
                                      worker_id, socket_fd, worker_id, e));
                }
            }
        }
        
        info!("🎉 SOCKET_ARRAY_DEBUG: Worker-ID-based socket array population completed!");
        info!("🎉 SOCKET_ARRAY_DEBUG: - Total workers: {}", worker_socket_map.len());
        info!("🎉 SOCKET_ARRAY_DEBUG: - Successful insertions: {}", successful_insertions);
        
        // Log final mapping for verification
        for (&worker_id, &socket_fd) in worker_socket_map {
            info!("🎉 SOCKET_ARRAY_DEBUG: Index {} = Worker {} socket_fd={}", 
                  worker_id, worker_id, socket_fd);
        }
        
        if successful_insertions == 0 {
            warn!("❌ SOCKET_ARRAY_DEBUG: WARNING - No sockets were successfully inserted into the array!");
        }
        
        Ok(())
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