use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as DeError};
use std::path::Path;
use std::time::Duration;
use anyhow::{anyhow, Result};

/// Protocol types following Kubernetes Gateway API specification
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum Protocol {
    HTTP,
    HTTPS,
    TCP,
    UDP,
}

/// Gateway configuration following Kubernetes Gateway API specification
/// Defines the infrastructure layer with listeners for different protocols and ports
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    #[serde(default = "default_gateway_name")]
    pub name: String,
    pub listeners: Vec<ListenerConfig>,
    #[serde(default = "default_worker_threads")]
    pub worker_threads: usize,
    #[serde(default = "default_api_port")]
    pub api_port: u16,
    #[serde(default = "default_api_enabled")]
    pub api_enabled: bool,
    #[serde(default)]
    pub io_uring_queue_depth: u32,
    #[serde(default)]
    pub connection_pool_size: usize,
    #[serde(deserialize_with = "deserialize_size", serialize_with = "serialize_size", default = "default_buffer_pool_size")]
    pub buffer_pool_size: usize,
}

/// Listener configuration defining protocol and port bindings
/// Follows Kubernetes Gateway API Listener specification
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenerConfig {
    pub name: String,
    pub protocol: Protocol,
    pub port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsConfig>,
}

/// TLS configuration for HTTPS/TLS listeners
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub certificate_refs: Vec<CertificateRef>,
    #[serde(default)]
    pub mode: TlsMode,
}

/// Reference to TLS certificate
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CertificateRef {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// TLS mode configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum TlsMode {
    Terminate,
    Passthrough,
}

impl Default for TlsMode {
    fn default() -> Self {
        TlsMode::Terminate
    }
}

// Default value functions
fn default_gateway_name() -> String {
    "uringress-gateway".to_string()
}

fn default_worker_threads() -> usize {
    4
}

fn default_api_port() -> u16 {
    8081
}

fn default_api_enabled() -> bool {
    true
}

fn default_buffer_pool_size() -> usize {
    64 * 1024 * 1024 // 64MB
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            name: default_gateway_name(),
            listeners: vec![
                ListenerConfig {
                    name: "http".to_string(),
                    protocol: Protocol::HTTP,
                    port: 8080,
                    hostname: None,
                    tls: None,
                },
            ],
            worker_threads: default_worker_threads(),
            api_port: default_api_port(),
            api_enabled: default_api_enabled(),
            io_uring_queue_depth: 256,
            connection_pool_size: 64,
            buffer_pool_size: default_buffer_pool_size(),
        }
    }
}

impl GatewayConfig {
}

/// Root configuration structure for UringRess
/// All parsing happens once at startup - zero runtime overhead
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UringRessConfig {
    #[serde(default)]
    pub gateway: GatewayConfig,
    
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

/// HTTP route configuration following Kubernetes Gateway API HTTPRoute specification
/// Defines how HTTP traffic is routed based on hostnames and request matching
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpRouteConfig {
    #[serde(default)]
    pub parent_refs: Vec<ParentRef>,
    #[serde(default)]
    pub hostnames: Vec<String>,
    pub rules: Vec<HttpRouteRule>,
}

/// Reference to a Gateway that this route attaches to
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParentRef {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

/// HTTP route rule with matches and backend references
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpRouteRule {
    #[serde(default)]
    pub matches: Vec<HttpRouteMatch>,
    pub backend_refs: Vec<BackendRef>,
}

/// HTTP route match conditions
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpRouteMatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<HttpPathMatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<Vec<HttpHeaderMatch>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_params: Option<Vec<HttpQueryParamMatch>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
}

/// HTTP path matching
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpPathMatch {
    #[serde(rename = "type")]
    pub match_type: HttpPathMatchType,
    pub value: String,
}

/// HTTP path match types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum HttpPathMatchType {
    Exact,
    PathPrefix,
    RegularExpression,
}

/// HTTP header matching
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpHeaderMatch {
    #[serde(rename = "type")]
    pub match_type: HttpHeaderMatchType,
    pub name: String,
    pub value: String,
}

/// HTTP header match types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum HttpHeaderMatchType {
    Exact,
    RegularExpression,
}

