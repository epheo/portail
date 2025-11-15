use std::time::Duration;
use fnv::FnvHashMap;
use serde::{Serialize, Deserialize};
use anyhow::{anyhow, Result};

#[derive(Debug, Clone)]
pub struct RouteTable {
    pub http_routes: FnvHashMap<String, HostEntry>,
    pub wildcard_http_routes: FnvHashMap<String, HostEntry>,
    pub tcp_routes: FnvHashMap<u16, Vec<Backend>>,
    pub udp_routes: FnvHashMap<u16, Vec<Backend>>,
    pub tls_routes: FnvHashMap<String, Vec<Backend>>,
    pub wildcard_tls_routes: FnvHashMap<String, Vec<Backend>>,
}

impl RouteTable {
    pub fn new() -> Self {
        Self {
            http_routes: FnvHashMap::with_capacity_and_hasher(128, Default::default()),
            wildcard_http_routes: FnvHashMap::with_capacity_and_hasher(32, Default::default()),
            tcp_routes: FnvHashMap::with_capacity_and_hasher(32, Default::default()),
            udp_routes: FnvHashMap::with_capacity_and_hasher(32, Default::default()),
            tls_routes: FnvHashMap::with_capacity_and_hasher(16, Default::default()),
            wildcard_tls_routes: FnvHashMap::with_capacity_and_hasher(8, Default::default()),
        }
    }

