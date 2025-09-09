/// Unified eBPF maps for cross-program communication and metrics
/// Provides zero-overhead connection tracking and address-aware routing
/// Single codepath: all maps are required, no optional functionality

use anyhow::{anyhow, Result};
use aya::maps::{HashMap as EbpfHashMap, PerCpuArray, Array, MapData};
use aya::Pod;
use std::net::{IpAddr, Ipv4Addr};
use tracing::{debug, info, warn};

use super::{PacketMetadata, WorkerInfo, ProtocolType, WorkerSpecialization};

/// Connection tracking key for eBPF maps (must match C struct)
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct ConnectionKey {
    pub src_ip: u32,
    pub dst_ip: u32,
    pub src_port: u16,
    pub dst_port: u16,
    pub protocol: u8,
}

/// Enhanced connection information with address awareness (must match C struct)
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct ConnectionInfo {
    pub packet_count: u64,
    pub byte_count: u64,
    pub last_seen: u64,
    pub primary_protocol: u8,
    pub sub_protocol: u8,
    pub application_protocol: u8,
    pub connection_flags: u8,
    pub assigned_worker: u32,
    pub connection_state: u16,
    pub dest_port: u16,
}

/// Enhanced address-protocol combination for multi-protocol specialized routing
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct AddressProtocolKey {
    pub dest_addr: u32,
    pub primary_protocol: u8,
    pub sub_protocol: u8,
}

/// Port-address-protocol combination for Gateway API compliance
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct PortAddressProtocolKey {
    pub dest_addr: u32,     /* IPv4 address from Gateway spec */
    pub dest_port: u16,     /* Port from Gateway listener */
    pub primary_protocol: u8, /* Expected protocol on this (addr, port) */
}

/// Listener key for eBPF listener_protocol_map - matches eBPF struct listener_key
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct ListenerKey {
    pub address: u32,       /* IPv4 address in network byte order */
    pub port: u16,          /* Port in network byte order */
    pub padding: u16,       /* Ensure 8-byte alignment */
}

/// Gateway API protocol configuration
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct GatewayProtocolConfig {
    pub enabled_protocols: [u8; 8], /* Bit mask of supported protocols (MAX_PROTOCOLS_PER_PORT) */
    pub preferred_protocol: u8,    /* Default protocol for ambiguous cases */
    pub detection_mode: u8,       /* STRICT, PERMISSIVE, or CONTENT_BASED */
    pub worker_specialization: u8, /* Preferred worker type */
}

/// Per-worker metrics with address-specific statistics and debug fields
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct AddressAwareWorkerStats {
    pub total_packets: u64,
    pub total_bytes: u64,
    pub http_requests: u64,
    pub tcp_connections: u64,
    pub active_connections: u32,
    pub address_assignments: u32,
    // Debug fields for eBPF program troubleshooting
    pub last_selected_worker: u32,    // Most recent worker ID selected by eBPF
    pub last_protocol: u8,            // Most recent protocol detected
    pub last_dest_addr: u32,          // Most recent destination address processed
    pub debug_flags: u8,              // Debug status flags (fallback used, etc.)
    pub selection_failures: u16,      // Count of failed worker selections
}

// SAFETY: These structs are Plain Old Data (POD) types with only primitive fields
// and #[repr(C)] layout, making them safe to transmit to/from eBPF programs
unsafe impl Pod for ConnectionKey {}
unsafe impl Pod for ConnectionInfo {}
unsafe impl Pod for AddressProtocolKey {}
unsafe impl Pod for PortAddressProtocolKey {}
unsafe impl Pod for ListenerKey {}
unsafe impl Pod for GatewayProtocolConfig {}
unsafe impl Pod for AddressAwareWorkerStats {}

/// Optimized eBPF maps manager for maximum performance
/// Minimal maps required for SO_REUSEPORT worker selection
pub struct UnifiedEbpfMaps {
    /// Essential: Metadata transfer for XDP → SO_REUSEPORT communication
    metadata_transfer: PerCpuArray<MapData, PacketMetadata>,
    
    /// Optional: Protocol-to-worker specialization mapping (1K max entries)
    protocol_worker_map: Option<EbpfHashMap<MapData, AddressProtocolKey, u32>>,
    
    /// Optional: Gateway API protocol configuration mapping (100 max entries)  
    gateway_protocol_map: Option<EbpfHashMap<MapData, PortAddressProtocolKey, GatewayProtocolConfig>>,
    
    /// Listener protocol mapping for (address, port) → protocol lookup
    listener_protocol_map: Option<EbpfHashMap<MapData, ListenerKey, u8>>,
    
