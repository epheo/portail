use anyhow::{anyhow, Result};
use fnv::FnvHashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

mod build;

#[cfg(test)]
mod tests;

/// A per-listener HTTP route scope.
/// Each Gateway listener produces its own scope with port + optional hostname.
/// Routes are only matched within the scope whose listener hostname matches the request.
#[derive(Debug, Clone)]
pub struct ListenerScope {
    /// Port this listener is bound to (redundant with the HashMap key; kept for Debug).
    #[allow(dead_code)]
    pub port: u16,
    /// Optional hostname constraint (None = catch-all for this port)
    pub listener_hostname: Option<String>,
    /// HTTP routes within this listener's scope
    pub http_routes: FnvHashMap<String, HostEntry>,
    pub wildcard_http_routes: FnvHashMap<String, HostEntry>,
    pub catch_all_http_routes: Option<HostEntry>,
}

impl ListenerScope {
    /// Check if a request Host header matches this listener's hostname constraint.
    /// Returns a match priority: higher = better match.
    /// Returns None if the host doesn't match this listener.
    ///
    /// Zero-allocation hot path: `request_host` is already lowercased by
    /// `find_http_route`, and `listener_hostname` was lowercased at
    /// construction.
    fn hostname_match_priority(&self, request_host: &str) -> Option<u32> {
        match &self.listener_hostname {
            None => Some(0), // Catch-all listener: lowest priority
            Some(lh) => {
                if let Some(parent) = lh.strip_prefix("*.") {
                    // Wildcard listener (e.g., *.example.com)
                    // More specific wildcards get higher priority:
                    // *.foo.example.com (len 15) > *.example.com (len 11)
                    if wildcard_covers(parent, request_host) {
                        Some(1 + parent.len() as u32) // Longer suffix = more specific = higher priority
                    } else {
                        None
                    }
                } else if request_host == lh {
                    // Exact listener hostname: highest priority
                    Some(u32::MAX)
                } else {
                    None
                }
            }
        }
    }

    /// Look up an HTTP route within this listener scope.
    fn lookup_http_route<'a>(
        &'a self,
        host: &str,
        method: &str,
        path: &str,
        header_data: &[u8],
        query_string: &str,
    ) -> Option<&'a HttpRouteRule> {
        // Try exact host first
        if let Some(host_entry) = self.http_routes.get(host) {
            if let Some(rule) = RouteTable::find_best_rule_match(
                host_entry,
                method,
                path,
                header_data,
                query_string,
            ) {
                return Some(rule);
            }
        }

        // Try wildcard: walk up domain labels (e.g. "foo.bar.example.com" ->
        // try "bar.example.com", then "example.com") to find *.example.com routes
        for parent in parent_domains(host) {
            if let Some(host_entry) = self.wildcard_http_routes.get(parent) {
                if let Some(rule) = RouteTable::find_best_rule_match(
                    host_entry,
                    method,
                    path,
                    header_data,
                    query_string,
                ) {
                    return Some(rule);
                }
            }
        }

        // Try catch-all routes (hostname="*", matches any host)
        if let Some(ref host_entry) = self.catch_all_http_routes {
            if let Some(rule) = RouteTable::find_best_rule_match(
                host_entry,
                method,
                path,
                header_data,
                query_string,
            ) {
                return Some(rule);
            }
        }

        None
    }
}

#[derive(Debug, Clone)]
pub struct RouteTable {
    /// Per-listener HTTP route scopes, indexed by port for O(1) lookup.
    /// Port 0 is a wildcard that matches any port (used in file-based configs / tests).
    pub listener_scopes: FnvHashMap<u16, Vec<ListenerScope>>,
    /// L4 routes remain port-based (no listener scoping needed)
    pub tcp_routes: FnvHashMap<u16, Vec<Backend>>,
    pub udp_routes: FnvHashMap<u16, Vec<Backend>>,
    /// TLS passthrough routes, scoped by listener port then SNI hostname.
    /// Port 0 is the any-port scope (file-based configs without gateway
    /// context). Without the port dimension, a TLSRoute attached to one
    /// listener would hijack matching SNI on every other port.
    pub tls_routes: FnvHashMap<u16, FnvHashMap<String, Vec<Backend>>>,
    /// Wildcard TLS passthrough routes, keyed by (port, parent domain):
    /// `*.example.com` is stored under "example.com".
    pub wildcard_tls_routes: FnvHashMap<u16, FnvHashMap<String, Vec<Backend>>>,
}