    /// Find an HTTP route rule for host, method, path, headers, and query string.
    /// Tries exact host first, then wildcard. Within a host entry, exact path
    /// beats prefix, then longest prefix wins. All match conditions are AND-combined.
    #[inline(always)]
    pub fn find_http_route<'a>(
        &'a self,
        host: &str,
        method: &str,
        path: &str,
        header_data: &[u8],
        query_string: &str,
    ) -> Result<&'a HttpRouteRule> {
        let mut buf = [0u8; 256];
        let host_bytes = host.as_bytes();
        let len = host_bytes.len().min(256);
        buf[..len].copy_from_slice(&host_bytes[..len]);
        buf[..len].make_ascii_lowercase();
        let key = std::str::from_utf8(&buf[..len]).unwrap_or(host);

        // Try exact host first
        if let Some(host_entry) = self.http_routes.get(key) {
            if let Some(rule) = Self::find_best_rule_match(&host_entry.rules, method, path, header_data, query_string) {
                return Ok(rule);
            }
        }

        // Try wildcard: strip first label (e.g. "foo.example.com" -> "example.com")
        if let Some(dot_pos) = key.find('.') {
            let parent = &key[dot_pos + 1..];
            if let Some(host_entry) = self.wildcard_http_routes.get(parent) {
                if let Some(rule) = Self::find_best_rule_match(&host_entry.rules, method, path, header_data, query_string) {
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

    /// Match rules: method first (fastest reject), then exact path before longest prefix,
    /// then headers, then query params. All conditions are AND-combined.
    #[inline(always)]
    fn find_best_rule_match<'a>(
        rules: &'a [HttpRouteRule],
        method: &str,
        path: &str,
        header_data: &[u8],
        query_string: &str,
    ) -> Option<&'a HttpRouteRule> {
        let path_bytes = path.as_bytes();

        for rule in rules {
            // Method match — single comparison, rejects early
            if let Some(ref required) = rule.method_match {
                if !required.eq_ignore_ascii_case(method) {
                    continue;
                }
            }

            let path_matches = match rule.path_match_type {
                PathMatchType::Exact => path_bytes == rule.path.as_bytes(),
                PathMatchType::RegularExpression => {
                    rule.path_regex.as_ref().is_some_and(|re| re.is_match(path))
                }
                PathMatchType::Prefix => {
                    let prefix_bytes = rule.path.as_bytes();
                    let prefix_len = prefix_bytes.len();
                    prefix_bytes.is_empty()
                        || (path_bytes.len() >= prefix_len
                            && path_bytes[..prefix_len] == *prefix_bytes
                            && (path_bytes.len() == prefix_len
                                || path_bytes[prefix_len] == b'/'
                                || prefix_bytes[prefix_len - 1] == b'/'))
                }
            };

            if !path_matches {
                continue;
            }

            if !rule.header_matches.is_empty() {
                if header_data.is_empty() {
                    continue;
                }
                let all_match = rule.header_matches.iter().all(|hm| {
                    find_header_value(header_data, &hm.name)
                        .is_some_and(|v| hm.matcher.is_match(v))
                });
                if !all_match {
                    continue;
                }
            }

            // Query param matches — only parsed when rule requires them
            if !rule.query_param_matches.is_empty() {
                let all_match = rule.query_param_matches.iter().all(|qm| {
                    find_query_param_value(query_string, &qm.name)
                        .is_some_and(|v| qm.matcher.is_match(v))
                });
                if !all_match {
                    continue;
                }
            }

            return Some(rule);
        }
        None
    }

    pub fn add_http_route(&mut self, host: &str, mut rule: HttpRouteRule) {
        // Pre-compute metadata to avoid per-request work
        rule.has_filters = !rule.filters.is_empty();
        let mut cumulative = 0u64;
        rule.cumulative_weights = rule.backends.iter().map(|b| {
            cumulative += b.weight as u64;
            cumulative
        }).collect();
        rule.total_weight = cumulative;

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

        // Sort: exact > regex > prefix, then by path length desc
        host_entry.rules.sort_by(|a, b| {
            fn rank(t: &PathMatchType) -> u8 {
                match t { PathMatchType::Exact => 0, PathMatchType::RegularExpression => 1, PathMatchType::Prefix => 2 }
            }
            let r = rank(&a.path_match_type).cmp(&rank(&b.path_match_type));
            if r != std::cmp::Ordering::Equal { return r; }
            b.path.len().cmp(&a.path.len())
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

    pub fn add_tls_route(&mut self, hostname: &str, backends: Vec<Backend>) {
        let host_lower = hostname.to_ascii_lowercase();
        if let Some(stripped) = host_lower.strip_prefix("*.") {
            self.wildcard_tls_routes.insert(stripped.to_string(), backends);
        } else {
            self.tls_routes.insert(host_lower, backends);
        }
    }

    /// Resolve a TLS passthrough connection to a backend address.
    /// Checks SNI-based TLS routes first (exact, then wildcard), then falls
    /// back to port-based TCP routes.
    pub fn resolve_tls_passthrough(&self, sni: &str, server_port: u16) -> Option<std::net::SocketAddr> {
        let sni_lower = sni.to_ascii_lowercase();

        // Exact SNI match
        if let Some(backends) = self.tls_routes.get(&sni_lower) {
            return backends.first().map(|b| b.socket_addr);
        }

        // Wildcard SNI match: "foo.example.com" -> check "example.com"
        if let Some(dot_pos) = sni_lower.find('.') {
            let parent = &sni_lower[dot_pos + 1..];
            if let Some(backends) = self.wildcard_tls_routes.get(parent) {
                return backends.first().map(|b| b.socket_addr);
            }
        }

        // Fall back to port-based TCP routes
        self.tcp_routes.get(&server_port)
            .and_then(|backends| backends.first())
            .map(|b| b.socket_addr)
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
    RegularExpression,
}

#[derive(Debug, Clone)]
pub struct HttpRouteRule {
    pub method_match: Option<String>,
    pub path_match_type: PathMatchType,
    pub path: String,
    pub path_regex: Option<regex::Regex>,
    pub header_matches: Vec<HeaderMatch>,
    pub query_param_matches: Vec<QueryParamMatch>,
    pub filters: Vec<HttpFilter>,
    pub backends: Vec<Backend>,
    pub request_timeout: Option<Duration>,
    pub backend_request_timeout: Option<Duration>,
    /// Pre-computed at add_http_route time to skip filter iteration on hot path
    pub has_filters: bool,
    /// Pre-computed sum of backend weights for O(1) access
    pub total_weight: u64,
    /// Pre-computed prefix sums for O(log n) binary search in select_weighted_backend
    pub cumulative_weights: Vec<u64>,
}

impl HttpRouteRule {
    pub fn new(
        path_match_type: PathMatchType,
        path: String,
        header_matches: Vec<HeaderMatch>,
        query_param_matches: Vec<QueryParamMatch>,
        filters: Vec<HttpFilter>,
        backends: Vec<Backend>,
    ) -> Self {
        Self {
            method_match: None,
            path_match_type,
            path,
            path_regex: None,
            header_matches,
            query_param_matches,
            filters,
            backends,
            request_timeout: None,
            backend_request_timeout: None,
            has_filters: false,
            total_weight: 0,
            cumulative_weights: vec![],
        }
    }

    pub fn with_method(mut self, method: Option<String>) -> Self {
        self.method_match = method;
        self
    }
}

#[derive(Debug, Clone)]
pub enum ValueMatcher {
    Exact(String),
    Regex(regex::Regex),
}

impl ValueMatcher {
    #[inline]
    pub fn is_match(&self, value: &str) -> bool {
        match self {
            ValueMatcher::Exact(expected) => value == expected,
            ValueMatcher::Regex(re) => re.is_match(value),
        }
    }
}

#[derive(Debug, Clone)]
pub struct HeaderMatch {
    /// Lowercase header name
    pub name: String,
    pub matcher: ValueMatcher,
}

#[derive(Debug, Clone)]
pub struct QueryParamMatch {
    pub name: String,
    pub matcher: ValueMatcher,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Zero-allocation query parameter value lookup.
/// Returns the value for the first matching `name=value` pair.
#[inline]
pub fn find_query_param_value<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    if query.is_empty() {
        return None;
    }
    for pair in query.split('&') {
        if let Some(eq_pos) = pair.find('=') {
            if &pair[..eq_pos] == name {
                return Some(&pair[eq_pos + 1..]);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query_string_contains_param(query: &str, name: &str, value: &str) -> bool {
        find_query_param_value(query, name).is_some_and(|v| v == value)
    }

    fn backend(port: u16) -> Backend {
        Backend { socket_addr: format!("127.0.0.1:{}", port).parse().unwrap(), weight: 1 }
    }

    fn rule(path_type: PathMatchType, path: &str, backends: Vec<Backend>) -> HttpRouteRule {
        HttpRouteRule::new(path_type, path.to_string(), vec![], vec![], vec![], backends)
    }

    #[test]
    fn test_exact_path_matching() {
        let mut rt = RouteTable::new();
        rt.add_http_route("example.com", rule(PathMatchType::Exact, "/foo", vec![backend(8001)]));
        rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/foo", vec![backend(8002)]));

        let r = rt.find_http_route("example.com", "GET", "/foo", &[], "").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());

        let r = rt.find_http_route("example.com", "GET", "/foo/bar", &[], "").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8002".parse().unwrap());
    }

    #[test]
    fn test_wildcard_host_matching() {
        let mut rt = RouteTable::new();
        rt.add_http_route("*.example.com", rule(PathMatchType::Prefix, "/", vec![backend(9001)]));
        rt.add_http_route("specific.example.com", rule(PathMatchType::Prefix, "/", vec![backend(9002)]));

        let r = rt.find_http_route("specific.example.com", "GET", "/", &[], "").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:9002".parse().unwrap());

        let r = rt.find_http_route("other.example.com", "GET", "/", &[], "").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:9001".parse().unwrap());

        assert!(rt.find_http_route("example.org", "GET", "/", &[], "").is_err());
    }

    #[test]
    fn test_header_matching() {
        let mut rt = RouteTable::new();
        rt.add_http_route("example.com", HttpRouteRule::new(
            PathMatchType::Prefix, "/".to_string(),
            vec![HeaderMatch { name: "x-env".to_string(), matcher: ValueMatcher::Exact("canary".to_string()) }],
            vec![], vec![],
            vec![backend(7001)],
        ));
        rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", vec![backend(7002)]));

        let headers = b"X-Env: canary\r\nAccept: */*\r\n";
        let r = rt.find_http_route("example.com", "GET", "/", headers, "").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:7001".parse().unwrap());

        let r = rt.find_http_route("example.com", "GET", "/", b"Accept: */*\r\n", "").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:7002".parse().unwrap());
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
    fn test_prefix_boundary_no_false_extension() {
        let mut rt = RouteTable::new();
        rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/foo", vec![backend(8001)]));
        rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", vec![backend(8002)]));

        // /foo/bar matches prefix /foo (boundary at /)
        let r = rt.find_http_route("example.com", "GET", "/foo/bar", &[], "").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());

        // /foobar must NOT match prefix /foo — falls through to /
        let r = rt.find_http_route("example.com", "GET", "/foobar", &[], "").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8002".parse().unwrap());

        // /foo exactly matches prefix /foo
        let r = rt.find_http_route("example.com", "GET", "/foo", &[], "").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());
    }

    #[test]
    fn test_prefix_boundary_trailing_slash() {
        let mut rt = RouteTable::new();
        rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/foo/", vec![backend(8001)]));
        rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", vec![backend(8002)]));

        // /foo/bar matches prefix /foo/ (prefix ends with /)
        let r = rt.find_http_route("example.com", "GET", "/foo/bar", &[], "").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());

        // /foo/ matches prefix /foo/
        let r = rt.find_http_route("example.com", "GET", "/foo/", &[], "").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());
    }

    #[test]
    fn test_weighted_backend() {
        let mut rt = RouteTable::new();
        rt.add_http_route("test.com", HttpRouteRule::new(
            PathMatchType::Prefix, "/".to_string(), vec![], vec![], vec![],
            vec![
                Backend { socket_addr: "127.0.0.1:8001".parse().unwrap(), weight: 3 },
                Backend { socket_addr: "127.0.0.1:8002".parse().unwrap(), weight: 1 },
            ],
        ));
        let r = rt.find_http_route("test.com", "GET", "/", &[], "").unwrap();

        let mut selector = BackendSelector::new();
        let mut counts = [0u32; 2];
        for _ in 0..400 {
            let idx = selector.select_weighted_backend(42, r);
            counts[idx] += 1;
        }
        assert_eq!(counts[0], 300);
        assert_eq!(counts[1], 100);
    }

    #[test]
    fn test_method_matching() {
        let mut rt = RouteTable::new();
        rt.add_http_route("example.com",
            rule(PathMatchType::Prefix, "/", vec![backend(8001)]).with_method(Some("POST".to_string())));
        rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", vec![backend(8002)]));

        // POST matches the method-constrained rule
        let r = rt.find_http_route("example.com", "POST", "/", &[], "").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());

        // GET skips the method-constrained rule, hits fallback
        let r = rt.find_http_route("example.com", "GET", "/", &[], "").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8002".parse().unwrap());

        // Case-insensitive
        let r = rt.find_http_route("example.com", "post", "/", &[], "").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());
    }

    #[test]
    fn test_query_param_matching() {
        let mut rt = RouteTable::new();
        rt.add_http_route("example.com", HttpRouteRule::new(
            PathMatchType::Prefix, "/".to_string(), vec![],
            vec![QueryParamMatch { name: "version".to_string(), matcher: ValueMatcher::Exact("2".to_string()) }],
            vec![], vec![backend(8001)],
        ));
        rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", vec![backend(8002)]));

        // Matching query param
        let r = rt.find_http_route("example.com", "GET", "/", &[], "version=2").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());

        // Missing query param -> fallback
        let r = rt.find_http_route("example.com", "GET", "/", &[], "").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8002".parse().unwrap());

        // Wrong value -> fallback
        let r = rt.find_http_route("example.com", "GET", "/", &[], "version=1").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8002".parse().unwrap());

        // Multiple params, one matches
        let r = rt.find_http_route("example.com", "GET", "/", &[], "foo=bar&version=2").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());
    }

    #[test]
    fn test_multiple_query_params_and_logic() {
        let mut rt = RouteTable::new();
        rt.add_http_route("example.com", HttpRouteRule::new(
            PathMatchType::Prefix, "/".to_string(), vec![],
            vec![
                QueryParamMatch { name: "a".to_string(), matcher: ValueMatcher::Exact("1".to_string()) },
                QueryParamMatch { name: "b".to_string(), matcher: ValueMatcher::Exact("2".to_string()) },
            ],
            vec![], vec![backend(8001)],
        ));
        rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", vec![backend(8002)]));

        // Both params present
        let r = rt.find_http_route("example.com", "GET", "/", &[], "a=1&b=2").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());

        // Only one param -> fallback
        let r = rt.find_http_route("example.com", "GET", "/", &[], "a=1").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8002".parse().unwrap());
    }

    #[test]
    fn test_combined_method_path_header_query() {
        let mut rt = RouteTable::new();
        rt.add_http_route("example.com", HttpRouteRule::new(
            PathMatchType::Prefix, "/api".to_string(),
            vec![HeaderMatch { name: "x-env".to_string(), matcher: ValueMatcher::Exact("prod".to_string()) }],
            vec![QueryParamMatch { name: "v".to_string(), matcher: ValueMatcher::Exact("2".to_string()) }],
            vec![],
            vec![backend(8001)],
        ).with_method(Some("POST".to_string())));
        rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", vec![backend(8002)]));

        let headers = b"X-Env: prod\r\n";

        // All match
        let r = rt.find_http_route("example.com", "POST", "/api/users", headers, "v=2").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());

        // Wrong method -> fallback
        let r = rt.find_http_route("example.com", "GET", "/api/users", headers, "v=2").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8002".parse().unwrap());

        // Wrong query -> fallback
        let r = rt.find_http_route("example.com", "POST", "/api/users", headers, "v=1").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8002".parse().unwrap());
    }

    #[test]
    fn test_query_string_contains_param() {
        assert!(query_string_contains_param("a=1&b=2", "a", "1"));
        assert!(query_string_contains_param("a=1&b=2", "b", "2"));
        assert!(!query_string_contains_param("a=1&b=2", "c", "3"));
        assert!(!query_string_contains_param("", "a", "1"));
        assert!(!query_string_contains_param("a=1", "a", "2"));
    }

    #[test]
    fn test_regex_header_matching() {
        let mut rt = RouteTable::new();
        rt.add_http_route("example.com", HttpRouteRule::new(
            PathMatchType::Prefix, "/".to_string(),
            vec![HeaderMatch { name: "x-env".to_string(), matcher: ValueMatcher::Regex(regex::Regex::new("^(canary|staging)$").unwrap()) }],
            vec![], vec![],
            vec![backend(7001)],
        ));
        rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", vec![backend(7002)]));

        let headers = b"X-Env: canary\r\n";
        let r = rt.find_http_route("example.com", "GET", "/", headers, "").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:7001".parse().unwrap());

        let headers = b"X-Env: staging\r\n";
        let r = rt.find_http_route("example.com", "GET", "/", headers, "").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:7001".parse().unwrap());

        let headers = b"X-Env: production\r\n";
        let r = rt.find_http_route("example.com", "GET", "/", headers, "").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:7002".parse().unwrap());
    }

    #[test]
    fn test_regex_query_param_matching() {
        let mut rt = RouteTable::new();
        rt.add_http_route("example.com", HttpRouteRule::new(
            PathMatchType::Prefix, "/".to_string(), vec![],
            vec![QueryParamMatch { name: "version".to_string(), matcher: ValueMatcher::Regex(regex::Regex::new(r"^\d+$").unwrap()) }],
            vec![], vec![backend(8001)],
        ));
        rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", vec![backend(8002)]));

        let r = rt.find_http_route("example.com", "GET", "/", &[], "version=2").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());

        let r = rt.find_http_route("example.com", "GET", "/", &[], "version=abc").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8002".parse().unwrap());
    }

    #[test]
    fn test_regex_path_matching() {
        let mut rt = RouteTable::new();
        let mut regex_rule = rule(PathMatchType::RegularExpression, r"^/api/v\d+/users$", vec![backend(8001)]);
        regex_rule.path_regex = Some(regex::Regex::new(r"^/api/v\d+/users$").unwrap());
        rt.add_http_route("example.com", regex_rule);
        rt.add_http_route("example.com", rule(PathMatchType::Prefix, "/", vec![backend(8002)]));

        let r = rt.find_http_route("example.com", "GET", "/api/v1/users", &[], "").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());

        let r = rt.find_http_route("example.com", "GET", "/api/v2/users", &[], "").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8001".parse().unwrap());

        // No match — falls to prefix
        let r = rt.find_http_route("example.com", "GET", "/api/v1/posts", &[], "").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8002".parse().unwrap());
    }

    #[test]
    fn test_exact_beats_regex_precedence() {
        let mut rt = RouteTable::new();
        let mut regex_rule = rule(PathMatchType::RegularExpression, r"^/foo.*", vec![backend(8001)]);
        regex_rule.path_regex = Some(regex::Regex::new(r"^/foo.*").unwrap());
        rt.add_http_route("example.com", regex_rule);
        rt.add_http_route("example.com", rule(PathMatchType::Exact, "/foo", vec![backend(8002)]));

        // Exact match wins over regex
        let r = rt.find_http_route("example.com", "GET", "/foo", &[], "").unwrap();
        assert_eq!(r.backends[0].socket_addr, "127.0.0.1:8002".parse().unwrap());
    }

    #[test]
    fn test_resolve_tls_passthrough_found() {
        let mut rt = RouteTable::new();
        rt.add_tcp_route(8443, vec![backend(9001)]);

        let addr = rt.resolve_tls_passthrough("example.com", 8443);
        assert_eq!(addr, Some("127.0.0.1:9001".parse().unwrap()));
    }

    #[test]
    fn test_resolve_tls_passthrough_no_route() {
        let rt = RouteTable::new();
        assert!(rt.resolve_tls_passthrough("example.com", 8443).is_none());
    }

    #[test]
    fn test_tls_route_sni_exact_match() {
        let mut rt = RouteTable::new();
        rt.add_tls_route("secure.example.com", vec![backend(9001)]);
        rt.add_tcp_route(8443, vec![backend(9999)]); // fallback

        let addr = rt.resolve_tls_passthrough("secure.example.com", 8443);
        assert_eq!(addr, Some("127.0.0.1:9001".parse().unwrap()));
    }

    #[test]
    fn test_tls_route_sni_wildcard_match() {
        let mut rt = RouteTable::new();
        rt.add_tls_route("*.example.com", vec![backend(9002)]);
        rt.add_tcp_route(8443, vec![backend(9999)]); // fallback

        let addr = rt.resolve_tls_passthrough("foo.example.com", 8443);
        assert_eq!(addr, Some("127.0.0.1:9002".parse().unwrap()));
    }

    #[test]
    fn test_tls_route_sni_fallback_to_tcp() {
        let mut rt = RouteTable::new();
        rt.add_tls_route("secure.example.com", vec![backend(9001)]);
        rt.add_tcp_route(8443, vec![backend(9999)]);

        // No SNI match -> falls back to TCP port route
        let addr = rt.resolve_tls_passthrough("other.example.org", 8443);
        assert_eq!(addr, Some("127.0.0.1:9999".parse().unwrap()));
    }
}

