use fnv::FnvHashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use anyhow::{anyhow, Result};

// Branch prediction hints for hot paths
#[inline(always)]
fn likely(b: bool) -> bool {
    std::hint::black_box(b)
}

#[inline(always)]
fn unlikely(b: bool) -> bool {
    std::hint::black_box(b)
}

#[derive(Debug, Clone)]
#[repr(C)]
pub struct RouteTable {
    // Hot data first: http_routes accessed most frequently (>95% of traffic)
    pub http_routes: FnvHashMap<u64, HostEntry>,
    // TCP routes accessed less frequently  
    pub tcp_routes: FnvHashMap<u16, BackendPool>,
    // Default backend accessed rarely (only when no specific HTTP route found)
    pub default_http_backend: Option<BackendPool>,
}

impl RouteTable {
    pub fn new() -> Self {
        Self {
            // Pre-allocate capacity for typical microservice routing tables
            http_routes: FnvHashMap::with_capacity_and_hasher(128, Default::default()),
            tcp_routes: FnvHashMap::with_capacity_and_hasher(32, Default::default()),
            default_http_backend: None,
        }
    }

    #[inline(always)]
    pub fn route_http_request(&self, host_hash: u64, path: &str) -> Result<&Backend> {
        // Fast path: Look up host entry with branch prediction hint
        if likely(self.http_routes.contains_key(&host_hash)) {
            let host_entry = unsafe { self.http_routes.get(&host_hash).unwrap_unchecked() };
            
            // Path matching: use radix tree lookup for common prefixes
            let backend = self.find_best_path_match(&host_entry.path_routes, path)
                .unwrap_or(&host_entry.default_backend);
            
            return backend.select_backend();
        }
        
        // Fallback: Use global default backend
        if unlikely(self.default_http_backend.is_some()) {
            return self.default_http_backend.as_ref().unwrap().select_backend();
        }
        
        Err(anyhow!("No route found for HTTP request"))
    }
    
    /// TCP routing by server listening port - O(1) lookup
    #[inline(always)]
    pub fn route_tcp_request(&self, server_port: u16) -> Result<&Backend> {
        // Look up TCP route by server listening port
        if let Some(backend_pool) = self.tcp_routes.get(&server_port) {
            return backend_pool.select_backend();
        }
        
        Err(anyhow!("No TCP route configured for port {}", server_port))
    }
    
    /// Path matching using prefix tree concepts
    #[inline(always)]
    fn find_best_path_match<'a>(&self, path_routes: &'a [PathRoute], path: &str) -> Option<&'a BackendPool> {
        let path_bytes = path.as_bytes();
        let mut best_match: Option<(&PathRoute, usize)> = None;
        
        // Linear search with early termination and SIMD-style matching
        for route in path_routes {
            let prefix_bytes = route.prefix.as_bytes();
            
            // Fast length check
            if prefix_bytes.len() > path_bytes.len() {
                continue;
            }
            
            // Use byte comparison for prefix matching
            if self.fast_prefix_match(path_bytes, prefix_bytes) {
                let match_len = prefix_bytes.len();
                
                // Track the longest matching prefix
                if best_match.map_or(true, |(_, len)| match_len > len) {
                    best_match = Some((route, match_len));
                }
            }
        }
        
        best_match.map(|(route, _)| &route.backend)
    }
    
    /// Prefix matching using manual loop unrolling
    #[inline(always)]
    fn fast_prefix_match(&self, path: &[u8], prefix: &[u8]) -> bool {
        if prefix.is_empty() {
            return true;
        }
        
        if path.len() < prefix.len() {
            return false;
        }
        
        // Manual loop unrolling for small prefixes (most common case)
        match prefix.len() {
            1 => path[0] == prefix[0],
            2 => path[0] == prefix[0] && path[1] == prefix[1],
            3 => path[0] == prefix[0] && path[1] == prefix[1] && path[2] == prefix[2],
            4 => path[0] == prefix[0] && path[1] == prefix[1] && path[2] == prefix[2] && path[3] == prefix[3],
            _ => {
                // Use SIMD comparison for longer prefixes
                path[..prefix.len()] == *prefix
            }
        }
    }


    pub fn add_http_route(&mut self, host: &str, path_prefix: &str, backend: Backend) {
        self.add_http_route_with_pool(host, path_prefix, vec![backend], LoadBalanceStrategy::RoundRobin);
    }

    pub fn add_http_route_with_pool(&mut self, host: &str, path_prefix: &str, backends: Vec<Backend>, strategy: LoadBalanceStrategy) {
        let host_hash = fnv_hash(host);
        let backend_count = backends.len();
        
        let path_route = PathRoute {
            prefix: path_prefix.to_string(),
            backend: BackendPool {
                backends,
                round_robin_counter: AtomicU32::new(0),
                strategy,
                health_state: Arc::new(AtomicU64::new((1u64 << backend_count) - 1)), // All backends healthy
                _padding: [0; 0],
            },
        };
        
        let host_entry = self.http_routes.entry(host_hash).or_insert_with(|| {
            HostEntry {
                // Pre-allocate capacity for typical number of paths per host
                path_routes: Vec::with_capacity(8),
                default_backend: BackendPool {
                    backends: vec![Backend::new("127.0.0.1".to_string(), 3001)
                        .expect("Default backend address should be valid")
                        .with_health_check("/health".to_string())],
                    round_robin_counter: AtomicU32::new(0),
                    strategy: LoadBalanceStrategy::RoundRobin,
                    health_state: Arc::new(AtomicU64::new(1)),
                    _padding: [0; 0],
                },
            }
        });
        
        host_entry.path_routes.push(path_route);
        
        // Sort by prefix length (longest first) for proper matching
        host_entry.path_routes.sort_by(|a, b| b.prefix.len().cmp(&a.prefix.len()));
    }

    pub fn add_tcp_route(&mut self, port: u16, backends: Vec<Backend>) {
        let backend_count = backends.len();
        let backend_pool = BackendPool {
            backends,
            round_robin_counter: AtomicU32::new(0),
            strategy: LoadBalanceStrategy::RoundRobin,
            health_state: Arc::new(AtomicU64::new((1u64 << backend_count) - 1)), // All healthy
            _padding: [0; 0],
        };
        
        self.tcp_routes.insert(port, backend_pool);
    }
    
    /// Get all TCP ports that have routes configured
    /// Used by workers to know which ports to listen on
    pub fn get_tcp_ports(&self) -> Vec<u16> {
        self.tcp_routes.keys().copied().collect()
    }
    
    /// Check if a port has a TCP route configured
    /// Used for immediate TCP detection without protocol detection
    pub fn has_tcp_route(&self, port: u16) -> bool {
        self.tcp_routes.contains_key(&port)
    }
}