impl RouteTable {
    /// Find an HTTP route rule scoped by the server port and request's Host header.
    ///
    /// 1. Find all listener scopes matching the server port
    /// 2. Among those, find scopes matching the Host header (exact > wildcard > catch-all)
    /// 3. Within the best-matching scope, look up the route rule
    #[inline(always)]
    pub fn find_http_route<'a>(
        &'a self,
        host: &str,
        method: &str,
        path: &str,
        header_data: &[u8],
        query_string: &str,
        server_port: u16,
    ) -> Result<&'a HttpRouteRule> {
        // Normalize host to lowercase
        let host_bytes = host.as_bytes();
        let needs_lowercase = host_bytes.iter().any(|b| b.is_ascii_uppercase());

        // The rare mixed-case path allocates so the common path pays nothing:
        // hoisting a stack buffer out of the branch costs a 256-byte zeroing
        // on every request (measured +6ns in e2e benches).
        let host_lower;
        let host_key = if !needs_lowercase {
            host
        } else {
            let len = host_bytes.len().min(256);
            let mut buf = [0u8; 256];
            buf[..len].copy_from_slice(&host_bytes[..len]);
            buf[..len].make_ascii_lowercase();
            host_lower = std::str::from_utf8(&buf[..len]).unwrap_or(host).to_string();
            &host_lower
        };

        // O(1) port lookup, then iterate only matching scopes.
        // Also check port-0 wildcard scopes (file-based configs / tests).
        let port_scopes = self.listener_scopes.get(&server_port);
        let wildcard_scopes = if server_port != 0 {
            self.listener_scopes.get(&0)
        } else {
            None
        };

        // Phase 1: Find the highest hostname match priority among all matching scopes.
        let mut highest_priority: Option<u32> = None;

        for scope in port_scopes
            .into_iter()
            .chain(wildcard_scopes)
            .flat_map(|v| v.iter())
        {
            if let Some(priority) = scope.hostname_match_priority(host_key) {
                if highest_priority.is_none() || priority > highest_priority.unwrap() {
                    highest_priority = Some(priority);
                }
            }
        }

        let highest_priority = match highest_priority {
            Some(p) => p,
            None => {
                return Err(anyhow!(
                    "No HTTP route found for host={} path={} port={}",
                    host_key,
                    path,
                    server_port
                ))
            }
        };

        // Phase 2: Only search routes in scopes at the highest matching priority.
        // This ensures listener isolation — if `bar.example.com` matches `*.example.com`
        // (priority 1), routes from the catch-all listener (priority 0) are excluded.
        for scope in port_scopes
            .into_iter()
            .chain(wildcard_scopes)
            .flat_map(|v| v.iter())
        {
            if let Some(priority) = scope.hostname_match_priority(host_key) {
                if priority < highest_priority {
                    continue;
                }
                if let Some(rule) =
                    scope.lookup_http_route(host_key, method, path, header_data, query_string)
                {
                    return Ok(rule);
                }
            }
        }

        Err(anyhow!(
            "No HTTP route found for host={} path={} port={}",
            host_key,
            path,
            server_port
        ))
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
                    find_header_value(header_data, &hm.name).is_some_and(|v| hm.matcher.is_match(v))
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

    #[inline(always)]
    pub fn find_udp_backends(&self, server_port: u16) -> Result<&Vec<Backend>> {
        if let Some(backend_list) = self.udp_routes.get(&server_port) {
            return Ok(backend_list);
        }
        Err(anyhow!("No UDP route found for port {}", server_port))
    }

    /// Look up the TLS-route backends for an SNI hostname on a port: exact
    /// hostname first, then wildcard walking up domain labels (matching the
    /// HTTP wildcard semantics — `*.example.com` also covers
    /// `a.b.example.com`), each within the connection's port scope and then
    /// the any-port (0) scope.
    fn tls_route_backends(&self, sni: &str, server_port: u16) -> Option<&Vec<Backend>> {
        let sni_lower = sni.to_ascii_lowercase();
        let ports: &[u16] = if server_port == 0 {
            &[0]
        } else {
            &[server_port, 0]
        };
        for port in ports {
            if let Some(by_host) = self.tls_routes.get(port) {
                if let Some(backends) = by_host.get(&sni_lower) {
                    return Some(backends);
                }
            }
            if let Some(by_host) = self.wildcard_tls_routes.get(port) {
                for parent in parent_domains(&sni_lower) {
                    if let Some(backends) = by_host.get(parent) {
                        return Some(backends);
                    }
                }
            }
        }
        None
    }

    /// Resolve a TLS passthrough connection to a backend address.
    /// Checks SNI-based TLS routes first (exact, then wildcard), then falls
    /// back to port-based TCP routes. Selection is healthy weighted
    /// round-robin, same as the L4 path; `None` also covers the
    /// all-backends-unhealthy case (caller drops the connection).
    pub fn resolve_tls_passthrough(
        &self,
        sni: &str,
        server_port: u16,
        selector: &BackendSelector,
        health: &crate::proxy::health::HealthRegistry,
    ) -> Option<std::net::SocketAddr> {
        if let Some(backends) = self.tls_route_backends(sni, server_port) {
            // Counter keyed per (port, SNI) so each route rotates
            // independently; a collision only shares a counter.
            let key = {
                use std::hash::Hasher;
                let mut h = fnv::FnvHasher::default();
                h.write(sni.as_bytes());
                h.write_u16(server_port);
                h.finish()
            };
            return selector
                .select_l4_backend(key, backends, health)
                .map(|b| b.socket_addr);
        }

        // Fall back to port-based TCP routes, sharing the TCP path's counter.
        self.tcp_routes.get(&server_port).and_then(|backends| {
            selector
                .select_l4_backend(server_port as u64, backends, health)
                .map(|b| b.socket_addr)
        })
    }

    /// Check if a TLS passthrough route exists for this SNI hostname on this
    /// port. Used by the worker to dynamically decide between passthrough and
    /// termination when both share a port.
    pub fn has_tls_passthrough_route(&self, sni: &str, server_port: u16) -> bool {
        self.tls_route_backends(sni, server_port).is_some()
    }
}

