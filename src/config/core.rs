use std::path::Path;
use anyhow::{anyhow, Result};

use super::types::*;

impl PortailConfig {
    /// Load configuration from file with auto-format detection
    /// Supports JSON (.json) and YAML (.yaml, .yml) formats
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();

        if !path.exists() {
            return Err(anyhow!("Configuration file not found: {}", path.display()));
        }

        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow!("Failed to read configuration file '{}': {}", path.display(), e))?;

        let config = match path.extension().and_then(|ext| ext.to_str()) {
            Some("json") => {
                serde_json::from_str::<Self>(&content)
                    .map_err(|e| anyhow!("JSON parsing error in '{}': {}", path.display(), e))?
            }
            Some("yaml") | Some("yml") => {
                serde_yaml::from_str::<Self>(&content)
                    .map_err(|e| anyhow!("YAML parsing error in '{}': {}", path.display(), e))?
            }
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

        config.validate()
            .map_err(|e| anyhow!("Configuration validation failed for '{}': {}", path.display(), e))?;

        Ok(config)
    }
}

impl PortailConfig {
    /// Convert configuration to RouteTable for runtime use
    /// All route processing happens once at startup - zero runtime overhead
    pub fn to_route_table(&self) -> Result<crate::routing::RouteTable> {
        use crate::routing;

        let mut route_table = routing::RouteTable::new();

        tracing::debug!("Converting {} HTTP routes to route table", self.http_routes.len());

        for (route_idx, http_route) in self.http_routes.iter().enumerate() {
            tracing::debug!("Processing HTTP route {}: {} hostnames, {} rules",
                route_idx, http_route.hostnames.len(), http_route.rules.len());

            // Compute effective hostnames by intersecting with listener scope
            let effective_hostnames = match find_listener_for_route(&http_route.parent_refs, &self.gateway) {
                Some(listener) => {
                    let hostnames = intersect_hostnames(listener, &http_route.hostnames);
                    if hostnames.is_empty() {
                        tracing::warn!("HTTP route {} has no hostnames matching listener '{}' scope, skipping",
                            route_idx, listener.name);
                        continue;
                    }
                    hostnames
                }
                None => http_route.hostnames.clone(),
            };

            for hostname in &effective_hostnames {
                tracing::debug!("  Processing hostname: {}", hostname);
                for (rule_idx, rule) in http_route.rules.iter().enumerate() {
                    tracing::debug!("    Processing rule {}: {} matches, {} backend_refs",
                        rule_idx, rule.matches.len(), rule.backend_refs.len());

                    let mut backends = Vec::new();
                    for backend_ref in &rule.backend_refs {
                        let backend = routing::Backend::with_weight(
                            backend_ref.name.clone(),
                            backend_ref.port,
                            backend_ref.weight,
                        )?;
                        backends.push(backend);
                        tracing::debug!("        Backend: {}:{} weight={}", backend_ref.name, backend_ref.port, backend_ref.weight);
                    }

                    let filters = convert_filters(&rule.filters)?;

                    for route_match in &rule.matches {
                        let (path, path_match_type, path_regex) = if let Some(path_match) = &route_match.path {
                            match path_match.match_type {
                                HttpPathMatchType::PathPrefix => (path_match.value.as_str(), routing::PathMatchType::Prefix, None),
                                HttpPathMatchType::Exact => (path_match.value.as_str(), routing::PathMatchType::Exact, None),
                                HttpPathMatchType::RegularExpression => {
                                    let re = regex::Regex::new(&path_match.value)
                                        .map_err(|e| anyhow!("Invalid path regex '{}': {}", path_match.value, e))?;
                                    (path_match.value.as_str(), routing::PathMatchType::RegularExpression, Some(re))
                                }
                            }
                        } else {
                            ("/", routing::PathMatchType::Prefix, None)
                        };

                        let header_matches: Vec<routing::HeaderMatch> = route_match.headers.iter().map(Into::into).collect();
                        let query_param_matches: Vec<routing::QueryParamMatch> = route_match.query_params.iter().map(Into::into).collect();

                        tracing::debug!("      Adding route: {} {} -> {} backends",
                            hostname, path, backends.len());

                        let mut routing_rule = routing::HttpRouteRule::new(
                            path_match_type,
                            path.to_string(),
                            header_matches,
                            query_param_matches,
                            filters.clone(),
                            backends.clone(),
                        ).with_method(route_match.method.clone());
                        routing_rule.path_regex = path_regex;
                        if let Some(ref timeouts) = rule.timeouts {
                            routing_rule.request_timeout = timeouts.request;
                            routing_rule.backend_request_timeout = timeouts.backend_request;
                        }
                        route_table.add_http_route(hostname, routing_rule);
                    }
                }
            }
        }

        self.convert_l4_routes(&mut route_table, &self.tcp_routes, Protocol::TCP)?;
        self.convert_l4_routes(&mut route_table, &self.udp_routes, Protocol::UDP)?;

        // Convert TLS routes (SNI-based)
        for tls_route in &self.tls_routes {
            for rule in &tls_route.rules {
                let backends: Vec<routing::Backend> = rule.backend_refs.iter()
                    .map(|br| routing::Backend::new(br.name.clone(), br.port))
                    .collect::<Result<_>>()?;

                for hostname in &tls_route.hostnames {
                    route_table.add_tls_route(hostname, backends.clone());
                }
            }
        }

        tracing::debug!("Route table conversion completed: {} HTTP routes, {} TCP routes, {} UDP routes",
            route_table.http_routes.len(), route_table.tcp_routes.len(), route_table.udp_routes.len());

        Ok(route_table)
    }
}

