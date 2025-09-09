/// Address Dispatcher for Gateway API compliance
/// Manages address-to-worker mappings with load balancing intelligence
/// Single codepath: no fallback to round-robin, address mapping is required

use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use tracing::{debug, info, warn};

use super::{WorkerInfo, WorkerSpecialization};
use crate::config::{WorkerConfig, WorkerProtocol};

/// eBPF Configuration Provider for Worker Specialization
/// Provides configuration hints to eBPF programs - does NOT perform worker selection
/// eBPF programs handle all actual routing decisions
pub struct AddressDispatcher {
    /// Dynamic worker configurations from config file
    worker_configs: Vec<WorkerConfig>,
    
    /// Worker specialization configurations for eBPF map population
    worker_specializations: HashMap<u32, WorkerSpecialization>,
    
    /// Protocol-to-worker preferences for eBPF protocol_worker_map
    protocol_worker_hints: HashMap<(IpAddr, u8), u32>, // (address, protocol) → preferred_worker
    
    /// Gateway API configurations for eBPF gateway_protocol_map
    gateway_configurations: HashMap<(IpAddr, u16), Vec<u8>>, // (address, port) → allowed_protocols
    
    /// Maximum number of workers available
    max_workers: u32,
    
    /// Track eBPF SO_REUSEPORT attachment status
    ebpf_attached: bool,
}

impl AddressDispatcher {
    /// Create new eBPF configuration provider with worker configurations
    pub fn new(worker_configs: Vec<WorkerConfig>) -> Self {
        let max_workers = worker_configs.len() as u32;
        info!("Creating eBPF Configuration Provider for {} dynamic workers", max_workers);
        
        // Generate worker specializations from dynamic config
        let mut worker_specializations = HashMap::new();
        
        for worker_config in &worker_configs {
            let specialization = if worker_config.protocols.contains(&WorkerProtocol::Tcp) {
                if worker_config.protocols.len() == 1 {
                    WorkerSpecialization::TcpOptimized  // Pure TCP worker
                } else {
                    WorkerSpecialization::Mixed         // Mixed TCP + HTTP worker
                }
            } else {
                WorkerSpecialization::HttpOptimized     // HTTP-only worker
            };
            
            worker_specializations.insert(worker_config.id as u32, specialization);
            debug!("Worker {} specialization: {:?} (protocols: {:?})", 
                   worker_config.id, specialization, worker_config.protocols);
        }
        
        Self {
            worker_configs,
            worker_specializations,
            protocol_worker_hints: HashMap::new(),
            gateway_configurations: HashMap::new(),
            max_workers,
            ebpf_attached: false,
        }
    }
    
    /// Generate eBPF map configuration data for protocol-worker mappings
    /// Returns data ready for populating eBPF protocol_worker_map
    pub fn generate_protocol_worker_configs(&mut self, addresses: &[IpAddr]) -> Result<Vec<((IpAddr, u8), u32)>> {
        info!("Generating eBPF protocol-worker configurations for {} Gateway API addresses", addresses.len());
        
        if addresses.is_empty() {
            return Ok(vec![]);
        }
        
        let mut ebpf_configs = Vec::new();
        
        // Generate protocol-specific worker assignments based on dynamic worker configs
        for &addr in addresses {
            // Find workers for each protocol type
            if let Some(http1_worker) = self.select_worker_for_protocol(WorkerProtocol::Http1) {
                ebpf_configs.push(((addr, 2), http1_worker)); // PROTOCOL_HTTP1 = 2
            }
            
            if let Some(http2_worker) = self.select_worker_for_protocol(WorkerProtocol::Http2) {
                ebpf_configs.push(((addr, 3), http2_worker)); // PROTOCOL_HTTP2 = 3
            }
            
            if let Some(tcp_worker) = self.select_worker_for_protocol(WorkerProtocol::Tcp) {
                ebpf_configs.push(((addr, 1), tcp_worker)); // PROTOCOL_TCP = 1
            }
            
            debug!("Address {} eBPF configs generated for available protocols", addr);
        }
        
        info!("Generated {} eBPF protocol-worker configurations", ebpf_configs.len());
        Ok(ebpf_configs)
    }
    
