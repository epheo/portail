use std::path::Path;
use anyhow::{anyhow, Result};

use super::types::*;

impl UringRessConfig {
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

impl UringRessConfig {
    /// Convert configuration to RouteTable for runtime use
    /// All route processing happens once at startup - zero runtime overhead
    pub fn to_route_table(&self) -> Result<crate::routing::RouteTable> {
        use crate::routing::{RouteTable, Backend};

        let mut route_table = RouteTable::new();

        tracing::debug!("Converting {} HTTP routes to route table", self.http_routes.len());

        for (route_idx, http_route) in self.http_routes.iter().enumerate() {
            tracing::debug!("Processing HTTP route {}: {} hostnames, {} rules",
                route_idx, http_route.hostnames.len(), http_route.rules.len());
            for hostname in &http_route.hostnames {
                tracing::debug!("  Processing hostname: {}", hostname);
                for (rule_idx, rule) in http_route.rules.iter().enumerate() {
                    tracing::debug!("    Processing rule {}: {} matches, {} backend_refs",
                        rule_idx, rule.matches.len(), rule.backend_refs.len());
                    for route_match in &rule.matches {
                        let path = if let Some(path_match) = &route_match.path {
                            &path_match.value
                        } else {
                            "/"
                        };

                        tracing::debug!("      Adding route: {} {} -> {} backends",
                            hostname, path, rule.backend_refs.len());

                        let mut backends = Vec::new();
                        for backend_ref in &rule.backend_refs {
                            let backend = Backend::new(
                                backend_ref.name.clone(),
                                backend_ref.port
                            )?;
                            backends.push(backend);
                            tracing::debug!("        Backend: {}:{}", backend_ref.name, backend_ref.port);
                        }

                        route_table.add_http_route(hostname, path, backends);
                    }
                }
            }
        }

        for tcp_route in &self.tcp_routes {
            if let Some(parent_ref) = tcp_route.parent_refs.first() {
                if let Some(section_name) = &parent_ref.section_name {
                    if let Some(listener) = self.gateway.listeners.iter()
                        .find(|l| l.name == *section_name && l.protocol == Protocol::TCP) {

                        for rule in &tcp_route.rules {
                            let mut backends = Vec::new();
                            for backend_ref in &rule.backend_refs {
                                let backend = Backend::new(
                                    backend_ref.name.clone(),
                                    backend_ref.port
                                )?;
                                backends.push(backend);
                            }

                            route_table.add_tcp_route(listener.port, backends);
                        }
                    }
                }
            }
        }

        for udp_route in &self.udp_routes {
            if let Some(parent_ref) = udp_route.parent_refs.first() {
                if let Some(section_name) = &parent_ref.section_name {
                    if let Some(listener) = self.gateway.listeners.iter()
                        .find(|l| l.name == *section_name && l.protocol == Protocol::UDP) {

                        for rule in &udp_route.rules {
                            let mut backends = Vec::new();
                            for backend_ref in &rule.backend_refs {
                                let backend = Backend::new(
                                    backend_ref.name.clone(),
                                    backend_ref.port
                                )?;
                                backends.push(backend);
                            }

                            route_table.add_udp_route(listener.port, backends);
                        }
                    }
                }
            }
        }

        tracing::debug!("Route table conversion completed: {} HTTP routes, {} TCP routes, {} UDP routes",
            route_table.http_routes.len(), route_table.tcp_routes.len(), route_table.udp_routes.len());

        Ok(route_table)
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::super::parsing::{parse_size, parse_duration};
    use std::time::Duration;

    #[test]
    fn test_parse_size() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("1KB").unwrap(), 1024);
        assert_eq!(parse_size("1mb").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("2GB").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("0.5MB").unwrap(), 512 * 1024);

        assert!(parse_size("invalid").is_err());
        assert!(parse_size("-1MB").is_err());
    }

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
        let config = UringRessConfig::default();
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
                "worker_threads": 8
            },
            "performance": {
                "keep_alive_timeout": "10s"
            }
        }"#;

        let config: UringRessConfig = serde_json::from_str(json).unwrap();
        assert!(config.validate().is_ok());
        assert_eq!(config.gateway.listeners[0].port, 9000);
        assert_eq!(config.performance.keep_alive_timeout, Duration::from_secs(10));
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
                "worker_threads": 8
            },
            "http_routes": [
                {
                    "parent_refs": [
                        {
                            "name": "test-gateway",
                            "section_name": "http-8080"
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
                            "backend_refs": [
                                {
                                    "name": "api-service",
                                    "port": 3000
                                }
                            ]
                        }
                    ]
                }
            ],
            "tcp_routes": [
                {
                    "parent_refs": [
                        {
                            "name": "test-gateway",
                            "section_name": "tcp-5432"
                        }
                    ],
                    "rules": [
                        {
                            "backend_refs": [
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

        let config: UringRessConfig = serde_json::from_str(json).unwrap();
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
        let minimal = UringRessConfig::generate_minimal();
        assert!(minimal.validate().is_ok());
        assert_eq!(minimal.http_routes.len(), 1);
        assert_eq!(minimal.tcp_routes.len(), 0);

        let development = UringRessConfig::generate_development();
        assert!(development.validate().is_ok());
        assert_eq!(development.http_routes.len(), 6);
        assert_eq!(development.tcp_routes.len(), 1);
    }

    #[test]
    fn test_serialization_roundtrip() {
        let config = UringRessConfig::generate_minimal();

        let json = config.to_json_pretty().unwrap();
        let parsed_json: UringRessConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed_json.validate().is_ok());

        let yaml = config.to_yaml_pretty().unwrap();
        let parsed_yaml: UringRessConfig = serde_yaml::from_str(&yaml).unwrap();
        assert!(parsed_yaml.validate().is_ok());
    }
}
