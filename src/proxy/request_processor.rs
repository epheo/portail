//! Request Processing Pipeline
//!
//! Analyzes incoming data and produces routing decisions.
//! Orchestrates http_parser + routing + health into a RoutingResult.
//! Works on byte slices — no I/O runtime dependency.

use anyhow::Result;
use std::borrow::Cow;
use std::net::SocketAddr;

use crate::logging::{debug, error};
use crate::metrics::METRICS;
use crate::proxy::backend_pool::LogThrottle;
use crate::proxy::health::HealthRegistry;
use crate::proxy::http_parser::{extract_routing_info, normalize_path, HttpRequestInfo};
use crate::proxy::method::{is_idempotent, method_len};
use crate::routing::{
    Backend, BackendAction, BackendSelector, HttpFilter, HttpRouteRule, RouteTable, URLRewritePath,
};

/// Stack-allocated request metadata: only a normalized path is on the heap.
pub struct RequestMeta {
    pub content_length: Option<usize>,
    pub is_chunked: bool,
    pub is_upgrade: bool,
    /// True when the request method is HEAD. The response must be treated as
    /// having no body regardless of `Content-Length`/`Transfer-Encoding` on the
    /// response — otherwise `forward_response_body` would block reading bytes
    /// the backend (correctly) never sends.
    pub is_head: bool,
    /// See [`crate::proxy::method::is_idempotent`].
    pub idempotent: bool,
    pub keepalive: bool,
    /// Byte span of the client's request path (query stripped) within the
    /// request buffer, so filters need not re-parse the request line. `None`
    /// when the parser substituted a static default ("/").
    pub path_span: Option<(u32, u32)>,
    /// The path routing matched, when `normalize_path` changed the
    /// client's. `RequestMeta::path` returns whichever applies.
    pub normalized_path: Option<String>,
    /// Hop-by-hop header spans recorded by the parser, skipped at forwarding.
    pub strip_spans: crate::proxy::http_parser::StripSpans,
    /// Upgrade header line span; kept only when `is_upgrade`.
    pub upgrade_span: Option<(u32, u32)>,
    /// Request carried `Expect: 100-continue`.
    pub expect_continue: bool,
    /// Whether the connection may stay open if the proxy answers this
    /// request itself (429); see `local_reply_reusable`.
    pub local_reply_reusable: bool,
    /// [`client_done`] for a reply the proxy writes itself (429), which
    /// reads nothing past what came with the head.
    pub client_done: bool,
}

impl RequestMeta {
    /// The path routing matched, the one the backend must get. `head` is
    /// the request buffer `path_span` indexes.
    pub fn path<'a>(&'a self, head: &'a [u8]) -> &'a str {
        if let Some(path) = &self.normalized_path {
            return path;
        }
        self.path_span
            .and_then(|(s, e)| head.get(s as usize..e as usize))
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("/")
    }
}

/// A response the proxy writes itself instead of forwarding the request.
pub struct LocalReply {
    pub status: u16,
    /// Redirect target; `None` for an error status.
    pub location: Option<String>,
    /// Whether the connection stays open afterwards; see `local_reply_reusable`.
    pub reusable: bool,
    /// The client sends nothing more (see [`client_done`]): a reply that
    /// closes has nothing to read off first.
    pub client_done: bool,
    /// Answering HEAD: the reply must not carry its body, or a client on a
    /// kept-open connection would read it as the next response.
    pub head: bool,
}

/// Routing result that borrows from the RouteTable — zero per-request allocations.
pub enum RoutingResult<'a> {
    HttpForward {
        backend: &'a Backend,
        rule: &'a HttpRouteRule,
        meta: RequestMeta,
    },
    /// Redirect or error status, answered without a backend.
    LocalReply(LocalReply),
    TcpForward {
        backend_addr: SocketAddr,
    },
    UdpForward {
        backend_addr: SocketAddr,
    },
    CloseConnection,
}

/// Whether the head is the whole request, which is all a reply the proxy
/// writes itself reads: no body, and no upgrade, whose client may already
/// be streaming the new protocol.
fn head_is_whole(info: &HttpRequestInfo<'_>) -> bool {
    !info.is_upgrade && !info.is_chunked && info.content_length.unwrap_or(0) == 0
}

/// Whether the connection may stay open after the proxy answers a request
/// itself instead of forwarding it: only past a whole request, as unread
/// body bytes would be parsed as the next request.
fn local_reply_reusable(info: &HttpRequestInfo<'_>) -> bool {
    info.keepalive && head_is_whole(info)
}

/// What a client has sent on its connection, for [`client_done`].
pub struct ClientSent {
    /// Its request asked to keep the connection open.
    pub keepalive: bool,
    /// All of its request is in. Never so for an upgrade: its client may
    /// already stream the new protocol.
    pub request_in: bool,
    /// Bytes past its request were read.
    pub seen_past: bool,
}

/// Whether the client sends nothing more on its connection, the one rule
/// behind every closing reply's linger. It must have asked to close
/// (`Connection: close`, or HTTP/1.0 without keep-alive), its request must
/// be all in, and no byte past it seen: a client sending past its request
/// may send more, whatever it asked.
pub fn client_done(sent: ClientSent) -> bool {
    !sent.keepalive && sent.request_in && !sent.seen_past
}

pub fn analyze_request<'a>(
    routes: &'a RouteTable,
    backend_selector: &BackendSelector,
    request_data: &[u8],
    server_port: u16,
    health: &HealthRegistry,
    is_tls: bool,
) -> Result<RoutingResult<'a>> {
    if request_data.is_empty() {
        return Ok(RoutingResult::CloseConnection);
    }

    if is_http_request(request_data) {
        analyze_http_request(
            routes,
            backend_selector,
            request_data,
            health,
            is_tls,
            server_port,
        )
    } else {
        analyze_tcp_request(routes, backend_selector, server_port, health)
    }
}

