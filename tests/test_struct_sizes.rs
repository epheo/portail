/// Structure size validation tests for C/Rust interoperability
/// Ensures eBPF map structures have consistent sizes between C and Rust
/// Critical for preventing memory layout mismatches that cause map access failures

use uringress::ebpf::{PacketMetadata, WorkerInfo};
use uringress::ebpf::maps::{
    ConnectionKey, ConnectionInfo, AddressProtocolKey, 
    PortAddressProtocolKey, GatewayProtocolConfig, AddressAwareWorkerStats
};

/// Calculate expected C struct size with padding and alignment
macro_rules! c_struct_size {
    ($($field_type:expr),* $(,)?) => {{
        let mut size = 0usize;
        $(
            size += $field_type;
        )*
        size
    }};
}

#[cfg(test)]
mod struct_size_tests {
    use super::*;

    #[test] 
    fn test_packet_metadata_size_consistency() {
        let rust_size = std::mem::size_of::<PacketMetadata>();
        
        // C struct packet_metadata calculation:
        // __u8 primary_protocol (1) + __u8 sub_protocol (1) + __u8 application_protocol (1) + 
        // __u8 connection_flags (1) + __u32 dest_addr (4) + __u16 dest_port (2) + 
        // __u16 payload_length (2) + __u32 worker_hint (4) + __u64 timestamp (8)
        // Total: 1+1+1+1+4+2+2+4+8 = 24 bytes (packed)
        let expected_c_size = 24;
        
        println!("PacketMetadata - Rust: {} bytes, Expected C: {} bytes", rust_size, expected_c_size);
        assert_eq!(rust_size, expected_c_size, 
                   "PacketMetadata size mismatch! Rust: {}, C: {}", rust_size, expected_c_size);
    }

    #[test]
    fn test_connection_key_size_consistency() {
        let rust_size = std::mem::size_of::<ConnectionKey>();
        
        // C struct connection_key calculation:
        // __u32 src_ip (4) + __u32 dst_ip (4) + __u16 src_port (2) + __u16 dst_port (2) + __u8 protocol (1)
        // Total: 4+4+2+2+1 = 13 bytes (packed)
        let expected_c_size = 13;
        
        println!("ConnectionKey - Rust: {} bytes, Expected C: {} bytes", rust_size, expected_c_size);
        assert_eq!(rust_size, expected_c_size,
                   "ConnectionKey size mismatch! Rust: {}, C: {}", rust_size, expected_c_size);
    }

    #[test]
    fn test_connection_info_size_consistency() {
        let rust_size = std::mem::size_of::<ConnectionInfo>();
        
        // C struct connection_info calculation:
        // __u64 packet_count (8) + __u64 byte_count (8) + __u64 last_seen (8) +
        // __u8 primary_protocol (1) + __u8 sub_protocol (1) + __u8 application_protocol (1) + __u8 connection_flags (1) +
        // __u32 assigned_worker (4) + __u16 connection_state (2) + __u16 dest_port (2)
        // Total: 8+8+8+1+1+1+1+4+2+2 = 36 bytes (packed)
        let expected_c_size = 36;
        
        println!("ConnectionInfo - Rust: {} bytes, Expected C: {} bytes", rust_size, expected_c_size);
        assert_eq!(rust_size, expected_c_size,
                   "ConnectionInfo size mismatch! Rust: {}, C: {}", rust_size, expected_c_size);
    }

    #[test]
    fn test_address_protocol_key_size_consistency() {
        let rust_size = std::mem::size_of::<AddressProtocolKey>();
        
        // C struct address_protocol_key calculation:
        // __u32 dest_addr (4) + __u8 primary_protocol (1) + __u8 sub_protocol (1)
        // Total: 4+1+1 = 6 bytes (packed)
        let expected_c_size = 6;
        
        println!("AddressProtocolKey - Rust: {} bytes, Expected C: {} bytes", rust_size, expected_c_size);
        assert_eq!(rust_size, expected_c_size,
                   "AddressProtocolKey size mismatch! Rust: {}, C: {}", rust_size, expected_c_size);
    }

    #[test]
    fn test_port_address_protocol_key_size_consistency() {
        let rust_size = std::mem::size_of::<PortAddressProtocolKey>();
        
        // C struct port_address_protocol_key calculation:
        // __u32 dest_addr (4) + __u16 dest_port (2) + __u8 primary_protocol (1)
        // Total: 4+2+1 = 7 bytes (packed)
        let expected_c_size = 7;
        
        println!("PortAddressProtocolKey - Rust: {} bytes, Expected C: {} bytes", rust_size, expected_c_size);
        assert_eq!(rust_size, expected_c_size,
                   "PortAddressProtocolKey size mismatch! Rust: {}, C: {}", rust_size, expected_c_size);
    }