    /// Select worker within a protocol specialization group (e.g., workers 0-1 for HTTP1)
    /// Uses simple round-robin within the group for load distribution
    fn select_worker_for_protocol_group(&self, group_start: u32, group_size: u32) -> u32 {
        // Simple round-robin selection within the group
        // eBPF will handle actual load balancing and intelligent routing
        use std::sync::atomic::{AtomicU32, Ordering};
        static ROUND_ROBIN_COUNTER: AtomicU32 = AtomicU32::new(0);
        
        let counter = ROUND_ROBIN_COUNTER.fetch_add(1, Ordering::Relaxed);
        let offset = counter % group_size;
        
        group_start + offset
    }
    
    /// Select worker for a specific protocol using dynamic worker configuration
    /// Returns None if no workers support the specified protocol
    fn select_worker_for_protocol(&self, protocol: WorkerProtocol) -> Option<u32> {
        // Find all workers that support this protocol
        let mut supporting_workers: Vec<u32> = self.worker_configs
            .iter()
            .filter(|config| config.protocols.contains(&protocol))
            .map(|config| config.id as u32)
            .collect();
        
        if supporting_workers.is_empty() {
            warn!("No workers configured for protocol {:?}", protocol);
            return None;
        }
        
        // Sort for consistent ordering
        supporting_workers.sort_unstable();
        
        // Simple round-robin selection within supporting workers
        use std::sync::atomic::{AtomicU32, Ordering};
        static ROUND_ROBIN_COUNTER: AtomicU32 = AtomicU32::new(0);
        
        let counter = ROUND_ROBIN_COUNTER.fetch_add(1, Ordering::Relaxed);
        let index = counter % supporting_workers.len() as u32;
        let selected_worker = supporting_workers[index as usize];
        
        debug!("Selected worker {} for protocol {:?} from {} supporting workers: {:?}",
               selected_worker, protocol, supporting_workers.len(), supporting_workers);
        
        Some(selected_worker)
    }
    
    /// Get worker specialization for eBPF routing hints
    pub fn get_worker_specialization(&self, worker_id: u32) -> Option<WorkerSpecialization> {
        self.worker_specializations.get(&worker_id).copied()
    }
    
    /// Add Gateway API protocol configuration for specific (address, port)
    /// Data will be used to populate eBPF gateway_protocol_map
    pub fn add_gateway_protocol_config(&mut self, addr: IpAddr, port: u16, allowed_protocols: Vec<u8>) -> Result<()> {
        debug!("Adding Gateway API protocol config for {}:{} - protocols: {:?}", 
               addr, port, allowed_protocols);
        
        self.gateway_configurations.insert((addr, port), allowed_protocols);
        
        info!("Gateway protocol configuration added for {}:{}", addr, port);
        Ok(())
    }
    
    /// Generate eBPF gateway protocol map configurations
    /// Returns data ready for populating eBPF gateway_protocol_map  
    pub fn generate_gateway_protocol_configs(&self) -> Vec<((IpAddr, u16), Vec<u8>)> {
        self.gateway_configurations.iter()
            .map(|(&key, value)| (key, value.clone()))
            .collect()
    }
    
