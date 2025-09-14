use super::core::*;
use std::time::Duration;
use anyhow::{anyhow, Result};


// Configuration example generation
impl UringRessConfig {
    /// Generate a minimal configuration example
    /// Single HTTP route with basic settings - ideal for getting started
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
                        tls: None,
                        address: None,
                        interface: None,
                    },
                ],
                workers: None, // Uses default: 1 HTTP worker (8KB buffers) + 1 TCP worker (64KB buffers)
                worker_threads: 2,
                api_port: 8081,
                api_enabled: true,
                io_uring_queue_depth: 128,
                connection_pool_size: 32,
                buffer_pool_size: 32 * 1024 * 1024, // 32MB
                network_interface: Some("eth0".to_string()),
            },
            backend_pool: BackendPoolConfig {
                default_pool_size: 5,
                tcp_keepalive: false, // Disabled for minimal setup
                tcp_keepalive_idle: Duration::from_secs(7200),
                tcp_keepalive_interval: Duration::from_secs(75),
                tcp_keepalive_probes: 9,
            },
            http_routes: vec![
                HttpRouteConfig {
                    parent_refs: vec![ParentRef {
                        name: "minimal-gateway".to_string(),
                        namespace: None,
                        section_name: Some("http".to_string()),
                        port: None,
                    }],
                    hostnames: vec!["localhost".to_string()],
                    rules: vec![HttpRouteRule {
                        matches: vec![HttpRouteMatch::path_prefix("/api.json")],
                        backend_refs: vec![BackendRef {
                            name: "api-service".to_string(),
                            namespace: None,
                            port: 3001,
                            weight: 1,
                            kind: None,
                            group: None,
                        }],
                    }],
                }
            ],
            tcp_routes: vec![],
            observability: ObservabilityConfig {
                logging: LoggingConfig {
                    level: LogLevel::Info,
                    format: LogFormat::Pretty,
                    output: LogOutput::Stdout,
                },
                metrics: MetricsConfig {
                    enabled: false, // Disabled in minimal setup
                    port: 9091,
                    path: "/metrics".to_string(),
                },
                health_check: HealthCheckConfig {
                    enabled: true,
                    interval: Duration::from_secs(30),
                    timeout: Duration::from_secs(5),
                    failure_threshold: 3,
                    success_threshold: 2,
                },
            },
            performance: PerformanceConfig {
                backend_response_timeout: Duration::from_secs(5),
                connection_pool_size: 32,
                max_requests_per_connection: 50,
                standard_buffer_size: 4096,  // 4KB
                large_buffer_size: 8192,     // 8KB
                header_buffer_size: 2048,    // 2KB
                connection_max_age: Duration::from_secs(60),
                circuit_breaker_timeout: Duration::from_secs(5),
                keep_alive_timeout: Duration::from_secs(5),
                circuit_breaker_failure_threshold: 3,
                max_retries: 1,
                tcp_nodelay: true,
                so_reuseport: true,
                cpu_affinity: None,
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
                        tls: None,
                        address: Some("127.0.0.1".to_string()),
                        interface: Some("lo".to_string()),
                    },
                    ListenerConfig {
                        name: "tcp-ssh".to_string(),
                        protocol: Protocol::TCP,
                        port: 2222,
                        hostname: None,
                        tls: None,
                        address: Some("127.0.0.1".to_string()),
                        interface: Some("lo".to_string()),
                    },
                ],
                workers: Some(vec![
                    // HTTP workers - optimized for low latency
                    WorkerConfig {
                        id: 0,
                        protocol: Protocol::HTTP,
                        buffer_size: 8192,  // 8KB for HTTP headers
                        buffer_count: 256,  // Increased for high concurrent load
                        tcp_nodelay: false,
                    },
                    WorkerConfig {
                        id: 1,
                        protocol: Protocol::HTTP,
                        buffer_size: 8192,  // 8KB for HTTP headers  
                        buffer_count: 256,  // Increased for high concurrent load
                        tcp_nodelay: false,
                    },
                    // TCP workers - optimized for bulk transfers (SCP, rsync, large file transfers)
                    WorkerConfig {
                        id: 2,
                        protocol: Protocol::TCP,
                        buffer_size: 65536, // 64KB for bulk data transfers
                        buffer_count: 16,   // Fewer buffers due to larger size
                        tcp_nodelay: true,  // Immediate transmission for interactive protocols
                    },
                    WorkerConfig {
                        id: 3,
                        protocol: Protocol::TCP,
                        buffer_size: 65536, // 64KB for bulk data transfers
                        buffer_count: 16,   // Fewer buffers due to larger size
                        tcp_nodelay: true,  // Immediate transmission for interactive protocols
                    },
                ]),
                worker_threads: 4, // Explicit worker count matches workers array
                api_port: 8081,
                api_enabled: true,
                io_uring_queue_depth: 256,
                connection_pool_size: 64,
                buffer_pool_size: 64 * 1024 * 1024, // 64MB
                network_interface: Some("eth0".to_string()),
            },
            backend_pool: BackendPoolConfig {
                default_pool_size: 10,
                tcp_keepalive: true, // Enable for development debugging
                tcp_keepalive_idle: Duration::from_secs(600), // 10 minutes
                tcp_keepalive_interval: Duration::from_secs(60), // 1 minute
                tcp_keepalive_probes: 5,
            },
            http_routes: vec![
                HttpRouteConfig {
                    parent_refs: vec![ParentRef {
                        name: "development-gateway".to_string(),
                        namespace: None,
                        section_name: Some("http".to_string()),
                        port: None,
                    }],
                    hostnames: vec!["localhost".to_string()],
                    rules: vec![HttpRouteRule {
                        matches: vec![HttpRouteMatch::path_prefix("/")],
                        backend_refs: vec![BackendRef {
                            name: "frontend-service".to_string(),
                            namespace: None,
                            port: 3001,
                            weight: 1,
                            kind: None,
                            group: None,
                        }],
                    }],
                },
                // /index.html route with round-robin across all 3 backends
                HttpRouteConfig {
                    parent_refs: vec![ParentRef {
                        name: "development-gateway".to_string(),
                        namespace: None,
                        section_name: Some("http".to_string()),
                        port: None,
                    }],
                    hostnames: vec!["localhost".to_string()],
                    rules: vec![HttpRouteRule {
                        matches: vec![HttpRouteMatch::path_prefix("/index.html")],
                        backend_refs: vec![
                            BackendRef {
                                name: "frontend-service-1".to_string(),
                                namespace: None,
                                port: 3001,
                                weight: 1,
                                kind: None,
                                group: None,
                            },
                            BackendRef {
                                name: "frontend-service-2".to_string(),
                                namespace: None,
                                port: 3002,
                                weight: 1,
                                kind: None,
                                group: None,
                            },
                            BackendRef {
                                name: "frontend-service-3".to_string(),
                                namespace: None,
                                port: 3003,
                                weight: 1,
                                kind: None,
                                group: None,
                            },
                        ],
                    }],
                },
                HttpRouteConfig {
                    parent_refs: vec![ParentRef {
                        name: "development-gateway".to_string(),
                        namespace: None,
                        section_name: Some("http".to_string()),
                        port: None,
                    }],
                    hostnames: vec!["localhost".to_string()],
                    rules: vec![HttpRouteRule {
                        matches: vec![HttpRouteMatch::path_prefix("/api.json")],
                        backend_refs: vec![BackendRef {
                            name: "api-service".to_string(),
                            namespace: None,
                            port: 3001,
                            weight: 1,
                            kind: None,
                            group: None,
                        }],
                    }],
                },
                HttpRouteConfig {
                    parent_refs: vec![ParentRef {
                        name: "development-gateway".to_string(),
                        namespace: None,
                        section_name: Some("http".to_string()),
                        port: None,
                    }],
                    hostnames: vec!["localhost".to_string()],
                    rules: vec![HttpRouteRule {
                        matches: vec![HttpRouteMatch::path_prefix("/users.json")],
                        backend_refs: vec![BackendRef {
                            name: "users-service".to_string(),
                            namespace: None,
                            port: 3002,
                            weight: 1,
                            kind: None,
                            group: None,
                        }],
                    }],
                },
                HttpRouteConfig {
                    parent_refs: vec![ParentRef {
                        name: "development-gateway".to_string(),
                        namespace: None,
                        section_name: Some("http".to_string()),
                        port: None,
                    }],
                    hostnames: vec!["localhost".to_string()],
                    rules: vec![HttpRouteRule {
                        matches: vec![HttpRouteMatch::path_prefix("/dataset.json")],
                        backend_refs: vec![BackendRef {
                            name: "dataset-service".to_string(),
                            namespace: None,
                            port: 3003,
                            weight: 1,
                            kind: None,
                            group: None,
                        }],
                    }],
                },
                HttpRouteConfig {
                    parent_refs: vec![ParentRef {
                        name: "development-gateway".to_string(),
                        namespace: None,
                        section_name: Some("http".to_string()),
                        port: None,
                    }],
                    hostnames: vec!["localhost".to_string()],
                    rules: vec![HttpRouteRule {
                        matches: vec![HttpRouteMatch::path_prefix("/health")],
                        backend_refs: vec![BackendRef {
                            name: "api-service".to_string(),
                            namespace: None,
                            port: 3001,
                            weight: 1,
                            kind: None,
                            group: None,
                        }],
                    }],
                },
            ],
            tcp_routes: vec![
                TcpRouteConfig {
                    parent_refs: vec![ParentRef {
                        name: "development-gateway".to_string(),
                        namespace: None,
                        section_name: Some("tcp-ssh".to_string()),
                        port: None,
                    }],
                    rules: vec![TcpRouteRule {
                        backend_refs: vec![BackendRef {
                            name: "ssh-service".to_string(),
                            namespace: None,
                            port: 22,
                            weight: 1,
                            kind: None,
                            group: None,
                        }],
                    }],
                }
            ],
            observability: ObservabilityConfig {
                logging: LoggingConfig {
                    level: LogLevel::Debug,
                    format: LogFormat::Pretty,
                    output: LogOutput::Stdout,
                },
                metrics: MetricsConfig {
                    enabled: true,
                    port: 9091,
                    path: "/metrics".to_string(),
                },
                health_check: HealthCheckConfig {
                    enabled: true,
                    interval: Duration::from_secs(15), // More frequent in dev
                    timeout: Duration::from_secs(10), // Longer timeout for debugging
                    failure_threshold: 5,
                    success_threshold: 2,
                },
            },
            performance: PerformanceConfig {
                backend_response_timeout: Duration::from_secs(30), // Relaxed for debugging
                connection_pool_size: 64,
                max_requests_per_connection: 100,
                standard_buffer_size: 8192,   // 8KB
                large_buffer_size: 16384,     // 16KB
                header_buffer_size: 4096,     // 4KB
                connection_max_age: Duration::from_secs(300),  // 5 minutes
                circuit_breaker_timeout: Duration::from_secs(15),
                keep_alive_timeout: Duration::from_secs(10),
                circuit_breaker_failure_threshold: 5,
                max_retries: 3,
                tcp_nodelay: true,
                so_reuseport: true,
                cpu_affinity: None,
            },
        }
    }

    /// Generate a production configuration example
    /// Configured for high performance and reliability
    pub fn generate_production() -> Self {
        Self {
            gateway: GatewayConfig {
                name: "production-gateway".to_string(),
                listeners: vec![
                    ListenerConfig {
                        name: "http-80".to_string(),
                        protocol: Protocol::HTTP,
                        port: 80,
                        hostname: None,
                        tls: None,
                        address: None,
                        interface: None,
                    },
                    ListenerConfig {
                        name: "http-8080".to_string(),
                        protocol: Protocol::HTTP,
                        port: 8080,
                        hostname: None,
                        tls: None,
                        address: None,
                        interface: None,
                    },
                    ListenerConfig {
                        name: "tcp-5432".to_string(),
                        protocol: Protocol::TCP,
                        port: 5432,
                        hostname: None,
                        tls: None,
                        address: None,
                        interface: None,
                    },
                    ListenerConfig {
                        name: "tcp-6379".to_string(),
                        protocol: Protocol::TCP,
                        port: 6379,
                        hostname: None,
                        tls: None,
                        address: None,
                        interface: None,
                    },
                ],
                workers: Some({
                    let cpu_count = num_cpus::get();
                    let mut workers = Vec::with_capacity(cpu_count);
                    
                    // Production: 50% HTTP workers, 50% TCP workers for balanced workload
                    let http_workers = cpu_count / 2;
                    
                    // HTTP workers - optimized for high-concurrency web traffic
                    for id in 0..http_workers {
                        workers.push(WorkerConfig {
                            id,
                            protocol: Protocol::HTTP,
                            buffer_size: 8192,  // 8KB for HTTP headers and small responses
                            buffer_count: 128,  // More buffers for high concurrency
                            tcp_nodelay: false, // HTTP can benefit from Nagle's algorithm
                        });
                    }
                    
                    // TCP workers - optimized for high-throughput bulk transfers  
                    for id in http_workers..cpu_count {
                        workers.push(WorkerConfig {
                            id,
                            protocol: Protocol::TCP,
                            buffer_size: 131072, // 128KB for maximum throughput
                            buffer_count: 32,   // Balanced buffer count for production
                            tcp_nodelay: true,  // Immediate transmission for protocols like SCP
                        });
                    }
                    
                    workers
                }),
                worker_threads: num_cpus::get(), // Match dynamic worker count
                api_port: 8090,
                api_enabled: true,
                io_uring_queue_depth: 512,
                connection_pool_size: 256,
                buffer_pool_size: 256 * 1024 * 1024, // 256MB
                network_interface: Some("eth0".to_string()),
            },
            http_routes: vec![
                // Integration test routes for compatibility
                HttpRouteConfig {
                    parent_refs: vec![
                        ParentRef {
                            name: "production-gateway".to_string(),
                            namespace: None,
                            section_name: Some("http-8080".to_string()),
                            port: Some(8080),
                        }
                    ],
                    hostnames: vec!["localhost".to_string()],
                    rules: vec![
                        HttpRouteRule {
                            matches: vec![
                                HttpRouteMatch {
                                    path: Some(HttpPathMatch {
                                        match_type: HttpPathMatchType::PathPrefix,
                                        value: "/api.json".to_string(),
                                    }),
                                    headers: None,
                                    query_params: None,
                                    method: None,
                                }
                            ],
                            backend_refs: vec![
                                BackendRef {
                                    name: "api-service".to_string(),
                                    namespace: None,
                                    port: 3001,
                                    weight: 100,
                                    kind: None,
                                    group: None,
                                }
                            ],
                        },
                        HttpRouteRule {
                            matches: vec![
                                HttpRouteMatch {
                                    path: Some(HttpPathMatch {
                                        match_type: HttpPathMatchType::PathPrefix,
                                        value: "/users.json".to_string(),
                                    }),
                                    headers: None,
                                    query_params: None,
                                    method: None,
                                }
                            ],
                            backend_refs: vec![
                                BackendRef {
                                    name: "users-service".to_string(),
                                    namespace: None,
                                    port: 3002,
                                    weight: 100,
                                    kind: None,
                                    group: None,
                                }
                            ],
                        },
                        HttpRouteRule {
                            matches: vec![
                                HttpRouteMatch {
                                    path: Some(HttpPathMatch {
                                        match_type: HttpPathMatchType::PathPrefix,
                                        value: "/dataset.json".to_string(),
                                    }),
                                    headers: None,
                                    query_params: None,
                                    method: None,
                                }
                            ],
                            backend_refs: vec![
                                BackendRef {
                                    name: "dataset-service".to_string(),
                                    namespace: None,
                                    port: 3003,
                                    weight: 100,
                                    kind: None,
                                    group: None,
                                }
                            ],
                        },
                    ],
                },
                // Production example routes for API gateway
                HttpRouteConfig {
                    parent_refs: vec![
                        ParentRef {
                            name: "production-gateway".to_string(),
                            namespace: None,
                            section_name: Some("http-80".to_string()),
                            port: Some(80),
                        }
                    ],
                    hostnames: vec!["api.example.com".to_string()],
                    rules: vec![
                        HttpRouteRule {
                            matches: vec![
                                HttpRouteMatch {
                                    path: Some(HttpPathMatch {
                                        match_type: HttpPathMatchType::PathPrefix,
                                        value: "/api".to_string(),
                                    }),
                                    headers: None,
                                    query_params: None,
                                    method: None,
                                }
                            ],
                            backend_refs: vec![
                                BackendRef {
                                    name: "api-backend".to_string(),
                                    namespace: None,
                                    port: 8000,
                                    weight: 100,
                                    kind: None,
                                    group: None,
                                }
                            ],
                        },
                    ],
                },
                // Production example routes for web frontend
                HttpRouteConfig {
                    parent_refs: vec![
                        ParentRef {
                            name: "production-gateway".to_string(),
                            namespace: None,
                            section_name: Some("http-80".to_string()),
                            port: Some(80),
                        }
                    ],
                    hostnames: vec!["www.example.com".to_string()],
                    rules: vec![
                        HttpRouteRule {
                            matches: vec![
                                HttpRouteMatch {
                                    path: Some(HttpPathMatch {
                                        match_type: HttpPathMatchType::PathPrefix,
                                        value: "/".to_string(),
                                    }),
                                    headers: None,
                                    query_params: None,
                                    method: None,
                                }
                            ],
                            backend_refs: vec![
                                BackendRef {
                                    name: "web-frontend".to_string(),
                                    namespace: None,
                                    port: 80,
                                    weight: 100,
                                    kind: None,
                                    group: None,
                                }
                            ],
                        },
                    ],
                },
            ],
            backend_pool: BackendPoolConfig {
                default_pool_size: 20,
                tcp_keepalive: true, // Enable for production stability
                tcp_keepalive_idle: Duration::from_secs(7200), // 2 hours
                tcp_keepalive_interval: Duration::from_secs(75), // 75 seconds
                tcp_keepalive_probes: 9,
            },
            tcp_routes: vec![
                TcpRouteConfig {
                    parent_refs: vec![
                        ParentRef {
                            name: "production-gateway".to_string(),
                            namespace: None,
                            section_name: Some("tcp-5432".to_string()),
                            port: Some(5432),
                        }
                    ],
                    rules: vec![
                        TcpRouteRule {
                            backend_refs: vec![
                                BackendRef {
                                    name: "postgres-primary".to_string(),
                                    namespace: None,
                                    port: 5432,
                                    weight: 50,
                                    kind: None,
                                    group: None,
                                },
                                BackendRef {
                                    name: "postgres-secondary".to_string(),
                                    namespace: None,
                                    port: 5432,
                                    weight: 50,
                                    kind: None,
                                    group: None,
                                },
                            ],
                        }
                    ],
                },
                TcpRouteConfig {
                    parent_refs: vec![
                        ParentRef {
                            name: "production-gateway".to_string(),
                            namespace: None,
                            section_name: Some("tcp-6379".to_string()),
                            port: Some(6379),
                        }
                    ],
                    rules: vec![
                        TcpRouteRule {
                            backend_refs: vec![
                                BackendRef {
                                    name: "redis-cache".to_string(),
                                    namespace: None,
                                    port: 6379,
                                    weight: 100,
                                    kind: None,
                                    group: None,
                                },
                            ],
                        }
                    ],
                },
            ],
            observability: ObservabilityConfig {
                logging: LoggingConfig {
                    level: LogLevel::Warn,
                    format: LogFormat::Json,
                    output: LogOutput::File("/var/log/uringress/uringress.log".to_string()),
                },
                metrics: MetricsConfig {
                    enabled: true,
                    port: 9090,
                    path: "/metrics".to_string(),
                },
                health_check: HealthCheckConfig {
                    enabled: true,
                    interval: Duration::from_secs(10),
                    timeout: Duration::from_secs(3),
                    failure_threshold: 3,
                    success_threshold: 1,
                },
            },
            performance: PerformanceConfig {
                backend_response_timeout: Duration::from_secs(5),
                connection_pool_size: 256,
                max_requests_per_connection: 1000,
                standard_buffer_size: 16384,   // 16KB
                large_buffer_size: 32768,      // 32KB
                header_buffer_size: 8192,      // 8KB
                connection_max_age: Duration::from_secs(600),  // 10 minutes
                circuit_breaker_timeout: Duration::from_secs(5),
                keep_alive_timeout: Duration::from_secs(5),
                circuit_breaker_failure_threshold: 5,
                max_retries: 2,
                tcp_nodelay: true,
                so_reuseport: true,
                cpu_affinity: Some((0..num_cpus::get()).collect()),
            },
        }
    }

    /// Generate a microservices configuration example
    /// Complex routing scenarios with multiple services and load balancing
    pub fn generate_microservices() -> Self {
        Self {
            gateway: GatewayConfig {
                name: "microservices-gateway".to_string(),
                listeners: vec![
                    ListenerConfig {
                        name: "http-8080".to_string(),
                        protocol: Protocol::HTTP,
                        port: 8080,
                        hostname: None,
                        tls: None,
                        address: None,
                        interface: None,
                    },
                    ListenerConfig {
                        name: "tcp-5432".to_string(),
                        protocol: Protocol::TCP,
                        port: 5432,
                        hostname: None,
                        tls: None,
                        address: None,
                        interface: None,
                    },
                    ListenerConfig {
                        name: "tcp-6379".to_string(),
                        protocol: Protocol::TCP,
                        port: 6379,
                        hostname: None,
                        tls: None,
                        address: None,
                        interface: None,
                    },
                    ListenerConfig {
                        name: "tcp-3306".to_string(),
                        protocol: Protocol::TCP,
                        port: 3306,
                        hostname: None,
                        tls: None,
                        address: None,
                        interface: None,
                    },
                    ListenerConfig {
                        name: "tcp-27017".to_string(),
                        protocol: Protocol::TCP,
                        port: 27017,
                        hostname: None,
                        tls: None,
                        address: None,
                        interface: None,
                    },
                ],
                workers: Some(vec![
                    // HTTP workers - optimized for API microservices
                    WorkerConfig {
                        id: 0,
                        protocol: Protocol::HTTP,
                        buffer_size: 8192,  // 8KB for REST API responses
                        buffer_count: 96,   // High buffer count for API burst traffic
                        tcp_nodelay: true,  // Low latency for API calls
                    },
                    WorkerConfig {
                        id: 1,
                        protocol: Protocol::HTTP,
                        buffer_size: 8192,  // 8KB for REST API responses
                        buffer_count: 96,   // High buffer count for API burst traffic
                        tcp_nodelay: true,  // Low latency for API calls
                    },
                    // Database connection workers - PostgreSQL optimized
                    WorkerConfig {
                        id: 2,
                        protocol: Protocol::TCP,
                        buffer_size: 32768, // 32KB for database query results
                        buffer_count: 32,
                        tcp_nodelay: true,  // Fast database query responses
                    },
                    WorkerConfig {
                        id: 3,
                        protocol: Protocol::TCP,
                        buffer_size: 32768, // 32KB for database query results
                        buffer_count: 32,
                        tcp_nodelay: true,  // Fast database query responses
                    },
                    // Redis/Cache workers - optimized for key-value operations
                    WorkerConfig {
                        id: 4,
                        protocol: Protocol::TCP,
                        buffer_size: 16384, // 16KB for Redis commands/responses
                        buffer_count: 48,
                        tcp_nodelay: true,  // Ultra-low latency for cache hits
                    },
                    WorkerConfig {
                        id: 5,
                        protocol: Protocol::TCP,
                        buffer_size: 16384, // 16KB for Redis commands/responses
                        buffer_count: 48,
                        tcp_nodelay: true,  // Ultra-low latency for cache hits
                    },
                    // MongoDB workers - optimized for document operations
                    WorkerConfig {
                        id: 6,
                        protocol: Protocol::TCP,
                        buffer_size: 65536, // 64KB for large documents
                        buffer_count: 24,
                        tcp_nodelay: true,  // Fast document queries
                    },
                    WorkerConfig {
                        id: 7,
                        protocol: Protocol::TCP,
                        buffer_size: 65536, // 64KB for large documents
                        buffer_count: 24,
                        tcp_nodelay: true,  // Fast document queries
                    },
                ]),
                worker_threads: 8,
                api_port: 8081,
                api_enabled: true,
                io_uring_queue_depth: 512,
                connection_pool_size: 128,
                buffer_pool_size: 128 * 1024 * 1024, // 128MB
                network_interface: Some("eth0".to_string()),
            },
            http_routes: vec![
                // API microservices routes
                HttpRouteConfig {
                    parent_refs: vec![
                        ParentRef {
                            name: "microservices-gateway".to_string(),
                            namespace: None,
                            section_name: Some("http-8080".to_string()),
                            port: Some(8080),
                        }
                    ],
                    hostnames: vec!["api.microservices.local".to_string()],
                    rules: vec![
                        HttpRouteRule {
                            matches: vec![
                                HttpRouteMatch {
                                    path: Some(HttpPathMatch {
                                        match_type: HttpPathMatchType::PathPrefix,
                                        value: "/auth".to_string(),
                                    }),
                                    headers: None,
                                    query_params: None,
                                    method: None,
                                }
                            ],
                            backend_refs: vec![
                                BackendRef {
                                    name: "auth-service".to_string(),
                                    namespace: None,
                                    port: 8001,
                                    weight: 100,
                                    kind: None,
                                    group: None,
                                }
                            ],
                        },
                        HttpRouteRule {
                            matches: vec![
                                HttpRouteMatch {
                                    path: Some(HttpPathMatch {
                                        match_type: HttpPathMatchType::PathPrefix,
                                        value: "/users".to_string(),
                                    }),
                                    headers: None,
                                    query_params: None,
                                    method: None,
                                }
                            ],
                            backend_refs: vec![
                                BackendRef {
                                    name: "user-service".to_string(),
                                    namespace: None,
                                    port: 8002,
                                    weight: 100,
                                    kind: None,
                                    group: None,
                                }
                            ],
                        },
                        HttpRouteRule {
                            matches: vec![
                                HttpRouteMatch {
                                    path: Some(HttpPathMatch {
                                        match_type: HttpPathMatchType::PathPrefix,
                                        value: "/orders".to_string(),
                                    }),
                                    headers: None,
                                    query_params: None,
                                    method: None,
                                }
                            ],
                            backend_refs: vec![
                                BackendRef {
                                    name: "order-service".to_string(),
                                    namespace: None,
                                    port: 8003,
                                    weight: 100,
                                    kind: None,
                                    group: None,
                                }
                            ],
                        },
                        HttpRouteRule {
                            matches: vec![
                                HttpRouteMatch {
                                    path: Some(HttpPathMatch {
                                        match_type: HttpPathMatchType::PathPrefix,
                                        value: "/payments".to_string(),
                                    }),
                                    headers: None,
                                    query_params: None,
                                    method: None,
                                }
                            ],
                            backend_refs: vec![
                                BackendRef {
                                    name: "payment-service".to_string(),
                                    namespace: None,
                                    port: 8004,
                                    weight: 100,
                                    kind: None,
                                    group: None,
                                }
                            ],
                        },
                    ],
                },
                // Admin dashboard route
                HttpRouteConfig {
                    parent_refs: vec![
                        ParentRef {
                            name: "microservices-gateway".to_string(),
                            namespace: None,
                            section_name: Some("http-8080".to_string()),
                            port: Some(8080),
                        }
                    ],
                    hostnames: vec!["admin.microservices.local".to_string()],
                    rules: vec![
                        HttpRouteRule {
                            matches: vec![
                                HttpRouteMatch {
                                    path: Some(HttpPathMatch {
                                        match_type: HttpPathMatchType::PathPrefix,
                                        value: "/".to_string(),
                                    }),
                                    headers: None,
                                    query_params: None,
                                    method: None,
                                }
                            ],
                            backend_refs: vec![
                                BackendRef {
                                    name: "admin-dashboard".to_string(),
                                    namespace: None,
                                    port: 3000,
                                    weight: 100,
                                    kind: None,
                                    group: None,
                                }
                            ],
                        },
                    ],
                },
            ],
            backend_pool: BackendPoolConfig {
                default_pool_size: 50,
                tcp_keepalive: true, // Enable for microservices stability
                tcp_keepalive_idle: Duration::from_secs(3600), // 1 hour
                tcp_keepalive_interval: Duration::from_secs(60), // 1 minute
                tcp_keepalive_probes: 6,
            },
            tcp_routes: vec![
                // PostgreSQL cluster
                TcpRouteConfig {
                    parent_refs: vec![
                        ParentRef {
                            name: "microservices-gateway".to_string(),
                            namespace: None,
                            section_name: Some("tcp-5432".to_string()),
                            port: Some(5432),
                        }
                    ],
                    rules: vec![
                        TcpRouteRule {
                            backend_refs: vec![
                                BackendRef {
                                    name: "postgres-primary".to_string(),
                                    namespace: None,
                                    port: 5432,
                                    weight: 50,
                                    kind: None,
                                    group: None,
                                },
                                BackendRef {
                                    name: "postgres-replica-1".to_string(),
                                    namespace: None,
                                    port: 5432,
                                    weight: 25,
                                    kind: None,
                                    group: None,
                                },
                                BackendRef {
                                    name: "postgres-replica-2".to_string(),
                                    namespace: None,
                                    port: 5432,
                                    weight: 25,
                                    kind: None,
                                    group: None,
                                },
                            ],
                        }
                    ],
                },
                // Redis cluster
                TcpRouteConfig {
                    parent_refs: vec![
                        ParentRef {
                            name: "microservices-gateway".to_string(),
                            namespace: None,
                            section_name: Some("tcp-6379".to_string()),
                            port: Some(6379),
                        }
                    ],
                    rules: vec![
                        TcpRouteRule {
                            backend_refs: vec![
                                BackendRef {
                                    name: "redis-cluster-1".to_string(),
                                    namespace: None,
                                    port: 6379,
                                    weight: 33,
                                    kind: None,
                                    group: None,
                                },
                                BackendRef {
                                    name: "redis-cluster-2".to_string(),
                                    namespace: None,
                                    port: 6379,
                                    weight: 33,
                                    kind: None,
                                    group: None,
                                },
                                BackendRef {
                                    name: "redis-cluster-3".to_string(),
                                    namespace: None,
                                    port: 6379,
                                    weight: 34,
                                    kind: None,
                                    group: None,
                                },
                            ],
                        }
                    ],
                },
                // MySQL cluster
                TcpRouteConfig {
                    parent_refs: vec![
                        ParentRef {
                            name: "microservices-gateway".to_string(),
                            namespace: None,
                            section_name: Some("tcp-3306".to_string()),
                            port: Some(3306),
                        }
                    ],
                    rules: vec![
                        TcpRouteRule {
                            backend_refs: vec![
                                BackendRef {
                                    name: "mysql-primary".to_string(),
                                    namespace: None,
                                    port: 3306,
                                    weight: 70,
                                    kind: None,
                                    group: None,
                                },
                                BackendRef {
                                    name: "mysql-replica".to_string(),
                                    namespace: None,
                                    port: 3306,
                                    weight: 30,
                                    kind: None,
                                    group: None,
                                },
                            ],
                        }
                    ],
                },
                // MongoDB sharded cluster
                TcpRouteConfig {
                    parent_refs: vec![
                        ParentRef {
                            name: "microservices-gateway".to_string(),
                            namespace: None,
                            section_name: Some("tcp-27017".to_string()),
                            port: Some(27017),
                        }
                    ],
                    rules: vec![
                        TcpRouteRule {
                            backend_refs: vec![
                                BackendRef {
                                    name: "mongodb-shard-1".to_string(),
                                    namespace: None,
                                    port: 27017,
                                    weight: 33,
                                    kind: None,
                                    group: None,
                                },
                                BackendRef {
                                    name: "mongodb-shard-2".to_string(),
                                    namespace: None,
                                    port: 27017,
                                    weight: 33,
                                    kind: None,
                                    group: None,
                                },
                                BackendRef {
                                    name: "mongodb-shard-3".to_string(),
                                    namespace: None,
                                    port: 27017,
                                    weight: 34,
                                    kind: None,
                                    group: None,
                                },
                            ],
                        }
                    ],
                },
            ],
            observability: ObservabilityConfig {
                logging: LoggingConfig {
                    level: LogLevel::Info,
                    format: LogFormat::Json,
                    output: LogOutput::Stdout,
                },
                metrics: MetricsConfig {
                    enabled: true,
                    port: 9090,
                    path: "/metrics".to_string(),
                },
                health_check: HealthCheckConfig {
                    enabled: true,
                    interval: Duration::from_secs(15),
                    timeout: Duration::from_secs(5),
                    failure_threshold: 3,
                    success_threshold: 2,
                },
            },
            performance: PerformanceConfig {
                backend_response_timeout: Duration::from_secs(10),
                connection_pool_size: 128,
                max_requests_per_connection: 200,
                standard_buffer_size: 8192,   // 8KB
                large_buffer_size: 16384,     // 16KB
                header_buffer_size: 4096,     // 4KB
                connection_max_age: Duration::from_secs(300),  // 5 minutes
                circuit_breaker_timeout: Duration::from_secs(10),
                keep_alive_timeout: Duration::from_secs(5),
                circuit_breaker_failure_threshold: 5,
                max_retries: 2,
                tcp_nodelay: true,
                so_reuseport: true,
                cpu_affinity: Some(vec![0, 1, 2, 3, 4, 5, 6, 7]),
            },
        }
    }

    /// Convert configuration to pretty-printed JSON with proper formatting
    pub fn to_json_pretty(&self) -> Result<String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| anyhow!("Failed to serialize config to JSON: {}", e))
    }

    /// Convert configuration to YAML format
    pub fn to_yaml_pretty(&self) -> Result<String> {
        serde_yaml::to_string(self)
            .map_err(|e| anyhow!("Failed to serialize config to YAML: {}", e))
    }
}

/// Print information about example configurations for CLI help
pub fn print_example_config_info() {
    println!("examples/minimal.json       - Basic single-service setup with default worker configuration");
    println!("examples/development.yaml   - Multi-service development with explicit worker configs (SCP-optimized)");
    println!("examples/production.json    - Production configuration with CPU-scaled workers");
    println!("examples/microservices.yaml - Complex routing with database-optimized workers");
    println!();
    println!("🚀 Worker Configuration Features:");
    println!("  • Protocol specialization: HTTP workers (8KB buffers) vs TCP workers (64KB+ buffers)");
    println!("  • TCP optimization: tcp_nodelay for immediate transmission, aggressive keepalive");
    println!("  • SCP performance: TCP workers solve stalling issues with large buffer optimization");
}

