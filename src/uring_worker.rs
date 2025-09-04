use anyhow::{anyhow, Result};
use io_uring::{opcode, types, IoUring};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::os::unix::io::{AsRawFd, RawFd};
use std::net::SocketAddr;
use tracing::{info, debug, warn, error};

use crate::routing::{RouteTable, Backend};
use crate::runtime_config::{RuntimeConfig, self as runtime_config};
use crate::parser::{ConnectionType, HttpResponseInfo};

/// Pre-generated HTTP error responses to eliminate allocations in hot paths
const HTTP_400_RESPONSE: &[u8] = b"HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nContent-Length: 15\r\nConnection: close\r\n\r\n400 Bad Request\n";
const HTTP_404_RESPONSE: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 13\r\nConnection: close\r\n\r\n404 Not Found\n";
const HTTP_500_RESPONSE: &[u8] = b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nContent-Length: 25\r\nConnection: close\r\n\r\n500 Internal Server Error\n";
const HTTP_502_RESPONSE: &[u8] = b"HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain\r\nContent-Length: 15\r\nConnection: close\r\n\r\n502 Bad Gateway\n";
const HTTP_503_RESPONSE: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: 23\r\nConnection: close\r\n\r\n503 Service Unavailable\n";

/// HTTP request parser state for zero-overhead parsing with keep-alive tracking
#[derive(Debug)]
pub struct HttpRequestParser {
    /// Number of requests processed on this connection
    pub requests_processed: u32,
    /// Whether keep-alive is enabled for this connection
    pub keep_alive_enabled: bool,
    /// Connection preference from last parsed request
    pub connection_preference: ConnectionType,
}

impl HttpRequestParser {
    pub fn new() -> Self {
        Self {
            requests_processed: 0,
            keep_alive_enabled: true, // Default to keep-alive for HTTP/1.1
            connection_preference: ConnectionType::Default,
        }
    }
    
    /// Check if connection should be kept alive based on request count and preferences
    
    /// Mark a request as processed and update keep-alive state
    #[inline(always)]
    pub fn mark_request_processed(&mut self, connection_type: ConnectionType) {
        self.requests_processed += 1;
        self.connection_preference = connection_type;
        
        // Disable keep-alive if explicitly requested to close
        if connection_type == ConnectionType::Close {
            self.keep_alive_enabled = false;
        }
    }
}

/// Raw io_uring based data plane worker
/// Handles HTTP and TCP proxying with zero-copy operations
pub struct UringWorker {
    worker_id: usize,
    ring: IoUring,
    routes: Arc<AtomicPtr<RouteTable>>,
    config: RuntimeConfig,
    listeners: HashMap<u16, RawFd>,
    connections: HashMap<u64, ConnectionState>,
    next_user_data: u64,
    buffer_ring: BufferRing,
    // Track active TCP connections to prevent double-close
    active_tcp_connections: HashSet<(RawFd, RawFd)>,
    // HTTP connection pool: backend_addr -> Vec<RawFd>
    http_connection_pool: HashMap<SocketAddr, Vec<RawFd>>,
    // Track backend addresses for HTTP connections: backend_fd -> backend_addr
    http_backend_addresses: HashMap<RawFd, SocketAddr>,
}

#[derive(Debug)]
enum ConnectionState {
    /// Listening socket waiting for connections
    Listening { port: u16, fd: RawFd },
    /// New connection detected, determining protocol
    DetectingProtocol { client_fd: RawFd, server_port: u16, buffer_id: u16 },
    /// HTTP connection processing request
    HttpConnection {
        client_fd: RawFd,
        server_port: u16,
        parser: HttpRequestParser,
        backend_fd: Option<RawFd>,
        buffer_id: u16,
    },
    /// TCP connection reading from client
    TcpReadingClient {
        client_fd: RawFd,
        backend_fd: RawFd,
        server_port: u16,
        buffer_id: u16,
    },
    /// TCP connection reading from backend
    TcpReadingBackend {
        client_fd: RawFd,
        backend_fd: RawFd,
        server_port: u16,
        buffer_id: u16,
    },
    /// TCP connection writing to backend (zero-copy forwarding)
    TcpWritingToBackend {
        client_fd: RawFd,
        backend_fd: RawFd,
        server_port: u16,
        buffer_id: u16,
        bytes_to_write: usize,
    },
    /// TCP connection writing to client (zero-copy forwarding)
    TcpWritingToClient {
        client_fd: RawFd,
        backend_fd: RawFd,
        server_port: u16,
        buffer_id: u16,
        bytes_to_write: usize,
    },
    /// HTTP error response being sent to client
    HttpErrorResponse {
        client_fd: RawFd,
        buffer_id: u16,
        response_size: usize,
    },
    /// HTTP connection waiting for next request (keep-alive)
    HttpWaitingForNextRequest {
        client_fd: RawFd,
        backend_fd: RawFd,
        server_port: u16,
        parser: HttpRequestParser,
        buffer_id: u16,
    },
    /// HTTP request being written to backend
    HttpWritingToBackend {
        client_fd: RawFd,
        backend_fd: RawFd,
        server_port: u16,
        buffer_id: u16,
        bytes_to_write: usize,
    },
    /// HTTP response being written to client
    HttpWritingToClient {
        client_fd: RawFd,
        backend_fd: RawFd,
        server_port: u16,
        buffer_id: u16,
        bytes_to_write: usize,
    },
    /// HTTP response being written to client with response info for keep-alive decision
    HttpWritingToClientWithResponseInfo {
        client_fd: RawFd,
        backend_fd: RawFd,
        server_port: u16,
        buffer_id: u16,
        bytes_to_write: usize,
        response_info: HttpResponseInfo,
    },
}

/// io_uring buffer ring for zero-overhead operations
/// Uses fixed buffers registered with the kernel for maximum performance
struct BufferRing {
    ring_id: u16,
    buffer_count: u16,
    buffer_size: u32,
    allocated_buffers: HashSet<u16>,
    next_buffer_id: u16,
    registered: bool,
    // Actual buffer memory allocated in userspace and registered with kernel
    buffers: Vec<Box<[u8]>>,
}

impl BufferRing {
    fn new(ring_id: u16, buffer_count: u16, buffer_size: u32) -> Self {
        // Pre-allocate all buffers in userspace for registration with kernel
        let mut buffers = Vec::with_capacity(buffer_count as usize);
        for _ in 0..buffer_count {
            // Allocate cache-line aligned buffers for optimal performance
            let buffer = vec![0u8; buffer_size as usize];
            buffers.push(buffer.into_boxed_slice());
        }
        
        Self {
            ring_id,
            buffer_count,
            buffer_size,
            allocated_buffers: HashSet::new(),
            next_buffer_id: 0,
            registered: false,
            buffers,
        }
    }
    
    /// Register buffer ring with io_uring kernel
    fn register_with_kernel(&mut self, ring: &mut IoUring) -> Result<()> {
        if self.registered {
            return Ok(());
        }
        
        // Create iovec array for fixed buffer registration
        let mut iovecs: Vec<libc::iovec> = Vec::with_capacity(self.buffer_count as usize);
        for buffer in &self.buffers {
            iovecs.push(libc::iovec {
                iov_base: buffer.as_ptr() as *mut libc::c_void,
                iov_len: buffer.len(),
            });
        }
        
        // Register fixed buffers with kernel using submitter
        let submitter = ring.submitter();
        unsafe {
            match submitter.register_buffers(&iovecs) {
                Ok(_) => {
                    self.registered = true;
                    info!("Fixed buffer ring {} registered with {} buffers of {} bytes each", 
                          self.ring_id, self.buffer_count, self.buffer_size);
                    Ok(())
                }
                Err(e) => {
                    Err(anyhow!("Failed to register fixed buffers: {}", e))
                }
            }
        }
    }
    
    /// Get next available buffer ID for io_uring operations
    fn get_buffer_id(&mut self) -> Option<u16> {
        // Find next available buffer ID
        for _ in 0..self.buffer_count {
            let buffer_id = self.next_buffer_id;
            self.next_buffer_id = (self.next_buffer_id + 1) % self.buffer_count;
            
            if !self.allocated_buffers.contains(&buffer_id) {
                self.allocated_buffers.insert(buffer_id);
                return Some(buffer_id);
            }
        }
        
        warn!("No available buffer IDs in ring {}", self.ring_id);
        None
    }
    
    /// Return buffer ID to available pool
    fn return_buffer_id(&mut self, buffer_id: u16) {
        self.allocated_buffers.remove(&buffer_id);
    }
    
