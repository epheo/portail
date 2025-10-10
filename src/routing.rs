use fnv::FnvHashMap;
use anyhow::{anyhow, Result};

#[derive(Debug, Clone)]
pub struct RouteTable {
    pub http_routes: FnvHashMap<String, HostEntry>,
    pub wildcard_http_routes: FnvHashMap<String, HostEntry>,
    pub tcp_routes: FnvHashMap<u16, Vec<Backend>>,
    pub udp_routes: FnvHashMap<u16, Vec<Backend>>,
}

impl RouteTable {
    pub fn new() -> Self {
        Self {
            http_routes: FnvHashMap::with_capacity_and_hasher(128, Default::default()),
            wildcard_http_routes: FnvHashMap::with_capacity_and_hasher(32, Default::default()),
            tcp_routes: FnvHashMap::with_capacity_and_hasher(32, Default::default()),
            udp_routes: FnvHashMap::with_capacity_and_hasher(32, Default::default()),
        }
    }

    #[inline(always)]
    pub fn hash_host(host: &str) -> u64 {
        FnvRouterHasher::hash(host)
    }

    /// Find an HTTP route rule for host, path, and headers.
    /// Tries exact host first, then wildcard. Within a host entry, exact path
    /// beats prefix, then longest prefix wins. Header matches are AND-combined.
    #[inline(always)]
    pub fn find_http_route<'a>(
        &'a self,
        host: &str,
        path: &str,
        header_data: &[u8],
    ) -> Result<&'a HttpRouteRule> {
        let mut buf = [0u8; 256];
        let host_bytes = host.as_bytes();
        let len = host_bytes.len().min(256);
        buf[..len].copy_from_slice(&host_bytes[..len]);
        buf[..len].make_ascii_lowercase();
        let key = std::str::from_utf8(&buf[..len]).unwrap_or(host);

        // Try exact host first
        if let Some(host_entry) = self.http_routes.get(key) {
            if let Some(rule) = Self::find_best_rule_match(&host_entry.rules, path, header_data) {
                return Ok(rule);
            }
        }

        // Try wildcard: strip first label (e.g. "foo.example.com" -> "example.com")
        if let Some(dot_pos) = key.find('.') {
            let parent = &key[dot_pos + 1..];
            if let Some(host_entry) = self.wildcard_http_routes.get(parent) {
                if let Some(rule) = Self::find_best_rule_match(&host_entry.rules, path, header_data) {
                    return Ok(rule);
                }
            }
        }

        Err(anyhow!("No HTTP route found for host={} path={}", host, path))
    }

    #[inline(always)]
    pub fn find_tcp_backends(&self, server_port: u16) -> Result<&Vec<Backend>> {
        if let Some(backend_list) = self.tcp_routes.get(&server_port) {
            return Ok(backend_list);
        }
        Err(anyhow!("No TCP route found for port {}", server_port))
    }

    /// Match rules: exact path first, then longest prefix. Header matches are AND-combined.
    #[inline(always)]
    fn find_best_rule_match<'a>(
        rules: &'a [HttpRouteRule],
        path: &str,
        header_data: &[u8],
    ) -> Option<&'a HttpRouteRule> {
        let path_bytes = path.as_bytes();

        for rule in rules {
            let path_matches = match rule.path_match_type {
                PathMatchType::Exact => path_bytes == rule.path.as_bytes(),
                PathMatchType::Prefix => {
                    let prefix_bytes = rule.path.as_bytes();
                    path_bytes.len() >= prefix_bytes.len()
                        && (prefix_bytes.is_empty()
                            || path_bytes[..prefix_bytes.len()] == *prefix_bytes)
                }
            };

            if !path_matches {
                continue;
            }

            // Check header matches (AND logic) — skip if no header_data provided and matches exist
            if !rule.header_matches.is_empty() {
                if header_data.is_empty() {
                    continue;
                }
                let all_match = rule.header_matches.iter().all(|hm| {
                    find_header_value(header_data, &hm.name)
                        .map_or(false, |v| v == hm.value)
                });
                if !all_match {
                    continue;
                }
            }

            return Some(rule);
        }
        None
    }

    pub fn add_http_route(&mut self, host: &str, rule: HttpRouteRule) {
        let host_lower = host.to_ascii_lowercase();

        // Wildcard hosts (*.example.com) are stored by their parent domain
        let (map, key) = if let Some(stripped) = host_lower.strip_prefix("*.") {
            (&mut self.wildcard_http_routes, stripped.to_string())
        } else {
            (&mut self.http_routes, host_lower)
        };

        let host_entry = map.entry(key).or_insert_with(|| HostEntry {
            rules: Vec::with_capacity(8),
        });

        host_entry.rules.push(rule);

        // Sort: exact paths first, then prefix by length desc
        host_entry.rules.sort_by(|a, b| {
            match (&a.path_match_type, &b.path_match_type) {
                (PathMatchType::Exact, PathMatchType::Prefix) => std::cmp::Ordering::Less,
                (PathMatchType::Prefix, PathMatchType::Exact) => std::cmp::Ordering::Greater,
                _ => b.path.len().cmp(&a.path.len()),
            }
        });
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
    pub rules: Vec<HttpRouteRule>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PathMatchType {
    Prefix,
    Exact,
}

#[derive(Debug, Clone)]
pub struct HttpRouteRule {
    pub path_match_type: PathMatchType,
    pub path: String,
    pub header_matches: Vec<HeaderMatch>,
    pub filters: Vec<HttpFilter>,
    pub backends: Vec<Backend>,
}

#[derive(Debug, Clone)]
pub struct HeaderMatch {
    /// Lowercase header name
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub enum HttpFilter {
    RequestHeaderModifier {
        add: Vec<HttpHeader>,
        set: Vec<HttpHeader>,
        remove: Vec<String>,
    },
    ResponseHeaderModifier {
        add: Vec<HttpHeader>,
        set: Vec<HttpHeader>,
        remove: Vec<String>,
    },
    RequestRedirect {
        scheme: Option<String>,
        hostname: Option<String>,
        port: Option<u16>,
        path: Option<String>,
        status_code: u16,
    },
    URLRewrite {
        hostname: Option<String>,
        path: Option<URLRewritePath>,
    },
    RequestMirror {
        backend_addr: std::net::SocketAddr,
    },
}

#[derive(Debug, Clone)]
pub enum URLRewritePath {
    ReplaceFullPath(String),
    ReplacePrefixMatch(String),
}

#[derive(Debug, Clone)]
pub struct HttpHeader {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct Backend {
    pub socket_addr: std::net::SocketAddr,
    pub weight: u32,
}

impl Backend {
    pub fn new(address: String, port: u16) -> Result<Self> {
        Self::with_weight(address, port, 1)
    }

    pub fn with_weight(address: String, port: u16, weight: u32) -> Result<Self> {
        let socket_addr = if let Ok(ip) = address.parse::<std::net::IpAddr>() {
            std::net::SocketAddr::new(ip, port)
        } else {
            use std::net::ToSocketAddrs;
            let addr_str = format!("{}:{}", address, port);
            let mut addrs = addr_str
                .to_socket_addrs()
                .map_err(|e| anyhow!("Failed to resolve hostname {}:{}: {}", address, port, e))?;
            addrs
                .next()
                .ok_or_else(|| anyhow!("No addresses found for hostname {}:{}", address, port))?
        };

        Ok(Self { socket_addr, weight })
    }
}

/// Zero-allocation header value lookup in raw header bytes.
/// Case-insensitive name match, returns trimmed value.
#[inline]
pub fn find_header_value<'a>(header_data: &'a [u8], name: &str) -> Option<&'a str> {
    let name_bytes = name.as_bytes();
    let name_len = name_bytes.len();
    let mut pos = 0;

    while pos < header_data.len() {
        // Find end of line
        let mut line_end = pos;
        while line_end < header_data.len()
            && header_data[line_end] != b'\r'
            && header_data[line_end] != b'\n'
        {
            line_end += 1;
        }

        let line = &header_data[pos..line_end];

        // Check "name:" prefix case-insensitively
        if line.len() > name_len && line[name_len] == b':' && line[..name_len].eq_ignore_ascii_case(name_bytes) {
            let mut start = name_len + 1;
            while start < line.len() && (line[start] == b' ' || line[start] == b'\t') {
                start += 1;
            }
            let mut end = line.len();
            while end > start && (line[end - 1] == b' ' || line[end - 1] == b'\t') {
                end -= 1;
            }
            // SAFETY: HTTP headers are ASCII per RFC 7230
            return Some(unsafe { std::str::from_utf8_unchecked(&line[start..end]) });
        }

        // Advance past CRLF
        pos = line_end;
        if pos < header_data.len() && header_data[pos] == b'\r' {
            pos += 1;
        }
        if pos < header_data.len() && header_data[pos] == b'\n' {
            pos += 1;
        }
    }

    None
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact_path_matching() {
        let mut rt = RouteTable::new();
        rt.add_http_route("example.com", HttpRouteRule {
            path_match_type: PathMatchType::Exact,
            path: "/foo".to_string(),
            header_matches: vec![],
            filters: vec![],
            backends: vec![Backend { socket_addr: "127.0.0.1:8001".parse().unwrap(), weight: 1 }],
        });
        rt.add_http_route("example.com", HttpRouteRule {
            path_match_type: PathMatchType::Prefix,
            path: "/foo".to_string(),
            header_matches: vec![],
            filters: vec![],
            backends: vec![Backend { socket_addr: "127.0.0.1:8002".parse().unwrap(), weight: 1 }],
        });

        // Exact match wins
        let rule = rt.find_http_route("example.com", "/foo", &[]).unwrap();
        assert_eq!(rule.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());

        // Prefix still works for sub-paths
        let rule = rt.find_http_route("example.com", "/foo/bar", &[]).unwrap();
        assert_eq!(rule.backends[0].socket_addr, "127.0.0.1:8002".parse().unwrap());
    }

    #[test]
    fn test_wildcard_host_matching() {
        let mut rt = RouteTable::new();
        rt.add_http_route("*.example.com", HttpRouteRule {
            path_match_type: PathMatchType::Prefix,
            path: "/".to_string(),
            header_matches: vec![],
            filters: vec![],
            backends: vec![Backend { socket_addr: "127.0.0.1:9001".parse().unwrap(), weight: 1 }],
        });
        rt.add_http_route("specific.example.com", HttpRouteRule {
            path_match_type: PathMatchType::Prefix,
            path: "/".to_string(),
            header_matches: vec![],
            filters: vec![],
            backends: vec![Backend { socket_addr: "127.0.0.1:9002".parse().unwrap(), weight: 1 }],
        });

        // Exact host takes priority
        let rule = rt.find_http_route("specific.example.com", "/", &[]).unwrap();
        assert_eq!(rule.backends[0].socket_addr, "127.0.0.1:9002".parse().unwrap());

        // Wildcard catches others
        let rule = rt.find_http_route("other.example.com", "/", &[]).unwrap();
        assert_eq!(rule.backends[0].socket_addr, "127.0.0.1:9001".parse().unwrap());

        // Non-matching returns error
        assert!(rt.find_http_route("example.org", "/", &[]).is_err());
    }

    #[test]
    fn test_header_matching() {
        let mut rt = RouteTable::new();
        rt.add_http_route("example.com", HttpRouteRule {
            path_match_type: PathMatchType::Prefix,
            path: "/".to_string(),
            header_matches: vec![HeaderMatch {
                name: "x-env".to_string(),
                value: "canary".to_string(),
            }],
            filters: vec![],
            backends: vec![Backend { socket_addr: "127.0.0.1:7001".parse().unwrap(), weight: 1 }],
        });
        rt.add_http_route("example.com", HttpRouteRule {
            path_match_type: PathMatchType::Prefix,
            path: "/".to_string(),
            header_matches: vec![],
            filters: vec![],
            backends: vec![Backend { socket_addr: "127.0.0.1:7002".parse().unwrap(), weight: 1 }],
        });

        let headers = b"X-Env: canary\r\nAccept: */*\r\n";

        // With matching header -> canary backend
        let rule = rt.find_http_route("example.com", "/", headers).unwrap();
        assert_eq!(rule.backends[0].socket_addr, "127.0.0.1:7001".parse().unwrap());

        // Without matching header -> fallback
        let rule = rt.find_http_route("example.com", "/", b"Accept: */*\r\n").unwrap();
        assert_eq!(rule.backends[0].socket_addr, "127.0.0.1:7002".parse().unwrap());
    }

    #[test]
    fn test_find_header_value() {
        let headers = b"Host: example.com\r\nX-Custom: hello\r\nContent-Type: text/plain\r\n";
        assert_eq!(find_header_value(headers, "host"), Some("example.com"));
        assert_eq!(find_header_value(headers, "x-custom"), Some("hello"));
        assert_eq!(find_header_value(headers, "content-type"), Some("text/plain"));
        assert_eq!(find_header_value(headers, "missing"), None);
    }

    #[test]
    fn test_weighted_backend() {
        let b1 = Backend { socket_addr: "127.0.0.1:8001".parse().unwrap(), weight: 3 };
        let b2 = Backend { socket_addr: "127.0.0.1:8002".parse().unwrap(), weight: 1 };
        let backends = vec![b1, b2];

        let mut selector = BackendSelector::new();
        let mut counts = [0u32; 2];
        // total weight = 4, cycle through enough to see distribution
        for _ in 0..400 {
            let idx = selector.select_weighted_backend(42, &backends);
            counts[idx] += 1;
        }
        // b1 (weight 3) should get ~75%, b2 (weight 1) should get ~25%
        assert_eq!(counts[0], 300);
        assert_eq!(counts[1], 100);
    }
}

/// Weighted round-robin backend selector
#[derive(Debug)]
pub struct BackendSelector {
    route_counters: FnvHashMap<u64, u64>,
}

impl BackendSelector {
    pub fn new() -> Self {
        Self {
            route_counters: FnvHashMap::default(),
        }
    }

    /// Select backend using weighted round-robin.
    /// Each call increments a counter; the counter modulo total weight
    /// determines which backend is selected based on cumulative weight ranges.
    pub fn select_weighted_backend(&mut self, route_hash: u64, backends: &[Backend]) -> usize {
        let total_weight: u64 = backends.iter().map(|b| b.weight as u64).sum();
        if total_weight == 0 {
            return 0;
        }

        let counter = self.route_counters.entry(route_hash).or_insert(0);
        let slot = *counter % total_weight;
        *counter = counter.wrapping_add(1);

        let mut cumulative = 0u64;
        for (i, backend) in backends.iter().enumerate() {
            cumulative += backend.weight as u64;
            if slot < cumulative {
                return i;
            }
        }

        backends.len() - 1
    }

    /// Simple round-robin (for equal-weight backends or non-HTTP routes)
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
