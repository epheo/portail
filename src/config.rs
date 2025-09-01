use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as DeError};
use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;
use anyhow::{anyhow, Result};

/// Root configuration structure for UringRess
/// All parsing happens once at startup - zero runtime overhead
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UringRessConfig {
    #[serde(default)]
    pub server: ServerConfig,
    
    #[serde(default)]
    pub http_routes: Vec<HttpRouteConfig>,
    
    #[serde(default)]
    pub tcp_routes: Vec<TcpRouteConfig>,
    
    #[serde(default)]
    pub observability: ObservabilityConfig,
    
    #[serde(default)]
    pub performance: PerformanceConfig,
}

/// Server configuration with ports and worker settings
/// Replaces environment variable-based configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub http_port: u16,
    pub tcp_ports: Vec<u16>,
    pub api_port: u16,
    pub api_enabled: bool,
    pub worker_threads: usize,
    pub io_uring_queue_depth: u32,
    pub connection_pool_size: usize,
    
    #[serde(deserialize_with = "deserialize_size", serialize_with = "serialize_size")]
    pub buffer_pool_size: usize,
}

/// HTTP route configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpRouteConfig {
    pub host: String,
    pub path: String,
    // Support both single and multiple backend configurations
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<BackendConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backends: Option<Vec<BackendConfig>>,
    #[serde(default, skip_serializing_if = "is_default_load_balance")]
    pub load_balance_strategy: LoadBalanceStrategy,
}

fn is_default_load_balance(strategy: &LoadBalanceStrategy) -> bool {
    matches!(strategy, LoadBalanceStrategy::RoundRobin)
}

/// TCP route configuration with load balancing
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcpRouteConfig {
    pub port: u16,
    pub backends: Vec<BackendConfig>,
    #[serde(default)]
    pub load_balance_strategy: LoadBalanceStrategy,
}

/// Backend configuration for both HTTP and TCP routes
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
    pub address: String,
    pub port: u16,
    pub health_check_path: Option<String>,
}

/// Load balancing strategy for TCP routes
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadBalanceStrategy {
    RoundRobin,
    LeastConnections,
}

/// Observability configuration (logging, metrics, health checks)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservabilityConfig {
    #[serde(default)]
    pub logging: LoggingConfig,
    
    #[serde(default)]
    pub metrics: MetricsConfig,
    
    #[serde(default)]
    pub health_check: HealthCheckConfig,
}

/// Logging configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    #[serde(default)]
    pub level: LogLevel,
    
    #[serde(default)]
    pub format: LogFormat,
    
    #[serde(default)]
    pub output: LogOutput,
}

/// Log level enumeration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

/// Log format enumeration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Json,
    Pretty,
    Compact,
}

/// Log output destination
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogOutput {
    Stdout,
    Stderr,
    File(String),
}

/// Metrics configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    pub enabled: bool,
    pub port: u16,
    pub path: String,
}

/// Health check configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthCheckConfig {
    pub enabled: bool,
    
    #[serde(deserialize_with = "deserialize_duration", serialize_with = "serialize_duration")]
    pub interval: Duration,
    
    #[serde(deserialize_with = "deserialize_duration", serialize_with = "serialize_duration")]
    pub timeout: Duration,
    
    pub failure_threshold: u32,
    pub success_threshold: u32,
}

/// Performance configuration - replaces RuntimeConfig entirely
/// All values pre-computed at startup for zero runtime overhead
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[repr(C)] // Predictable memory layout for cache efficiency
pub struct PerformanceConfig {
    // Hot data first - accessed every request
    #[serde(deserialize_with = "deserialize_duration", serialize_with = "serialize_duration")]
    pub backend_response_timeout: Duration,
    
    pub connection_pool_size: usize,
    pub max_requests_per_connection: u32,
    
    // Buffer configuration
    #[serde(deserialize_with = "deserialize_size", serialize_with = "serialize_size")]
    pub standard_buffer_size: usize,
    
    #[serde(deserialize_with = "deserialize_size", serialize_with = "serialize_size")]
    pub large_buffer_size: usize,
    
    #[serde(deserialize_with = "deserialize_size", serialize_with = "serialize_size")]
    pub header_buffer_size: usize,
    
    // Pre-computed Duration objects (zero runtime parsing)
    #[serde(deserialize_with = "deserialize_duration", serialize_with = "serialize_duration")]
    pub connection_max_age: Duration,
    
    #[serde(deserialize_with = "deserialize_duration", serialize_with = "serialize_duration")]
    pub circuit_breaker_timeout: Duration,
    
    #[serde(deserialize_with = "deserialize_duration", serialize_with = "serialize_duration")]
    pub keep_alive_timeout: Duration,
    
    // Circuit breaker and retry configuration
    pub circuit_breaker_failure_threshold: u32,
    pub max_retries: u32,
    
    // Network optimization
    pub tcp_nodelay: bool,
    pub so_reuseport: bool,
    
    // CPU affinity (optional)
    pub cpu_affinity: Option<Vec<usize>>,
}

// Default implementations with production-ready values
// Based on current RuntimeConfig::from_env() defaults

impl Default for UringRessConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            http_routes: Vec::new(),
            tcp_routes: Vec::new(),
            observability: ObservabilityConfig::default(),
            performance: PerformanceConfig::default(),
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            http_port: 8080,
            tcp_ports: vec![9090],
            api_port: 8081,
            api_enabled: true,
            worker_threads: num_cpus::get().min(8), // Cap at 8 for performance
            io_uring_queue_depth: 256,
            connection_pool_size: 64,
            buffer_pool_size: 64 * 1024 * 1024, // 64MB
        }
    }
}