/// Whether `data` opens like an HTTP/1.x request line. This is the sniff
/// that splits HTTP from opaque TCP on a shared listener, so it runs on
/// every request and must never read another protocol's greeting as HTTP.
#[inline(always)]
pub fn is_http_request(data: &[u8]) -> bool {
    if data.len() < 4 {
        return false;
    }

    const GET: u32 = u32::from_ne_bytes(*b"GET ");
    const POST: u32 = u32::from_ne_bytes(*b"POST");
    const PUT: u32 = u32::from_ne_bytes(*b"PUT ");
    const DELE: u32 = u32::from_ne_bytes(*b"DELE"); // DELETE
    const HEAD: u32 = u32::from_ne_bytes(*b"HEAD");
    const OPTI: u32 = u32::from_ne_bytes(*b"OPTI"); // OPTIONS
    const PATC: u32 = u32::from_ne_bytes(*b"PATC"); // PATCH

    // Seven methods carry nearly all traffic and one u32 compare settles
    // them (3-letter tags include the SP). This path is unchanged.
    let tag = u32::from_ne_bytes([data[0], data[1], data[2], data[3]]);
    if matches!(tag, GET | POST | PUT | DELE | HEAD | OPTI | PATC) {
        return true;
    }

    // CONNECT, TRACE, WebDAV and CalDAV (PROPFIND, REPORT, MKCALENDAR...)
    // and extension methods take the bounded token scan. Through v0.1.21
    // they fell to the TCP path, which has no route on an HTTP listener,
    // and the connection closed without a response: every CalDAV client
    // behind portail broke on its first request.
    method_len(data).is_some()
}

fn analyze_http_request<'a>(
    routes: &'a RouteTable,
    backend_selector: &BackendSelector,
    request_data: &[u8],
    health: &HealthRegistry,
    is_tls: bool,
    server_port: u16,
) -> Result<RoutingResult<'a>> {
    // A head the parser cannot represent, or flags as ambiguous (framing
    // conflicts such as TE+CL, malformed lines), is refused rather than
    // interpreted: the ambiguity itself is the smuggling surface (haproxy /
    // envoy / nginx agree). Its framing is unknown, so the connection closes.
    const BAD_REQUEST: RoutingResult<'static> = RoutingResult::LocalReply(LocalReply {
        status: 400,
        location: None,
        reusable: false,
        client_done: false,
        head: false,
    });
    // Counted, logged at debug only: the client picks how many it sends.
    let request_info = match extract_routing_info(request_data) {
        Ok(info) if !info.header_violation => info,
        Ok(_) => {
            METRICS.http_requests_rejected_total.inc();
            debug!("Rejecting malformed request head on port {}", server_port);
            return Ok(BAD_REQUEST);
        }
        Err(_e) => {
            METRICS.http_requests_rejected_total.inc();
            debug!(
                "Rejecting unparseable request on port {}: {}",
                server_port, _e
            );
            return Ok(BAD_REQUEST);
        }
    };
    let reusable = local_reply_reusable(&request_info);
    // A reply the proxy writes itself reads no body.
    let client_done = client_done(ClientSent {
        keepalive: request_info.keepalive,
        request_in: head_is_whole(&request_info),
        seen_past: request_data.len() > request_info.head_len,
    });
    let is_head = request_info.method.eq_ignore_ascii_case("HEAD");
    let reply = |status, location| {
        Ok(RoutingResult::LocalReply(LocalReply {
            status,
            location,
            reusable,
            client_done,
            head: is_head,
        }))
    };

    let Some(path) = normalize_path(request_info.path) else {
        METRICS.http_requests_rejected_total.inc();
        debug!(
            "Rejecting a dot-segment with path parameters on port {}",
            server_port
        );
        return Ok(BAD_REQUEST);
    };
    let Some(rule) = routes.find_http_route(
        request_info.host,
        request_info.method,
        &path,
        request_info.header_data,
        request_info.query_string,
        server_port,
    ) else {
        return reply(404, None);
    };

    // Check for redirect filters FIRST — redirect rules often have no backends,
    // so we must handle them before the empty-backends check.
    if rule.has_filters {
        if let Some((status_code, location)) = build_redirect(
            rule,
            request_info.host,
            &path,
            request_info.query_string,
            is_tls,
            server_port,
        ) {
            return reply(status_code, Some(location));
        }
    }

    if rule.backends.is_empty() {
        return reply(500, None);
    }

    let backend_index = match backend_selector.select_healthy_weighted_backend(rule, health) {
        Some(idx) => idx,
        None => return reply(503, None),
    };
    let selected_backend = &rule.backends[backend_index];
    if selected_backend.action == BackendAction::Reject {
        return reply(500, None);
    }

    Ok(RoutingResult::HttpForward {
        backend: selected_backend,
        rule,
        meta: RequestMeta {
            content_length: request_info.content_length,
            is_chunked: request_info.is_chunked,
            is_upgrade: request_info.is_upgrade,
            is_head,
            idempotent: is_idempotent(request_info.method),
            keepalive: request_info.keepalive,
            path_span: subslice_span(request_data, request_info.path),
            normalized_path: match path {
                Cow::Owned(path) => Some(path),
                Cow::Borrowed(_) => None,
            },
            strip_spans: request_info.strip_spans,
            upgrade_span: request_info.upgrade_span,
            expect_continue: request_info.expect_continue,
            local_reply_reusable: reusable,
            client_done,
        },
    })
}

/// Resolve the rule's first RequestRedirect filter against the request:
/// `(status_code, Location)`. Sibling of `extract_url_rewrite` on the
/// forward path; `None` when the rule carries no redirect.
fn build_redirect(
    rule: &HttpRouteRule,
    request_host: &str,
    request_path: &str,
    query: &str,
    is_tls: bool,
    server_port: u16,
) -> Option<(u16, String)> {
    for filter in &rule.filters {
        if let HttpFilter::RequestRedirect {
            scheme,
            hostname,
            port,
            path,
            status_code,
        } = filter
        {
            let (redirect_path, value_query) = match path {
                Some(URLRewritePath::ReplaceFullPath(value)) => {
                    (Cow::Borrowed(value.path.as_str()), value.query.as_deref())
                }
                Some(URLRewritePath::ReplacePrefixMatch(value)) => (
                    Cow::Owned(crate::proxy::http_filters::join_prefix_path(
                        &value.path,
                        crate::proxy::http_filters::prefix_remainder(&rule.path, request_path),
                    )),
                    value.query.as_deref(),
                ),
                None => (Cow::Borrowed(request_path), None),
            };
            let default_scheme = if is_tls { "https" } else { "http" };
            let location = build_redirect_location(
                scheme.as_deref(),
                hostname.as_deref().unwrap_or(request_host),
                *port,
                (&redirect_path, value_query),
                query,
                default_scheme,
                server_port,
            );
            return Some((*status_code, location));
        }
    }
    None
}