    /// Get buffer pointer for io_uring operations
    fn get_buffer_ptr(&self, buffer_id: u16) -> *mut u8 {
        // Return actual buffer pointer for fixed buffer operations
        if buffer_id < self.buffer_count {
            self.buffers[buffer_id as usize].as_ptr() as *mut u8
        } else {
            error!("Invalid buffer_id {} >= buffer_count {}", buffer_id, self.buffer_count);
            std::ptr::null_mut()
        }
    }
    
    /// Get buffer content for reading from fixed buffers
    fn get_buffer_content(&self, buffer_id: u16, length: usize) -> &[u8] {
        if buffer_id < self.buffer_count {
            let buffer = &self.buffers[buffer_id as usize];
            let actual_length = std::cmp::min(length, buffer.len());
            &buffer[..actual_length]
        } else {
            error!("Invalid buffer_id {} >= buffer_count {}", buffer_id, self.buffer_count);
            &[]
        }
    }
    
    /// Get mutable buffer content for writing to fixed buffers
    fn get_buffer_content_mut(&mut self, buffer_id: u16) -> Option<&mut [u8]> {
        if buffer_id < self.buffer_count {
            Some(&mut self.buffers[buffer_id as usize])
        } else {
            error!("Invalid buffer_id {} >= buffer_count {}", buffer_id, self.buffer_count);
            None
        }
    }
    
    
}

impl UringWorker {
    pub fn new(
        worker_id: usize,
        routes: Arc<AtomicPtr<RouteTable>>,
        config: RuntimeConfig,
    ) -> Result<Self> {
        let mut ring = IoUring::new(256)?;
        let listeners = HashMap::new();
        let connections = HashMap::new();
        
        // Create buffer ring with 64 buffers of standard_buffer_size (8KB default)
        // This uses 64 * 8KB = 512KB per worker (2MB total for 4 workers)
        let mut buffer_ring = BufferRing::new(
            worker_id as u16,  // Use worker_id as ring_id for uniqueness
            64u16,             // 64 buffers for reasonable memory usage
            config.standard_buffer_size as u32,
        );
        
        // Register buffer ring with kernel
        buffer_ring.register_with_kernel(&mut ring)?;
        
        Ok(Self {
            worker_id,
            ring,
            routes,
            config,
            listeners,
            connections,
            next_user_data: 1,
            buffer_ring,
            active_tcp_connections: HashSet::new(),
            http_connection_pool: HashMap::new(),
            http_backend_addresses: HashMap::new(),
        })
    }
    
    /// Initialize listeners for all required ports
    pub fn setup_listeners(&mut self) -> Result<()> {
        // Get current route table to determine which ports to listen on
        let routes_ptr = self.routes.load(Ordering::Acquire);
        if routes_ptr.is_null() {
            return Err(anyhow!("Route table not initialized"));
        }
        
        let routes = unsafe { &*routes_ptr };
        
        // Listen on all configured Gateway listeners
        let listener_ports: Vec<u16> = self.config.listeners.keys().copied().collect();
        for port in listener_ports {
            self.setup_listener(port)?;
        }
        
        // Also listen on any additional TCP route ports not covered by Gateway listeners
        for &port in routes.get_tcp_ports().iter() {
            if !self.config.listeners.contains_key(&port) {
                self.setup_listener(port)?;
            }
        }
        
        info!("Worker {} setup listeners for ports: {:?}", 
              self.worker_id, self.listeners.keys().collect::<Vec<_>>());
        
        Ok(())
    }
    