impl Default for LoadBalanceStrategy {
    fn default() -> Self {
        LoadBalanceStrategy::RoundRobin
    }
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            logging: LoggingConfig::default(),
            metrics: MetricsConfig::default(),
            health_check: HealthCheckConfig::default(),
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            format: LogFormat::Pretty,
            output: LogOutput::Stdout,
        }
    }
}

impl Default for LogLevel {
    fn default() -> Self {
        LogLevel::Info
    }
}

impl Default for LogFormat {
    fn default() -> Self {
        LogFormat::Pretty
    }
}

impl Default for LogOutput {
    fn default() -> Self {
        LogOutput::Stdout
    }
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 9091,
            path: "/metrics".to_string(),
        }
    }
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval: Duration::from_secs(30),
            timeout: Duration::from_secs(5),
            failure_threshold: 3,
            success_threshold: 2,
        }
    }
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            // Hot path values (matching RuntimeConfig::from_env defaults)
            backend_response_timeout: Duration::from_secs(10),
            connection_pool_size: 64,
            max_requests_per_connection: 100,
            
            // Buffer configuration (matching RuntimeConfig defaults)
            standard_buffer_size: 8192,   // 8KB
            large_buffer_size: 16384,     // 16KB
            header_buffer_size: 4096,     // 4KB
            
            // Pre-computed Duration objects
            connection_max_age: Duration::from_secs(120),
            circuit_breaker_timeout: Duration::from_secs(10),
            keep_alive_timeout: Duration::from_secs(5),
            
            // Circuit breaker and retry
            circuit_breaker_failure_threshold: 5,
            max_retries: 2,
            
            // Network optimization
            tcp_nodelay: true,
            so_reuseport: true,
            
            // CPU affinity disabled by default
            cpu_affinity: None,
        }
    }
}

// Custom deserializers for human-readable formats
// All parsing happens once at startup

/// Deserialize human-readable sizes like "64MB", "1GB", "512KB"
fn deserialize_size<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;
    parse_size(&s).map_err(D::Error::custom)
}

/// Deserialize human-readable durations like "10s", "500ms", "1m"
fn deserialize_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;
    parse_duration(&s).map_err(D::Error::custom)
}

/// Serialize Duration to human-readable format
fn serialize_duration<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let s = format_duration(duration);
    serializer.serialize_str(&s)
}

/// Serialize size to human-readable format
fn serialize_size<S>(size: &usize, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let s = format_size(*size);
    serializer.serialize_str(&s)
}

/// Parse human-readable size strings to bytes
fn parse_size(s: &str) -> Result<usize> {
    let s = s.trim().to_lowercase();
    
    if let Ok(bytes) = s.parse::<usize>() {
        return Ok(bytes);
    }
    
    let (number_part, suffix) = if s.ends_with("kb") {
        (s.trim_end_matches("kb"), 1024)
    } else if s.ends_with("mb") {
        (s.trim_end_matches("mb"), 1024 * 1024)
    } else if s.ends_with("gb") {
        (s.trim_end_matches("gb"), 1024 * 1024 * 1024)
    } else if s.ends_with("k") {
        (s.trim_end_matches("k"), 1024)
    } else if s.ends_with("m") {
        (s.trim_end_matches("m"), 1024 * 1024)
    } else if s.ends_with("g") {
        (s.trim_end_matches("g"), 1024 * 1024 * 1024)
    } else {
        return Err(anyhow!("Invalid size format: {}. Expected format like '64MB', '1GB', '512KB'", s));
    };
    
    let number: f64 = number_part.parse()
        .map_err(|_| anyhow!("Invalid number in size: {}", s))?;
    
    if number < 0.0 {
        return Err(anyhow!("Size cannot be negative: {}", s));
    }
    
    Ok((number * suffix as f64) as usize)
}

/// Parse human-readable duration strings to Duration
fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim().to_lowercase();
    
    if let Ok(secs) = s.parse::<u64>() {
        return Ok(Duration::from_secs(secs));
    }
    
    let (number_part, suffix) = if s.ends_with("ns") {
        (s.trim_end_matches("ns"), Duration::from_nanos(1))
    } else if s.ends_with("us") || s.ends_with("μs") {
        (s.trim_end_matches("us").trim_end_matches("μs"), Duration::from_micros(1))
    } else if s.ends_with("ms") {
        (s.trim_end_matches("ms"), Duration::from_millis(1))
    } else if s.ends_with("s") {
        (s.trim_end_matches("s"), Duration::from_secs(1))
    } else if s.ends_with("m") {
        (s.trim_end_matches("m"), Duration::from_secs(60))
    } else if s.ends_with("h") {
        (s.trim_end_matches("h"), Duration::from_secs(3600))
    } else {
        return Err(anyhow!("Invalid duration format: {}. Expected format like '10s', '500ms', '1m'", s));
    };
    
    let number: f64 = number_part.parse()
        .map_err(|_| anyhow!("Invalid number in duration: {}", s))?;
    
    if number < 0.0 {
        return Err(anyhow!("Duration cannot be negative: {}", s));
    }
    
    Ok(Duration::from_nanos((number * suffix.as_nanos() as f64) as u64))
}

/// Format Duration to human-readable string
fn format_duration(duration: &Duration) -> String {
    let total_secs = duration.as_secs();
    let nanos = duration.subsec_nanos();
    
    if total_secs >= 3600 {
        format!("{}h", total_secs / 3600)
    } else if total_secs >= 60 {
        format!("{}m", total_secs / 60)
    } else if total_secs > 0 {
        format!("{}s", total_secs)
    } else if nanos >= 1_000_000 {
        format!("{}ms", nanos / 1_000_000)
    } else if nanos >= 1_000 {
        format!("{}us", nanos / 1_000)
    } else {
        format!("{}ns", nanos)
    }
}

