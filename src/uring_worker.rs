use anyhow::{anyhow, Result};
use io_uring::{opcode, types, IoUring};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::os::unix::io::{AsRawFd, RawFd};
use std::net::SocketAddr;
use tracing::{info, debug, warn, error};

use crate::routing::{RouteTable, Backend};
use crate::runtime_config::RuntimeConfig;

/// Simple HTTP request parser state for zero-overhead parsing
#[derive(Debug)]
pub struct HttpRequestParser {
    // Empty state struct - parsing is done directly in parse_http_headers_fast()
    // This exists only to maintain the connection state machine type safety
}

impl HttpRequestParser {
    pub fn new() -> Self {
        Self {}
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
    
    /// Get the ring ID for io_uring operations
    fn ring_id(&self) -> u16 {
        self.ring_id
    }
    
    /// Temporary compatibility method - to be removed
    fn get_buffer(&mut self) -> Option<Box<[u8]>> {
        // Temporary stub - return None to fail gracefully during transition
        warn!("Using deprecated buffer method - buffer ring operations not yet implemented");
        None
    }
    
    /// Temporary compatibility method - to be removed  
    fn return_buffer(&mut self, _buffer: Box<[u8]>) {
        // Temporary stub - do nothing during transition
        warn!("Using deprecated buffer return method - buffer ring operations not yet implemented");
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
        
        // Always listen on HTTP port (8080)
        self.setup_listener(8080)?;
        
        // Listen on all TCP route ports
        for &port in routes.get_tcp_ports().iter() {
            self.setup_listener(port)?;
        }
        
        info!("Worker {} setup listeners for ports: {:?}", 
              self.worker_id, self.listeners.keys().collect::<Vec<_>>());
        
        Ok(())
    }
    
    fn setup_listener(&mut self, port: u16) -> Result<()> {
        let addr: SocketAddr = format!("0.0.0.0:{}", port).parse()?;
        
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
                        self.handle_http_write_completion(user_data, result, client_fd, backend_fd, buffer_id, bytes_to_write, true)?;
                    }
                    ConnectionState::HttpWritingToClient { client_fd, backend_fd, server_port, buffer_id, bytes_to_write } => {
                        self.handle_http_write_completion(user_data, result, client_fd, backend_fd, buffer_id, bytes_to_write, false)?;
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
        
        // TODO: Replace with eBPF-based protocol detection for production
        // 
        // Future eBPF Architecture:
        // ========================
        // 1. eBPF TC (Traffic Control) hooks will inspect packets at kernel level
        // 2. Protocol classification happens before userspace receives data:
        //    - HTTP/1.1: "GET/POST/PUT/DELETE/HEAD/OPTIONS/PATCH " + path + " HTTP/"
        //    - HTTP/2: Connection preface "PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"  
        //    - gRPC: HTTP/2 + Content-Type: application/grpc*
        //    - TLS: Handshake signature (0x16 0x03 0x01/0x02/0x03)
        //    - Raw TCP/UDP: Fallback for unrecognized protocols
        // 3. eBPF map stores routing decisions, eliminating userspace protocol detection
        // 4. Zero-copy forwarding with kernel-level route table updates
        // 5. Support for Gateway API multi-protocol listeners on same ports
        //
        // Current MVP Implementation:
        // ===========================
        // Route-table-first logic with hardcoded HTTP assumption for development
        
        // Check route table first for explicit TCP routes
        let routes_ptr = self.routes.load(Ordering::Acquire);
        if !routes_ptr.is_null() {
            let routes = unsafe { &*routes_ptr };
            if routes.has_tcp_route(server_port) {
                // Explicit TCP route configured - handle as TCP proxy
                self.start_tcp_forwarding_with_data(client_fd, server_port, buffer_id, bytes_read as usize)?;
                return Ok(());
            }
        }
        
        // Hardcoded protocol detection for MVP (will be replaced by eBPF inspection)
        if server_port == 8080 {
            // Port 8080 = HTTP
            self.start_http_processing_with_data(client_fd, server_port, buffer_id, bytes_read as usize)?;
        } else {
            // All other ports = TCP
            self.start_tcp_forwarding_with_data(client_fd, server_port, buffer_id, bytes_read as usize)?;
        }
        
        Ok(())
    }
    
    fn start_http_processing(&mut self, client_fd: RawFd, server_port: u16) -> Result<()> {
        debug!("Starting HTTP processing for fd={} on port {}", client_fd, server_port);
        
        if let Some(buffer_id) = self.buffer_ring.get_buffer_id() {
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
        
            // Get buffer ID from stored state
            let buffer_id = if let Some(ConnectionState::HttpConnection { buffer_id, .. }) = self.connections.get(&user_data) {
                *buffer_id
            } else {
                return Err(anyhow!("Failed to get buffer ID for HTTP connection"));
            };
            
            // Continue reading HTTP request
            let buffer_ptr = self.buffer_ring.get_buffer_ptr(buffer_id);
            let buffer_size = self.buffer_ring.buffer_size;
            let read_op = opcode::ReadFixed::new(types::Fd(client_fd), buffer_ptr, buffer_size, buffer_id)
                .build()
                .user_data(user_data);
            
            unsafe {
                if self.ring.submission().push(&read_op).is_err() {
                    return Err(anyhow!("Failed to submit HTTP read"));
                }
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
        let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)?;
        // Use blocking connect for simplicity in this initial implementation
        // TODO: Implement proper async connect with io_uring
        socket.set_nonblocking(false)?;
        
        let addr = socket2::SockAddr::from(backend.socket_addr);
        
        match socket.connect(&addr) {
            Ok(()) => {
                let fd = socket.as_raw_fd();
                std::mem::forget(socket); // Keep socket alive
                
                // Set to non-blocking for io_uring operations
                unsafe {
                    let flags = libc::fcntl(fd, libc::F_GETFL);
                    libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
                
                debug!("Successfully connected to backend {} on fd={}", backend.socket_addr, fd);
                Ok(fd)
            }
            Err(e) => {
                error!("Failed to connect to backend {}: {}", backend.socket_addr, e);
                Err(anyhow!("Backend connection failed: {}", e))
            }
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
    
    fn handle_http_data(&mut self, user_data: u64, bytes_read: i32, client_fd: RawFd, server_port: u16, _parser: HttpRequestParser, backend_fd: Option<RawFd>, buffer_id: u16) -> Result<()> {
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
            Ok((method, path, host)) => {
                debug!("Parsed HTTP request: {} {} Host: {:?}", method, path, host);
                
                // Route the HTTP request
                debug!("Attempting to route HTTP request to host: {:?}, path: {}", host, path);
                match self.route_http_request(host, path) {
                    Ok(backend) => {
                        // Clone backend info to avoid borrow checker issues
                        let backend_clone = backend.clone();
                        
                        if let Some(existing_backend_fd) = backend_fd {
                            // Already connected to a backend, forward the request
                            self.forward_http_request_to_backend(user_data, client_fd, existing_backend_fd, server_port, buffer_id, bytes_read as usize)?;
                        } else {
                            // Need to connect to backend first
                            self.connect_and_forward_http_request(user_data, client_fd, server_port, &backend_clone, buffer_id, bytes_read as usize)?;
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
        
        // Check if this is an HTTP connection - only port 8080 is HTTP
        let is_http = server_port == 8080;
        
        if is_http {
            // HTTP response - use HTTP write (will close connection after write)
            self.submit_http_write_to_client(client_fd, backend_fd, server_port, buffer_id, bytes_read as usize)?;
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
    
    /// Zero-copy write to client using io_uring (HTTP)
    fn submit_http_write_to_client(&mut self, client_fd: RawFd, backend_fd: RawFd, server_port: u16, buffer_id: u16, bytes_to_write: usize) -> Result<()> {
        let user_data = self.next_user_data;
        self.next_user_data += 1;
        
        // Store HTTP write state with buffer ID  
        self.connections.insert(user_data, ConnectionState::HttpWritingToClient {
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
                return Err(anyhow!("Failed to submit HTTP write to client"));
            }
        }
        
        Ok(())
    }
    
    /// Handle completion of io_uring write operations
    fn handle_tcp_write_completion(&mut self, _user_data: u64, bytes_written: i32, client_fd: RawFd, backend_fd: RawFd, server_port: u16, buffer_id: u16, expected_bytes: usize, to_backend: bool) -> Result<()> {
        if bytes_written < 0 {
            error!("Write operation failed with error: {}", bytes_written);
            self.buffer_ring.return_buffer_id(buffer_id);
            self.cleanup_tcp_connection(client_fd, backend_fd);
            return Ok(());
        }
        
        if bytes_written as usize != expected_bytes {
            warn!("Partial write: {} bytes written, {} expected", bytes_written, expected_bytes);
            // TODO: Handle partial writes by continuing the write operation
        }
        
        debug!("Write completed: {} bytes {}", 
               bytes_written, 
               if to_backend { "to backend" } else { "to client" });
        
        // Return buffer to pool after successful write
        self.buffer_ring.return_buffer_id(buffer_id);
        
        Ok(())
    }
    
    /// Handle completion of HTTP write operations (zero-overhead connection cleanup)
    fn handle_http_write_completion(&mut self, _user_data: u64, bytes_written: i32, client_fd: RawFd, backend_fd: RawFd, buffer_id: u16, expected_bytes: usize, to_backend: bool) -> Result<()> {
        // Return buffer to pool
        self.buffer_ring.return_buffer_id(buffer_id);
        
        if bytes_written < 0 {
            error!("HTTP write operation failed with error: {}", bytes_written);
            // Close both connections on error
            self.cleanup_http_connection(client_fd, backend_fd);
            return Ok(());
        }
        
        if bytes_written as usize != expected_bytes {
            warn!("Partial HTTP write: {} bytes written, {} expected", bytes_written, expected_bytes);
            // TODO: Handle partial writes by continuing the write operation
        }
        
        debug!("HTTP write completed: {} bytes {}", 
               bytes_written, 
               if to_backend { "to backend" } else { "to client" });
        
        if to_backend {
            // HTTP request sent to backend - start reading response
            if let Err(e) = self.submit_http_backend_read(client_fd, backend_fd, 0) {
                error!("Failed to start reading HTTP response from backend: {}", e);
                self.cleanup_http_connection(client_fd, backend_fd);
            }
        } else {
            // HTTP response sent to client - close connection (zero-overhead)
            debug!("HTTP response forwarded to client, closing connection");
            self.cleanup_http_connection(client_fd, backend_fd);
        }
        
        Ok(())
    }
    
    /// Clean up HTTP connection (close both client and backend)
    fn cleanup_http_connection(&mut self, client_fd: RawFd, backend_fd: RawFd) {
        debug!("Cleaning up HTTP connection: client_fd={}, backend_fd={}", client_fd, backend_fd);
        unsafe {
            libc::close(client_fd);
            libc::close(backend_fd);
        }
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
            // Calculate FNV hash of host header
            let host_hash = crate::routing::fnv_hash(host_str);
            debug!("Calculated host hash for '{}': {}", host_str, host_hash);
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
    fn connect_and_forward_http_request(&mut self, user_data: u64, client_fd: RawFd, server_port: u16, backend: &Backend, buffer_id: u16, request_size: usize) -> Result<()> {
        debug!("Connecting to HTTP backend: {}", backend.socket_addr);
        
        // Connect to backend (blocking for now, TODO: async connect)
        match self.connect_to_backend(backend) {
            Ok(backend_fd) => {
                debug!("Connected to HTTP backend on fd={}", backend_fd);
                
                // Note: HTTP connections are not tracked in active_tcp_connections 
                // since they use cleanup_http_connection() instead of cleanup_tcp_connection()
                
                // Forward the request to backend
                self.forward_http_request_to_backend(user_data, client_fd, backend_fd, server_port, buffer_id, request_size)?;
                
                // Note: HTTP write completion handler will start reading response from backend
            }
            Err(e) => {
                error!("Failed to connect to HTTP backend {}: {}", backend.socket_addr, e);
                self.send_http_error_response(client_fd, 502, "Bad Gateway")?;
                self.buffer_ring.return_buffer_id(buffer_id);
                unsafe { libc::close(client_fd) };
            }
        }
        
        Ok(())
    }
    
    /// Forward HTTP request to backend using zero-copy write
    fn forward_http_request_to_backend(&mut self, _user_data: u64, client_fd: RawFd, backend_fd: RawFd, server_port: u16, buffer_id: u16, request_size: usize) -> Result<()> {
        debug!("Forwarding HTTP request to backend: {} bytes", request_size);
        
        // Submit HTTP-specific zero-copy write to backend (will close connection after response)
        self.submit_http_write_to_backend(client_fd, backend_fd, server_port, buffer_id, request_size)?;
        
        // No need to continue reading from client - HTTP is request/response, not persistent
        
        Ok(())
    }
    
    /// Send HTTP error response to client
    fn send_http_error_response(&mut self, client_fd: RawFd, status_code: u16, status_text: &str) -> Result<()> {
        let response = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{} {}\n",
            status_code, status_text, status_text.len() + status_code.to_string().len() + 2, status_code, status_text
        );
        
        debug!("Sending HTTP error response: {} {}", status_code, status_text);
        
        // Get buffer for error response
        if let Some(buffer_id) = self.buffer_ring.get_buffer_id() {
            let response_bytes = response.as_bytes();
            
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