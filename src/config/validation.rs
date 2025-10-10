use anyhow::{anyhow, Result};

use super::types::*;

impl UringRessConfig {
    /// Validate the entire configuration for consistency and correctness
    /// All validation happens once at startup
    pub fn validate(&self) -> Result<()> {
        self.gateway.validate()?;

        self.gateway.validate_worker_configs()
            .map_err(|e| anyhow!("Worker configuration validation failed: {}", e))?;

        for (i, route) in self.http_routes.iter().enumerate() {
            route.validate().map_err(|e| anyhow!("HTTP route {}: {}", i, e))?;
        }

        for (i, route) in self.tcp_routes.iter().enumerate() {
            route.validate().map_err(|e| anyhow!("TCP route {}: {}", i, e))?;
        }

        for (i, route) in self.udp_routes.iter().enumerate() {
            route.validate().map_err(|e| anyhow!("UDP route {}: {}", i, e))?;
        }

        self.validate_port_conflicts()?;

        Ok(())
    }

    fn validate_port_conflicts(&self) -> Result<()> {
        let mut used_ports = std::collections::HashSet::new();

        for listener in &self.gateway.listeners {
            if !used_ports.insert(listener.port) {
                return Err(anyhow!("Port conflict: Listener port {} already in use", listener.port));
            }
        }

        Ok(())
    }
}

impl HttpRouteConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.parent_refs.is_empty() {
            return Err(anyhow!("HTTP route must have at least one parent_ref"));
        }

        for parent_ref in &self.parent_refs {
            parent_ref.validate()?;
        }

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
            // Wildcard hostnames must be "*.domain" format
            if hostname.starts_with('*') && !hostname.starts_with("*.") {
                return Err(anyhow!("Wildcard hostname must use '*.domain' format: {}", hostname));
            }
        }

        if self.rules.is_empty() {
            return Err(anyhow!("HTTP route must have at least one rule"));
        }

        for (i, rule) in self.rules.iter().enumerate() {
            rule.validate().map_err(|e| anyhow!("HTTP route rule {}: {}", i, e))?;
        }

        Ok(())
    }
}

impl TcpRouteConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.parent_refs.is_empty() {
            return Err(anyhow!("TCP route must have at least one parent_ref"));
        }

        for parent_ref in &self.parent_refs {
            parent_ref.validate()?;
        }

        if self.rules.is_empty() {
            return Err(anyhow!("TCP route must have at least one rule"));
        }

        for (i, rule) in self.rules.iter().enumerate() {
            rule.validate().map_err(|e| anyhow!("TCP route rule {}: {}", i, e))?;
        }

        Ok(())
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
            listener.validate().map_err(|e| anyhow!("Listener {}: {}", i, e))?;
        }

        if self.worker_threads == 0 {
            return Err(anyhow!("Worker threads must be greater than 0"));
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
                return Err(anyhow!("Interface name '{}' is too long (max 15 characters)", interface));
            }

            if interface.contains(' ') || interface.contains('/') || interface.contains(':') {
                return Err(anyhow!("Interface name '{}' contains invalid characters", interface));
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
        // backend_refs can be empty when a redirect filter is present
        let has_redirect = self.filters.iter().any(|f| matches!(f, HttpRouteFilter::RequestRedirect { .. }));
        if self.backend_refs.is_empty() && !has_redirect {
            return Err(anyhow!("HTTP route rule must have at least one backend_ref (or a RequestRedirect filter)"));
        }

        for (i, backend_ref) in self.backend_refs.iter().enumerate() {
            backend_ref.validate().map_err(|e| anyhow!("Backend ref {}: {}", i, e))?;
        }

        for (i, match_rule) in self.matches.iter().enumerate() {
            match_rule.validate().map_err(|e| anyhow!("Match rule {}: {}", i, e))?;
        }

        for (i, filter) in self.filters.iter().enumerate() {
            validate_filter(filter).map_err(|e| anyhow!("Filter {}: {}", i, e))?;
        }

        Ok(())
    }
}

impl HttpRouteMatch {
    fn validate(&self) -> Result<()> {
        if let Some(path_match) = &self.path {
            path_match.validate()?;
        }

        for (i, header_match) in self.headers.iter().enumerate() {
            validate_header_match(header_match)
                .map_err(|e| anyhow!("Header match {}: {}", i, e))?;
        }

        Ok(())
    }
}

impl HttpPathMatch {
    fn validate(&self) -> Result<()> {
        if !self.value.starts_with('/') {
            return Err(anyhow!("Path match value must start with '/': {}", self.value));
        }

        Ok(())
    }
}

impl UdpRouteConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.parent_refs.is_empty() {
            return Err(anyhow!("UDP route must have at least one parent_ref"));
        }

        for parent_ref in &self.parent_refs {
            parent_ref.validate()?;
        }

        if self.rules.is_empty() {
            return Err(anyhow!("UDP route must have at least one rule"));
        }

        for (i, rule) in self.rules.iter().enumerate() {
            rule.validate().map_err(|e| anyhow!("UDP route rule {}: {}", i, e))?;
        }

        Ok(())
    }
}

impl UdpRouteRule {
    fn validate(&self) -> Result<()> {
        if self.backend_refs.is_empty() {
            return Err(anyhow!("UDP route rule must have at least one backend_ref"));
        }

        for (i, backend_ref) in self.backend_refs.iter().enumerate() {
            backend_ref.validate().map_err(|e| anyhow!("Backend ref {}: {}", i, e))?;
        }

        Ok(())
    }
}

impl TcpRouteRule {
    fn validate(&self) -> Result<()> {
        if self.backend_refs.is_empty() {
            return Err(anyhow!("TCP route rule must have at least one backend_ref"));
        }

        for (i, backend_ref) in self.backend_refs.iter().enumerate() {
            backend_ref.validate().map_err(|e| anyhow!("Backend ref {}: {}", i, e))?;
        }

        Ok(())
    }
}

impl BackendRef {
    fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            return Err(anyhow!("Backend ref name cannot be empty"));
        }

        validate_port(self.port, "Backend ref port")?;

        if self.weight > 1_000_000 {
            return Err(anyhow!("Backend weight must be 0-1000000, got {}", self.weight));
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
    if !hm.name.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)) {
        return Err(anyhow!("Header match name '{}' contains invalid characters", hm.name));
    }
    Ok(())
}

fn validate_filter(filter: &HttpRouteFilter) -> Result<()> {
    match filter {
        HttpRouteFilter::RequestRedirect { scheme, hostname, port, path, status_code } => {
            if scheme.is_none() && hostname.is_none() && port.is_none() && path.is_none() {
                return Err(anyhow!("RequestRedirect must set at least one of: scheme, hostname, port, path"));
            }
            match status_code {
                301 | 302 | 303 | 307 | 308 => {}
                _ => return Err(anyhow!("RequestRedirect status_code must be 301, 302, 303, 307, or 308, got {}", status_code)),
            }
            if let Some(p) = path {
                if !p.starts_with('/') {
                    return Err(anyhow!("RequestRedirect path must start with '/', got '{}'", p));
                }
            }
        }
        HttpRouteFilter::RequestHeaderModifier { .. } | HttpRouteFilter::ResponseHeaderModifier { .. } => {}
    }
    Ok(())
}
