//! UringWorker Modular Architecture
//!
//! This module implements the raw io_uring data plane worker with clean modular separation:
//! - Immutable analysis phase (request_processor.rs) 
//! - Mutable execution phase (decision_executor.rs)
//! - Connection state management (connection_state.rs)
//! - I/O operations (io_operations.rs) 
//! - Protocol-specific handlers (tcp_handler.rs)

// Module declarations
pub mod request_processor;
pub mod io_operations;
pub mod unified_http_parser;
pub mod native_buffer_ring;
pub mod user_data;
// Removed: connection_manager, decision_executor, connection_state, tcp_handler, cleanup_system

// Re-export public types
pub use request_processor::{ProcessingDecision, ProcessingContext};
pub use native_buffer_ring::NativeBufferRing;
use user_data::{*, encode_connection_data_with_port, extract_server_port, BACKEND_CONNECT_FLAG};

use anyhow::{anyhow, Result};
use io_uring::{IoUring, opcode, types};
use fnv::FnvHashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, AtomicBool, AtomicU32, Ordering};
use std::os::unix::io::RawFd;
use std::net::SocketAddr;
use std::time::Duration;
use tracing::{error, warn};

// Conditional compilation for debug/info logging
// In release builds, these become no-ops with zero runtime cost
#[cfg(debug_assertions)]
use tracing::{info, debug};

#[cfg(not(debug_assertions))]
macro_rules! debug {
    ($($arg:tt)*) => {};
}

#[cfg(not(debug_assertions))]
macro_rules! info {
    ($($arg:tt)*) => {};
}

// Import from parent modules
use crate::routing::RouteTable;
use crate::runtime_config::RuntimeConfig;
use crate::config::{WorkerConfig, PerformanceConfig};
use crate::uring_worker::unified_http_parser::ConnectionType;
use crate::backend_pool::TcpKeepAliveConfig;
// Removed: use crate::connection::HttpConnectionManager;

// === CONNECTION DATA STRUCTURES ===
// Moved from connection_manager.rs for direct access

/// Protocol type for connection classification
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    HTTP,
    TCP,
}

/// Per-worker load balancing state - eliminates shared atomic contention
/// Cache-line aligned to prevent false sharing between workers
#[derive(Debug)]
#[repr(C, align(64))]
pub struct WorkerLoadBalancerState {
    /// Pre-allocated counters for backend pools (power-of-2: 64 backends per route)
    backend_counters: [AtomicU32; 64],
    /// Mapping from route hash to counter index for O(1) counter selection
    route_to_counter: FnvHashMap<u64, u8>,
    /// Worker-specific offset for initial distribution across backends
    worker_offset: u32,
    /// Next available counter index for new routes
    next_counter_index: u8,
}

impl WorkerLoadBalancerState {
    /// Create new worker load balancing state with worker-specific offset
    fn new(worker_id: usize) -> Self {
        // Initialize all counters with worker-specific offset for even distribution
        let worker_offset = (worker_id & 63) as u32; // Use bit mask for power-of-2
        let mut counters = [const { AtomicU32::new(0) }; 64];
        for counter in &mut counters {
            *counter = AtomicU32::new(worker_offset);
        }
        
        Self {
            backend_counters: counters,
            route_to_counter: FnvHashMap::default(),
            worker_offset,
            next_counter_index: 0,
        }
    }
    
    /// Get or create counter for a route (host_hash + path_hash combination)
    fn get_or_create_counter(&mut self, route_hash: u64) -> u8 {
        if let Some(&counter_index) = self.route_to_counter.get(&route_hash) {
            return counter_index;
        }
        
        // Allocate new counter for this route
        let counter_index = self.next_counter_index;
        if counter_index < 64 {
            self.route_to_counter.insert(route_hash, counter_index);
            self.next_counter_index += 1;
            counter_index
        } else {
            // Fallback using bit mask if we exceed pre-allocated counters
            (route_hash & 63) as u8
        }
    }
    
    /// Select backend using worker-local round-robin counter with power-of-2 optimization
    fn select_backend_index(&mut self, route_hash: u64, backend_count: usize) -> usize {
        let counter_index = self.get_or_create_counter(route_hash);
        let counter = &self.backend_counters[counter_index as usize];
        let count = counter.fetch_add(1, Ordering::Relaxed) as usize;
        
        // Use bit mask for power-of-2 backend counts, modulo for others
        if backend_count.is_power_of_two() {
            count & (backend_count - 1)
        } else {
            count % backend_count
        }
    }
}



/// Pending backend connection data for async connect operations
#[derive(Debug)]
struct PendingConnect {
    backend_addr: SocketAddr,
    // Store owned copy of request data to prevent use-after-free
    // The buffer from io_uring is returned to kernel immediately when BorrowedBuffer drops,
    // so we MUST copy the data for async operations
    request_data: Vec<u8>,
    // Store sockaddr to ensure it lives until connect completes
    sockaddr_storage: Box<SockaddrStorage>,
}

/// Storage for sockaddr that persists until connect completes
#[repr(C)]
union SockaddrStorage {
    v4: libc::sockaddr_in,
    v6: libc::sockaddr_in6,
}

impl std::fmt::Debug for SockaddrStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Safe to read v4.sin_family as it's at the same offset in both structs
        let family = unsafe { self.v4.sin_family };
        if family == libc::AF_INET as libc::sa_family_t {
            write!(f, "SockaddrStorage::V4")
        } else {
            write!(f, "SockaddrStorage::V6")
        }
    }
}

/// Thread-local backend connection pool for individual workers
/// HikariCP-style pool: lazy creation, fast borrow/return, automatic cleanup
#[derive(Debug)]
struct WorkerBackendPool {
    /// Simple pools per backend - Vec is fine for thread-local use
    pools: FnvHashMap<SocketAddr, HikariPool>,
    /// TCP configuration for new connections
    tcp_config: TcpKeepAliveConfig,
    /// Maximum connections per backend (configurable)
    max_connections_per_backend: usize,
    /// Pending async connect operations
    pending_connects: FnvHashMap<RawFd, PendingConnect>,
    /// Track which connections are being monitored (fd -> backend_addr)
    monitored_connections: FnvHashMap<RawFd, SocketAddr>,
}

/// HikariCP-style connection pool for a single backend
/// Zero overhead for healthy connections, automatic cleanup of dead ones
#[derive(Debug)]
struct HikariPool {
    /// Available connections - simple Vec is fine for thread-local use
    available: Vec<RawFd>,
    /// Currently active (borrowed) connections
    active_count: usize,
    /// Total connections (active + available)
    total_count: usize,
    /// Maximum pool size
    max_size: usize,
}

impl HikariPool {
    /// Create new pool for a backend
    fn new(max_size: usize) -> Self {
        Self {
            available: Vec::with_capacity(max_size),
            active_count: 0,
            total_count: 0,
            max_size,
        }
    }
    
    /// Borrow connection from pool - zero checks, just pop
    fn borrow(&mut self) -> Option<RawFd> {
        if let Some(fd) = self.available.pop() {
            self.active_count += 1;
            Some(fd)
        } else {
            None
        }
    }
    
    /// Return healthy connection to pool
    fn return_healthy(&mut self, fd: RawFd) {
        if self.active_count > 0 {
            self.active_count -= 1;
        }
        
        // Only pool if we have space
        if self.available.len() < self.max_size {
            self.available.push(fd);
        } else {
            // Pool full - close excess connection
            unsafe { libc::close(fd); }
            self.total_count -= 1;
        }
    }
    
    /// Discard failed connection
    fn discard_failed(&mut self, fd: RawFd) {
        if self.active_count > 0 {
            self.active_count -= 1;
        }
        if self.total_count > 0 {
            self.total_count -= 1;
        }
        unsafe { libc::close(fd); }
    }
    
    /// Remove specific fd from available pool (when monitoring detects closure)
    fn remove_connection(&mut self, fd: RawFd) -> bool {
        if let Some(pos) = self.available.iter().position(|&x| x == fd) {
            self.available.remove(pos);
            if self.total_count > 0 {
                self.total_count -= 1;
            }
            unsafe { libc::close(fd); }
            true
        } else {
            false
        }
    }
    
    /// Check if we can create a new connection
    fn can_create_new(&self) -> bool {
        self.total_count < self.max_size
    }
    
    /// Record that a new connection was created
    fn record_new_connection(&mut self) {
        self.total_count += 1;
        self.active_count += 1;
    }
}

impl WorkerBackendPool {
    /// Create new thread-local backend pool
    fn new(tcp_config: TcpKeepAliveConfig, max_connections_per_backend: usize) -> Result<Self> {
        Ok(Self {
            pools: FnvHashMap::default(),
            tcp_config,
            max_connections_per_backend,
            pending_connects: FnvHashMap::default(),
            monitored_connections: FnvHashMap::default(),
        })
    }
    
    /// Get connection from pool - HikariCP style
    /// Returns (fd, needs_cancel) where needs_cancel indicates if monitoring should be cancelled
    fn get_connection(&mut self, backend_addr: SocketAddr) -> Option<(RawFd, bool)> {
        // Get or create pool for this backend
        let pool = self.pools.entry(backend_addr)
            .or_insert_with(|| HikariPool::new(self.max_connections_per_backend));
        
        // Try to borrow existing connection
        if let Some(fd) = pool.borrow() {
            debug!("Reusing connection fd={} for backend {}", fd, backend_addr);
            // Connection was in pool, so it has monitoring that needs cancellation
            return Some((fd, true));
        }
        
        // No connections available
        None
    }
    
    /// Check if we can create a new connection for this backend
    fn can_create_new(&mut self, backend_addr: SocketAddr) -> bool {
        let pool = self.pools.entry(backend_addr)
            .or_insert_with(|| HikariPool::new(self.max_connections_per_backend));
        pool.can_create_new()
    }
    
    /// Record that a new connection was created
    fn record_new_connection(&mut self, backend_addr: SocketAddr) {
        if let Some(pool) = self.pools.get_mut(&backend_addr) {
            pool.record_new_connection();
        }
    }
    
    /// Return connection to pool - only if healthy
    fn return_connection(&mut self, backend_addr: SocketAddr, fd: RawFd) {
        if let Some(pool) = self.pools.get_mut(&backend_addr) {
            pool.return_healthy(fd);
            debug!("Returned healthy connection fd={} to pool for {}", fd, backend_addr);
        } else {
            // No pool exists - create one and add connection
            let mut pool = HikariPool::new(self.max_connections_per_backend);
            pool.total_count = 1;
            pool.available.push(fd);
            self.pools.insert(backend_addr, pool);
        }
    }
    
    /// Discard failed connection
    fn discard_connection(&mut self, backend_addr: SocketAddr, fd: RawFd) {
        if let Some(pool) = self.pools.get_mut(&backend_addr) {
            pool.discard_failed(fd);
            debug!("Discarded failed connection fd={} for {}", fd, backend_addr);
        } else {
            // No pool - just close
            unsafe { libc::close(fd); }
        }
        // Remove from monitoring if present
        self.monitored_connections.remove(&fd);
    }
    
    /// Remove connection detected as closed by monitoring
    fn remove_monitored_connection(&mut self, fd: RawFd) -> Option<SocketAddr> {
        // Find and remove from monitored list
        if let Some(backend_addr) = self.monitored_connections.remove(&fd) {
            // Remove from pool
            if let Some(pool) = self.pools.get_mut(&backend_addr) {
                pool.remove_connection(fd);
                debug!("Removed monitored connection fd={} for {} (backend closed)", fd, backend_addr);
            }
            Some(backend_addr)
        } else {
            None
        }
    }
    