/// HTTP query parameter matching
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpQueryParamMatch {
    #[serde(rename = "type")]
    pub match_type: HttpQueryParamMatchType,
    pub name: String,
    pub value: String,
}

/// HTTP query parameter match types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum HttpQueryParamMatchType {
    Exact,
    RegularExpression,
}

/// Backend reference with optional weight and filters
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendRef {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub port: u16,
    #[serde(default = "default_backend_weight")]
    pub weight: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

fn default_backend_weight() -> i32 {
    1
}

// Helper implementations for common HTTP route patterns
impl HttpRouteMatch {
    pub fn path_prefix(path: &str) -> Self {
        Self {
            path: Some(HttpPathMatch {
                match_type: HttpPathMatchType::PathPrefix,
                value: path.to_string(),
            }),
            headers: None,
            query_params: None,
            method: None,
        }
    }

}

/// TCP route configuration following Kubernetes Gateway API TCPRoute specification
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcpRouteConfig {
    #[serde(default)]
    pub parent_refs: Vec<ParentRef>,
    pub rules: Vec<TcpRouteRule>,
}

/// TCP route rule with backend references
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcpRouteRule {
    pub backend_refs: Vec<BackendRef>,
}

/// Backend configuration for both HTTP and TCP routes
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
    pub address: String,
    pub port: u16,
    pub health_check_path: Option<String>,
}

/// Load balancing strategy for backend selection
/// 
/// Currently supported strategies:
/// - `RoundRobin`: Distributes requests evenly across backends in rotation
/// - `LeastConnections`: **[NOT YET IMPLEMENTED]** Would select backend with fewest active connections
///   
/// **Note**: `LeastConnections` is accepted in configuration files for future compatibility,
/// but currently falls back to `RoundRobin` behavior. This will be implemented in a future version
/// once proper connection tracking infrastructure is added to the io_uring data plane.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadBalanceStrategy {
    /// Round-robin distribution across backends
    RoundRobin,
    /// **[PLANNED - NOT IMPLEMENTED]** Select backend with fewest active connections
    /// Currently falls back to `RoundRobin` behavior
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
            gateway: GatewayConfig::default(),
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
    
    
}

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
                    },
                ],
                worker_threads: 2,
                api_port: 8081,
                api_enabled: true,
                io_uring_queue_depth: 128,
                connection_pool_size: 32,
                buffer_pool_size: 32 * 1024 * 1024, // 32MB
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
                    },
                    ListenerConfig {
                        name: "tcp-ssh".to_string(),
                        protocol: Protocol::TCP,
                        port: 2222,
                        hostname: None,
                        tls: None,
                    },
                ],
                worker_threads: 4,
                api_port: 8081,
                api_enabled: true,
                io_uring_queue_depth: 256,
                connection_pool_size: 64,
                buffer_pool_size: 64 * 1024 * 1024, // 64MB
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
                    },
                    ListenerConfig {
                        name: "http-8080".to_string(),
                        protocol: Protocol::HTTP,
                        port: 8080,
                        hostname: None,
                        tls: None,
                    },
                    ListenerConfig {
                        name: "tcp-5432".to_string(),
                        protocol: Protocol::TCP,
                        port: 5432,
                        hostname: None,
                        tls: None,
                    },
                    ListenerConfig {
                        name: "tcp-6379".to_string(),
                        protocol: Protocol::TCP,
                        port: 6379,
                        hostname: None,
                        tls: None,
                    },
                ],
                worker_threads: num_cpus::get(),
                api_port: 8090,
                api_enabled: true,
                io_uring_queue_depth: 512,
                connection_pool_size: 256,
                buffer_pool_size: 256 * 1024 * 1024, // 256MB
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
                    },
                    ListenerConfig {
                        name: "tcp-5432".to_string(),
                        protocol: Protocol::TCP,
                        port: 5432,
                        hostname: None,
                        tls: None,
                    },
                    ListenerConfig {
                        name: "tcp-6379".to_string(),
                        protocol: Protocol::TCP,
                        port: 6379,
                        hostname: None,
                        tls: None,
                    },
                    ListenerConfig {
                        name: "tcp-3306".to_string(),
                        protocol: Protocol::TCP,
                        port: 3306,
                        hostname: None,
                        tls: None,
                    },
                    ListenerConfig {
                        name: "tcp-27017".to_string(),
                        protocol: Protocol::TCP,
                        port: 27017,
                        hostname: None,
                        tls: None,
                    },
                ],
                worker_threads: 8,
                api_port: 8081,
                api_enabled: true,
                io_uring_queue_depth: 512,
                connection_pool_size: 128,
                buffer_pool_size: 128 * 1024 * 1024, // 128MB
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