pub type HostEntry = Vec<HttpRouteRule>;

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
    /// Weighted round-robin position for this rule, shared by all clones
    /// (`Clone` shares the `Arc`). Keying the counter to the rule itself —
    /// rather than a map keyed by request-path hash — bounds counter storage
    /// to the number of rules: high-cardinality paths (`/users/12345`, …)
    /// previously inserted one never-evicted map entry per distinct path.
    /// A table rebuild creates fresh rules and restarts the rotation.
    pub counter: Arc<std::sync::atomic::AtomicU64>,
    /// `portail_http_requests_total{host=...}` slot for this rule's route
    /// hostname, fetched from the metrics registry at insertion
    /// (`ListenerScope::add_route`) so the per-request cost is one relaxed
    /// add. Registry-backed: table swaps re-attach the SAME counter, so the
    /// series stays monotonic across reconciles and DNS refreshes.
    pub requests: Arc<std::sync::atomic::AtomicU64>,
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
            counter: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            // Placeholder until `add_route` binds the registry slot for the
            // route's hostname; a rule that never enters a table (tests)
            // counts into this unrendered cell.
            requests: Arc::new(std::sync::atomic::AtomicU64::new(0)),
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
        add: Arc<Vec<HttpHeader>>,
        set: Arc<Vec<HttpHeader>>,
        remove: Arc<Vec<String>>,
    },
    ResponseHeaderModifier {
        add: Arc<Vec<HttpHeader>>,
        set: Arc<Vec<HttpHeader>>,
        remove: Arc<Vec<String>>,
    },
    RequestRedirect {
        scheme: Option<String>,
        hostname: Option<String>,
        port: Option<u16>,
        path: Option<URLRewritePath>,
        status_code: u16,
    },
    URLRewrite {
        hostname: Option<String>,
        path: Option<URLRewritePath>,
    },
    RequestMirror {
        backend_addr: std::net::SocketAddr,
        /// Percentage of requests to mirror (0–100). Default 100 = mirror all.
        mirror_percent: u32,
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
    /// Per-backend filters (e.g. BackendRequestHeaderModifier)
    pub filters: Vec<HttpFilter>,
    /// Whether to use TLS when connecting to this backend
    pub use_tls: bool,
    /// Hostname for TLS SNI (the original DNS name before resolution)
    pub server_name: String,
}