/// Weighted round-robin backend selector
#[derive(Debug, Default)]
pub struct BackendSelector {
    route_counters: FnvHashMap<u64, u64>,
}

impl BackendSelector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Select backend using weighted round-robin with pre-computed weights.
    /// Uses O(log n) binary search on pre-computed cumulative_weights.
    #[inline(always)]
    pub fn select_weighted_backend(
        &mut self,
        route_hash: u64,
        rule: &HttpRouteRule,
    ) -> usize {
        if rule.backends.len() <= 1 {
            return 0;
        }
        if rule.total_weight == 0 {
            return 0;
        }

        let counter = self.route_counters.entry(route_hash).or_insert(0);
        let slot = *counter % rule.total_weight;
        *counter = counter.wrapping_add(1);

        rule.cumulative_weights.partition_point(|&cw| cw <= slot)
    }

    /// Health-aware weighted backend selection.
    ///
    /// Returns `None` when all backends are unhealthy (caller should send 503).
    ///
    /// Three-branch logic:
    /// - **All healthy** → fast path: reuse pre-computed `cumulative_weights` (zero overhead)
    /// - **Some unhealthy** → slow path: filter + linear walk with manual weight sum
    /// - **All unhealthy** → `None`
    ///
    /// The slow path is O(n) on backend count which is acceptable because
    /// n is typically 2–5 and this path is only taken during partial failures.
    pub fn select_healthy_weighted_backend(
        &mut self,
        route_hash: u64,
        rule: &HttpRouteRule,
        health: &crate::health::HealthRegistry,
    ) -> Option<usize> {
        let backends = &rule.backends;

        if backends.is_empty() {
            return None;
        }

        if backends.len() == 1 {
            // Single backend: always return it so the worker can attempt a
            // connection and record the failure — there is no alternative anyway.
            return Some(0);
        }

        // One linear pass: count healthy backends and sum their weights.
        let mut healthy_weight: u64 = 0;
        let mut healthy_count: usize = 0;
        for b in backends {
            if health.is_healthy(&b.socket_addr) {
                healthy_weight += b.weight as u64;
                healthy_count += 1;
            }
        }

        if healthy_count == 0 {
            return None;
        }

        let counter = self.route_counters.entry(route_hash).or_insert(0);

        if healthy_count == backends.len() {
            // Fast path — all healthy, use pre-computed cumulative_weights.
            if rule.total_weight == 0 {
                return Some(0);
            }
            let slot = *counter % rule.total_weight;
            *counter = counter.wrapping_add(1);
            return Some(rule.cumulative_weights.partition_point(|&cw| cw <= slot));
        }

        // Slow path — some backends are unhealthy; recompute distribution over
        // the healthy subset only.
        let slot = *counter % healthy_weight;
        *counter = counter.wrapping_add(1);

        let mut cumulative: u64 = 0;
        for (i, b) in backends.iter().enumerate() {
            if !health.is_healthy(&b.socket_addr) {
                continue;
            }
            cumulative += b.weight as u64;
            if cumulative > slot {
                return Some(i);
            }
        }

        // Unreachable given healthy_weight > 0, but fall back to first healthy
        // backend rather than panic.
        backends.iter().position(|b| health.is_healthy(&b.socket_addr))
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