    fn setup_listener(&mut self, port: u16) -> Result<()> {
        let addr = SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), port);
        
        // Create socket and set SO_REUSEPORT BEFORE binding
        let sock = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)?;
        
        #[cfg(target_os = "linux")]
        unsafe {
            let optval: libc::c_int = 1;
            let ret = libc::setsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_REUSEPORT,
                &optval as *const _ as *const libc::c_void,
                std::mem::size_of_val(&optval) as libc::socklen_t,
            );
            if ret != 0 {
                return Err(anyhow!("Failed to set SO_REUSEPORT"));
            }
        }
        
        // Set non-blocking and bind to address
        sock.set_nonblocking(true)?;
        sock.bind(&socket2::SockAddr::from(addr))?;
        sock.listen(128)?;
        
        let fd = sock.as_raw_fd();
        self.listeners.insert(port, fd);
        
        // Start accepting connections on this listener
        self.submit_accept(fd, port)?;
        
        debug!("Worker {} listening on port {}", self.worker_id, port);
        std::mem::forget(sock); // Keep socket alive
        
        Ok(())
    }
    
    fn submit_accept(&mut self, listener_fd: RawFd, port: u16) -> Result<()> {
        let user_data = self.next_user_data;
        self.next_user_data += 1;
        
        self.connections.insert(user_data, ConnectionState::Listening { port, fd: listener_fd });
        
        let accept_op = opcode::Accept::new(types::Fd(listener_fd), std::ptr::null_mut(), std::ptr::null_mut())
            .build()
            .user_data(user_data);
        
        unsafe {
            if self.ring.submission().push(&accept_op).is_err() {
                return Err(anyhow!("Failed to submit accept operation"));
            }
        }
        
        Ok(())
    }
    
    /// Main event loop for this worker
    pub fn run(&mut self) -> Result<()> {
        info!("Worker {} starting main event loop", self.worker_id);
        
        loop {
            // Submit pending operations
            self.ring.submit()?;
            
            // Wait for completions
            self.ring.submit_and_wait(1)?;
            
            // Process completions
            self.process_completions()?;
        }
    }
    
    fn process_completions(&mut self) -> Result<()> {
        // Collect completions first to avoid borrowing issues
        let completions: Vec<(u64, i32)> = {
            let cq = self.ring.completion();
            cq.map(|cqe| (cqe.user_data(), cqe.result())).collect()
        };
        
        // Process each completion
        for (user_data, result) in completions {
            if result < 0 {
                warn!("Operation failed with error: {}", result);
                
                // Properly clean up failed operations to prevent EBADF errors
                if let Some(connection_state) = self.connections.remove(&user_data) {
                    match connection_state {
                        ConnectionState::TcpReadingClient { client_fd, backend_fd, buffer_id, .. } |
                        ConnectionState::TcpReadingBackend { client_fd, backend_fd, buffer_id, .. } |
                        ConnectionState::TcpWritingToBackend { client_fd, backend_fd, buffer_id, .. } |
                        ConnectionState::TcpWritingToClient { client_fd, backend_fd, buffer_id, .. } => {
                            self.buffer_ring.return_buffer_id(buffer_id);
                            self.cleanup_tcp_connection(client_fd, backend_fd);
                        }
                        ConnectionState::HttpConnection { client_fd, backend_fd: Some(backend_fd), buffer_id, .. } |
                        ConnectionState::HttpWritingToBackend { client_fd, backend_fd, buffer_id, .. } |
                        ConnectionState::HttpWritingToClient { client_fd, backend_fd, buffer_id, .. } => {
                            self.buffer_ring.return_buffer_id(buffer_id);
                            self.cleanup_http_connection(client_fd, backend_fd);
                        }
                        ConnectionState::HttpConnection { client_fd, backend_fd: None, buffer_id, .. } |
                        ConnectionState::HttpErrorResponse { client_fd, buffer_id, .. } => {
                            self.buffer_ring.return_buffer_id(buffer_id);
                            unsafe { libc::close(client_fd) };
                        }
                        ConnectionState::DetectingProtocol { client_fd, buffer_id, .. } => {
                            self.buffer_ring.return_buffer_id(buffer_id);
                            unsafe { libc::close(client_fd) };
                        }
                        ConnectionState::Listening { .. } => {
                            // Listening socket failures are handled separately
                        }
                        ConnectionState::HttpWaitingForNextRequest { client_fd, backend_fd, buffer_id, .. } => {
                            // Keep-alive connection failed, clean up
                            self.buffer_ring.return_buffer_id(buffer_id);
                            self.cleanup_http_connection(client_fd, backend_fd);
                        }
                        ConnectionState::HttpWritingToClientWithResponseInfo { client_fd, backend_fd, buffer_id, .. } => {
                            // HTTP write with response info failed, clean up
                            self.buffer_ring.return_buffer_id(buffer_id);
                            self.cleanup_http_connection(client_fd, backend_fd);
                        }
                    }
                }
                continue;
            }
            
            if let Some(connection_state) = self.connections.remove(&user_data) {
                match connection_state {
                    ConnectionState::Listening { port, fd } => {
                        self.handle_accept(user_data, result, port, fd)?;
                    }
                    ConnectionState::DetectingProtocol { client_fd, server_port, buffer_id } => {
                        self.handle_protocol_detection(user_data, result, client_fd, server_port, buffer_id)?;
                    }
                    ConnectionState::HttpConnection { client_fd, server_port, parser, backend_fd, buffer_id } => {
                        self.handle_http_data(user_data, result, client_fd, server_port, parser, backend_fd, buffer_id)?;
                    }
                    ConnectionState::TcpReadingClient { client_fd, backend_fd, server_port, buffer_id } => {
                        self.handle_tcp_client_data(user_data, result, client_fd, backend_fd, server_port, buffer_id)?;
                    }
                    ConnectionState::TcpReadingBackend { client_fd, backend_fd, server_port, buffer_id } => {
                        self.handle_tcp_backend_data(user_data, result, client_fd, backend_fd, server_port, buffer_id)?;
                    }
                    ConnectionState::TcpWritingToBackend { client_fd, backend_fd, server_port, buffer_id, bytes_to_write } => {
                        self.handle_tcp_write_completion(user_data, result, client_fd, backend_fd, server_port, buffer_id, bytes_to_write, true)?;
                    }
                    ConnectionState::TcpWritingToClient { client_fd, backend_fd, server_port, buffer_id, bytes_to_write } => {
                        self.handle_tcp_write_completion(user_data, result, client_fd, backend_fd, server_port, buffer_id, bytes_to_write, false)?;
                    }
                    ConnectionState::HttpErrorResponse { client_fd, buffer_id, response_size } => {
                        self.handle_http_error_response_completion(user_data, result, client_fd, buffer_id, response_size)?;
                    }
                    ConnectionState::HttpWritingToBackend { client_fd, backend_fd, server_port, buffer_id, bytes_to_write } => {
                        self.handle_http_write_completion(user_data, result, client_fd, backend_fd, server_port, buffer_id, bytes_to_write, true)?;
                    }
                    ConnectionState::HttpWritingToClient { client_fd, backend_fd, server_port, buffer_id, bytes_to_write } => {
                        self.handle_http_write_completion(user_data, result, client_fd, backend_fd, server_port, buffer_id, bytes_to_write, false)?;
                    }
                    ConnectionState::HttpWaitingForNextRequest { client_fd, server_port, parser, backend_fd: _backend_fd, buffer_id } => {
                        // New HTTP request arrived on keep-alive connection
                        debug!("Processing keep-alive HTTP request: {} bytes on client_fd={}", result, client_fd);
                        
                        if result <= 0 {
                            debug!("Client closed keep-alive connection");
                            self.buffer_ring.return_buffer_id(buffer_id);
                            unsafe { libc::close(client_fd) };
                        } else {
                            // Process the HTTP request using the buffer from the read operation
                            self.handle_http_data(user_data, result, client_fd, server_port, parser, None, buffer_id)?;
                        }
                    }
                    ConnectionState::HttpWritingToClientWithResponseInfo { client_fd, backend_fd, server_port, buffer_id, bytes_to_write, response_info } => {
                        // HTTP write with response info completed - use keep-alive logic
                        self.handle_http_write_completion_with_response_info(user_data, result, client_fd, backend_fd, server_port, buffer_id, bytes_to_write, response_info)?;
                    }
                }
            }
        }
        
        Ok(())
    }
    
    fn handle_accept(&mut self, _user_data: u64, result: i32, port: u16, listener_fd: RawFd) -> Result<()> {
        let client_fd = result;
        debug!("Worker {} accepted connection fd={} on port {}", self.worker_id, client_fd, port);
        
        // Immediately submit another accept for this listener
        self.submit_accept(listener_fd, port)?;
        
        // Start protocol detection by reading initial data
        self.start_protocol_detection(client_fd, port)?;
        
        Ok(())
    }
    
    fn start_protocol_detection(&mut self, client_fd: RawFd, server_port: u16) -> Result<()> {
        // For TCP routes, skip protocol detection and go straight to TCP forwarding
        let routes_ptr = self.routes.load(Ordering::Acquire);
        if !routes_ptr.is_null() {
            let routes = unsafe { &*routes_ptr };
            if routes.has_tcp_route(server_port) {
                return self.start_tcp_forwarding(client_fd, server_port);
            }
        }
        
        // For HTTP or unknown, start with protocol detection
        if let Some(buffer_id) = self.buffer_ring.get_buffer_id() {
            let user_data = self.next_user_data;
            self.next_user_data += 1;
            
            self.connections.insert(user_data, ConnectionState::DetectingProtocol { 
                client_fd, 
                server_port,
                buffer_id,
            });
            
            let buffer_ptr = self.buffer_ring.get_buffer_ptr(buffer_id);
            let buffer_size = self.buffer_ring.buffer_size;
            let read_op = opcode::ReadFixed::new(types::Fd(client_fd), buffer_ptr, buffer_size, buffer_id)
                .build()
                .user_data(user_data);
            
            unsafe {
                if self.ring.submission().push(&read_op).is_err() {
                    return Err(anyhow!("Failed to submit read for protocol detection"));
                }
            }
        }
        
        Ok(())
    }
    
    fn handle_protocol_detection(&mut self, _user_data: u64, bytes_read: i32, client_fd: RawFd, server_port: u16, buffer_id: u16) -> Result<()> {
        if bytes_read <= 0 {
            debug!("Client closed connection during protocol detection");
            self.buffer_ring.return_buffer_id(buffer_id);
            unsafe { libc::close(client_fd) };
            return Ok(());
        }
        
        debug!("Protocol detection read {} bytes on port {}", bytes_read, server_port);
        
        
        // Gateway API listener-based protocol detection - single codepath execution
        match self.config.get_protocol(server_port) {
            Some(runtime_config::Protocol::HTTP) | Some(runtime_config::Protocol::HTTPS) => {
                // HTTP/HTTPS listener configured - handle as HTTP proxy
                self.start_http_processing_with_data(client_fd, server_port, buffer_id, bytes_read as usize)?;
            }
            Some(runtime_config::Protocol::TCP) | Some(runtime_config::Protocol::UDP) => {
                // TCP/UDP listener configured - handle as TCP proxy
                self.start_tcp_forwarding_with_data(client_fd, server_port, buffer_id, bytes_read as usize)?;
            }
            None => {
                // No listener configured for this port - reject connection
                error!("No listener configured for port {}", server_port);
                return Err(anyhow!("No listener configured for port {}", server_port));
            }
        }
        
        Ok(())
    }
    
    
    /// Start HTTP processing with initial data already read during protocol detection
    fn start_http_processing_with_data(&mut self, client_fd: RawFd, server_port: u16, buffer_id: u16, data_length: usize) -> Result<()> {
        debug!("Starting HTTP processing with {} bytes of initial data for fd={} on port {}", data_length, client_fd, server_port);
        
        // Process the initial data immediately instead of starting a new read
        let user_data = self.next_user_data;
        self.next_user_data += 1;
        
        let parser = HttpRequestParser::new();
        self.connections.insert(user_data, ConnectionState::HttpConnection {
            client_fd,
            server_port,
            parser,
            backend_fd: None,
            buffer_id,
        });
        
        // Remove the connection from map to extract the parser and avoid borrowing issues
        if let Some(ConnectionState::HttpConnection { parser, .. }) = self.connections.remove(&user_data) {
            // Process the HTTP data that was already read
            self.handle_http_data(user_data, data_length as i32, client_fd, server_port, parser, None, buffer_id)?;
        } else {
            error!("Failed to extract HTTP connection state");
            self.buffer_ring.return_buffer_id(buffer_id);
            unsafe { libc::close(client_fd) };
            return Err(anyhow!("Failed to extract HTTP connection state"));
        }
        
        Ok(())
    }
    
    fn start_tcp_forwarding(&mut self, client_fd: RawFd, server_port: u16) -> Result<()> {
        debug!("Starting TCP forwarding for fd={} on port {}", client_fd, server_port);
        
        // Get backend for this TCP route
        let routes_ptr = self.routes.load(Ordering::Acquire);
        if routes_ptr.is_null() {
            error!("Route table not available for TCP forwarding");
            unsafe { libc::close(client_fd) };
            return Ok(());
        }
        
        let routes = unsafe { &*routes_ptr };
        let backend = match routes.route_tcp_request(server_port) {
            Ok(backend) => backend,
            Err(e) => {
                error!("No TCP route found for port {}: {}", server_port, e);
                unsafe { libc::close(client_fd) };
                return Ok(());
            }
        };
        
        // Connect to backend
        let backend_fd = self.connect_to_backend(backend)?;
        
        // Start bidirectional forwarding by reading from both sides
        self.start_bidirectional_forwarding(client_fd, backend_fd)?;
        
        Ok(())
    }
    
    /// Start TCP forwarding with initial data already read during protocol detection
    fn start_tcp_forwarding_with_data(&mut self, client_fd: RawFd, server_port: u16, buffer_id: u16, data_length: usize) -> Result<()> {
        debug!("Starting TCP forwarding with {} bytes of initial data for fd={} on port {}", data_length, client_fd, server_port);
        
        // Get backend for this TCP route
        let routes_ptr = self.routes.load(Ordering::Acquire);
        if routes_ptr.is_null() {
            error!("Route table not available for TCP forwarding");
            self.buffer_ring.return_buffer_id(buffer_id);
            unsafe { libc::close(client_fd) };
            return Ok(());
        }
        
        let routes = unsafe { &*routes_ptr };
        let backend = match routes.route_tcp_request(server_port) {
            Ok(backend) => backend,
            Err(e) => {
                error!("No TCP route found for port {}: {}", server_port, e);
                self.buffer_ring.return_buffer_id(buffer_id);
                unsafe { libc::close(client_fd) };
                return Ok(());
            }
        };
        
        // Connect to backend
        let backend_fd = match self.connect_to_backend(backend) {
            Ok(fd) => fd,
            Err(e) => {
                error!("Failed to connect to TCP backend: {}", e);
                self.buffer_ring.return_buffer_id(buffer_id);
                unsafe { libc::close(client_fd) };
                return Ok(());
            }
        };
        
        // Track this connection
        self.active_tcp_connections.insert((client_fd, backend_fd));
        
        // Forward the initial data to backend immediately
        self.submit_zero_copy_write_to_backend(client_fd, backend_fd, server_port, buffer_id, data_length)?;
        
        // Start reading from both directions for ongoing traffic
        self.submit_client_read(client_fd, backend_fd, server_port)?;
        self.submit_backend_read(client_fd, backend_fd, server_port)?;
        
        Ok(())
    }
    
    fn connect_to_backend(&self, backend: &Backend) -> Result<RawFd> {
        self.connect_to_backend_addr(backend.socket_addr)
    }

    /// Connect to backend using socket address directly (zero-allocation)
    fn connect_to_backend_addr(&self, backend_socket_addr: SocketAddr) -> Result<RawFd> {
        let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)?;
        // Use blocking connect for simplicity in this implementation
        socket.set_nonblocking(false)?;
        
        let addr = socket2::SockAddr::from(backend_socket_addr);
        
        match socket.connect(&addr) {
            Ok(()) => {
                let fd = socket.as_raw_fd();
                std::mem::forget(socket); // Keep socket alive
                
                // Set to non-blocking for io_uring operations
                unsafe {
                    let flags = libc::fcntl(fd, libc::F_GETFL);
                    libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
                
                debug!("Successfully connected to backend {} on fd={}", backend_socket_addr, fd);
                Ok(fd)
            }
            Err(e) => {
                error!("Failed to connect to backend {}: {}", backend_socket_addr, e);
                Err(anyhow!("Backend connection failed: {}", e))
            }
        }
    }
    
    /// Get a pooled HTTP connection or create a new one
    fn get_or_create_http_connection(&mut self, backend: &Backend) -> Result<RawFd> {
        self.get_or_create_http_connection_to_addr(backend.socket_addr)
    }

    /// Get or create HTTP connection using socket address directly (zero-allocation)
    fn get_or_create_http_connection_to_addr(&mut self, backend_socket_addr: SocketAddr) -> Result<RawFd> {
        // Try to get a connection from the pool first
        if let Some(pool) = self.http_connection_pool.get_mut(&backend_socket_addr) {
            if let Some(fd) = pool.pop() {
                // Verify the connection is still valid by checking if it's writable
                let mut poll_fd = libc::pollfd {
                    fd,
                    events: libc::POLLOUT | libc::POLLHUP | libc::POLLERR,
                    revents: 0,
                };
                
                unsafe {
                    let poll_result = libc::poll(&mut poll_fd, 1, 0);
                    if poll_result > 0 && (poll_fd.revents & (libc::POLLHUP | libc::POLLERR)) == 0 {
                        debug!("Reusing pooled HTTP connection to {} on fd={}", backend_socket_addr, fd);
                        return Ok(fd);
                    } else {
                        debug!("Pooled HTTP connection to {} on fd={} is stale, closing", backend_socket_addr, fd);
                        libc::close(fd);
                        self.http_backend_addresses.remove(&fd);
                    }
                }
            }
        }
        
        // No valid pooled connection available, create a new one
        debug!("Creating new HTTP connection to {}", backend_socket_addr);
        let backend_fd = self.connect_to_backend_addr(backend_socket_addr)?;
        self.http_backend_addresses.insert(backend_fd, backend_socket_addr);
        Ok(backend_fd)
    }
    
    /// Return an HTTP connection to the pool for reuse
    fn return_http_connection_to_pool(&mut self, backend_fd: RawFd) {
        const MAX_POOLED_CONNECTIONS_PER_BACKEND: usize = 32;
        
        if let Some(&backend_addr) = self.http_backend_addresses.get(&backend_fd) {
            let pool = self.http_connection_pool.entry(backend_addr).or_insert_with(Vec::new);
            
            if pool.len() < MAX_POOLED_CONNECTIONS_PER_BACKEND {
                debug!("Returning HTTP connection to {} (fd={}) to pool", backend_addr, backend_fd);
                pool.push(backend_fd);
                return;
            } else {
                debug!("HTTP connection pool for {} is full, closing connection fd={}", backend_addr, backend_fd);
            }
        } else {
            debug!("Backend address not found for fd={}, closing connection", backend_fd);
        }
        
        // Connection not pooled, close it
        unsafe { libc::close(backend_fd); }
        self.http_backend_addresses.remove(&backend_fd);
    }
    
    /// Close HTTP connection and cleanup tracking
    fn close_http_connection(&mut self, backend_fd: RawFd) {
        unsafe { libc::close(backend_fd); }
        self.http_backend_addresses.remove(&backend_fd);
    }
    
    /// Updated HTTP connection cleanup that supports pooling
    fn cleanup_http_connection_pooled(&mut self, client_fd: RawFd, backend_fd: RawFd, should_pool: bool) {
        debug!("Cleaning up HTTP connection: client_fd={}, backend_fd={}, should_pool={}", client_fd, backend_fd, should_pool);
        
        unsafe { libc::close(client_fd); }
        
        if should_pool {
            self.return_http_connection_to_pool(backend_fd);
        } else {
            self.close_http_connection(backend_fd);
        }
    }
    
    fn start_bidirectional_forwarding(&mut self, client_fd: RawFd, backend_fd: RawFd) -> Result<()> {
        debug!("Starting bidirectional forwarding between fd={} and fd={}", client_fd, backend_fd);
        
        // Track this connection to prevent double-close
        self.active_tcp_connections.insert((client_fd, backend_fd));
        
        // Submit reads for both directions with proper state tracking
        self.submit_client_read(client_fd, backend_fd, 0)?;
        self.submit_backend_read(client_fd, backend_fd, 0)?;
        
        Ok(())
    }
    
    /// Safe connection cleanup that prevents double-close
    fn cleanup_tcp_connection(&mut self, client_fd: RawFd, backend_fd: RawFd) {
        let connection_pair = (client_fd, backend_fd);
        if self.active_tcp_connections.remove(&connection_pair) {
            debug!("Cleaning up TCP connection: client_fd={}, backend_fd={}", client_fd, backend_fd);
            unsafe {
                libc::close(client_fd);
                libc::close(backend_fd);
            }
        } else {
            debug!("TCP connection already cleaned up: client_fd={}, backend_fd={}", client_fd, backend_fd);
        }
    }
    
    fn submit_client_read(&mut self, client_fd: RawFd, backend_fd: RawFd, server_port: u16) -> Result<()> {
        if let Some(buffer_id) = self.buffer_ring.get_buffer_id() {
            let user_data = self.next_user_data;
            self.next_user_data += 1;
            
            // Store connection state with buffer ID
            self.connections.insert(user_data, ConnectionState::TcpReadingClient {
                client_fd,
                backend_fd,
                server_port,
                buffer_id,
            });
            
            // Get buffer from ring using buffer_id
            let buffer_ptr = self.buffer_ring.get_buffer_ptr(buffer_id);
            let buffer_size = self.buffer_ring.buffer_size;
            
            let read_op = opcode::ReadFixed::new(
                types::Fd(client_fd), 
                buffer_ptr,
                buffer_size,
                buffer_id
            )
            .build()
            .user_data(user_data);
            
            unsafe {
                if self.ring.submission().push(&read_op).is_err() {
                    return Err(anyhow!("Failed to submit client read"));
                }
            }
        }
        Ok(())
    }
    
    fn submit_backend_read(&mut self, client_fd: RawFd, backend_fd: RawFd, server_port: u16) -> Result<()> {
        if let Some(buffer_id) = self.buffer_ring.get_buffer_id() {
            let user_data = self.next_user_data;
            self.next_user_data += 1;
            
            // Store connection state with buffer ID
            self.connections.insert(user_data, ConnectionState::TcpReadingBackend {
                client_fd,
                backend_fd,
                server_port,
                buffer_id,
            });
            
            // Get buffer from ring using buffer_id
            let buffer_ptr = self.buffer_ring.get_buffer_ptr(buffer_id);
            let buffer_size = self.buffer_ring.buffer_size;
            
            let read_op = opcode::ReadFixed::new(
                types::Fd(backend_fd), 
                buffer_ptr,
                buffer_size,
                buffer_id
            )
            .build()
            .user_data(user_data);
            
            unsafe {
                if self.ring.submission().push(&read_op).is_err() {
                    return Err(anyhow!("Failed to submit backend read"));
                }
            }
        }
        Ok(())
    }
    
    fn handle_http_data(&mut self, user_data: u64, bytes_read: i32, client_fd: RawFd, server_port: u16, mut parser: HttpRequestParser, backend_fd: Option<RawFd>, buffer_id: u16) -> Result<()> {
        if bytes_read <= 0 {
            debug!("Client closed HTTP connection");
            self.buffer_ring.return_buffer_id(buffer_id);
            if let Some(backend_fd) = backend_fd {
                self.cleanup_tcp_connection(client_fd, backend_fd);
            } else {
                unsafe { libc::close(client_fd) };
            }
            return Ok(());
        }
        
        debug!("Processing HTTP request: {} bytes", bytes_read);
        
        // Get buffer content for HTTP parsing from fixed buffers
        let request_data = self.buffer_ring.get_buffer_content(buffer_id, bytes_read as usize);
        
        // Parse HTTP request using actual buffer content
        match crate::parser::parse_http_headers_fast(request_data) {
            Ok((method, path, host, connection_type)) => {
                debug!("Parsed HTTP request: {} {} Host: {:?} Connection: {:?}", method, path, host, connection_type);
                
                // Route the HTTP request
                debug!("Attempting to route HTTP request to host: {:?}, path: {}", host, path);
                match self.route_http_request(host, path) {
                    Ok(backend) => {
                        // Extract socket address to avoid borrow checker issues
                        let backend_socket_addr = backend.socket_addr;
                        
                        // Update parser state with connection preference
                        parser.mark_request_processed(connection_type);
                        
                        if let Some(existing_backend_fd) = backend_fd {
                            // Already connected to a backend, forward the request  
                            self.forward_http_request_to_backend(user_data, client_fd, existing_backend_fd, server_port, buffer_id, bytes_read as usize, parser)?;
                        } else {
                            // Need to connect to backend first - use socket address directly
                            self.connect_and_forward_http_request_to_addr(user_data, client_fd, server_port, backend_socket_addr, buffer_id, bytes_read as usize, parser)?;
                        }
                    }
                    Err(e) => {
                        error!("Failed to route HTTP request: {}", e);
                        self.send_http_error_response(client_fd, 404, "Not Found")?;
                        self.buffer_ring.return_buffer_id(buffer_id);
                        unsafe { libc::close(client_fd) };
                    }
                }
            }
            Err(e) => {
                error!("Failed to parse HTTP request: {}", e);
                self.send_http_error_response(client_fd, 400, "Bad Request")?;
                self.buffer_ring.return_buffer_id(buffer_id);
                unsafe { libc::close(client_fd) };
            }
        }
        
        Ok(())
    }
    
    fn handle_tcp_client_data(&mut self, _user_data: u64, bytes_read: i32, client_fd: RawFd, backend_fd: RawFd, server_port: u16, buffer_id: u16) -> Result<()> {
        if bytes_read <= 0 {
            debug!("Client closed connection, cleaning up");
            self.buffer_ring.return_buffer_id(buffer_id);
            self.cleanup_tcp_connection(client_fd, backend_fd);
            return Ok(());
        }
        
        debug!("Client->Backend: Forwarding {} bytes", bytes_read);
        
        // Zero-copy write: transfer buffer ownership to write operation
        self.submit_zero_copy_write_to_backend(client_fd, backend_fd, server_port, buffer_id, bytes_read as usize)?;
        
        // Continue reading from client
        self.submit_client_read(client_fd, backend_fd, server_port)?;
        
        Ok(())
    }
    
    fn handle_tcp_backend_data(&mut self, _user_data: u64, bytes_read: i32, client_fd: RawFd, backend_fd: RawFd, server_port: u16, buffer_id: u16) -> Result<()> {
        if bytes_read <= 0 {
            debug!("Backend closed connection, cleaning up");
            self.buffer_ring.return_buffer_id(buffer_id);
            self.cleanup_tcp_connection(client_fd, backend_fd);
            return Ok(());
        }
        
        debug!("Backend->Client: Forwarding {} bytes", bytes_read);
        
        // Check if this is an HTTP connection - use Gateway API protocol detection
        let is_http = matches!(self.config.get_protocol(server_port), 
                              Some(runtime_config::Protocol::HTTP) | Some(runtime_config::Protocol::HTTPS));
        debug!("handle_tcp_backend_data: server_port={}, is_http={}", server_port, is_http);
        
        if is_http {
            // HTTP response - parse headers to check for keep-alive support
            let response_data = self.buffer_ring.get_buffer_content(buffer_id, bytes_read as usize);
            
            // Debug: Log the actual response data to understand why parsing is failing
            let preview_len = std::cmp::min(200, response_data.len());
            let response_preview = String::from_utf8_lossy(&response_data[..preview_len]);
            debug!("HTTP response preview ({} bytes total): {:?}", bytes_read, response_preview);
            
            match crate::parser::parse_http_response_headers(response_data) {
                Ok(response_info) => {
                    debug!("Parsed HTTP response: status={}, connection={:?}, content_length={:?}, complete={}", 
                           response_info.status_code, response_info.connection_type, 
                           response_info.content_length, response_info.is_complete);
                    
                    // Forward response to client with connection info for keep-alive decision
                    self.submit_http_write_to_client_with_response_info(client_fd, backend_fd, server_port, buffer_id, bytes_read as usize, response_info)?;
                }
                Err(e) => {
                    warn!("Failed to parse HTTP response headers: {}", e);
                    debug!("Response content (first {} bytes): {:?}", preview_len, response_preview);
                    
                    // Create fallback response info that defaults to closing connection
                    // This is safer than the old fallback that completely bypassed keep-alive logic
                    let fallback_response_info = HttpResponseInfo {
                        status_code: 200, // Assume success if we can't parse
                        connection_type: ConnectionType::Close, // Close connection on parse failure for safety
                        content_length: Some(bytes_read as usize), // Use actual response size
                        is_complete: true, // Assume complete since we have the data
                    };
                    
                    debug!("Using fallback response info with connection close for safety");
                    self.submit_http_write_to_client_with_response_info(client_fd, backend_fd, server_port, buffer_id, bytes_read as usize, fallback_response_info)?;
                }
            }
            // No need to continue reading - HTTP response complete
        } else {
            // TCP traffic - use TCP write (persistent connection)
            self.submit_zero_copy_write_to_client(client_fd, backend_fd, server_port, buffer_id, bytes_read as usize)?;
            // Continue reading from backend for ongoing TCP traffic
            self.submit_backend_read(client_fd, backend_fd, server_port)?;
        }
        
        Ok(())
    }
    
    /// Zero-copy write to backend using io_uring (TCP)
    fn submit_zero_copy_write_to_backend(&mut self, client_fd: RawFd, backend_fd: RawFd, server_port: u16, buffer_id: u16, bytes_to_write: usize) -> Result<()> {
        let user_data = self.next_user_data;
        self.next_user_data += 1;
        
        // Store write state with buffer ID
        self.connections.insert(user_data, ConnectionState::TcpWritingToBackend {
            client_fd,
            backend_fd,
            server_port,
            buffer_id,
            bytes_to_write,
        });
        
        // Get buffer reference for write operation
        let buffer_ptr = self.buffer_ring.get_buffer_ptr(buffer_id);
        let _buffer_size = self.buffer_ring.buffer_size;
        
        let write_op = opcode::WriteFixed::new(
            types::Fd(backend_fd),
            buffer_ptr,
            bytes_to_write as u32,
            buffer_id
        )
        .build()
        .user_data(user_data);
        
        unsafe {
            if self.ring.submission().push(&write_op).is_err() {
                return Err(anyhow!("Failed to submit write to backend"));
            }
        }
        
        Ok(())
    }
    
    /// Zero-copy write to backend using io_uring (HTTP) 
    fn submit_http_write_to_backend(&mut self, client_fd: RawFd, backend_fd: RawFd, server_port: u16, buffer_id: u16, bytes_to_write: usize) -> Result<()> {
        let user_data = self.next_user_data;
        self.next_user_data += 1;
        
        // Store HTTP write state with buffer ID
        self.connections.insert(user_data, ConnectionState::HttpWritingToBackend {
            client_fd,
            backend_fd,
            server_port,
            buffer_id,
            bytes_to_write,
        });
        
        // Get buffer reference for write operation
        let buffer_ptr = self.buffer_ring.get_buffer_ptr(buffer_id);
        let _buffer_size = self.buffer_ring.buffer_size;
        
        let write_op = opcode::WriteFixed::new(
            types::Fd(backend_fd),
            buffer_ptr,
            bytes_to_write as u32,
            buffer_id
        )
        .build()
        .user_data(user_data);
        
        unsafe {
            if self.ring.submission().push(&write_op).is_err() {
                return Err(anyhow!("Failed to submit HTTP write to backend"));
            }
        }
        
        Ok(())
    }
    
    /// Zero-copy write to client using io_uring (TCP)
    fn submit_zero_copy_write_to_client(&mut self, client_fd: RawFd, backend_fd: RawFd, server_port: u16, buffer_id: u16, bytes_to_write: usize) -> Result<()> {
        let user_data = self.next_user_data;
        self.next_user_data += 1;
        
        // Store write state with buffer ID
        self.connections.insert(user_data, ConnectionState::TcpWritingToClient {
            client_fd,
            backend_fd,
            server_port,
            buffer_id,
            bytes_to_write,
        });
        
        // Get buffer reference for write operation
        let buffer_ptr = self.buffer_ring.get_buffer_ptr(buffer_id);
        let _buffer_size = self.buffer_ring.buffer_size;
        
        let write_op = opcode::WriteFixed::new(
            types::Fd(client_fd),
            buffer_ptr,
            bytes_to_write as u32,
            buffer_id
        )
        .build()
        .user_data(user_data);
        
        unsafe {
            if self.ring.submission().push(&write_op).is_err() {
                return Err(anyhow!("Failed to submit write to client"));
            }
        }
        
        Ok(())
    }
    
    
    /// Zero-copy write to client using io_uring (HTTP with response info for keep-alive)
    fn submit_http_write_to_client_with_response_info(&mut self, client_fd: RawFd, backend_fd: RawFd, server_port: u16, buffer_id: u16, bytes_to_write: usize, response_info: HttpResponseInfo) -> Result<()> {
        let user_data = self.next_user_data;
        self.next_user_data += 1;
        
        // Store HTTP write state with response info for keep-alive decision
        self.connections.insert(user_data, ConnectionState::HttpWritingToClientWithResponseInfo {
            client_fd,
            backend_fd,
            server_port,
            buffer_id,
            bytes_to_write,
            response_info,
        });
        
        // Get buffer reference for write operation
        let buffer_ptr = self.buffer_ring.get_buffer_ptr(buffer_id);
        let _buffer_size = self.buffer_ring.buffer_size;
        
        let write_op = opcode::WriteFixed::new(
            types::Fd(client_fd),
            buffer_ptr,
            bytes_to_write as u32,
            buffer_id
        )
        .build()
        .user_data(user_data);
        
        unsafe {
            if self.ring.submission().push(&write_op).is_err() {
                return Err(anyhow!("Failed to submit HTTP write to client with response info"));
            }
        }
        
        Ok(())
    }
    
    /// Handle completion of io_uring write operations with proper partial write handling
    fn handle_tcp_write_completion(&mut self, _user_data: u64, bytes_written: i32, client_fd: RawFd, backend_fd: RawFd, _server_port: u16, buffer_id: u16, expected_bytes: usize, to_backend: bool) -> Result<()> {
        if bytes_written < 0 {
            error!("Write operation failed with error: {}", bytes_written);
            self.buffer_ring.return_buffer_id(buffer_id);
            self.cleanup_tcp_connection(client_fd, backend_fd);
            return Ok(());
        }
        
        let written = bytes_written as usize;
        if written < expected_bytes {
            // Partial write - continue writing remaining data
            warn!("Partial write: {} bytes written, {} expected, continuing write", written, expected_bytes);
            
            let remaining_bytes = expected_bytes - written;
            let user_data = self.next_user_data;
            self.next_user_data += 1;
            
            // Update connection state to continue writing from offset
            let connection_state = if to_backend {
                ConnectionState::TcpWritingToBackend {
                    client_fd,
                    backend_fd,
                    server_port: _server_port,
                    buffer_id,
                    bytes_to_write: remaining_bytes,
                }
            } else {
                ConnectionState::TcpWritingToClient {
                    client_fd,
                    backend_fd,
                    server_port: _server_port,
                    buffer_id,
                    bytes_to_write: remaining_bytes,
                }
            };
            
            self.connections.insert(user_data, connection_state);
            
            // Continue writing from offset
            let buffer_ptr = self.buffer_ring.get_buffer_ptr(buffer_id);
            let offset_ptr = unsafe { buffer_ptr.add(written) };
            let target_fd = if to_backend { backend_fd } else { client_fd };
            
            let write_op = opcode::WriteFixed::new(
                types::Fd(target_fd),
                offset_ptr,
                remaining_bytes as u32,
                buffer_id
            )
            .build()
            .user_data(user_data);
            
            unsafe {
                if self.ring.submission().push(&write_op).is_err() {
                    error!("Failed to submit continuation write");
                    self.buffer_ring.return_buffer_id(buffer_id);
                    self.cleanup_tcp_connection(client_fd, backend_fd);
                }
            }
            
            return Ok(());
        }
        
        debug!("Write completed: {} bytes {}", 
               bytes_written, 
               if to_backend { "to backend" } else { "to client" });
        
        // Return buffer to pool after successful complete write
        self.buffer_ring.return_buffer_id(buffer_id);
        
        Ok(())
    }
    
    /// Handle completion of HTTP write operations with proper partial write handling
    fn handle_http_write_completion(&mut self, _user_data: u64, bytes_written: i32, client_fd: RawFd, backend_fd: RawFd, server_port: u16, buffer_id: u16, expected_bytes: usize, to_backend: bool) -> Result<()> {
        if bytes_written < 0 {
            error!("HTTP write operation failed with error: {}", bytes_written);
            // Return buffer and close connections on error
            self.buffer_ring.return_buffer_id(buffer_id);
            self.cleanup_http_connection(client_fd, backend_fd);
            return Ok(());
        }
        
        let written = bytes_written as usize;
        if written < expected_bytes {
            // Partial write - continue writing remaining data
            warn!("Partial HTTP write: {} bytes written, {} expected, continuing write", written, expected_bytes);
            
            let remaining_bytes = expected_bytes - written;
            let user_data = self.next_user_data;
            self.next_user_data += 1;
            
            // Update connection state to continue writing from offset
            let connection_state = if to_backend {
                ConnectionState::HttpWritingToBackend {
                    client_fd,
                    backend_fd,
                    server_port,
                    buffer_id,
                    bytes_to_write: remaining_bytes,
                }
            } else {
                ConnectionState::HttpWritingToClient {
                    client_fd,
                    backend_fd,
                    server_port,
                    buffer_id,
                    bytes_to_write: remaining_bytes,
                }
            };
            
            self.connections.insert(user_data, connection_state);
            
            // Continue writing from offset
            let buffer_ptr = self.buffer_ring.get_buffer_ptr(buffer_id);
            let offset_ptr = unsafe { buffer_ptr.add(written) };
            let target_fd = if to_backend { backend_fd } else { client_fd };
            
            let write_op = opcode::WriteFixed::new(
                types::Fd(target_fd),
                offset_ptr,
                remaining_bytes as u32,
                buffer_id
            )
            .build()
            .user_data(user_data);
            
            unsafe {
                if self.ring.submission().push(&write_op).is_err() {
                    error!("Failed to submit HTTP continuation write");
                    self.buffer_ring.return_buffer_id(buffer_id);
                    self.cleanup_http_connection(client_fd, backend_fd);
                }
            }
            
            return Ok(());
        }
        
        debug!("HTTP write completed: {} bytes {}", 
               bytes_written, 
               if to_backend { "to backend" } else { "to client" });
        
        // Return buffer to pool after successful complete write
        self.buffer_ring.return_buffer_id(buffer_id);
        
        if to_backend {
            // HTTP request sent to backend - start reading response
            if let Err(e) = self.submit_http_backend_read(client_fd, backend_fd, server_port) {
                error!("Failed to start reading HTTP response from backend: {}", e);
                self.cleanup_http_connection(client_fd, backend_fd);
            }
        } else {
            // HTTP response sent to client - close connection (will be replaced by keep-alive logic)
            debug!("HTTP response forwarded to client, closing connection");
            self.cleanup_http_connection(client_fd, backend_fd);
        }
        
        Ok(())
    }
    
    /// Handle HTTP write completion with response info for keep-alive decision and proper partial write handling
    fn handle_http_write_completion_with_response_info(&mut self, _user_data: u64, bytes_written: i32, client_fd: RawFd, backend_fd: RawFd, server_port: u16, buffer_id: u16, expected_bytes: usize, response_info: HttpResponseInfo) -> Result<()> {
        if bytes_written < 0 {
            error!("HTTP write operation failed with error: {}", bytes_written);
            // Return buffer and close connections on error
            self.buffer_ring.return_buffer_id(buffer_id);
            self.cleanup_http_connection(client_fd, backend_fd);
            return Ok(());
        }
        
        let written = bytes_written as usize;
        if written < expected_bytes {
            // Partial write - continue writing remaining data
            warn!("Partial HTTP write with response info: {} bytes written, {} expected, continuing write", written, expected_bytes);
            
            let remaining_bytes = expected_bytes - written;
            let user_data = self.next_user_data;
            self.next_user_data += 1;
            
            // Continue writing with same response info for final keep-alive decision
            self.connections.insert(user_data, ConnectionState::HttpWritingToClientWithResponseInfo {
                client_fd,
                backend_fd,
                server_port,
                buffer_id,
                bytes_to_write: remaining_bytes,
                response_info,
            });
            
            // Continue writing from offset
            let buffer_ptr = self.buffer_ring.get_buffer_ptr(buffer_id);
            let offset_ptr = unsafe { buffer_ptr.add(written) };
            
            let write_op = opcode::WriteFixed::new(
                types::Fd(client_fd),
                offset_ptr,
                remaining_bytes as u32,
                buffer_id
            )
            .build()
            .user_data(user_data);
            
            unsafe {
                if self.ring.submission().push(&write_op).is_err() {
                    error!("Failed to submit HTTP continuation write with response info");
                    self.buffer_ring.return_buffer_id(buffer_id);
                    self.cleanup_http_connection(client_fd, backend_fd);
                }
            }
            
            return Ok(());
        }
        
        debug!("HTTP response with headers sent to client: {} bytes", bytes_written);
        
        // Return buffer to pool after successful complete write
        self.buffer_ring.return_buffer_id(buffer_id);
        
        // Key decision point: Check if we should keep client connection alive
        // This implements client-side keep-alive independent of backend behavior
        
        // Keep-alive logic: consider status code and connection headers
        let should_keep_alive = response_info.connection_type != ConnectionType::Close && 
                                response_info.status_code < 400;  // Don't keep-alive on client/server errors
        
        if should_keep_alive {
            debug!("Response allows keep-alive - setting up connection for next request");
            debug!("Response info: status={}, connection={:?}, content_length={:?}", 
                   response_info.status_code, response_info.connection_type, response_info.content_length);
            
            // Close backend connection (HTTP is request/response, not persistent)
            self.close_http_connection(backend_fd);
            
            // Setup keep-alive read for next HTTP request from client
            if let Err(e) = self.setup_http_keepalive_read(client_fd, server_port) {
                error!("Failed to setup keep-alive read: {}", e);
                // If we can't setup keep-alive, close the client connection
                unsafe { libc::close(client_fd) };
            }
        } else {
            debug!("Response indicates connection should close (status={}, connection={:?})", 
                   response_info.status_code, response_info.connection_type);
            // Close both connections
            self.close_http_connection(backend_fd);
            unsafe { libc::close(client_fd) };
        }
        
        Ok(())
    }
    
    /// Setup HTTP keep-alive read for next request on the same client connection
    fn setup_http_keepalive_read(&mut self, client_fd: RawFd, server_port: u16) -> Result<()> {
        debug!("Setting up keep-alive read for next HTTP request on client_fd={}", client_fd);
        
        // Allocate a new buffer for the next request
        if let Some(buffer_id) = self.buffer_ring.get_buffer_id() {
            let user_data = self.next_user_data;
            self.next_user_data += 1;
            
            // Create a fresh parser for the next request
            let parser = HttpRequestParser::new();
            
            // Transition to waiting for next request state
            // Note: backend_fd is set to 0 since we don't have a backend connection yet
            self.connections.insert(user_data, ConnectionState::HttpWaitingForNextRequest {
                client_fd,
                backend_fd: 0, // Will be established when we get the next request
                server_port,
                parser,
                buffer_id,
            });
            
            // Start reading the next HTTP request from client
            let buffer_ptr = self.buffer_ring.get_buffer_ptr(buffer_id);
            let buffer_size = self.buffer_ring.buffer_size;
            
            let read_op = opcode::ReadFixed::new(types::Fd(client_fd), buffer_ptr, buffer_size, buffer_id)
                .build()
                .user_data(user_data);
            
            unsafe {
                if self.ring.submission().push(&read_op).is_err() {
                    // Failed to submit read, clean up
                    self.buffer_ring.return_buffer_id(buffer_id);
                    self.connections.remove(&user_data);
                    return Err(anyhow!("Failed to submit keep-alive read"));
                }
            }
            
            debug!("Keep-alive read operation submitted for client_fd={}", client_fd);
            Ok(())
        } else {
            Err(anyhow!("No buffer available for keep-alive read"))
        }
    }
    
    /// Clean up HTTP connection with intelligent connection pooling decision
    fn cleanup_http_connection(&mut self, client_fd: RawFd, backend_fd: RawFd) {
        // Since we don't have response info here, use conservative approach:
        // Pool backend connections for reuse, but close client connections
        // This is safe because:
        // 1. Backend connections can be reused for future requests
        // 2. Client connections without explicit keep-alive should be closed
        // 3. Keep-alive decisions are made in handle_http_write_completion_with_response_info
        self.cleanup_http_connection_pooled(client_fd, backend_fd, true);
    }
    
    /// Submit read operation for HTTP backend response
    fn submit_http_backend_read(&mut self, client_fd: RawFd, backend_fd: RawFd, server_port: u16) -> Result<()> {
        if let Some(buffer_id) = self.buffer_ring.get_buffer_id() {
            let user_data = self.next_user_data;
            self.next_user_data += 1;
            
            // Store connection state with buffer ID - use TcpReadingBackend since the logic is the same
            // but will use HTTP write when forwarding to client
            self.connections.insert(user_data, ConnectionState::TcpReadingBackend {
                client_fd,
                backend_fd,
                server_port,
                buffer_id,
            });
            
            // Get buffer from ring using buffer_id
            let buffer_ptr = self.buffer_ring.get_buffer_ptr(buffer_id);
            let buffer_size = self.buffer_ring.buffer_size;
            
            let read_op = opcode::ReadFixed::new(
                types::Fd(backend_fd), 
                buffer_ptr,
                buffer_size,
                buffer_id
            )
            .build()
            .user_data(user_data);
            
            unsafe {
                if self.ring.submission().push(&read_op).is_err() {
                    return Err(anyhow!("Failed to submit HTTP backend read"));
                }
            }
        }
        Ok(())
    }
    
    /// Handle completion of HTTP error response write
    fn handle_http_error_response_completion(&mut self, _user_data: u64, bytes_written: i32, client_fd: RawFd, buffer_id: u16, response_size: usize) -> Result<()> {
        self.buffer_ring.return_buffer_id(buffer_id);
        
        if bytes_written < 0 {
            error!("Failed to send HTTP error response: {}", bytes_written);
        } else if bytes_written as usize != response_size {
            warn!("Partial HTTP error response write: {} bytes written, {} expected", bytes_written, response_size);
        } else {
            debug!("HTTP error response sent successfully: {} bytes", bytes_written);
        }
        
        // Close client connection after sending error response
        unsafe { libc::close(client_fd) };
        
        Ok(())
    }
    
    /// Route HTTP request using FNV hash-based routing
    fn route_http_request(&self, host: Option<&str>, path: &str) -> Result<&Backend> {
        let routes = unsafe { &*self.routes.load(std::sync::atomic::Ordering::Acquire) };
        debug!("Route table: {} HTTP routes, default backend: {}", routes.http_routes.len(), routes.default_http_backend.is_some());
        
        if let Some(host_str) = host {
            // Normalize host header by removing port (Gateway API compliance)
            let normalized_host = crate::config::normalize_host_header(host_str);
            debug!("Host header: '{}' → normalized: '{}'", host_str, normalized_host);
            
            // Calculate FNV hash of normalized host header
            let host_hash = crate::routing::fnv_hash(&normalized_host);
            debug!("Calculated host hash for '{}': {}", normalized_host, host_hash);
            let result = routes.route_http_request(host_hash, path);
            debug!("Route lookup result: {:?}", result.as_ref().map(|_| "Found").map_err(|e| e.to_string()));
            result
        } else {
            debug!("No host header provided");
            // No host header - use default backend if available
            if let Some(default_backend) = &routes.default_http_backend {
                debug!("Using default HTTP backend");
                default_backend.select_backend()
            } else {
                debug!("No default HTTP backend configured");
                Err(anyhow!("No host header and no default HTTP backend configured"))
            }
        }
    }
    
    /// Connect to backend and forward HTTP request
    fn connect_and_forward_http_request(&mut self, user_data: u64, client_fd: RawFd, server_port: u16, backend: &Backend, buffer_id: u16, request_size: usize, parser: HttpRequestParser) -> Result<()> {
        self.connect_and_forward_http_request_to_addr(user_data, client_fd, server_port, backend.socket_addr, buffer_id, request_size, parser)
    }

    /// Connect and forward HTTP request to backend using socket address directly (zero-allocation)
    fn connect_and_forward_http_request_to_addr(&mut self, user_data: u64, client_fd: RawFd, server_port: u16, backend_socket_addr: SocketAddr, buffer_id: u16, request_size: usize, parser: HttpRequestParser) -> Result<()> {
        debug!("Connecting to HTTP backend: {}", backend_socket_addr);
        
        // Connect to backend using connection pool
        match self.get_or_create_http_connection_to_addr(backend_socket_addr) {
            Ok(backend_fd) => {
                debug!("Connected to HTTP backend on fd={}", backend_fd);
                
                // Note: HTTP connections are not tracked in active_tcp_connections 
                // since they use cleanup_http_connection() instead of cleanup_tcp_connection()
                
                // Forward the request to backend
                self.forward_http_request_to_backend(user_data, client_fd, backend_fd, server_port, buffer_id, request_size, parser)?;
                
                // Note: HTTP write completion handler will start reading response from backend
            }
            Err(e) => {
                error!("Failed to connect to HTTP backend {}: {}", backend_socket_addr, e);
                self.send_http_error_response(client_fd, 502, "Bad Gateway")?;
                self.buffer_ring.return_buffer_id(buffer_id);
                unsafe { libc::close(client_fd) };
            }
        }
        
        Ok(())
    }
    
    /// Forward HTTP request to backend using zero-copy write
    fn forward_http_request_to_backend(&mut self, _user_data: u64, client_fd: RawFd, backend_fd: RawFd, server_port: u16, buffer_id: u16, request_size: usize, _parser: HttpRequestParser) -> Result<()> {
        debug!("Forwarding HTTP request to backend: {} bytes", request_size);
        
        // Submit HTTP-specific zero-copy write to backend (will close connection after response)
        self.submit_http_write_to_backend(client_fd, backend_fd, server_port, buffer_id, request_size)?;
        
        // No need to continue reading from client - HTTP is request/response, not persistent
        
        Ok(())
    }
    
    /// Send HTTP error response to client
    fn send_http_error_response(&mut self, client_fd: RawFd, status_code: u16, _status_text: &str) -> Result<()> {
        let response_bytes = match status_code {
            400 => HTTP_400_RESPONSE,
            404 => HTTP_404_RESPONSE,
            500 => HTTP_500_RESPONSE,
            502 => HTTP_502_RESPONSE,
            503 => HTTP_503_RESPONSE,
            _ => {
                // For uncommon status codes, fall back to minimal allocation
                warn!("Using fallback error response for status code: {}", status_code);
                HTTP_500_RESPONSE // Default to 500 for unknown codes
            }
        };
        
        debug!("Sending HTTP error response: {}", status_code);
        
        // Get buffer for error response
        if let Some(buffer_id) = self.buffer_ring.get_buffer_id() {
            
            if response_bytes.len() <= self.buffer_ring.buffer_size as usize {
                // Write response content to fixed buffer
                if let Some(buffer_content) = self.buffer_ring.get_buffer_content_mut(buffer_id) {
                    buffer_content[..response_bytes.len()].copy_from_slice(response_bytes);
                    
                    // Send error response using io_uring WriteFixed
                    let user_data = self.next_user_data;
                    self.next_user_data += 1;
                    
                    self.connections.insert(user_data, ConnectionState::HttpErrorResponse {
                        client_fd,
                        buffer_id,
                        response_size: response_bytes.len(),
                    });
                    
                    let buffer_ptr = self.buffer_ring.get_buffer_ptr(buffer_id);
                    
                    let write_op = opcode::WriteFixed::new(
                        types::Fd(client_fd),
                        buffer_ptr,
                        response_bytes.len() as u32,
                        buffer_id
                    )
                    .build()
                    .user_data(user_data);
                    
                    unsafe {
                        if self.ring.submission().push(&write_op).is_err() {
                            self.buffer_ring.return_buffer_id(buffer_id);
                            return Err(anyhow!("Failed to submit HTTP error response"));
                        }
                    }
                } else {
                    error!("Failed to get mutable buffer content for buffer_id {}", buffer_id);
                    self.buffer_ring.return_buffer_id(buffer_id);
                }
            } else {
                error!("Error response too large for buffer: {} > {}", response_bytes.len(), self.buffer_ring.buffer_size);
                self.buffer_ring.return_buffer_id(buffer_id);
            }
        } else {
            error!("No buffer available for HTTP error response");
        }
        
        Ok(())
    }
    
}

