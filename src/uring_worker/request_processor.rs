//! Immutable Request Processing Pipeline
//!
//! This module implements the immutable analysis phase of the processing pipeline.
//! All functions work directly with raw buffer pointers for zero-copy parsing.
//! No mutable operations are performed here - that's handled by decision_executor.rs.

use anyhow::Result;
use std::os::unix::io::RawFd;
use tracing::{debug, error};

use super::unified_http_parser::ConnectionType;
use super::unified_http_parser::UnifiedHttpParser;


/// Decision made during immutable request analysis phase
/// Contains all information needed to execute processing with single buffer acquisition
#[derive(Debug, Clone)]
pub enum ProcessingDecision {
    /// Forward HTTP request to backend server via TCP socket
    HttpForward {
        backend_addr: std::net::SocketAddr,
        connection_type: ConnectionType,
        buffer_ptr: *const u8,
        buffer_length: usize,
    },
    /// Forward TCP data to backend server (zero-copy)
    TcpForward {
        backend_addr: std::net::SocketAddr,
        buffer_ptr: *const u8,
        buffer_length: usize,
    },
    /// Send HTTP error response to client
    SendHttpError {
        error_code: u16,
        close_connection: bool,
    },
    /// Close connection cleanly
    CloseConnection,
}

/// Context information extracted from connection state for processing
#[derive(Debug, Clone)]
pub struct ProcessingContext {
    pub client_fd: RawFd,
    pub server_port: u16,
}


/// Immutable request processor with single-acquisition buffer lifecycle
pub struct RequestProcessor;

impl RequestProcessor {
    /// Unified request analysis with protocol auto-detection and worker-local backend selection
    /// Single entry point for all request processing (HTTP/TCP auto-detected)
    /// Works directly with raw buffer pointers for zero overhead
    pub fn analyze_request(
        routes: &crate::routing::RouteTable,
        worker_state: &mut crate::uring_worker::WorkerLoadBalancerState,
        buffer_ptr: *const u8,
        bytes_read: usize,
        context: &ProcessingContext,
    ) -> Result<ProcessingDecision> {
        // Validate buffer pointer before use
        Self::validate_buffer_pointer(buffer_ptr, bytes_read)?;
        
        // Create slice from raw pointer for protocol detection
        let request_data = unsafe { std::slice::from_raw_parts(buffer_ptr, bytes_read) };
        
        // Protocol auto-detection: HTTP requests start with method names
        if Self::is_http_request(request_data) {
            Self::analyze_http_request_internal(routes, worker_state, request_data, buffer_ptr, bytes_read, context)
        } else {
            Self::analyze_tcp_request_internal(routes, worker_state, buffer_ptr, bytes_read, context)
        }
    }
    
    /// Detect if request is HTTP based on first bytes
    #[inline(always)]
    fn is_http_request(data: &[u8]) -> bool {
        if data.len() < 3 { return false; }
        
        // Check for common HTTP methods
        data.starts_with(b"GET ") || 
        data.starts_with(b"POST ") || 
        data.starts_with(b"PUT ") || 
        data.starts_with(b"DELETE ") ||
        data.starts_with(b"HEAD ") ||
        data.starts_with(b"OPTIONS ") ||
        data.starts_with(b"PATCH ")
    }
    
    /// Validate buffer pointer safety before use
    #[inline(always)]
    fn validate_buffer_pointer(buffer_ptr: *const u8, buffer_length: usize) -> Result<()> {
        if buffer_ptr.is_null() {
            return Err(anyhow::anyhow!("Buffer pointer is null"));
        }
        if buffer_length == 0 {
            return Err(anyhow::anyhow!("Buffer length is zero"));
        }
        Ok(())
    }
    
    /// Compute route hash for load balancing counter selection
    #[inline(always)]
    fn compute_route_hash(host_hash: u64, path: &str) -> u64 {
        // Combine host hash and path hash for unique route identification
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        
        let mut hasher = DefaultHasher::new();
        host_hash.hash(&mut hasher);
        path.hash(&mut hasher);
        hasher.finish()
    }
    /// Internal HTTP request analysis (called by unified analyze_request)
    fn analyze_http_request_internal(
        routes: &crate::routing::RouteTable,
        worker_state: &mut crate::uring_worker::WorkerLoadBalancerState,
        request_data: &[u8],
        buffer_ptr: *const u8,
        bytes_read: usize,
        context: &ProcessingContext,
    ) -> Result<ProcessingDecision> {
        // Zero-copy header extraction using unified parser
        let request_info = UnifiedHttpParser::extract_routing_info(request_data)?;
        
        // Clean routing API: pure routing without backend selection
        let host_hash = routes.hash_host(request_info.host);
        let backend_pool = match routes.find_http_backend_pool(host_hash, request_info.path) {
            Ok(pool) => pool,
            Err(_) => {
                return Ok(ProcessingDecision::SendHttpError {
                    error_code: 404,
                    close_connection: request_info.connection_type == ConnectionType::Close,
                });
            }
        };
        
        // Worker-local backend selection (single codepath)
        if backend_pool.is_empty() {
            return Ok(ProcessingDecision::SendHttpError {
                error_code: 503,
                close_connection: request_info.connection_type == ConnectionType::Close,
            });
        }
        
        // Compute route hash for load balancing
        let route_hash = Self::compute_route_hash(host_hash, request_info.path);
        let backend_index = worker_state.select_backend_index(route_hash, backend_pool.backend_count());
        let selected_backend = backend_pool.get_backend(backend_index).unwrap();
        
        debug!("Processing HTTP request on connection (client_fd={})", context.client_fd);
        
        // Validate buffer pointer before storing
        Self::validate_buffer_pointer(buffer_ptr, bytes_read)?;
        
        Ok(ProcessingDecision::HttpForward {
            backend_addr: selected_backend.socket_addr,
            connection_type: request_info.connection_type,
            buffer_ptr,
            buffer_length: bytes_read,
        })
    }
    
    
    
    
    /// Internal TCP request analysis (called by unified analyze_request)
    fn analyze_tcp_request_internal(
        routes: &crate::routing::RouteTable,
        worker_state: &mut crate::uring_worker::WorkerLoadBalancerState,
        buffer_ptr: *const u8,
        bytes_read: usize,
        context: &ProcessingContext,
    ) -> Result<ProcessingDecision> {
        
        // Clean routing API: pure routing without backend selection
        let backend_pool = match routes.find_tcp_backend_pool(context.server_port) {
            Ok(pool) => pool,
            Err(_) => {
                error!("TCP routing failed for port {}: no route configured", context.server_port);
                return Ok(ProcessingDecision::CloseConnection);
            }
        };
        
        // Worker-local backend selection (single codepath)
        if backend_pool.is_empty() {
            error!("TCP routing failed for port {}: no backends available", context.server_port);
            return Ok(ProcessingDecision::CloseConnection);
        }
        
        // Use server port as route hash for TCP load balancing
        let route_hash = context.server_port as u64;
        let backend_index = worker_state.select_backend_index(route_hash, backend_pool.backend_count());
        let selected_backend = backend_pool.get_backend(backend_index).unwrap();
        
        debug!("TCP route found: port {} -> backend {}", 
               context.server_port, selected_backend.socket_addr);
        
        Ok(ProcessingDecision::TcpForward {
            backend_addr: selected_backend.socket_addr,
            buffer_ptr,
            buffer_length: bytes_read,
        })
    }
    
    
}


