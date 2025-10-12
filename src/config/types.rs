use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::parsing::{deserialize_size, serialize_size, deserialize_duration, serialize_duration};

/// Protocol types following Kubernetes Gateway API specification
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
#[allow(clippy::upper_case_acronyms)]
pub enum Protocol {
    HTTP,
    HTTPS,
    TCP,
    UDP,
}

/// Individual worker configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerConfig {
    /// Worker ID (0-based index)
    pub id: usize,

    /// Buffer size for this worker's buffer ring
    #[serde(deserialize_with = "deserialize_size", serialize_with = "serialize_size")]
    pub buffer_size: usize,

    /// Number of buffers to allocate for this worker
    #[serde(default = "default_buffer_count")]
    pub buffer_count: u16,

    /// Enable TCP_NODELAY for immediate packet transmission
    #[serde(default)]
    pub tcp_nodelay: bool,
}

/// Gateway configuration following Kubernetes Gateway API specification
/// Defines the infrastructure layer with listeners for different protocols and ports
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    #[serde(default = "default_gateway_name")]
    pub name: String,
    pub listeners: Vec<ListenerConfig>,

    /// Optional worker-specific configurations
    /// If not specified, generates defaults from worker_threads
    pub workers: Option<Vec<WorkerConfig>>,

    #[serde(default = "default_worker_threads")]
    pub worker_threads: usize,
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
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
}

// Default value functions
fn default_gateway_name() -> String {
    "uringress-gateway".to_string()
}

fn default_worker_threads() -> usize {
    4
}

pub(crate) fn default_buffer_count() -> u16 {
    256 // Increased for high concurrent load handling
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
                    address: None,
                    interface: None,
                },
            ],
            workers: None,
            worker_threads: default_worker_threads(),
        }
    }
}

impl GatewayConfig {
    /// Get effective worker configurations, generating defaults if none specified
    pub fn get_worker_configs(&self) -> Vec<WorkerConfig> {
        if let Some(ref workers) = self.workers {
            workers.clone()
        } else {
            self.generate_default_worker_configs()
        }
    }

    fn generate_default_worker_configs(&self) -> Vec<WorkerConfig> {
        (0..self.worker_threads)
            .map(|id| WorkerConfig {
                id,
                buffer_size: 16384, // 16KB — balanced for both HTTP and TCP
                buffer_count: default_buffer_count(),
                tcp_nodelay: true,
            })
            .collect()
    }

    /// Validate worker configurations for consistency and correctness
    pub fn validate_worker_configs(&self) -> Result<(), String> {
        let worker_configs = self.get_worker_configs();

        if worker_configs.is_empty() {
            return Err("At least one worker must be configured".to_string());
        }

        let mut ids: Vec<usize> = worker_configs.iter().map(|w| w.id).collect();
        ids.sort_unstable();

        for (expected, &actual) in ids.iter().enumerate() {
            if expected != actual {
                return Err(format!("Worker IDs must be sequential starting from 0. Expected {}, found {}", expected, actual));
            }
        }

        for worker in &worker_configs {
            if worker.buffer_size < 1024 {
                return Err(format!("Worker {}: buffer_size must be at least 1KB, got {}", worker.id, worker.buffer_size));
            }
            if worker.buffer_size > 1024 * 1024 {
                return Err(format!("Worker {}: buffer_size must be at most 1MB, got {}", worker.id, worker.buffer_size));
            }
            if worker.buffer_count == 0 {
                return Err(format!("Worker {}: buffer_count must be at least 1", worker.id));
            }
            if worker.buffer_count > 1024 {
                return Err(format!("Worker {}: buffer_count must be at most 1024, got {}", worker.id, worker.buffer_count));
            }
        }

        Ok(())
    }
}

/// Root configuration structure for UringRess
/// All parsing happens once at startup - zero runtime overhead
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UringRessConfig {
    #[serde(default)]
    pub gateway: GatewayConfig,

    #[serde(default)]
    pub http_routes: Vec<HttpRouteConfig>,

    #[serde(default)]
    pub tcp_routes: Vec<TcpRouteConfig>,

    #[serde(default)]
    pub udp_routes: Vec<UdpRouteConfig>,

    #[serde(default)]
    pub observability: ObservabilityConfig,

    #[serde(default)]
    pub performance: PerformanceConfig,
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
    pub section_name: Option<String>,
}