/// Data plane manager for raw io_uring workers
pub struct UringDataPlane {
    workers: Vec<std::thread::JoinHandle<Result<()>>>,
    worker_count: usize,
}

impl UringDataPlane {
    pub fn new(worker_count: usize) -> Self {
        Self {
            workers: Vec::with_capacity(worker_count),
            worker_count,
        }
    }
    
    pub fn start(&mut self, routes: Arc<AtomicPtr<RouteTable>>, config: RuntimeConfig) -> Result<()> {
        info!("Starting {} raw io_uring data plane workers", self.worker_count);
        
        for worker_id in 0..self.worker_count {
            let routes_clone = routes.clone();
            let config_clone = config.clone();
            
            let handle = std::thread::spawn(move || {
                info!("Worker {} thread starting", worker_id);
                
                let mut worker = match UringWorker::new(worker_id, routes_clone, config_clone) {
                    Ok(worker) => {
                        info!("Worker {} created successfully", worker_id);
                        worker
                    }
                    Err(e) => {
                        error!("Worker {} failed to initialize: {}", worker_id, e);
                        return Err(e);
                    }
                };
                
                if let Err(e) = worker.setup_listeners() {
                    error!("Worker {} failed to setup listeners: {}", worker_id, e);
                    return Err(e);
                }
                
                info!("Worker {} listeners setup complete, starting event loop", worker_id);
                worker.run()
            });
            
            self.workers.push(handle);
        }
        
        info!("Raw io_uring data plane workers started");
        Ok(())
    }
    
    pub fn shutdown(self) -> Result<()> {
        info!("Shutting down raw io_uring data plane workers");
        
        for handle in self.workers {
            if let Err(e) = handle.join() {
                error!("Worker thread failed: {:?}", e);
            }
        }
        
        info!("All data plane workers shut down");
        Ok(())
    }
}