trait L4Route {
    fn parent_refs(&self) -> &[ParentRef];
    fn backend_refs_per_rule(&self) -> Vec<&[BackendRef]>;
}

impl L4Route for TcpRouteConfig {
    fn parent_refs(&self) -> &[ParentRef] { &self.parent_refs }
    fn backend_refs_per_rule(&self) -> Vec<&[BackendRef]> {
        self.rules.iter().map(|r| r.backend_refs.as_slice()).collect()
    }
}

impl L4Route for UdpRouteConfig {
    fn parent_refs(&self) -> &[ParentRef] { &self.parent_refs }
    fn backend_refs_per_rule(&self) -> Vec<&[BackendRef]> {
        self.rules.iter().map(|r| r.backend_refs.as_slice()).collect()
    }
}

impl PortailConfig {
    fn convert_l4_routes<R: L4Route>(
        &self,
        route_table: &mut crate::routing::RouteTable,
        routes: &[R],
        protocol: Protocol,
    ) -> Result<()> {
        use crate::routing;

        for route in routes {
            if let Some(parent_ref) = route.parent_refs().first() {
                if let Some(section_name) = &parent_ref.section_name {
                    let protocol_matches = |l: &&ListenerConfig| match protocol {
                        Protocol::TCP => matches!(l.protocol, Protocol::TCP | Protocol::TLS),
                        _ => l.protocol == protocol,
                    };
                    if let Some(listener) = self.gateway.listeners.iter()
                        .find(|l| l.name == *section_name && protocol_matches(l)) {

                        for backend_refs in route.backend_refs_per_rule() {
                            let backends: Vec<routing::Backend> = backend_refs.iter()
                                .map(|br| routing::Backend::new(br.name.clone(), br.port))
                                .collect::<Result<_>>()?;

                            match protocol {
                                Protocol::TCP | Protocol::TLS | Protocol::HTTP | Protocol::HTTPS => route_table.add_tcp_route(listener.port, backends),
                                Protocol::UDP => route_table.add_udp_route(listener.port, backends),
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Find the listener that a route attaches to via parentRef.sectionName
fn find_listener_for_route<'a>(parent_refs: &[ParentRef], gateway: &'a GatewayConfig) -> Option<&'a ListenerConfig> {
    parent_refs.first().and_then(|pr| {
        pr.section_name.as_ref().and_then(|section| {
            gateway.listeners.iter().find(|l| l.name == *section)
        })
    })
}

/// Check if a route hostname is within the scope of a listener hostname.
/// Rules:
/// - Listener has no hostname: all route hostnames are valid
/// - Listener "*.example.com" accepts "foo.example.com" and "*.example.com"
/// - Listener "example.com" accepts only "example.com"
/// - Route "*.example.com" is within listener "*.example.com"
fn hostname_matches(listener_hostname: &str, route_hostname: &str) -> bool {
    let lh = listener_hostname.to_ascii_lowercase();
    let rh = route_hostname.to_ascii_lowercase();

    if let Some(listener_parent) = lh.strip_prefix("*.") {
        // Wildcard listener: route hostname must be under the same parent domain
        if let Some(route_parent) = rh.strip_prefix("*.") {
            // *.example.com listener, *.example.com route -> match
            route_parent == listener_parent
        } else {
            // *.example.com listener, foo.example.com route -> match if parent matches
            rh.ends_with(listener_parent) && rh.len() > listener_parent.len()
                && rh.as_bytes()[rh.len() - listener_parent.len() - 1] == b'.'
        }
    } else {
        // Exact listener: route hostname must match exactly (or route can be more specific wildcard)
        rh == lh
    }
}

/// Compute the intersection of listener hostname scope with route hostnames.
/// Returns the set of route hostnames that are valid within the listener's scope.
fn intersect_hostnames(listener: &ListenerConfig, route_hostnames: &[String]) -> Vec<String> {
    match &listener.hostname {
        None => {
            // No listener hostname -> all route hostnames are valid
            route_hostnames.to_vec()
        }
        Some(listener_hostname) => {
            route_hostnames.iter()
                .filter(|rh| hostname_matches(listener_hostname, rh))
                .cloned()
                .collect()
        }
    }
}

fn convert_filters(config_filters: &[HttpRouteFilter]) -> Result<Vec<crate::routing::HttpFilter>> {
    config_filters.iter().map(crate::routing::HttpFilter::try_from).collect()
}

impl TryFrom<&HttpRouteFilter> for crate::routing::HttpFilter {
    type Error = anyhow::Error;

    fn try_from(f: &HttpRouteFilter) -> Result<Self> {
        Ok(match f {
            HttpRouteFilter::RequestHeaderModifier { config } => {
                Self::RequestHeaderModifier {
                    add: config.add.clone(), set: config.set.clone(), remove: config.remove.clone(),
                }
            }
            HttpRouteFilter::ResponseHeaderModifier { config } => {
                Self::ResponseHeaderModifier {
                    add: config.add.clone(), set: config.set.clone(), remove: config.remove.clone(),
                }
            }
            HttpRouteFilter::RequestRedirect { config } => {
                let path = config.path.as_ref().map(rewrite_path_to_string);
                Self::RequestRedirect {
                    scheme: config.scheme.clone(),
                    hostname: config.hostname.clone(),
                    port: config.port,
                    path,
                    status_code: config.status_code,
                }
            }
            HttpRouteFilter::URLRewrite { config } => {
                Self::URLRewrite {
                    hostname: config.hostname.clone(),
                    path: config.path.as_ref().map(crate::routing::URLRewritePath::from),
                }
            }
            HttpRouteFilter::RequestMirror { config } => {
                let backend = crate::routing::Backend::with_weight(
                    config.backend_ref.name.clone(), config.backend_ref.port, config.backend_ref.weight,
                )?;
                Self::RequestMirror { backend_addr: backend.socket_addr }
            }
        })
    }
}

/// Extract the path string from an HttpURLRewritePath for redirect Location header
fn rewrite_path_to_string(p: &HttpURLRewritePath) -> String {
    match p {
        HttpURLRewritePath::ReplaceFullPath { value } => value.clone(),
        HttpURLRewritePath::ReplacePrefixMatch { value } => value.clone(),
    }
}

impl From<&HttpURLRewritePath> for crate::routing::URLRewritePath {
    fn from(p: &HttpURLRewritePath) -> Self {
        match p {
            HttpURLRewritePath::ReplaceFullPath { value } => Self::ReplaceFullPath(value.clone()),
            HttpURLRewritePath::ReplacePrefixMatch { value } => Self::ReplacePrefixMatch(value.clone()),
        }
    }
}

fn build_value_matcher(value: &str, match_type: &super::types::StringMatchType) -> crate::routing::ValueMatcher {
    match match_type {
        super::types::StringMatchType::Exact => crate::routing::ValueMatcher::Exact(value.to_string()),
        super::types::StringMatchType::RegularExpression => {
            // Validation ensures regex is valid before we get here
            let re = regex::Regex::new(value).expect("regex validated at config load time");
            crate::routing::ValueMatcher::Regex(re)
        }
    }
}

impl From<&super::types::HttpHeaderMatch> for crate::routing::HeaderMatch {
    fn from(hm: &super::types::HttpHeaderMatch) -> Self {
        Self { name: hm.name.to_ascii_lowercase(), matcher: build_value_matcher(&hm.value, &hm.match_type) }
    }
}

impl From<&super::types::HttpQueryParamMatch> for crate::routing::QueryParamMatch {
    fn from(qm: &super::types::HttpQueryParamMatch) -> Self {
        Self { name: qm.name.clone(), matcher: build_value_matcher(&qm.value, &qm.match_type) }
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::super::parsing::parse_duration;
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
                ],
                "workerThreads": 8
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
                ],
                "workerThreads": 8
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
        assert_eq!(config.http_routes[0].rules[0].matches[0].path.as_ref().unwrap().value, "/v1");
        assert_eq!(config.http_routes[0].rules[0].backend_refs[0].name, "api-service");
        assert_eq!(config.http_routes[0].rules[0].backend_refs[0].port, 3000);

        assert_eq!(config.tcp_routes.len(), 1);
        assert_eq!(config.tcp_routes[0].rules[0].backend_refs.len(), 2);
        assert_eq!(config.tcp_routes[0].rules[0].backend_refs[0].name, "db1-service");
        assert_eq!(config.tcp_routes[0].rules[0].backend_refs[1].name, "db2-service");
    }

    #[test]
    fn test_example_generation_functions() {
        let minimal = PortailConfig::generate_minimal();
        assert!(minimal.validate().is_ok());
        assert_eq!(minimal.http_routes.len(), 1);
        assert_eq!(minimal.tcp_routes.len(), 0);

        let development = PortailConfig::generate_development();
        assert!(development.validate().is_ok());
        assert_eq!(development.http_routes.len(), 8);
        assert_eq!(development.tcp_routes.len(), 1);
    }

    #[test]
    fn test_serialization_roundtrip() {
        let config = PortailConfig::generate_minimal();

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
                "listeners": [{"name": "http", "protocol": "HTTP", "port": 8080}],
                "workerThreads": 1
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
        let config = PortailConfig::generate_development();
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
                "listeners": [{"name": "http", "protocol": "HTTP", "port": 8080}],
                "workerThreads": 1
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
                "listeners": [{"name": "http", "protocol": "HTTP", "port": 8080}],
                "workerThreads": 1
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
        let rule = rt.find_http_route("example.com", "GET", "/v1/test", &[], "").unwrap();
        assert_eq!(rule.filters.len(), 1);
        assert!(matches!(&rule.filters[0], crate::routing::HttpFilter::URLRewrite { hostname: Some(h), .. } if h == "new.example.com"));
    }

    #[test]
    fn test_request_mirror_to_route_table() {
        let json = r#"{
            "gateway": {
                "name": "test-gw",
                "listeners": [{"name": "http", "protocol": "HTTP", "port": 8080}],
                "workerThreads": 1
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
        let rule = rt.find_http_route("example.com", "GET", "/", &[], "").unwrap();
        assert_eq!(rule.filters.len(), 1);
        if let crate::routing::HttpFilter::RequestMirror { backend_addr } = &rule.filters[0] {
            assert_eq!(backend_addr.port(), 9999);
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
                ],
                "workerThreads": 1
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
                ],
                "workerThreads": 1
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
        let config = PortailConfig::generate_development();
        let yaml = config.to_yaml_pretty().unwrap();
        let parsed: PortailConfig = serde_yaml::from_str(&yaml).unwrap();
        assert!(parsed.validate().is_ok());

        // Find the HTTPS listener
        let https = parsed.gateway.listeners.iter()
            .find(|l| matches!(l.protocol, Protocol::HTTPS))
            .expect("development config should have HTTPS listener");
        let tls = https.tls.as_ref().unwrap();
        assert!(matches!(tls.mode, TlsMode::Terminate));
        assert_eq!(tls.certificate_refs[0].name, "example-cert");
    }

    #[test]
    fn test_tls_config_json_roundtrip() {
        let config = PortailConfig::generate_development();
        let json = config.to_json_pretty().unwrap();
        let parsed: PortailConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.validate().is_ok());

        let https = parsed.gateway.listeners.iter()
            .find(|l| matches!(l.protocol, Protocol::HTTPS))
            .expect("development config should have HTTPS listener");
        assert_eq!(https.tls.as_ref().unwrap().certificate_refs[0].name, "example-cert");
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
    fn test_intersect_hostnames_no_listener_hostname() {
        let listener = ListenerConfig {
            name: "http".to_string(),
            protocol: Protocol::HTTP,
            port: 8080,
            hostname: None,
            address: None,
            interface: None,
            tls: None,
        };
        let route_hostnames = vec!["a.com".to_string(), "b.com".to_string()];
        let result = super::intersect_hostnames(&listener, &route_hostnames);
        assert_eq!(result, route_hostnames);
    }

    #[test]
    fn test_intersect_hostnames_wildcard_listener_filters() {
        let listener = ListenerConfig {
            name: "http".to_string(),
            protocol: Protocol::HTTP,
            port: 8080,
            hostname: Some("*.example.com".to_string()),
            address: None,
            interface: None,
            tls: None,
        };
        let route_hostnames = vec![
            "foo.example.com".to_string(),
            "bar.example.com".to_string(),
            "other.org".to_string(),
        ];
        let result = super::intersect_hostnames(&listener, &route_hostnames);
        assert_eq!(result, vec!["foo.example.com", "bar.example.com"]);
    }

    #[test]
    fn test_intersect_hostnames_exact_listener_restricts() {
        let listener = ListenerConfig {
            name: "http".to_string(),
            protocol: Protocol::HTTP,
            port: 8080,
            hostname: Some("api.example.com".to_string()),
            address: None,
            interface: None,
            tls: None,
        };
        let route_hostnames = vec![
            "api.example.com".to_string(),
            "web.example.com".to_string(),
        ];
        let result = super::intersect_hostnames(&listener, &route_hostnames);
        assert_eq!(result, vec!["api.example.com"]);
    }
}