/// True when `host` is a strict subdomain of `parent` on a label boundary
/// (`parent` is a wildcard's suffix, without the leading "*.").
#[inline]
pub fn wildcard_covers(parent: &str, host: &str) -> bool {
    host.ends_with(parent)
        && host.len() > parent.len()
        && host.as_bytes()[host.len() - parent.len() - 1] == b'.'
}

/// True when two (possibly `*.`-wildcard) hostname patterns share at least
/// one concrete hostname. Inputs must already be lowercase: Gateway API CRD
/// validation guarantees it for routes, config table build normalizes first.
/// Per spec `*.example.com` covers subdomains only, never `example.com`.
pub fn hostnames_intersect(a: &str, b: &str) -> bool {
    match (a.strip_prefix("*."), b.strip_prefix("*.")) {
        (None, None) => a == b,
        (Some(pa), None) => wildcard_covers(pa, b),
        (None, Some(pb)) => wildcard_covers(pb, a),
        // Nested wildcards overlap: *.sub.example.com is inside *.example.com,
        // and the wildcard pattern is itself a valid host under the wider suffix.
        (Some(pa), Some(pb)) => pa == pb || wildcard_covers(pa, b) || wildcard_covers(pb, a),
    }
}

/// Successive parent domains: "a.b.example.com" yields "b.example.com",
/// "example.com", "com".
#[inline]
fn parent_domains(host: &str) -> ParentDomains<'_> {
    ParentDomains(host)
}

/// Hand-rolled rather than iter::successors: the closure-based adapter did
/// not inline flat, costing measurable ns on the route-miss path.
struct ParentDomains<'a>(&'a str);

impl<'a> Iterator for ParentDomains<'a> {
    type Item = &'a str;

