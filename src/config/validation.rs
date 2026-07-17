use anyhow::{anyhow, Result};

use super::types::*;

impl PortailConfig {
    /// Validate the entire configuration for consistency and correctness
    /// All validation happens once at startup
    pub fn validate(&self) -> Result<()> {
        self.gateway.validate()?;

        for (i, route) in self.http_routes.iter().enumerate() {
            route
                .validate()
                .map_err(|e| anyhow!("HTTP route {}: {}", i, e))?;
        }

        for (i, route) in self.tcp_routes.iter().enumerate() {
            route
                .validate()
                .map_err(|e| anyhow!("TCP route {}: {}", i, e))?;
        }

        for (i, route) in self.tls_routes.iter().enumerate() {
            route
                .validate()
                .map_err(|e| anyhow!("TLS route {}: {}", i, e))?;
        }

        for (i, route) in self.udp_routes.iter().enumerate() {
            route
                .validate()
                .map_err(|e| anyhow!("UDP route {}: {}", i, e))?;
        }

        self.validate_port_conflicts()?;
        self.validate_parent_refs_resolve()?;

        Ok(())
    }

    fn validate_port_conflicts(&self) -> Result<()> {
        // A conflict is two listeners binding the same SOCKET, so key on what
        // the data plane actually binds: (bind address, bound port, socket
        // family). `target_port` overrides `port` when set, and listeners on
        // different addresses may legally share a port number. TCP and UDP can
        // also share a port number (different socket families);
        // HTTP/HTTPS/TLS/TCP all bind TCP sockets.
        let mut used_sockets = std::collections::HashSet::new();

        for listener in &self.gateway.listeners {
            let is_udp = matches!(listener.protocol, Protocol::UDP);
            let bound_port = listener.target_port.unwrap_or(listener.port);
            if !used_sockets.insert((listener.address.clone(), bound_port, is_udp)) {
                let proto_family = if is_udp { "UDP" } else { "TCP" };
                return Err(anyhow!(
                    "Port conflict: {} port {} on {} is already bound by another listener",
                    proto_family,
                    bound_port,
                    listener.address.as_deref().unwrap_or("0.0.0.0"),
                ));
            }
        }

        Ok(())
    }

    /// Every route parentRef must resolve to at least one Gateway listener.
    /// Without this, a mistyped `sectionName` silently produced a config that
    /// "succeeds" but routes nothing (HTTP routes even fell into the any-port
    /// scope). Only enforced on the file-config path — in Kubernetes mode
    /// route attachment failures are reported via route status instead.
    fn validate_parent_refs_resolve(&self) -> Result<()> {
        fn check(
            parent_refs: &[ParentRef],
            listeners: &[ListenerConfig],
            proto: &str,
            idx: usize,
        ) -> Result<()> {
            for pr in parent_refs {
                let resolves = listeners.iter().any(|l| {
                    pr.section_name.as_ref().is_none_or(|s| &l.name == s)
                        && pr.port.is_none_or(|p| l.port == p as u16)
                });
                if !resolves {
                    return Err(anyhow!(
                        "{} route {}: parentRef (sectionName: {:?}, port: {:?}) does not match any Gateway listener",
                        proto,
                        idx,
                        pr.section_name,
                        pr.port,
                    ));
                }
            }
            Ok(())
        }

        let listeners = &self.gateway.listeners;
        for (i, r) in self.http_routes.iter().enumerate() {
            check(&r.parent_refs, listeners, "HTTP", i)?;
        }
        for (i, r) in self.tcp_routes.iter().enumerate() {
            check(&r.parent_refs, listeners, "TCP", i)?;
        }
        for (i, r) in self.tls_routes.iter().enumerate() {
            check(&r.parent_refs, listeners, "TLS", i)?;
        }
        for (i, r) in self.udp_routes.iter().enumerate() {
            check(&r.parent_refs, listeners, "UDP", i)?;
        }
        Ok(())
    }
}