    /// Cancel pool monitoring for a connection being borrowed
    fn cancel_pool_monitor(&mut self, ring: &mut IoUring, fd: RawFd, backend_addr: SocketAddr) -> Result<()> {
        // Check if this connection was being monitored
        if self.monitored_connections.remove(&fd).is_some() {
            // Submit PollRemove to cancel monitoring
            let user_data = encode_pool_monitor(fd, backend_addr.port());
            
            let cancel_op = opcode::PollRemove::new(user_data)
                .build()
                .user_data(0); // User data for cancel operation itself
            
            unsafe {
                // Try to push without immediate submit - let event loop handle batching
                let _ = ring.submission().push(&cancel_op);
                // Ignore queue full - monitoring cancellation is best-effort
            }
            
            debug!("Cancelled pool monitoring for fd={}", fd);
        }
        
        Ok(())
    }
}

impl UringWorker {
    /// Submit async backend connection with io_uring Connect operation
    fn submit_backend_connect(&mut self, backend_addr: SocketAddr, client_fd: RawFd, buffer_ptr: *const u8, buffer_length: usize) -> Result<()> {
        use libc::{socket, AF_INET, AF_INET6, SOCK_STREAM, SOCK_NONBLOCK};
        
        info!("ASYNC_CONNECT: Starting async backend connection: client_fd={}, backend_addr={}, request_buffer_len={}", 
              client_fd, backend_addr, buffer_length);
        
        // Create non-blocking socket
        let domain = match backend_addr {
            SocketAddr::V4(_) => AF_INET,
            SocketAddr::V6(_) => AF_INET6,
        };
        
        let sock_fd = unsafe { socket(domain, SOCK_STREAM | SOCK_NONBLOCK, 0) };
        if sock_fd < 0 {
            error!("ASYNC_CONNECT: Failed to create socket for backend connection");
            // Send error response to client
            let (error_ptr, error_len) = self.get_error_response_for_sendzc(503);
            let error_user_data = encode_connection_data(client_fd, 0, 0);
            self.submit_sendzc(client_fd, error_ptr, error_len, error_user_data)?;
            return Ok(());
        }
        
        info!("ASYNC_CONNECT: Created socket fd={} for async connect to {}", sock_fd, backend_addr);
        
        // Configure TCP keep-alive before connecting
        if self.worker_backend_pool.tcp_config.enabled {
            // Best effort - don't fail if keepalive setup fails
            let _ = self.configure_tcp_keepalive(sock_fd);
        }
        
        // Create and store sockaddr with proper lifetime
        let (sockaddr_storage, sockaddr_len) = match backend_addr {
            SocketAddr::V4(addr_v4) => {
                let mut storage = Box::new(SockaddrStorage {
                    v4: unsafe { std::mem::zeroed() },
                });
                unsafe {
                    storage.v4.sin_family = libc::AF_INET as libc::sa_family_t;
                    storage.v4.sin_port = addr_v4.port().to_be();
                    // IPv4 address: octets are in network order, from_ne_bytes for correct endianness
                    storage.v4.sin_addr.s_addr = u32::from_ne_bytes(addr_v4.ip().octets());
                }
                (storage, std::mem::size_of::<libc::sockaddr_in>() as u32)
            },
            SocketAddr::V6(addr_v6) => {
                let mut storage = Box::new(SockaddrStorage {
                    v6: unsafe { std::mem::zeroed() },
                });
                unsafe {
                    storage.v6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
                    storage.v6.sin6_port = addr_v6.port().to_be();
                    storage.v6.sin6_addr.s6_addr = addr_v6.ip().octets();
                }
                (storage, std::mem::size_of::<libc::sockaddr_in6>() as u32)
            }
        };
        
        // Store pending connection data with sockaddr
        info!("ASYNC_CONNECT: Storing pending connection: backend_fd={}, client_fd={}", sock_fd, client_fd);
        let sockaddr_ptr = sockaddr_storage.as_ref() as *const _ as *const libc::sockaddr;
        
        // CRITICAL: Copy the request data because the buffer will be returned to kernel
        // when this function returns and the BorrowedBuffer is dropped
        let request_data = unsafe {
            std::slice::from_raw_parts(buffer_ptr, buffer_length).to_vec()
        };
        debug!("ASYNC_CONNECT: Copied {} bytes of request data for async connect", request_data.len());
        
        self.worker_backend_pool.pending_connects.insert(sock_fd, PendingConnect {
            backend_addr,
            request_data,
            sockaddr_storage,
        });
        info!("ASYNC_CONNECT: Current pending connections count: {}", self.worker_backend_pool.pending_connects.len());
        
        // Prepare and submit connect operation
        let connect_user_data = encode_connection_data(client_fd, 0, sock_fd) | BACKEND_CONNECT_FLAG;
        let connect_op = opcode::Connect::new(
            types::Fd(sock_fd),
            sockaddr_ptr,
            sockaddr_len,
        )
        .build()
        .user_data(connect_user_data);
        
        unsafe {
            if self.ring.submission().push(&connect_op).is_err() {
                // Queue full - clean up and return error
                libc::close(sock_fd);
                self.worker_backend_pool.pending_connects.remove(&sock_fd);
                return Err(anyhow!("Submission queue full, cannot submit connect"));
            }
        }
        
        // debug!("Submitted async connect: sock_fd={}, backend_addr={}, client_fd={}", sock_fd, backend_addr, client_fd);
        Ok(())
    }
    
    /// Handle backend connect completion from async io_uring Connect operation
    fn handle_backend_connect_completion(&mut self, user_data: u64, result: i32) -> Result<()> {
        let original_data = user_data & !BACKEND_CONNECT_FLAG;
        let client_fd = extract_client_fd(original_data);
        let backend_fd = extract_backend_fd(original_data) as RawFd;
        
        info!("ASYNC_CONNECT_COMPLETE: Backend connect completion: backend_fd={}, client_fd={}, result={}", 
              backend_fd, client_fd, result);
        info!("ASYNC_CONNECT_COMPLETE: Current pending connections count before removal: {}", 
              self.worker_backend_pool.pending_connects.len());
        
        // Get pending connect data
        let pending = match self.worker_backend_pool.pending_connects.remove(&backend_fd) {
            Some(p) => p,
            None => {
                error!("No pending connect found for fd={}", backend_fd);
                // Close orphaned connection
                unsafe { libc::close(backend_fd); }
                return Ok(());
            }
        };
        
        if result < 0 {
            error!("Backend connection failed to {}: errno={}", pending.backend_addr, -result);
            // Don't add failed connection to pool
            unsafe { libc::close(backend_fd); }
            
            // Send error to client
            let (error_ptr, error_len) = self.get_error_response_for_sendzc(503);
            let error_user_data = encode_connection_data(client_fd, 0, 0);
            self.submit_sendzc(client_fd, error_ptr, error_len, error_user_data)?;
            return Ok(());
        }
        
        // debug!("Backend connection established: fd={} to {}", backend_fd, pending.backend_addr);
        
        // Store connection mapping - connection is now ready for use
        self.backend_fd_to_addr.insert(backend_fd, pending.backend_addr);
        
        // Record new connection in pool
        self.worker_backend_pool.record_new_connection(pending.backend_addr);
        
        // Configure TCP keepalive on the new connection
        if self.worker_backend_pool.tcp_config.enabled {
            if let Err(e) = self.configure_tcp_keepalive(backend_fd) {
                warn!("Failed to configure TCP keepalive: {}", e);
            }
        }
        
        // Now forward the buffered request
        let pool_index = 0;
        let write_user_data = encode_connection_data(0, pool_index, backend_fd);
        let read_user_data = encode_connection_data(client_fd, pool_index, backend_fd);
        
        self.submit_backend_write_read_linked_with_buffer(
            backend_fd,
            pending.request_data.as_ptr() as *mut u8,
            pending.request_data.len(),
            self.backend_buffers.get_buffer_group_id(),
            self.backend_buffers.get_buffer_size(),
            write_user_data,
            read_user_data,
        )?;
        
        Ok(())
    }
    
    
    /// Configure TCP keep-alive options with all settings
    fn configure_tcp_keepalive(&self, fd: RawFd) -> Result<()> {
        unsafe {
            // Enable SO_KEEPALIVE
            let keepalive: libc::c_int = 1;
            if libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE,
                               &keepalive as *const _ as *const libc::c_void,
                               std::mem::size_of::<libc::c_int>() as libc::socklen_t) != 0 {
                return Err(anyhow!("Failed to set SO_KEEPALIVE: {}", std::io::Error::last_os_error()));
            }
            
            // Set TCP_KEEPIDLE (time before first probe)
            let idle_secs = self.worker_backend_pool.tcp_config.idle_time.as_secs() as libc::c_int;
            if libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_KEEPIDLE,
                               &idle_secs as *const _ as *const libc::c_void,
                               std::mem::size_of::<libc::c_int>() as libc::socklen_t) != 0 {
                return Err(anyhow!("Failed to set TCP_KEEPIDLE: {}", std::io::Error::last_os_error()));
            }
            
            // Set TCP_KEEPINTVL (interval between probes)
            let interval_secs = self.worker_backend_pool.tcp_config.probe_interval.as_secs() as libc::c_int;
            if libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_KEEPINTVL,
                               &interval_secs as *const _ as *const libc::c_void,
                               std::mem::size_of::<libc::c_int>() as libc::socklen_t) != 0 {
                return Err(anyhow!("Failed to set TCP_KEEPINTVL: {}", std::io::Error::last_os_error()));
            }
            
            // Set TCP_KEEPCNT (number of failed probes before timeout)
            let probe_count = self.worker_backend_pool.tcp_config.probe_count as libc::c_int;
            if libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_KEEPCNT,
                               &probe_count as *const _ as *const libc::c_void,
                               std::mem::size_of::<libc::c_int>() as libc::socklen_t) != 0 {
                return Err(anyhow!("Failed to set TCP_KEEPCNT: {}", std::io::Error::last_os_error()));
            }
            
            // debug!("Configured TCP keep-alive: idle={}s, interval={}s, probes={}",
            //        idle_secs, interval_secs, probe_count);
        }
        Ok(())
    }
}

impl Drop for WorkerBackendPool {
    fn drop(&mut self) {
        // Close all pooled connections
        for (_backend_addr, pool) in &mut self.pools {
            for &fd in &pool.available {
                debug!("Closing pooled connection fd={} for backend {}", fd, backend_addr);
                unsafe { libc::close(fd); }
            }
        }
        // Close any pending connections
        for (&fd, _) in &self.pending_connects {
            debug!("Closing pending connection fd={}", fd);
            unsafe { libc::close(fd); }
        }
    }
}



// Performance-critical batching constants - optimized for low latency
const BATCH_SUBMIT_THRESHOLD: usize = 16;  // Submit when this many ops are queued (reduced for lower latency)
const BATCH_TIME_THRESHOLD_MICROS: u64 = 50;  // Submit if this much time has passed (reduced from 100μs)
const MIN_BATCH_SIZE: usize = 4;  // Try to batch at least this many ops (reduced from 8)
const QUEUE_PRESSURE_THRESHOLD: usize = 256;  // Submit if queue has this many pending ops (avoid overflow)

/// Raw io_uring based data plane worker with Immutable Processing Pipeline
/// Handles HTTP and TCP proxying with zero-copy operations
/// Memory layout optimized for cache line efficiency: hot fields first (accessed every request)
#[repr(C)]
pub struct UringWorker {
    // === HOT PATH DATA (First Cache Line - 64 bytes) ===
    // Accessed on every request - keep in first cache line for maximum performance
    
    /// Route table pointer - accessed for every HTTP/TCP routing decision
    routes: Arc<AtomicPtr<RouteTable>>,
    /// io_uring instance - used for all I/O operations  
    ring: IoUring,
    
    // === MEDIUM ACCESS DATA (Second Cache Line) ===
    // Accessed during I/O setup and buffer management
    
