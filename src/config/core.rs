use anyhow::{anyhow, Result};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;

use super::compile::{CompiledFilter, CompiledRule};
use super::types::*;
use crate::routing::{Backend, RouteTable};

impl PortailConfig {
    /// Load configuration from file with auto-format detection
    /// Supports JSON (.json) and YAML (.yaml, .yml) formats
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();

        if !path.exists() {
            return Err(anyhow!("Configuration file not found: {}", path.display()));
        }

        let content = std::fs::read_to_string(path).map_err(|e| {
            anyhow!(
                "Failed to read configuration file '{}': {}",
                path.display(),
                e
            )
        })?;

        let config = match path.extension().and_then(|ext| ext.to_str()) {
            Some("json") => serde_json::from_str::<Self>(&content)
                .map_err(|e| anyhow!("JSON parsing error in '{}': {}", path.display(), e))?,
            Some("yaml") | Some("yml") => serde_yaml::from_str::<Self>(&content)
                .map_err(|e| anyhow!("YAML parsing error in '{}': {}", path.display(), e))?,
            Some(ext) => {
                return Err(anyhow!(
                    "Unsupported configuration file format '.{}' in '{}'. Supported formats: .json, .yaml, .yml",
                    ext,
                    path.display()
                ));
            }
            None => {
                return Err(anyhow!(
                    "Configuration file '{}' must have a file extension (.json, .yaml, .yml)",
                    path.display()
                ));
            }
        };

        config.validate().map_err(|e| {
            anyhow!(
                "Configuration validation failed for '{}': {}",
                path.display(),
                e
            )
        })?;

        Ok(config)
    }

    pub fn to_json_pretty(&self) -> Result<String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| anyhow!("Failed to serialize config to JSON: {}", e))
    }

    pub fn to_yaml_pretty(&self) -> Result<String> {
        serde_yaml::to_string(self)
            .map_err(|e| anyhow!("Failed to serialize config to YAML: {}", e))
    }
}

/// What the table build does with a route that fails its check, or a name
/// DNS cannot resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuildPolicy {
    /// Fail the build. The standalone load and reload paths: a valid config
    /// hit by a DNS blip must not swap in routes that serve 5xx while
    /// reporting success.
    Strict,
    /// Leave that route, backend or mirror out and build the rest. The
    /// Kubernetes path: route status reports the rejected route, a route
    /// whose backends do not resolve must serve 500 (conformance), and the
    /// DNS-refresh task picks names up as their records appear.
    Lenient,
}

/// Every address of a backend (name, port). Injected so tests can count and
/// fail lookups without a resolver.
pub(crate) type Lookup<'l> = dyn FnMut(&str, u16) -> Result<Vec<SocketAddr>> + 'l;

/// One lookup per distinct (name, port) per build: rules sharing a backend
/// get one address set, and a dead name costs one resolver timeout, not one
/// per rule.
struct Resolver<'a, 'l> {
    policy: BuildPolicy,
    lookup: &'a mut Lookup<'l>,
    seen: HashMap<(String, u16), Option<Vec<SocketAddr>>>,
}

impl Resolver<'_, '_> {
    /// `None`: the name did not resolve and the policy is lenient. Warns on
    /// the first lookup only.
    fn addrs(&mut self, name: &str, port: u16, what: &str) -> Result<Option<&[SocketAddr]>> {
        let found = match self.seen.entry((name.to_string(), port)) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                let found = match (self.lookup)(name, port) {
                    Ok(addrs) => Some(addrs),
                    Err(err) if self.policy == BuildPolicy::Strict => {
                        return Err(anyhow!(
                            "{} backend {}:{} failed to resolve: {}",
                            what,
                            name,
                            port,
                            err
                        ));
                    }
                    Err(err) => {
                        tracing::warn!(
                            "Skipping {} backend {}:{} - DNS resolution failed: {}",
                            what,
                            name,
                            port,
                            err
                        );
                        None
                    }
                };
                e.insert(found)
            }
        };
        Ok(found.as_deref())
    }

    /// One backend per address of each ref, at the ref's weight. An invalid
    /// ref is never looked up: it keeps its weight as a rejecting stand-in.
    fn backends(&mut self, refs: &[BackendRef], what: &str) -> Result<Vec<Backend>> {
        let mut backends = Vec::new();
        for br in refs {
            if br.invalid {
                backends.push(Backend::rejecting(br.weight));
            } else if let Some(addrs) = self.addrs(&br.name, br.port, what)? {
                backends.extend(addrs.iter().map(|&a| backend_at(a, br.weight, &br.name)));
            }
        }
        Ok(backends)
    }
}

fn backend_at(socket_addr: SocketAddr, weight: u32, server_name: &str) -> Backend {
    Backend {
        socket_addr,
        weight,
        filters: vec![],
        use_tls: false,
        server_name: server_name.to_string(),
        action: crate::routing::BackendAction::Forward,
    }
}

impl PortailConfig {
    /// Like [`Self::to_route_table`], but a route that fails its check, or a
    /// backendRef or mirror DNS cannot resolve, fails the build.
    pub fn to_route_table_strict(&self) -> Result<RouteTable> {
        self.build_route_table(
            BuildPolicy::Strict,
            &mut crate::routing::resolve_socket_addrs,
        )
    }

    /// Convert configuration to RouteTable for runtime use. What cannot be
    /// built is left out (see [`BuildPolicy::Lenient`]).
    pub fn to_route_table(&self) -> Result<RouteTable> {
        self.build_route_table(
            BuildPolicy::Lenient,
            &mut crate::routing::resolve_socket_addrs,
        )
    }