/// Shared route validation: non-empty `parent_refs`, non-empty `rules`,
/// per-element validation. Protocol-specific checks (HTTP hostnames, TLS SNI)
/// stay in the caller.
fn validate_route<R>(
    parent_refs: &[ParentRef],
    rules: &[R],
    proto: &str,
    validate_rule: impl Fn(&R) -> Result<()>,
) -> Result<()> {
    if parent_refs.is_empty() {
        return Err(anyhow!("{} route must have at least one parent_ref", proto));
    }
    for parent_ref in parent_refs {
        parent_ref.validate()?;
    }
    if rules.is_empty() {
        return Err(anyhow!("{} route must have at least one rule", proto));
    }
    for (i, rule) in rules.iter().enumerate() {
        validate_rule(rule).map_err(|e| anyhow!("{} route rule {}: {}", proto, i, e))?;
    }
    Ok(())
}

/// Wildcard hostnames must use the `*.domain` form. Empty-check is left to
/// callers because they emit protocol-specific messages.
fn validate_hostname_wildcard(hostname: &str) -> Result<()> {
    if hostname.starts_with('*') && !hostname.starts_with("*.") {
        return Err(anyhow!(
            "Wildcard hostname must use '*.domain' format: {}",
            hostname
        ));
    }
    Ok(())
}

impl HttpRouteConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.hostnames.is_empty() {
            return Err(anyhow!("HTTP route must have at least one hostname"));
        }
        for hostname in &self.hostnames {
            if hostname.is_empty() {
                return Err(anyhow!("Hostname cannot be empty"));
            }
            if hostname.contains(':') {
                return Err(anyhow!("Hostname must not contain port: {}", hostname));
            }
            validate_hostname_wildcard(hostname)?;
        }
        validate_route(&self.parent_refs, &self.rules, "HTTP", |r| r.validate())
    }
}

impl TcpRouteConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_route(&self.parent_refs, &self.rules, "TCP", |r| r.validate())
    }
}

impl GatewayConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            return Err(anyhow!("Gateway name cannot be empty"));
        }

        if self.listeners.is_empty() {
            return Err(anyhow!("Gateway must have at least one listener"));
        }

        for (i, listener) in self.listeners.iter().enumerate() {
            listener
                .validate()
                .map_err(|e| anyhow!("Listener {}: {}", i, e))?;
        }

        Ok(())
    }
}

impl ListenerConfig {
    fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            return Err(anyhow!("Listener name cannot be empty"));
        }

        validate_port(self.port, "Listener port")?;

        if let Some(ref interface) = self.interface {
            if interface.is_empty() {
                return Err(anyhow!("Interface name cannot be empty when specified"));
            }

            // Basic interface name validation (Linux interface naming rules)
            if interface.len() > 15 {
                return Err(anyhow!(
                    "Interface name '{}' is too long (max 15 characters)",
                    interface
                ));
            }

            if interface.contains(' ') || interface.contains('/') || interface.contains(':') {
                return Err(anyhow!(
                    "Interface name '{}' contains invalid characters",
                    interface
                ));
            }
        }

        if let Some(ref addr) = self.address {
            addr.parse::<std::net::IpAddr>().map_err(|_| {
                anyhow!(
                    "Invalid bind address '{}': must be a valid IPv4 or IPv6 address",
                    addr
                )
            })?;
        }

        // TLS config validation per protocol
        match self.protocol {
            Protocol::HTTPS => {
                let tls_cfg = self
                    .tls
                    .as_ref()
                    .ok_or_else(|| anyhow!("HTTPS listener requires tls config"))?;
                if tls_cfg.mode != TlsMode::Terminate {
                    return Err(anyhow!("HTTPS listener requires tls mode Terminate"));
                }
                if tls_cfg.certificate_refs.is_empty() {
                    return Err(anyhow!(
                        "HTTPS listener requires at least one certificateRef"
                    ));
                }
                for (i, cert_ref) in tls_cfg.certificate_refs.iter().enumerate() {
                    if cert_ref.name.is_empty() {
                        return Err(anyhow!("certificateRef {} name cannot be empty", i));
                    }
                }
            }
            Protocol::TLS => {
                let tls_cfg = self
                    .tls
                    .as_ref()
                    .ok_or_else(|| anyhow!("TLS listener requires tls config"))?;
                if tls_cfg.mode != TlsMode::Passthrough {
                    return Err(anyhow!("TLS listener requires tls mode Passthrough"));
                }
            }
            Protocol::HTTP | Protocol::TCP | Protocol::UDP => {
                if self.tls.is_some() {
                    return Err(anyhow!(
                        "{:?} listener must not have tls config",
                        self.protocol
                    ));
                }
            }
        }

        Ok(())
    }
}