    /// Client buffer ring for request processing - accessed during client I/O setup
    client_buffers: NativeBufferRing,
    /// Backend buffer ring for response handling - accessed during backend I/O setup
    backend_buffers: NativeBufferRing,
    
    // === COLD PATH DATA (Remaining Cache Lines) ===
    // Accessed only during setup, cleanup, or error handling
    
    /// Worker ID - only used for logging and identification
    worker_id: usize,
    /// Worker-specific configuration for protocol optimization  
    worker_config: WorkerConfig,
    /// Performance configuration - accessed for keep-alive limits and buffer management
    performance_config: PerformanceConfig,
    /// Runtime configuration - only accessed during setup and port-based routing
    config: RuntimeConfig,
    /// Listener sockets - only accessed during setup and new connections
    listeners: FnvHashMap<u16, RawFd>,
    /// Reverse lookup: listener FD to port mapping for server_port resolution
    listener_fd_to_port: FnvHashMap<RawFd, u16>,
    /// Backend FD to address mapping for connection return
    backend_fd_to_addr: FnvHashMap<RawFd, SocketAddr>,
    // All client connection tracking now handled by unified connection_manager
    /// Last time we performed connection pool cleanup
    /// Last time we submitted operations (for completion-driven batching)
    last_submission_time: std::time::Instant,
    /// Pre-allocated completion buffer to eliminate hot-path allocations
    completion_buffer: Vec<(u64, i32, Option<u16>)>,
    /// Thread-local backend connection pool for this worker
    worker_backend_pool: WorkerBackendPool,
    /// Per-worker load balancing state for zero-contention backend selection
    worker_lb_state: WorkerLoadBalancerState,
    /// Shutdown flag for graceful termination - only accessed during shutdown
    shutdown_flag: Arc<AtomicBool>,
    
    // === NATIVE IO_URING EVENTFD INTEGRATION (Single Codepath) ===
    /// Eventfd for io_uring completion notifications - registered with IORING_REGISTER_EVENTFD
    eventfd: RawFd,
    /// Epoll instance for blocking on io_uring eventfd notifications
    epoll_fd: RawFd,
}

impl UringWorker {
    /// Create new UringWorker with modular architecture
    /// Compatible with UringDataPlane interface
    pub fn new(
        worker_config: WorkerConfig,
        routes: Arc<AtomicPtr<RouteTable>>,
        config: RuntimeConfig,
        performance_config: PerformanceConfig,
        tcp_config: TcpKeepAliveConfig,
    ) -> Result<Self> {
        let worker_id = worker_config.id;
        let _primary_protocol = Self::protocol_to_runtime_protocol(&worker_config.protocol)?;
        info!("Creating UringWorker {} with modular architecture for protocol {:?}", worker_id, _primary_protocol);
        
        // Create io_uring instance with larger queue for better batching
        // 4096 entries allows much better batching under load
        let mut ring = IoUring::new(4096)?;
        
        // Initialize dual buffer rings for client/backend separation
        let mut client_buffers = NativeBufferRing::new(
            (worker_id + 1000) as u16,    // client buffer_group_id
            worker_config.buffer_count,   // buffer_count from config
            worker_config.buffer_size as u32, // buffer_size from config
        );
        
        let mut backend_buffers = NativeBufferRing::new(
            (worker_id + 2000) as u16,    // backend buffer_group_id
            worker_config.buffer_count,   // buffer_count from config
            worker_config.buffer_size as u32, // buffer_size from config
        );
        
        // Register both buffer rings with kernel for zero-copy operations
        client_buffers.register_with_kernel(&mut ring)?;
        backend_buffers.register_with_kernel(&mut ring)?;
        info!("Worker {} successfully registered dual buffer rings with kernel", worker_id);
        
        // Initialize thread-local backend pool
        let worker_backend_pool = WorkerBackendPool::new(
            tcp_config,
            32, // Default max connections per backend
        )?;
        info!("Worker {} initialized thread-local backend pool", worker_id);
        
        // Initialize per-worker load balancing state
        let worker_lb_state = WorkerLoadBalancerState::new(worker_id);
        info!("Worker {} initialized per-worker load balancing state with offset {}", worker_id, worker_lb_state.worker_offset);
        
        
        let worker = Self {
            routes,
            ring,
            client_buffers,
            backend_buffers,
            worker_id,
            worker_config,
            performance_config,
            config,
            listeners: FnvHashMap::default(),
            listener_fd_to_port: FnvHashMap::default(),
            backend_fd_to_addr: FnvHashMap::default(),
            // All client connection management now unified in connection_manager
            last_submission_time: std::time::Instant::now(),
            completion_buffer: Vec::with_capacity(128), // Pre-allocate for typical batch sizes
            // Thread-local backend connection pool
            worker_backend_pool,
            // Per-worker load balancing state
            worker_lb_state,
            // Initialize shutdown flag
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            
            // === INITIALIZE EVENT-DRIVEN POLLING (Single Codepath) ===
            eventfd: Self::create_eventfd()?,
            epoll_fd: Self::create_epoll_instance()?,
        };
        
        // Connection manager will be initialized without raw pointers
        // It will receive references when methods are called instead
        
        // NATIVE IO_URING EVENTFD REGISTRATION: Single superior architecture
        // Register eventfd with io_uring for automatic completion notifications
        worker.ring.submitter().register_eventfd(worker.eventfd)?;
        
        // Register eventfd with epoll for blocking on io_uring notifications
        Self::register_eventfd_with_epoll(worker.epoll_fd, worker.eventfd)?;
        
        info!("Worker {} initialized native io_uring eventfd integration (eventfd: {}, epoll: {})", 
              worker_id, worker.eventfd, worker.epoll_fd);
        
        Ok(worker)
    }
    
    // === EVENT-DRIVEN POLLING INFRASTRUCTURE ===
    