/// Byte span of `sub` within `base`, or `None` when `sub` is not a subslice
/// (the parser falls back to static defaults like "/" for malformed lines).
/// Pointer compares only — no deref, so this is safe for any pair.
fn subslice_span(base: &[u8], sub: &str) -> Option<(u32, u32)> {
    let base_start = base.as_ptr() as usize;
    let sub_start = sub.as_ptr() as usize;
    if sub_start >= base_start && sub_start + sub.len() <= base_start + base.len() {
        let off = sub_start - base_start;
        Some((off as u32, (off + sub.len()) as u32))
    } else {
        None
    }
}

/// The redirect's path replaces the request's path only (HTTPPathModifier):
/// a non-empty `query` follows it, after the query its value carries if
/// any (`path.1`).
fn build_redirect_location(
    scheme: Option<&str>,
    hostname: &str,
    port: Option<u16>,
    (path, value_query): (&str, Option<&str>),
    query: &str,
    default_scheme: &str,
    incoming_port: u16,
) -> String {
    let final_scheme = scheme.unwrap_or(default_scheme);
    let (query_mark, value_query, glue) = match value_query {
        None if query.is_empty() => ("", "", ""),
        None => ("?", "", ""),
        Some(value_query) if query.is_empty() => ("?", value_query, ""),
        Some(value_query) => (
            "?",
            value_query,
            crate::proxy::http_filters::query_glue(value_query),
        ),
    };
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
        Some(p)
            if !((final_scheme == "http" && p == 80) || (final_scheme == "https" && p == 443)) =>
        {
            format!("{final_scheme}://{hostname}:{p}{path}{query_mark}{value_query}{glue}{query}")
        }
        _ => format!("{final_scheme}://{hostname}{path}{query_mark}{value_query}{glue}{query}"),
    }
}

pub fn analyze_udp_request(
    routes: &RouteTable,
    backend_selector: &BackendSelector,
    server_port: u16,
    health: &HealthRegistry,
) -> Result<RoutingResult<'static>> {
    static UNROUTABLE: L4Throttles = L4Throttles::new();
    analyze_l4_request(
        backend_selector,
        server_port,
        "UDP",
        &UNROUTABLE,
        |port| routes.find_udp_backends(port),
        |addr| RoutingResult::UdpForward { backend_addr: addr },
        health,
    )
}

pub(crate) fn analyze_tcp_request(
    routes: &RouteTable,
    backend_selector: &BackendSelector,
    server_port: u16,
    health: &HealthRegistry,
) -> Result<RoutingResult<'static>> {
    static UNROUTABLE: L4Throttles = L4Throttles::new();
    analyze_l4_request(
        backend_selector,
        server_port,
        "TCP",
        &UNROUTABLE,
        |port| routes.find_tcp_backends(port),
        |addr| RoutingResult::TcpForward { backend_addr: addr },
        health,
    )
}

/// One log throttle per unroutable condition, for one protocol. A line
/// names the port it met; the count it carries covers every port.
struct L4Throttles {
    no_route: LogThrottle,
    no_backends: LogThrottle,
    no_weight: LogThrottle,
}

impl L4Throttles {
    const fn new() -> Self {
        Self {
            no_route: LogThrottle::new(),
            no_backends: LogThrottle::new(),
            no_weight: LogThrottle::new(),
        }
    }
}

