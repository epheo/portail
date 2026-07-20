use anyhow::{anyhow, Result};
use fnv::FnvHashMap;

use super::*;

impl ListenerScope {
    /// `listener_hostname` is normalized to lowercase at construction so the
    /// per-request match never allocates.
    pub fn new(port: u16, listener_hostname: Option<String>) -> Self {
        Self {
            port,
            listener_hostname: listener_hostname.map(|h| h.to_ascii_lowercase()),
            http_routes: FnvHashMap::with_capacity_and_hasher(32, Default::default()),
            wildcard_http_routes: FnvHashMap::with_capacity_and_hasher(8, Default::default()),
            catch_all_http_routes: None,
        }
    }

    /// Add a route to this listener scope.
    fn add_route(&mut self, route_host: &str, mut rule: HttpRouteRule) {
        // Pre-compute metadata to avoid per-request work
        rule.has_filters =
            !rule.filters.is_empty() || rule.backends.iter().any(|b| !b.filters.is_empty());
        let mut cumulative = 0u64;
        rule.cumulative_weights = rule
            .backends
            .iter()
            .map(|b| {
                cumulative += b.weight as u64;
                cumulative
            })
            .collect();
        rule.total_weight = cumulative;

        let host_lower = route_host.to_ascii_lowercase();

        // Bind the per-hostname request counter (the route's CONFIGURED
        // hostname — bounded label cardinality — never the request's Host).
        rule.requests = crate::metrics::host_requests_counter(&host_lower);

        // Catch-all hostname "*" matches any host (stored separately)
        if host_lower == "*" {
            let host_entry = self
                .catch_all_http_routes
                .get_or_insert_with(|| Vec::with_capacity(8));
            host_entry.push(rule);
            RouteTable::sort_rules(host_entry);
            return;
        }

        // Wildcard hosts (*.example.com) are stored by their parent domain
        let (map, key) = if let Some(stripped) = host_lower.strip_prefix("*.") {
            (&mut self.wildcard_http_routes, stripped.to_string())
        } else {
            (&mut self.http_routes, host_lower)
        };

        let host_entry = map.entry(key).or_insert_with(|| Vec::with_capacity(8));

        host_entry.push(rule);
        RouteTable::sort_rules(host_entry);
    }
}

impl RouteTable {
    pub fn new() -> Self {
        Self {
            listener_scopes: FnvHashMap::with_capacity_and_hasher(16, Default::default()),
            tcp_routes: FnvHashMap::with_capacity_and_hasher(32, Default::default()),
            udp_routes: FnvHashMap::with_capacity_and_hasher(32, Default::default()),
            tls_routes: FnvHashMap::with_capacity_and_hasher(16, Default::default()),
            wildcard_tls_routes: FnvHashMap::with_capacity_and_hasher(8, Default::default()),
        }
    }

    /// Order-independent fingerprint of every backend socket address in the table.
    ///
    /// The DNS-refresh task re-resolves and rebuilds the table on an interval but
    /// swaps it only when this signature changes, so a churny resolver that merely
    /// reorders records (or returns an unchanged set) costs nothing. `wrapping_add`
    /// (commutative, and unlike XOR does not cancel duplicate addresses) makes the
    /// fold independent of record order across rules and route kinds.
    pub fn dns_signature(&self) -> u64 {
        let http_backends = self.listener_scopes.values().flatten().flat_map(|scope| {
            scope
                .http_routes
                .values()
                .chain(scope.wildcard_http_routes.values())
                .chain(scope.catch_all_http_routes.iter())
                .flat_map(|he| he.iter())
                .flat_map(|rule| rule.backends.iter())
        });
        let l4_backends = self
            .tcp_routes
            .values()
            .chain(self.udp_routes.values())
            .chain(
                self.tls_routes
                    .values()
                    .flat_map(|by_host| by_host.values()),
            )
            .chain(
                self.wildcard_tls_routes
                    .values()
                    .flat_map(|by_host| by_host.values()),
            )
            .flatten();
        http_backends
            .chain(l4_backends)
            .map(|b| backend_addr_hash(&b.socket_addr))
            .fold(0u64, |acc, h| acc.wrapping_add(h))
    }

    /// Add an HTTP route scoped to a specific listener (port + hostname).
    /// If no ListenerScope exists for this (port, hostname) pair, creates one.
    pub fn add_http_route_for_listener(
        &mut self,
        listener_port: u16,
        listener_hostname: Option<&str>,
        route_host: &str,
        rule: HttpRouteRule,
    ) {
        // Scopes store lowercase hostnames (see ListenerScope::new), so
        // normalize before matching an existing scope.
        let normalized = listener_hostname.map(|s| s.to_ascii_lowercase());
        let scopes = self.listener_scopes.entry(listener_port).or_default();
        let scope = match scopes
            .iter_mut()
            .find(|s| s.listener_hostname == normalized)
        {
            Some(s) => s,
            None => {
                scopes.push(ListenerScope::new(listener_port, normalized));
                scopes.last_mut().unwrap()
            }
        };

        scope.add_route(route_host, rule);
    }

    /// Convenience: add an HTTP route to a default catch-all scope on port 0.
    /// Used by tests and benchmarks.
    #[allow(dead_code)]
    pub fn add_http_route(&mut self, host: &str, rule: HttpRouteRule) {
        self.add_http_route_for_listener(0, None, host, rule);
    }