impl ParentRef {
    fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            return Err(anyhow!("Parent ref name cannot be empty"));
        }

        Ok(())
    }
}

impl HttpRouteRule {
    fn validate(&self) -> Result<()> {
        let has_redirect = self
            .filters
            .iter()
            .any(|f| matches!(f, HttpRouteFilter::RequestRedirect { .. }));
        let has_rewrite = self
            .filters
            .iter()
            .any(|f| matches!(f, HttpRouteFilter::URLRewrite { .. }));

        if has_redirect && has_rewrite {
            return Err(anyhow!(
                "URLRewrite and RequestRedirect are mutually exclusive"
            ));
        }

        // backend_refs can be empty when a redirect filter is present
        if self.backend_refs.is_empty() && !has_redirect {
            return Err(anyhow!(
                "HTTP route rule must have at least one backend_ref (or a RequestRedirect filter)"
            ));
        }

        for (i, backend_ref) in self.backend_refs.iter().enumerate() {
            backend_ref
                .validate()
                .map_err(|e| anyhow!("Backend ref {}: {}", i, e))?;
        }

        for (i, match_rule) in self.matches.iter().enumerate() {
            match_rule
                .validate()
                .map_err(|e| anyhow!("Match rule {}: {}", i, e))?;
        }

        for (i, filter) in self.filters.iter().enumerate() {
            validate_filter(filter).map_err(|e| anyhow!("Filter {}: {}", i, e))?;
        }

        // Gateway API restricts ReplacePrefixMatch to PathPrefix matches (CEL
        // enforces this in-cluster; file mode must reject it here). The data
        // plane would otherwise splice the request path by pattern length.
        let uses_prefix_replace = self.filters.iter().any(|f| {
            let path = match f {
                HttpRouteFilter::RequestRedirect { config } => config.path.as_ref(),
                HttpRouteFilter::URLRewrite { config } => config.path.as_ref(),
                _ => None,
            };
            matches!(path, Some(HttpURLRewritePath::ReplacePrefixMatch { .. }))
        });
        if uses_prefix_replace {
            for (i, match_rule) in self.matches.iter().enumerate() {
                if let Some(p) = &match_rule.path {
                    if p.match_type != HttpPathMatchType::PathPrefix {
                        return Err(anyhow!(
                            "Match rule {}: ReplacePrefixMatch requires a PathPrefix path match, got {:?}",
                            i,
                            p.match_type
                        ));
                    }
                }
            }
        }

        Ok(())
    }
}

const VALID_HTTP_METHODS: &[&str] = &[
    "GET", "POST", "PUT", "DELETE", "HEAD", "OPTIONS", "PATCH", "CONNECT", "TRACE",
];

impl HttpRouteMatch {
    fn validate(&self) -> Result<()> {
        if let Some(ref method) = self.method {
            if !VALID_HTTP_METHODS
                .iter()
                .any(|m| m.eq_ignore_ascii_case(method))
            {
                return Err(anyhow!(
                    "Invalid HTTP method '{}'. Must be one of: {}",
                    method,
                    VALID_HTTP_METHODS.join(", ")
                ));
            }
        }

        if let Some(path_match) = &self.path {
            path_match.validate()?;
        }

        for (i, header_match) in self.headers.iter().enumerate() {
            validate_header_match(header_match)
                .map_err(|e| anyhow!("Header match {}: {}", i, e))?;
        }

        for (i, qp) in self.query_params.iter().enumerate() {
            validate_query_param_match(qp)
                .map_err(|e| anyhow!("Query param match {}: {}", i, e))?;
        }

        Ok(())
    }
}

