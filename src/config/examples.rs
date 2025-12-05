use super::types::*;
use std::time::Duration;
use anyhow::{anyhow, Result};

impl PortailConfig {
    /// Generate a minimal configuration example
    pub fn generate_minimal() -> Self {
        Self {
            gateway: GatewayConfig {
                name: "minimal-gateway".to_string(),
                listeners: vec![
                    ListenerConfig {
                        name: "http".to_string(),
                        protocol: Protocol::HTTP,
                        port: 8080,
                        hostname: None,
                        address: None,
                        interface: None,
                        tls: None,
                    },
                ],
                worker_threads: 2,
            },
            http_routes: vec![
                HttpRouteConfig {
                    parent_refs: vec![ParentRef {
                        name: "minimal-gateway".to_string(),
                        section_name: Some("http".to_string()),
                    }],
                    hostnames: vec!["localhost".to_string()],
                    rules: vec![HttpRouteRule {
                        filters: vec![],
                        matches: vec![HttpRouteMatch::path_prefix("/api.json")],
                        backend_refs: vec![BackendRef {
                            name: "api-service".to_string(),
                            port: 3001,
                            weight: 1,
                            group: String::new(),
                            kind: "Service".to_string(),
                        }],
                        timeouts: None,
                    }],
                }
            ],
            tcp_routes: vec![],
            tls_routes: vec![],
            udp_routes: vec![],
            observability: ObservabilityConfig {
                logging: LoggingConfig {
                    level: LogLevel::Info,
                    format: LogFormat::Pretty,
                    output: LogOutput::Stdout,
                },
            },
            performance: PerformanceConfig {
                backend_timeout: Duration::from_secs(30),
                udp_session_timeout: Duration::from_secs(30),
            },
        }
    }