    fn sort_rules(rules: &mut [HttpRouteRule]) {
        // Sort by specificity (most specific first):
        // 1. Method presence (rules with method match first)
        // 2. Path type: exact > regex > prefix
        // 3. Path length (longer first)
        // 4. Header matcher count (more matchers = higher priority)
        // 5. Query param matcher count (more matchers = higher priority)
        rules.sort_by(|a, b| {
            // 1. Path match type: exact > regex > prefix
            fn rank(t: &PathMatchType) -> u8 {
                match t {
                    PathMatchType::Exact => 0,
                    PathMatchType::RegularExpression => 1,
                    PathMatchType::Prefix => 2,
                }
            }
            let r = rank(&a.path_match_type).cmp(&rank(&b.path_match_type));
            if r != std::cmp::Ordering::Equal {
                return r;
            }

            // 2. Path length (longer = more specific = higher priority)
            let r = b.path.len().cmp(&a.path.len());
            if r != std::cmp::Ordering::Equal {
                return r;
            }

            // 3. Method match presence (rules with method > rules without)
            let am = a.method_match.is_some() as u8;
            let bm = b.method_match.is_some() as u8;
            let r = bm.cmp(&am);
            if r != std::cmp::Ordering::Equal {
                return r;
            }

            // 4. Header matcher count (more matchers = higher priority)
            let r = b.header_matches.len().cmp(&a.header_matches.len());
            if r != std::cmp::Ordering::Equal {
                return r;
            }

            // 5. Query param matcher count
            b.query_param_matches
                .len()
                .cmp(&a.query_param_matches.len())
        });
    }

    pub fn add_tcp_route(&mut self, port: u16, backends: Vec<Backend>) {
        self.tcp_routes.insert(port, backends);
    }

    pub fn add_udp_route(&mut self, port: u16, backends: Vec<Backend>) {
        self.udp_routes.insert(port, backends);
    }

    /// Register a TLS passthrough route on a listener port. Port 0 means
    /// any-port (file-based configs without gateway context).
    pub fn add_tls_route(&mut self, port: u16, hostname: &str, backends: Vec<Backend>) {
        let host_lower = hostname.to_ascii_lowercase();
        if let Some(stripped) = host_lower.strip_prefix("*.") {
            self.wildcard_tls_routes
                .entry(port)
                .or_default()
                .insert(stripped.to_string(), backends);
        } else {
            self.tls_routes
                .entry(port)
                .or_default()
                .insert(host_lower, backends);
        }
    }
}

impl Default for RouteTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve `address` (IP literal or DNS name) to socket addresses for `port`.
///
/// An IP literal yields exactly one address. A DNS name resolves to every
/// record the resolver returns (deduped, resolver order preserved) — this is
/// the STRICT_DNS contract: a headless Service publishes one A-record per ready
/// pod at `<svc>.<ns>.svc`, a ClusterIP Service one VIP, an ExternalName a
/// CNAME chain. Errors if a DNS name resolves to nothing.
pub(super) fn resolve_socket_addrs(address: &str, port: u16) -> Result<Vec<std::net::SocketAddr>> {
    if let Ok(ip) = address.parse::<std::net::IpAddr>() {
        return Ok(vec![std::net::SocketAddr::new(ip, port)]);
    }
    use std::net::ToSocketAddrs;
    let mut seen = std::collections::HashSet::new();
    let addrs: Vec<std::net::SocketAddr> = format!("{}:{}", address, port)
        .to_socket_addrs()
        .map_err(|e| anyhow!("Failed to resolve hostname {}:{}: {}", address, port, e))?
        .filter(|sa| seen.insert(*sa))
        .collect();
    if addrs.is_empty() {
        return Err(anyhow!(
            "No addresses found for hostname {}:{}",
            address,
            port
        ));
    }
    Ok(addrs)
}

/// Stable per-address hash for [`RouteTable::dns_signature`]. Uses fnv (not the
/// process-seeded default) so the fingerprint is deterministic across resolves.
fn backend_addr_hash(addr: &std::net::SocketAddr) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = fnv::FnvHasher::default();
    addr.hash(&mut h);
    h.finish()
}

impl Backend {
    pub fn new(address: String, port: u16) -> Result<Self> {
        Self::with_weight(address, port, 1)
    }

    /// Resolve to a single backend, taking the first resolved address.
    /// Used where exactly one target is wanted (request mirroring, ExternalName
    /// overrides). For DNS-based load balancing across all records, use
    /// [`Backend::all_with_weight`].
    pub fn with_weight(address: String, port: u16, weight: u32) -> Result<Self> {
        let socket_addr = resolve_socket_addrs(&address, port)?[0];
        Ok(Self {
            socket_addr,
            weight,
            filters: vec![],
            use_tls: false,
            server_name: address,
        })
    }

    /// Resolve to one backend per resolved address (STRICT_DNS-style multi-A).
    ///
    /// A headless Service FQDN expands to N pod backends that the existing
    /// weighted selection load-balances across; a ClusterIP FQDN yields one.
    /// Each backend carries the same `weight` and the original DNS name as
    /// `server_name` (for TLS SNI). No "is this headless?" branch — the data
    /// plane treats every resolved address as an ordinary fixed-`socket_addr`
    /// backend, so health and pooling are unchanged.
    pub fn all_with_weight(address: String, port: u16, weight: u32) -> Result<Vec<Self>> {
        let addrs = resolve_socket_addrs(&address, port)?;
        Ok(addrs
            .into_iter()
            .map(|socket_addr| Self {
                socket_addr,
                weight,
                filters: vec![],
                use_tls: false,
                server_name: address.clone(),
            })
            .collect())
    }
}
