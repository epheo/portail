//! Request Processing Pipeline
//!
//! Analyzes incoming data and produces routing decisions.
//! Works on byte slices — no I/O runtime dependency.

use anyhow::Result;
use std::net::SocketAddr;
use crate::logging::{debug, error};
use crate::http_parser::{ConnectionType, extract_routing_info};
use crate::routing::RouteTable;

#[derive(Debug, Clone)]
pub enum ProcessingDecision {
    HttpForward {
        backend_addr: SocketAddr,
        keepalive: bool,
    },
    TcpForward {
        backend_addr: SocketAddr,
    },
    UdpForward {
        backend_addr: SocketAddr,
    },
    SendHttpError {
        error_code: u16,
        close_connection: bool,
    },
    CloseConnection,
}

/// Round-robin backend selector
#[derive(Debug)]
pub struct BackendSelector {
    route_counters: fnv::FnvHashMap<u64, u32>,
}

impl BackendSelector {
    pub fn new() -> Self {
        Self {
            route_counters: fnv::FnvHashMap::default(),
        }
    }

    pub fn select_backend(&mut self, route_hash: u64, backend_count: usize) -> usize {
        let counter = self.route_counters.entry(route_hash).or_insert(0);
        let count = *counter as usize;
        *counter = counter.wrapping_add(1);

        if backend_count.is_power_of_two() {
            count & (backend_count - 1)
        } else {
            count % backend_count
        }
    }
}

pub fn analyze_request(
    routes: &RouteTable,
    backend_selector: &mut BackendSelector,
    request_data: &[u8],
    server_port: u16,
) -> Result<ProcessingDecision> {
    if request_data.is_empty() {
        return Ok(ProcessingDecision::CloseConnection);
    }

    if is_http_request(request_data) {
        analyze_http_request(routes, backend_selector, request_data)
    } else {
        analyze_tcp_request(routes, backend_selector, server_port)
    }
}

#[inline(always)]
fn is_http_request(data: &[u8]) -> bool {
    if data.len() < 3 { return false; }

    data.starts_with(b"GET ") ||
    data.starts_with(b"POST ") ||
    data.starts_with(b"PUT ") ||
    data.starts_with(b"DELETE ") ||
    data.starts_with(b"HEAD ") ||
    data.starts_with(b"OPTIONS ") ||
    data.starts_with(b"PATCH ")
}

#[inline(always)]
fn compute_route_hash(host_hash: u64, path: &str) -> u64 {
    const FNV_PRIME: u64 = 1099511628211;
    let mut hash = host_hash;
    for &byte in path.as_bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn analyze_http_request(
    routes: &RouteTable,
    backend_selector: &mut BackendSelector,
    request_data: &[u8],
) -> Result<ProcessingDecision> {
    let request_info = extract_routing_info(request_data)?;

    let backend_list = match routes.find_http_backends(request_info.host, request_info.path) {
        Ok(list) => list,
        Err(_) => {
            return Ok(ProcessingDecision::SendHttpError {
                error_code: 404,
                close_connection: request_info.connection_type == ConnectionType::Close,
            });
        }
    };

    if backend_list.is_empty() {
        return Ok(ProcessingDecision::SendHttpError {
            error_code: 503,
            close_connection: request_info.connection_type == ConnectionType::Close,
        });
    }

    let host_hash = RouteTable::hash_host(request_info.host);
    let route_hash = compute_route_hash(host_hash, request_info.path);
    let backend_index = backend_selector.select_backend(route_hash, backend_list.len());
    let selected_backend = &backend_list[backend_index];

    Ok(ProcessingDecision::HttpForward {
        backend_addr: selected_backend.socket_addr,
        keepalive: request_info.connection_type != ConnectionType::Close,
    })
}

pub fn analyze_udp_request(
    routes: &RouteTable,
    backend_selector: &mut BackendSelector,
    server_port: u16,
) -> Result<ProcessingDecision> {
    let backend_list = match routes.find_udp_backends(server_port) {
        Ok(list) => list,
        Err(_) => {
            error!("UDP routing failed for port {}: no route configured", server_port);
            return Ok(ProcessingDecision::CloseConnection);
        }
    };

    if backend_list.is_empty() {
        error!("UDP routing failed for port {}: no backends available", server_port);
        return Ok(ProcessingDecision::CloseConnection);
    }

    let route_hash = server_port as u64;
    let backend_index = backend_selector.select_backend(route_hash, backend_list.len());
    let selected_backend = &backend_list[backend_index];

    debug!("UDP route found: port {} -> backend {}",
           server_port, selected_backend.socket_addr);

    Ok(ProcessingDecision::UdpForward {
        backend_addr: selected_backend.socket_addr,
    })
}

fn analyze_tcp_request(
    routes: &RouteTable,
    backend_selector: &mut BackendSelector,
    server_port: u16,
) -> Result<ProcessingDecision> {
    let backend_list = match routes.find_tcp_backends(server_port) {
        Ok(list) => list,
        Err(_) => {
            error!("TCP routing failed for port {}: no route configured", server_port);
            return Ok(ProcessingDecision::CloseConnection);
        }
    };

    if backend_list.is_empty() {
        error!("TCP routing failed for port {}: no backends available", server_port);
        return Ok(ProcessingDecision::CloseConnection);
    }

    let route_hash = server_port as u64;
    let backend_index = backend_selector.select_backend(route_hash, backend_list.len());
    let selected_backend = &backend_list[backend_index];

    debug!("TCP route found: port {} -> backend {}",
           server_port, selected_backend.socket_addr);

    Ok(ProcessingDecision::TcpForward {
        backend_addr: selected_backend.socket_addr,
    })
}
