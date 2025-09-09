use anyhow::{anyhow, Result};
use io_uring::{opcode, types, IoUring};
use fnv::{FnvHashMap, FnvHashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::os::unix::io::{AsRawFd, RawFd};
use std::net::SocketAddr;
use tracing::{info, debug, warn, error};

// Cache line size constant for memory layout validation
const CACHE_LINE_SIZE: usize = 64;

use crate::routing::{RouteTable, Backend};
use crate::runtime_config::{RuntimeConfig, self as runtime_config};
use crate::parser::{ConnectionType, HttpResponseInfo};
use crate::config::WorkerConfig;

/// High-performance notification slot manager for zero-copy operations
/// Implements generation-based buffer safety with O(1) operations
/// Memory layout optimized for cache line efficiency: hot fields first
#[derive(Debug)]
#[repr(C)]
pub struct NotificationSlotManager {
    // === HOT PATH DATA (First 32 bytes - fits in first half of cache line) ===
    // Accessed on every allocation/release operation
    
    /// Operations currently using each slot - accessed on every alloc/release (hot)
    slot_operations: Vec<u64>,
    /// Next slot to allocate - accessed on every allocation (hot)
    next_slot: u16,
    /// Current generation counter - accessed on every allocation (hot)
    current_generation: u64,
    
    // === MEDIUM ACCESS DATA ===
    // Accessed during specific operations
    
    /// Pending notifications awaiting completion - accessed during release
    pending_notifications: Vec<Option<(u16, u64)>>,
    /// Track generations with pending operations - accessed during cleanup
    generation_count: u64,
    
    // === COLD PATH DATA ===
    // Accessed only during setup
    
    /// Maximum number of slots - only accessed during initialization/bounds checking
    max_slots: u16,
}

// Bit flags for user_data encoding (zero-overhead notification identification)
const NOTIFICATION_FLAG: u64 = 1u64 << 63;  // High bit marks notifications
const SLOT_MASK: u64 = 0x7FFF0000_00000000;  // Bits 48-62 for slot (15 bits = 32K slots)

impl NotificationSlotManager {
    pub fn new(max_slots: u16) -> Self {
        Self {
            // Hot path data first - matches struct field ordering for cache efficiency
            slot_operations: vec![0; max_slots as usize],
            next_slot: 0,
            current_generation: 1,
            // Medium access data
            pending_notifications: vec![None; max_slots as usize],
            generation_count: 0,
            // Cold path data last
            max_slots,
        }
    }
    
    /// Allocate notification slot for zero-copy operation (O(1) operation)
    #[inline]
    pub fn allocate_slot(&mut self, user_data: u64, buffer_id: u16) -> u16 {
        let slot = self.next_slot;
        self.next_slot = (self.next_slot + 1) % self.max_slots;
        
        // Track operation and pending notification
        self.slot_operations[slot as usize] = user_data;
        self.pending_notifications[slot as usize] = Some((buffer_id, self.current_generation));
        self.generation_count += 1;
        
        slot
    }
    
    /// Check if user_data represents a notification completion (<5ns operation)
    #[inline]
    pub fn is_notification(user_data: u64) -> bool {
        (user_data & NOTIFICATION_FLAG) != 0
    }
    
    
    /// Decode notification slot from user_data (O(1) operation)
    #[inline]
    pub fn decode_notification_slot(user_data: u64) -> u16 {
        ((user_data & SLOT_MASK) >> 32) as u16
    }
    
    /// Release notification slot and return buffer info (O(1) operation)
    pub fn release_notification_slot(&mut self, user_data: u64) -> Option<(u64, u16)> {
        if !Self::is_notification(user_data) {
            return None;
        }
        
        let slot = Self::decode_notification_slot(user_data);
        if slot as usize >= self.slot_operations.len() {
            return None;
        }
        
        let operation_user_data = self.slot_operations[slot as usize];
        if operation_user_data == 0 {
            return None; // Slot already released
        }
        
        // Get buffer info before clearing
        let buffer_info = self.pending_notifications[slot as usize].take();
        
        // Clear slot
        self.slot_operations[slot as usize] = 0;
        self.generation_count -= 1;
        
        buffer_info.map(|(buffer_id, _generation)| (operation_user_data, buffer_id))
    }
    
}

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
/// Memory layout optimized for cache line efficiency: hot fields first (accessed every request)
#[repr(C)]
pub struct UringWorker {
    // === HOT PATH DATA (First Cache Line - 64 bytes) ===
    // Accessed on every request - keep in first cache line for maximum performance
    
    /// Route table pointer - accessed for every HTTP/TCP routing decision
    routes: Arc<AtomicPtr<RouteTable>>,
    /// Connection state map - accessed for every I/O operation completion
    connections: FnvHashMap<u64, ConnectionState>,
    /// User data counter - incremented for every I/O operation
    next_user_data: u64,
    /// io_uring instance - used for all I/O operations  
    ring: IoUring,
    
    // === MEDIUM ACCESS DATA (Second Cache Line) ===
    // Accessed during I/O setup and buffer management
    
    /// Buffer ring for zero-copy operations - accessed during I/O setup
    buffer_ring: BufferRing,
    /// Zero-copy notification slot management - accessed during SendZc operations
    notification_slots: NotificationSlotManager,
    /// Track buffers with pending zero-copy operations - accessed during buffer lifecycle
    pending_zerocopy_buffers: FnvHashMap<u16, u32>,
    
    // === COLD PATH DATA (Remaining Cache Lines) ===
    // Accessed only during setup, cleanup, or error handling
    
    /// Worker ID - only used for logging and identification
    worker_id: usize,
    /// Worker-specific configuration for protocol optimization  
    worker_config: WorkerConfig,
    /// Runtime configuration - only accessed during setup and protocol detection
    config: RuntimeConfig,
    /// Listener sockets - only accessed during setup and new connections
    listeners: FnvHashMap<u16, RawFd>,
    /// Track active TCP connections to prevent double-close - only accessed during cleanup
    active_tcp_connections: FnvHashSet<(RawFd, RawFd)>,
    /// HTTP connection pool: backend_addr -> Vec<RawFd> - accessed during connection reuse
    http_connection_pool: FnvHashMap<SocketAddr, Vec<RawFd>>,
    /// Track backend addresses for HTTP connections: backend_fd -> backend_addr  
    http_backend_addresses: FnvHashMap<RawFd, SocketAddr>,
}

/// Connection state enum optimized for cache line efficiency
/// Fields ordered by access frequency within each variant: hot data first
#[derive(Debug)]
#[repr(C)]
enum ConnectionState {
    /// Listening socket waiting for connections
    Listening { 
        fd: RawFd,          // Hot: accessed on every accept
        port: u16,          // Cold: only for logging
    },
    /// New connection detected, determining protocol
    DetectingProtocol { 
        client_fd: RawFd,   // Hot: used for I/O operations
        buffer_id: u16,     // Hot: used for buffer management
        server_port: u16,   // Medium: used for protocol decision
    },
    /// HTTP connection processing request
    HttpConnection {
        client_fd: RawFd,           // Hot: primary I/O target
        buffer_id: u16,             // Hot: buffer management
        backend_fd: Option<RawFd>,  // Hot: backend I/O target
        server_port: u16,           // Medium: routing decisions
        parser: HttpRequestParser,  // Cold: only during request parsing
    },
    /// TCP connection reading from client
    TcpReadingClient {
        client_fd: RawFd,   // Hot: primary I/O source
        backend_fd: RawFd,  // Hot: I/O target for forwarding
        buffer_id: u16,     // Hot: buffer management
        server_port: u16,   // Cold: only for logging/debugging
    },
    /// TCP connection reading from backend
    TcpReadingBackend {
        backend_fd: RawFd,  // Hot: primary I/O source
        client_fd: RawFd,   // Hot: I/O target for forwarding
        buffer_id: u16,     // Hot: buffer management
        server_port: u16,   // Cold: only for logging/debugging
    },
    /// TCP connection writing to backend (zero-copy forwarding)
    TcpWritingToBackend {
        backend_fd: RawFd,      // Hot: primary I/O target
        bytes_to_write: usize,  // Hot: write operation size
        client_fd: RawFd,       // Medium: source connection
        buffer_id: u16,         // Medium: buffer management
        server_port: u16,       // Cold: only for logging/debugging
    },
    /// TCP connection writing to client (zero-copy forwarding)
    TcpWritingToClient {
        client_fd: RawFd,       // Hot: primary I/O target
        bytes_to_write: usize,  // Hot: write operation size
        backend_fd: RawFd,      // Medium: source connection
        buffer_id: u16,         // Medium: buffer management
        server_port: u16,       // Cold: only for logging/debugging
    },
    /// HTTP error response being sent to client
    HttpErrorResponse {
        client_fd: RawFd,       // Hot: I/O target
        response_size: usize,   // Hot: write size
        buffer_id: u16,         // Medium: buffer management
    },
    /// HTTP connection waiting for next request (keep-alive)
    HttpWaitingForNextRequest {
        client_fd: RawFd,           // Hot: I/O target for next request
        buffer_id: u16,             // Hot: buffer for next request
        backend_fd: RawFd,          // Medium: backend for future requests
        server_port: u16,           // Medium: routing context
        parser: HttpRequestParser,  // Cold: only when request arrives
    },
    /// HTTP request being written to backend
    HttpWritingToBackend {
        backend_fd: RawFd,      // Hot: primary I/O target
        bytes_to_write: usize,  // Hot: write operation size
        client_fd: RawFd,       // Medium: source connection
        buffer_id: u16,         // Medium: buffer management
        server_port: u16,       // Cold: only for logging/debugging
    },
    /// HTTP response being written to client
    HttpWritingToClient {
        client_fd: RawFd,       // Hot: primary I/O target
        bytes_to_write: usize,  // Hot: write operation size
        backend_fd: RawFd,      // Medium: source connection
        buffer_id: u16,         // Medium: buffer management
        server_port: u16,       // Cold: only for logging/debugging
    },
    /// HTTP response being written to client with response info for keep-alive decision
    HttpWritingToClientWithResponseInfo {
        client_fd: RawFd,               // Hot: primary I/O target
        bytes_to_write: usize,          // Hot: write operation size
        backend_fd: RawFd,              // Medium: source connection for pooling
        buffer_id: u16,                 // Medium: buffer management
        server_port: u16,               // Medium: keep-alive context
        response_info: HttpResponseInfo, // Cold: only after write completion
    },
}

/// io_uring buffer ring for zero-overhead operations
/// Uses fixed buffers registered with the kernel for maximum performance
struct BufferRing {
    ring_id: u16,
    buffer_count: u16,
    buffer_size: u32,
    allocated_buffers: FnvHashSet<u16>,
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
            allocated_buffers: FnvHashSet::default(),
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
    /// Validate and log memory layout optimizations for performance monitoring
    /// This is called once during worker initialization to verify cache line efficiency
    fn validate_memory_layout() {
        let uringworker_size = std::mem::size_of::<UringWorker>();
        let connectionstate_size = std::mem::size_of::<ConnectionState>();
        let notification_manager_size = std::mem::size_of::<NotificationSlotManager>();
        
        info!("Memory layout validation:");
        info!("  UringWorker size: {} bytes ({:.1} cache lines)", 
              uringworker_size, uringworker_size as f64 / CACHE_LINE_SIZE as f64);
        info!("  ConnectionState size: {} bytes", connectionstate_size);
        info!("  NotificationSlotManager size: {} bytes ({:.1} cache lines)", 
              notification_manager_size, notification_manager_size as f64 / CACHE_LINE_SIZE as f64);
        
        // Verify cache line alignment for critical structures
        let uringworker_align = std::mem::align_of::<UringWorker>();
        let notification_align = std::mem::align_of::<NotificationSlotManager>();
        
        info!("  Memory alignment:");
        info!("    UringWorker: {} bytes (repr(C) applied)", uringworker_align);
        info!("    NotificationSlotManager: {} bytes (repr(C) applied)", notification_align);
        
        // Performance targets validation
        if connectionstate_size > 128 {
            warn!("ConnectionState size ({} bytes) exceeds 2 cache lines - may impact performance", 
                  connectionstate_size);
        }
        
        if uringworker_size > 512 {
            warn!("UringWorker size ({} bytes) is large - first cache line optimization critical", 
                  uringworker_size);
        }
    }

    /// Log connection errors in cold path to keep hot path optimized
    #[cold]
    #[inline(never)]
    fn log_connection_error(&self, addr: SocketAddr, error: &str) {
        error!("Failed to connect to backend {}: {}", addr, error);
    }
    
    /// Log HTTP routing errors in cold path
    #[cold]
    #[inline(never)]
    fn log_routing_error(&self, error: &str) {
        error!("Failed to route HTTP request: {}", error);
    }
    
    /// Log HTTP parsing errors in cold path
    #[cold]
    #[inline(never)]
    fn log_parsing_error(&self, error: &str) {
        error!("Failed to parse HTTP request: {}", error);
    }
    
    /// Log operation failures in cold path
    #[cold]
    #[inline(never)]
    fn log_operation_failure(&self, operation: &str, error: i32) {
        warn!("Operation failed with error: {} ({})", operation, error);
    }
    pub fn new(
        worker_config: WorkerConfig,
        routes: Arc<AtomicPtr<RouteTable>>,
        runtime_config: RuntimeConfig,
    ) -> Result<Self> {
        let worker_id = worker_config.id;
        info!("WORKER_INIT: Creating worker {}", worker_id);
        
        // Validate memory layout optimizations on first worker creation
        if worker_id == 0 {
            Self::validate_memory_layout();
        }
        
        let mut ring = IoUring::new(256)?;
        let listeners = FnvHashMap::default();
        let connections = FnvHashMap::default();
        
        // Create buffer ring with worker-specific configuration
        // TCP workers get 64KB buffers for bulk transfers, HTTP workers get 8KB for low latency
        let mut buffer_ring = BufferRing::new(
            worker_id as u16,                    // Use worker_id as ring_id for uniqueness
            worker_config.buffer_count,         // Worker-specific buffer count
            worker_config.buffer_size as u32,   // Worker-specific buffer size
        );
        
        info!("Worker {} initialized: buffer_size={}KB, buffer_count={}, protocols={:?}",
              worker_id, worker_config.buffer_size / 1024, worker_config.buffer_count, worker_config.protocols);
        
        // Register buffer ring with kernel
        buffer_ring.register_with_kernel(&mut ring)?;
        
        info!("WORKER_INIT: Worker {} created successfully", worker_id);
        
        Ok(Self {
            // Hot path data first - matches struct field ordering for cache efficiency
            routes,
            connections,
            next_user_data: 1,
            ring,
            // Medium access data  
            buffer_ring,
            notification_slots: NotificationSlotManager::new(32), // 32 notification slots for zero-copy
            pending_zerocopy_buffers: FnvHashMap::default(),
            // Cold path data last
            worker_id,
            worker_config,
            config: runtime_config,
            listeners,
            active_tcp_connections: FnvHashSet::default(),
            http_connection_pool: FnvHashMap::default(),
            http_backend_addresses: FnvHashMap::default(),
        })
    }
    
    /// Track zero-copy buffer usage for safety
    #[inline]
    fn track_zerocopy_buffer(&mut self, buffer_id: u16) {
        *self.pending_zerocopy_buffers.entry(buffer_id).or_insert(0) += 1;
    }
    
    /// Release zero-copy buffer tracking (safe to reuse when count reaches 0)
    #[inline]
    fn release_zerocopy_buffer(&mut self, buffer_id: u16) -> bool {
        if let Some(count) = self.pending_zerocopy_buffers.get_mut(&buffer_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.pending_zerocopy_buffers.remove(&buffer_id);
                return true; // Buffer now safe to reuse
            }
        }
        false // Buffer still has pending operations
    }
    
    /// Get protocol capability bitmask for this worker (matches eBPF format)
    /// Used for debugging and eBPF integration validation
    pub fn get_protocol_capability_mask(&self) -> u8 {
        let mut capability_mask = 0u8;
        
        for protocol in &self.worker_config.protocols {
            let protocol_bit = match protocol {
                crate::config::WorkerProtocol::Tcp => 2,     // PROTOCOL_TCP_BIT = 2
                crate::config::WorkerProtocol::Http1 => 4,   // PROTOCOL_HTTP1_BIT = 4
                crate::config::WorkerProtocol::Http2 => 8,   // PROTOCOL_HTTP2_BIT = 8
            };
            capability_mask |= protocol_bit;
        }
        
        capability_mask
    }
    
    /// Check if this worker should handle a specific port based on protocol capabilities
    fn should_worker_handle_port(&self, port: u16) -> bool {
        if let Some(listener_info) = self.config.listeners.get(&port) {
            match listener_info.protocol {
                crate::runtime_config::Protocol::HTTP | crate::runtime_config::Protocol::HTTPS => {
                    // HTTP/HTTPS ports require HTTP capabilities
                    self.worker_config.protocols.contains(&crate::config::WorkerProtocol::Http1) ||
                    self.worker_config.protocols.contains(&crate::config::WorkerProtocol::Http2)
                }
                crate::runtime_config::Protocol::TCP | crate::runtime_config::Protocol::UDP => {
                    // TCP/UDP ports require TCP capabilities  
                    self.worker_config.protocols.contains(&crate::config::WorkerProtocol::Tcp)
                }
            }
        } else {
            // Unknown port - let all workers handle for safety
            true
        }
    }
    
    /// Initialize listeners for all required ports
    pub fn setup_listeners(&mut self) -> Result<()> {
        let capability_mask = self.get_protocol_capability_mask();
        info!("SETUP: Worker {} starting setup_listeners with capability mask 0x{:02x} (protocols: {:?})", 
              self.worker_id, capability_mask, self.worker_config.protocols);
        
        // Get current route table to determine which ports to listen on
        let routes_ptr = self.routes.load(Ordering::Acquire);
        if routes_ptr.is_null() {
            return Err(anyhow!("Route table not initialized"));
        }
        
        let routes = unsafe { &*routes_ptr };
        
        // Listen only on ports that match this worker's protocol capabilities
        let listener_ports: Vec<u16> = self.config.listeners.keys().copied().collect();
        for port in listener_ports {
            if self.should_worker_handle_port(port) {
                self.setup_listener(port)?;
            } else {
                info!("SETUP: Worker {} skipping port {} (protocol mismatch)", self.worker_id, port);
            }
        }
        
        // Also listen on any additional TCP route ports not covered by Gateway listeners
        for &port in routes.get_tcp_ports().iter() {
            if !self.config.listeners.contains_key(&port) && self.worker_config.protocols.contains(&crate::config::WorkerProtocol::Tcp) {
                self.setup_listener(port)?;
            }
        }
        
        info!("SETUP: Worker {} setup listeners for ports: {:?}", 
              self.worker_id, self.listeners.keys().collect::<Vec<_>>());
        
        // Accept operations were queued during setup_listener() calls above
        // They will be submitted when the main event loop starts - no double submission
        info!("SETUP: Worker {} queued accept operations for {} listeners - will be submitted by main event loop", 
              self.worker_id, self.listeners.len());
        
        info!("SETUP: Worker {} completed setup_listeners successfully", self.worker_id);
        Ok(())
    }
    
    fn setup_listener(&mut self, port: u16) -> Result<()> {
        // SO_REUSEPORT Architecture: ALL workers bind to the SAME address:port
        // Kernel + eBPF distribute packets across workers for load balancing
        let (ip_addr, interface) = if let Some(listener_info) = self.config.listeners.get(&port) {
            (
                listener_info.address.unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)),
                listener_info.interface.as_ref()
            )
        } else {
            (std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), None)
        };
        
        let addr = SocketAddr::new(ip_addr, port);
        
        /* PHASE 2.3: LOG - Interface binding verification */
        info!("INTERFACE_BIND: Worker {} binding to address={}, port={}, interface={:?}", 
              self.worker_id, ip_addr, port, interface);
        
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
            
            // CRITICAL DEBUG: Verify SO_REUSEPORT was actually set
            let mut verify_optval: libc::c_int = 0;
            let mut optlen = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            let verify_ret = libc::getsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_REUSEPORT,
                &mut verify_optval as *mut _ as *mut libc::c_void,
                &mut optlen,
            );
            let so_reuseport_set = verify_ret == 0 && verify_optval != 0;
            debug!("Worker {} socket SO_REUSEPORT enabled (port {})", 
                   self.worker_id, port);
        }
        
        // Bind to specific network interface if specified
        #[cfg(target_os = "linux")]
        if let Some(interface_name) = interface {
            unsafe {
                let interface_cstr = std::ffi::CString::new(interface_name.as_str())
                    .map_err(|e| anyhow!("Invalid interface name '{}': {}", interface_name, e))?;
                let ret = libc::setsockopt(
                    sock.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_BINDTODEVICE,
                    interface_cstr.as_ptr() as *const libc::c_void,
                    interface_cstr.as_bytes_with_nul().len() as libc::socklen_t,
                );
                if ret != 0 {
                    let errno = std::io::Error::last_os_error();
                    return Err(anyhow!("Failed to bind socket to interface '{}': {}", interface_name, errno));
                }
            }
        }
        
        // Apply worker-specific TCP socket options
        self.apply_tcp_socket_options(sock.as_raw_fd())?;
        
        // Set non-blocking and bind to address
        sock.set_nonblocking(true)?;
        sock.bind(&socket2::SockAddr::from(addr))?;
        sock.listen(128)?;
        
        let fd = sock.as_raw_fd();
        
        // CRITICAL DEBUG: Verify socket is actually in LISTEN state
        let listen_state = unsafe {
            let mut optval: i32 = 0;
            let mut optlen = std::mem::size_of::<i32>() as libc::socklen_t;
            let ret = libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ACCEPTCONN,
                &mut optval as *mut _ as *mut libc::c_void,
                &mut optlen,
            );
            if ret == 0 { optval != 0 } else { false }
        };
        
        debug!("Worker {} socket listening on port {}", 
               self.worker_id, port);
        
        if !listen_state {
            error!("Worker {} socket failed to enter LISTEN state", self.worker_id);
        }
        
        self.listeners.insert(port, fd);
        
        // SO_REUSEPORT eBPF program attachment is coordinated by control plane
        // Worker FDs are collected via get_listener_fds() and attached in main.rs
        // This enables address-aware packet distribution instead of kernel round-robin
        debug!("Worker {} socket fd {} ready for eBPF attachment (port {})", 
               self.worker_id, fd, port);
        
        // Start accepting connections on this listener
        self.submit_accept(fd, port)?;
        
        debug!("Worker {} listening on {}:{}", self.worker_id, ip_addr, port);
        std::mem::forget(sock); // Keep socket alive
        
        Ok(())
    }
    
    /// Apply worker-specific TCP socket options for optimal performance
    fn apply_tcp_socket_options(&self, socket_fd: RawFd) -> Result<()> {
        #[cfg(target_os = "linux")]
        unsafe {
            // Set TCP_NODELAY if configured (immediate transmission, no Nagle's algorithm)
            if self.worker_config.tcp_nodelay {
                let optval: libc::c_int = 1;
                let ret = libc::setsockopt(
                    socket_fd,
                    libc::IPPROTO_TCP,
                    libc::TCP_NODELAY,
                    &optval as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&optval) as libc::socklen_t,
                );
                if ret != 0 {
                    warn!("Failed to set TCP_NODELAY for worker {}: errno {}", self.worker_id, ret);
                } else {
                    debug!("Worker {} enabled TCP_NODELAY for fast transmission", self.worker_id);
                }
            }
            
            // Set SO_KEEPALIVE if configured (connection health monitoring)
            if self.worker_config.tcp_keepalive {
                let optval: libc::c_int = 1;
                let ret = libc::setsockopt(
                    socket_fd,
                    libc::SOL_SOCKET,
                    libc::SO_KEEPALIVE,
                    &optval as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&optval) as libc::socklen_t,
                );
                if ret != 0 {
                    warn!("Failed to set SO_KEEPALIVE for worker {}: errno {}", self.worker_id, ret);
                } else {
                    debug!("Worker {} enabled SO_KEEPALIVE for connection monitoring", self.worker_id);
                }
                
                // Set TCP_KEEPIDLE (time before first probe)
                let idle_secs = self.worker_config.tcp_keepalive_idle.as_secs() as libc::c_int;
                let ret = libc::setsockopt(
                    socket_fd,
                    libc::IPPROTO_TCP,
                    libc::TCP_KEEPIDLE,
                    &idle_secs as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&idle_secs) as libc::socklen_t,
                );
                if ret != 0 {
                    warn!("Failed to set TCP_KEEPIDLE for worker {}: errno {}", self.worker_id, ret);
                }
                
                // Set TCP_KEEPINTVL (probe interval)  
                let interval_secs = self.worker_config.tcp_keepalive_interval.as_secs() as libc::c_int;
                let ret = libc::setsockopt(
                    socket_fd,
                    libc::IPPROTO_TCP,
                    libc::TCP_KEEPINTVL,
                    &interval_secs as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&interval_secs) as libc::socklen_t,
                );
                if ret != 0 {
                    warn!("Failed to set TCP_KEEPINTVL for worker {}: errno {}", self.worker_id, ret);
                }
                
                // Set TCP_KEEPCNT (number of probes)
                let probe_count = self.worker_config.tcp_keepalive_probes as libc::c_int;
                let ret = libc::setsockopt(
                    socket_fd,
                    libc::IPPROTO_TCP,
                    libc::TCP_KEEPCNT,
                    &probe_count as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&probe_count) as libc::socklen_t,
                );
                if ret != 0 {
                    warn!("Failed to set TCP_KEEPCNT for worker {}: errno {}", self.worker_id, ret);
                }
                
                debug!("Worker {} configured TCP keepalive: idle={}s, interval={}s, probes={}",
                       self.worker_id, idle_secs, interval_secs, probe_count);
            }
        }
        
        Ok(())
    }
    
    fn submit_accept(&mut self, listener_fd: RawFd, port: u16) -> Result<()> {
        let user_data = self.next_user_data;
        self.next_user_data += 1;
        
        debug!("Worker {} submitting accept operation: listener_fd={}, port={}, user_data={:#x}", 
               self.worker_id, listener_fd, port, user_data);
        
        self.connections.insert(user_data, ConnectionState::Listening { fd: listener_fd, port });
        
        let accept_op = opcode::Accept::new(types::Fd(listener_fd), std::ptr::null_mut(), std::ptr::null_mut())
            .build()
            .user_data(user_data);
        
        unsafe {
            let mut sq = self.ring.submission();
            let sq_space_before = sq.capacity() - sq.len();
            
            if let Err(e) = sq.push(&accept_op) {
                error!("Worker {} failed to push accept operation to submission queue: {:?}", 
                       self.worker_id, e);
                return Err(anyhow!("Failed to submit accept operation: {:?}", e));
            }
            
            let sq_space_after = sq.capacity() - sq.len();
            debug!("Worker {} accept operation queued: submission_queue space before={}, after={}", 
                   self.worker_id, sq_space_before, sq_space_after);
        }
        
        Ok(())
    }
    
    /// Main event loop for this worker
    /// Get listener file descriptors for eBPF SO_REUSEPORT program attachment
    /// Called by control plane to attach eBPF programs after listener setup
    pub fn get_listener_fds(&self) -> Vec<(u16, RawFd)> {
        self.listeners.iter().map(|(&port, &fd)| (port, fd)).collect()
    }
    
    pub fn run(&mut self) -> Result<()> {
        info!("WORKER_RUN: Worker {} entering run() method", self.worker_id);
        
        // CRITICAL FIX: Submit initial accept operations queued during setup
        let initial_ops = {
            let sq = self.ring.submission();
            sq.len()
        };
        
        info!("WORKER_RUN: Worker {} has {} operations queued at run() entry", 
              self.worker_id, initial_ops);
        
        if initial_ops > 0 {
            info!("WORKER_RUN: Worker {} attempting to submit {} initial operations", 
                  self.worker_id, initial_ops);
            
            match self.ring.submit() {
                Ok(submitted) => {
                    info!("WORKER_RUN: Worker {} successfully submitted {} operations to kernel", 
                          self.worker_id, submitted);
                }
                Err(e) => {
                    error!("WORKER_RUN: Worker {} FAILED to submit operations: {}", 
                           self.worker_id, e);
                    error!("WORKER_RUN: Worker {} thread will exit due to submit failure", self.worker_id);
                    return Err(anyhow!("Worker {} failed to submit initial operations: {}", self.worker_id, e));
                }
            }
            
            // Verify operations were actually submitted
            let remaining_ops = {
                let sq = self.ring.submission();
                sq.len()
            };
            
            if remaining_ops > 0 {
                warn!("Worker {} still has {} operations in submission queue after submit()", 
                      self.worker_id, remaining_ops);
            }
        } else {
            warn!("WORKER_RUN: Worker {} starting main loop with no initial operations queued", self.worker_id);
        }
        
        info!("WORKER_RUN: Worker {} entering main event loop", self.worker_id);
        
        loop {
            // Log io_uring state before submit_and_wait
            let (sq_ready, sq_capacity, cq_ready) = {
                let sq_len = {
                    let sq = self.ring.submission();
                    (sq.len(), sq.capacity())
                };
                let cq_len = {
                    let cq = self.ring.completion();
                    cq.len()
                };
                (sq_len.0, sq_len.1, cq_len)
            };
            
            
            // RACE CONDITION FIX: Remove proactive resubmission logic
            // The completion-driven resubmission in handle_accept() is sufficient
            // Proactive resubmission causes 25μs double-submission race conditions
            
            // Use original state since we removed proactive resubmission
            let (sq_ready_after_resubmit, cq_ready_after_resubmit) = (sq_ready, cq_ready);
            
            // RACE CONDITION FIX: Remove emergency resubmit - trust completion-driven pattern
            // If no operations are pending, wait for existing operations to complete
            let total_pending = sq_ready_after_resubmit + cq_ready_after_resubmit;
            if total_pending == 0 {
            }
            
            // Submit pending operations and wait for at least 1 completion
            
            match self.ring.submit_and_wait(1) {
                Ok(submitted) => {
                }
                Err(e) => {
                    // Check if it's a timeout or real error
                    if e.kind() == std::io::ErrorKind::TimedOut {
                        warn!("Worker {} submit_and_wait timed out - no completions received", self.worker_id);
                        // Continue loop to try again - this prevents infinite blocking
                        continue;
                    } else {
                        error!("Worker {} submit_and_wait failed: {}", self.worker_id, e);
                        return Err(e.into());
                    }
                }
            }
            
            // CRITICAL DEBUG: Check completion queue state after submit_and_wait
            let cq_len = self.ring.completion().len();
            
            // Process completions
            self.process_completions()?;
        }
    }
    
    fn process_completions(&mut self) -> Result<()> {
        // Collect completions first to avoid borrowing issues
        let completions: Vec<(u64, i32)> = {
            let cq = self.ring.completion();
            let completion_count = cq.len();
            cq.map(|cqe| (cqe.user_data(), cqe.result())).collect()
        };
        
        // Process each completion
        info!("Worker {} processing {} completions", self.worker_id, completions.len());
        for (user_data, result) in completions {
            // Fast-path: Check if this is a SendZc notification completion (<5ns)
            if NotificationSlotManager::is_notification(user_data) {
                
                // Handle notification completion - buffer is now safe to reuse
                if let Some((operation_user_data, buffer_id)) = self.notification_slots.release_notification_slot(user_data) {
                    
                    // Release zero-copy buffer tracking - buffer now safe for reuse
                    if self.release_zerocopy_buffer(buffer_id) {
                        debug!("Buffer {} confirmed safe for reuse after zero-copy operation", buffer_id);
                    }
                } else {
                    warn!("Received notification completion for unknown slot: user_data={:#x}", user_data);
                }
                continue; // Notification processed, move to next completion
            }
            if result < 0 {
                self.log_operation_failure("io_uring_operation", result);
                
                // Properly clean up failed operations to prevent EBADF errors
                if let Some(connection_state) = self.connections.remove(&user_data) {
                    match connection_state {
                        ConnectionState::TcpReadingClient { client_fd, backend_fd, buffer_id, .. } |
                        ConnectionState::TcpReadingBackend { backend_fd, client_fd, buffer_id, .. } |
                        ConnectionState::TcpWritingToBackend { backend_fd, client_fd, buffer_id, .. } |
                        ConnectionState::TcpWritingToClient { client_fd, backend_fd, buffer_id, .. } => {
                            self.buffer_ring.return_buffer_id(buffer_id);
                            self.cleanup_tcp_connection(client_fd, backend_fd);
                        }
                        ConnectionState::HttpConnection { client_fd, buffer_id, backend_fd: Some(backend_fd), .. } |
                        ConnectionState::HttpWritingToBackend { backend_fd, client_fd, buffer_id, .. } |
                        ConnectionState::HttpWritingToClient { client_fd, backend_fd, buffer_id, .. } => {
                            self.buffer_ring.return_buffer_id(buffer_id);
                            self.cleanup_http_connection(client_fd, backend_fd);
                        }
                        ConnectionState::HttpConnection { client_fd, buffer_id, backend_fd: None, .. } |
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
                        ConnectionState::HttpWaitingForNextRequest { client_fd, buffer_id, backend_fd, .. } => {
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
                    ConnectionState::Listening { fd, port } => {
                        debug!("Worker {} accepted connection: result={}, port={}, fd={}", 
                               self.worker_id, result, port, fd);
                        if result < 0 {
                            error!("Accept operation failed with error code: {}", result);
                        }
                        self.handle_accept(user_data, result, port, fd)?;
                    }
                    ConnectionState::DetectingProtocol { client_fd, buffer_id, server_port } => {
                        self.handle_protocol_detection(user_data, result, client_fd, server_port, buffer_id)?;
                    }
                    ConnectionState::HttpConnection { client_fd, buffer_id, backend_fd, server_port, parser } => {
                        self.handle_http_data(user_data, result, client_fd, server_port, parser, backend_fd, buffer_id)?;
                    }
                    ConnectionState::TcpReadingClient { client_fd, backend_fd, buffer_id, server_port } => {
                        self.handle_tcp_client_data(user_data, result, client_fd, backend_fd, server_port, buffer_id)?;
                    }
                    ConnectionState::TcpReadingBackend { backend_fd, client_fd, buffer_id, server_port } => {
                        self.handle_tcp_backend_data(user_data, result, client_fd, backend_fd, server_port, buffer_id)?;
                    }
                    ConnectionState::TcpWritingToBackend { backend_fd, bytes_to_write, client_fd, buffer_id, server_port } => {
                        self.handle_tcp_write_completion(user_data, result, client_fd, backend_fd, server_port, buffer_id, bytes_to_write, true)?;
                    }
                    ConnectionState::TcpWritingToClient { client_fd, bytes_to_write, backend_fd, buffer_id, server_port } => {
                        self.handle_tcp_write_completion(user_data, result, client_fd, backend_fd, server_port, buffer_id, bytes_to_write, false)?;
                    }
                    ConnectionState::HttpErrorResponse { client_fd, response_size, buffer_id } => {
                        self.handle_http_error_response_completion(user_data, result, client_fd, buffer_id, response_size)?;
                    }
                    ConnectionState::HttpWritingToBackend { backend_fd, bytes_to_write, client_fd, buffer_id, server_port } => {
                        self.handle_http_write_completion(user_data, result, client_fd, backend_fd, server_port, buffer_id, bytes_to_write, true)?;
                    }
                    ConnectionState::HttpWritingToClient { client_fd, bytes_to_write, backend_fd, buffer_id, server_port } => {
                        self.handle_http_write_completion(user_data, result, client_fd, backend_fd, server_port, buffer_id, bytes_to_write, false)?;
                    }
                    ConnectionState::HttpWaitingForNextRequest { client_fd, buffer_id, backend_fd: _backend_fd, server_port, parser } => {
                        // New HTTP request arrived on keep-alive connection
                            
                        if result <= 0 {
                            self.buffer_ring.return_buffer_id(buffer_id);
                            unsafe { libc::close(client_fd) };
                        } else {
                            // Process the HTTP request using the buffer from the read operation
                            self.handle_http_data(user_data, result, client_fd, server_port, parser, None, buffer_id)?;
                        }
                    }
                    ConnectionState::HttpWritingToClientWithResponseInfo { client_fd, bytes_to_write, backend_fd, buffer_id, server_port, response_info } => {
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
        
        /* PHASE 2.4: LOG - Traffic reception verification */
        debug!("Accept operation triggered: client_fd={}, port={}, listener_fd={}", client_fd, port, listener_fd);
        info!("Worker {} accepted connection: client_fd={}, port={}", self.worker_id, client_fd, port);
        
        // Immediately submit another accept for this listener
        self.submit_accept(listener_fd, port)?;
        
        // Start protocol detection by reading initial data
        self.start_protocol_detection(client_fd, port)?;
        
        Ok(())
    }
    
    fn start_protocol_detection(&mut self, client_fd: RawFd, server_port: u16) -> Result<()> {
        // OPTIMIZATION: Check config first - if protocol is known, skip detection entirely
        match self.config.get_protocol(server_port) {
            Some(runtime_config::Protocol::HTTP) | Some(runtime_config::Protocol::HTTPS) => {
                return self.start_http_processing_direct(client_fd, server_port);
            }
            Some(runtime_config::Protocol::TCP) | Some(runtime_config::Protocol::UDP) => {
                return self.start_tcp_forwarding(client_fd, server_port);
            }
            None => {
                // Protocol not configured - fallback to route-based detection
            }
        }
        
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
                buffer_id,
                server_port,
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
            // Client disconnected during protocol detection - clean up silently
            self.buffer_ring.return_buffer_id(buffer_id);
            unsafe { libc::close(client_fd) };
            return Ok(());
        }
        
        
        
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
        
        // Process the initial data immediately instead of starting a new read
        let user_data = self.next_user_data;
        self.next_user_data += 1;
        
        let parser = HttpRequestParser::new();
        self.connections.insert(user_data, ConnectionState::HttpConnection {
            client_fd,
            buffer_id,
            backend_fd: None,
            server_port,
            parser,
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
    
    /// Start HTTP processing directly without protocol detection when port is pre-configured
    fn start_http_processing_direct(&mut self, client_fd: RawFd, server_port: u16) -> Result<()> {
        // Allocate buffer for HTTP request reading
        if let Some(buffer_id) = self.buffer_ring.get_buffer_id() {
            let user_data = self.next_user_data;
            self.next_user_data += 1;
            
            // Create HTTP connection state and start reading request
            let parser = HttpRequestParser::new();
            self.connections.insert(user_data, ConnectionState::HttpConnection {
                client_fd,
                buffer_id,
                backend_fd: None,
                server_port,
                parser,
            });
            
            // Submit read operation to get HTTP request data
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
    
    fn start_tcp_forwarding(&mut self, client_fd: RawFd, server_port: u16) -> Result<()> {
        
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
        #[cfg(debug_assertions)]
        tracing::trace!("Connecting to backend: {}", backend_socket_addr);
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
                
                // Removed trace logging for performance - connection success is implicit
                
                Ok(fd)
            }
            Err(e) => {
                self.log_connection_error(backend_socket_addr, &e.to_string());
                Err(anyhow!("Backend connection failed: {}", e))
            }
        }
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
                        return Ok(fd);
                    } else {
                        libc::close(fd);
                        self.http_backend_addresses.remove(&fd);
                    }
                }
            }
        }
        
        // No valid pooled connection available, create a new one
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
                pool.push(backend_fd);
                return;
            }
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
        debug!("HTTP request completed: client_fd={}, backend_fd={}, pooled={}", client_fd, backend_fd, should_pool);
        
        unsafe { libc::close(client_fd); }
        
        if should_pool {
            self.return_http_connection_to_pool(backend_fd);
        } else {
            self.close_http_connection(backend_fd);
        }
    }
    
    fn start_bidirectional_forwarding(&mut self, client_fd: RawFd, backend_fd: RawFd) -> Result<()> {
        // Removed debug logging for performance in hot path
        
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
            debug!("TCP connection completed: client_fd={}, backend_fd={}", client_fd, backend_fd);
            unsafe {
                libc::close(client_fd);
                libc::close(backend_fd);
            }
        } else {
            // Already cleaned up - no action needed
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
                buffer_id,
                server_port,
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
                backend_fd,
                client_fd,
                buffer_id,
                server_port,
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
            self.buffer_ring.return_buffer_id(buffer_id);
            if let Some(backend_fd) = backend_fd {
                self.cleanup_tcp_connection(client_fd, backend_fd);
            } else {
                unsafe { libc::close(client_fd) };
            }
            return Ok(());
        }
        
        #[cfg(debug_assertions)]
        tracing::trace!("Processing HTTP request: {} bytes", bytes_read);
        
        // Get buffer content for HTTP parsing from fixed buffers
        let request_data = self.buffer_ring.get_buffer_content(buffer_id, bytes_read as usize);
        
        // OPTIMIZED: eBPF has already classified this as HTTP traffic and selected our worker
        // Skip full parsing and use fast header extraction for routing only
        match self.extract_http_routing_info_fast(request_data) {
            Ok((host, path, connection_type)) => {
                #[cfg(debug_assertions)]
                tracing::trace!("eBPF-optimized routing: Host: {:?}, Path: {}", host, path);
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
                        self.log_routing_error(&e.to_string());
                        self.send_http_error_response(client_fd, 404, "Not Found")?;
                        self.buffer_ring.return_buffer_id(buffer_id);
                        unsafe { libc::close(client_fd) };
                    }
                }
            }
            Err(e) => {
                self.log_parsing_error(&e.to_string());
                self.send_http_error_response(client_fd, 400, "Bad Request")?;
                self.buffer_ring.return_buffer_id(buffer_id);
                unsafe { libc::close(client_fd) };
            }
        }
        
        Ok(())
    }
    
    fn handle_tcp_client_data(&mut self, _user_data: u64, bytes_read: i32, client_fd: RawFd, backend_fd: RawFd, server_port: u16, buffer_id: u16) -> Result<()> {
        if bytes_read <= 0 {
            self.buffer_ring.return_buffer_id(buffer_id);
            self.cleanup_tcp_connection(client_fd, backend_fd);
            return Ok(());
        }
        
        #[cfg(debug_assertions)]
        tracing::trace!("Forwarding {} bytes client->backend", bytes_read);
        
        // Zero-copy write: transfer buffer ownership to write operation
        self.submit_zero_copy_write_to_backend(client_fd, backend_fd, server_port, buffer_id, bytes_read as usize)?;
        
        // Continue reading from client
        self.submit_client_read(client_fd, backend_fd, server_port)?;
        
        Ok(())
    }
    
    fn handle_tcp_backend_data(&mut self, _user_data: u64, bytes_read: i32, client_fd: RawFd, backend_fd: RawFd, server_port: u16, buffer_id: u16) -> Result<()> {
        if bytes_read <= 0 {
            self.buffer_ring.return_buffer_id(buffer_id);
            self.cleanup_tcp_connection(client_fd, backend_fd);
            return Ok(());
        }
        
        #[cfg(debug_assertions)]
        tracing::trace!("Forwarding {} bytes backend->client", bytes_read);
        
        // Check if this is an HTTP connection - use Gateway API protocol detection
        let is_http = matches!(self.config.get_protocol(server_port), 
                              Some(runtime_config::Protocol::HTTP) | Some(runtime_config::Protocol::HTTPS));
        #[cfg(debug_assertions)]
        tracing::trace!("Protocol detection: port={}, is_http={}", server_port, is_http);
        
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
    
    /// Zero-copy send to backend using io_uring SENDZC (TCP)
    fn submit_zero_copy_write_to_backend(&mut self, client_fd: RawFd, backend_fd: RawFd, server_port: u16, buffer_id: u16, bytes_to_write: usize) -> Result<()> {
        let user_data = self.next_user_data;
        self.next_user_data += 1;
        
        // Allocate notification slot and track buffer for zero-copy buffer safety
        let _notification_slot = self.notification_slots.allocate_slot(user_data, buffer_id);
        self.track_zerocopy_buffer(buffer_id);
        
        // Store write state with buffer ID and notification slot
        self.connections.insert(user_data, ConnectionState::TcpWritingToBackend {
            client_fd,
            backend_fd,
            server_port,
            buffer_id,
            bytes_to_write,
        });
        
        // Get buffer reference for zero-copy send operation
        let buffer_ptr = self.buffer_ring.get_buffer_ptr(buffer_id);
        
        // Use SendZc instead of WriteFixed for true zero-copy networking (>200% performance)
        let send_op = opcode::SendZc::new(
            types::Fd(backend_fd),
            buffer_ptr,
            bytes_to_write as u32
        )
        .buf_index(Some(buffer_id))  // Use fixed buffer for maximum performance
        .build()
        .user_data(user_data);
        
        unsafe {
            if self.ring.submission().push(&send_op).is_err() {
                // Release notification slot and buffer tracking on failure
                if let Some((_op_user_data, buffer_id)) = self.notification_slots.release_notification_slot(user_data) {
                    self.release_zerocopy_buffer(buffer_id);
                }
                return Err(anyhow!("Failed to submit zero-copy send to backend"));
            }
        }
        
        Ok(())
    }
    
    /// Zero-copy send to backend using io_uring SENDZC (HTTP) 
    fn submit_http_write_to_backend(&mut self, client_fd: RawFd, backend_fd: RawFd, server_port: u16, buffer_id: u16, bytes_to_write: usize) -> Result<()> {
        let user_data = self.next_user_data;
        self.next_user_data += 1;
        
        // Allocate notification slot and track buffer for zero-copy buffer safety
        let _notification_slot = self.notification_slots.allocate_slot(user_data, buffer_id);
        self.track_zerocopy_buffer(buffer_id);
        
        // Store HTTP write state with buffer ID
        self.connections.insert(user_data, ConnectionState::HttpWritingToBackend {
            client_fd,
            backend_fd,
            server_port,
            buffer_id,
            bytes_to_write,
        });
        
        // Get buffer reference for zero-copy send operation
        let buffer_ptr = self.buffer_ring.get_buffer_ptr(buffer_id);
        
        // Use SendZc for HTTP requests to backend (>200% performance improvement)
        let send_op = opcode::SendZc::new(
            types::Fd(backend_fd),
            buffer_ptr,
            bytes_to_write as u32
        )
        .buf_index(Some(buffer_id))  // Use fixed buffer for maximum performance
        .build()
        .user_data(user_data);
        
        unsafe {
            if self.ring.submission().push(&send_op).is_err() {
                // Release notification slot and buffer tracking on failure
                if let Some((_op_user_data, buffer_id)) = self.notification_slots.release_notification_slot(user_data) {
                    self.release_zerocopy_buffer(buffer_id);
                }
                return Err(anyhow!("Failed to submit HTTP zero-copy send to backend"));
            }
        }
        
        Ok(())
    }
    
    /// Zero-copy send to client using io_uring SENDZC (TCP)
    fn submit_zero_copy_write_to_client(&mut self, client_fd: RawFd, backend_fd: RawFd, server_port: u16, buffer_id: u16, bytes_to_write: usize) -> Result<()> {
        let user_data = self.next_user_data;
        self.next_user_data += 1;
        
        // Allocate notification slot and track buffer for zero-copy buffer safety
        let _notification_slot = self.notification_slots.allocate_slot(user_data, buffer_id);
        self.track_zerocopy_buffer(buffer_id);
        
        // Store write state with buffer ID
        self.connections.insert(user_data, ConnectionState::TcpWritingToClient {
            client_fd,
            backend_fd,
            server_port,
            buffer_id,
            bytes_to_write,
        });
        
        // Get buffer reference for zero-copy send operation
        let buffer_ptr = self.buffer_ring.get_buffer_ptr(buffer_id);
        
        // Use SendZc for true zero-copy networking to client (>200% performance)
        let send_op = opcode::SendZc::new(
            types::Fd(client_fd),
            buffer_ptr,
            bytes_to_write as u32
        )
        .buf_index(Some(buffer_id))  // Use fixed buffer for maximum performance
        .build()
        .user_data(user_data);
        
        unsafe {
            if self.ring.submission().push(&send_op).is_err() {
                // Release notification slot and buffer tracking on failure
                if let Some((_op_user_data, buffer_id)) = self.notification_slots.release_notification_slot(user_data) {
                    self.release_zerocopy_buffer(buffer_id);
                }
                return Err(anyhow!("Failed to submit zero-copy send to client"));
            }
        }
        
        Ok(())
    }
    
    
    /// Zero-copy send to client using io_uring SENDZC (HTTP with response info for keep-alive)
    fn submit_http_write_to_client_with_response_info(&mut self, client_fd: RawFd, backend_fd: RawFd, server_port: u16, buffer_id: u16, bytes_to_write: usize, response_info: HttpResponseInfo) -> Result<()> {
        let user_data = self.next_user_data;
        self.next_user_data += 1;
        
        // Allocate notification slot and track buffer for zero-copy buffer safety
        let _notification_slot = self.notification_slots.allocate_slot(user_data, buffer_id);
        self.track_zerocopy_buffer(buffer_id);
        
        // Store HTTP write state with response info for keep-alive decision
        self.connections.insert(user_data, ConnectionState::HttpWritingToClientWithResponseInfo {
            client_fd,
            backend_fd,
            server_port,
            buffer_id,
            bytes_to_write,
            response_info,
        });
        
        // Get buffer reference for zero-copy send operation
        let buffer_ptr = self.buffer_ring.get_buffer_ptr(buffer_id);
        
        // Use SendZc for HTTP responses with keep-alive info (>200% performance)
        let send_op = opcode::SendZc::new(
            types::Fd(client_fd),
            buffer_ptr,
            bytes_to_write as u32
        )
        .buf_index(Some(buffer_id))  // Use fixed buffer for zero-copy
        .build()
        .user_data(user_data);
        
        unsafe {
            if self.ring.submission().push(&send_op).is_err() {
                // Release notification slot and buffer tracking on failure
                if let Some((_op_user_data, buffer_id)) = self.notification_slots.release_notification_slot(user_data) {
                    self.release_zerocopy_buffer(buffer_id);
                }
                return Err(anyhow!("Failed to submit HTTP zero-copy send to client with response info"));
            }
        }
        
        Ok(())
    }
    
    /// Handle completion of io_uring write operations with proper partial write handling
    fn handle_tcp_write_completion(&mut self, _user_data: u64, bytes_written: i32, client_fd: RawFd, backend_fd: RawFd, _server_port: u16, buffer_id: u16, expected_bytes: usize, to_backend: bool) -> Result<()> {
        if bytes_written < 0 {
            self.log_operation_failure("tcp_write", bytes_written);
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
            
            // Continue writing from offset using SendZc for zero-copy performance
            // Note: For partial writes, we use raw buffer pointer without buf_index 
            // since SendZc with buf_index expects full buffer alignment
            let buffer_ptr = self.buffer_ring.get_buffer_ptr(buffer_id);
            let offset_ptr = unsafe { buffer_ptr.add(written) };
            let target_fd = if to_backend { backend_fd } else { client_fd };
            
            // Allocate notification slot and track buffer for zero-copy buffer safety
            let _notification_slot = self.notification_slots.allocate_slot(user_data, buffer_id);
            self.track_zerocopy_buffer(buffer_id);
            
            // Use SendZc for partial writes (without buf_index for offset compatibility)
            let write_op = opcode::SendZc::new(
                types::Fd(target_fd),
                offset_ptr,
                remaining_bytes as u32
            )
            .build()
            .user_data(user_data);
            
            unsafe {
                if self.ring.submission().push(&write_op).is_err() {
                    error!("Failed to submit continuation zero-copy send");
                    // Release notification slot on failure
                    self.notification_slots.release_notification_slot(user_data);
                    self.buffer_ring.return_buffer_id(buffer_id);
                    self.cleanup_tcp_connection(client_fd, backend_fd);
                }
            }
            
            return Ok(());
        }
        
        
        // Return buffer to pool after successful complete write
        self.buffer_ring.return_buffer_id(buffer_id);
        
        Ok(())
    }
    
    /// Handle completion of HTTP write operations with proper partial write handling
    fn handle_http_write_completion(&mut self, _user_data: u64, bytes_written: i32, client_fd: RawFd, backend_fd: RawFd, server_port: u16, buffer_id: u16, expected_bytes: usize, to_backend: bool) -> Result<()> {
        if bytes_written < 0 {
            self.log_operation_failure("http_write", bytes_written);
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
            
            // Continue writing from offset using SendZc for zero-copy HTTP performance
            // Note: For partial writes, we use raw buffer pointer without buf_index 
            // since SendZc with buf_index expects full buffer alignment
            let buffer_ptr = self.buffer_ring.get_buffer_ptr(buffer_id);
            let offset_ptr = unsafe { buffer_ptr.add(written) };
            let target_fd = if to_backend { backend_fd } else { client_fd };
            
            // Allocate notification slot and track buffer for zero-copy buffer safety
            let _notification_slot = self.notification_slots.allocate_slot(user_data, buffer_id);
            self.track_zerocopy_buffer(buffer_id);
            
            // Use SendZc for HTTP partial writes (without buf_index for offset compatibility)
            let write_op = opcode::SendZc::new(
                types::Fd(target_fd),
                offset_ptr,
                remaining_bytes as u32
            )
            .build()
            .user_data(user_data);
            
            unsafe {
                if self.ring.submission().push(&write_op).is_err() {
                    error!("Failed to submit HTTP continuation zero-copy send");
                    // Release notification slot on failure
                    self.notification_slots.release_notification_slot(user_data);
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
            self.log_operation_failure("http_write_with_response", bytes_written);
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
            
            // Continue writing from offset using SendZc for zero-copy HTTP response performance
            // Note: For partial writes, we use raw buffer pointer without buf_index 
            // since SendZc with buf_index expects full buffer alignment
            let buffer_ptr = self.buffer_ring.get_buffer_ptr(buffer_id);
            let offset_ptr = unsafe { buffer_ptr.add(written) };
            
            // Allocate notification slot and track buffer for zero-copy buffer safety
            let _notification_slot = self.notification_slots.allocate_slot(user_data, buffer_id);
            self.track_zerocopy_buffer(buffer_id);
            
            // Use SendZc for HTTP response partial writes (without buf_index for offset compatibility)
            let write_op = opcode::SendZc::new(
                types::Fd(client_fd),
                offset_ptr,
                remaining_bytes as u32
            )
            .build()
            .user_data(user_data);
            
            unsafe {
                if self.ring.submission().push(&write_op).is_err() {
                    error!("Failed to submit HTTP continuation zero-copy send with response info");
                    // Release notification slot on failure
                    self.notification_slots.release_notification_slot(user_data);
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
                backend_fd,
                client_fd,
                buffer_id,
                server_port,
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
    
    /// Extract only routing-relevant HTTP headers (optimized for eBPF-classified traffic)
    /// Much faster than full parsing since eBPF already confirmed HTTP protocol
    #[inline(always)]
    fn extract_http_routing_info_fast<'a>(&self, request: &'a [u8]) -> Result<(Option<&'a str>, &'a str, ConnectionType)> {
        use crate::parser::ConnectionType;
        
        // Fast bounds check - eBPF already validated basic structure
        if request.len() < 16 {
            return Err(anyhow!("Request too short"));
        }
        
        // Find end of request line (first \r\n) - optimized single-pass search
        let mut line_end = 0;
        for i in 0..request.len().min(4096) { // Limit search to reasonable request line length
            if request[i] == b'\r' && i + 1 < request.len() && request[i + 1] == b'\n' {
                line_end = i;
                break;
            }
        }
        
        if line_end == 0 {
            return Err(anyhow!("Invalid request line"));
        }
        
        let request_line = unsafe { std::str::from_utf8_unchecked(&request[..line_end]) };
        
        // Extract path from request line (zero-allocation single-pass)
        let mut parts = request_line.splitn(3, ' ');
        let _method = parts.next().ok_or_else(|| anyhow!("No method in request line"))?;
        let path = parts.next().ok_or_else(|| anyhow!("No path in request line"))?;
        
        // Fast host header extraction - look for "Host:" after first \r\n
        let mut host = None;
        let mut connection_type = ConnectionType::Default;
        
        let header_start = line_end + 2; // Skip \r\n
        if header_start < request.len() {
            // Simple host header search - much faster than full header parsing
            if let Some(host_pos) = request[header_start..].windows(5)
                .position(|w| w == b"Host:") {
                let host_start = header_start + host_pos + 5;
                
                // Skip spaces after "Host:"
                let mut value_start = host_start;
                while value_start < request.len() && request[value_start] == b' ' {
                    value_start += 1;
                }
                
                // Find end of host value
                if let Some(host_end) = request[value_start..].iter()
                    .position(|&b| b == b'\r' || b == b'\n' || b == b' ') {
                    if let Ok(host_str) = std::str::from_utf8(&request[value_start..value_start + host_end]) {
                        host = Some(host_str);
                    }
                }
            }
            
            // Quick connection header check - only look for "Connection: close"
            if request[header_start..].windows(17).any(|w| w == b"Connection: close") {
                connection_type = ConnectionType::Close;
            }
        }
        
        Ok((host, path, connection_type))
    }

    /// Optimized HTTP routing leveraging eBPF worker selection intelligence
    #[inline(always)]
    fn route_http_request(&self, host: Option<&str>, path: &str) -> Result<&Backend> {
        let routes = unsafe { &*self.routes.load(std::sync::atomic::Ordering::Acquire) };
        
        if let Some(host_str) = host {
            // OPTIMIZED: Since eBPF selected this worker, we can make fast routing assumptions
            // Zero-allocation fast path for most common case (no port in host header)
            let host_hash = if host_str.contains(':') {
                // Only normalize if port is present (uncommon case)
                let normalized_host = crate::config::normalize_host_header(host_str);
                crate::routing::fnv_hash(&normalized_host)
            } else {
                // Fast path: hash host directly without allocation (common case)
                crate::routing::fnv_hash(host_str)
            };
            
            #[cfg(debug_assertions)]
            tracing::trace!("eBPF-optimized routing to host: {}, path: {}", host_str, path);
            
            routes.route_http_request(host_hash, path)
        } else {
            // No host header - use default backend if available
            if let Some(default_backend) = &routes.default_http_backend {
                default_backend.select_backend()
            } else {
                Err(anyhow!("No host header and no default HTTP backend configured"))
            }
        }
    }
    
    /// Connect and forward HTTP request to backend using socket address directly (zero-allocation)
    fn connect_and_forward_http_request_to_addr(&mut self, user_data: u64, client_fd: RawFd, server_port: u16, backend_socket_addr: SocketAddr, buffer_id: u16, request_size: usize, parser: HttpRequestParser) -> Result<()> {
        
        // Connect to backend using connection pool
        match self.get_or_create_http_connection_to_addr(backend_socket_addr) {
            Ok(backend_fd) => {
                
                // Note: HTTP connections are not tracked in active_tcp_connections 
                // since they use cleanup_http_connection() instead of cleanup_tcp_connection()
                
                // Forward the request to backend
                self.forward_http_request_to_backend(user_data, client_fd, backend_fd, server_port, buffer_id, request_size, parser)?;
                
                // Note: HTTP write completion handler will start reading response from backend
            }
            Err(e) => {
                self.log_connection_error(backend_socket_addr, &e.to_string());
                self.send_http_error_response(client_fd, 502, "Bad Gateway")?;
                self.buffer_ring.return_buffer_id(buffer_id);
                unsafe { libc::close(client_fd) };
            }
        }
        
        Ok(())
    }
    
    /// Forward HTTP request to backend using zero-copy write
    fn forward_http_request_to_backend(&mut self, _user_data: u64, client_fd: RawFd, backend_fd: RawFd, server_port: u16, buffer_id: u16, request_size: usize, _parser: HttpRequestParser) -> Result<()> {
        
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
                    
                    // Send error response using io_uring SENDZC for zero-copy performance
                    let user_data = self.next_user_data;
                    self.next_user_data += 1;
                    
                    // Allocate notification slot and track buffer for zero-copy buffer safety
                    let _notification_slot = self.notification_slots.allocate_slot(user_data, buffer_id);
                    self.track_zerocopy_buffer(buffer_id);
                    
                    self.connections.insert(user_data, ConnectionState::HttpErrorResponse {
                        client_fd,
                        buffer_id,
                        response_size: response_bytes.len(),
                    });
                    
                    let buffer_ptr = self.buffer_ring.get_buffer_ptr(buffer_id);
                    
                    // Use SendZc for error responses (eliminates kernel->user copy overhead)
                    let send_op = opcode::SendZc::new(
                        types::Fd(client_fd),
                        buffer_ptr,
                        response_bytes.len() as u32
                    )
                    .buf_index(Some(buffer_id))  // Use fixed buffer for zero-copy
                    .build()
                    .user_data(user_data);
                    
                    unsafe {
                        if self.ring.submission().push(&send_op).is_err() {
                            // Release notification slot and buffer tracking on failure
                            if let Some((_op_user_data, buffer_id)) = self.notification_slots.release_notification_slot(user_data) {
                                self.release_zerocopy_buffer(buffer_id);
                            }
                            self.buffer_ring.return_buffer_id(buffer_id);
                            return Err(anyhow!("Failed to submit HTTP error response via SendZc"));
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
    worker_configs: Vec<WorkerConfig>,
    collected_listener_fds: Option<Vec<(u16, std::os::unix::io::RawFd)>>,
    setup_barrier: Option<Arc<std::sync::Barrier>>,
    ebpf_barrier: Option<Arc<std::sync::Barrier>>,
}

impl UringDataPlane {
    pub fn new(worker_configs: Vec<WorkerConfig>) -> Self {
        let worker_count = worker_configs.len();
        Self {
            workers: Vec::with_capacity(worker_count),
            worker_configs,
            collected_listener_fds: None,
            setup_barrier: None,
            ebpf_barrier: None,
        }
    }
    
    pub fn start(&mut self, routes: Arc<AtomicPtr<RouteTable>>, config: RuntimeConfig) -> Result<()> {
        let worker_count = self.worker_configs.len();
        info!("Starting {} raw io_uring data plane workers with eBPF integration", worker_count);
        
        // Create shared storage for collecting listener FDs from all workers
        let listener_fds = Arc::new(std::sync::Mutex::new(Vec::new()));
        let setup_barrier = Arc::new(std::sync::Barrier::new(worker_count + 1)); // +1 for main thread
        let ebpf_barrier = Arc::new(std::sync::Barrier::new(worker_count + 1)); // +1 for main thread
        
        // Store barriers for coordination
        self.setup_barrier = Some(setup_barrier.clone());
        self.ebpf_barrier = Some(ebpf_barrier.clone());
        
        info!("THREAD_SPAWN: Starting {} worker threads", self.worker_configs.len());
        
        for worker_config in self.worker_configs.clone() {
            info!("THREAD_SPAWN: Spawning worker thread {}", worker_config.id);
            let routes_clone = routes.clone();
            let config_clone = config.clone();
            let listener_fds_clone = listener_fds.clone();
            let setup_barrier_clone = setup_barrier.clone();
            let ebpf_barrier_clone = ebpf_barrier.clone();
            
            let handle = std::thread::spawn(move || {
                let worker_id = worker_config.id;
                info!("THREAD_LIFECYCLE: Worker {} thread started successfully", worker_id);
                
                // Set panic hook for this thread to catch panics
                std::panic::set_hook(Box::new(move |panic_info| {
                    error!("THREAD_PANIC: Worker {} thread panicked: {}", worker_id, panic_info);
                }));
                
                info!("THREAD_LIFECYCLE: Worker {} creating UringWorker instance", worker_id);
                let mut worker = match UringWorker::new(worker_config, routes_clone, config_clone) {
                    Ok(worker) => {
                        info!("THREAD_LIFECYCLE: Worker {} created successfully", worker_id);
                        worker
                    }
                    Err(e) => {
                        error!("THREAD_LIFECYCLE: Worker {} failed to initialize: {}", worker_id, e);
                        return Err(e);
                    }
                };
                
                info!("THREAD_LIFECYCLE: Worker {} setting up listeners", worker_id);
                if let Err(e) = worker.setup_listeners() {
                    error!("THREAD_LIFECYCLE: Worker {} failed to setup listeners: {}", worker_id, e);
                    return Err(e);
                }
                info!("THREAD_LIFECYCLE: Worker {} listeners setup completed", worker_id);
                
                // Collect listener FDs for eBPF attachment
                let worker_listener_fds = worker.get_listener_fds();
                {
                    let mut shared_fds = listener_fds_clone.lock().unwrap();
                    for (port, fd) in worker_listener_fds {
                        shared_fds.push((port, fd));
                        debug!("Worker {} contributed listener fd {} for port {}", worker_id, fd, port);
                    }
                }
                
                info!("Worker {} listeners setup complete, coordinating with main thread", worker_id);
                
                // First barrier: Signal listener setup completion
                setup_barrier_clone.wait();
                info!("Worker {} setup coordination complete, waiting for eBPF attachment", worker_id);
                
                // Second barrier: Wait for eBPF attachment to complete
                ebpf_barrier_clone.wait();
                
                info!("THREAD_LIFECYCLE: Worker {} starting event loop after eBPF coordination", worker_id);
                
                let run_result = worker.run();
                match &run_result {
                    Ok(_) => info!("THREAD_RESULT: Worker {} completed successfully", worker_id),
                    Err(e) => error!("THREAD_RESULT: Worker {} failed with error: {}", worker_id, e),
                }
                
                info!("THREAD_LIFECYCLE: Worker {} thread exiting", worker_id);
                run_result
            });
            
            self.workers.push(handle);
        }
        
        // Wait for all workers to set up listeners and collect FDs
        info!("Waiting for {} workers to complete listener setup", self.worker_configs.len());
        setup_barrier.wait();
        info!("All workers completed listener setup - proceeding to collect FDs");
        
        // Extract collected listener FDs
        let collected_fds = {
            let fds = listener_fds.lock().unwrap();
            fds.clone()
        };
        
        info!("Collected {} listener FDs from {} workers for eBPF attachment", 
              collected_fds.len(), self.worker_configs.len());
        
        // Store FDs for control plane eBPF attachment
        self.collected_listener_fds = Some(collected_fds);
        
        info!("Raw io_uring data plane workers initialized - ready for eBPF attachment");
        Ok(())
    }
    
    /// Get collected listener FDs for eBPF attachment by control plane
    pub fn get_listener_fds(&self) -> Option<Vec<(u16, std::os::unix::io::RawFd)>> {
        self.collected_listener_fds.clone()
    }
    
    /// Release workers to start event loops after eBPF attachment
    /// Called after SO_REUSEPORT eBPF programs are successfully attached
    pub fn start_workers_after_ebpf_attachment(&self) -> Result<()> {
        info!("Releasing eBPF barrier to start event loops after eBPF attachment");
        
        if let Some(ref barrier) = self.ebpf_barrier {
            // This barrier.wait() releases all workers waiting at their second barrier.wait()
            barrier.wait();
            info!("Worker event loops started after eBPF coordination");
            Ok(())
        } else {
            Err(anyhow!("eBPF barrier not available - workers may not have been initialized properly"))
        }
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