    /// Generate worker capability configurations for eBPF worker_capability_map
    /// Returns (worker_id, capability_bitmask) pairs for all configured workers
    pub fn generate_worker_capability_configs(&self) -> Result<Vec<(u32, u8)>> {
        let mut capability_configs = Vec::new();
        
        info!("Generating worker capability configurations for {} workers", self.worker_configs.len());
        
        for worker_config in &self.worker_configs {
            let mut capability_mask = 0u8;
            
            // Build protocol capability bitmask from YAML configuration
            for protocol in &worker_config.protocols {
                let protocol_bit = match protocol {
                    WorkerProtocol::Tcp => 1 << 1,      // PROTOCOL_TCP = 1
                    WorkerProtocol::Http1 => 1 << 2,    // PROTOCOL_HTTP1 = 2  
                    WorkerProtocol::Http2 => 1 << 3,    // PROTOCOL_HTTP2 = 3
                };
                capability_mask |= protocol_bit;
                
                debug!("Worker {} supports protocol {:?} (bit {})", 
                       worker_config.id, protocol, protocol_bit);
            }
            
            capability_configs.push((worker_config.id as u32, capability_mask));
            
            info!("Worker {} capability mask: 0x{:02x} (protocols: {:?})", 
                  worker_config.id, capability_mask, worker_config.protocols);
        }
        
        info!("Generated {} worker capability configurations", capability_configs.len());
        Ok(capability_configs)
    }
    
    /// Get the number of workers configured from YAML config
    pub fn get_worker_count(&self) -> u32 {
        self.worker_configs.len() as u32
    }

    /// Get eBPF configuration summary for validation
    pub fn get_config_stats(&self) -> AddressDistributionStats {
        let total_addresses = self.protocol_worker_hints.len() as u32;
        let active_workers = self.worker_specializations.len() as u32;
        let gateway_configs = self.gateway_configurations.len() as u32;
        
        AddressDistributionStats {
            total_addresses,
            active_workers,
            total_load: gateway_configs, // Reuse field for gateway config count
            max_worker_load: self.max_workers,
            min_worker_load: 0,
            load_imbalance: 1, // Not applicable for configuration provider
        }
    }
    
    /// Validate eBPF configuration consistency
    pub fn validate_ebpf_configs(&self) -> Result<()> {
        // Check that all workers are within valid range
        for &worker_id in self.worker_specializations.keys() {
            if worker_id >= self.max_workers {
                return Err(anyhow!("Worker {} exceeds max workers {} in eBPF configuration", 
                                 worker_id, self.max_workers));
            }
        }
        
        // Validate protocol-worker hints reference valid workers
        for (_key, &worker_id) in &self.protocol_worker_hints {
            if worker_id >= self.max_workers {
                return Err(anyhow!("Protocol hint references invalid worker {}", worker_id));
            }
        }
        
        debug!("eBPF configuration validation passed");
        Ok(())
    }
    
    /// Set eBPF SO_REUSEPORT attachment status
    /// Updates internal tracking of whether eBPF programs are active
    pub fn set_ebpf_attachment_status(&mut self, attached: bool) {
        self.ebpf_attached = attached;
        
        if attached {
            info!("eBPF SO_REUSEPORT programs marked as active - address-aware distribution enabled");
        } else {
            warn!("eBPF SO_REUSEPORT programs marked as inactive - using kernel round-robin");
        }
    }
    
    /// Check if eBPF SO_REUSEPORT programs are attached
    pub fn is_ebpf_attached(&self) -> bool {
        self.ebpf_attached
    }
}

/// Statistics for address distribution analysis
#[derive(Debug, Clone)]
pub struct AddressDistributionStats {
    pub total_addresses: u32,
    pub active_workers: u32,
    pub total_load: u32,
    pub max_worker_load: u32,
    pub min_worker_load: u32,
    pub load_imbalance: u32, // Ratio of max to min load
}

impl AddressDistributionStats {
    /// Check if load distribution is balanced
    pub fn is_balanced(&self) -> bool {
        // Consider balanced if load imbalance is less than 2:1 ratio
        self.load_imbalance < 2
    }
    
    /// Get utilization percentage (0-100)
    pub fn get_utilization(&self, total_capacity: u32) -> f32 {
        if total_capacity == 0 {
            0.0
        } else {
            (self.total_load as f32 / total_capacity as f32) * 100.0
        }
    }
}