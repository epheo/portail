//! Request Processing Pipeline
//!
//! Analyzes incoming data and produces routing decisions.
//! Works on byte slices — no I/O runtime dependency.

use anyhow::Result;
use std::net::SocketAddr;
use crate::logging::{debug, error};
use crate::http_parser::{ConnectionType, extract_routing_info};
use crate::routing::{self, RouteTable, HttpFilter, BackendSelector};

#[derive(Debug, Clone)]
pub struct HeaderModifications {
    pub add: Vec<routing::HttpHeader>,
    pub set: Vec<routing::HttpHeader>,
    pub remove: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum ProcessingDecision {
    HttpForward {
        backend_addr: SocketAddr,
        keepalive: bool,
        request_header_mods: Option<HeaderModifications>,
        response_header_mods: Option<HeaderModifications>,
    },
    HttpRedirect {
        status_code: u16,
        location: String,
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
    let keepalive = request_info.connection_type != ConnectionType::Close;

    let rule = match routes.find_http_route(request_info.host, request_info.path, request_info.header_data) {
        Ok(rule) => rule,
        Err(_) => {
            return Ok(ProcessingDecision::SendHttpError {
                error_code: 404,
                close_connection: !keepalive,
            });
        }
    };

    // Check for redirect filter first
    for filter in &rule.filters {
        if let HttpFilter::RequestRedirect { scheme, hostname, port, path, status_code } = filter {
            let location = build_redirect_location(
                scheme.as_deref(),
                hostname.as_deref().unwrap_or(request_info.host),
                *port,
                path.as_deref().unwrap_or(request_info.path),
                request_info.host,
            );
            return Ok(ProcessingDecision::HttpRedirect {
                status_code: *status_code,
                location,
                keepalive,
            });
        }
    }

    if rule.backends.is_empty() {
        return Ok(ProcessingDecision::SendHttpError {
            error_code: 503,
            close_connection: !keepalive,
        });
    }

    let host_hash = RouteTable::hash_host(request_info.host);
    let route_hash = compute_route_hash(host_hash, request_info.path);
    let backend_index = backend_selector.select_weighted_backend(route_hash, &rule.backends);
    let selected_backend = &rule.backends[backend_index];

    // Collect header modification filters
    let mut request_header_mods: Option<HeaderModifications> = None;
    let mut response_header_mods: Option<HeaderModifications> = None;

    for filter in &rule.filters {
        match filter {
            HttpFilter::RequestHeaderModifier { add, set, remove } => {
                request_header_mods = Some(HeaderModifications {
                    add: add.clone(),
                    set: set.clone(),
                    remove: remove.clone(),
                });
            }
            HttpFilter::ResponseHeaderModifier { add, set, remove } => {
                response_header_mods = Some(HeaderModifications {
                    add: add.clone(),
                    set: set.clone(),
                    remove: remove.clone(),
                });
            }
            HttpFilter::RequestRedirect { .. } => {} // already handled above
        }
    }

    Ok(ProcessingDecision::HttpForward {
        backend_addr: selected_backend.socket_addr,
        keepalive,
        request_header_mods,
        response_header_mods,
    })
}

fn build_redirect_location(
    scheme: Option<&str>,
    hostname: &str,
    port: Option<u16>,
    path: &str,
    original_host: &str,
) -> String {
    let scheme = scheme.unwrap_or("http");
    let host = if hostname == original_host { original_host } else { hostname };
    match port {
        Some(p) => format!("{}://{}:{}{}", scheme, host, p, path),
        None => format!("{}://{}{}", scheme, host, path),
    }
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