fn analyze_l4_request<'a>(
    backend_selector: &BackendSelector,
    server_port: u16,
    proto: &str,
    throttles: &L4Throttles,
    lookup: impl FnOnce(u16) -> Option<&'a Vec<Backend>>,
    make_decision: impl FnOnce(SocketAddr) -> RoutingResult<'static>,
    health: &HealthRegistry,
) -> Result<RoutingResult<'static>> {
    let unroutable = |throttle: &LogThrottle, condition: &str| {
        if let Some(suppressed) = throttle.admit_now() {
            error!(
                "{} routing failed for port {}: {}; {} more since the last such line, on any port",
                proto, server_port, condition, suppressed
            );
        }
        Ok(RoutingResult::CloseConnection)
    };

    let Some(backend_list) = lookup(server_port) else {
        return unroutable(&throttles.no_route, "no route configured");
    };
    if backend_list.is_empty() {
        return unroutable(&throttles.no_backends, "no backends available");
    }

    // Weighted round-robin among healthy backends, honoring per-backendRef
    // weights (weight 0 = no traffic, per Gateway API semantics).
    let Some(selected_backend) =
        backend_selector.select_l4_backend(server_port as u64, backend_list, health)
    else {
        return unroutable(&throttles.no_weight, "no backend with positive weight");
    };
    if selected_backend.action == BackendAction::Reject {
        debug!(
            "{} routing: port {} picked an invalid backendRef's share; rejecting",
            proto, server_port
        );
        return Ok(RoutingResult::CloseConnection);
    }

    debug!(
        "{} route found: port {} -> backend {}",
        proto, server_port, selected_backend.socket_addr
    );

    Ok(make_decision(selected_backend.socket_addr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::health::HealthRegistry;
    use crate::routing::{
        Backend, BackendSelector, Fraction, HttpFilter, HttpHeader, HttpRouteRule, PathMatchType,
        RouteTable, URLRewritePath,
    };
    use std::sync::Arc;

    fn build_route_table(
        host: &str,
        path: &str,
        path_type: PathMatchType,
        backend_port: u16,
        filters: Vec<HttpFilter>,
    ) -> RouteTable {
        let mut rt = RouteTable::new();
        rt.add_http_route(
            host,
            HttpRouteRule::new(
                path_type,
                path.to_string(),
                vec![],
                vec![],
                filters,
                vec![Backend {
                    socket_addr: format!("127.0.0.1:{}", backend_port).parse().unwrap(),
                    weight: 1,
                    filters: vec![],
                    use_tls: false,
                    server_name: String::new(),
                    action: BackendAction::Forward,
                }],
            ),
        );
        rt
    }

    fn make_request(method: &str, path: &str, host: &str) -> Vec<u8> {
        format!("{} {} HTTP/1.1\r\nHost: {}\r\n\r\n", method, path, host).into_bytes()
    }

    fn health() -> HealthRegistry {
        HealthRegistry::new()
    }

    #[test]
    fn test_redirect_prefix_replace_on_regex_rule_no_panic() {
        // Regression: the redirect arm sliced the request path by pattern
        // length; a pattern longer than the matched path aborted the process.
        let pattern = r"^/a$|^/much-longer-alternative$";
        let mut rule = HttpRouteRule::new(
            PathMatchType::RegularExpression,
            pattern.to_string(),
            vec![],
            vec![],
            vec![HttpFilter::RequestRedirect {
                scheme: None,
                hostname: None,
                port: None,
                path: Some(URLRewritePath::ReplacePrefixMatch("/new".into())),
                status_code: 302,
            }],
            vec![Backend {
                socket_addr: "127.0.0.1:8001".parse().unwrap(),
                weight: 1,
                filters: vec![],
                use_tls: false,
                server_name: String::new(),
                action: BackendAction::Forward,
            }],
        );
        rule.path_regex = Some(regex::Regex::new(pattern).unwrap());
        let mut rt = RouteTable::new();
        rt.add_http_route("example.com", rule);

        let sel = BackendSelector::new();
        let req = make_request("GET", "/a", "example.com");
        let result = analyze_request(&rt, &sel, &req, 8080, &health(), false).unwrap();

        match result {
            RoutingResult::LocalReply(LocalReply {
                location: Some(location),
                ..
            }) => {
                assert!(location.ends_with("/new"), "location: {}", location);
            }
            _ => panic!("expected a redirect"),
        }
    }

    #[test]
    fn test_url_rewrite_full_path() {
        let rt = build_route_table(
            "example.com",
            "/",
            PathMatchType::Prefix,
            8001,
            vec![HttpFilter::URLRewrite {
                hostname: None,
                path: Some(URLRewritePath::ReplaceFullPath("/new".into())),
            }],
        );
        let sel = BackendSelector::new();
        let req = make_request("GET", "/old", "example.com");
        let result = analyze_request(&rt, &sel, &req, 8080, &health(), false).unwrap();

        if let RoutingResult::HttpForward { rule, .. } = result {
            let rewrite =
                crate::proxy::http_filters::extract_url_rewrite(&rule.filters, &rule.path, "/old")
                    .unwrap();
            assert!(
                matches!(rewrite.path, Some(crate::proxy::http_filters::RewrittenPath { path: ref p, .. }) if p == "/new")
            );
        } else {
            panic!("expected HttpForward");
        }
    }

    #[test]
    fn test_url_rewrite_prefix_match() {
        let rt = build_route_table(
            "example.com",
            "/v1",
            PathMatchType::Prefix,
            8001,
            vec![HttpFilter::URLRewrite {
                hostname: None,
                path: Some(URLRewritePath::ReplacePrefixMatch("/v2".into())),
            }],
        );
        let sel = BackendSelector::new();
        let req = make_request("GET", "/v1/users", "example.com");
        let result = analyze_request(&rt, &sel, &req, 8080, &health(), false).unwrap();

        if let RoutingResult::HttpForward { rule, .. } = result {
            let rewrite = crate::proxy::http_filters::extract_url_rewrite(
                &rule.filters,
                &rule.path,
                "/v1/users",
            )
            .unwrap();
            assert!(
                matches!(rewrite.path, Some(crate::proxy::http_filters::RewrittenPath { path: ref p, .. }) if p == "/v2/users")
            );
        } else {
            panic!("expected HttpForward");
        }
    }

    #[test]
    fn test_url_rewrite_prefix_match_exact_prefix() {
        let rt = build_route_table(
            "example.com",
            "/v1",
            PathMatchType::Prefix,
            8001,
            vec![HttpFilter::URLRewrite {
                hostname: None,
                path: Some(URLRewritePath::ReplacePrefixMatch("/v2".into())),
            }],
        );
        let sel = BackendSelector::new();
        let req = make_request("GET", "/v1", "example.com");
        let result = analyze_request(&rt, &sel, &req, 8080, &health(), false).unwrap();

        if let RoutingResult::HttpForward { rule, .. } = result {
            let rewrite =
                crate::proxy::http_filters::extract_url_rewrite(&rule.filters, &rule.path, "/v1")
                    .unwrap();
            assert!(
                matches!(rewrite.path, Some(crate::proxy::http_filters::RewrittenPath { path: ref p, .. }) if p == "/v2")
            );
        } else {
            panic!("expected HttpForward");
        }
    }

    #[test]
    fn test_url_rewrite_prefix_match_root() {
        let rt = build_route_table(
            "example.com",
            "/",
            PathMatchType::Prefix,
            8001,
            vec![HttpFilter::URLRewrite {
                hostname: None,
                path: Some(URLRewritePath::ReplacePrefixMatch("/api".into())),
            }],
        );
        let sel = BackendSelector::new();
        let req = make_request("GET", "/users", "example.com");
        let result = analyze_request(&rt, &sel, &req, 8080, &health(), false).unwrap();

        if let RoutingResult::HttpForward { rule, .. } = result {
            let rewrite = crate::proxy::http_filters::extract_url_rewrite(
                &rule.filters,
                &rule.path,
                "/users",
            )
            .unwrap();
            assert!(
                matches!(rewrite.path, Some(crate::proxy::http_filters::RewrittenPath { path: ref p, .. }) if p == "/api/users"),
                "expected /api/users, got {:?}",
                rewrite.path
            );
        } else {
            panic!("expected HttpForward");
        }
    }

    #[test]
    fn test_url_rewrite_hostname() {
        let rt = build_route_table(
            "example.com",
            "/",
            PathMatchType::Prefix,
            8001,
            vec![HttpFilter::URLRewrite {
                hostname: Some("new.example.com".to_string()),
                path: None,
            }],
        );
        let sel = BackendSelector::new();
        let req = make_request("GET", "/", "example.com");
        let result = analyze_request(&rt, &sel, &req, 8080, &health(), false).unwrap();

        if let RoutingResult::HttpForward { rule, .. } = result {
            let rewrite =
                crate::proxy::http_filters::extract_url_rewrite(&rule.filters, &rule.path, "/")
                    .unwrap();
            assert_eq!(rewrite.hostname, Some("new.example.com"));
            assert!(rewrite.path.is_none());
        } else {
            panic!("expected HttpForward");
        }
    }

    #[test]
    fn test_url_rewrite_combined_with_header_mods() {
        let rt = build_route_table(
            "example.com",
            "/v1",
            PathMatchType::Prefix,
            8001,
            vec![
                HttpFilter::URLRewrite {
                    hostname: None,
                    path: Some(URLRewritePath::ReplaceFullPath("/new".into())),
                },
                HttpFilter::RequestHeaderModifier {
                    add: Arc::new(vec![HttpHeader {
                        name: "X-Added".to_string(),
                        value: "yes".to_string(),
                    }]),
                    set: Arc::new(vec![]),
                    remove: Arc::new(vec![]),
                },
            ],
        );
        let sel = BackendSelector::new();
        let req = make_request("GET", "/v1/test", "example.com");
        let result = analyze_request(&rt, &sel, &req, 8080, &health(), false).unwrap();

        if let RoutingResult::HttpForward { rule, .. } = result {
            assert!(crate::proxy::http_filters::extract_url_rewrite(
                &rule.filters,
                &rule.path,
                "/v1/test"
            )
            .is_some());
            assert!(
                crate::proxy::http_filters::extract_header_mods(&rule.filters, false).is_some()
            );
        } else {
            panic!("expected HttpForward");
        }
    }

    #[test]
    fn test_request_mirror_single() {
        let rt = build_route_table(
            "example.com",
            "/",
            PathMatchType::Prefix,
            8001,
            vec![HttpFilter::RequestMirror {
                backend_addr: "127.0.0.1:9999".parse().unwrap(),
                fraction: Fraction::ALL,
            }],
        );
        let sel = BackendSelector::new();
        let req = make_request("GET", "/", "example.com");
        let result = analyze_request(&rt, &sel, &req, 8080, &health(), false).unwrap();

        if let RoutingResult::HttpForward { rule, .. } = result {
            let mirrors: Vec<_> = rule
                .filters
                .iter()
                .filter_map(|f| match f {
                    HttpFilter::RequestMirror {
                        backend_addr,
                        fraction,
                    } => Some((*backend_addr, *fraction)),
                    _ => None,
                })
                .collect();
            assert_eq!(mirrors.len(), 1);
            assert_eq!(mirrors[0].0, "127.0.0.1:9999".parse().unwrap());
            assert_eq!(mirrors[0].1, Fraction::ALL);
        } else {
            panic!("expected HttpForward");
        }
    }

    #[test]
    fn test_request_mirror_multiple() {
        let rt = build_route_table(
            "example.com",
            "/",
            PathMatchType::Prefix,
            8001,
            vec![
                HttpFilter::RequestMirror {
                    backend_addr: "127.0.0.1:9998".parse().unwrap(),
                    fraction: Fraction::new(1, 2).unwrap(),
                },
                HttpFilter::RequestMirror {
                    backend_addr: "127.0.0.1:9999".parse().unwrap(),
                    fraction: Fraction::ALL,
                },
            ],
        );
        let sel = BackendSelector::new();
        let req = make_request("GET", "/", "example.com");
        let result = analyze_request(&rt, &sel, &req, 8080, &health(), false).unwrap();

        if let RoutingResult::HttpForward { rule, .. } = result {
            let mirrors: Vec<_> = rule
                .filters
                .iter()
                .filter_map(|f| match f {
                    HttpFilter::RequestMirror {
                        backend_addr,
                        fraction,
                    } => Some((*backend_addr, *fraction)),
                    _ => None,
                })
                .collect();
            assert_eq!(mirrors.len(), 2);
            assert_eq!(mirrors[0].1, Fraction::new(1, 2).unwrap());
            assert_eq!(mirrors[1].1, Fraction::ALL);
        } else {
            panic!("expected HttpForward");
        }
    }

    #[test]
    fn test_redirect_takes_priority_over_backends() {
        let mut rt = RouteTable::new();
        rt.add_http_route(
            "example.com",
            HttpRouteRule::new(
                PathMatchType::Prefix,
                "/".to_string(),
                vec![],
                vec![],
                vec![HttpFilter::RequestRedirect {
                    scheme: Some("https".to_string()),
                    hostname: None,
                    port: None,
                    path: None,
                    status_code: 301,
                }],
                vec![Backend {
                    socket_addr: "127.0.0.1:8001".parse().unwrap(),
                    weight: 1,
                    filters: vec![],
                    use_tls: false,
                    server_name: String::new(),
                    action: BackendAction::Forward,
                }],
            ),
        );
        let sel = BackendSelector::new();
        let req = make_request("GET", "/", "example.com");
        let result = analyze_request(&rt, &sel, &req, 8080, &health(), false).unwrap();

        assert!(matches!(
            result,
            RoutingResult::LocalReply(LocalReply {
                status: 301,
                location: Some(_),
                ..
            })
        ));
    }

    /// A redirect path replaces the request's path only (HTTPPathModifier):
    /// the query follows it as sent.
    #[test]
    fn redirect_keeps_the_query() {
        let sel = BackendSelector::new();
        for (path, target, want) in [
            (None, "/old/x?a=1&b", "https://r.test/old/x?a=1&b"),
            (
                Some(URLRewritePath::ReplacePrefixMatch("/new".into())),
                "/old/x?a=1",
                "https://r.test/new/x?a=1",
            ),
            (
                Some(URLRewritePath::ReplaceFullPath("/full".into())),
                "/old/x?a=1",
                "https://r.test/full?a=1",
            ),
            (None, "/old/x", "https://r.test/old/x"),
            // A value's own query comes first, then the request's after a
            // '&'. It made two: /login?next=home?a=1.
            (
                Some(URLRewritePath::ReplaceFullPath("/login?next=home".into())),
                "/old/x?a=1",
                "https://r.test/login?next=home&a=1",
            ),
            (
                Some(URLRewritePath::ReplaceFullPath("/login?next=home".into())),
                "/old/x",
                "https://r.test/login?next=home",
            ),
            (
                Some(URLRewritePath::ReplacePrefixMatch("/new?src=gw".into())),
                "/old/x?a=1",
                "https://r.test/new/x?src=gw&a=1",
            ),
            (
                Some(URLRewritePath::ReplaceFullPath("/login?".into())),
                "/old/x?a=1",
                "https://r.test/login?a=1",
            ),
            // A value's query already ending in '&' takes the request's as
            // is: it made /login?next=home&&a=1.
            (
                Some(URLRewritePath::ReplaceFullPath("/login?next=home&".into())),
                "/old/x?a=1",
                "https://r.test/login?next=home&a=1",
            ),
        ] {
            let redirect = HttpFilter::RequestRedirect {
                scheme: Some("https".to_string()),
                hostname: None,
                port: None,
                path,
                status_code: 302,
            };
            let rt = build_route_table(
                "r.test",
                "/old",
                PathMatchType::Prefix,
                8001,
                vec![redirect],
            );
            let req = make_request("GET", target, "r.test");
            match analyze_request(&rt, &sel, &req, 8080, &health(), false).unwrap() {
                RoutingResult::LocalReply(LocalReply {
                    location: Some(location),
                    ..
                }) => assert_eq!(location, want, "{target}"),
                _ => panic!("expected a redirect for {target}"),
            }
        }
    }

    /// Routing reads the path a backend resolves dot-segments to, so the
    /// rule on `/admin` cannot be walked around.
    #[test]
    fn dot_segments_route_by_the_path_they_resolve_to() {
        let mut rt = build_route_table("example.com", "/", PathMatchType::Prefix, 8001, vec![]);
        rt.add_http_route(
            "example.com",
            HttpRouteRule::new(
                PathMatchType::Prefix,
                "/admin".to_string(),
                vec![],
                vec![],
                vec![],
                vec![Backend {
                    socket_addr: "127.0.0.1:8002".parse().unwrap(),
                    weight: 1,
                    filters: vec![],
                    use_tls: false,
                    server_name: String::new(),
                    action: BackendAction::Forward,
                }],
            ),
        );
        let sel = BackendSelector::new();
        for (target, port, normalized) in [
            ("/public/../admin", 8002, Some("/admin")),
            ("/public/%2e%2E/admin/x?q=1", 8002, Some("/admin/x")),
            ("/admin/../public", 8001, Some("/public")),
            ("/admin/app.min.js", 8002, None),
        ] {
            let req = make_request("GET", target, "example.com");
            match analyze_request(&rt, &sel, &req, 8080, &health(), false).unwrap() {
                RoutingResult::HttpForward { backend, meta, .. } => {
                    assert_eq!(backend.socket_addr.port(), port, "{target}");
                    assert_eq!(meta.normalized_path.as_deref(), normalized, "{target}");
                    assert_eq!(meta.path(&req), normalized.unwrap_or(target), "{target}");
                }
                _ => panic!("expected HttpForward for {target}"),
            }
        }
    }

    /// Tomcat, Jetty and Spring drop a segment's path parameters, then
    /// resolve what is left: `/public/..;/admin`, routed by `/`, was served
    /// `/admin` there. Ordinary path parameters still route.
    #[test]
    fn a_dot_segment_with_path_parameters_gets_a_counted_400() {
        let rt = build_route_table("example.com", "/", PathMatchType::Prefix, 8001, vec![]);
        let rejected = || METRICS.http_requests_rejected_total.get();
        for target in [
            "/public/..;/admin",
            "/public/..;x=1/admin?q",
            "/a/.;/b",
            "/a/%2e%2E;v/b",
            "/a/../b/..;",
            "/.;",
        ] {
            let before = rejected();
            let req = make_request("GET", target, "example.com");
            assert_eq!(local_reply(&rt, &req), (400, false), "{target}");
            assert!(rejected() > before, "{target}");
        }
        let sel = BackendSelector::new();
        for target in [
            "/a;jsessionid=1",
            "/a/..b;x/c",
            "/a/...;x",
            "/a/;../b",
            "/a/x.;y",
        ] {
            let req = make_request("GET", target, "example.com");
            let routed = analyze_request(&rt, &sel, &req, 8080, &health(), false).unwrap();
            assert!(
                matches!(routed, RoutingResult::HttpForward { .. }),
                "{target}"
            );
        }
    }

    #[test]
    fn redirect_takes_the_normalized_path() {
        let redirect = HttpFilter::RequestRedirect {
            scheme: None,
            hostname: None,
            port: None,
            path: Some(URLRewritePath::ReplacePrefixMatch("/new".into())),
            status_code: 301,
        };
        let rt = build_route_table(
            "r.test",
            "/old",
            PathMatchType::Prefix,
            8001,
            vec![redirect],
        );
        let req = make_request("GET", "/x/../old/./a?q", "r.test");
        let sel = BackendSelector::new();
        match analyze_request(&rt, &sel, &req, 8080, &health(), false).unwrap() {
            RoutingResult::LocalReply(LocalReply {
                location: Some(location),
                ..
            }) => assert_eq!(location, "http://r.test:8080/new/a?q"),
            _ => panic!("expected a redirect"),
        }
    }

    #[test]
    fn test_no_filters_no_overhead() {
        let rt = build_route_table("example.com", "/", PathMatchType::Prefix, 8001, vec![]);
        let sel = BackendSelector::new();
        let req = make_request("GET", "/", "example.com");
        let result = analyze_request(&rt, &sel, &req, 8080, &health(), false).unwrap();

        if let RoutingResult::HttpForward { rule, .. } = result {
            assert!(!rule.has_filters);
        } else {
            panic!("expected HttpForward");
        }
    }

    #[test]
    fn sniff_accepts_every_method_the_parser_can_forward() {
        for m in [
            "GET",
            "HEAD",
            "POST",
            "PUT",
            "DELETE",
            "OPTIONS",
            "PATCH",
            "TRACE",
            "PROPFIND",
            "PROPPATCH",
            "MKCOL",
            "MKCALENDAR",
            "REPORT",
            "COPY",
            "MOVE",
            "LOCK",
            "UNLOCK",
            "ACL",
            "SEARCH",
        ] {
            assert!(is_http_request(&make_request(m, "/", "example.com")), "{m}");
        }
    }

    #[test]
    fn sniff_keeps_foreign_protocols_on_the_tcp_path() {
        let tls_client_hello = [0x16u8, 0x03, 0x01, 0x00, 0xf4, 0x01, 0x00, 0x00];
        assert!(!is_http_request(&tls_client_hello));
        assert!(!is_http_request(b"SSH-2.0-OpenSSH_9.9 Ubuntu-3\r\n"));
        assert!(!is_http_request(b"*1\r\n$4\r\nPING\r\n"));
        assert!(!is_http_request(b"get / HTTP/1.1\r\n"));
        assert!(!is_http_request(b"GET"));
    }

    #[test]
    fn webdav_request_is_routed_like_any_http_request() {
        let rt = build_route_table("cal.example.com", "/", PathMatchType::Prefix, 8001, vec![]);
        let sel = BackendSelector::new();
        let req = b"PROPFIND /user/ HTTP/1.1\r\nHost: cal.example.com\r\nDepth: 0\r\nContent-Length: 0\r\n\r\n";
        let result = analyze_request(&rt, &sel, req, 443, &health(), true).unwrap();
        match result {
            RoutingResult::HttpForward { meta, .. } => {
                assert!(!meta.is_head);
                assert!(meta.keepalive);
                assert_eq!(meta.content_length, Some(0));
            }
            _ => panic!("expected HttpForward"),
        }
    }

    /// (status, reusable) of a request the proxy answers itself.
    fn local_reply(rt: &RouteTable, req: &[u8]) -> (u16, bool) {
        let sel = BackendSelector::new();
        match analyze_request(rt, &sel, req, 8080, &health(), false).unwrap() {
            RoutingResult::LocalReply(r) => (r.status, r.reusable),
            _ => panic!("expected a locally answered request"),
        }
    }

    #[test]
    fn local_reply_to_head_is_flagged_bodyless() {
        let rt = build_route_table("example.com", "/", PathMatchType::Prefix, 8001, vec![]);
        let sel = BackendSelector::new();
        let req = b"HEAD / HTTP/1.1\r\nHost: nope.test\r\n\r\n";
        match analyze_request(&rt, &sel, req, 8080, &health(), false).unwrap() {
            RoutingResult::LocalReply(r) => assert!(r.status == 404 && r.head && r.reusable),
            _ => panic!("expected a locally answered request"),
        }
    }

    #[test]
    fn local_reply_keeps_the_connection_only_for_bodyless_keepalive_requests() {
        let rt = build_route_table("example.com", "/", PathMatchType::Prefix, 8001, vec![]);
        let unrouted = |extra: &str| format!("POST / HTTP/1.1\r\nHost: nope.test\r\n{}\r\n", extra);
        assert_eq!(local_reply(&rt, unrouted("").as_bytes()), (404, true));
        assert_eq!(
            local_reply(&rt, unrouted("Content-Length: 0\r\n").as_bytes()),
            (404, true)
        );
        // The reply never reads a body: its bytes would parse as a request.
        for extra in [
            "Content-Length: 44\r\n",
            "Transfer-Encoding: chunked\r\n",
            "Connection: close\r\n",
        ] {
            assert_eq!(
                local_reply(&rt, unrouted(extra).as_bytes()),
                (404, false),
                "{extra}"
            );
        }
        for req in [
            &b"GET /ws HTTP/1.1\r\nHost: nope.test\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n"[..],
            &b"GET / HTTP/1.0\r\nHost: nope.test\r\n\r\n"[..],
        ] {
            assert_eq!(local_reply(&rt, req), (404, false));
        }
    }

    #[test]
    fn redirect_with_a_body_closes_the_connection() {
        let rt = build_route_table(
            "r.test",
            "/",
            PathMatchType::Prefix,
            8001,
            vec![HttpFilter::RequestRedirect {
                scheme: Some("https".to_string()),
                hostname: None,
                port: None,
                path: None,
                status_code: 301,
            }],
        );
        assert_eq!(
            local_reply(&rt, b"GET /x HTTP/1.1\r\nHost: r.test\r\n\r\n"),
            (301, true)
        );
        assert_eq!(
            local_reply(
                &rt,
                b"POST /x HTTP/1.1\r\nHost: r.test\r\nContent-Length: 44\r\n\r\n"
            ),
            (301, false)
        );
    }

    #[test]
    fn unparseable_or_ambiguous_heads_get_a_closing_400() {
        let rt = build_route_table("example.com", "/", PathMatchType::Prefix, 8001, vec![]);
        // HTTP/1.1 without Host, and a TE+CL framing conflict.
        assert_eq!(local_reply(&rt, b"GET / HTTP/1.1\r\n\r\n"), (400, false));
        assert_eq!(
            local_reply(
                &rt,
                b"POST / HTTP/1.1\r\nHost: example.com\r\nContent-Length: 3\r\nTransfer-Encoding: chunked\r\n\r\n"
            ),
            (400, false)
        );
    }

    /// Logged at debug only, so the counter is what shows a client sending
    /// them. Other tests share the global: only growth is asserted.
    #[test]
    fn every_malformed_head_400_is_counted() {
        let rt = build_route_table("example.com", "/", PathMatchType::Prefix, 8001, vec![]);
        let rejected = || METRICS.http_requests_rejected_total.get();
        for req in [
            &b"GET / HTTP/1.1\r\n\r\n"[..],
            b"GET / HTTP/1.1\r\nHost: a.test\r\nHost: b.test\r\n\r\n",
        ] {
            let before = rejected();
            assert_eq!(local_reply(&rt, req), (400, false));
            assert!(rejected() > before);
        }
    }

    /// Browsers send '\' raw in a query; in a path some backends read it
    /// as '/', past the rule routing matched.
    #[test]
    fn backslash_routes_in_the_query_only() {
        let rt = build_route_table("example.com", "/", PathMatchType::Prefix, 8001, vec![]);
        let sel = BackendSelector::new();
        let req = make_request("GET", "/?q=a\\b", "example.com");
        assert!(matches!(
            analyze_request(&rt, &sel, &req, 8080, &health(), false).unwrap(),
            RoutingResult::HttpForward { .. }
        ));
        for target in ["/a\\b", "/a\\b?x"] {
            let req = make_request("GET", target, "example.com");
            assert_eq!(local_reply(&rt, &req), (400, false), "{target}");
        }
    }

    /// Sniffed as HTTP so the client gets an answer instead of a silent
    /// close, and that answer is never a 2xx: nothing here tunnels.
    #[test]
    fn connect_gets_a_closing_400() {
        let rt = build_route_table("example.com", "/", PathMatchType::Prefix, 8001, vec![]);
        assert_eq!(
            local_reply(
                &rt,
                b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n"
            ),
            (400, false)
        );
    }

    #[test]
    fn forwarded_request_carries_the_local_reply_rule_for_429() {
        let rt = build_route_table("example.com", "/", PathMatchType::Prefix, 8001, vec![]);
        let sel = BackendSelector::new();
        for (req, reusable, client_done) in [
            (
                &b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n"[..],
                true,
                false,
            ),
            (
                b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n",
                false,
                true,
            ),
            (
                b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\nGET /",
                false,
                false,
            ),
            (
                b"POST / HTTP/1.1\r\nHost: example.com\r\nContent-Length: 5\r\n\r\n",
                false,
                false,
            ),
        ] {
            match analyze_request(&rt, &sel, req, 8080, &health(), false).unwrap() {
                RoutingResult::HttpForward { meta, .. } => {
                    assert_eq!(meta.local_reply_reusable, reusable);
                    assert_eq!(meta.client_done, client_done);
                }
                _ => panic!("expected HttpForward"),
            }
        }
    }

    /// Only a client that asked to close, with nothing left to send, is
    /// done: a keep-alive client may pipeline its next request, a body or
    /// upgrade tail may follow, bytes already past the head show it still
    /// sending, and a head too malformed to frame tells nothing.
    #[test]
    fn local_reply_knows_whether_the_client_is_done() {
        let rt = build_route_table("example.com", "/", PathMatchType::Prefix, 8001, vec![]);
        let sel = BackendSelector::new();
        let client_done = |req: &[u8]| match analyze_request(&rt, &sel, req, 8080, &health(), false)
        {
            Ok(RoutingResult::LocalReply(r)) => r.client_done,
            _ => panic!("expected a locally answered request"),
        };
        for (req, want) in [
            (
                &b"GET / HTTP/1.1\r\nHost: nope.test\r\nConnection: close\r\n\r\n"[..],
                true,
            ),
            (b"GET / HTTP/1.0\r\nHost: nope.test\r\n\r\n", true),
            (
                b"POST / HTTP/1.1\r\nHost: nope.test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                true,
            ),
            (b"GET / HTTP/1.1\r\nHost: nope.test\r\n\r\n", false),
            (
                b"GET / HTTP/1.0\r\nHost: nope.test\r\nConnection: keep-alive\r\n\r\n",
                false,
            ),
            (
                b"POST / HTTP/1.1\r\nHost: nope.test\r\nContent-Length: 44\r\nConnection: close\r\n\r\n",
                false,
            ),
            (
                b"POST / HTTP/1.0\r\nHost: nope.test\r\nContent-Length: 44\r\n\r\n",
                false,
            ),
            (
                b"GET /ws HTTP/1.1\r\nHost: nope.test\r\nUpgrade: websocket\r\nConnection: Upgrade, close\r\n\r\n",
                false,
            ),
            (b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n", false),
            (
                b"GET / HTTP/1.1\r\nHost: nope.test\r\nConnection: close\r\n\r\nGET / HTTP/1.1\r\n",
                false,
            ),
            (b"GET / HTTP/1.0\r\nHost: nope.test\r\n\r\n\r\n", false),
        ] {
            let text = String::from_utf8_lossy(req);
            assert_eq!(client_done(req), want, "{text}");
        }
    }

    /// A UDP port with no route, or none to forward to, meets it once per
    /// datagram, and a spoofed flood sends those at packet rate: one line
    /// per window names each condition.
    #[test]
    fn an_unroutable_l4_port_logs_once_per_window() {
        use crate::proxy::test_util::{in_own_process, Captured};
        if !in_own_process(module_path!(), "an_unroutable_l4_port_logs_once_per_window") {
            return;
        }
        let (log, _subscriber) = Captured::warnings();
        let mut rt = RouteTable::new();
        rt.add_udp_route(5354, vec![]);
        let mut weightless = Backend::new("127.0.0.1".to_string(), 53).unwrap();
        weightless.weight = 0;
        rt.add_udp_route(5355, vec![weightless]);
        let sel = BackendSelector::new();
        for _ in 0..100 {
            for port in [5353, 5354, 5355] {
                let routed = analyze_udp_request(&rt, &sel, port, &health()).unwrap();
                assert!(matches!(routed, RoutingResult::CloseConnection));
            }
        }
        let log = log.text();
        for condition in [
            "port 5353: no route configured",
            "port 5354: no backends available",
            "port 5355: no backend with positive weight",
        ] {
            assert_eq!(log.matches(condition).count(), 1, "{log}");
        }
    }

    /// TCP and UDP shared one throttle per condition: a TCP miss silenced
    /// the UDP line, and the next line counted both under one port. Each
    /// protocol now has its own, and the count says it covers every port.
    #[test]
    fn l4_log_throttles_are_per_protocol() {
        use crate::proxy::test_util::{in_own_process, Captured};
        if !in_own_process(module_path!(), "l4_log_throttles_are_per_protocol") {
            return;
        }
        let (log, _subscriber) = Captured::warnings();
        let rt = RouteTable::new();
        let sel = BackendSelector::new();
        for port in [5432, 5433, 5434] {
            analyze_tcp_request(&rt, &sel, port, &health()).unwrap();
        }
        analyze_udp_request(&rt, &sel, 5353, &health()).unwrap();
        let log = log.text();
        for line in [
            "TCP routing failed for port 5432: no route configured; 0 more since the last such line, on any port",
            "UDP routing failed for port 5353: no route configured; 0 more since the last such line, on any port",
        ] {
            assert_eq!(log.matches(line).count(), 1, "{log}");
        }
        assert_eq!(log.matches("routing failed").count(), 2, "{log}");
    }

    #[test]
    fn non_http_bytes_on_an_http_only_port_close_the_connection() {
        let rt = build_route_table("example.com", "/", PathMatchType::Prefix, 8001, vec![]);
        let sel = BackendSelector::new();
        let tls_client_hello = [0x16u8, 0x03, 0x01, 0x00, 0xf4, 0x01, 0x00, 0x00];
        let result = analyze_request(&rt, &sel, &tls_client_hello, 8080, &health(), false).unwrap();
        assert!(matches!(result, RoutingResult::CloseConnection));
    }
}
