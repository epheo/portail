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
        if self.backend_refs.is_empty() {
            return Err(anyhow!("HTTP route rule must have at least one backend_ref"));
        }

        for (i, backend_ref) in self.backend_refs.iter().enumerate() {
            backend_ref.validate().map_err(|e| anyhow!("Backend ref {}: {}", i, e))?;
        }

        for (i, match_rule) in self.matches.iter().enumerate() {
            match_rule.validate().map_err(|e| anyhow!("Match rule {}: {}", i, e))?;
        }

        Ok(())
    }
}

impl HttpRouteMatch {
    fn validate(&self) -> Result<()> {
        if let Some(path_match) = &self.path {
            path_match.validate()?;
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
