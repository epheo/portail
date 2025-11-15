//! Simplified eBPF Manager for Portail
//! Manages SO_REUSEPORT program only - now using Rust eBPF implementation
//! Single codepath: eBPF functionality is required, no fallback behavior

use anyhow::{anyhow, Result};
use aya::{
    maps::{HashMap as EbpfHashMap, ReusePortSockArray},
    programs::SkReuseport,
    Ebpf,
};
use std::os::unix::io::{RawFd, AsRawFd, BorrowedFd};
use crate::logging::{info, debug, warn};

/// Simple wrapper for RawFd that implements AsRawFd for ReusePortSockArray::set()
/// This allows us to pass raw file descriptors to the socket array map
struct SocketFdWrapper(RawFd);

impl AsRawFd for SocketFdWrapper {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct PortWorkerConfig {
    base_index: u32,
    worker_count: u32,
}

// SAFETY: #[repr(C)] with only u32 fields, no padding
unsafe impl aya::Pod for PortWorkerConfig {}

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
    
    /// Attach SO_REUSEPORT program to specific socket
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
    
    /// Populate socket array and port→worker config maps for eBPF program.
    /// Groups sockets by port with contiguous array indices so each SO_REUSEPORT
    /// group maps to a distinct slice of the socket array.
    pub fn populate_socket_maps(
        &mut self,
        worker_fds: &[(usize, u16, RawFd)],
    ) -> Result<()> {
        use std::collections::BTreeMap;

        // Group by port, collecting (worker_id, fd) pairs
        let mut by_port: BTreeMap<u16, Vec<(usize, RawFd)>> = BTreeMap::new();
        for &(worker_id, port, fd) in worker_fds {
            by_port.entry(port).or_default().push((worker_id, fd));
        }
        // Sort each port's workers by worker_id for deterministic ordering
        for workers in by_port.values_mut() {
            workers.sort_by_key(|&(wid, _)| wid);
        }

        // Populate socket array (first borrow of self.bpf)
        let mut current_index: u32 = 0;
        let mut port_configs: Vec<(u16, PortWorkerConfig)> = Vec::new();
        {
            let mut worker_socket_array: ReusePortSockArray<_> = ReusePortSockArray::try_from(
                self.bpf.map_mut("worker_socket_array")
                    .ok_or_else(|| anyhow!("worker_socket_array map not found"))?
            ).map_err(|e| anyhow!("Failed to convert worker_socket_array: {}", e))?;

            for (&port, workers) in &by_port {
                let base_index = current_index;
                let worker_count = workers.len() as u32;

                for &(worker_id, fd) in workers {
                    worker_socket_array.set(current_index, &SocketFdWrapper(fd), 0)
                        .map_err(|e| anyhow!("Failed to set socket array index {} (worker {} port {}): {}",
                                              current_index, worker_id, port, e))?;
                    debug!("Socket array[{}]: worker {} port {} fd={}", current_index, worker_id, port, fd);
                    current_index += 1;
                }

                port_configs.push((port, PortWorkerConfig { base_index, worker_count }));
                debug!("Port {}: base_index={}, worker_count={}", port, base_index, worker_count);
            }
        }

        // Populate port config HashMap (second borrow of self.bpf)
        {
            let mut port_config_map: EbpfHashMap<_, u16, PortWorkerConfig> = EbpfHashMap::try_from(
                self.bpf.map_mut("port_worker_config")
                    .ok_or_else(|| anyhow!("port_worker_config map not found"))?
            ).map_err(|e| anyhow!("Failed to convert port_worker_config: {}", e))?;

            for (port, config) in &port_configs {
                port_config_map.insert(port, config, 0)
                    .map_err(|e| anyhow!("Failed to insert port_worker_config for port {}: {}", port, e))?;
            }
        }

        info!("Socket maps populated: {} ports, {} total entries", by_port.len(), current_index);
        Ok(())
    }
}