    pub(crate) fn build_route_table(
        &self,
        policy: BuildPolicy,
        lookup: &mut Lookup,
    ) -> Result<RouteTable> {
        use crate::routing;

        let mut resolver = Resolver {
            policy,
            lookup,
            seen: HashMap::new(),
        };
        let mut route_table = routing::RouteTable::new();

        tracing::debug!(
            "Converting {} HTTP routes to route table",
            self.http_routes.len()
        );

        for (route_idx, http_route) in self.http_routes.iter().enumerate() {
            tracing::debug!(
                "Processing HTTP route {}: {} hostnames, {} rules",
                route_idx,
                http_route.hostnames.len(),
                http_route.rules.len()
            );

            // Each route alone: one that fails is left out (lenient) and the
            // rest still goes live.
            let compiled = match super::compile::compile_http_route(http_route) {
                Ok(compiled) => compiled,
                Err(e) if policy == BuildPolicy::Strict => {
                    return Err(anyhow!("HTTP route {}: {}", route_idx, e));
                }
                Err(e) => {
                    tracing::warn!(
                        "Skipping HTTP route {} {:?} ({}): {}",
                        route_idx,
                        http_route.hostnames,
                        e.reason.as_str(),
                        e
                    );
                    continue;
                }
            };

            // Per-rule work once, however many listeners and hostnames.
            let mut rules = Vec::new();
            for (rule_idx, rule) in compiled.rules.iter().enumerate() {
                tracing::debug!(
                    "    Processing rule {}: {} matches, {} backend_refs",
                    rule_idx,
                    rule.matches.len(),
                    rule.source.backend_refs.len()
                );
                let backends = self.rule_backends(rule, &mut resolver)?;
                let filters = rule_filters(&rule.filters, &mut resolver)?;
                let timeouts = rule.source.timeouts.as_ref();
                rules.extend(
                    rule.matches
                        .iter()
                        .map(|m| m.to_rule(&filters, &backends, timeouts)),
                );
            }

            let matched_listeners =
                find_listeners_for_route(&http_route.parent_refs, &self.gateway);

            // No parentRefs: route without gateway context (file-based
            // configs). One pseudo-scope on port 0 with no hostname
            // constraint, so hostname intersection passes routes through.
            // Rate limiting rides the listener, so the pseudo-scope has none.
            // Refs that select no listener attach nowhere: the any-port scope
            // would serve the route on every port.
            type Scope<'a> = (
                u16,
                Option<&'a str>,
                &'a str,
                Option<std::sync::Arc<crate::rate_limit::Limiter>>,
            );
            let scopes: Vec<Scope> = if http_route.parent_refs.is_empty() {
                vec![(0, None, "", None)]
            } else {
                matched_listeners
                    .iter()
                    .map(|l| {
                        // Registry get-or-create, keyed on the listener
                        // identity: every rebuild re-attaches the same
                        // limiter, so bucket state survives table swaps.
                        let limiter = l.rate_limit.as_ref().map(|rl| {
                            crate::rate_limit::limiter_for(
                                l.port,
                                &l.name,
                                rl.requests_per_second,
                                rl.effective_burst(),
                            )
                        });
                        (l.port, l.hostname.as_deref(), l.name.as_str(), limiter)
                    })
                    .collect()
            };

            // For each scope, compute hostname intersection and add routes
            for (port, listener_hostname, listener_name, limiter) in scopes {
                let effective_hostnames = if http_route.hostnames.is_empty() {
                    // Route has no hostnames = match all.
                    // Use listener hostname if set, otherwise catch-all "*".
                    vec![listener_hostname.unwrap_or("*").to_string()]
                } else {
                    let hostnames = intersect_hostnames(listener_hostname, &http_route.hostnames);
                    if hostnames.is_empty() {
                        tracing::warn!(
                            "HTTP route {} has no hostnames matching listener '{}' scope, skipping",
                            route_idx,
                            listener_name
                        );
                        continue;
                    }
                    hostnames
                };

                for hostname in &effective_hostnames {
                    tracing::debug!(
                        "  Processing hostname: {} for listener {}:{} ({:?})",
                        hostname,
                        listener_name,
                        port,
                        listener_hostname
                    );
                    for rule in &rules {
                        let mut routing_rule = rule.clone();
                        // Each inserted rule rotates over its backends alone,
                        // as when every insert compiled its own rule.
                        routing_rule.counter = Default::default();
                        routing_rule.limiter = limiter.clone();
                        tracing::debug!(
                            "      Adding route: {} {} -> {} backends (listener {}:{:?})",
                            hostname,
                            routing_rule.path,
                            routing_rule.backends.len(),
                            port,
                            listener_hostname
                        );
                        route_table.push_http_route_for_listener(
                            port,
                            listener_hostname,
                            hostname,
                            routing_rule,
                        );
                    }
                }
            }
        }
        route_table.sort_http_routes();

        self.convert_l4_routes(&mut route_table, &self.tcp_routes, false, &mut resolver)?;
        self.convert_l4_routes(&mut route_table, &self.udp_routes, true, &mut resolver)?;

        // Convert TLS routes (SNI-based), scoped to the ports of the TLS
        // listeners their parentRefs attach to. Without parentRefs
        // (file-based configs) they land in the any-port (0) scope.
        for tls_route in &self.tls_routes {
            let matched_listeners = find_listeners_for_route(&tls_route.parent_refs, &self.gateway);
            let mut ports: Vec<u16> = matched_listeners
                .iter()
                .filter(|l| l.protocol == Protocol::TLS)
                .map(|l| l.port)
                .collect();
            ports.sort_unstable();
            ports.dedup();
            if tls_route.parent_refs.is_empty() {
                ports.push(0);
            }

            for rule in &tls_route.rules {
                let backends = resolver.backends(&rule.backend_refs, "TLS")?;

                for hostname in &tls_route.hostnames {
                    for &port in &ports {
                        route_table.add_tls_route(port, hostname, backends.clone());
                    }
                }
            }
        }

        tracing::debug!("Route table conversion completed: {} HTTP listener scopes, {} TCP routes, {} UDP routes",
            route_table.listener_scopes.values().map(|v| v.len()).sum::<usize>(), route_table.tcp_routes.len(), route_table.udp_routes.len());

        Ok(route_table)
    }
}

impl PortailConfig {
    /// `UdpRouteConfig` aliases `TcpRouteConfig`, so one signature serves
    /// both route lists; `is_udp` picks the socket family.
    ///
    /// Attachment is the same as for HTTP routes. A TCP or UDP listener
    /// carries one rule: rules have no match to choose between, so the
    /// first rule of the first route wins (routes come in precedence order,
    /// oldest first in Kubernetes mode, file order otherwise).
    fn convert_l4_routes(
        &self,
        route_table: &mut crate::routing::RouteTable,
        routes: &[TcpRouteConfig],
        is_udp: bool,
        resolver: &mut Resolver,
    ) -> Result<()> {
        let proto = if is_udp { "UDP" } else { "TCP" };
        for (route_idx, route) in routes.iter().enumerate() {
            let Some(rule) = route.rules.first() else {
                continue;
            };
            // TCPRoutes may also attach to TLS(Passthrough) listeners.
            let listeners: Vec<&ListenerConfig> =
                find_listeners_for_route(&route.parent_refs, &self.gateway)
                    .into_iter()
                    .filter(|l| {
                        if is_udp {
                            l.protocol == Protocol::UDP
                        } else {
                            matches!(l.protocol, Protocol::TCP | Protocol::TLS)
                        }
                    })
                    .collect();
            if listeners.is_empty() {
                continue;
            }
            let backends = resolver.backends(&rule.backend_refs, proto)?;
            for listener in listeners {
                let added = if is_udp {
                    route_table.add_udp_route(listener.port, backends.clone())
                } else {
                    route_table.add_tcp_route(listener.port, backends.clone())
                };
                if !added {
                    tracing::warn!(
                        "{} route {}: listener '{}' already carries an earlier route, skipping",
                        proto,
                        route_idx,
                        listener.name
                    );
                }
            }
        }
        Ok(())
    }
}

/// Listeners a route's parentRefs select, for every route kind in both
/// modes. In Kubernetes mode each ref names one listener the reconciler
/// accepted the route on; in a file config an unset sectionName or port
/// selects every listener.
fn find_listeners_for_route<'a>(
    parent_refs: &[ParentRef],
    gateway: &'a GatewayConfig,
) -> Vec<&'a ListenerConfig> {
    if parent_refs.is_empty() {
        return Vec::new();
    }

    let mut matched = Vec::new();
    for pr in parent_refs {
        let candidates = gateway.listeners.iter().filter(|l| pr.selects(l));

        for listener in candidates {
            if !matched
                .iter()
                .any(|l: &&ListenerConfig| l.name == listener.name)
            {
                matched.push(listener);
            }
        }
    }
    matched
}

