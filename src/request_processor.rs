//! Request Processing Pipeline
//!
//! Analyzes incoming data and produces routing decisions.
//! Works on byte slices — no I/O runtime dependency.

use anyhow::Result;
use std::net::SocketAddr;
use crate::logging::{debug, error};
use crate::http_parser::{ConnectionType, extract_routing_info};
use crate::routing::{RouteTable, HttpFilter, URLRewritePath, BackendSelector, HttpHeader};

#[derive(Debug, Clone)]
pub struct HeaderModifications {
    pub add: Vec<HttpHeader>,
    pub set: Vec<HttpHeader>,
    pub remove: Vec<String>,
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
    backend_selector: &mut BackendSelector,
    request_data: &[u8],
    server_port: u16,
) -> Result<ProcessingDecision> {
    if request_data.is_empty() {
        return Ok(ProcessingDecision::CloseConnection);
    }

    if is_http_request(request_data) {
        analyze_http_request(routes, backend_selector, request_data)
    } else {
        analyze_tcp_request(routes, backend_selector, server_port)
    }
}

#[inline(always)]
fn is_http_request(data: &[u8]) -> bool {
    if data.len() < 3 { return false; }

    data.starts_with(b"GET ") ||
    data.starts_with(b"POST ") ||
    data.starts_with(b"PUT ") ||
    data.starts_with(b"DELETE ") ||
    data.starts_with(b"HEAD ") ||
    data.starts_with(b"OPTIONS ") ||
    data.starts_with(b"PATCH ")
}

