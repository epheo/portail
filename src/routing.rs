use fnv::FnvHashMap;
use anyhow::{anyhow, Result};

#[derive(Debug, Clone)]
pub struct RouteTable {
    pub http_routes: FnvHashMap<String, HostEntry>,
    pub tcp_routes: FnvHashMap<u16, Vec<Backend>>,
    pub udp_routes: FnvHashMap<u16, Vec<Backend>>,
}

impl RouteTable {
    pub fn new() -> Self {
        Self {
            http_routes: FnvHashMap::with_capacity_and_hasher(128, Default::default()),
            tcp_routes: FnvHashMap::with_capacity_and_hasher(32, Default::default()),
            udp_routes: FnvHashMap::with_capacity_and_hasher(32, Default::default()),
        }
    }

    #[inline(always)]
    pub fn hash_host(host: &str) -> u64 {
        FnvRouterHasher::hash(host)
    }

    /// Find HTTP backends for host and path (1:1 architecture)
    /// Stack-allocates a 256-byte buffer for ASCII lowercase — zero heap allocation,
    /// covers all valid DNS hostnames (max 253 chars per RFC 1035).
    #[inline(always)]
    pub fn find_http_backends(&self, host: &str, path: &str) -> Result<&Vec<Backend>> {
        let mut buf = [0u8; 256];
        let host_bytes = host.as_bytes();
        let len = host_bytes.len().min(256);
        buf[..len].copy_from_slice(&host_bytes[..len]);
        buf[..len].make_ascii_lowercase();
        let key = std::str::from_utf8(&buf[..len]).unwrap_or(host);

        if let Some(host_entry) = self.http_routes.get(key) {
            if let Some(backend_list) = Self::find_best_path_match(&host_entry.path_routes, path) {
                return Ok(backend_list);
            }
        }

        Err(anyhow!("No HTTP route found for host={} path={}", host, path))
    }

    /// Find TCP backends by server port (1:1 architecture)
    #[inline(always)]
    pub fn find_tcp_backends(&self, server_port: u16) -> Result<&Vec<Backend>> {
        // Look up TCP route by server listening port
        if let Some(backend_list) = self.tcp_routes.get(&server_port) {
            return Ok(backend_list);
        }
        
        Err(anyhow!("No TCP route found for port {}", server_port))
    }

    #[inline(always)]
    fn find_best_path_match<'a>(path_routes: &'a [PathRoute], path: &str) -> Option<&'a Vec<Backend>> {
        let path_bytes = path.as_bytes();
        for route in path_routes {
            let prefix_bytes = route.prefix.as_bytes();
            if path_bytes.len() >= prefix_bytes.len()
                && (prefix_bytes.is_empty() || path_bytes[..prefix_bytes.len()] == *prefix_bytes)
            {
                return Some(&route.backend);
            }
        }
        None
    }

    pub fn add_http_route(&mut self, host: &str, path_prefix: &str, backends: Vec<Backend>) {
        let host_key = host.to_ascii_lowercase();

        let path_route = PathRoute {
            prefix: path_prefix.to_string(),
            backend: backends,
        };

        let host_entry = self.http_routes.entry(host_key).or_insert_with(|| {
            HostEntry {
                path_routes: Vec::with_capacity(8),
            }
        });
        
        host_entry.path_routes.push(path_route);
        
        // Sort by prefix length (longest first) for proper matching
        host_entry.path_routes.sort_by(|a, b| b.prefix.len().cmp(&a.prefix.len()));
    }

    pub fn add_tcp_route(&mut self, port: u16, backends: Vec<Backend>) {
        self.tcp_routes.insert(port, backends);
    }

    #[inline(always)]
    pub fn find_udp_backends(&self, server_port: u16) -> Result<&Vec<Backend>> {
        if let Some(backend_list) = self.udp_routes.get(&server_port) {
            return Ok(backend_list);
        }
        Err(anyhow!("No UDP route found for port {}", server_port))
    }

    pub fn add_udp_route(&mut self, port: u16, backends: Vec<Backend>) {
        self.udp_routes.insert(port, backends);
    }
}

impl Default for RouteTable {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct HostEntry {
    pub path_routes: Vec<PathRoute>,
}

#[derive(Debug, Clone)]
pub struct PathRoute {
    pub prefix: String,
    pub backend: Vec<Backend>,
}

#[derive(Debug, Clone)]
pub struct Backend {
    pub socket_addr: std::net::SocketAddr,
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
            
        Ok(Self { socket_addr })
    }
}

/// FNV hash for host-based routing lookups.
/// eBPF also uses FNV for worker selection — both are needed:
/// - eBPF FNV: Address + protocol -> worker selection (kernel space)
/// - Rust FNV: Host header -> route table lookup (user space)
#[derive(Debug, Default, Clone, Copy)]
pub struct FnvRouterHasher;

impl FnvRouterHasher {
    /// Case-insensitive FNV hash — folds ASCII uppercase to lowercase per RFC 7230 §2.7.3
    #[inline(always)]
    pub fn hash(data: &str) -> u64 {
        const FNV_OFFSET_BASIS: u64 = 14695981039346656037;
        const FNV_PRIME: u64 = 1099511628211;

        let bytes = data.as_bytes();
        let mut hash = FNV_OFFSET_BASIS;

        for &byte in bytes {
            let normalized = byte.to_ascii_lowercase();
            hash ^= normalized as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
        }

        hash
    }
}