impl PortailConfig {
    /// Backends of one compiled rule, resolved through `resolver`.
    fn rule_backends(&self, rule: &CompiledRule, resolver: &mut Resolver) -> Result<Vec<Backend>> {
        let mut backends = Vec::new();
        for (backend_ref, backend_filters) in
            rule.source.backend_refs.iter().zip(&rule.backend_filters)
        {
            if backend_ref.invalid {
                backends.push(Backend::rejecting(backend_ref.weight));
                continue;
            }

            // Determine app_protocol: explicit on BackendRef (from JSON config) or from Service spec override
            let override_key = (backend_ref.name.clone(), backend_ref.port);
            let effective_app_protocol = backend_ref.app_protocol.as_deref().or_else(|| {
                self.app_protocol_overrides
                    .get(&override_key)
                    .map(|s| s.as_str())
            });
            let backend_use_tls = effective_app_protocol == Some("https");
            let mut push = |addr: SocketAddr, server_name: &str| {
                let mut backend = backend_at(addr, backend_ref.weight, server_name);
                backend.filters = backend_filters.clone();
                backend.use_tls = backend_use_tls;
                backends.push(backend);
            };

            // Check if this backend has endpoint overrides (headless service)
            if let Some(endpoints) = self
                .endpoint_overrides
                .get(&override_key)
                .filter(|e| !e.is_empty())
            {
                tracing::debug!(
                    "        Headless backend {}:{} -> {} pod endpoints",
                    backend_ref.name,
                    backend_ref.port,
                    endpoints.len()
                );
                // An endpoint is one target: its first address.
                for (host, target_port) in endpoints {
                    if let Some(&addr) = resolver
                        .addrs(host, *target_port, "HTTP")?
                        .and_then(<[_]>::first)
                    {
                        push(addr, host);
                    }
                }
                continue;
            }

            // Standard DNS-based resolution resolves to every A-record (STRICT_DNS):
            // a headless Service FQDN expands to one backend per ready pod, a ClusterIP
            // FQDN to one VIP. The existing weighted selection load-balances across the
            // resulting flat entries.
            //
            // Port: a headless Service has no VIP, so DNS returns pod IPs and we must
            // connect on the Service's targetPort, not the service port (DNS carries no
            // port). For ClusterIP the FQDN resolves to the VIP and the service port is
            // correct (kube-proxy maps it), so there is no override and we keep the ref's
            // port.
            let connect_port = self
                .headless_target_ports
                .get(&override_key)
                .copied()
                .unwrap_or(backend_ref.port);
            if let Some(addrs) = resolver.addrs(&backend_ref.name, connect_port, "HTTP")? {
                tracing::debug!(
                    "        Backend: {}:{} weight={} tls={} -> {} address(es)",
                    backend_ref.name,
                    backend_ref.port,
                    backend_ref.weight,
                    backend_use_tls,
                    addrs.len()
                );
                for &addr in addrs {
                    push(addr, &backend_ref.name);
                }
            }
        }
        Ok(backends)
    }
}

/// Rule filters in the table's form. A mirror DNS cannot resolve is left
/// out when the policy is lenient: primary traffic never depends on it.
fn rule_filters(
    filters: &[CompiledFilter],
    resolver: &mut Resolver,
) -> Result<Vec<crate::routing::HttpFilter>> {
    let mut out = Vec::with_capacity(filters.len());
    for f in filters {
        match f {
            CompiledFilter::Ready(filter) => out.push(filter.clone()),
            CompiledFilter::Mirror { backend, fraction } => {
                // Mirrors go to one endpoint: the first address.
                if let Some(&backend_addr) = resolver
                    .addrs(&backend.name, backend.port, "RequestMirror")?
                    .and_then(<[_]>::first)
                {
                    out.push(crate::routing::HttpFilter::RequestMirror {
                        backend_addr,
                        fraction: *fraction,
                    });
                }
            }
        }
    }
    Ok(out)
}

/// Check if a route hostname is within the scope of a listener hostname.
/// File-config hostnames are not validated to be lowercase, so normalize
/// before the shared case-exact predicate.
fn hostname_matches(listener_hostname: &str, route_hostname: &str) -> bool {
    crate::routing::hostnames_intersect(
        &listener_hostname.to_ascii_lowercase(),
        &route_hostname.to_ascii_lowercase(),
    )
}