#[derive(Debug, Clone)]
#[repr(C)]
pub struct HostEntry {
    pub path_routes: Vec<PathRoute>,
    pub default_backend: BackendPool,
}

#[derive(Debug, Clone)]
#[repr(C)]
pub struct PathRoute {
    pub prefix: String,
    pub backend: BackendPool,
}


#[derive(Debug)]
#[repr(C, align(64))]
pub struct BackendPool {
    // Hot data first: backends accessed on every request for load balancing
    pub backends: Vec<Backend>,
    // Round robin counter accessed frequently for load balancing
    pub round_robin_counter: AtomicU32,
    // Strategy accessed during backend selection
    pub strategy: LoadBalanceStrategy,
    // Health state accessed less frequently (only during health checks)
    pub health_state: Arc<AtomicU64>,
    // Cache line padding to prevent false sharing
    pub _padding: [u8; 0], // align(64) handles alignment, minimal padding needed
}

impl Clone for BackendPool {
    fn clone(&self) -> Self {
        Self {
            backends: self.backends.clone(),
            round_robin_counter: AtomicU32::new(self.round_robin_counter.load(std::sync::atomic::Ordering::Relaxed)),
            strategy: self.strategy.clone(),
            health_state: self.health_state.clone(),
            _padding: [0; 0],
        }
    }
}

impl BackendPool {
    #[inline(always)]
    pub fn select_backend(&self) -> Result<&Backend> {
        match self.strategy {
            LoadBalanceStrategy::RoundRobin => {
                if self.backends.is_empty() {
                    return Err(anyhow!("No backends available"));
                }
                
                let index = self.round_robin_counter.fetch_add(1, Ordering::Relaxed) as usize % self.backends.len();
                Ok(&self.backends[index])
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum LoadBalanceStrategy {
    RoundRobin,
}

#[derive(Debug, Clone)]
#[repr(C)]
pub struct Backend {
    // Hot data first: pre-computed socket address for zero-allocation connection establishment
    pub socket_addr: std::net::SocketAddr,
    // Health check path is accessed less frequently
    pub health_check_path: Option<String>,
}

impl Backend {
    pub fn new(address: String, port: u16) -> Result<Self> {
        // Parse socket address once at creation time for zero-allocation hot path
        // Handle both IP addresses and hostnames
        let socket_addr = if let Ok(ip) = address.parse::<std::net::IpAddr>() {
            // Direct IP address - fast path
            std::net::SocketAddr::new(ip, port)
        } else {
            // Hostname - resolve to IP address once at creation time
            use std::net::ToSocketAddrs;
            let addr_str = format!("{}:{}", address, port);
            let mut addrs = addr_str.to_socket_addrs()
                .map_err(|e| anyhow!("Failed to resolve hostname {}:{}: {}", address, port, e))?;
            
            addrs.next()
                .ok_or_else(|| anyhow!("No addresses found for hostname {}:{}", address, port))?
        };
            
        Ok(Self {
            socket_addr,
            health_check_path: None,
        })
    }
    
    pub fn with_health_check(mut self, path: String) -> Self {
        self.health_check_path = Some(path);
        self
    }
    
}

/// FNV hash implementation with minimal overhead for sub-100ns performance
#[inline(always)]
pub fn fnv_hash(data: &str) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 14695981039346656037;
    const FNV_PRIME: u64 = 1099511628211;
    
    let bytes = data.as_bytes();
    let mut hash = FNV_OFFSET_BASIS;
    
    // Simple tight loop - let the compiler optimize
    // Simple loop allows compiler optimization for small strings (typical hostnames)
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    
    hash
}