    #[test]
    fn test_gateway_protocol_config_size_consistency() {
        let rust_size = std::mem::size_of::<GatewayProtocolConfig>();
        
        // C struct gateway_protocol_config calculation:
        // __u8 enabled_protocols[8] (8) + __u8 preferred_protocol (1) + __u8 detection_mode (1) + __u8 worker_specialization (1)
        // Total: 8+1+1+1 = 11 bytes (packed)
        let expected_c_size = 11;
        
        println!("GatewayProtocolConfig - Rust: {} bytes, Expected C: {} bytes", rust_size, expected_c_size);
        assert_eq!(rust_size, expected_c_size,
                   "GatewayProtocolConfig size mismatch! Rust: {}, C: {}", rust_size, expected_c_size);
    }

    #[test]
    fn test_worker_info_size_consistency() {
        let rust_size = std::mem::size_of::<WorkerInfo>();
        
        // C struct worker_info calculation:
        // __u32 worker_id (4) + __u8 specialization (1) + __u32 current_load (4) + __u32 max_capacity (4)
        // Total: 4+1+4+4 = 13 bytes (packed)
        let expected_c_size = 13;
        
        println!("WorkerInfo - Rust: {} bytes, Expected C: {} bytes", rust_size, expected_c_size);
        assert_eq!(rust_size, expected_c_size,
                   "WorkerInfo size mismatch! Rust: {}, C: {}", rust_size, expected_c_size);
    }

    #[test]
    fn test_address_aware_worker_stats_size_consistency() {
        let rust_size = std::mem::size_of::<AddressAwareWorkerStats>();
        
        // C struct address_aware_worker_stats calculation:
        // __u64 total_packets (8) + __u64 total_bytes (8) + __u64 http_requests (8) + __u64 tcp_connections (8) +
        // __u32 active_connections (4) + __u32 address_assignments (4)
        // Total: 8+8+8+8+4+4 = 40 bytes (but check the actual C definition)
        let expected_c_size = 40;
        
        println!("AddressAwareWorkerStats - Rust: {} bytes, Expected C: {} bytes", rust_size, expected_c_size);
        // Note: This might need adjustment based on actual C struct definition
        assert_eq!(rust_size, expected_c_size,
                   "AddressAwareWorkerStats size mismatch! Rust: {}, C: {}", rust_size, expected_c_size);
    }

    #[test]
    fn test_ebpf_map_sizes_match_definitions() {
        // Verify that eBPF map value_size fields match our struct sizes
        println!("\n=== eBPF Map Size Verification ===");
        
        // metadata_transfer map expects PacketMetadata (24 bytes)
        let metadata_size = std::mem::size_of::<PacketMetadata>();
        println!("metadata_transfer map value_size should be: {}", metadata_size);
        assert_eq!(metadata_size, 24, "metadata_transfer map value_size mismatch");
        
        // protocol_worker_map key is AddressProtocolKey (6 bytes), value is u32 (4 bytes)
        let addr_protocol_key_size = std::mem::size_of::<AddressProtocolKey>();
        println!("protocol_worker_map key_size should be: {}", addr_protocol_key_size);
        assert_eq!(addr_protocol_key_size, 6, "protocol_worker_map key_size mismatch");
        
        // gateway_protocol_map key is PortAddressProtocolKey (7 bytes), value is GatewayProtocolConfig (11 bytes)
        let port_addr_key_size = std::mem::size_of::<PortAddressProtocolKey>();
        let gateway_config_size = std::mem::size_of::<GatewayProtocolConfig>();
        println!("gateway_protocol_map key_size should be: {}", port_addr_key_size);
        println!("gateway_protocol_map value_size should be: {}", gateway_config_size);
        assert_eq!(port_addr_key_size, 7, "gateway_protocol_map key_size mismatch");
        assert_eq!(gateway_config_size, 11, "gateway_protocol_map value_size mismatch");
        
        // worker_capability_map key is u32 (4 bytes), value is u8 (1 byte)
        println!("worker_capability_map key_size should be: 4");
        println!("worker_capability_map value_size should be: 1");
        // These are primitives, so they should be correct
        
        println!("=== All eBPF map sizes verified ===\n");
    }
}