/// Format size to human-readable string
fn format_size(size: usize) -> String {
    const GB: usize = 1024 * 1024 * 1024;
    const MB: usize = 1024 * 1024;
    const KB: usize = 1024;
    
    if size >= GB && size % GB == 0 {
        format!("{}GB", size / GB)
    } else if size >= MB && size % MB == 0 {
        format!("{}MB", size / MB)
    } else if size >= KB && size % KB == 0 {
        format!("{}KB", size / KB)
    } else {
        size.to_string()
    }
}

// Configuration loading and persistence
impl UringRessConfig {
    /// Load configuration from file with auto-format detection
    /// Supports JSON (.json) and YAML (.yaml, .yml) formats
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        
        // Check if file exists
        if !path.exists() {
            return Err(anyhow!("Configuration file not found: {}", path.display()));
        }
        
        // Read file contents
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow!("Failed to read configuration file '{}': {}", path.display(), e))?;
        
        // Auto-detect format from file extension
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
        
        // Validate the loaded configuration
        config.validate()
            .map_err(|e| anyhow!("Configuration validation failed for '{}': {}", path.display(), e))?;
        
        Ok(config)
    }
    
    /// Save configuration to file with format auto-detection
    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let path = path.as_ref();
        
        // Create parent directory if it doesn't exist
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow!("Failed to create directory '{}': {}", parent.display(), e))?;
        }
        
        // Auto-detect format from file extension
        let content = match path.extension().and_then(|ext| ext.to_str()) {
            Some("json") => self.to_json_pretty()?,
            Some("yaml") | Some("yml") => self.to_yaml_pretty()?,
            Some(ext) => {
                return Err(anyhow!(
                    "Unsupported configuration file format '.{}' for '{}'. Supported formats: .json, .yaml, .yml",
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
        
        // Write to file
        std::fs::write(path, content)
            .map_err(|e| anyhow!("Failed to write configuration file '{}': {}", path.display(), e))?;
        
        Ok(())
    }
    
    /// Load configuration with fallback to default values
    /// If file doesn't exist, returns default configuration
    /// If file exists but has errors, returns the error
    pub fn load_or_default<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        
        if !path.exists() {
            Ok(Self::default())
        } else {
            Self::load_from_file(path)
        }
    }
}

// Configuration example generation
impl UringRessConfig {
    /// Generate a minimal configuration example
    /// Single HTTP route with basic settings - ideal for getting started
    pub fn generate_minimal() -> Self {
        Self {
            server: ServerConfig {
                http_port: 8080,
                tcp_ports: vec![],
                api_port: 8081,
                api_enabled: true,
                worker_threads: 2,
                io_uring_queue_depth: 128,
                connection_pool_size: 32,
                buffer_pool_size: 32 * 1024 * 1024, // 32MB
            },
            http_routes: vec![
                HttpRouteConfig {
                    host: "localhost:8080".to_string(),
                    path: "/api.json".to_string(),
                    backend: Some(BackendConfig {
                        address: "127.0.0.1".to_string(),
                        port: 3001,
                        health_check_path: Some("/health".to_string()),
                    }),
                    backends: None,
                    load_balance_strategy: LoadBalanceStrategy::RoundRobin,
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
            server: ServerConfig {
                http_port: 8080,
                tcp_ports: vec![],
                api_port: 8081,
                api_enabled: true,
                worker_threads: 4,
                io_uring_queue_depth: 256,
                connection_pool_size: 64,
                buffer_pool_size: 64 * 1024 * 1024, // 64MB
            },
            http_routes: vec![
                HttpRouteConfig {
                    host: "localhost:8080".to_string(),
                    path: "/".to_string(),
                    backend: Some(BackendConfig {
                        address: "127.0.0.1".to_string(),
                        port: 3001,
                        health_check_path: Some("/health".to_string()),
                    }),
                    backends: None,
                    load_balance_strategy: LoadBalanceStrategy::RoundRobin,
                },
                // /index.html route with round-robin across all 3 backends
                HttpRouteConfig {
                    host: "localhost:8080".to_string(),
                    path: "/index.html".to_string(),
                    backend: None,
                    backends: Some(vec![
                        BackendConfig {
                            address: "127.0.0.1".to_string(),
                            port: 3001,
                            health_check_path: Some("/health".to_string()),
                        },
                        BackendConfig {
                            address: "127.0.0.1".to_string(),
                            port: 3002,
                            health_check_path: Some("/health".to_string()),
                        },
                        BackendConfig {
                            address: "127.0.0.1".to_string(),
                            port: 3003,
                            health_check_path: Some("/health".to_string()),
                        },
                    ]),
                    load_balance_strategy: LoadBalanceStrategy::RoundRobin,
                },
                HttpRouteConfig {
                    host: "localhost:8080".to_string(),
                    path: "/api.json".to_string(),
                    backend: Some(BackendConfig {
                        address: "127.0.0.1".to_string(),
                        port: 3001,
                        health_check_path: Some("/health".to_string()),
                    }),
                    backends: None,
                    load_balance_strategy: LoadBalanceStrategy::RoundRobin,
                },
                HttpRouteConfig {
                    host: "localhost:8080".to_string(),
                    path: "/users.json".to_string(),
                    backend: Some(BackendConfig {
                        address: "127.0.0.1".to_string(),
                        port: 3002,
                        health_check_path: Some("/health".to_string()),
                    }),
                    backends: None,
                    load_balance_strategy: LoadBalanceStrategy::RoundRobin,
                },
                HttpRouteConfig {
                    host: "localhost:8080".to_string(),
                    path: "/dataset.json".to_string(),
                    backend: Some(BackendConfig {
                        address: "127.0.0.1".to_string(),
                        port: 3003,
                        health_check_path: Some("/health".to_string()),
                    }),
                    backends: None,
                    load_balance_strategy: LoadBalanceStrategy::RoundRobin,
                },
                HttpRouteConfig {
                    host: "localhost:8080".to_string(),
                    path: "/health".to_string(),
                    backend: Some(BackendConfig {
                        address: "127.0.0.1".to_string(),
                        port: 3001,
                        health_check_path: Some("/health".to_string()),
                    }),
                    backends: None,
                    load_balance_strategy: LoadBalanceStrategy::RoundRobin,
                },
                HttpRouteConfig {
                    host: "localhost".to_string(),
                    path: "/api.json".to_string(),
                    backend: Some(BackendConfig {
                        address: "127.0.0.1".to_string(),
                        port: 3001,
                        health_check_path: Some("/health".to_string()),
                    }),
                    backends: None,
                    load_balance_strategy: LoadBalanceStrategy::RoundRobin,
                },
                HttpRouteConfig {
                    host: "localhost".to_string(),
                    path: "/users.json".to_string(),
                    backend: Some(BackendConfig {
                        address: "127.0.0.1".to_string(),
                        port: 3002,
                        health_check_path: Some("/health".to_string()),
                    }),
                    backends: None,
                    load_balance_strategy: LoadBalanceStrategy::RoundRobin,
                },
            ],
            tcp_routes: vec![
                TcpRouteConfig {
                    port: 2222,
                    backends: vec![
                        BackendConfig {
                            address: "127.0.0.1".to_string(),
                            port: 22,
                            health_check_path: None,
                        }
                    ],
                    load_balance_strategy: LoadBalanceStrategy::RoundRobin,
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
    /// Optimized for high performance and reliability
    pub fn generate_production() -> Self {
        Self {
            server: ServerConfig {
                http_port: 80,
                tcp_ports: vec![],
                api_port: 8080,
                api_enabled: true,
                worker_threads: num_cpus::get(),
                io_uring_queue_depth: 512,
                connection_pool_size: 256,
                buffer_pool_size: 256 * 1024 * 1024, // 256MB
            },
            http_routes: vec![
                // Integration test routes for compatibility
                HttpRouteConfig {
                    host: "localhost:8080".to_string(),
                    path: "/api.json".to_string(),
                    backend: Some(BackendConfig {
                        address: "127.0.0.1".to_string(),
                        port: 3001,
                        health_check_path: Some("/health".to_string()),
                    }),
                    backends: None,
                    load_balance_strategy: LoadBalanceStrategy::RoundRobin,
                },
                HttpRouteConfig {
                    host: "localhost:8080".to_string(),
                    path: "/users.json".to_string(),
                    backend: Some(BackendConfig {
                        address: "127.0.0.1".to_string(),
                        port: 3002,
                        health_check_path: Some("/health".to_string()),
                    }),
                    backends: None,
                    load_balance_strategy: LoadBalanceStrategy::RoundRobin,
                },
                HttpRouteConfig {
                    host: "localhost:8080".to_string(),
                    path: "/dataset.json".to_string(),
                    backend: Some(BackendConfig {
                        address: "127.0.0.1".to_string(),
                        port: 3003,
                        health_check_path: Some("/health".to_string()),
                    }),
                    backends: None,
                    load_balance_strategy: LoadBalanceStrategy::RoundRobin,
                },
                // Production example routes
                HttpRouteConfig {
                    host: "api.example.com".to_string(),
                    path: "/api".to_string(),
                    backend: Some(BackendConfig {
                        address: "10.0.1.10".to_string(),
                        port: 8000,
                        health_check_path: Some("/health".to_string()),
                    }),
                    backends: None,
                    load_balance_strategy: LoadBalanceStrategy::RoundRobin,
                },
                HttpRouteConfig {
                    host: "www.example.com".to_string(),
                    path: "/".to_string(),
                    backend: Some(BackendConfig {
                        address: "10.0.1.20".to_string(),
                        port: 80,
                        health_check_path: Some("/status".to_string()),
                    }),
                    backends: None,
                    load_balance_strategy: LoadBalanceStrategy::RoundRobin,
                },
            ],
            tcp_routes: vec![
                TcpRouteConfig {
                    port: 5432,
                    backends: vec![
                        BackendConfig {
                            address: "10.0.2.10".to_string(),
                            port: 5432,
                            health_check_path: None,
                        },
                        BackendConfig {
                            address: "10.0.2.11".to_string(),
                            port: 5432,
                            health_check_path: None,
                        },
                    ],
                    load_balance_strategy: LoadBalanceStrategy::LeastConnections,
                },
                TcpRouteConfig {
                    port: 6379,
                    backends: vec![
                        BackendConfig {
                            address: "10.0.3.10".to_string(),
                            port: 6379,
                            health_check_path: None,
                        },
                    ],
                    load_balance_strategy: LoadBalanceStrategy::RoundRobin,
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
            server: ServerConfig {
                http_port: 8080,
                tcp_ports: vec![],
                api_port: 8081,
                api_enabled: true,
                worker_threads: 8,
                io_uring_queue_depth: 512,
                connection_pool_size: 128,
                buffer_pool_size: 128 * 1024 * 1024, // 128MB
            },
            http_routes: vec![
                HttpRouteConfig {
                    host: "api.microservices.local".to_string(),
                    path: "/auth".to_string(),
                    backend: Some(BackendConfig {
                        address: "auth-service".to_string(),
                        port: 8001,
                        health_check_path: Some("/health".to_string()),
                    }),
                    backends: None,
                    load_balance_strategy: LoadBalanceStrategy::RoundRobin,
                },
                HttpRouteConfig {
                    host: "api.microservices.local".to_string(),
                    path: "/users".to_string(),
                    backend: Some(BackendConfig {
                        address: "user-service".to_string(),
                        port: 8002,
                        health_check_path: Some("/health".to_string()),
                    }),
                    backends: None,
                    load_balance_strategy: LoadBalanceStrategy::RoundRobin,
                },
                HttpRouteConfig {
                    host: "api.microservices.local".to_string(),
                    path: "/orders".to_string(),
                    backend: Some(BackendConfig {
                        address: "order-service".to_string(),
                        port: 8003,
                        health_check_path: Some("/health".to_string()),
                    }),
                    backends: None,
                    load_balance_strategy: LoadBalanceStrategy::RoundRobin,
                },
                HttpRouteConfig {
                    host: "api.microservices.local".to_string(),
                    path: "/payments".to_string(),
                    backend: Some(BackendConfig {
                        address: "payment-service".to_string(),
                        port: 8004,
                        health_check_path: Some("/health".to_string()),
                    }),
                    backends: None,
                    load_balance_strategy: LoadBalanceStrategy::RoundRobin,
                },
                HttpRouteConfig {
                    host: "admin.microservices.local".to_string(),
                    path: "/".to_string(),
                    backend: Some(BackendConfig {
                        address: "admin-dashboard".to_string(),
                        port: 3000,
                        health_check_path: Some("/status".to_string()),
                    }),
                    backends: None,
                    load_balance_strategy: LoadBalanceStrategy::RoundRobin,
                },
            ],
            tcp_routes: vec![
                TcpRouteConfig {
                    port: 5432,
                    backends: vec![
                        BackendConfig {
                            address: "postgres-primary".to_string(),
                            port: 5432,
                            health_check_path: None,
                        },
                        BackendConfig {
                            address: "postgres-replica-1".to_string(),
                            port: 5432,
                            health_check_path: None,
                        },
                        BackendConfig {
                            address: "postgres-replica-2".to_string(),
                            port: 5432,
                            health_check_path: None,
                        },
                    ],
                    load_balance_strategy: LoadBalanceStrategy::LeastConnections,
                },
                TcpRouteConfig {
                    port: 6379,
                    backends: vec![
                        BackendConfig {
                            address: "redis-cluster-1".to_string(),
                            port: 6379,
                            health_check_path: None,
                        },
                        BackendConfig {
                            address: "redis-cluster-2".to_string(),
                            port: 6379,
                            health_check_path: None,
                        },
                        BackendConfig {
                            address: "redis-cluster-3".to_string(),
                            port: 6379,
                            health_check_path: None,
                        },
                    ],
                    load_balance_strategy: LoadBalanceStrategy::RoundRobin,
                },
                TcpRouteConfig {
                    port: 3306,
                    backends: vec![
                        BackendConfig {
                            address: "mysql-primary".to_string(),
                            port: 3306,
                            health_check_path: None,
                        },
                        BackendConfig {
                            address: "mysql-replica".to_string(),
                            port: 3306,
                            health_check_path: None,
                        },
                    ],
                    load_balance_strategy: LoadBalanceStrategy::LeastConnections,
                },
                TcpRouteConfig {
                    port: 27017,
                    backends: vec![
                        BackendConfig {
                            address: "mongodb-shard-1".to_string(),
                            port: 27017,
                            health_check_path: None,
                        },
                        BackendConfig {
                            address: "mongodb-shard-2".to_string(),
                            port: 27017,
                            health_check_path: None,
                        },
                        BackendConfig {
                            address: "mongodb-shard-3".to_string(),
                            port: 27017,
                            health_check_path: None,
                        },
                    ],
                    load_balance_strategy: LoadBalanceStrategy::RoundRobin,
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

// Configuration validation
impl UringRessConfig {
    /// Validate the entire configuration for consistency and correctness
    /// All validation happens once at startup
    pub fn validate(&self) -> Result<()> {
        self.server.validate()?;
        
        for (i, route) in self.http_routes.iter().enumerate() {
            route.validate().map_err(|e| anyhow!("HTTP route {}: {}", i, e))?;
        }
        
        for (i, route) in self.tcp_routes.iter().enumerate() {
            route.validate().map_err(|e| anyhow!("TCP route {}: {}", i, e))?;
        }
        
        self.observability.validate()?;
        self.performance.validate()?;
        
        // Check for port conflicts
        self.validate_port_conflicts()?;
        
        Ok(())
    }
    
    /// Check for port conflicts between different services
    fn validate_port_conflicts(&self) -> Result<()> {
        let mut used_ports = std::collections::HashSet::new();
        
        // Check HTTP port
        if !used_ports.insert(self.server.http_port) {
            return Err(anyhow!("Port conflict: HTTP port {} already in use", self.server.http_port));
        }
        
        // Check API port
        if self.server.api_enabled {
            if !used_ports.insert(self.server.api_port) {
                return Err(anyhow!("Port conflict: API port {} already in use", self.server.api_port));
            }
        }
        
        // Check metrics port
        if self.observability.metrics.enabled {
            if !used_ports.insert(self.observability.metrics.port) {
                return Err(anyhow!("Port conflict: Metrics port {} already in use", self.observability.metrics.port));
            }
        }
        
        // Check TCP ports
        for port in &self.server.tcp_ports {
            if !used_ports.insert(*port) {
                return Err(anyhow!("Port conflict: TCP port {} already in use", port));
            }
        }
        
        // Check TCP route ports
        for route in &self.tcp_routes {
            if !used_ports.insert(route.port) {
                return Err(anyhow!("Port conflict: TCP route port {} already in use", route.port));
            }
        }
        
        Ok(())
    }
}

impl ServerConfig {
    fn validate(&self) -> Result<()> {
        validate_port(self.http_port, "HTTP port")?;
        validate_port(self.api_port, "API port")?;
        
        for (i, port) in self.tcp_ports.iter().enumerate() {
            validate_port(*port, &format!("TCP port {}", i))?;
        }
        
        if self.worker_threads == 0 {
            return Err(anyhow!("Worker threads must be greater than 0"));
        }
        
        if self.worker_threads > 256 {
            return Err(anyhow!("Worker threads cannot exceed 256"));
        }
        
        if self.io_uring_queue_depth == 0 {
            return Err(anyhow!("io_uring queue depth must be greater than 0"));
        }
        
        if self.connection_pool_size == 0 {
            return Err(anyhow!("Connection pool size must be greater than 0"));
        }
        
        Ok(())
    }
}

impl HttpRouteConfig {
    fn validate(&self) -> Result<()> {
        if self.host.is_empty() {
            return Err(anyhow!("Host cannot be empty"));
        }
        
        if !self.path.starts_with('/') {
            return Err(anyhow!("Path must start with '/': {}", self.path));
        }
        
        // Validate backend configuration - must have either single backend or multiple backends
        match (&self.backend, &self.backends) {
            (Some(backend), None) => {
                // Single backend configuration
                backend.validate()?;
            }
            (None, Some(backends)) => {
                // Multiple backend configuration
                if backends.is_empty() {
                    return Err(anyhow!("HTTP route with 'backends' field must have at least one backend"));
                }
                
                for (i, backend) in backends.iter().enumerate() {
                    backend.validate().map_err(|e| anyhow!("Backend {}: {}", i, e))?;
                }
            }
            (Some(_), Some(_)) => {
                return Err(anyhow!("HTTP route cannot have both 'backend' and 'backends' fields"));
            }
            (None, None) => {
                return Err(anyhow!("HTTP route must have either 'backend' or 'backends' field"));
            }
        }
        
        Ok(())
    }
}

impl TcpRouteConfig {
    fn validate(&self) -> Result<()> {
        validate_port(self.port, "TCP route port")?;
        
        if self.backends.is_empty() {
            return Err(anyhow!("TCP route must have at least one backend"));
        }
        
        for (i, backend) in self.backends.iter().enumerate() {
            backend.validate().map_err(|e| anyhow!("Backend {}: {}", i, e))?;
        }
        
        Ok(())
    }
}

impl BackendConfig {
    fn validate(&self) -> Result<()> {
        if self.address.is_empty() {
            return Err(anyhow!("Backend address cannot be empty"));
        }
        
        // Validate address format (IP or hostname)
        if self.address.parse::<IpAddr>().is_err() && !is_valid_hostname(&self.address) {
            return Err(anyhow!("Invalid backend address format: {}", self.address));
        }
        
        validate_port(self.port, "Backend port")?;
        
        if let Some(path) = &self.health_check_path {
            if !path.starts_with('/') {
                return Err(anyhow!("Health check path must start with '/': {}", path));
            }
        }
        
        Ok(())
    }
}

impl ObservabilityConfig {
    fn validate(&self) -> Result<()> {
        self.metrics.validate()?;
        self.health_check.validate()?;
        Ok(())
    }
}

impl MetricsConfig {
    fn validate(&self) -> Result<()> {
        if self.enabled {
            validate_port(self.port, "Metrics port")?;
            
            if !self.path.starts_with('/') {
                return Err(anyhow!("Metrics path must start with '/': {}", self.path));
            }
        }
        
        Ok(())
    }
}

impl HealthCheckConfig {
    fn validate(&self) -> Result<()> {
        if self.enabled {
            if self.interval.as_secs() == 0 {
                return Err(anyhow!("Health check interval must be greater than 0"));
            }
            
            if self.timeout.as_secs() == 0 {
                return Err(anyhow!("Health check timeout must be greater than 0"));
            }
            
            if self.failure_threshold == 0 {
                return Err(anyhow!("Health check failure threshold must be greater than 0"));
            }
            
            if self.success_threshold == 0 {
                return Err(anyhow!("Health check success threshold must be greater than 0"));
            }
        }
        
        Ok(())
    }
}

impl PerformanceConfig {
    fn validate(&self) -> Result<()> {
        if self.connection_pool_size == 0 {
            return Err(anyhow!("Connection pool size must be greater than 0"));
        }
        
        if self.max_requests_per_connection == 0 {
            return Err(anyhow!("Max requests per connection must be greater than 0"));
        }
        
        if self.standard_buffer_size == 0 {
            return Err(anyhow!("Standard buffer size must be greater than 0"));
        }
        
        if self.large_buffer_size == 0 {
            return Err(anyhow!("Large buffer size must be greater than 0"));
        }
        
        if self.header_buffer_size == 0 {
            return Err(anyhow!("Header buffer size must be greater than 0"));
        }
        
        if self.circuit_breaker_failure_threshold == 0 {
            return Err(anyhow!("Circuit breaker failure threshold must be greater than 0"));
        }
        
        Ok(())
    }
}

// Configuration to runtime structure conversion
impl UringRessConfig {
    /// Convert configuration to RouteTable for runtime use
    /// All route processing happens once at startup - zero runtime overhead
    pub fn to_route_table(&self) -> Result<crate::routing::RouteTable> {
        use crate::routing::{RouteTable, Backend};
        
        let mut route_table = RouteTable::new();
        
        // Convert HTTP routes - handle both single and multiple backends
        for http_route in &self.http_routes {
            match (&http_route.backend, &http_route.backends) {
                (Some(backend_config), None) => {
                    // Single backend - use existing add_http_route method
                    let route_backend = Backend::new(backend_config.address.clone(), backend_config.port)?;
                    let route_backend = if let Some(ref health_path) = backend_config.health_check_path {
                        route_backend.with_health_check(health_path.clone())
                    } else {
                        route_backend
                    };
                    
                    route_table.add_http_route(&http_route.host, &http_route.path, route_backend);
                }
                (None, Some(backend_configs)) => {
                    // Multiple backends - create all backends and add as pool
                    let mut route_backends = Vec::new();
                    
                    for backend_config in backend_configs {
                        let backend = Backend::new(backend_config.address.clone(), backend_config.port)?;
                        let backend = if let Some(ref health_path) = backend_config.health_check_path {
                            backend.with_health_check(health_path.clone())
                        } else {
                            backend
                        };
                        route_backends.push(backend);
                    }
                    
                    // Add HTTP route with multiple backends
                    let routing_strategy = match &http_route.load_balance_strategy {
                        LoadBalanceStrategy::RoundRobin => crate::routing::LoadBalanceStrategy::RoundRobin,
                        LoadBalanceStrategy::LeastConnections => crate::routing::LoadBalanceStrategy::LeastConnections,
                    };
                    route_table.add_http_route_with_pool(
                        &http_route.host, 
                        &http_route.path, 
                        route_backends, 
                        routing_strategy
                    );
                }
                _ => {
                    // This should be caught by validation, but let's be safe
                    return Err(anyhow!("Invalid HTTP route configuration for {}{}", http_route.host, http_route.path));
                }
            }
        }
        
        // Convert TCP routes using existing add_tcp_route method
        for tcp_route in &self.tcp_routes {
            let mut backends = Vec::new();
            
            for backend_config in &tcp_route.backends {
                let backend = Backend::new(backend_config.address.clone(), backend_config.port)?;
                backends.push(backend);
            }
            
            // Use existing add_tcp_route method (it creates BackendPool internally)
            route_table.add_tcp_route(tcp_route.port, backends);
        }
        
        Ok(route_table)
    }
}

impl PerformanceConfig {
    /// Convert PerformanceConfig to RuntimeConfig for backward compatibility
    /// All values are pre-computed at startup for zero runtime overhead
    pub fn to_runtime_config(&self) -> crate::runtime_config::RuntimeConfig {
        crate::runtime_config::RuntimeConfig::new(
            self.connection_pool_size,
            self.max_requests_per_connection,
            self.standard_buffer_size,
            self.large_buffer_size,
            self.header_buffer_size,
            self.connection_max_age,
            self.backend_response_timeout,
            self.circuit_breaker_timeout,
            self.keep_alive_timeout,
            self.circuit_breaker_failure_threshold,
            self.max_retries,
        )
    }
}

// Utility functions

/// Validate port number is in valid range (1-65535)
fn validate_port(port: u16, name: &str) -> Result<()> {
    if port == 0 {
        return Err(anyhow!("{} must be between 1 and 65535", name));
    }
    Ok(())
}

/// Basic hostname validation (simplified)
fn is_valid_hostname(hostname: &str) -> bool {
    if hostname.is_empty() || hostname.len() > 253 {
        return false;
    }
    
    hostname.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        && !hostname.starts_with('-')
        && !hostname.ends_with('-')
        && !hostname.starts_with('.')
        && !hostname.ends_with('.')
}

#[cfg(test)]
mod tests {
    use super::*;
    
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
    fn test_hostname_validation() {
        assert!(is_valid_hostname("example.com"));
        assert!(is_valid_hostname("api.example.com"));
        assert!(is_valid_hostname("localhost"));
        assert!(is_valid_hostname("test-server"));
        
        assert!(!is_valid_hostname(""));
        assert!(!is_valid_hostname("-invalid"));
        assert!(!is_valid_hostname("invalid-"));
        assert!(!is_valid_hostname(".invalid"));
        assert!(!is_valid_hostname("invalid."));
    }
    
    #[test]
    fn test_default_config() {
        let config = UringRessConfig::default();
        assert!(config.validate().is_ok());
        assert_eq!(config.server.http_port, 8080);
        assert_eq!(config.server.api_port, 8081);
        assert!(config.server.api_enabled);
        assert_eq!(config.performance.connection_pool_size, 64);
    }
    
    #[test]
    fn test_json_deserialization() {
        let json = r#"{
            "server": {
                "http_port": 9000,
                "tcp_ports": [8080, 8081],
                "api_port": 8090,
                "api_enabled": true,
                "worker_threads": 4,
                "io_uring_queue_depth": 512,
                "connection_pool_size": 128,
                "buffer_pool_size": "128MB"
            },
            "performance": {
                "backend_response_timeout": "30s",
                "connection_pool_size": 128,
                "max_requests_per_connection": 200,
                "standard_buffer_size": "16KB",
                "large_buffer_size": "32KB",
                "header_buffer_size": "8KB",
                "connection_max_age": "5m",
                "circuit_breaker_timeout": "15s",
                "keep_alive_timeout": "10s",
                "circuit_breaker_failure_threshold": 10,
                "max_retries": 3,
                "tcp_nodelay": true,
                "so_reuseport": true,
                "cpu_affinity": [0, 1, 2, 3]
            }
        }"#;
        
        let config: UringRessConfig = serde_json::from_str(json).unwrap();
        assert!(config.validate().is_ok());
        assert_eq!(config.server.http_port, 9000);
        assert_eq!(config.server.buffer_pool_size, 128 * 1024 * 1024);
        assert_eq!(config.performance.backend_response_timeout, Duration::from_secs(30));
        assert_eq!(config.performance.standard_buffer_size, 16 * 1024);
        assert_eq!(config.performance.connection_max_age, Duration::from_secs(300));
        assert_eq!(config.performance.cpu_affinity, Some(vec![0, 1, 2, 3]));
    }
    
    #[test]
    fn test_route_configuration() {
        let json = r#"{
            "server": {
                "http_port": 8080,
                "tcp_ports": [9090],
                "api_port": 8081,
                "api_enabled": true,
                "worker_threads": 4,
                "io_uring_queue_depth": 256,
                "connection_pool_size": 64,
                "buffer_pool_size": "64MB"
            },
            "http_routes": [
                {
                    "host": "api.example.com",
                    "path": "/v1",
                    "backend": {
                        "address": "127.0.0.1",
                        "port": 3000,
                        "health_check_path": "/health"
                    }
                }
            ],
            "tcp_routes": [
                {
                    "port": 5432,
                    "backends": [
                        {
                            "address": "db1.example.com",
                            "port": 5432
                        },
                        {
                            "address": "db2.example.com", 
                            "port": 5432
                        }
                    ],
                    "load_balance_strategy": "round_robin"
                }
            ]
        }"#;
        
        let config: UringRessConfig = serde_json::from_str(json).unwrap();
        assert!(config.validate().is_ok());
        
        assert_eq!(config.http_routes.len(), 1);
        assert_eq!(config.http_routes[0].host, "api.example.com");
        assert_eq!(config.http_routes[0].path, "/v1");
        if let Some(backend) = &config.http_routes[0].backend {
            assert_eq!(backend.address, "127.0.0.1");
            assert_eq!(backend.port, 3000);
            assert_eq!(backend.health_check_path, Some("/health".to_string()));
        } else {
            panic!("Expected single backend configuration");
        }
        
        assert_eq!(config.tcp_routes.len(), 1);
        assert_eq!(config.tcp_routes[0].port, 5432);
        assert_eq!(config.tcp_routes[0].backends.len(), 2);
        assert_eq!(config.tcp_routes[0].backends[0].address, "db1.example.com");
        assert_eq!(config.tcp_routes[0].backends[1].address, "db2.example.com");
        assert!(matches!(config.tcp_routes[0].load_balance_strategy, LoadBalanceStrategy::RoundRobin));
    }
    
    #[test]
    fn test_example_generation_functions() {
        // Test that all example generation functions produce valid configurations
        let minimal = UringRessConfig::generate_minimal();
        assert!(minimal.validate().is_ok());
        assert_eq!(minimal.http_routes.len(), 1);
        assert_eq!(minimal.tcp_routes.len(), 0);
        
        let development = UringRessConfig::generate_development();
        assert!(development.validate().is_ok());
        assert_eq!(development.http_routes.len(), 8); // Now includes /index.html route
        assert_eq!(development.tcp_routes.len(), 1);
        
        let production = UringRessConfig::generate_production();
        assert!(production.validate().is_ok());
        assert_eq!(production.http_routes.len(), 5); // Integration test routes + production routes
        assert_eq!(production.tcp_routes.len(), 2);
        
        let microservices = UringRessConfig::generate_microservices();
        assert!(microservices.validate().is_ok());
        assert_eq!(microservices.http_routes.len(), 5);
        assert_eq!(microservices.tcp_routes.len(), 4);
    }
    
    #[test]
    fn test_serialization_roundtrip() {
        let config = UringRessConfig::generate_minimal();
        
        // Test JSON roundtrip
        let json = config.to_json_pretty().unwrap();
        let parsed_json: UringRessConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed_json.validate().is_ok());
        
        // Test YAML roundtrip
        let yaml = config.to_yaml_pretty().unwrap();
        let parsed_yaml: UringRessConfig = serde_yaml::from_str(&yaml).unwrap();
        assert!(parsed_yaml.validate().is_ok());
    }
    
    #[test]
    #[ignore] // Run with cargo test generate_example_files -- --ignored
    fn generate_example_files() {
        use std::fs;
        
        // Create examples directory if it doesn't exist
        fs::create_dir_all("examples").unwrap();
        
        // Generate minimal.json
        let minimal = UringRessConfig::generate_minimal();
        let json = minimal.to_json_pretty().unwrap();
        fs::write("examples/minimal.json", json).unwrap();
        
        // Generate development.yaml
        let development = UringRessConfig::generate_development();
        let yaml = development.to_yaml_pretty().unwrap();
        fs::write("examples/development.yaml", yaml).unwrap();
        
        // Generate production.json
        let production = UringRessConfig::generate_production();
        let json = production.to_json_pretty().unwrap();
        fs::write("examples/production.json", json).unwrap();
        
        // Generate microservices.yaml
        let microservices = UringRessConfig::generate_microservices();
        let yaml = microservices.to_yaml_pretty().unwrap();
        fs::write("examples/microservices.yaml", yaml).unwrap();
        
        println!("Generated example configuration files:");
        println!("- examples/minimal.json");
        println!("- examples/development.yaml");
        println!("- examples/production.json");
        println!("- examples/microservices.yaml");
    }
}

/// Print information about example configurations for CLI help
pub fn print_example_config_info() {
    println!("examples/minimal.json       - Basic single-service configuration");
    println!("examples/development.yaml   - Multi-service development setup");
    println!("examples/production.json    - Optimized production configuration");
    println!("examples/microservices.yaml - Complex routing for microservices");
}