use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::parsing::{deserialize_duration, serialize_duration, deserialize_duration_opt, serialize_duration_opt};

/// Protocol types following Kubernetes Gateway API specification
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
#[allow(clippy::upper_case_acronyms)]
pub enum Protocol {
    HTTP,
    HTTPS,
    TLS,
    TCP,
    UDP,
}

/// Gateway configuration following Kubernetes Gateway API specification
/// Defines the infrastructure layer with listeners for different protocols and ports
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    #[serde(default = "default_gateway_name")]
    pub name: String,
    pub listeners: Vec<ListenerConfig>,

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TlsConfig {
    pub mode: TlsMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub certificate_refs: Vec<CertificateRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TlsMode {
    Terminate,
    Passthrough,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CertificateRef {
    pub name: String,
    #[serde(skip)]
    pub cert_pem: Option<Vec<u8>>,
    #[serde(skip)]
    pub key_pem: Option<Vec<u8>>,
}

// Default value functions
fn default_gateway_name() -> String {
    "portail-gateway".to_string()
}

fn default_worker_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
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
                    tls: None,
                },
            ],
            worker_threads: default_worker_threads(),
        }
    }
}

/// Root configuration structure for Portail
/// All parsing happens once at startup - zero runtime overhead
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct PortailConfig {
    #[serde(default)]
    pub gateway: GatewayConfig,

    #[serde(default)]
    pub http_routes: Vec<HttpRouteConfig>,

    #[serde(default)]
    pub tcp_routes: Vec<TcpRouteConfig>,

    #[serde(default)]
    pub tls_routes: Vec<TlsRouteConfig>,

    #[serde(default)]
    pub udp_routes: Vec<UdpRouteConfig>,

    #[serde(default)]
    pub observability: ObservabilityConfig,

    #[serde(default)]
    pub performance: PerformanceConfig,

    /// Pod IP + targetPort overrides for headless services (not serialized).
    /// Map: (backend_fqdn, service_port) → Vec<(pod_ip, target_port)>
    #[serde(skip)]
    pub endpoint_overrides: std::collections::HashMap<(String, u16), Vec<(String, u16)>>,
}

/// HTTP route configuration following Kubernetes Gateway API HTTPRoute specification
/// Defines how HTTP traffic is routed based on hostnames and request matching
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct ParentRef {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section_name: Option<String>,
}

/// HTTP route rule with matches and backend references
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRouteRule {
    #[serde(default)]
    pub matches: Vec<HttpRouteMatch>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<HttpRouteFilter>,
    pub backend_refs: Vec<BackendRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeouts: Option<HttpRouteTimeouts>,
}

/// Per-rule timeout configuration following Gateway API HTTPRouteTimeouts
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRouteTimeouts {
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_duration_opt", serialize_with = "serialize_duration_opt")]
    pub request: Option<Duration>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_duration_opt", serialize_with = "serialize_duration_opt")]
    pub backend_request: Option<Duration>,
}

/// HTTP route match conditions
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
    #[serde(default, rename = "type")]
    pub match_type: StringMatchType,
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
    RegularExpression,
}

/// String match type for headers and query parameters
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub enum StringMatchType {
    #[default]
    Exact,
    RegularExpression,
}

/// HTTP header match condition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpHeaderMatch {
    pub name: String,
    pub value: String,
    #[serde(default, rename = "type")]
    pub match_type: StringMatchType,
}

/// Header modification config shared by request/response header modifiers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeaderModifierConfig {
    #[serde(default)]
    pub add: Vec<HttpHeader>,
    #[serde(default)]
    pub set: Vec<HttpHeader>,
    #[serde(default)]
    pub remove: Vec<String>,
}

/// RequestRedirect config
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestRedirectConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<HttpURLRewritePath>,
    #[serde(default = "default_redirect_status")]
    pub status_code: u16,
}

/// URLRewrite config
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct URLRewriteConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<HttpURLRewritePath>,
}

/// RequestMirror config
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestMirrorConfig {
    pub backend_ref: BackendRef,
}

/// HTTP route filter (Gateway API spec)
///
/// Wire format nests each filter's config under a camelCase key matching the type:
/// ```yaml
/// - type: RequestHeaderModifier
///   requestHeaderModifier:
///     add: [...]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HttpRouteFilter {
    RequestHeaderModifier {
        #[serde(rename = "requestHeaderModifier")]
        config: HeaderModifierConfig,
    },
    ResponseHeaderModifier {
        #[serde(rename = "responseHeaderModifier")]
        config: HeaderModifierConfig,
    },
    RequestRedirect {
        #[serde(rename = "requestRedirect")]
        config: RequestRedirectConfig,
    },
    URLRewrite {
        #[serde(rename = "urlRewrite")]
        config: URLRewriteConfig,
    },
    RequestMirror {
        #[serde(rename = "requestMirror")]
        config: RequestMirrorConfig,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HttpURLRewritePath {
    ReplaceFullPath {
        #[serde(rename = "replaceFullPath")]
        value: String,
    },
    ReplacePrefixMatch {
        #[serde(rename = "replacePrefixMatch")]
        value: String,
    },
}

fn default_redirect_status() -> u16 {
    302
}

/// Re-export from routing — single HttpHeader type used by both config and routing
pub use crate::routing::HttpHeader;

/// Backend reference for routing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendRef {
    pub name: String,
    pub port: u16,
    #[serde(default = "default_backend_weight", skip_serializing_if = "is_default_weight")]
    pub weight: u32,
    /// Original group from the gateway-api backendRef (empty string = core API group)
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub group: String,
    /// Original kind from the gateway-api backendRef (default "Service")
    #[serde(default = "default_backend_kind", skip_serializing_if = "is_default_kind")]
    pub kind: String,
    /// Per-backend filters (e.g. BackendRequestHeaderModifier)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<HttpRouteFilter>,
}

fn default_backend_kind() -> String {
    "Service".to_string()
}

fn is_default_kind(k: &String) -> bool {
    k == "Service"
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
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct TcpRouteConfig {
    #[serde(default)]
    pub parent_refs: Vec<ParentRef>,
    pub rules: Vec<TcpRouteRule>,
}

/// TCP route rule with backend references
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct TcpRouteRule {
    pub backend_refs: Vec<BackendRef>,
}

/// UDP route configuration following Kubernetes Gateway API UDPRoute specification
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct UdpRouteConfig {
    #[serde(default)]
    pub parent_refs: Vec<ParentRef>,
    pub rules: Vec<UdpRouteRule>,
}

/// UDP route rule with backend references
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct UdpRouteRule {
    pub backend_refs: Vec<BackendRef>,
}

/// TLS route configuration following Kubernetes Gateway API TLSRoute specification
/// Routes TLS connections based on SNI hostname
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct TlsRouteConfig {
    #[serde(default)]
    pub parent_refs: Vec<ParentRef>,
    #[serde(default)]
    pub hostnames: Vec<String>,
    pub rules: Vec<TlsRouteRule>,
}

/// TLS route rule with backend references
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct TlsRouteRule {
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
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct PerformanceConfig {
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
            backend_timeout: default_backend_timeout(),
            udp_session_timeout: default_udp_session_timeout(),
        }
    }
}