/// Compute the intersection of listener hostname scope with route hostnames.
/// Returns the set of effective hostnames that result from the intersection.
/// Per Gateway API spec, the intersection is the more specific of the two:
///   - listener "very.specific.com" ∩ route "*.specific.com" → "very.specific.com"
///   - listener "*.example.com" ∩ route "foo.example.com" → "foo.example.com"
///   - listener "*.example.com" ∩ route "*.example.com" → "*.example.com"
fn intersect_hostnames(listener_hostname: Option<&str>, route_hostnames: &[String]) -> Vec<String> {
    match listener_hostname {
        None => {
            // No listener hostname -> all route hostnames are valid
            route_hostnames.to_vec()
        }
        Some(listener_hostname) => {
            let lh = listener_hostname.to_ascii_lowercase();
            route_hostnames
                .iter()
                .filter(|rh| hostname_matches(listener_hostname, rh))
                .map(|rh| {
                    let rh_lower = rh.to_ascii_lowercase();
                    // Return the more specific of the two hostnames
                    if rh_lower.starts_with("*.") && !lh.starts_with("*.") {
                        // Route is wildcard, listener is exact: use listener (more specific)
                        lh.clone()
                    } else if lh.starts_with("*.") && !rh_lower.starts_with("*.") {
                        // Listener is wildcard, route is exact: use route (more specific)
                        rh_lower
                    } else if rh_lower.len() >= lh.len() {
                        // Same type: nested wildcards intersect, the longer
                        // pattern is the narrower scope
                        rh_lower
                    } else {
                        lh.clone()
                    }
                })
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::parsing::parse_duration;
    use super::super::*;
    use std::time::Duration;

    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration("5").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));

        assert!(parse_duration("invalid").is_err());
        assert!(parse_duration("-5s").is_err());
    }

    #[test]
    fn test_default_config() {
        let config = PortailConfig::default();
        assert!(config.validate().is_ok());
        assert!(!config.gateway.listeners.is_empty());
    }

    /// Collect every backend socket-address port in the route table (HTTP scopes).
    fn http_backend_ports(rt: &crate::routing::RouteTable) -> Vec<u16> {
        let mut ports = Vec::new();
        for scopes in rt.listener_scopes.values() {
            for scope in scopes {
                let entries = scope
                    .http_routes
                    .values()
                    .chain(scope.wildcard_http_routes.values())
                    .chain(scope.catch_all_http_routes.iter());
                for he in entries {
                    for rule in he {
                        for b in &rule.backends {
                            ports.push(b.socket_addr.port());
                        }
                    }
                }
            }
        }
        ports
    }

    #[test]
    fn headless_backend_connects_on_target_port() {
        // IP-literal backend name keeps this hermetic (no DNS). A headless service
        // has no VIP, so the data plane must connect to the resolved pod IP on the
        // Service targetPort, not the service port.
        let json = r#"{
            "gateway": {"name":"g","listeners":[{"name":"h","protocol":"HTTP","port":80}]},
            "httpRoutes": [{
                "hostnames": ["example.com"],
                "rules": [{"matches":[{"path":{"type":"PathPrefix","value":"/"}}],
                           "backendRefs":[{"name":"10.0.0.7","port":8080}]}]
            }]
        }"#;
        let mut config: PortailConfig = serde_json::from_str(json).unwrap();

        // No override (ClusterIP-style): keep the service port.
        let ports = http_backend_ports(&config.to_route_table().unwrap());
        assert_eq!(ports, vec![8080]);

        // Headless override maps service port 8080 -> targetPort 3000.
        config
            .headless_target_ports
            .insert(("10.0.0.7".to_string(), 8080), 3000);
        let ports = http_backend_ports(&config.to_route_table().unwrap());
        assert_eq!(
            ports,
            vec![3000],
            "headless backend must connect on the targetPort, not the service port"
        );
    }

    /// config-routing #7, file configs: a TCP route without sectionName
    /// passed validation, then vanished. It attaches like an HTTP route now,
    /// and on a shared listener the first route's first rule wins.
    #[test]
    fn test_l4_routes_attach_like_http_routes() {
        let json = r#"{
            "gateway": {"name": "g", "listeners": [
                {"name": "db", "protocol": "TCP", "port": 5432},
                {"name": "cache", "protocol": "TCP", "port": 6379},
                {"name": "web", "protocol": "HTTP", "port": 8080}
            ]},
            "tcpRoutes": [
                {"parentRefs": [{"name": "g"}],
                 "rules": [{"backendRefs": [{"name": "127.0.0.1", "port": 7001}]},
                           {"backendRefs": [{"name": "127.0.0.1", "port": 7002}]}]},
                {"parentRefs": [{"name": "g", "sectionName": "db"}],
                 "rules": [{"backendRefs": [{"name": "127.0.0.1", "port": 7003}]}]}
            ]
        }"#;
        let config: PortailConfig = serde_json::from_str(json).unwrap();
        config.validate().unwrap();
        let rt = config.to_route_table().unwrap();
        let backend_port = |port| rt.find_tcp_backends(port).unwrap()[0].socket_addr.port();
        assert_eq!(backend_port(5432), 7001);
        assert_eq!(backend_port(6379), 7001);
        assert!(rt.find_tcp_backends(8080).is_err());
    }

    #[test]
    fn test_json_deserialization() {
        let json = r#"{
            "gateway": {
                "name": "test-gateway",
                "listeners": [
                    {
                        "name": "http-9000",
                        "protocol": "HTTP",
                        "port": 9000
                    },
                    {
                        "name": "tcp-8080",
                        "protocol": "TCP",
                        "port": 8080
                    }
                ]
            }
        }"#;

        let config: PortailConfig = serde_json::from_str(json).unwrap();
        assert!(config.validate().is_ok());
        assert_eq!(config.gateway.listeners[0].port, 9000);
    }

    #[test]
    fn test_route_configuration() {
        let json = r#"{
            "gateway": {
                "name": "test-gateway",
                "listeners": [
                    {
                        "name": "http-8080",
                        "protocol": "HTTP",
                        "port": 8080
                    },
                    {
                        "name": "tcp-5432",
                        "protocol": "TCP",
                        "port": 5432
                    }
                ]
            },
            "httpRoutes": [
                {
                    "parentRefs": [
                        {
                            "name": "test-gateway",
                            "sectionName": "http-8080"
                        }
                    ],
                    "hostnames": ["api.example.com"],
                    "rules": [
                        {
                            "matches": [
                                {
                                    "path": {
                                        "type": "PathPrefix",
                                        "value": "/v1"
                                    }
                                }
                            ],
                            "backendRefs": [
                                {
                                    "name": "api-service",
                                    "port": 3000
                                }
                            ]
                        }
                    ]
                }
            ],
            "tcpRoutes": [
                {
                    "parentRefs": [
                        {
                            "name": "test-gateway",
                            "sectionName": "tcp-5432"
                        }
                    ],
                    "rules": [
                        {
                            "backendRefs": [
                                {
                                    "name": "db1-service",
                                    "port": 5432
                                },
                                {
                                    "name": "db2-service",
                                    "port": 5432
                                }
                            ]
                        }
                    ]
                }
            ]
        }"#;

        let config: PortailConfig = serde_json::from_str(json).unwrap();
        assert!(config.validate().is_ok());

        assert_eq!(config.gateway.name, "test-gateway");
        assert_eq!(config.gateway.listeners.len(), 2);

        assert_eq!(config.http_routes.len(), 1);
        assert_eq!(config.http_routes[0].hostnames[0], "api.example.com");
        assert_eq!(
            config.http_routes[0].rules[0].matches[0]
                .path
                .as_ref()
                .unwrap()
                .value,
            "/v1"
        );
        assert_eq!(
            config.http_routes[0].rules[0].backend_refs[0].name,
            "api-service"
        );
        assert_eq!(config.http_routes[0].rules[0].backend_refs[0].port, 3000);

        assert_eq!(config.tcp_routes.len(), 1);
        assert_eq!(config.tcp_routes[0].rules[0].backend_refs.len(), 2);
        assert_eq!(
            config.tcp_routes[0].rules[0].backend_refs[0].name,
            "db1-service"
        );
        assert_eq!(
            config.tcp_routes[0].rules[0].backend_refs[1].name,
            "db2-service"
        );
    }

    /// The committed example YAML files are embedded via include_str! and
    /// parsed here — schema drift in examples/standalone/ fails this test
    /// instead of silently rotting the documented examples.
    #[test]
    fn test_embedded_examples_parse_and_validate() {
        let minimal: PortailConfig =
            serde_yaml::from_str(crate::config::examples::MINIMAL_YAML).unwrap();
        assert!(minimal.validate().is_ok());
        assert_eq!(minimal.http_routes.len(), 1);
        assert_eq!(minimal.tcp_routes.len(), 0);

        let development: PortailConfig =
            serde_yaml::from_str(crate::config::examples::DEVELOPMENT_YAML).unwrap();
        assert!(development.validate().is_ok());
        assert_eq!(development.http_routes.len(), 8);
        assert_eq!(development.tcp_routes.len(), 1);
    }

    #[test]
    fn test_serialization_roundtrip() {
        let config: PortailConfig =
            serde_yaml::from_str(crate::config::examples::MINIMAL_YAML).unwrap();

        let json = config.to_json_pretty().unwrap();
        let parsed_json: PortailConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed_json.validate().is_ok());

        let yaml = config.to_yaml_pretty().unwrap();
        let parsed_yaml: PortailConfig = serde_yaml::from_str(&yaml).unwrap();
        assert!(parsed_yaml.validate().is_ok());
    }

    #[test]
    fn test_url_rewrite_json_deserialization() {
        let json = r#"{
            "gateway": {
                "name": "test-gw",
                "listeners": [{"name": "http", "protocol": "HTTP", "port": 8080}]
            },
            "httpRoutes": [{
                "parentRefs": [{"name": "test-gw", "sectionName": "http"}],
                "hostnames": ["example.com"],
                "rules": [{
                    "matches": [{"path": {"type": "PathPrefix", "value": "/v1"}}],
                    "filters": [{"type": "URLRewrite", "urlRewrite": {"path": {"type": "ReplaceFullPath", "replaceFullPath": "/v2"}}}],
                    "backendRefs": [{"name": "127.0.0.1", "port": 8001}]
                }]
            }]
        }"#;
        let config: PortailConfig = serde_json::from_str(json).unwrap();
        assert!(config.validate().is_ok());
        assert!(matches!(
            config.http_routes[0].rules[0].filters[0],
            HttpRouteFilter::URLRewrite { ref config } if config.path.is_some()
        ));
    }

    #[test]
    fn test_url_rewrite_yaml_roundtrip() {
        let config: PortailConfig =
            serde_yaml::from_str(crate::config::examples::DEVELOPMENT_YAML).unwrap();
        let yaml = config.to_yaml_pretty().unwrap();
        let parsed: PortailConfig = serde_yaml::from_str(&yaml).unwrap();
        assert!(parsed.validate().is_ok());
        assert_eq!(parsed.http_routes.len(), 8);
    }

    #[test]
    fn test_request_mirror_json_deserialization() {
        let json = r#"{
            "gateway": {
                "name": "test-gw",
                "listeners": [{"name": "http", "protocol": "HTTP", "port": 8080}]
            },
            "httpRoutes": [{
                "parentRefs": [{"name": "test-gw", "sectionName": "http"}],
                "hostnames": ["example.com"],
                "rules": [{
                    "matches": [{"path": {"type": "PathPrefix", "value": "/"}}],
                    "filters": [{"type": "RequestMirror", "requestMirror": {"backendRef": {"name": "127.0.0.1", "port": 9999}}}],
                    "backendRefs": [{"name": "127.0.0.1", "port": 8001}]
                }]
            }]
        }"#;
        let config: PortailConfig = serde_json::from_str(json).unwrap();
        assert!(config.validate().is_ok());
        assert!(matches!(
            config.http_routes[0].rules[0].filters[0],
            HttpRouteFilter::RequestMirror { .. }
        ));
    }

    #[test]
    fn test_url_rewrite_to_route_table() {
        let json = r#"{
            "gateway": {
                "name": "test-gw",
                "listeners": [{"name": "http", "protocol": "HTTP", "port": 8080}]
            },
            "httpRoutes": [{
                "parentRefs": [{"name": "test-gw", "sectionName": "http"}],
                "hostnames": ["example.com"],
                "rules": [{
                    "matches": [{"path": {"type": "PathPrefix", "value": "/v1"}}],
                    "filters": [{"type": "URLRewrite", "urlRewrite": {"hostname": "new.example.com"}}],
                    "backendRefs": [{"name": "127.0.0.1", "port": 8001}]
                }]
            }]
        }"#;
        let config: PortailConfig = serde_json::from_str(json).unwrap();
        let rt = config.to_route_table().unwrap();
        let rule = rt
            .find_http_route("example.com", "GET", "/v1/test", &[], "", 8080)
            .unwrap();
        assert_eq!(rule.filters.len(), 1);
        assert!(
            matches!(&rule.filters[0], crate::routing::HttpFilter::URLRewrite { hostname: Some(h), .. } if h == "new.example.com")
        );
    }

    /// A file config may spell the scheme in any case (RFC 3986 3.1); the
    /// redirect must still write it, and compare it, in lowercase.
    #[test]
    fn test_redirect_scheme_any_case_reaches_the_table_lowercase() {
        let json = r#"{
            "gateway": {
                "name": "test-gw",
                "listeners": [{"name": "http", "protocol": "HTTP", "port": 8080}]
            },
            "httpRoutes": [{
                "parentRefs": [{"name": "test-gw", "sectionName": "http"}],
                "hostnames": ["example.com"],
                "rules": [{
                    "matches": [{"path": {"type": "PathPrefix", "value": "/"}}],
                    "filters": [{"type": "RequestRedirect", "requestRedirect": {"scheme": "HTTPS", "statusCode": 301}}],
                    "backendRefs": [{"name": "127.0.0.1", "port": 8001}]
                }]
            }]
        }"#;
        let config: PortailConfig = serde_json::from_str(json).unwrap();
        config.validate().unwrap();
        let rt = config.to_route_table().unwrap();
        let rule = rt
            .find_http_route("example.com", "GET", "/", &[], "", 8080)
            .unwrap();
        assert!(
            matches!(&rule.filters[0], crate::routing::HttpFilter::RequestRedirect { scheme: Some(s), .. } if s == "https"),
            "{:?}",
            rule.filters
        );
    }

    /// Callers that skip validate() (reconciler, DNS refresh) still cannot
    /// build a table whose filters would write CR/LF: the route stays out.
    #[test]
    fn test_route_table_build_rejects_injectable_filter() {
        let json = r#"{
            "gateway": {
                "name": "test-gw",
                "listeners": [{"name": "http", "protocol": "HTTP", "port": 8080}]
            },
            "httpRoutes": [{
                "parentRefs": [{"name": "test-gw", "sectionName": "http"}],
                "hostnames": ["example.com"],
                "rules": [{
                    "matches": [{"path": {"type": "PathPrefix", "value": "/"}}],
                    "backendRefs": [{"name": "127.0.0.1", "port": 8001, "filters": [
                        {"type": "ResponseHeaderModifier", "responseHeaderModifier": {"set": [{"name": "X-A", "value": "a\r\nSet-Cookie: s=1"}]}}
                    ]}]
                }]
            }]
        }"#;
        let config: PortailConfig = serde_json::from_str(json).unwrap();
        let rt = config.to_route_table().unwrap();
        assert!(rt
            .find_http_route("example.com", "GET", "/", &[], "", 8080)
            .is_none());
        let err = config.to_route_table_strict().unwrap_err().to_string();
        assert!(
            err.starts_with(
                "HTTP route 0: rule 0: backendRef 0: filter 0: ResponseHeaderModifier set[0].value"
            ),
            "{}",
            err
        );
    }

    #[test]
    fn test_request_mirror_to_route_table() {
        let json = r#"{
            "gateway": {
                "name": "test-gw",
                "listeners": [{"name": "http", "protocol": "HTTP", "port": 8080}]
            },
            "httpRoutes": [{
                "parentRefs": [{"name": "test-gw", "sectionName": "http"}],
                "hostnames": ["example.com"],
                "rules": [{
                    "matches": [{"path": {"type": "PathPrefix", "value": "/"}}],
                    "filters": [{"type": "RequestMirror", "requestMirror": {"backendRef": {"name": "127.0.0.1", "port": 9999}}}],
                    "backendRefs": [{"name": "127.0.0.1", "port": 8001}]
                }]
            }]
        }"#;
        let config: PortailConfig = serde_json::from_str(json).unwrap();
        let rt = config.to_route_table().unwrap();
        let rule = rt
            .find_http_route("example.com", "GET", "/", &[], "", 8080)
            .unwrap();
        assert_eq!(rule.filters.len(), 1);
        if let crate::routing::HttpFilter::RequestMirror {
            backend_addr,
            fraction,
        } = &rule.filters[0]
        {
            assert_eq!(backend_addr.port(), 9999);
            assert_eq!(*fraction, crate::routing::Fraction::ALL); // default when no percent/fraction specified
        } else {
            panic!("expected RequestMirror filter");
        }
    }

    #[test]
    fn test_https_listener_json_deserialization() {
        let json = r#"{
            "gateway": {
                "name": "tls-gw",
                "listeners": [
                    {
                        "name": "https",
                        "protocol": "HTTPS",
                        "port": 443,
                        "tls": {
                            "mode": "Terminate",
                            "certificateRefs": [{"name": "my-cert"}]
                        }
                    }
                ]
            }
        }"#;
        let config: PortailConfig = serde_json::from_str(json).unwrap();
        assert!(config.validate().is_ok());

        let listener = &config.gateway.listeners[0];
        assert!(matches!(listener.protocol, Protocol::HTTPS));
        let tls = listener.tls.as_ref().unwrap();
        assert!(matches!(tls.mode, TlsMode::Terminate));
        assert_eq!(tls.certificate_refs[0].name, "my-cert");
    }

    #[test]
    fn test_tls_passthrough_listener_json_deserialization() {
        let json = r#"{
            "gateway": {
                "name": "tls-gw",
                "listeners": [
                    {
                        "name": "tls-passthrough",
                        "protocol": "TLS",
                        "port": 8443,
                        "tls": {
                            "mode": "Passthrough"
                        }
                    }
                ]
            }
        }"#;
        let config: PortailConfig = serde_json::from_str(json).unwrap();
        assert!(config.validate().is_ok());

        let listener = &config.gateway.listeners[0];
        assert!(matches!(listener.protocol, Protocol::TLS));
        let tls = listener.tls.as_ref().unwrap();
        assert!(matches!(tls.mode, TlsMode::Passthrough));
        assert!(tls.certificate_refs.is_empty());
    }

    #[test]
    fn test_tls_config_yaml_roundtrip() {
        let config: PortailConfig =
            serde_yaml::from_str(crate::config::examples::DEVELOPMENT_YAML).unwrap();
        let yaml = config.to_yaml_pretty().unwrap();
        let parsed: PortailConfig = serde_yaml::from_str(&yaml).unwrap();
        assert!(parsed.validate().is_ok());

        // Find the HTTPS listener
        let https = parsed
            .gateway
            .listeners
            .iter()
            .find(|l| matches!(l.protocol, Protocol::HTTPS))
            .expect("development config should have HTTPS listener");
        let tls = https.tls.as_ref().unwrap();
        assert!(matches!(tls.mode, TlsMode::Terminate));
        assert_eq!(tls.certificate_refs[0].name, "example-cert");
    }

    #[test]
    fn test_tls_config_json_roundtrip() {
        let config: PortailConfig =
            serde_yaml::from_str(crate::config::examples::DEVELOPMENT_YAML).unwrap();
        let json = config.to_json_pretty().unwrap();
        let parsed: PortailConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.validate().is_ok());

        let https = parsed
            .gateway
            .listeners
            .iter()
            .find(|l| matches!(l.protocol, Protocol::HTTPS))
            .expect("development config should have HTTPS listener");
        assert_eq!(
            https.tls.as_ref().unwrap().certificate_refs[0].name,
            "example-cert"
        );
    }

    #[test]
    fn test_hostname_matches_wildcard_listener() {
        assert!(super::hostname_matches("*.example.com", "foo.example.com"));
        assert!(super::hostname_matches("*.example.com", "*.example.com"));
        assert!(!super::hostname_matches("*.example.com", "example.com"));
        assert!(!super::hostname_matches("*.example.com", "foo.other.com"));
    }

    #[test]
    fn test_hostname_matches_exact_listener() {
        assert!(super::hostname_matches("example.com", "example.com"));
        assert!(!super::hostname_matches("example.com", "foo.example.com"));
        assert!(!super::hostname_matches("example.com", "other.com"));
    }

    #[test]
    fn test_hostname_matches_is_case_insensitive() {
        assert!(super::hostname_matches("*.Example.COM", "foo.example.com"));
        assert!(super::hostname_matches(
            "API.example.com",
            "api.Example.com"
        ));
    }

    #[test]
    fn test_intersect_hostnames_nested_wildcards() {
        // Route wildcard inside the listener wildcard: keep the route pattern
        let result =
            super::intersect_hostnames(Some("*.example.com"), &["*.sub.example.com".to_string()]);
        assert_eq!(result, vec!["*.sub.example.com"]);
        // Listener narrower than the route: keep the listener pattern
        let result =
            super::intersect_hostnames(Some("*.sub.example.com"), &["*.example.com".to_string()]);
        assert_eq!(result, vec!["*.sub.example.com"]);
    }

    #[test]
    fn test_intersect_hostnames_no_listener_hostname() {
        let route_hostnames = vec!["a.com".to_string(), "b.com".to_string()];
        let result = super::intersect_hostnames(None, &route_hostnames);
        assert_eq!(result, route_hostnames);
    }

    #[test]
    fn test_intersect_hostnames_wildcard_listener_filters() {
        let route_hostnames = vec![
            "foo.example.com".to_string(),
            "bar.example.com".to_string(),
            "other.org".to_string(),
        ];
        let result = super::intersect_hostnames(Some("*.example.com"), &route_hostnames);
        assert_eq!(result, vec!["foo.example.com", "bar.example.com"]);
    }

    #[test]
    fn test_intersect_hostnames_exact_listener_restricts() {
        let route_hostnames = vec!["api.example.com".to_string(), "web.example.com".to_string()];
        let result = super::intersect_hostnames(Some("api.example.com"), &route_hostnames);
        assert_eq!(result, vec!["api.example.com"]);
    }

    #[test]
    fn test_external_name_endpoint_override_resolves() {
        // Simulate what the controller does for ExternalName services:
        // endpoint_overrides maps (svc_fqdn, port) → (externalName, port)
        let json = r#"{
            "gateway": {
                "name": "test-gw",
                "listeners": [{"name": "http", "protocol": "HTTP", "port": 8080}]
            },
            "httpRoutes": [{
                "parentRefs": [{"name": "test-gw", "sectionName": "http"}],
                "hostnames": ["proxy.example.com"],
                "rules": [{
                    "matches": [{"path": {"type": "PathPrefix", "value": "/"}}],
                    "backendRefs": [{"name": "external-api.default.svc", "port": 443}]
                }]
            }]
        }"#;
        let mut config: PortailConfig = serde_json::from_str(json).unwrap();
        assert!(config.validate().is_ok());

        // Insert ExternalName override: svc FQDN → external DNS name
        config.endpoint_overrides.insert(
            ("external-api.default.svc".to_string(), 443),
            vec![("httpbin.org".to_string(), 443)],
        );

        // to_route_table should resolve "httpbin.org" via DNS
        let rt = config.to_route_table();
        assert!(
            rt.is_ok(),
            "ExternalName DNS resolution should succeed: {:?}",
            rt.err()
        );
        let rt = rt.unwrap();

        let rule = rt
            .find_http_route("proxy.example.com", "GET", "/", &[], "", 8080)
            .unwrap();
        assert!(
            !rule.backends.is_empty(),
            "should have resolved ExternalName backend"
        );
        assert_eq!(rule.backends[0].socket_addr.port(), 443);
    }

    /// One route whose regex does not compile used to fail the whole build:
    /// every Gateway in it stopped updating and went Programmed=False.
    #[test]
    fn one_bad_route_does_not_take_the_table_down() {
        let json = r#"{
            "gateway": {"name":"g","listeners":[{"name":"h","protocol":"HTTP","port":8080}]},
            "httpRoutes": [
                {"hostnames":["bad.example.com"],
                 "rules":[{"matches":[{"path":{"type":"RegularExpression","value":"/(?=x)"}}],
                           "backendRefs":[{"name":"127.0.0.1","port":8001}]}]},
                {"hostnames":["good.example.com"],
                 "rules":[{"matches":[{"path":{"type":"PathPrefix","value":"/"}}],
                           "backendRefs":[{"name":"127.0.0.1","port":8002}]}]}
            ]
        }"#;
        let config: PortailConfig = serde_json::from_str(json).unwrap();
        let rt = config.to_route_table().unwrap();
        assert!(rt
            .find_http_route("good.example.com", "GET", "/", &[], "", 8080)
            .is_some());
        assert!(rt
            .find_http_route("bad.example.com", "GET", "/x", &[], "", 8080)
            .is_none());

        let err = config.to_route_table_strict().unwrap_err().to_string();
        assert!(
            err.starts_with("HTTP route 0: rule 0: match 0: Invalid path regex '/(?=x)'"),
            "{err}"
        );
    }

    /// Unanchored, path regex `/api` matched `/static/api/x`, and header or
    /// query regex `v1` matched `xv1x`.
    #[test]
    fn regex_matches_are_full_matches() {
        let json = r#"{
            "gateway": {"name":"g","listeners":[{"name":"h","protocol":"HTTP","port":8080}]},
            "httpRoutes": [
                {"hostnames":["p.example.com"],
                 "rules":[{"matches":[{"path":{"type":"RegularExpression","value":"/api"}}],
                           "backendRefs":[{"name":"127.0.0.1","port":8001}]}]},
                {"hostnames":["h.example.com"],
                 "rules":[{"matches":[{"headers":[{"name":"x-v","type":"RegularExpression","value":"v1"}],
                                       "queryParams":[{"name":"q","type":"RegularExpression","value":"v1"}]}],
                           "backendRefs":[{"name":"127.0.0.1","port":8001}]}]}
            ]
        }"#;
        let config: PortailConfig = serde_json::from_str(json).unwrap();
        let rt = config.to_route_table().unwrap();
        let found = |host, path, headers: &[u8], query| {
            rt.find_http_route(host, "GET", path, headers, query, 8080)
                .is_some()
        };
        assert!(found("p.example.com", "/api", b"", ""));
        assert!(!found("p.example.com", "/static/api/x", b"", ""));
        assert!(!found("p.example.com", "/api/x", b"", ""));
        assert!(found("h.example.com", "/", b"x-v: v1\r\n", "q=v1"));
        assert!(!found("h.example.com", "/", b"x-v: xv1x\r\n", "q=v1"));
        assert!(!found("h.example.com", "/", b"x-v: v1\r\n", "q=v1x"));
    }

    /// PathPrefix /abc/ did not match /abc.
    #[test]
    fn a_path_prefix_ignores_its_trailing_slash() {
        let json = r#"{
            "gateway": {"name":"g","listeners":[{"name":"h","protocol":"HTTP","port":8080}]},
            "httpRoutes": [
                {"hostnames":["p.example.com"],
                 "rules":[{"matches":[{"path":{"type":"PathPrefix","value":"/foo/"}}],
                           "filters":[{"type":"URLRewrite","urlRewrite":{"path":{"type":"ReplacePrefixMatch","replacePrefixMatch":"/xyz"}}}],
                           "backendRefs":[{"name":"127.0.0.1","port":8001}]}]},
                {"hostnames":["r.example.com"],
                 "rules":[{"matches":[{"path":{"type":"PathPrefix","value":"/"}}],
                           "backendRefs":[{"name":"127.0.0.1","port":8001}]}]}
            ]
        }"#;
        let config: PortailConfig = serde_json::from_str(json).unwrap();
        let rt = config.to_route_table().unwrap();
        let rewrite = |path: &str| {
            let rule = rt.find_http_route("p.example.com", "GET", path, &[], "", 8080)?;
            let rewrite =
                crate::proxy::http_filters::extract_url_rewrite(&rule.filters, &rule.path, path)?;
            match rewrite.path? {
                crate::proxy::http_filters::RewrittenPath::PrefixReplaced(p) => Some(p),
                crate::proxy::http_filters::RewrittenPath::Full(p) => panic!("{p}"),
            }
        };
        // The ReplacePrefixMatch table of Gateway API v1.4.1, prefix /foo/.
        assert_eq!(rewrite("/foo/bar").as_deref(), Some("/xyz/bar"));
        assert_eq!(rewrite("/foo").as_deref(), Some("/xyz"));
        assert_eq!(rewrite("/foo/").as_deref(), Some("/xyz/"));
        assert_eq!(rewrite("/foobar"), None);

        let root = |path| {
            rt.find_http_route("r.example.com", "GET", path, &[], "", 8080)
                .is_some()
        };
        assert!(root("/") && root("/x") && root("/x/y"));
    }

    /// A rule with `matches: []`, or none at all, served nothing.
    #[test]
    fn a_rule_without_matches_matches_every_path() {
        for matches in [r#""matches":[],"#, ""] {
            let json = format!(
                r#"{{
                "gateway": {{"name":"g","listeners":[{{"name":"h","protocol":"HTTP","port":8080}}]}},
                "httpRoutes": [{{"hostnames":["e.example.com"],
                    "rules":[{{{matches}"backendRefs":[{{"name":"127.0.0.1","port":8001}}]}}]}}]
            }}"#
            );
            let config: PortailConfig = serde_json::from_str(&json).unwrap();
            let rt = config.to_route_table().unwrap();
            for path in ["/", "/anything/at/all"] {
                let rule = rt
                    .find_http_route("e.example.com", "GET", path, &[], "", 8080)
                    .unwrap_or_else(|| panic!("{matches:?} {path}: no route"));
                assert_eq!(rule.path_match_type, crate::routing::PathMatchType::Prefix);
            }
        }
    }

    /// Rounded to whole percents, fraction 1/1000 became 0%: it never mirrored.
    #[test]
    fn mirror_fraction_reaches_the_table_exact() {
        let fraction = |mirror: &str| {
            let json = format!(
                r#"{{
                "gateway": {{"name":"g","listeners":[{{"name":"h","protocol":"HTTP","port":8080}}]}},
                "httpRoutes": [{{"hostnames":["m.example.com"],
                    "rules":[{{"matches":[{{"path":{{"type":"PathPrefix","value":"/"}}}}],
                               "filters":[{{"type":"RequestMirror","requestMirror":{{"backendRef":{{"name":"127.0.0.1","port":9999}}{mirror}}}}}],
                               "backendRefs":[{{"name":"127.0.0.1","port":8001}}]}}]}}]
            }}"#
            );
            let config: PortailConfig = serde_json::from_str(&json).unwrap();
            let rt = config.to_route_table().unwrap();
            let rule = rt
                .find_http_route("m.example.com", "GET", "/", &[], "", 8080)
                .unwrap();
            match &rule.filters[..] {
                [crate::routing::HttpFilter::RequestMirror { fraction, .. }] => *fraction,
                other => panic!("{other:?}"),
            }
        };
        let exact = crate::routing::Fraction::new;
        assert_eq!(
            fraction(r#","fraction":{"numerator":1,"denominator":1000}"#),
            exact(1, 1000).unwrap()
        );
        assert_eq!(
            fraction(r#","fraction":{"numerator":1}"#),
            exact(1, 100).unwrap()
        );
        assert_eq!(fraction(r#","percent":25"#), exact(25, 100).unwrap());
        assert_eq!(fraction(""), crate::routing::Fraction::ALL);
    }

    /// IP literals resolve to themselves, `multi.svc` to two pods, any other
    /// name fails. Records every lookup.
    fn fake_dns(
        calls: &std::cell::RefCell<Vec<String>>,
        name: &str,
        port: u16,
    ) -> anyhow::Result<Vec<std::net::SocketAddr>> {
        calls.borrow_mut().push(format!("{name}:{port}"));
        let ips: Vec<std::net::IpAddr> = match name.parse() {
            Ok(ip) => vec![ip],
            Err(_) if name == "multi.svc" => vec![[10, 0, 0, 1].into(), [10, 0, 0, 2].into()],
            Err(_) => anyhow::bail!("NXDOMAIN"),
        };
        Ok(ips
            .into_iter()
            .map(|ip| std::net::SocketAddr::new(ip, port))
            .collect())
    }

    fn build_with_fake_dns(
        config: &PortailConfig,
        policy: super::BuildPolicy,
    ) -> (anyhow::Result<crate::routing::RouteTable>, Vec<String>) {
        let calls = std::cell::RefCell::new(vec![]);
        let built = config.build_route_table(policy, &mut |n, p| fake_dns(&calls, n, p));
        (built, calls.into_inner())
    }

    /// Two listeners, two hostnames, two rules on one backend, plus TCP and
    /// TLS routes on it.
    fn shared_backend_config(http_backend: &str) -> PortailConfig {
        let json = format!(
            r#"{{
            "gateway": {{"name":"g","listeners":[
                {{"name":"a","protocol":"HTTP","port":8080}},
                {{"name":"b","protocol":"HTTP","port":8081}},
                {{"name":"t","protocol":"TCP","port":5432}},
                {{"name":"p","protocol":"TLS","port":8443,"tls":{{"mode":"Passthrough"}}}}]}},
            "httpRoutes": [{{
                "parentRefs": [{{"name":"g","sectionName":"a"}},{{"name":"g","sectionName":"b"}}],
                "hostnames": ["x.example.com","y.example.com"],
                "rules": [
                    {{"matches":[{{"path":{{"type":"PathPrefix","value":"/a"}}}}],"backendRefs":[{{"name":"{b}","port":80}}]}},
                    {{"matches":[{{"path":{{"type":"PathPrefix","value":"/b"}}}}],"backendRefs":[{{"name":"multi.svc","port":80}}]}}
                ]}}],
            "tcpRoutes": [{{"parentRefs":[{{"name":"g","sectionName":"t"}}],"rules":[{{"backendRefs":[{{"name":"multi.svc","port":80}}]}}]}}],
            "tlsRoutes": [{{"parentRefs":[{{"name":"g","sectionName":"p"}}],"hostnames":["s.example.com"],"rules":[{{"backendRefs":[{{"name":"multi.svc","port":80}}]}}]}}]
        }}"#,
            b = http_backend
        );
        serde_json::from_str(&json).unwrap()
    }

    /// Strict used to look every backend up twice (a pre-check, then the
    /// build), and the build once per rule x hostname x listener.
    #[test]
    fn each_backend_name_is_looked_up_once_per_build() {
        let config = shared_backend_config("multi.svc");
        let (built, calls) = build_with_fake_dns(&config, super::BuildPolicy::Strict);
        let rt = built.unwrap();
        assert_eq!(calls, vec!["multi.svc:80"]);
        let rule = rt
            .find_http_route("y.example.com", "GET", "/a", &[], "", 8081)
            .unwrap();
        assert_eq!(rule.backends.len(), 2);
        assert_eq!(rt.find_tcp_backends(5432).unwrap().len(), 2);
    }

    #[test]
    fn strict_build_fails_on_a_dead_backend_lenient_skips_it() {
        let config = shared_backend_config("dead.svc");
        let (built, calls) = build_with_fake_dns(&config, super::BuildPolicy::Strict);
        let err = built.unwrap_err().to_string();
        assert!(
            err.starts_with("HTTP backend dead.svc:80 failed to resolve: NXDOMAIN"),
            "{err}"
        );
        assert_eq!(calls, vec!["dead.svc:80"]);

        let (built, calls) = build_with_fake_dns(&config, super::BuildPolicy::Lenient);
        let rt = built.unwrap();
        assert_eq!(calls, vec!["dead.svc:80", "multi.svc:80"]);
        let dead = rt
            .find_http_route("x.example.com", "GET", "/a", &[], "", 8080)
            .unwrap();
        assert!(dead.backends.is_empty());
        let live = rt
            .find_http_route("x.example.com", "GET", "/b", &[], "", 8080)
            .unwrap();
        assert_eq!(live.backends.len(), 2);
    }

    /// A mirror DNS cannot resolve used to fail the whole lenient build, so
    /// every route of the Gateway stopped updating.
    #[test]
    fn mirror_dns_miss_drops_the_mirror_and_keeps_the_route() {
        let json = r#"{
            "gateway": {"name":"g","listeners":[{"name":"h","protocol":"HTTP","port":8080}]},
            "httpRoutes": [{
                "hostnames": ["m.example.com"],
                "rules": [{"matches":[{"path":{"type":"PathPrefix","value":"/"}}],
                           "filters":[{"type":"RequestMirror","requestMirror":{"backendRef":{"name":"dead.svc","port":9999}}}],
                           "backendRefs":[{"name":"127.0.0.1","port":8001}]}]
            }]
        }"#;
        let config: PortailConfig = serde_json::from_str(json).unwrap();

        let (built, _) = build_with_fake_dns(&config, super::BuildPolicy::Lenient);
        let rt = built.unwrap();
        let rule = rt
            .find_http_route("m.example.com", "GET", "/", &[], "", 8080)
            .unwrap();
        assert_eq!(rule.backends.len(), 1);
        assert!(rule.filters.is_empty(), "{:?}", rule.filters);

        let (built, _) = build_with_fake_dns(&config, super::BuildPolicy::Strict);
        let err = built.unwrap_err().to_string();
        assert!(
            err.starts_with("RequestMirror backend dead.svc:9999 failed to resolve"),
            "{err}"
        );
    }
}