    /// Runtime configuration from YAML file
    runtime_config_map: Option<Array<MapData, u32>>,
    
    /// Dynamic: Worker capability mapping - populated from YAML configuration
    worker_capability_map: Option<Array<MapData, u8>>,
    
    /// Worker metrics and debug information
    worker_metrics: Option<PerCpuArray<MapData, AddressAwareWorkerStats>>,
}

impl UnifiedEbpfMaps {
    
    /// Initialize optimized eBPF maps - minimal required maps only
    pub fn initialize(
        metadata_transfer: PerCpuArray<MapData, PacketMetadata>,
    ) -> Self {
        debug!("Initializing optimized eBPF maps (minimal overhead for maximum performance)");
        
        Self {
            metadata_transfer,
            protocol_worker_map: None,
            gateway_protocol_map: None,
            listener_protocol_map: None,
            runtime_config_map: None,
            worker_capability_map: None,
            worker_metrics: None,
        }
    }
    
    /// Add optional protocol-worker mapping for advanced routing
    pub fn add_protocol_worker_map(
        &mut self,
        protocol_worker_map: EbpfHashMap<MapData, AddressProtocolKey, u32>,
    ) {
        debug!("Adding optional protocol-worker specialization map");
        self.protocol_worker_map = Some(protocol_worker_map);
    }
    
    /// Add optional Gateway API configuration mapping
    pub fn add_gateway_protocol_map(
        &mut self,
        gateway_protocol_map: EbpfHashMap<MapData, PortAddressProtocolKey, GatewayProtocolConfig>,
    ) {
        debug!("Adding optional Gateway API protocol configuration map");
        self.gateway_protocol_map = Some(gateway_protocol_map);
    }
    
    /// Add listener protocol mapping for (address, port) → protocol lookup
    pub fn add_listener_protocol_map(
        &mut self,
        listener_protocol_map: EbpfHashMap<MapData, ListenerKey, u8>,
    ) {
        debug!("Adding listener protocol mapping for address+port to protocol lookup");
        self.listener_protocol_map = Some(listener_protocol_map);
    }
    
    /// Add runtime configuration map for YAML config data
    pub fn add_runtime_config_map(
        &mut self,
        runtime_config_map: Array<MapData, u32>,
    ) {
        debug!("Adding runtime configuration map for YAML config data");
        self.runtime_config_map = Some(runtime_config_map);
    }
    
    /// Add worker capability mapping - populated from YAML configuration
    pub fn add_worker_capability_map(
        &mut self,
        worker_capability_map: Array<MapData, u8>,
    ) {
        debug!("Adding worker capability map for dynamic protocol validation");
        self.worker_capability_map = Some(worker_capability_map);
    }
    
    /// Add worker metrics map for debug monitoring
    pub fn add_worker_metrics_map(
        &mut self,
        worker_metrics: PerCpuArray<MapData, AddressAwareWorkerStats>,
    ) {
        debug!("Adding worker metrics map for debug monitoring and statistics");
        self.worker_metrics = Some(worker_metrics);
    }
    
    /// Get metadata from XDP program for SO_REUSEPORT worker selection
    pub fn get_packet_metadata(&self) -> Result<PacketMetadata> {
        let key = 0u32;
        let per_cpu_values = self.metadata_transfer.get(&key, 0)
            .map_err(|e| anyhow!("Failed to get packet metadata: {}", e))?;
        
        // Get the first CPU's value (all CPUs should have the same metadata for this use case)
        let metadata = per_cpu_values.iter().next()
            .ok_or_else(|| anyhow!("No metadata available from any CPU"))?;
        
        Ok(*metadata)
    }
    
    /// Update protocol-worker specialization mapping (optional feature)
    pub fn update_protocol_worker_mapping(&mut self, addr: IpAddr, protocol: u8, worker_id: u32) -> Result<()> {
        if let Some(ref mut map) = self.protocol_worker_map {
            let addr_u32 = match addr {
                IpAddr::V4(ipv4) => u32::from_be_bytes(ipv4.octets()),
                IpAddr::V6(_) => return Err(anyhow!("IPv6 not supported")),
            };
            
            let key = AddressProtocolKey {
                dest_addr: addr_u32,
                primary_protocol: protocol,
                sub_protocol: 0,
            };
            
            map.insert(&key, &worker_id, 0)
                .map_err(|e| anyhow!("Failed to update protocol-worker mapping: {}", e))?;
            
            debug!("Updated protocol-worker mapping: {}:{} -> worker {}", addr, protocol, worker_id);
        }
        Ok(())
    }
    
