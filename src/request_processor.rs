//! Request Processing Pipeline
//!
//! Analyzes incoming data and produces routing decisions.
//! Works on byte slices — no I/O runtime dependency.

use std::hash::Hasher;
use std::sync::Arc;
use anyhow::Result;
use std::net::SocketAddr;
use crate::logging::{debug, error};
use crate::http_parser::{ConnectionType, extract_routing_info};
use crate::health::HealthRegistry;
use crate::routing::{RouteTable, HttpFilter, URLRewritePath, BackendSelector, HttpHeader};

#[derive(Debug, Clone)]
pub struct HeaderModifications {
    pub add: Arc<Vec<HttpHeader>>,
    pub set: Arc<Vec<HttpHeader>>,
    pub remove: Arc<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct URLRewrite {
    pub hostname: Option<String>,
    pub path: Option<RewrittenPath>,
}

#[derive(Debug, Clone)]
pub enum RewrittenPath {
    Full(String),
    PrefixReplaced(String),
}

/// Join a prefix replacement with the remaining suffix from the original path.
/// Avoids double-slash: `/` + `/three` → `/three`, not `//three`.
fn join_prefix_path(prefix: &str, suffix: &str) -> String {
    // Strip trailing slash from prefix if suffix starts with slash
    let prefix = if prefix.ends_with('/') && suffix.starts_with('/') {
        &prefix[..prefix.len() - 1]
    } else {
        prefix
    };
    let needs_slash = !prefix.ends_with('/') && !suffix.starts_with('/') && !suffix.is_empty();
    let mut result = String::with_capacity(prefix.len() + suffix.len() + usize::from(needs_slash));
    result.push_str(prefix);
    if needs_slash { result.push('/'); }
    result.push_str(suffix);
    if result.is_empty() { result.push('/'); }
    result
}

/// Filter data only allocated when a route actually has filters.
#[derive(Debug, Clone)]
pub struct HttpFilterData {
    pub request_header_mods: Option<HeaderModifications>,
    pub response_header_mods: Option<HeaderModifications>,
    pub url_rewrite: Option<URLRewrite>,
    pub mirror_addrs: Vec<SocketAddr>,
}

#[derive(Debug, Clone)]
pub enum ProcessingDecision {
    /// Boxed filter data keeps the enum small (~40 bytes vs ~256).
    /// None on the fast path (no filters) avoids the heap allocation entirely.
    HttpForward {
        backend_addr: SocketAddr,
        keepalive: bool,
        filters: Option<Box<HttpFilterData>>,
        backend_timeout: Option<std::time::Duration>,
        request_timeout: Option<std::time::Duration>,
    },
    HttpRedirect {
        status_code: u16,
        location: String,
        keepalive: bool,
    },
    TcpForward {
        backend_addr: SocketAddr,
    },
    UdpForward {
        backend_addr: SocketAddr,
    },
    SendHttpError {
        error_code: u16,
        close_connection: bool,
    },
    CloseConnection,
}

pub fn analyze_request(
    routes: &RouteTable,
    backend_selector: &BackendSelector,
    request_data: &[u8],
    server_port: u16,
    health: &HealthRegistry,
    is_tls: bool,
) -> Result<ProcessingDecision> {
    if request_data.is_empty() {
        return Ok(ProcessingDecision::CloseConnection);
    }

    if is_http_request(request_data) {
        analyze_http_request(routes, backend_selector, request_data, health, is_tls, server_port)
    } else {
        analyze_tcp_request(routes, backend_selector, server_port, health)
    }
}

#[inline(always)]
fn is_http_request(data: &[u8]) -> bool {
    if data.len() < 4 { return false; }

    const GET:  u32 = u32::from_ne_bytes(*b"GET ");
    const POST: u32 = u32::from_ne_bytes(*b"POST");
    const PUT:  u32 = u32::from_ne_bytes(*b"PUT ");
    const DELE: u32 = u32::from_ne_bytes(*b"DELE"); // DELETE
    const HEAD: u32 = u32::from_ne_bytes(*b"HEAD");
    const OPTI: u32 = u32::from_ne_bytes(*b"OPTI"); // OPTIONS
    const PATC: u32 = u32::from_ne_bytes(*b"PATC"); // PATCH

    // Compare first 4 bytes as a u32 — all standard HTTP methods
    // are uniquely identified by their first 4 bytes (including space for 3-letter methods).
    let tag = u32::from_ne_bytes([data[0], data[1], data[2], data[3]]);
    matches!(tag, GET | POST | PUT | DELE | HEAD | OPTI | PATC)
}

#[inline(always)]
fn compute_route_hash(path: &str) -> u64 {
    let mut hasher = fnv::FnvHasher::default();
    hasher.write(path.as_bytes());
    hasher.finish()
}

fn analyze_http_request(
    routes: &RouteTable,
    backend_selector: &BackendSelector,
    request_data: &[u8],
    health: &HealthRegistry,
    is_tls: bool,
    server_port: u16,
) -> Result<ProcessingDecision> {
    let request_info = extract_routing_info(request_data)?;
    let keepalive = request_info.connection_type != ConnectionType::Close;

    let rule = match routes.find_http_route(request_info.host, request_info.method, request_info.path, request_info.header_data, request_info.query_string, server_port) {
        Ok(rule) => rule,
        Err(_) => {
            return Ok(ProcessingDecision::SendHttpError {
                error_code: 404,
                close_connection: !keepalive,
            });
        }
    };

    // Check for redirect filters FIRST — redirect rules often have no backends,
    // so we must handle them before the empty-backends check.
    if rule.has_filters {
        for filter in &rule.filters {
            if let HttpFilter::RequestRedirect { scheme, hostname, port, path, status_code } = filter {
                let redirect_path = match path {
                    Some(URLRewritePath::ReplaceFullPath(value)) => value.clone(),
                    Some(URLRewritePath::ReplacePrefixMatch(value)) => {
                        join_prefix_path(value, &request_info.path[rule.path.len()..])
                    }
                    None => request_info.path.to_string(),
                };
                let default_scheme = if is_tls { "https" } else { "http" };
                let location = build_redirect_location(
                    scheme.as_deref(),
                    hostname.as_deref().unwrap_or(request_info.host),
                    *port,
                    &redirect_path,
                    default_scheme,
                    server_port,
                );
                return Ok(ProcessingDecision::HttpRedirect {
                    status_code: *status_code,
                    location,
                    keepalive,
                });
            }
        }
    }

    if rule.backends.is_empty() {
        return Ok(ProcessingDecision::SendHttpError {
            error_code: 500,
            close_connection: !keepalive,
        });
    }

    let route_hash = compute_route_hash(request_info.path);
    let backend_index = match backend_selector.select_healthy_weighted_backend(route_hash, rule, health) {
        Some(idx) => idx,
        None => {
            return Ok(ProcessingDecision::SendHttpError {
                error_code: 503,
                close_connection: !keepalive,
            });
        }
    };
    let selected_backend = &rule.backends[backend_index];

    // Fast path: no filters — no Box allocation, enum stays 40 bytes
    if !rule.has_filters {
        return Ok(ProcessingDecision::HttpForward {
            backend_addr: selected_backend.socket_addr,
            keepalive,
            filters: None,
            backend_timeout: rule.backend_request_timeout,
            request_timeout: rule.request_timeout,
        });
    }

    // Slow path: process filters, box the result
    let mut request_header_mods: Option<HeaderModifications> = None;
    let mut response_header_mods: Option<HeaderModifications> = None;
    let mut url_rewrite: Option<URLRewrite> = None;
    let mut mirror_addrs: Vec<SocketAddr> = Vec::new();

    for filter in &rule.filters {
        match filter {
            HttpFilter::RequestRedirect { .. } => {
                // Handled above, before backend selection — unreachable here
                unreachable!("redirect filters are handled before backend selection");
            }
            HttpFilter::RequestHeaderModifier { add, set, remove } => {
                request_header_mods = Some(HeaderModifications {
                    add: Arc::clone(add),
                    set: Arc::clone(set),
                    remove: Arc::clone(remove),
                });
            }
            HttpFilter::ResponseHeaderModifier { add, set, remove } => {
                response_header_mods = Some(HeaderModifications {
                    add: Arc::clone(add),
                    set: Arc::clone(set),
                    remove: Arc::clone(remove),
                });
            }
            HttpFilter::URLRewrite { hostname, path } => {
                let rewritten_path = path.as_ref().map(|p| match p {
                    URLRewritePath::ReplaceFullPath(value) => RewrittenPath::Full(value.clone()),
                    URLRewritePath::ReplacePrefixMatch(value) => {
                        RewrittenPath::PrefixReplaced(join_prefix_path(value, &request_info.path[rule.path.len()..]))
                    }
                });
                url_rewrite = Some(URLRewrite {
                    hostname: hostname.clone(),
                    path: rewritten_path,
                });
            }
            HttpFilter::RequestMirror { backend_addr } => {
                mirror_addrs.push(*backend_addr);
            }
        }
    }

    // Apply per-backend filters (e.g. BackendRequestHeaderModifier)
    for filter in &selected_backend.filters {
        match filter {
            HttpFilter::RequestHeaderModifier { add, set, remove } => {
                // Merge with rule-level mods: backend-level is additive
                match &mut request_header_mods {
                    Some(existing) => {
                        let mut merged_add: Vec<_> = (*existing.add).clone();
                        merged_add.extend((*add).iter().cloned());
                        existing.add = Arc::new(merged_add);
                        let mut merged_set: Vec<_> = (*existing.set).clone();
                        merged_set.extend((*set).iter().cloned());
                        existing.set = Arc::new(merged_set);
                        let mut merged_remove: Vec<_> = (*existing.remove).clone();
                        merged_remove.extend((*remove).iter().cloned());
                        existing.remove = Arc::new(merged_remove);
                    }
                    None => {
                        request_header_mods = Some(HeaderModifications {
                            add: Arc::clone(add),
                            set: Arc::clone(set),
                            remove: Arc::clone(remove),
                        });
                    }
                }
            }
            HttpFilter::ResponseHeaderModifier { add, set, remove } => {
                match &mut response_header_mods {
                    Some(existing) => {
                        let mut merged_add: Vec<_> = (*existing.add).clone();
                        merged_add.extend((*add).iter().cloned());
                        existing.add = Arc::new(merged_add);
                        let mut merged_set: Vec<_> = (*existing.set).clone();
                        merged_set.extend((*set).iter().cloned());
                        existing.set = Arc::new(merged_set);
                        let mut merged_remove: Vec<_> = (*existing.remove).clone();
                        merged_remove.extend((*remove).iter().cloned());
                        existing.remove = Arc::new(merged_remove);
                    }
                    None => {
                        response_header_mods = Some(HeaderModifications {
                            add: Arc::clone(add),
                            set: Arc::clone(set),
                            remove: Arc::clone(remove),
                        });
                    }
                }
            }
            _ => {} // Other filter types not valid at backend level
        }
    }

    Ok(ProcessingDecision::HttpForward {
        backend_addr: selected_backend.socket_addr,
        keepalive,
        filters: Some(Box::new(HttpFilterData {
            request_header_mods,
            response_header_mods,
            url_rewrite,
            mirror_addrs,
        })),
        backend_timeout: rule.backend_request_timeout,
        request_timeout: rule.request_timeout,
    })
}

fn build_redirect_location(
    scheme: Option<&str>,
    hostname: &str,
    port: Option<u16>,
    path: &str,
    default_scheme: &str,
    incoming_port: u16,
) -> String {
    let final_scheme = scheme.unwrap_or(default_scheme);
    let scheme_changed = scheme.is_some() && final_scheme != default_scheme;

    // Determine effective port:
    // 1. Explicit port from redirect filter → always use it
    // 2. Scheme changed but no port specified → use new scheme's default (omit)
    // 3. Scheme unchanged and no port specified → preserve incoming port
    let effective_port = match port {
        Some(p) => Some(p),
        None if scheme_changed => None, // use default for new scheme
        None => Some(incoming_port),    // preserve incoming port
    };

    // Omit port if it's the well-known default for the final scheme
    match effective_port {
        Some(p) if !((final_scheme == "http" && p == 80) || (final_scheme == "https" && p == 443)) => {
            format!("{}://{}:{}{}", final_scheme, hostname, p, path)
        }
        _ => format!("{}://{}{}", final_scheme, hostname, path),
    }
}

pub fn analyze_udp_request(
    routes: &RouteTable,
    backend_selector: &BackendSelector,
    server_port: u16,
    health: &HealthRegistry,
) -> Result<ProcessingDecision> {
    analyze_l4_request(
        backend_selector, server_port, "UDP",
        |port| routes.find_udp_backends(port),
        |addr| ProcessingDecision::UdpForward { backend_addr: addr },
        health,
    )
}

fn analyze_tcp_request(
    routes: &RouteTable,
    backend_selector: &BackendSelector,
    server_port: u16,
    health: &HealthRegistry,
) -> Result<ProcessingDecision> {
    analyze_l4_request(
        backend_selector, server_port, "TCP",
        |port| routes.find_tcp_backends(port),
        |addr| ProcessingDecision::TcpForward { backend_addr: addr },
        health,
    )
}

fn analyze_l4_request<'a>(
    backend_selector: &BackendSelector,
    server_port: u16,
    proto: &str,
    lookup: impl FnOnce(u16) -> Result<&'a Vec<crate::routing::Backend>>,
    make_decision: impl FnOnce(SocketAddr) -> ProcessingDecision,
    health: &HealthRegistry,
) -> Result<ProcessingDecision> {
    let backend_list = match lookup(server_port) {
        Ok(list) => list,
        Err(_) => {
            error!("{} routing failed for port {}: no route configured", proto, server_port);
            return Ok(ProcessingDecision::CloseConnection);
        }
    };

    if backend_list.is_empty() {
        error!("{} routing failed for port {}: no backends available", proto, server_port);
        return Ok(ProcessingDecision::CloseConnection);
    }

    let route_hash = server_port as u64;
    let n = backend_list.len();
    // Use round-robin as a base index, then scan forward for the first healthy backend.
    let base = backend_selector.select_backend(route_hash, n);

    let selected = (0..n)
        .map(|offset| (base + offset) % n)
        .find_map(|i| {
            let b = &backend_list[i];
            if health.is_healthy(&b.socket_addr) { Some(b) } else { None }
        });

    let selected_backend = match selected {
        Some(b) => b,
        None => {
            error!("{} routing: all backends unhealthy for port {}", proto, server_port);
            return Ok(ProcessingDecision::CloseConnection);
        }
    };

    debug!("{} route found: port {} -> backend {}",
           proto, server_port, selected_backend.socket_addr);

    Ok(make_decision(selected_backend.socket_addr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::HealthRegistry;
    use crate::routing::{RouteTable, HttpRouteRule, PathMatchType, Backend, HttpFilter, URLRewritePath, HttpHeader};

    fn build_route_table(
        host: &str,
        path: &str,
        path_type: PathMatchType,
        backend_port: u16,
        filters: Vec<HttpFilter>,
    ) -> RouteTable {
        let mut rt = RouteTable::new();
        rt.add_http_route(host, HttpRouteRule::new(
            path_type,
            path.to_string(),
            vec![],
            vec![],
            filters,
            vec![Backend { socket_addr: format!("127.0.0.1:{}", backend_port).parse().unwrap(), weight: 1, filters: vec![] }],
        ));
        rt
    }

    fn make_request(method: &str, path: &str, host: &str) -> Vec<u8> {
        format!("{} {} HTTP/1.1\r\nHost: {}\r\n\r\n", method, path, host).into_bytes()
    }

    fn health() -> HealthRegistry {
        HealthRegistry::new()
    }

    #[test]
    fn test_url_rewrite_full_path() {
        let rt = build_route_table(
            "example.com", "/", PathMatchType::Prefix, 8001,
            vec![HttpFilter::URLRewrite {
                hostname: None,
                path: Some(URLRewritePath::ReplaceFullPath("/new".to_string())),
            }],
        );
        let mut sel = BackendSelector::new();
        let req = make_request("GET", "/old", "example.com");
        let decision = analyze_request(&rt, &mut sel, &req, 8080, &health(), false).unwrap();

        if let ProcessingDecision::HttpForward { filters, .. } = decision {
            let fd = filters.unwrap();
            let rewrite = fd.url_rewrite.unwrap();
            assert!(matches!(rewrite.path, Some(RewrittenPath::Full(ref p)) if p == "/new"));
        } else {
            panic!("expected HttpForward");
        }
    }

    #[test]
    fn test_url_rewrite_prefix_match() {
        let rt = build_route_table(
            "example.com", "/v1", PathMatchType::Prefix, 8001,
            vec![HttpFilter::URLRewrite {
                hostname: None,
                path: Some(URLRewritePath::ReplacePrefixMatch("/v2".to_string())),
            }],
        );
        let mut sel = BackendSelector::new();
        let req = make_request("GET", "/v1/users", "example.com");
        let decision = analyze_request(&rt, &mut sel, &req, 8080, &health(), false).unwrap();

        if let ProcessingDecision::HttpForward { filters, .. } = decision {
            let fd = filters.unwrap();
            let rewrite = fd.url_rewrite.unwrap();
            assert!(matches!(rewrite.path, Some(RewrittenPath::PrefixReplaced(ref p)) if p == "/v2/users"));
        } else {
            panic!("expected HttpForward");
        }
    }

    #[test]
    fn test_url_rewrite_prefix_match_exact_prefix() {
        let rt = build_route_table(
            "example.com", "/v1", PathMatchType::Prefix, 8001,
            vec![HttpFilter::URLRewrite {
                hostname: None,
                path: Some(URLRewritePath::ReplacePrefixMatch("/v2".to_string())),
            }],
        );
        let mut sel = BackendSelector::new();
        let req = make_request("GET", "/v1", "example.com");
        let decision = analyze_request(&rt, &mut sel, &req, 8080, &health(), false).unwrap();

        if let ProcessingDecision::HttpForward { filters, .. } = decision {
            let fd = filters.unwrap();
            let rewrite = fd.url_rewrite.unwrap();
            assert!(matches!(rewrite.path, Some(RewrittenPath::PrefixReplaced(ref p)) if p == "/v2"));
        } else {
            panic!("expected HttpForward");
        }
    }

    #[test]
    fn test_url_rewrite_prefix_match_root() {
        let rt = build_route_table(
            "example.com", "/", PathMatchType::Prefix, 8001,
            vec![HttpFilter::URLRewrite {
                hostname: None,
                path: Some(URLRewritePath::ReplacePrefixMatch("/api".to_string())),
            }],
        );
        let mut sel = BackendSelector::new();
        let req = make_request("GET", "/users", "example.com");
        let decision = analyze_request(&rt, &mut sel, &req, 8080, &health(), false).unwrap();

        if let ProcessingDecision::HttpForward { filters, .. } = decision {
            let fd = filters.unwrap();
            let rewrite = fd.url_rewrite.unwrap();
            assert!(matches!(rewrite.path, Some(RewrittenPath::PrefixReplaced(ref p)) if p == "/api/users"),
                "expected /api/users, got {:?}", rewrite.path);
        } else {
            panic!("expected HttpForward");
        }
    }

    #[test]
    fn test_url_rewrite_hostname() {
        let rt = build_route_table(
            "example.com", "/", PathMatchType::Prefix, 8001,
            vec![HttpFilter::URLRewrite {
                hostname: Some("new.example.com".to_string()),
                path: None,
            }],
        );
        let mut sel = BackendSelector::new();
        let req = make_request("GET", "/", "example.com");
        let decision = analyze_request(&rt, &mut sel, &req, 8080, &health(), false).unwrap();

        if let ProcessingDecision::HttpForward { filters, .. } = decision {
            let fd = filters.unwrap();
            let rewrite = fd.url_rewrite.unwrap();
            assert_eq!(rewrite.hostname.as_deref(), Some("new.example.com"));
            assert!(rewrite.path.is_none());
        } else {
            panic!("expected HttpForward");
        }
    }

    #[test]
    fn test_url_rewrite_combined_with_header_mods() {
        let rt = build_route_table(
            "example.com", "/v1", PathMatchType::Prefix, 8001,
            vec![
                HttpFilter::URLRewrite {
                    hostname: None,
                    path: Some(URLRewritePath::ReplaceFullPath("/new".to_string())),
                },
                HttpFilter::RequestHeaderModifier {
                    add: Arc::new(vec![HttpHeader { name: "X-Added".to_string(), value: "yes".to_string() }]),
                    set: Arc::new(vec![]),
                    remove: Arc::new(vec![]),
                },
            ],
        );
        let mut sel = BackendSelector::new();
        let req = make_request("GET", "/v1/test", "example.com");
        let decision = analyze_request(&rt, &mut sel, &req, 8080, &health(), false).unwrap();

        if let ProcessingDecision::HttpForward { filters, .. } = decision {
            let fd = filters.unwrap();
            assert!(fd.url_rewrite.is_some());
            assert!(fd.request_header_mods.is_some());
        } else {
            panic!("expected HttpForward");
        }
    }

    #[test]
    fn test_request_mirror_single() {
        let rt = build_route_table(
            "example.com", "/", PathMatchType::Prefix, 8001,
            vec![HttpFilter::RequestMirror {
                backend_addr: "127.0.0.1:9999".parse().unwrap(),
            }],
        );
        let mut sel = BackendSelector::new();
        let req = make_request("GET", "/", "example.com");
        let decision = analyze_request(&rt, &mut sel, &req, 8080, &health(), false).unwrap();

        if let ProcessingDecision::HttpForward { filters, .. } = decision {
            let fd = filters.unwrap();
            assert_eq!(fd.mirror_addrs.len(), 1);
            assert_eq!(fd.mirror_addrs[0], "127.0.0.1:9999".parse().unwrap());
        } else {
            panic!("expected HttpForward");
        }
    }

    #[test]
    fn test_request_mirror_multiple() {
        let rt = build_route_table(
            "example.com", "/", PathMatchType::Prefix, 8001,
            vec![
                HttpFilter::RequestMirror { backend_addr: "127.0.0.1:9998".parse().unwrap() },
                HttpFilter::RequestMirror { backend_addr: "127.0.0.1:9999".parse().unwrap() },
            ],
        );
        let mut sel = BackendSelector::new();
        let req = make_request("GET", "/", "example.com");
        let decision = analyze_request(&rt, &mut sel, &req, 8080, &health(), false).unwrap();

        if let ProcessingDecision::HttpForward { filters, .. } = decision {
            let fd = filters.unwrap();
            assert_eq!(fd.mirror_addrs.len(), 2);
        } else {
            panic!("expected HttpForward");
        }
    }

    #[test]
    fn test_redirect_takes_priority_over_backends() {
        let mut rt = RouteTable::new();
        rt.add_http_route("example.com", HttpRouteRule::new(
            PathMatchType::Prefix, "/".to_string(), vec![], vec![],
            vec![HttpFilter::RequestRedirect {
                scheme: Some("https".to_string()),
                hostname: None,
                port: None,
                path: None,
                status_code: 301,
            }],
            vec![Backend { socket_addr: "127.0.0.1:8001".parse().unwrap(), weight: 1, filters: vec![] }],
        ));
        let mut sel = BackendSelector::new();
        let req = make_request("GET", "/", "example.com");
        let decision = analyze_request(&rt, &mut sel, &req, 8080, &health(), false).unwrap();

        assert!(matches!(decision, ProcessingDecision::HttpRedirect { status_code: 301, .. }));
    }

    #[test]
    fn test_no_filters_no_overhead() {
        let rt = build_route_table("example.com", "/", PathMatchType::Prefix, 8001, vec![]);
        let mut sel = BackendSelector::new();
        let req = make_request("GET", "/", "example.com");
        let decision = analyze_request(&rt, &mut sel, &req, 8080, &health(), false).unwrap();

        if let ProcessingDecision::HttpForward { filters, .. } = decision {
            assert!(filters.is_none());
        } else {
            panic!("expected HttpForward");
        }
    }
}