#[inline(always)]
fn compute_route_hash(path: &str) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 14695981039346656037;
    const FNV_PRIME: u64 = 1099511628211;
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in path.as_bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn analyze_http_request(
    routes: &RouteTable,
    backend_selector: &mut BackendSelector,
    request_data: &[u8],
) -> Result<ProcessingDecision> {
    let request_info = extract_routing_info(request_data)?;
    let keepalive = request_info.connection_type != ConnectionType::Close;

    let rule = match routes.find_http_route(request_info.host, request_info.method, request_info.path, request_info.header_data, request_info.query_string) {
        Ok(rule) => rule,
        Err(_) => {
            return Ok(ProcessingDecision::SendHttpError {
                error_code: 404,
                close_connection: !keepalive,
            });
        }
    };

    if rule.backends.is_empty() {
        return Ok(ProcessingDecision::SendHttpError {
            error_code: 503,
            close_connection: !keepalive,
        });
    }

    let route_hash = compute_route_hash(request_info.path);
    let backend_index = backend_selector.select_weighted_backend(route_hash, rule);
    let selected_backend = &rule.backends[backend_index];

    // Fast path: no filters — no Box allocation, enum stays 40 bytes
    if !rule.has_filters {
        return Ok(ProcessingDecision::HttpForward {
            backend_addr: selected_backend.socket_addr,
            keepalive,
            filters: None,
        });
    }

    // Slow path: process filters, box the result
    let mut request_header_mods: Option<HeaderModifications> = None;
    let mut response_header_mods: Option<HeaderModifications> = None;
    let mut url_rewrite: Option<URLRewrite> = None;
    let mut mirror_addrs: Vec<SocketAddr> = Vec::new();

    for filter in &rule.filters {
        match filter {
            HttpFilter::RequestRedirect { scheme, hostname, port, path, status_code } => {
                let location = build_redirect_location(
                    scheme.as_deref(),
                    hostname.as_deref().unwrap_or(request_info.host),
                    *port,
                    path.as_deref().unwrap_or(request_info.path),
                    request_info.host,
                );
                return Ok(ProcessingDecision::HttpRedirect {
                    status_code: *status_code,
                    location,
                    keepalive,
                });
            }
            HttpFilter::RequestHeaderModifier { add, set, remove } => {
                request_header_mods = Some(HeaderModifications {
                    add: add.clone(),
                    set: set.clone(),
                    remove: remove.clone(),
                });
            }
            HttpFilter::ResponseHeaderModifier { add, set, remove } => {
                response_header_mods = Some(HeaderModifications {
                    add: add.clone(),
                    set: set.clone(),
                    remove: remove.clone(),
                });
            }
            HttpFilter::URLRewrite { hostname, path } => {
                let rewritten_path = path.as_ref().map(|p| match p {
                    URLRewritePath::ReplaceFullPath(value) => RewrittenPath::Full(value.clone()),
                    URLRewritePath::ReplacePrefixMatch(value) => {
                        let suffix = &request_info.path[rule.path.len()..];
                        let needs_slash = !value.ends_with('/') && !suffix.starts_with('/') && !suffix.is_empty();
                        let result = if needs_slash {
                            format!("{}/{}", value, suffix)
                        } else {
                            format!("{}{}", value, suffix)
                        };
                        RewrittenPath::PrefixReplaced(result)
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

    Ok(ProcessingDecision::HttpForward {
        backend_addr: selected_backend.socket_addr,
        keepalive,
        filters: Some(Box::new(HttpFilterData {
            request_header_mods,
            response_header_mods,
            url_rewrite,
            mirror_addrs,
        })),
    })
}

fn build_redirect_location(
    scheme: Option<&str>,
    hostname: &str,
    port: Option<u16>,
    path: &str,
    original_host: &str,
) -> String {
    let scheme = scheme.unwrap_or("http");
    let host = if hostname == original_host { original_host } else { hostname };
    match port {
        Some(p) => format!("{}://{}:{}{}", scheme, host, p, path),
        None => format!("{}://{}{}", scheme, host, path),
    }
}

pub fn analyze_udp_request(
    routes: &RouteTable,
    backend_selector: &mut BackendSelector,
    server_port: u16,
) -> Result<ProcessingDecision> {
    analyze_l4_request(
        backend_selector, server_port, "UDP",
        |port| routes.find_udp_backends(port),
        |addr| ProcessingDecision::UdpForward { backend_addr: addr },
    )
}

fn analyze_tcp_request(
    routes: &RouteTable,
    backend_selector: &mut BackendSelector,
    server_port: u16,
) -> Result<ProcessingDecision> {
    analyze_l4_request(
        backend_selector, server_port, "TCP",
        |port| routes.find_tcp_backends(port),
        |addr| ProcessingDecision::TcpForward { backend_addr: addr },
    )
}

fn analyze_l4_request<'a>(
    backend_selector: &mut BackendSelector,
    server_port: u16,
    proto: &str,
    lookup: impl FnOnce(u16) -> Result<&'a Vec<crate::routing::Backend>>,
    make_decision: impl FnOnce(SocketAddr) -> ProcessingDecision,
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
    let backend_index = backend_selector.select_backend(route_hash, backend_list.len());
    let selected_backend = &backend_list[backend_index];

    debug!("{} route found: port {} -> backend {}",
           proto, server_port, selected_backend.socket_addr);

    Ok(make_decision(selected_backend.socket_addr))
}

#[cfg(test)]
mod tests {
    use super::*;
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
            vec![Backend { socket_addr: format!("127.0.0.1:{}", backend_port).parse().unwrap(), weight: 1 }],
        ));
        rt
    }

    fn make_request(method: &str, path: &str, host: &str) -> Vec<u8> {
        format!("{} {} HTTP/1.1\r\nHost: {}\r\n\r\n", method, path, host).into_bytes()
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
        let decision = analyze_request(&rt, &mut sel, &req, 8080).unwrap();

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
        let decision = analyze_request(&rt, &mut sel, &req, 8080).unwrap();

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
        let decision = analyze_request(&rt, &mut sel, &req, 8080).unwrap();

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
        let decision = analyze_request(&rt, &mut sel, &req, 8080).unwrap();

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
        let decision = analyze_request(&rt, &mut sel, &req, 8080).unwrap();

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
                    add: vec![HttpHeader { name: "X-Added".to_string(), value: "yes".to_string() }],
                    set: vec![],
                    remove: vec![],
                },
            ],
        );
        let mut sel = BackendSelector::new();
        let req = make_request("GET", "/v1/test", "example.com");
        let decision = analyze_request(&rt, &mut sel, &req, 8080).unwrap();

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
        let decision = analyze_request(&rt, &mut sel, &req, 8080).unwrap();

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
        let decision = analyze_request(&rt, &mut sel, &req, 8080).unwrap();

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
            vec![Backend { socket_addr: "127.0.0.1:8001".parse().unwrap(), weight: 1 }],
        ));
        let mut sel = BackendSelector::new();
        let req = make_request("GET", "/", "example.com");
        let decision = analyze_request(&rt, &mut sel, &req, 8080).unwrap();

        assert!(matches!(decision, ProcessingDecision::HttpRedirect { status_code: 301, .. }));
    }

    #[test]
    fn test_no_filters_no_overhead() {
        let rt = build_route_table("example.com", "/", PathMatchType::Prefix, 8001, vec![]);
        let mut sel = BackendSelector::new();
        let req = make_request("GET", "/", "example.com");
        let decision = analyze_request(&rt, &mut sel, &req, 8080).unwrap();

        if let ProcessingDecision::HttpForward { filters, .. } = decision {
            assert!(filters.is_none());
        } else {
            panic!("expected HttpForward");
        }
    }
}
