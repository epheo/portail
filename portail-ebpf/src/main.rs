#![no_std]
#![no_main]

use aya_ebpf::{
    macros::{map, sk_reuseport},
    maps::{HashMap, ReusePortSockArray},
    programs::{SkReuseportContext, SK_PASS},
};

#[cfg(not(test))]
extern crate ebpf_panic;

#[repr(C)]
#[derive(Copy, Clone)]
struct PortWorkerConfig {
    base_index: u32,
    worker_count: u32,
}

#[map(name = "port_worker_config")]
static PORT_WORKER_CONFIG: HashMap<u16, PortWorkerConfig> =
    HashMap::with_max_entries(16, 0);

#[map(name = "worker_socket_array")]
static WORKER_SOCKET_ARRAY: ReusePortSockArray = ReusePortSockArray::with_max_entries(64, 0);

#[sk_reuseport]
pub fn address_aware_worker_selector(ctx: SkReuseportContext) -> u32 {
    let connection_hash = ctx.hash();
    let dest_port = extract_destination_port_safe(&ctx);

    let (base_worker_index, worker_count) = get_worker_range_for_port(dest_port);

    if worker_count == 0 {
        return SK_PASS;
    }

    let worker_offset = connection_hash % worker_count;
    let worker_index = base_worker_index + worker_offset;

    match ctx.select_socket(&WORKER_SOCKET_ARRAY, worker_index, 0) {
        Ok(()) => SK_PASS,
        Err(_) => SK_PASS,
    }
}

/// Extract destination port from TCP/UDP header with verifier-safe bounds checking
/// Returns the destination port or 0 if extraction fails
#[inline(always)]
fn extract_destination_port_safe(ctx: &SkReuseportContext) -> u16 {
    let data = ctx.data() as *const u8;
    let data_end = ctx.data_end() as *const u8;
    
    // Ensure we have at least 4 bytes for source port (2 bytes) + destination port (2 bytes)
    if unsafe { data.add(4) > data_end } {
        return 0; // Not enough data for port extraction
    }
    
    // TCP/UDP header format: [src_port:16][dst_port:16][...other fields]
    // Destination port is at bytes 2-3 (network byte order - big endian)
    let dest_port = unsafe {
        (((*data.add(2)) as u16) << 8) | (*data.add(3) as u16)
    };
    
    dest_port
}

#[inline(always)]
fn get_worker_range_for_port(port: u16) -> (u32, u32) {
    // SAFETY: Map is populated by control plane before workers start accepting.
    // Verifier requires the None check — satisfied by match arms.
    match unsafe { PORT_WORKER_CONFIG.get(&port) } {
        Some(config) => (config.base_index, config.worker_count),
        None => (0, 1),
    }
}