    /// Update worker capability mapping - sets protocol capability bitmask for a worker
    pub fn update_worker_capability(&mut self, worker_id: u32, capability_mask: u8) -> Result<()> {
        if let Some(ref mut map) = self.worker_capability_map {
            map.set(worker_id, capability_mask, 0)
                .map_err(|e| anyhow!("Failed to update worker capability mapping: {}", e))?;
            
            debug!("Updated worker capability: worker {} -> capability mask 0x{:02x}", 
                   worker_id, capability_mask);
        } else {
            return Err(anyhow!("Worker capability map not available"));
        }
        Ok(())
    }
    
    /// Update runtime configuration values from YAML config
    pub fn update_runtime_config(&mut self, worker_count: u32) -> Result<()> {
        if let Some(ref mut map) = self.runtime_config_map {
            // Define config keys matching eBPF program
            const CONFIG_WORKER_COUNT: u32 = 0;
            
            map.set(CONFIG_WORKER_COUNT, worker_count, 0)
                .map_err(|e| anyhow!("Failed to update runtime config: {}", e))?;
                
            debug!("Updated runtime config: worker_count={}", worker_count);
        } else {
            return Err(anyhow!("Runtime config map not available"));
        }
        Ok(())
    }
    
    /// Update listener protocol mapping from YAML configuration
    pub fn update_listener_protocol_map(&mut self, listeners: &[(std::net::IpAddr, u16, u8)]) -> Result<()> {
        if let Some(ref mut map) = self.listener_protocol_map {
            debug!("Updating listener protocol map with {} listener configurations", listeners.len());
            
            for &(addr, port, protocol_mask) in listeners {
                let key = ListenerKey {
                    address: match addr {
                        std::net::IpAddr::V4(ipv4) => u32::from_be_bytes(ipv4.octets()),
                        std::net::IpAddr::V6(_) => {
                            warn!("IPv6 not supported yet, skipping listener {}", addr);
                            continue;
                        }
                    },
                    port: port.to_be(), // Convert to network byte order
                    padding: 0,
                };
                
                map.insert(&key, protocol_mask, 0)
                    .map_err(|e| anyhow!("Failed to update listener protocol map for {}:{}: {}", addr, port, e))?;
                    
                debug!("Added listener protocol mapping: {}:{} → protocol_mask=0x{:02x}", 
                       addr, port, protocol_mask);
            }
            
            info!("Successfully updated listener protocol map with {} entries", listeners.len());
        } else {
            return Err(anyhow!("Listener protocol map not available"));
        }
        Ok(())
    }
    
    /// Read debug metrics from worker_metrics map for troubleshooting
    pub fn get_worker_debug_metrics(&self, worker_id: u32) -> Result<AddressAwareWorkerStats> {
        if let Some(ref metrics_map) = self.worker_metrics {
            let per_cpu_values = metrics_map.get(&worker_id, 0)
                .map_err(|e| anyhow!("Failed to get worker metrics for worker {}: {}", worker_id, e))?;
            
            // Get metrics from the first CPU (for simplicity)
            let metrics = per_cpu_values.iter().next()
                .ok_or_else(|| anyhow!("No metrics available for worker {} from any CPU", worker_id))?;
            
            Ok(*metrics)
        } else {
            Err(anyhow!("Worker metrics map not available - debug monitoring disabled"))
        }
    }
    
    /// Read debug metrics from all workers for comprehensive troubleshooting
    pub fn get_all_worker_debug_metrics(&self) -> Result<Vec<(u32, AddressAwareWorkerStats)>> {
        if let Some(ref metrics_map) = self.worker_metrics {
            let mut all_metrics = Vec::new();
            
            // Check up to 16 workers (should be sufficient for debugging)
            for worker_id in 0..16 {
                if let Ok(per_cpu_values) = metrics_map.get(&worker_id, 0) {
                    if let Some(metrics) = per_cpu_values.iter().next() {
                        // Include ALL workers for debugging - even with 0 packets
                        // This will reveal workers where eBPF map updates are failing
                        all_metrics.push((worker_id, *metrics));
                    }
                }
            }
            
            Ok(all_metrics)
        } else {
            Err(anyhow!("Worker metrics map not available - debug monitoring disabled"))
        }
    }
    
}

/// Helper functions for address conversion
pub fn ipv4_to_u32(addr: Ipv4Addr) -> u32 {
    u32::from_be_bytes(addr.octets())
}

pub fn u32_to_ipv4(addr: u32) -> Ipv4Addr {
    Ipv4Addr::from(addr.to_be_bytes())
}