impl HttpPathMatch {
    fn validate(&self) -> Result<()> {
        if self.match_type == HttpPathMatchType::RegularExpression {
            regex::Regex::new(&self.value)
                .map_err(|e| anyhow!("Invalid path regex '{}': {}", self.value, e))?;
        } else if !self.value.starts_with('/') {
            return Err(anyhow!(
                "Path match value must start with '/': {}",
                self.value
            ));
        }

        Ok(())
    }
}

impl TlsRouteConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.hostnames.is_empty() {
            return Err(anyhow!(
                "TLS route must have at least one hostname for SNI matching"
            ));
        }
        for hostname in &self.hostnames {
            if hostname.is_empty() {
                return Err(anyhow!("TLS route hostname cannot be empty"));
            }
            validate_hostname_wildcard(hostname)?;
        }
        validate_route(&self.parent_refs, &self.rules, "TLS", |r| r.validate())
    }
}

/// Shared by TCP/UDP/TLS route rules (they alias `L4RouteRule`). The
/// `validate_route` wrapper prefixes errors with the protocol name.
impl L4RouteRule {
    fn validate(&self) -> Result<()> {
        if self.backend_refs.is_empty() {
            return Err(anyhow!("route rule must have at least one backend_ref"));
        }

        for (i, backend_ref) in self.backend_refs.iter().enumerate() {
            backend_ref
                .validate()
                .map_err(|e| anyhow!("Backend ref {}: {}", i, e))?;
        }

        Ok(())
    }
}

impl UdpRouteConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_route(&self.parent_refs, &self.rules, "UDP", |r| r.validate())
    }
}

impl BackendRef {
    fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            return Err(anyhow!("Backend ref name cannot be empty"));
        }

        validate_port(self.port, "Backend ref port")?;

        if self.weight > 1_000_000 {
            return Err(anyhow!(
                "Backend weight must be 0-1000000, got {}",
                self.weight
            ));
        }

        Ok(())
    }
}

/// Validate port number is in valid range (1-65535)
fn validate_port(port: u16, name: &str) -> Result<()> {
    if port == 0 {
        return Err(anyhow!("{} must be between 1 and 65535", name));
    }
    Ok(())
}

fn validate_header_match(hm: &HttpHeaderMatch) -> Result<()> {
    if hm.name.is_empty() {
        return Err(anyhow!("Header match name cannot be empty"));
    }
    // RFC 7230: header field names are tokens (visible ASCII, no delimiters)
    if !hm
        .name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
    {
        return Err(anyhow!(
            "Header match name '{}' contains invalid characters",
            hm.name
        ));
    }
    if hm.match_type == StringMatchType::RegularExpression {
        regex::Regex::new(&hm.value)
            .map_err(|e| anyhow!("Invalid header match regex '{}': {}", hm.value, e))?;
    }
    Ok(())
}

fn validate_query_param_match(qp: &HttpQueryParamMatch) -> Result<()> {
    if qp.name.is_empty() {
        return Err(anyhow!("Query param name cannot be empty"));
    }
    if qp.name.contains('&') || qp.name.contains('=') {
        return Err(anyhow!(
            "Query param name '{}' must not contain '&' or '='",
            qp.name
        ));
    }
    if qp.match_type == StringMatchType::RegularExpression {
        regex::Regex::new(&qp.value)
            .map_err(|e| anyhow!("Invalid query param match regex '{}': {}", qp.value, e))?;
    }
    Ok(())
}