    /// Create eventfd for completion notifications
    fn create_eventfd() -> Result<RawFd> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(anyhow!("Failed to create eventfd for worker polling"));
        }
        Ok(fd)
    }
    
    /// Create epoll instance for blocking
    fn create_epoll_instance() -> Result<RawFd> {
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd < 0 {
            return Err(anyhow!("Failed to create epoll instance for worker"));
        }
        Ok(fd)
    }
    
    /// Register eventfd with epoll for monitoring
    fn register_eventfd_with_epoll(epoll_fd: RawFd, eventfd: RawFd) -> Result<()> {
        let mut event = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: eventfd as u64,
        };
        
        let result = unsafe { 
            libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, eventfd, &mut event) 
        };
        if result < 0 {
            return Err(anyhow!("Failed to register eventfd with epoll"));
        }
        Ok(())
    }
    
    // === Queue Management Methods ===
    
    /// Get available space in submission queue
    #[inline(always)]
    fn sq_space_left(&mut self) -> usize {
        // The submission queue capacity minus current pending operations
        // We can't use submission() on immutable reference, so use conservative estimate
        // Most io_uring implementations have a fixed SQ size
        // We configured 4096, use that as capacity
        4096_usize.saturating_sub(self.ring.submission().len())
    }
    
    /// Check if we have enough queue space for N operations
    #[inline(always)]
    fn has_queue_space(&mut self, required: usize) -> bool {
        self.sq_space_left() >= required
    }
    
    /// Check if queue is under pressure (>75% full)
    #[inline(always)]
    fn is_queue_under_pressure(&mut self) -> bool {
        self.sq_space_left() < 1024 // Less than 25% space left (1024 of 4096)
    }
    
    /// Main event loop with Immutable Processing Pipeline
    /// Simplified to unified request processing pattern
    pub fn run(&mut self) -> Result<()> {
        info!("Starting UringWorker {} with Immutable Processing Pipeline", self.worker_id);
        
        loop {
            // Check for shutdown signal
            if self.shutdown_flag.load(Ordering::Acquire) {
                info!("Worker {} received shutdown signal, exiting gracefully", self.worker_id);
                break Ok(());
            }
            
            // SINGLE CODEPATH: Event-driven work processing
            self.wait_for_work_and_process()?;
        }
    }
    
    /// SINGLE CODEPATH: Native io_uring eventfd-driven work processing with smart batching
    /// Uses IORING_REGISTER_EVENTFD for automatic completion notifications
    fn wait_for_work_and_process(&mut self) -> Result<()> {
        // Proactive queue management using our new helper methods
        let pending_ops = self.ring.submission().len();
        
        // Check queue pressure using our helper
        if self.is_queue_under_pressure() || pending_ops >= QUEUE_PRESSURE_THRESHOLD {
            // Queue is under pressure, submit immediately to free space
            self.ring.submit()?;
            self.last_submission_time = std::time::Instant::now();
        } else if pending_ops > 0 {
            let time_since_submit = self.last_submission_time.elapsed();
            
            // Submit if we have enough operations OR enough time has passed
            if pending_ops >= BATCH_SUBMIT_THRESHOLD || 
               (pending_ops >= MIN_BATCH_SIZE && time_since_submit.as_micros() as u64 > BATCH_TIME_THRESHOLD_MICROS) {
                self.ring.submit()?;
                self.last_submission_time = std::time::Instant::now();
            }
        }
        
        // Now check for completions
        let has_completions = {
            let completion = self.ring.completion();
            completion.len() > 0
        };
        
        // If no completions and we have pending work, decide whether to block or submit
        if !has_completions {
            let pending_ops = self.ring.submission().len();
            
            if pending_ops == 0 {
                // No work pending - safe to block
                self.wait_for_completion_notification()?;
            } else {
                // We have pending submissions - use submit_and_wait for efficiency
                // This submits pending ops and waits for at least 1 completion
                self.ring.submit_and_wait(1)?;
                self.last_submission_time = std::time::Instant::now();
            }
        }
        
        // Process all available completions
        self.completion_buffer.clear();
        
        {
            let mut completion = self.ring.completion();
            completion.sync();
            
            for cqe in completion {
                let user_data = cqe.user_data();
                let result = cqe.result();
                let flags = cqe.flags();
                
                // Extract buffer ID from CQE flags if present
                let kernel_provided_buffer_id = if flags != 0 {
                    Some((flags >> 16) as u16)
                } else {
                    None
                };
                
                self.completion_buffer.push((user_data, result, kernel_provided_buffer_id));
            }
        } // completion borrow ends here
        
        // Process all completions in batch
        for i in 0..self.completion_buffer.len() {
            let (user_data, result, kernel_provided_buffer_id) = self.completion_buffer[i];
            self.dispatch_completion(user_data, result, kernel_provided_buffer_id)?;
        }
        
        // Post-completion batching: submit if we've accumulated enough operations
        let pending_ops = self.ring.submission().len();
        
        // Check queue pressure first
        if pending_ops >= QUEUE_PRESSURE_THRESHOLD {
            self.ring.submit()?;
            self.last_submission_time = std::time::Instant::now();
        } else if pending_ops >= BATCH_SUBMIT_THRESHOLD {
            self.ring.submit()?;
            self.last_submission_time = std::time::Instant::now();
        } else if pending_ops >= MIN_BATCH_SIZE {
            // Check time threshold for smaller batches
            let time_since_submit = self.last_submission_time.elapsed();
            if time_since_submit.as_micros() as u64 > BATCH_TIME_THRESHOLD_MICROS {
                self.ring.submit()?;
                self.last_submission_time = std::time::Instant::now();
            }
        }
        
        Ok(())
    }
    
    /// Consolidated completion dispatch with single codepath
    /// Routes completions based on ownership and operation type
    fn dispatch_completion(
        &mut self,
        user_data: u64,
        result: i32,
        kernel_provided_buffer_id: Option<u16>,
    ) -> Result<()> {
        // SINGLE CODEPATH: Direct dispatch based on operation flags
        // Use bit flags for O(1) operation identification
        
        // Route directly based on operation type - single codepath
        // Check SENDZC operations first
        if io_operations::BufferRingOperations::is_sendzc_operation(user_data) {
            // debug!("DISPATCH: Routing to SENDZC handler: user_data={:016x}", user_data);
            return self.handle_sendzc_completion(user_data, result);
        }
        
        // Check for pool monitor completion (backend closed connection)
        if is_pool_monitor(user_data) {
            return self.handle_pool_monitor_completion(user_data, result);
        }
        
        // Check for backend connect completion
        if (user_data & BACKEND_CONNECT_FLAG) != 0 {
            // info!("DISPATCH: Routing to backend connect handler: user_data={:016x}, result={}", user_data, result);
            return self.handle_backend_connect_completion(user_data, result);
        }
        
        if is_accept_operation(user_data) {
            // debug!("DISPATCH: Routing to accept handler: user_data={:016x}", user_data);
            return self.handle_accept_completion_specialized(user_data, result);
        }
        
        // Discriminate client vs backend reads using CLIENT_OP_FLAG
        // Backend reads don't have CLIENT_OP_FLAG set
        if kernel_provided_buffer_id.is_some() && (user_data & CLIENT_OP_FLAG) == 0 {
            // Backend read completion (has buffer_id but no CLIENT_OP_FLAG)
            // debug!("BACKEND_READ_COMPLETION: backend_fd={}, routing to backend handler", extract_backend_fd(user_data));
            return self.handle_backend_read_completion(user_data, result, kernel_provided_buffer_id);
        }
        
        // Route timeouts directly based on bit 60
        if (user_data & TIMEOUT_FLAG) != 0 {
            return self.handle_timeout_completion(user_data, result);
        }
        
        // Client connection processing
        
        // Extract connection data directly from user_data
        let client_fd = extract_client_fd(user_data);
        
        // Handle close operations - need a different way to detect close operations
        // Could use a specific bit flag for close operations instead of ownership
        // For now, skip this check - close completions won't have buffer_id
        if result < 0 && kernel_provided_buffer_id.is_none() {
            debug!("Operation completed with error: client_fd={}, result={}", client_fd, result);
            // No HashMap cleanup needed - data is in user_data
            return Ok(());
        }
        
        // Streamlined error handling with ownership
        if result < 0 && result != -2 {
            // Real errors (excluding ENOENT which indicates disconnected client with data)
            warn!("Connection operation failed: client_fd={}, error={}", client_fd, result);
            debug!("Connection error cleanup: client_fd={}, error={}", client_fd, result);
            let cleanup_user_data = user_data;
            let _ = self.submit_close(client_fd, cleanup_user_data);
            return Ok(());
        }
        
        // Client operation processing
        if let Some(buffer_id) = kernel_provided_buffer_id {
            // debug!("DISPATCH: Client read completion: client_fd={}, buffer_id={}, bytes_read={}", client_fd, buffer_id, result);
            // Cancel keep-alive timeout if needed
            if result > 0 && is_keepalive_enabled(user_data) {
                let timeout_user_data = user_data | TIMEOUT_FLAG;  // Set timeout bit
                let _ = self.submit_timeout_remove(timeout_user_data, user_data);
                // debug!("Cancelled keep-alive timeout: client_fd={}, timeout_user_data={:016x}", 
                //        client_fd, timeout_user_data);
            }
            
            let bytes_available = if result == -2 { (-result) as usize } else { result as usize };
            
            // Client processing: extract server_port from user_data and create context
            let server_port = extract_server_port(user_data);
            let context = request_processor::ProcessingContext {
                client_fd,
                server_port,
            };
            
            // Process request directly with protocol dispatch
            match self.get_worker_protocol() {
                Protocol::HTTP => {
                    return self.process_http_request_with_pipeline(context, buffer_id, bytes_available, user_data);
                }
                Protocol::TCP => {
                    return self.process_tcp_data_with_pipeline(context, buffer_id, bytes_available, user_data);
                }
            }
        }
        
        Ok(())
    }
    
    /// Request worker shutdown
    /// This sets the shutdown flag which will be checked in the next iteration of the event loop
    #[allow(dead_code)]
    pub fn request_shutdown(&self) {
        info!("Requesting shutdown for worker {}", self.worker_id);
        self.shutdown_flag.store(true, Ordering::Release);
    }
    
    /// Get clone of shutdown flag for external signaling
    /// Used by UringDataPlane to signal shutdown to workers
    #[allow(dead_code)]
    pub fn get_shutdown_flag(&self) -> Arc<AtomicBool> {
        self.shutdown_flag.clone()
    }
    
    /// Replace internal shutdown flag with external one for coordinated shutdown
    /// Used by UringDataPlane to provide centralized shutdown control
    pub fn set_external_shutdown_flag(&mut self, external_flag: Arc<AtomicBool>) {
        self.shutdown_flag = external_flag;
    }
    
    /// SINGLE CODEPATH: Universal connection cleanup with ownership protection
    /// Replaces all 5 scattered cleanup locations with race-free single cleanup
    fn cleanup_connection_with_ownership(&mut self, user_data: u64) -> Result<()> {
        let client_fd = extract_client_fd(user_data);
        let backend_fd = extract_backend_fd(user_data);
        
        // Only cleanup if we currently own the connection (prevents races)
        if !should_cleanup_connection(user_data) {
            // Connection already being cleaned up by another operation
            return Ok(());
        }
        
        // No ownership transfer needed
        let cleanup_user_data = user_data;
        
        // SINGLE CODEPATH: Backend connections returned only via SENDZC completion
        // Don't return backend connection here to prevent double-return race
        // The backend connection will be returned by handle_sendzc_completion()
        if backend_fd > 0 {
            debug!("Backend connection fd={} cleanup deferred to SENDZC completion", backend_fd);
        }
        
        // No HashMap cleanup needed - data is in user_data
        self.submit_close(client_fd, cleanup_user_data)?;
        
        Ok(())
    }
    
    /// Handle backend read completion - forward response to client
    /// Backend responses are forwarded to client instead of being processed as HTTP requests
    fn handle_backend_read_completion(
        &mut self, 
        backend_user_data: u64, 
        result: i32, 
        kernel_provided_buffer_id: Option<u16>
    ) -> Result<()> {
        debug!("BACKEND_READ_START: user_data={:016x}, result={}, buffer_id={:?}", 
               backend_user_data, result, kernel_provided_buffer_id);
        // No ownership transfer needed
        
        // Extract all info directly from user_data
        let _client_fd = extract_client_fd(backend_user_data);
        let _pool_index = extract_pool_index(backend_user_data);
        let backend_fd = extract_backend_fd(backend_user_data);
        let mut client_user_data = backend_user_data; // Connection data preserves client context
        
        // DEBUG: Track backend read execution
        debug!("BACKEND_READ_COMPLETION: backend_fd={}, result={}, buffer_id={:?}, pool_index={}", 
               backend_fd, result, kernel_provided_buffer_id, pool_index);
        
        // CRITICAL FIX: DO NOT return backend connection to pool here!
        // The response hasn't been sent to the client yet - it's only queued.
        // We must wait until AFTER the sendzc completes to return the backend to the pool.
        // Otherwise we have a race condition where another request can use the backend
        // while the previous response is still being sent.
        
        // Store backend pool decision in user_data for sendzc completion handler
        if backend_fd > 0 {
            if let Some(&_backend_addr) = self.backend_fd_to_addr.get(&backend_fd) {
                if result > 0 {
                    // Successful read - check if backend wants to keep connection alive
                    let should_pool = if let Some(buffer_id) = kernel_provided_buffer_id {
                        if let Some(buffer) = self.backend_buffers.get_buffer(buffer_id, result as usize) {
                            unified_http_parser::UnifiedHttpParser::should_pool_backend_connection(&buffer)
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    
                    if should_pool {
                        debug!("Backend read successful: fd={}, bytes={}, will pool AFTER response sent", backend_fd, result);
                        // Mark connection for pooling in user_data - will be handled after sendzc
                        client_user_data = set_backend_pooling_bit(client_user_data);
                    } else {
                        debug!("Backend read successful but will close: fd={}, bytes={} (Connection: close)", backend_fd, result);
                        // Will be discarded after sendzc completes
                    }
                } else if result == 0 {
                    debug!("Backend connection closed gracefully (bytes=0) - will discard fd={}", backend_fd);
                    // Mark for discard after sendzc
                } else {
                    debug!("Backend connection error: fd={}, result={}, will discard", backend_fd, result);
                    // Mark for discard after sendzc  
                }
                // Keep mapping alive until sendzc completes
            }
        }
        
        // Handle backend I/O errors with proper cleanup
        if result < 0 {
            debug!("Backend read failed: client_user_data={:016x}, error={}", client_user_data, result);
            
            // Clean up failed backend connection
            if backend_fd > 0 {
                if let Some(&backend_addr) = self.backend_fd_to_addr.get(&backend_fd) {
                    debug!("Removing failed backend connection fd={} for {}", backend_fd, backend_addr);
                    // Discard failed connection properly
                    self.worker_backend_pool.discard_connection(backend_addr, backend_fd);
                    self.backend_fd_to_addr.remove(&backend_fd);
                }
            }
            
            // Send error response to client and close connection
            let client_fd = extract_client_fd(backend_user_data);
            debug!("Backend read error cleanup for client_fd={}", client_fd);
            let (buffer_ptr, response_len) = self.get_error_response_for_sendzc(502); // Bad Gateway
            let error_user_data = backend_user_data; // No ownership transfer needed
            
            self.submit_sendzc(client_fd, buffer_ptr, response_len, error_user_data)?;
            
            // Clean up client connection
            self.cleanup_connection_with_ownership(backend_user_data)?;
            
            return Ok(());
        }
        
        // Forward backend response to client if we have data
        if let Some(buffer_id) = kernel_provided_buffer_id {
            if result > 0 {
                self.forward_backend_response_simple(client_user_data, buffer_id, result as usize)?;
            } else {
                debug!("Backend read completed with 0 bytes - connection was stale");
                // Backend connection was dead - need to retry with a new connection
                let client_fd = extract_client_fd(backend_user_data);
                
                debug!("Stale backend connection detected for client_fd={}, will retry", client_fd);
                
                // For now, send 503 to client - in future could retry automatically
                // TODO: Implement automatic retry with new connection
                let (error_ptr, error_len) = self.get_error_response_for_sendzc(503);
                let error_user_data = backend_user_data;
                self.submit_sendzc(client_fd, error_ptr, error_len, error_user_data)?;
            }
        }
        
        Ok(())
    }
    
    /// Forward backend response to client connection with complete connection reset
    /// This method handles the complete backend→client forwarding cycle and prepares for next request
    fn forward_backend_response_simple(
        &mut self, 
        client_user_data: u64, 
        buffer_id: u16, 
        bytes_read: usize
    ) -> Result<()> {
        debug!("Forwarding backend response to client: client_user_data={:016x}, bytes={}", 
               client_user_data, bytes_read);
               
        // DEBUG: Log backend response using stored buffer pointer
        if let Some(buffer) = self.backend_buffers.get_buffer(buffer_id, bytes_read) {
            let _response_slice = &buffer[..bytes_read.min(150)];
            // debug!("BACKEND_RESPONSE_DEBUG: received {} bytes: {:?}", 
            //        bytes_read, String::from_utf8_lossy(_response_slice));
        }
        
        // Always forward backend responses when available
        // Zero-copy forwarding with guaranteed ordering via io_uring links
        
        // Extract all data directly from connection user_data
        let client_fd = extract_client_fd(client_user_data);
        
        
        // Extract buffer pointer in separate scope to avoid borrow conflicts
        let buffer_ptr_result = {
            if let Some(buffer) = self.backend_buffers.get_buffer(buffer_id, bytes_read) {
                Ok(buffer.as_ptr())
            } else {
                Err(())
            }
        };
        
        // Handle buffer acquisition result after scope ends
        let buffer_ptr = match buffer_ptr_result {
            Ok(ptr) => ptr,
            Err(()) => {
                error!("Failed to get buffer for response forwarding: buffer_id={}", buffer_id);
                return self.cleanup_connection_with_ownership(client_user_data);
            }
        };
        
        // Now we can call mutable methods safely
        self.forward_response_to_client_with_buffer(
            client_fd, 
            extract_backend_fd(client_user_data),
            buffer_ptr,
            bytes_read,
            client_user_data,
            buffer_id,
        )?;
        
        // SINGLE CODEPATH: Keep-alive decision deferred to write completion
        // Write completion handler (handle_sendzc_completion) will:
        // 1. Check keepalive bit to determine if connection should continue
        // 2. Submit next read if keep-alive is enabled
        // 3. Close connection if keep-alive is disabled
        // This ensures proper ordering - next read only happens AFTER response is sent
        
        // Note: Connection cleanup for close-after-write is handled by write completion handler
        // This eliminates race condition where connection is closed before write completes
        
        Ok(())
    }
    
    /// Handle pool monitor completion - backend closed the connection
    fn handle_pool_monitor_completion(&mut self, user_data: u64, _result: i32) -> Result<()> {
        let fd = extract_monitored_fd(user_data);
        let _port = extract_monitored_port(user_data);
        
        debug!("Pool monitor triggered: fd={}, port={}, result={}", fd, port, result);
        
        // Remove from pool and close
        if let Some(_backend_addr) = self.worker_backend_pool.remove_monitored_connection(fd) {
            info!("Backend {} closed connection fd={} while in pool", backend_addr, fd);
        } else {
            // Connection was already removed or borrowed
            debug!("Pool monitor for fd={} completed but connection not in pool", fd);
        }
        
        Ok(())
    }
    
    /// Handle SENDZC completion with direct buffer_id decoding  
    /// Simplified approach eliminates complex slot management
    fn handle_sendzc_completion(&mut self, user_data: u64, _result: i32) -> Result<()> {
        // No ownership transfer needed
        
        let original_user_data = io_operations::BufferRingOperations::decode_original_from_sendzc(user_data);
        
        // Extract client_fd for keep-alive handling
        let client_fd = extract_client_fd(original_user_data);
        
        // NOW we can safely handle the backend connection since the response has been sent
        let backend_fd = extract_backend_fd(original_user_data);
        if backend_fd > 0 {
            if let Some(&backend_addr) = self.backend_fd_to_addr.get(&backend_fd) {
                // Check if connection should be pooled (bit was set during backend read)
                if is_backend_pooling_enabled(original_user_data) {
                    debug!("SENDZC complete, returning backend to pool: fd={}, addr={}", backend_fd, backend_addr);
                    // Submit monitoring before returning to pool
                    self.submit_pool_monitor(backend_fd, backend_addr)?;
                    self.worker_backend_pool.return_connection(backend_addr, backend_fd);
                } else {
                    debug!("SENDZC complete, discarding backend: fd={}, addr={}", backend_fd, backend_addr);
                    self.worker_backend_pool.discard_connection(backend_addr, backend_fd);
                }
                // Clean up mapping
                self.backend_fd_to_addr.remove(&backend_fd);
            }
        }
        
        // NOW handle keep-alive after response has been sent
        if client_fd > 0 && should_continue_connection(original_user_data) {
            // Keep-alive enabled: submit next client read for next request
            let buffer_size = self.client_buffers.get_buffer_size();
            let buffer_group = self.client_buffers.get_buffer_group_id();
            
            // Mark as client operation for completion routing
            let client_read_user_data = original_user_data | CLIENT_OP_FLAG;
            self.submit_read_with_buffers(
                client_fd,
                buffer_group,
                buffer_size,
                client_read_user_data,
            )?;
            
            // Schedule keep-alive timeout using bit 60 encoding
            let timeout_user_data = original_user_data | TIMEOUT_FLAG;  // Set timeout bit
            self.submit_timeout(
                self.performance_config.keep_alive_timeout,
                timeout_user_data,
            )?;
            
            debug!("Keep-alive: next read queued AFTER response sent - client_fd={}", client_fd);
        } else if client_fd > 0 {
            // Connection should close - cleanup
            debug!("Closing connection after response sent - client_fd={}", client_fd);
            self.cleanup_connection_with_ownership(original_user_data)?;
        }
        
        // Buffer can now be reused by kernel - no complex tracking needed
        // Zero-copy safety provided by kernel buffer ring
        
        Ok(())
    }
    
    
    /// Handle timeout completion - handles both regular timeouts and timeout removals
    /// Since both use identical encoding and are fire-and-forget, single handler is optimal
    fn handle_timeout_completion(&mut self, user_data: u64, result: i32) -> Result<()> {
        // No ownership transfer needed
        
        debug!("Timeout completion (regular or removal): user_data={}, result={}", user_data, result);
        
        // Timeout operations are fire-and-forget - just handle common error cases gracefully
        match result {
            // Success cases - timeout expired, close idle connection
            0 => {
                debug!("Keep-alive timeout expired: timeout_user_data={}", user_data);
                
                // Decode original connection user_data from timeout user_data
                let original_user_data = user_data & !TIMEOUT_FLAG;  // Clear timeout bit
                
                // Timeout cleanup using user_data extraction
                let _client_fd = extract_client_fd(original_user_data);
                
                debug!("Keep-alive timeout cleanup: client_fd={}", client_fd);
                
                // Use ownership-based cleanup for timeout
                self.cleanup_connection_with_ownership(original_user_data)?;
                debug!("Submitted close for idle client: client_fd={}", client_fd);
            }
            // Common timeout removal errors (harmless)
            -2 => {
                debug!("Timeout not found (ENOENT) - may have expired naturally: user_data={}", user_data);
            }
            -125 => {
                debug!("Timeout already cancelled (ECANCELED) - io_uring cancelled it: user_data={}", user_data);
            }
            // Potential issues worth noting
            -22 => {
                debug!("Invalid timeout_user_data (EINVAL) - possible bug: user_data={}", user_data);
            }
            // Other errors
            _ => {
                debug!("Timeout operation failed: user_data={}, result={} (errno: {})", 
                       user_data, result, result);
            }
        }
        
        // No further action needed - all timeout operations are fire-and-forget
        // Direct connection management handles actual timeout logic
        Ok(())
    }
    
    /// Ultra-high-performance specialized accept completion handler
    /// Uses worker's pre-assigned protocol - zero syscalls, zero lookups, zero detection
    fn handle_accept_completion_specialized(
        &mut self,
        user_data: u64,
        result: i32,
    ) -> Result<()> {
        // No ownership transfer needed
        
        if result < 0 {
            error!("Accept failed: user_data={:016x}, error={}", user_data, result);

            // RESILIENCE: Resubmit accept even on failure to maintain connection availability
            let listener_fd = extract_client_fd(user_data);
            let _ = self.submit_accept_with_encoding(listener_fd); // Ignore errors to prevent cascade failures

            return Ok(());
        }
        
        let client_fd = result;
        // debug!("Accept completed: client_fd={}", client_fd);
        
        // CRITICAL: Resubmit accept operation immediately for parallel processing
        let listener_fd = extract_client_fd(user_data); // Extract listener_fd from original user_data
        self.submit_accept_with_encoding(listener_fd)?;
        
        // Start client processing based on worker's protocol
        match self.get_worker_protocol() {
            Protocol::HTTP => {
                // debug!("Starting HTTP processing for client_fd={}", client_fd);
                self.start_http_processing_direct(client_fd, listener_fd)
            }
            Protocol::TCP => {
                // debug!("Starting TCP processing for client_fd={}", client_fd);
                self.start_tcp_processing_direct(client_fd, listener_fd)
            }
        }
    }
    
    
    /// Direct HTTP processing for specialized workers
    /// Eliminates protocol encoding since worker is HTTP-specialized
    fn start_http_processing_direct(&mut self, client_fd: RawFd, listener_fd: RawFd) -> Result<()> {
        // Get server port from listener configuration
        let server_port = self.get_listener_port(listener_fd)
            .ok_or_else(|| anyhow!("Unknown listener FD: {}", listener_fd))?;
        
        // Encode server port directly in user_data - no HashMap needed
        let encoded_user_data = encode_connection_data_with_port(client_fd, 0, 0, server_port);
        
        debug!("HTTP connection: client_fd={}, keepalive={}", 
               client_fd, is_keepalive_enabled(encoded_user_data));
        
        // Queue HTTP request read operation (mark as client operation)
        let client_read_user_data = encoded_user_data | CLIENT_OP_FLAG;
        let buffer_size = self.client_buffers.get_buffer_size();
        let buffer_group = self.client_buffers.get_buffer_group_id();
        self.submit_read_with_buffers(client_fd, buffer_group, buffer_size, client_read_user_data)?;
        
        debug!("HTTP read queued: client_fd={}", client_fd);
        Ok(())
    }
    
    /// Direct TCP processing for specialized workers
    /// Eliminates protocol encoding since worker is TCP-specialized
    fn start_tcp_processing_direct(&mut self, client_fd: RawFd, listener_fd: RawFd) -> Result<()> {
        // Get server port from listener configuration
        let server_port = self.get_listener_port(listener_fd)
            .ok_or_else(|| anyhow!("Unknown listener FD: {}", listener_fd))?;
        
        // Encode server port directly in user_data - no HashMap needed
        let read_user_data = encode_connection_data_with_port(client_fd, 0, 0, server_port);
        
        // Mark as client operation for completion routing
        let client_read_user_data = read_user_data | CLIENT_OP_FLAG;
        let buffer_size = self.client_buffers.get_buffer_size();
        let buffer_group = self.client_buffers.get_buffer_group_id();
        self.submit_read_with_buffers(client_fd, buffer_group, buffer_size, client_read_user_data)?;
        
        debug!("TCP read queued: client_fd={}", client_fd);
        Ok(())
    }
    
    
    
    /// Convert config Protocol to runtime Protocol
    fn protocol_to_runtime_protocol(protocol: &crate::config::core::Protocol) -> Result<Protocol> {
        match protocol {
            crate::config::core::Protocol::HTTP | 
            crate::config::core::Protocol::HTTPS => Ok(Protocol::HTTP),
            crate::config::core::Protocol::TCP => Ok(Protocol::TCP),
            crate::config::core::Protocol::UDP => {
                // UDP not yet implemented in the worker
                anyhow::bail!("UDP protocol not yet supported in worker implementation")
            },
        }
    }
    
    /// Get the protocol this worker handles
    pub fn get_worker_protocol(&self) -> Protocol {
        Self::protocol_to_runtime_protocol(&self.worker_config.protocol)
            .unwrap_or(Protocol::HTTP)
    }
    
    /// Check if worker protocol is compatible with listener protocol
    /// Implements single-codepath protocol specialization logic
    fn is_protocol_compatible(&self, worker_protocol: &Protocol, listener_protocol: &crate::runtime_config::Protocol) -> bool {
        use crate::runtime_config::Protocol as RuntimeProtocol;
        
        match (worker_protocol, listener_protocol) {
            // HTTP workers can handle HTTP and HTTPS listeners
            (Protocol::HTTP, RuntimeProtocol::HTTP) => true,
            (Protocol::HTTP, RuntimeProtocol::HTTPS) => true,
            
            // TCP workers can only handle TCP listeners  
            (Protocol::TCP, RuntimeProtocol::TCP) => true,
            
            // UDP not supported in worker protocol
            // (Protocol::UDP, RuntimeProtocol::UDP) => true,
            
            // All other combinations are incompatible
            _ => false,
        }
    }
    
    
    
    
    
    
    /// Setup listeners for protocol-compatible ports only  
    /// Implements single-codepath worker specialization for eBPF routing
    pub fn setup_listeners(&mut self) -> Result<()> {
        info!("Setting up listeners for worker {} with protocol specialization", self.worker_id);
        
        let worker_protocol = self.get_worker_protocol();
        info!("Worker {} protocol: {:?}", self.worker_id, worker_protocol);
        
        // Filter configured listener ports by protocol compatibility
        let mut listener_ports: Vec<u16> = Vec::new();
        for (&port, listener_info) in &self.config.listeners {
            if self.is_protocol_compatible(&worker_protocol, &listener_info.protocol) {
                listener_ports.push(port);
                info!("Worker {} will listen on port {} (protocol: {:?})", 
                      self.worker_id, port, listener_info.protocol);
            } else {
                info!("Worker {} skipping port {} (incompatible protocol: {:?}, worker protocol: {:?})", 
                      self.worker_id, port, listener_info.protocol, worker_protocol);
            }
        }
        
        // Add TCP route ports only if worker handles TCP protocol
        if worker_protocol == Protocol::TCP {
            let routes = unsafe { &*self.routes.load(Ordering::Acquire) };
            let tcp_ports = routes.get_tcp_ports();
            for tcp_port in tcp_ports {
                if !listener_ports.contains(&tcp_port) {
                    info!("Adding TCP route port {} to listeners for TCP worker {}", tcp_port, self.worker_id);
                    listener_ports.push(tcp_port);
                }
            }
        }
        
        let _port_count = listener_ports.len();
        
        for port in listener_ports {
            // Setup listener socket with SO_REUSEPORT for this port
            self.setup_listener(port)?;
        }
        
        // CRITICAL: Submit all queued accept operations before entering event loop
        // Without this, the initial accept operations are never submitted to the kernel,
        // causing the event loop to deadlock waiting for completions that never arrive
        if self.ring.submission().len() > 0 {
            let _submitted = self.ring.submit()?;
            info!("Worker {} submitted {} initial accept operations to kernel", self.worker_id, submitted);
        }
        
        info!("Worker {} completed protocol-specialized listener setup for {} ports", self.worker_id, port_count);
        Ok(())
    }
    
    /// Setup individual listener socket for a port
    /// Creates SO_REUSEPORT socket for eBPF distribution
    fn setup_listener(&mut self, port: u16) -> Result<()> {
        use socket2::{Socket, Domain, Type, Protocol};
        use std::os::unix::io::AsRawFd;
        
        // Create socket with SO_REUSEPORT BEFORE binding
        let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
        socket.set_reuse_port(true)?;  // Set BEFORE binding
        socket.set_nonblocking(true)?;
        
        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
        socket.bind(&addr.into())?;
        socket.listen(1024)?;
        
        let fd = socket.as_raw_fd();
        
        // Store listener FD for eBPF attachment
        self.listeners.insert(port, fd);
        // Store reverse mapping for server_port resolution
        self.listener_fd_to_port.insert(fd, port);
        
        // Submit accept operation
        self.submit_accept_with_encoding(fd)?;
        
        info!("Worker {} listening on port {} (fd={}) with SO_REUSEPORT", self.worker_id, port, fd);
        
        // Keep socket alive by forgetting it
        std::mem::forget(socket);
        
        Ok(())
    }
    
    /// Submit accept operation for listener socket
    fn submit_accept_with_encoding(&mut self, listener_fd: RawFd) -> Result<()> {
        let base_user_data = encode_connection_data(listener_fd as RawFd, 0, 0);
        
        // Mark as accept operation using bit flag for infinite scalability (1M+ RPS capable)
        let user_data = mark_as_accept_operation(base_user_data);
        
        // Submit accept operation directly
        self.submit_accept(listener_fd, user_data)
    }
    
    /// Get listener file descriptors for SO_REUSEPORT eBPF attachment
    /// Required by UringDataPlane interface
    pub fn get_listener_fds(&self) -> Vec<(u16, RawFd)> {
        self.listeners.iter().map(|(&port, &fd)| (port, fd)).collect()
    }
    
    /// Get server port from listener file descriptor
    /// Used to resolve server_port from accept operations without user_data encoding
    fn get_listener_port(&self, listener_fd: RawFd) -> Option<u16> {
        self.listener_fd_to_port.get(&listener_fd).copied()
    }
}

impl Drop for UringWorker {
    fn drop(&mut self) {
        info!("Dropping UringWorker {} - performing cleanup", self.worker_id);
        
        // Clean up backend connection mappings
        info!("Cleaning up {} backend connection mappings", self.backend_fd_to_addr.len());
        self.backend_fd_to_addr.clear();
        
        // WorkerBackendPool will clean up its own connections and epoll in its Drop
        
        // Release both buffer rings to prevent kernel resource leaks
        let client_ring = std::mem::replace(&mut self.client_buffers, NativeBufferRing::new(0, 0, 0));
        let backend_ring = std::mem::replace(&mut self.backend_buffers, NativeBufferRing::new(0, 0, 0));
        
        if let Err(e) = client_ring.release(&mut self.ring) {
            error!("Failed to release client buffer ring for worker {}: {}", self.worker_id, e);
        }
        if let Err(e) = backend_ring.release(&mut self.ring) {
            error!("Failed to release backend buffer ring for worker {}: {}", self.worker_id, e);
        }
        debug!("Successfully released dual buffer rings for worker {}", self.worker_id);
        
        // Clean up event-driven polling infrastructure
        unsafe {
            if self.eventfd >= 0 {
                libc::close(self.eventfd);
                debug!("Closed eventfd {} for worker {}", self.eventfd, self.worker_id);
            }
            if self.epoll_fd >= 0 {
                libc::close(self.epoll_fd);
                debug!("Closed epoll {} for worker {}", self.epoll_fd, self.worker_id);
            }
        }
        
        info!("UringWorker {} cleanup complete (including event-driven polling)", self.worker_id);
    }
}

impl UringWorker {
    
    /// Process HTTP request with connection parser for request counting
    pub fn process_http_request_with_pipeline(
        &mut self,
        context: ProcessingContext,
        buffer_id: u16,
        bytes_read: usize,
        user_data: u64,
    ) -> Result<()> {
        debug!("Processing HTTP request with TRUE ZERO-COPY: bytes={}, client_fd={}", 
               bytes_read, context.client_fd);
        
        // Handle client connection closure (important for keep-alive cleanup)
        if bytes_read == 0 {
            debug!("Client connection closed during HTTP request processing: client_fd={}", context.client_fd);
            return self.cleanup_connection_with_ownership(user_data);
        }
        
        // SINGLE BUFFER ACQUISITION: Get raw buffer pointer for zero overhead
        let decision = {
            // Get raw pointer directly without wrapper
            let buffer_ptr_opt = unsafe { self.client_buffers.get_buffer_ptr(buffer_id, bytes_read) };
            if let Some((buffer_ptr, actual_len)) = buffer_ptr_opt {
                debug!("PROCESS_HTTP: Got buffer pointer for analysis, client_fd={}, bytes={}", context.client_fd, actual_len);
                let routes = unsafe { &*self.routes.load(Ordering::Acquire) };
                
                // Unified request analysis with protocol auto-detection (single codepath)
                match request_processor::RequestProcessor::analyze_request(
                    &routes,
                    &mut self.worker_lb_state,
                    buffer_ptr,
                    actual_len,
                    &context,
                ) {
                    Ok(decision) => {
                        debug!("PROCESS_HTTP: Analysis successful for client_fd={}", context.client_fd);
                        Some(decision)
                    },
                    Err(e) => {
                        error!("PROCESS_HTTP: HTTP analysis failed for client_fd={}: {}", context.client_fd, e);
                        None
                    }
                }
            } else {
                error!("PROCESS_HTTP: Failed to get buffer pointer for HTTP request client_fd={}", context.client_fd);
                None
            }
        };
        
        // BorrowedBuffer is dropped here, but buffer pointer is preserved in decision
        match decision {
            Some(decision) => {
                debug!("PROCESS_HTTP: Executing decision for client_fd={}", context.client_fd);
                self.execute_processing_decision(decision, context, user_data)
            }
            None => {
                debug!("PROCESS_HTTP: No decision made, cleaning up connection for client_fd={}", context.client_fd);
                // HTTP processing failed - cleanup connection
                self.cleanup_connection_with_ownership(user_data)
            }
        }
    }
    
    
    /// Process TCP data using TRUE ZERO-COPY implementation
    /// Pure zero-copy forwarding without buffer inspection
    pub fn process_tcp_data_with_pipeline(
        &mut self,
        context: ProcessingContext,
        buffer_id: u16,
        bytes_read: usize,
        user_data: u64,
    ) -> Result<()> {
        debug!("Processing TCP data with TRUE ZERO-COPY: bytes={}, port={}", bytes_read, context.server_port);
        
        // For TCP, route purely based on port without inspecting data
        let decision = {
            let routes = unsafe { &*self.routes.load(Ordering::Acquire) };
            
            // Get raw pointer for forwarding (but don't inspect the data)
            let buffer_ptr_opt = unsafe { self.client_buffers.get_buffer_ptr(buffer_id, bytes_read) };
            if let Some((buffer_ptr, actual_len)) = buffer_ptr_opt {
                // Direct TCP routing based on server port only - NO data inspection
                match routes.find_tcp_backend_pool(context.server_port) {
                    Ok(backend_pool) => {
                        if backend_pool.is_empty() {
                            error!("TCP routing failed for port {}: no backends available", context.server_port);
                            Some(ProcessingDecision::CloseConnection)
                        } else {
                            // Worker-local backend selection
                            let route_hash = context.server_port as u64;
                            let backend_index = self.worker_lb_state.select_backend_index(route_hash, backend_pool.backend_count());
                            let selected_backend = backend_pool.get_backend(backend_index).unwrap();
                            
                            debug!("TCP route found: port {} -> backend {}", 
                                   context.server_port, selected_backend.socket_addr);
                            
                            // Forward TCP data without modification
                            Some(ProcessingDecision::TcpForward {
                                backend_addr: selected_backend.socket_addr,
                                buffer_ptr,
                                buffer_length: actual_len,
                            })
                        }
                    }
                    Err(_) => {
                        error!("TCP routing failed for port {}: no route configured", context.server_port);
                        Some(ProcessingDecision::CloseConnection)
                    }
                }
            } else {
                error!("Failed to get buffer pointer for TCP connection");
                None
            }
        };
        
        // BorrowedBuffer is dropped here, but buffer pointer is preserved in decision
        match decision {
            Some(decision) => {
                self.execute_processing_decision(decision, context, user_data)
            }
            None => {
                // TCP processing failed - cleanup connection
                self.cleanup_connection_with_ownership(user_data)
            }
        }
    }
    
    
    /// Forward response to client using stored buffer pointer (eliminates get_buffer() call)
    fn forward_response_to_client_with_buffer(
        &mut self,
        client_fd: RawFd,
        _backend_fd: RawFd,
        buffer_ptr: *const u8,
        bytes_to_write: usize,
        user_data: u64,
        _buffer_id: u16,
    ) -> Result<()> {
        debug!("Forwarding response to client: client_fd={}, bytes={}, keep_alive={}", 
               client_fd, bytes_to_write, is_keepalive_enabled(user_data));
        
        // Use passed user_data to preserve keep-alive bit and other connection state
        let write_user_data = user_data;
        
        // DEBUG: Log buffer details using stored pointer
        let _buffer_slice = unsafe { std::slice::from_raw_parts(buffer_ptr, bytes_to_write.min(100)) };
        debug!("BUFFER_DEBUG: buffer_id={}, buffer_ptr={:?}, first_100_bytes={:?}", 
               buffer_id, buffer_ptr, String::from_utf8_lossy(buffer_slice));
        
        // SINGLE-ACQUISITION SENDZC: Use stored buffer pointer directly
        self.submit_sendzc(
            client_fd,
            buffer_ptr as *mut u8,
            bytes_to_write,
            write_user_data,
        )?;
        
        debug!("Submitted HTTP response write: client_fd={}, backend_fd={}, write_user_data={:016x}", 
               client_fd, extract_backend_fd(write_user_data), write_user_data);
        Ok(())
    }
    

    // === CORE HTTP/TCP HANDLING METHODS ===
    // These methods replace the decision_executor.rs abstraction layer
    
    /// Handle HTTP request forwarding to backend with stored buffer pointer
    pub fn handle_http_forward_with_buffer(
        &mut self,
        backend_addr: std::net::SocketAddr,
        context: request_processor::ProcessingContext,
        buffer_ptr: *const u8,
        buffer_length: usize,
        original_user_data: u64,
    ) -> Result<()> {
        debug!("Handling HTTP forward: backend_addr={}, client_fd={}, bytes={}", 
               backend_addr, context.client_fd, buffer_length);
        
        // Try to get existing connection from pool
        if let Some((backend_fd, needs_cancel)) = self.worker_backend_pool.get_connection(backend_addr) {
            // Got connection from pool, use it immediately
            debug!("Got backend connection from pool: fd={} for {}", backend_fd, backend_addr);
            
            // Cancel monitoring if this connection was being monitored
            if needs_cancel {
                self.worker_backend_pool.cancel_pool_monitor(&mut self.ring, backend_fd, backend_addr)?;
            }
            
            self.backend_fd_to_addr.insert(backend_fd, backend_addr);
            
            let pool_index = 0; // Dummy value for user_data encoding
            let write_user_data = encode_connection_data(0, pool_index, backend_fd);
            let read_user_data = encode_connection_data(context.client_fd, pool_index, backend_fd) | (original_user_data & KEEPALIVE_ENABLED);
            
            self.submit_backend_write_read_linked_with_buffer(
                backend_fd,
                buffer_ptr as *mut u8,
                buffer_length,
                self.backend_buffers.get_buffer_group_id(),
                self.backend_buffers.get_buffer_size(),
                write_user_data,
                read_user_data,
            )?;
        } else {
            // No connection available - check if we can create new
            if self.worker_backend_pool.can_create_new(backend_addr) {
                debug!("No backend connection available, submitting async connect to {}", backend_addr);
                self.submit_backend_connect(backend_addr, context.client_fd, buffer_ptr, buffer_length)?;
            } else {
                // Pool exhausted - return error
                debug!("Backend pool exhausted for {}, returning 503", backend_addr);
                let (error_ptr, error_len) = self.get_error_response_for_sendzc(503);
                let error_user_data = original_user_data;
                self.submit_sendzc(context.client_fd, error_ptr, error_len, error_user_data)?;
            }
        }
        
        Ok(())
    }
    
    /// Handle TCP request forwarding to backend with stored buffer pointer
    pub fn handle_tcp_forward_with_buffer(
        &mut self,
        backend_addr: std::net::SocketAddr,
        context: request_processor::ProcessingContext,
        buffer_ptr: *const u8,
        buffer_length: usize,
        original_user_data: u64,
    ) -> Result<()> {
        debug!("Handling TCP forward: backend_addr={}, client_fd={}, bytes={}", 
               backend_addr, context.client_fd, buffer_length);
        
        // Try to get existing connection from pool
        if let Some((backend_fd, needs_cancel)) = self.worker_backend_pool.get_connection(backend_addr) {
            // Got connection from pool, use it immediately
            debug!("Got backend connection from pool: fd={} for {}", backend_fd, backend_addr);
            
            // Cancel monitoring if this connection was being monitored
            if needs_cancel {
                self.worker_backend_pool.cancel_pool_monitor(&mut self.ring, backend_fd, backend_addr)?;
            }
            
            self.backend_fd_to_addr.insert(backend_fd, backend_addr);
            
            let pool_index = 0; // Dummy value for user_data encoding
            let write_user_data = encode_connection_data(0, pool_index, backend_fd);
            let read_user_data = encode_connection_data(context.client_fd, pool_index, backend_fd) | (original_user_data & KEEPALIVE_ENABLED);
            
            self.submit_backend_write_read_linked_with_buffer(
                backend_fd,
                buffer_ptr as *mut u8,
                buffer_length,
                self.backend_buffers.get_buffer_group_id(),
                self.backend_buffers.get_buffer_size(),
                write_user_data,
                read_user_data,
            )?;
            
            // TCP also needs bidirectional forwarding - client→backend
            self.submit_tcp_client_read(context.client_fd, backend_fd, context.server_port, original_user_data)?;
        } else {
            // No connection available - check if we can create new
            if self.worker_backend_pool.can_create_new(backend_addr) {
                debug!("No backend connection available, submitting async connect to {}", backend_addr);
                self.submit_backend_connect(backend_addr, context.client_fd, buffer_ptr, buffer_length)?;
            } else {
                // Pool exhausted - return error
                debug!("Backend pool exhausted for {}, returning 503", backend_addr);
                let (error_ptr, error_len) = self.get_error_response_for_sendzc(503);
                let error_user_data = original_user_data;
                self.submit_sendzc(context.client_fd, error_ptr, error_len, error_user_data)?;
            }
        }
        
        Ok(())
    }
    
    /// Handle HTTP error response
    pub fn handle_http_error(
        &mut self,
        context: request_processor::ProcessingContext,
        error_code: u16,
        close_after_write: bool,
        original_user_data: u64,
    ) -> Result<()> {
        debug!("Handling HTTP error: client_fd={}, error_code={}, close_after_write={}", 
               context.client_fd, error_code, close_after_write);
        
        // Generate error response using buffer ring for zero-copy
        let (buffer_ptr, response_len) = self.get_error_response_for_sendzc(error_code);
        
        // Submit error response write with state preservation
        let write_user_data = if close_after_write {
            // Force close for error responses that should close
            let _force_close_user_data = set_keepalive_bit(original_user_data, &ConnectionType::Close);
            encode_connection_data(context.client_fd, 0, 0)
        } else {
            // Preserve original keep-alive state for non-closing errors
            encode_connection_data(context.client_fd, 0, 0) | (original_user_data & KEEPALIVE_ENABLED)
        };
        self.submit_sendzc(context.client_fd, buffer_ptr, response_len, write_user_data)?;
        
        // Connection state transitions are handled by write completion handler
        // No explicit phase transitions needed in simplified system
        
        Ok(())
    }
    
    
    /// Submit TCP client read for bidirectional forwarding
    fn submit_tcp_client_read(
        &mut self,
        client_fd: RawFd,
        _backend_fd: RawFd,
        _server_port: u16,
        _original_user_data: u64,
    ) -> Result<()> {
        debug!("Submitting TCP client read: client_fd={}, backend_fd={}, port={}", 
               client_fd, _backend_fd, _server_port);
        
        let user_data = encode_connection_data(client_fd, 0, 0);
        // Mark as client operation for completion routing
        let client_read_user_data = user_data | CLIENT_OP_FLAG;
        let buffer_size = self.client_buffers.get_buffer_size();
        let buffer_group = self.client_buffers.get_buffer_group_id();
        
        self.submit_read_with_buffers(
            client_fd,
            buffer_group,
            buffer_size,
            client_read_user_data,
        )?;
        
        Ok(())
    }
    
    /// Create error response for SENDZC using pre-allocated static responses
    /// MEMORY LEAK FIX: Use static responses instead of heap allocation + forget
    fn get_error_response_for_sendzc(&mut self, error_code: u16) -> (*mut u8, usize) {
        // Pre-allocated static error responses - zero heap allocation
        let response_bytes: &'static [u8] = match error_code {
            400 => b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            404 => b"HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n", 
            500 => b"HTTP/1.1 500 Internal Server Error\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            502 => b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            503 => b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            _ => b"HTTP/1.1 500 Internal Server Error\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        };
        
        (response_bytes.as_ptr() as *mut u8, response_bytes.len())
    }
    
    /// Execute processing decision with stored buffer pointers (eliminates get_buffer() calls)
    fn execute_processing_decision(
        &mut self,
        decision: request_processor::ProcessingDecision,
        context: request_processor::ProcessingContext,
        original_user_data: u64,
    ) -> Result<()> {
        use request_processor::ProcessingDecision;
        
        debug!("EXECUTE_PROCESSING_DECISION: Processing decision for client_fd={}", context.client_fd);
        
        match decision {
            ProcessingDecision::HttpForward { backend_addr, buffer_ptr, buffer_length, connection_type, .. } => {
                debug!("EXECUTE_PROCESSING_DECISION: HttpForward to {} for client_fd={}", backend_addr, context.client_fd);
                
                // Apply parsed connection type to user_data (respect client's Connection header)
                let connection_aware_user_data = set_keepalive_bit(original_user_data, &connection_type);
                debug!("STATE_TRACE: original_user_data={:016x}, connection_type={:?}, connection_aware_user_data={:016x}", 
                       original_user_data, connection_type, connection_aware_user_data);
                
                self.handle_http_forward_with_buffer(backend_addr, context, buffer_ptr, buffer_length, connection_aware_user_data)
            }
            ProcessingDecision::TcpForward { backend_addr, buffer_ptr, buffer_length } => {
                debug!("EXECUTE_PROCESSING_DECISION: TcpForward to {} for client_fd={}", backend_addr, context.client_fd);
                self.handle_tcp_forward_with_buffer(backend_addr, context, buffer_ptr, buffer_length, original_user_data)
            }
            ProcessingDecision::SendHttpError { error_code, close_connection } => {
                debug!("EXECUTE_PROCESSING_DECISION: SendHttpError {} for client_fd={}", error_code, context.client_fd);
                self.handle_http_error(context, error_code, close_connection, original_user_data)
            }
            ProcessingDecision::CloseConnection => {
                self.cleanup_connection_with_ownership(original_user_data)
            }
        }
    }
    
    // === DIRECT IO_URING OPERATIONS ===
    // Direct operations without abstraction layer overhead
    
    #[inline(always)]
    fn submit_accept(&mut self, listener_fd: RawFd, user_data: u64) -> Result<()> {
        let entry = opcode::Accept::new(
            types::Fd(listener_fd),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
        .build()
        .user_data(user_data);
        
        unsafe {
            if self.ring.submission().push(&entry).is_err() {
                // Queue full - critical operation, return error
                return Err(anyhow!("Submission queue full, cannot submit accept"));
            }
        }
        
        // Operations are batched - submission handled by event loop
        Ok(())
    }
    
    #[inline(always)]
    fn submit_read_with_buffers(&mut self, fd: RawFd, buffer_group: u16, buffer_size: u32, user_data: u64) -> Result<()> {
        // No ownership transfer needed - kernel manages operation lifecycle
        
        let entry = opcode::Read::new(
            types::Fd(fd),
            std::ptr::null_mut(),
            buffer_size,
        )
        .buf_group(buffer_group)
        .build()
        .flags(io_uring::squeue::Flags::BUFFER_SELECT)
        .user_data(user_data);
        
        unsafe {
            if self.ring.submission().push(&entry).is_err() {
                // Queue full - return error
                return Err(anyhow!("Submission queue full, cannot submit read"));
            }
        }
        
        // Operations are batched - submission handled by event loop
        Ok(())
    }
    
    #[inline(always)]
    fn submit_close(&mut self, fd: RawFd, user_data: u64) -> Result<()> {
        debug!("Submitting close operation: fd={}", fd);
        
        let entry = opcode::Close::new(types::Fd(fd))
            .build()
            .user_data(user_data);
        
        unsafe {
            // Close is best-effort - don't block on queue full
            let _ = self.ring.submission().push(&entry);
        }
        
        // Operations are batched - submission handled by event loop
        Ok(())
    }
    
    /// Submit pool monitor operation to detect backend-initiated closures
    #[inline(always)]
    fn submit_pool_monitor(&mut self, fd: RawFd, backend_addr: SocketAddr) -> Result<()> {
        // Encode monitoring user_data with fd and backend port
        let user_data = encode_pool_monitor(fd, backend_addr.port());
        
        // Submit PollAdd to detect when backend closes connection
        let poll_op = opcode::PollAdd::new(
            types::Fd(fd),
            (libc::POLLRDHUP | libc::POLLHUP) as u32  // Detect peer closure
        )
        .build()
        .user_data(user_data);
        
        unsafe {
            // Pool monitoring is best-effort - don't block on queue full
            let _ = self.ring.submission().push(&poll_op);
        }
        
        // Track that this connection is being monitored
        self.worker_backend_pool.monitored_connections.insert(fd, backend_addr);
        debug!("Submitted pool monitor for fd={} backend={}", fd, backend_addr);
        
        Ok(())
    }
    
    /// Cancel pool monitor operation when borrowing connection from pool
    #[inline(always)]
    fn cancel_pool_monitor(&mut self, fd: RawFd, backend_addr: SocketAddr) -> Result<()> {
        // Remove from tracked monitors
        self.worker_backend_pool.monitored_connections.remove(&fd);
        
        // Cancel the poll operation
        let user_data = encode_pool_monitor(fd, backend_addr.port());
        let cancel_op = opcode::PollRemove::new(user_data)
            .build()
            .user_data(0); // We don't care about the cancel completion
        
        unsafe {
            // Pool monitor cancellation is best-effort
            let _ = self.ring.submission().push(&cancel_op);
        }
        
        debug!("Cancelled pool monitor for fd={} backend={}", fd, backend_addr);
        Ok(())
    }
    
    #[inline(always)]
    fn submit_timeout(&mut self, timeout_duration: Duration, user_data: u64) -> Result<()> {
        debug!("Submitting timeout operation: duration={:?}, user_data={}", timeout_duration, user_data);
        
        let timeout_spec = types::Timespec::new()
            .sec(timeout_duration.as_secs())
            .nsec(timeout_duration.subsec_nanos());
            
        let entry = opcode::Timeout::new(&timeout_spec)
            .build()
            .user_data(user_data);
        
        unsafe {
            // Timeout is best-effort
            let _ = self.ring.submission().push(&entry);
        }
        
        // Operations are batched - submission handled by event loop
        Ok(())
    }
    
    #[inline(always)]
    fn submit_timeout_remove(&mut self, timeout_user_data: u64, user_data: u64) -> Result<()> {
        debug!("Submitting timeout remove operation: timeout_user_data={}, user_data={}", timeout_user_data, user_data);
        
        let entry = opcode::TimeoutRemove::new(timeout_user_data)
            .build()
            .user_data(user_data);
        
        unsafe {
            // Timeout removal is best-effort
            let _ = self.ring.submission().push(&entry);
        }
        
        // Operations are batched - submission handled by event loop
        Ok(())
    }

    
    /// Wait for completion notification via eventfd + epoll
    fn wait_for_completion_notification(&mut self) -> Result<()> {
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 1];
        
        let result = unsafe {
            libc::epoll_wait(
                self.epoll_fd,
                events.as_mut_ptr(),
                1,
                10, // 10ms timeout for responsive shutdown checks and batch timing
            )
        };
        
        if result > 0 {
            // Eventfd signaled - clear it and continue
            self.clear_eventfd()?;
        }
        
        Ok(())
    }
    
    
    /// Clear eventfd counter after notification
    fn clear_eventfd(&mut self) -> Result<()> {
        let mut value: u64 = 0;
        let result = unsafe {
            libc::read(
                self.eventfd,
                &mut value as *mut u64 as *mut libc::c_void,
                8,
            )
        };
        
        // Non-blocking read may return EAGAIN if no data
        if result < 0 {
            let errno = std::io::Error::last_os_error();
            if errno.raw_os_error() != Some(libc::EAGAIN) {
                return Err(anyhow!("Failed to clear eventfd: {}", errno));
            }
        }
        
        Ok(())
    }
    
    
    #[inline(always)]
    fn submit_sendzc(&mut self, fd: RawFd, buffer_ptr: *mut u8, bytes_to_write: usize, user_data: u64) -> Result<()> {
        use crate::uring_worker::io_operations::BufferRingOperations;
        
        // No ownership transfer needed - kernel manages operation lifecycle
        
        // Simplified SENDZC operation - no buffer_id tracking needed
        let encoded_user_data = BufferRingOperations::encode_sendzc_user_data(user_data);
        
        let entry = opcode::Send::new(
            types::Fd(fd),
            buffer_ptr,
            bytes_to_write as u32,
        )
        .flags(libc::MSG_ZEROCOPY)
        .build()
        .user_data(encoded_user_data);
        
        unsafe {
            if self.ring.submission().push(&entry).is_err() {
                // Queue full - critical operation, return error
                return Err(anyhow!("Submission queue full, cannot submit send"));
            }
        }
        
        // CRITICAL PERFORMANCE FIX: Do NOT submit immediately
        // Operations will be batched and submitted by the event loop
        Ok(())
    }
    
    #[inline(always)]
    fn submit_backend_write_read_linked_with_buffer(
        &mut self,
        backend_fd: RawFd,
        buffer_ptr: *mut u8,
        bytes_to_write: usize,
        buffer_group_id: u16,
        buffer_size: u32,
        write_user_data: u64,
        read_user_data: u64,
    ) -> Result<()> {
        use crate::uring_worker::io_operations::BufferRingOperations;
        
        // Pre-check: Ensure we have space for 2 linked operations
        if !self.has_queue_space(2) {
            return Err(anyhow!("Insufficient queue space for linked operations"));
        }
        
        debug!("Submitting backend write-read link: backend_fd={}, bytes={}", backend_fd, bytes_to_write);
        debug!("BACKEND_READ_DEBUG: Setting up read on backend_fd={}, buffer_size={}, buffer_group_id={}", 
               backend_fd, buffer_size, buffer_group_id);
        
        // Build linked operations: SENDZC(backend) → Read(backend)
        // Simplified encoding - no buffer_id tracking needed
        let encoded_write_data = BufferRingOperations::encode_sendzc_user_data(write_user_data);
        
        let write_entry = opcode::Send::new(
            types::Fd(backend_fd),
            buffer_ptr,
            bytes_to_write as u32,
        )
        .flags(libc::MSG_ZEROCOPY)
        .build()
        .flags(io_uring::squeue::Flags::IO_LINK)  // Link to next operation
        .user_data(encoded_write_data);
        
        let read_entry = opcode::Read::new(
            types::Fd(backend_fd),
            std::ptr::null_mut(),
            buffer_size,
        )
        .buf_group(buffer_group_id)
        .build()
        .flags(io_uring::squeue::Flags::BUFFER_SELECT)  // Removed IO_LINK - this is the last operation
        .user_data(read_user_data);
        
        // Submit as linked operations - both must succeed or we fail
        unsafe {
            if self.ring.submission().push(&write_entry).is_err() {
                // Queue full - cannot submit linked operations
                return Err(anyhow!("Submission queue full, cannot submit linked write-read"));
            }
            if self.ring.submission().push(&read_entry).is_err() {
                // Critical error - write was queued but not read, operations not linked!
                // This breaks our protocol - fail immediately
                return Err(anyhow!("Failed to link read operation after write - protocol violation"));
            }
        }
        
        // Operations are batched - linked operations remain atomic
        // Submission handled by event loop
        
        debug!("Backend write-read link queued: backend_fd={}, write_user_data={:016x}, read_user_data={:016x}", 
               backend_fd, write_user_data, read_user_data);
        
        Ok(())
    }
    
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    #[test]
    fn test_sockaddr_v4_conversion() {
        // Test 127.0.0.1:3001
        let addr = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 3001);
        let storage = Box::new(SockaddrStorage {
            v4: unsafe { std::mem::zeroed() },
        });
        
        unsafe {
            let storage_ptr = Box::into_raw(storage);
            (*storage_ptr).v4.sin_family = libc::AF_INET as libc::sa_family_t;
            (*storage_ptr).v4.sin_port = addr.port().to_be();
            (*storage_ptr).v4.sin_addr.s_addr = u32::from_ne_bytes(addr.ip().octets());
            
            // Verify the values
            assert_eq!((*storage_ptr).v4.sin_family, libc::AF_INET as libc::sa_family_t);
            assert_eq!((*storage_ptr).v4.sin_port, 3001u16.to_be());
            
            // On little-endian systems, 127.0.0.1 in network byte order is 0x0100007f
            let expected_addr = if cfg!(target_endian = "little") {
                0x0100007f_u32
            } else {
                0x7f000001_u32
            };
            assert_eq!((*storage_ptr).v4.sin_addr.s_addr, expected_addr);
            
            // Clean up
            let _ = Box::from_raw(storage_ptr);
        }
    }
    
    #[test]
    fn test_sockaddr_v4_common_addresses() {
        // Test various common addresses
        let test_cases = vec![
            (Ipv4Addr::new(127, 0, 0, 1), "127.0.0.1"),
            (Ipv4Addr::new(192, 168, 1, 1), "192.168.1.1"),
            (Ipv4Addr::new(10, 0, 0, 1), "10.0.0.1"),
            (Ipv4Addr::new(0, 0, 0, 0), "0.0.0.0"),
            (Ipv4Addr::new(255, 255, 255, 255), "255.255.255.255"),
        ];
        
        for (ip, name) in test_cases {
            let addr = SocketAddrV4::new(ip, 8080);
            let storage = Box::new(SockaddrStorage {
                v4: unsafe { std::mem::zeroed() },
            });
            
            unsafe {
                let storage_ptr = Box::into_raw(storage);
                (*storage_ptr).v4.sin_family = libc::AF_INET as libc::sa_family_t;
                (*storage_ptr).v4.sin_port = addr.port().to_be();
                (*storage_ptr).v4.sin_addr.s_addr = u32::from_ne_bytes(addr.ip().octets());
                
                // Basic sanity checks
                assert_eq!((*storage_ptr).v4.sin_family, libc::AF_INET as libc::sa_family_t,
                    "Address family mismatch for {}", name);
                assert_eq!((*storage_ptr).v4.sin_port, 8080u16.to_be(),
                    "Port mismatch for {}", name);
                
                // Clean up
                let _ = Box::from_raw(storage_ptr);
            }
        }
    }
    
    #[test] 
    fn test_pending_connect_with_sockaddr() {
        let backend_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 3001));
        let buffer = vec![1, 2, 3, 4];
        let buffer_ptr = buffer.as_ptr();
        
        let mut storage = Box::new(SockaddrStorage {
            v4: unsafe { std::mem::zeroed() },
        });
        
        unsafe {
            storage.v4.sin_family = libc::AF_INET as libc::sa_family_t;
            storage.v4.sin_port = 3001u16.to_be();
            storage.v4.sin_addr.s_addr = u32::from_ne_bytes([127, 0, 0, 1]);
        }
        
        let pending = PendingConnect {
            backend_addr,
            buffer_ptr,
            buffer_length: buffer.len(),
            sockaddr_storage: storage,
        };
        
        // Verify the pending connect retains the sockaddr
        unsafe {
            assert_eq!(pending.sockaddr_storage.v4.sin_family, libc::AF_INET as libc::sa_family_t);
            assert_eq!(pending.sockaddr_storage.v4.sin_port, 3001u16.to_be());
        }
    }
}