/// HTTP route rule with matches and backend references
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpRouteRule {
    #[serde(default)]
    pub matches: Vec<HttpRouteMatch>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<HttpRouteFilter>,
    pub backend_refs: Vec<BackendRef>,
}

/// HTTP route match conditions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpRouteMatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<HttpPathMatch>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<HttpHeaderMatch>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_params: Vec<HttpQueryParamMatch>,
}

/// HTTP query parameter match condition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpQueryParamMatch {
    pub name: String,
    pub value: String,
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub enum HttpPathMatchType {
    PathPrefix,
    Exact,
}

/// HTTP header match condition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpHeaderMatch {
    pub name: String,
    pub value: String,
}

/// HTTP route filter (Gateway API spec)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HttpRouteFilter {
    RequestHeaderModifier {
        #[serde(default)]
        add: Vec<HttpHeader>,
        #[serde(default)]
        set: Vec<HttpHeader>,
        #[serde(default)]
        remove: Vec<String>,
    },
    ResponseHeaderModifier {
        #[serde(default)]
        add: Vec<HttpHeader>,
        #[serde(default)]
        set: Vec<HttpHeader>,
        #[serde(default)]
        remove: Vec<String>,
    },
    RequestRedirect {
        #[serde(skip_serializing_if = "Option::is_none")]
        scheme: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        hostname: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default = "default_redirect_status")]
        status_code: u16,
    },
    URLRewrite {
        #[serde(skip_serializing_if = "Option::is_none")]
        hostname: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<HttpURLRewritePath>,
    },
    RequestMirror {
        backend_ref: BackendRef,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HttpURLRewritePath {
    ReplaceFullPath { value: String },
    ReplacePrefixMatch { value: String },
}

fn default_redirect_status() -> u16 {
    302
}

/// HTTP header name/value pair for filter operations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpHeader {
    pub name: String,
    pub value: String,
}

/// Backend reference for routing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendRef {
    pub name: String,
    pub port: u16,
    #[serde(default = "default_backend_weight", skip_serializing_if = "is_default_weight")]
    pub weight: u32,
}

fn default_backend_weight() -> u32 {
    1
}

fn is_default_weight(w: &u32) -> bool {
    *w == 1
}

impl HttpRouteMatch {
    pub fn path_prefix(path: &str) -> Self {
        Self {
            method: None,
            path: Some(HttpPathMatch {
                match_type: HttpPathMatchType::PathPrefix,
                value: path.to_string(),
            }),
            headers: vec![],
            query_params: vec![],
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

/// UDP route configuration following Kubernetes Gateway API UDPRoute specification
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UdpRouteConfig {
    #[serde(default)]
    pub parent_refs: Vec<ParentRef>,
    pub rules: Vec<UdpRouteRule>,
}

/// UDP route rule with backend references
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UdpRouteRule {
    pub backend_refs: Vec<BackendRef>,
}

/// Observability configuration
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservabilityConfig {
    #[serde(default)]
    pub logging: LoggingConfig,
}

/// Logging configuration
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

/// Log format enumeration
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Json,
    #[default]
    Pretty,
    Compact,
}

/// Log output destination
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogOutput {
    #[default]
    Stdout,
    Stderr,
    File(String),
}

/// Performance configuration
/// All values pre-computed at startup for zero runtime overhead
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerformanceConfig {
    #[serde(deserialize_with = "deserialize_duration", serialize_with = "serialize_duration")]
    pub keep_alive_timeout: Duration,

    #[serde(default = "default_backend_timeout", deserialize_with = "deserialize_duration", serialize_with = "serialize_duration")]
    pub backend_timeout: Duration,

    #[serde(default = "default_udp_session_timeout", deserialize_with = "deserialize_duration", serialize_with = "serialize_duration")]
    pub udp_session_timeout: Duration,
}

fn default_backend_timeout() -> Duration {
    Duration::from_secs(30)
}

fn default_udp_session_timeout() -> Duration {
    Duration::from_secs(30)
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            keep_alive_timeout: Duration::from_secs(5),
            backend_timeout: default_backend_timeout(),
            udp_session_timeout: default_udp_session_timeout(),
        }
    }
}