fn validate_rewrite_path(p: &HttpURLRewritePath) -> Result<()> {
    let value = match p {
        HttpURLRewritePath::ReplaceFullPath { value } => value,
        HttpURLRewritePath::ReplacePrefixMatch { value } => value,
    };
    if !value.starts_with('/') {
        return Err(anyhow!("path value must start with '/'"));
    }
    Ok(())
}

fn validate_filter(filter: &HttpRouteFilter) -> Result<()> {
    match filter {
        HttpRouteFilter::RequestRedirect { config } => {
            if config.scheme.is_none()
                && config.hostname.is_none()
                && config.port.is_none()
                && config.path.is_none()
            {
                return Err(anyhow!(
                    "RequestRedirect must set at least one of: scheme, hostname, port, path"
                ));
            }
            match config.status_code {
                301 | 302 | 303 | 307 | 308 => {}
                _ => {
                    return Err(anyhow!(
                        "RequestRedirect status_code must be 301, 302, 303, 307, or 308, got {}",
                        config.status_code
                    ))
                }
            }
            if let Some(ref p) = config.path {
                validate_rewrite_path(p).map_err(|e| anyhow!("RequestRedirect {}", e))?;
            }
        }
        HttpRouteFilter::RequestHeaderModifier { .. }
        | HttpRouteFilter::ResponseHeaderModifier { .. } => {}
        HttpRouteFilter::URLRewrite { config } => {
            if config.hostname.is_none() && config.path.is_none() {
                return Err(anyhow!(
                    "URLRewrite must set at least one of: hostname, path"
                ));
            }
            if let Some(ref p) = config.path {
                validate_rewrite_path(p).map_err(|e| anyhow!("URLRewrite {}", e))?;
            }
        }
        HttpRouteFilter::RequestMirror { config } => {
            config.backend_ref.validate()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_rule(filters: Vec<HttpRouteFilter>, backend_refs: Vec<BackendRef>) -> HttpRouteRule {
        HttpRouteRule {
            matches: vec![HttpRouteMatch::path_prefix("/")],
            filters,
            backend_refs,
            timeouts: None,
        }
    }

    fn default_backend() -> BackendRef {
        BackendRef {
            name: "127.0.0.1".to_string(),
            port: 8080,
            weight: 1,
            group: String::new(),
            kind: "Service".to_string(),
            filters: vec![],
            app_protocol: None,
        }
    }

    #[test]
    fn test_replace_prefix_match_requires_path_prefix() {
        let mut rule = make_rule(
            vec![HttpRouteFilter::URLRewrite {
                config: URLRewriteConfig {
                    hostname: None,
                    path: Some(HttpURLRewritePath::ReplacePrefixMatch {
                        value: "/new".to_string(),
                    }),
                },
            }],
            vec![default_backend()],
        );
        rule.matches = vec![HttpRouteMatch {
            method: None,
            path: Some(HttpPathMatch {
                match_type: HttpPathMatchType::RegularExpression,
                value: "^/api/.*$".to_string(),
            }),
            headers: vec![],
            query_params: vec![],
        }];
        let err = rule.validate().unwrap_err().to_string();
        assert!(err.contains("ReplacePrefixMatch requires a PathPrefix"));

        rule.matches = vec![HttpRouteMatch::path_prefix("/api")];
        assert!(rule.validate().is_ok());
    }

    #[test]
    fn test_url_rewrite_requires_at_least_one_field() {
        let rule = make_rule(
            vec![HttpRouteFilter::URLRewrite {
                config: URLRewriteConfig {
                    hostname: None,
                    path: None,
                },
            }],
            vec![default_backend()],
        );
        assert!(rule.validate().is_err());
    }

    #[test]
    fn test_url_rewrite_path_must_start_with_slash() {
        let rule = make_rule(
            vec![HttpRouteFilter::URLRewrite {
                config: URLRewriteConfig {
                    hostname: None,
                    path: Some(HttpURLRewritePath::ReplaceFullPath {
                        value: "foo".to_string(),
                    }),
                },
            }],
            vec![default_backend()],
        );
        assert!(rule.validate().is_err());
    }

    #[test]
    fn test_url_rewrite_and_redirect_mutually_exclusive() {
        let rule = make_rule(
            vec![
                HttpRouteFilter::URLRewrite {
                    config: URLRewriteConfig {
                        hostname: Some("new.com".to_string()),
                        path: None,
                    },
                },
                HttpRouteFilter::RequestRedirect {
                    config: RequestRedirectConfig {
                        scheme: Some("https".to_string()),
                        hostname: None,
                        port: None,
                        path: None,
                        status_code: 301,
                    },
                },
            ],
            vec![default_backend()],
        );
        assert!(rule.validate().is_err());
    }

    #[test]
    fn test_request_mirror_empty_backend_name() {
        let rule = make_rule(
            vec![HttpRouteFilter::RequestMirror {
                config: RequestMirrorConfig {
                    backend_ref: BackendRef {
                        name: "".to_string(),
                        port: 8080,
                        weight: 1,
                        group: String::new(),
                        kind: "Service".to_string(),
                        filters: vec![],
                        app_protocol: None,
                    },
                    percent: None,
                    fraction: None,
                },
            }],
            vec![default_backend()],
        );
        assert!(rule.validate().is_err());
    }

    #[test]
    fn test_url_rewrite_valid_full_path() {
        let rule = make_rule(
            vec![HttpRouteFilter::URLRewrite {
                config: URLRewriteConfig {
                    hostname: None,
                    path: Some(HttpURLRewritePath::ReplaceFullPath {
                        value: "/new".to_string(),
                    }),
                },
            }],
            vec![default_backend()],
        );
        assert!(rule.validate().is_ok());
    }

    #[test]
    fn test_url_rewrite_valid_prefix_match() {
        let rule = make_rule(
            vec![HttpRouteFilter::URLRewrite {
                config: URLRewriteConfig {
                    hostname: None,
                    path: Some(HttpURLRewritePath::ReplacePrefixMatch {
                        value: "/v2".to_string(),
                    }),
                },
            }],
            vec![default_backend()],
        );
        assert!(rule.validate().is_ok());
    }

    #[test]
    fn test_redirect_without_backends_ok() {
        let rule = make_rule(
            vec![HttpRouteFilter::RequestRedirect {
                config: RequestRedirectConfig {
                    scheme: Some("https".to_string()),
                    hostname: None,
                    port: None,
                    path: None,
                    status_code: 301,
                },
            }],
            vec![], // no backends
        );
        assert!(rule.validate().is_ok());
    }

    #[test]
    fn test_rewrite_requires_backends() {
        let rule = make_rule(
            vec![HttpRouteFilter::URLRewrite {
                config: URLRewriteConfig {
                    hostname: Some("new.com".to_string()),
                    path: None,
                },
            }],
            vec![], // no backends — rewrite needs a backend to forward to
        );
        assert!(rule.validate().is_err());
    }

    // --- TLS validation tests ---

    fn make_listener(protocol: Protocol, tls: Option<TlsConfig>) -> ListenerConfig {
        ListenerConfig {
            name: "test".to_string(),
            protocol,
            port: 443,
            target_port: None,
            hostname: None,
            address: None,
            interface: None,
            tls,
        }
    }

    #[test]
    fn test_https_requires_tls_config() {
        let listener = make_listener(Protocol::HTTPS, None);
        assert!(listener.validate().is_err());
    }

    #[test]
    fn test_https_requires_terminate_mode() {
        let listener = make_listener(
            Protocol::HTTPS,
            Some(TlsConfig {
                mode: TlsMode::Passthrough,
                certificate_refs: vec![CertificateRef {
                    name: "cert".to_string(),
                    ..Default::default()
                }],
            }),
        );
        assert!(listener.validate().is_err());
    }

    #[test]
    fn test_https_requires_certificate_refs() {
        let listener = make_listener(
            Protocol::HTTPS,
            Some(TlsConfig {
                mode: TlsMode::Terminate,
                certificate_refs: vec![],
            }),
        );
        assert!(listener.validate().is_err());
    }

    #[test]
    fn test_https_valid_tls_config() {
        let listener = make_listener(
            Protocol::HTTPS,
            Some(TlsConfig {
                mode: TlsMode::Terminate,
                certificate_refs: vec![CertificateRef {
                    name: "my-cert".to_string(),
                    ..Default::default()
                }],
            }),
        );
        assert!(listener.validate().is_ok());
    }

    #[test]
    fn test_tls_requires_passthrough_mode() {
        let listener = make_listener(
            Protocol::TLS,
            Some(TlsConfig {
                mode: TlsMode::Terminate,
                certificate_refs: vec![CertificateRef {
                    name: "cert".to_string(),
                    ..Default::default()
                }],
            }),
        );
        assert!(listener.validate().is_err());
    }

    #[test]
    fn test_tls_passthrough_valid() {
        let listener = make_listener(
            Protocol::TLS,
            Some(TlsConfig {
                mode: TlsMode::Passthrough,
                certificate_refs: vec![],
            }),
        );
        assert!(listener.validate().is_ok());
    }

    #[test]
    fn test_tls_requires_tls_config() {
        let listener = make_listener(Protocol::TLS, None);
        assert!(listener.validate().is_err());
    }

    #[test]
    fn test_http_rejects_tls_config() {
        let listener = make_listener(
            Protocol::HTTP,
            Some(TlsConfig {
                mode: TlsMode::Terminate,
                certificate_refs: vec![CertificateRef {
                    name: "cert".to_string(),
                    ..Default::default()
                }],
            }),
        );
        assert!(listener.validate().is_err());
    }

    #[test]
    fn test_tcp_rejects_tls_config() {
        let mut listener = make_listener(
            Protocol::TCP,
            Some(TlsConfig {
                mode: TlsMode::Passthrough,
                certificate_refs: vec![],
            }),
        );
        listener.port = 2222;
        assert!(listener.validate().is_err());
    }

    #[test]
    fn test_https_empty_cert_name_rejected() {
        let listener = make_listener(
            Protocol::HTTPS,
            Some(TlsConfig {
                mode: TlsMode::Terminate,
                certificate_refs: vec![CertificateRef {
                    name: "".to_string(),
                    ..Default::default()
                }],
            }),
        );
        assert!(listener.validate().is_err());
    }

    // --- Port-conflict validation keys on the actually-bound socket ---------

    fn listener_on(
        name: &str,
        port: u16,
        target_port: Option<u16>,
        address: Option<&str>,
    ) -> ListenerConfig {
        ListenerConfig {
            name: name.to_string(),
            protocol: Protocol::HTTP,
            port,
            target_port,
            hostname: None,
            address: address.map(str::to_string),
            interface: None,
            tls: None,
        }
    }

    fn config_with_listeners(listeners: Vec<ListenerConfig>) -> PortailConfig {
        PortailConfig {
            gateway: GatewayConfig {
                name: "gw".to_string(),
                listeners,
                addresses: vec![],
            },
            ..Default::default()
        }
    }

    /// Two listeners with distinct published ports whose targetPorts collide
    /// bind the same socket — must be rejected.
    #[test]
    fn test_target_port_collision_detected() {
        let cfg = config_with_listeners(vec![
            listener_on("a", 80, Some(8080), None),
            listener_on("b", 8081, Some(8080), None),
        ]);
        assert!(cfg.validate().is_err());
    }

    /// Same port number on different bind addresses is legal (distinct sockets).
    #[test]
    fn test_same_port_different_addresses_allowed() {
        let cfg = config_with_listeners(vec![
            listener_on("a", 8080, None, Some("127.0.0.1")),
            listener_on("b", 8080, None, Some("127.0.0.2")),
        ]);
        assert!(cfg.validate().is_ok());
    }

    /// Published-port collision resolved by distinct targetPorts is legal.
    #[test]
    fn test_same_published_port_distinct_target_ports_allowed() {
        let cfg = config_with_listeners(vec![
            listener_on("a", 80, Some(8080), None),
            listener_on("b", 80, Some(8081), Some("127.0.0.1")),
        ]);
        assert!(cfg.validate().is_ok());
    }

    // --- parentRef must resolve to a real listener ---------------------------

    #[test]
    fn test_parent_ref_with_unknown_section_name_rejected() {
        let mut cfg = config_with_listeners(vec![listener_on("http", 8080, None, None)]);
        cfg.http_routes.push(HttpRouteConfig {
            parent_refs: vec![ParentRef {
                name: "gw".to_string(),
                section_name: Some("no-such-listener".to_string()),
                port: None,
            }],
            hostnames: vec!["example.com".to_string()],
            rules: vec![HttpRouteRule {
                matches: vec![HttpRouteMatch::path_prefix("/")],
                filters: vec![],
                backend_refs: vec![BackendRef {
                    name: "127.0.0.1".to_string(),
                    port: 9000,
                    weight: 1,
                    group: String::new(),
                    kind: "Service".to_string(),
                    filters: vec![],
                    app_protocol: None,
                }],
                timeouts: None,
            }],
        });
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("does not match any Gateway listener"),
            "{}",
            err
        );
    }

    #[test]
    fn test_parent_ref_matching_section_name_accepted() {
        let mut cfg = config_with_listeners(vec![listener_on("http", 8080, None, None)]);
        cfg.http_routes.push(HttpRouteConfig {
            parent_refs: vec![ParentRef {
                name: "gw".to_string(),
                section_name: Some("http".to_string()),
                port: None,
            }],
            hostnames: vec!["example.com".to_string()],
            rules: vec![HttpRouteRule {
                matches: vec![HttpRouteMatch::path_prefix("/")],
                filters: vec![],
                backend_refs: vec![BackendRef {
                    name: "127.0.0.1".to_string(),
                    port: 9000,
                    weight: 1,
                    group: String::new(),
                    kind: "Service".to_string(),
                    filters: vec![],
                    app_protocol: None,
                }],
                timeouts: None,
            }],
        });
        assert!(cfg.validate().is_ok());
    }

    // --- Unknown fields in rule-level config are rejected at parse time ------

    /// A typo inside an HTTP rule (`weght`) must fail parsing, not be
    /// silently ignored while the route serves with a default value.
    #[test]
    fn test_unknown_field_in_backend_ref_rejected() {
        let json = r#"{
          "gateway": {"name": "gw", "listeners": [{"name": "http", "protocol": "HTTP", "port": 8080}]},
          "httpRoutes": [{
            "parentRefs": [{"name": "gw", "sectionName": "http"}],
            "hostnames": ["example.com"],
            "rules": [{
              "matches": [{"path": {"type": "PathPrefix", "value": "/"}}],
              "backendRefs": [{"name": "127.0.0.1", "port": 9000, "weght": 5}]
            }]
          }]
        }"#;
        let err = serde_json::from_str::<PortailConfig>(json).unwrap_err();
        assert!(err.to_string().contains("weght"), "{}", err);
    }

    #[test]
    fn test_unknown_field_in_rule_rejected() {
        let json = r#"{
          "gateway": {"name": "gw", "listeners": [{"name": "http", "protocol": "HTTP", "port": 8080}]},
          "httpRoutes": [{
            "parentRefs": [{"name": "gw", "sectionName": "http"}],
            "hostnames": ["example.com"],
            "rules": [{
              "matches": [{"path": {"type": "PathPrefix", "value": "/"}}],
              "backendRef": [{"name": "127.0.0.1", "port": 9000}],
              "backendRefs": [{"name": "127.0.0.1", "port": 9000}]
            }]
          }]
        }"#;
        let err = serde_json::from_str::<PortailConfig>(json).unwrap_err();
        assert!(err.to_string().contains("backendRef"), "{}", err);
    }
}