    /// Generate a development configuration example
    /// Multiple services with debug logging and relaxed timeouts
    pub fn generate_development() -> Self {
        Self {
            gateway: GatewayConfig {
                name: "development-gateway".to_string(),
                listeners: vec![
                    ListenerConfig {
                        name: "http".to_string(),
                        protocol: Protocol::HTTP,
                        port: 8080,
                        hostname: None,
                        address: Some("127.0.0.1".to_string()),
                        interface: Some("lo".to_string()),
                        tls: None,
                    },
                    ListenerConfig {
                        name: "https".to_string(),
                        protocol: Protocol::HTTPS,
                        port: 8443,
                        hostname: None,
                        address: Some("127.0.0.1".to_string()),
                        interface: Some("lo".to_string()),
                        tls: Some(TlsConfig {
                            mode: TlsMode::Terminate,
                            certificate_refs: vec![CertificateRef {
                                name: "example-cert".to_string(),
                                ..Default::default()
                            }],
                        }),
                    },
                    ListenerConfig {
                        name: "tcp-ssh".to_string(),
                        protocol: Protocol::TCP,
                        port: 2222,
                        hostname: None,
                        address: Some("127.0.0.1".to_string()),
                        interface: Some("lo".to_string()),
                        tls: None,
                    },
                ],
                worker_threads: 4,
            },
            http_routes: vec![
                HttpRouteConfig {
                    parent_refs: vec![ParentRef {
                        name: "development-gateway".to_string(),
                        section_name: Some("http".to_string()),
                    }],
                    hostnames: vec!["localhost".to_string()],
                    rules: vec![HttpRouteRule {
                        filters: vec![],
                        matches: vec![HttpRouteMatch::path_prefix("/")],
                        backend_refs: vec![BackendRef {
                            name: "frontend-service".to_string(),
                            port: 3001,
                            weight: 1,
                            group: String::new(),
                            kind: "Service".to_string(),
                        }],
                        timeouts: None,
                    }],
                },
                HttpRouteConfig {
                    parent_refs: vec![ParentRef {
                        name: "development-gateway".to_string(),
                        section_name: Some("http".to_string()),
                    }],
                    hostnames: vec!["localhost".to_string()],
                    rules: vec![HttpRouteRule {
                        filters: vec![],
                        matches: vec![HttpRouteMatch::path_prefix("/index.html")],
                        backend_refs: vec![
                            BackendRef { name: "frontend-service-1".to_string(), port: 3001, weight: 1, group: String::new(), kind: "Service".to_string() },
                            BackendRef { name: "frontend-service-2".to_string(), port: 3002, weight: 1, group: String::new(), kind: "Service".to_string() },
                            BackendRef { name: "frontend-service-3".to_string(), port: 3003, weight: 1, group: String::new(), kind: "Service".to_string() },
                        ],
                        timeouts: None,
                    }],
                },
                HttpRouteConfig {
                    parent_refs: vec![ParentRef { name: "development-gateway".to_string(), section_name: Some("http".to_string()) }],
                    hostnames: vec!["localhost".to_string()],
                    rules: vec![HttpRouteRule {
                        filters: vec![],
                        matches: vec![HttpRouteMatch::path_prefix("/api.json")],
                        backend_refs: vec![BackendRef { name: "api-service".to_string(), port: 3001, weight: 1, group: String::new(), kind: "Service".to_string() }],
                        timeouts: None,
                    }],
                },
                HttpRouteConfig {
                    parent_refs: vec![ParentRef { name: "development-gateway".to_string(), section_name: Some("http".to_string()) }],
                    hostnames: vec!["localhost".to_string()],
                    rules: vec![HttpRouteRule {
                        filters: vec![],
                        matches: vec![HttpRouteMatch::path_prefix("/users.json")],
                        backend_refs: vec![BackendRef { name: "users-service".to_string(), port: 3002, weight: 1, group: String::new(), kind: "Service".to_string() }],
                        timeouts: None,
                    }],
                },
                HttpRouteConfig {
                    parent_refs: vec![ParentRef { name: "development-gateway".to_string(), section_name: Some("http".to_string()) }],
                    hostnames: vec!["localhost".to_string()],
                    rules: vec![HttpRouteRule {
                        filters: vec![],
                        matches: vec![HttpRouteMatch::path_prefix("/dataset.json")],
                        backend_refs: vec![BackendRef { name: "dataset-service".to_string(), port: 3003, weight: 1, group: String::new(), kind: "Service".to_string() }],
                        timeouts: None,
                    }],
                },
                HttpRouteConfig {
                    parent_refs: vec![ParentRef { name: "development-gateway".to_string(), section_name: Some("http".to_string()) }],
                    hostnames: vec!["localhost".to_string()],
                    rules: vec![HttpRouteRule {
                        filters: vec![],
                        matches: vec![HttpRouteMatch::path_prefix("/health")],
                        backend_refs: vec![BackendRef { name: "api-service".to_string(), port: 3001, weight: 1, group: String::new(), kind: "Service".to_string() }],
                        timeouts: None,
                    }],
                },
                HttpRouteConfig {
                    parent_refs: vec![ParentRef { name: "development-gateway".to_string(), section_name: Some("http".to_string()) }],
                    hostnames: vec!["localhost".to_string()],
                    rules: vec![HttpRouteRule {
                        filters: vec![HttpRouteFilter::URLRewrite {
                            config: URLRewriteConfig {
                                hostname: None,
                                path: Some(HttpURLRewritePath::ReplacePrefixMatch { value: "/v2".to_string() }),
                            },
                        }],
                        matches: vec![HttpRouteMatch::path_prefix("/v1")],
                        backend_refs: vec![BackendRef { name: "api-service".to_string(), port: 3001, weight: 1, group: String::new(), kind: "Service".to_string() }],
                        timeouts: None,
                    }],
                },
                HttpRouteConfig {
                    parent_refs: vec![ParentRef { name: "development-gateway".to_string(), section_name: Some("http".to_string()) }],
                    hostnames: vec!["localhost".to_string()],
                    rules: vec![HttpRouteRule {
                        filters: vec![HttpRouteFilter::RequestMirror {
                            config: RequestMirrorConfig {
                                backend_ref: BackendRef { name: "127.0.0.1".to_string(), port: 9999, weight: 1, group: String::new(), kind: "Service".to_string() },
                            },
                        }],
                        matches: vec![HttpRouteMatch::path_prefix("/mirrored")],
                        backend_refs: vec![BackendRef { name: "api-service".to_string(), port: 3001, weight: 1, group: String::new(), kind: "Service".to_string() }],
                        timeouts: None,
                    }],
                },
            ],
            tls_routes: vec![],
            tcp_routes: vec![
                TcpRouteConfig {
                    parent_refs: vec![ParentRef {
                        name: "development-gateway".to_string(),
                        section_name: Some("tcp-ssh".to_string()),
                    }],
                    rules: vec![TcpRouteRule {
                        backend_refs: vec![BackendRef {
                            name: "ssh-service".to_string(),
                            port: 22,
                            weight: 1,
                            group: String::new(),
                            kind: "Service".to_string(),
                        }],
                    }],
                }
            ],
            udp_routes: vec![],
            observability: ObservabilityConfig {
                logging: LoggingConfig {
                    level: LogLevel::Debug,
                    format: LogFormat::Pretty,
                    output: LogOutput::Stdout,
                },
            },
            performance: PerformanceConfig {
                backend_timeout: Duration::from_secs(30),
                udp_session_timeout: Duration::from_secs(30),
            },
        }
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

pub fn print_example_config_info() {
    println!("minimal       - Basic single-service setup with default worker configuration");
    println!("development   - Multi-service development with explicit worker configs (SCP-optimized)");
}