    #[inline]
    fn next(&mut self) -> Option<&'a str> {
        let dot = self.0.find('.')?;
        self.0 = &self.0[dot + 1..];
        Some(self.0)
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
        if line.len() > name_len
            && line[name_len] == b':'
            && line[..name_len].eq_ignore_ascii_case(name_bytes)
        {
            let mut start = name_len + 1;
            while start < line.len() && (line[start] == b' ' || line[start] == b'\t') {
                start += 1;
            }
            let mut end = line.len();
            while end > start && (line[end - 1] == b' ' || line[end - 1] == b'\t') {
                end -= 1;
            }
            return std::str::from_utf8(&line[start..end]).ok();
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

/// Round-robin backend selector for L4 (TCP/UDP) routes.
///
/// Uses DashMap for lock-free per-port counters, allowing `&self` access
/// without an outer Mutex. Bounded: keys are listener ports. HTTP rules carry
/// their own counter (`HttpRouteRule::counter`) instead — keying HTTP
/// selection by request-path hash grew this map without bound on
/// high-cardinality paths.
#[derive(Debug, Default)]
pub struct BackendSelector {
    route_counters: dashmap::DashMap<u64, std::sync::atomic::AtomicU64, fnv::FnvBuildHasher>,
}

impl BackendSelector {
    pub fn new() -> Self {
        Self {
            route_counters: dashmap::DashMap::with_hasher(Default::default()),
        }
    }

    /// Atomically fetch-and-increment the counter for a route hash.
    #[inline(always)]
    fn next_counter(&self, route_hash: u64) -> u64 {
        let entry = self
            .route_counters
            .entry(route_hash)
            .or_insert_with(|| std::sync::atomic::AtomicU64::new(0));
        entry
            .value()
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Health-aware weighted backend selection.
    ///
    /// Weight-0 backends receive no traffic (Gateway API semantics), matching
    /// `select_l4_backend`. Returns `None` when no healthy backend with
    /// positive weight exists (caller should send 503).
    ///
    /// Fast path: every positive-weight backend healthy, reuse pre-computed
    /// `cumulative_weights` (a weight-0 backend's prefix sum equals its
    /// predecessor's, so `partition_point` can never land on it).
    /// Slow path: O(n) filter + walk, taken only during partial failures.
    pub fn select_healthy_weighted_backend(
        &self,
        rule: &HttpRouteRule,
        health: &crate::proxy::health::HealthRegistry,
    ) -> Option<usize> {
        let backends = &rule.backends;

        if backends.len() == 1 {
            let b = &backends[0];
            return (b.weight > 0 && health.is_healthy(&b.socket_addr)).then_some(0);
        }

        let mut healthy_weight: u64 = 0;
        for b in backends {
            if b.weight > 0 && health.is_healthy(&b.socket_addr) {
                healthy_weight += b.weight as u64;
            }
        }

        // Covers empty, all-unhealthy, and all-weight-zero: modulo below must
        // never see a zero divisor (panic = abort kills the data plane).
        if healthy_weight == 0 {
            return None;
        }

        let counter = rule
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        if healthy_weight == rule.total_weight {
            let slot = counter % rule.total_weight;
            return Some(rule.cumulative_weights.partition_point(|&cw| cw <= slot));
        }

        // Slow path — some positive-weight backends are unhealthy
        let slot = counter % healthy_weight;

        if let Some(i) = nth_healthy_weighted(backends, health, slot) {
            return Some(i);
        }

        // Only reachable if health flipped between the sum and the walk.
        backends
            .iter()
            .position(|b| b.weight > 0 && health.is_healthy(&b.socket_addr))
    }

    /// Health-aware weighted round-robin over an L4 backend list, keyed by
    /// listener port. Weight-0 backends receive no traffic (Gateway API
    /// semantics). Returns `None` when no healthy backend with positive
    /// weight exists.
    pub fn select_l4_backend<'a>(
        &self,
        key: u64,
        backends: &'a [Backend],
        health: &crate::proxy::health::HealthRegistry,
    ) -> Option<&'a Backend> {
        let mut healthy_weight: u64 = 0;
        for b in backends {
            if health.is_healthy(&b.socket_addr) {
                healthy_weight += b.weight as u64;
            }
        }
        if healthy_weight == 0 {
            return None;
        }
        let slot = self.next_counter(key) % healthy_weight;
        nth_healthy_weighted(backends, health, slot).map(|i| &backends[i])
    }
}

/// Walk to the healthy positive-weight backend owning weighted slot `slot`.
/// `None` only when health flipped between the caller's weight sum and this
/// walk.
#[inline]
fn nth_healthy_weighted(
    backends: &[Backend],
    health: &crate::proxy::health::HealthRegistry,
    slot: u64,
) -> Option<usize> {
    let mut cumulative: u64 = 0;
    for (i, b) in backends.iter().enumerate() {
        if b.weight == 0 || !health.is_healthy(&b.socket_addr) {
            continue;
        }
        cumulative += b.weight as u64;
        if cumulative > slot {
            return Some(i);
        }
    }
    None
}