// Configuration validation
impl UringRessConfig {
    /// Validate the entire configuration for consistency and correctness
    /// All validation happens once at startup
    pub fn validate(&self) -> Result<()> {
        self.gateway.validate()?;
        
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
        
        // Check gateway listener ports
        for listener in &self.gateway.listeners {
            if !used_ports.insert(listener.port) {
                return Err(anyhow!("Port conflict: Listener port {} already in use", listener.port));
            }
        }
        
        // Check API port
        if self.gateway.api_enabled {
            if !used_ports.insert(self.gateway.api_port) {
                return Err(anyhow!("Port conflict: API port {} already in use", self.gateway.api_port));
            }
        }
        
        // Check metrics port
        if self.observability.metrics.enabled {
            if !used_ports.insert(self.observability.metrics.port) {
                return Err(anyhow!("Port conflict: Metrics port {} already in use", self.observability.metrics.port));
            }
        }
        
        // TCP route ports are checked via parent_refs and gateway listeners
        // No additional port validation needed since TCP routes must reference valid listeners
        
        Ok(())
    }
}

impl HttpRouteConfig {
    fn validate(&self) -> Result<()> {
        // Validate parent refs
        if self.parent_refs.is_empty() {
            return Err(anyhow!("HTTP route must have at least one parent_ref"));
        }
        
        for parent_ref in &self.parent_refs {
            parent_ref.validate()?;
        }
        
        // Validate hostnames
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
        
        // Validate rules
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
    fn validate(&self) -> Result<()> {
        // Validate parent refs
        if self.parent_refs.is_empty() {
            return Err(anyhow!("TCP route must have at least one parent_ref"));
        }
        
        for parent_ref in &self.parent_refs {
            parent_ref.validate()?;
        }
        
        // Validate rules
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
    fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            return Err(anyhow!("Gateway name cannot be empty"));
        }
        
        if self.listeners.is_empty() {
            return Err(anyhow!("Gateway must have at least one listener"));
        }
        
        for (i, listener) in self.listeners.iter().enumerate() {
            listener.validate().map_err(|e| anyhow!("Listener {}: {}", i, e))?;
        }
        
        validate_port(self.api_port, "API port")?;
        
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
        
        if self.weight <= 0 {
            return Err(anyhow!("Backend ref weight must be greater than 0: {}", self.weight));
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

/// Normalize HTTP host header by removing port number
/// Required for Gateway API compliance where hostnames don't include ports
/// Examples: "localhost:8080" → "localhost", "api.example.com:443" → "api.example.com"
pub fn normalize_host_header(host: &str) -> String {
    // Split by ':' and take only the hostname part
    // This handles both IPv4:port and hostname:port patterns
    match host.split(':').next() {
        Some(hostname) => hostname.to_string(),
        None => host.to_string(), // Fallback to original if no ':' found
    }
}

/// Resolve service name to IP address
/// For MVP: simple localhost resolution
/// In production: would integrate with service discovery (DNS, Consul, K8s, etc.)
fn resolve_service_address(service_name: &str) -> String {
    // For development/testing, all services resolve to localhost
    // This is appropriate for our examples where services run locally
    match service_name {
        // Allow some common service patterns
        name if name.contains("localhost") => "127.0.0.1".to_string(),
        name if name.contains("127.0.0.1") => "127.0.0.1".to_string(),
        // All other services resolve to localhost for MVP
        _ => "127.0.0.1".to_string(),
    }
}

// Configuration to runtime structure conversion
impl UringRessConfig {
    /// Convert configuration to RouteTable for runtime use
    /// All route processing happens once at startup - zero runtime overhead
    pub fn to_route_table(&self) -> Result<crate::routing::RouteTable> {
        use crate::routing::{RouteTable, Backend};
        
        let mut route_table = RouteTable::new();
        
        tracing::debug!("Converting {} HTTP routes to route table", self.http_routes.len());
        
        // Convert HTTP routes using new Gateway API structure
        for (route_idx, http_route) in self.http_routes.iter().enumerate() {
            tracing::debug!("Processing HTTP route {}: {} hostnames, {} rules", 
                route_idx, http_route.hostnames.len(), http_route.rules.len());
            // For each hostname in the route
            for hostname in &http_route.hostnames {
                tracing::debug!("  Processing hostname: {}", hostname);
                // For each rule in the route
                for (rule_idx, rule) in http_route.rules.iter().enumerate() {
                    tracing::debug!("    Processing rule {}: {} matches, {} backend_refs", 
                        rule_idx, rule.matches.len(), rule.backend_refs.len());
                    // For each match in the rule
                    for route_match in &rule.matches {
                        // Extract path from match (default to "/" if no path match)
                        let path = if let Some(path_match) = &route_match.path {
                            &path_match.value
                        } else {
                            "/"
                        };
                        
                        tracing::debug!("      Adding route: {} {} -> {} backends", 
                            hostname, path, rule.backend_refs.len());
                        
                        // Convert backend references to Backend objects
                        let mut backends = Vec::new();
                        for backend_ref in &rule.backend_refs {
                            // For MVP: resolve service names to localhost
                            // In production, this would resolve via service discovery
                            let backend_address = resolve_service_address(&backend_ref.name);
                            let backend = Backend::new(
                                backend_address.clone(),
                                backend_ref.port
                            )?;
                            backends.push(backend);
                            tracing::debug!("        Backend: {}:{} (service: {})", 
                                backend_address, backend_ref.port, backend_ref.name);
                        }
                        
                        if backends.len() == 1 {
                            // Single backend
                            route_table.add_http_route(hostname, path, backends.into_iter().next().unwrap());
                        } else {
                            // Multiple backends with round-robin
                            route_table.add_http_route_with_pool(
                                hostname,
                                path,
                                backends,
                                crate::routing::LoadBalanceStrategy::RoundRobin
                            );
                        }
                    }
                }
            }
        }
        
        // Convert TCP routes using new Gateway API structure
        for tcp_route in &self.tcp_routes {
            // Find the listener port for this TCP route by examining parent refs
            if let Some(parent_ref) = tcp_route.parent_refs.first() {
                if let Some(section_name) = &parent_ref.section_name {
                    // Find the listener with this section name
                    if let Some(listener) = self.gateway.listeners.iter()
                        .find(|l| l.name == *section_name && l.protocol == Protocol::TCP) {
                        
                        // Convert backend references to Backend objects
                        for rule in &tcp_route.rules {
                            let mut backends = Vec::new();
                            for backend_ref in &rule.backend_refs {
                                let backend = Backend::new(
                                    format!("127.0.0.1"), // Placeholder - in real implementation would resolve service
                                    backend_ref.port
                                )?;
                                backends.push(backend);
                            }
                            
                            // Add TCP route using the listener port
                            route_table.add_tcp_route(listener.port, backends);
                        }
                    }
                }
            }
        }
        
        tracing::debug!("Route table conversion completed: {} HTTP routes, {} TCP routes", 
            route_table.http_routes.len(), route_table.tcp_routes.len());
        
        Ok(route_table)
    }
}

impl PerformanceConfig {
    /// Convert PerformanceConfig and GatewayConfig to RuntimeConfig
    /// All values are pre-computed at startup for zero runtime overhead
    pub fn to_runtime_config(&self, gateway: &GatewayConfig) -> crate::runtime_config::RuntimeConfig {
        use std::collections::HashMap;
        
        // Build listener map for zero-overhead protocol detection
        let mut listeners = HashMap::new();
        for listener in &gateway.listeners {
            let protocol = match listener.protocol {
                Protocol::HTTP => crate::runtime_config::Protocol::HTTP,
                Protocol::HTTPS => crate::runtime_config::Protocol::HTTPS,
                Protocol::TCP => crate::runtime_config::Protocol::TCP,
                Protocol::UDP => crate::runtime_config::Protocol::UDP,
            };
            
            let listener_info = crate::runtime_config::ListenerInfo {
                protocol,
            };
            
            listeners.insert(listener.port, listener_info);
        }
        
        crate::runtime_config::RuntimeConfig::new(
            listeners,
            self.standard_buffer_size,
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
    fn test_default_config() {
        let config = UringRessConfig::default();
        assert!(config.validate().is_ok());
        assert_eq!(config.gateway.api_port, 8081);
        assert!(config.gateway.api_enabled);
        assert_eq!(config.performance.connection_pool_size, 64);
        // Gateway should have at least one listener
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
                "worker_threads": 4,
                "api_port": 8090,
                "api_enabled": true,
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
        assert_eq!(config.gateway.listeners[0].port, 9000);
        assert_eq!(config.gateway.buffer_pool_size, 128 * 1024 * 1024);
        assert_eq!(config.performance.backend_response_timeout, Duration::from_secs(30));
        assert_eq!(config.performance.standard_buffer_size, 16 * 1024);
        assert_eq!(config.performance.connection_max_age, Duration::from_secs(300));
        assert_eq!(config.performance.cpu_affinity, Some(vec![0, 1, 2, 3]));
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
                "worker_threads": 4,
                "api_port": 8081,
                "api_enabled": true,
                "io_uring_queue_depth": 256,
                "connection_pool_size": 64,
                "buffer_pool_size": "64MB"
            },
            "http_routes": [
                {
                    "parent_refs": [
                        {
                            "name": "test-gateway",
                            "section_name": "http-8080",
                            "port": 8080
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
                                    "port": 3000,
                                    "weight": 100
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
                            "section_name": "tcp-5432",
                            "port": 5432
                        }
                    ],
                    "rules": [
                        {
                            "backend_refs": [
                                {
                                    "name": "db1-service",
                                    "port": 5432,
                                    "weight": 50
                                },
                                {
                                    "name": "db2-service",
                                    "port": 5432,
                                    "weight": 50
                                }
                            ]
                        }
                    ]
                }
            ]
        }"#;
        
        let config: UringRessConfig = serde_json::from_str(json).unwrap();
        assert!(config.validate().is_ok());
        
        // Test Gateway API structure
        assert_eq!(config.gateway.name, "test-gateway");
        assert_eq!(config.gateway.listeners.len(), 2);
        
        // Test HTTP routes
        assert_eq!(config.http_routes.len(), 1);
        assert_eq!(config.http_routes[0].hostnames[0], "api.example.com");
        assert_eq!(config.http_routes[0].rules[0].matches[0].path.as_ref().unwrap().value, "/v1");
        assert_eq!(config.http_routes[0].rules[0].backend_refs[0].name, "api-service");
        assert_eq!(config.http_routes[0].rules[0].backend_refs[0].port, 3000);
        
        // Test TCP routes  
        assert_eq!(config.tcp_routes.len(), 1);
        assert_eq!(config.tcp_routes[0].rules[0].backend_refs.len(), 2);
        assert_eq!(config.tcp_routes[0].rules[0].backend_refs[0].name, "db1-service");
        assert_eq!(config.tcp_routes[0].rules[0].backend_refs[1].name, "db2-service");
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
        assert_eq!(development.http_routes.len(), 6); // Gateway API groups routes efficiently
        assert_eq!(development.tcp_routes.len(), 1);
        
        let production = UringRessConfig::generate_production();
        assert!(production.validate().is_ok());
        assert_eq!(production.http_routes.len(), 3); // Gateway API groups routes efficiently
        assert_eq!(production.tcp_routes.len(), 2);
        
        let microservices = UringRessConfig::generate_microservices();
        assert!(microservices.validate().is_ok());
        assert_eq!(microservices.http_routes.len(), 2); // Gateway API groups microservices routes
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
    println!("examples/production.json    - Production configuration");
    println!("examples/microservices.yaml - Complex routing for microservices");
}