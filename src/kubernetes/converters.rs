//! Gateway API type → internal config type converters.
//!
//! Pure functions that convert kube-rs generated Gateway API structs
//! into portail's internal config types. No I/O, no K8s API calls.

use anyhow::{anyhow, Result};

use gateway_api::experimental::tcproutes::*;
use gateway_api::experimental::tlsroutes::*;
use gateway_api::experimental::udproutes::*;
use gateway_api::gateways::{Gateway, GatewayListeners, GatewayListenersTlsMode};
use gateway_api::httproutes::*;

use crate::config::*;
use crate::logging::warn;

use super::parent_ref::L4BackendRefAccess;

// ---------------------------------------------------------------------------
// Type alias for TLS certificate data
// ---------------------------------------------------------------------------

/// TLS certificate data keyed by (secret_name, namespace) → (cert_pem, key_pem).
pub(crate) type CertData = std::collections::HashMap<(String, String), (Vec<u8>, Vec<u8>)>;

// ---------------------------------------------------------------------------
// Gateway + Listener conversion
// ---------------------------------------------------------------------------

pub(crate) fn convert_gateway(gw: &Gateway, cert_data: &CertData) -> Result<GatewayConfig> {
    let name = gw
        .metadata
        .name
        .clone()
        .unwrap_or_else(|| "portail-gateway".to_string());

    let gw_ns = gw.metadata.namespace.as_deref().unwrap_or("default");

    // A Network value is a network name; the controller swaps in this pod's
    // IP on that network before anything binds.
    let addresses: Vec<String> = gw
        .spec
        .addresses
        .as_ref()
        .map(|addrs| {
            addrs
                .iter()
                .filter_map(|a| match a.r#type.as_deref().unwrap_or("IPAddress") {
                    "IPAddress" => a.value.clone(),
                    t if t == super::addresses::NETWORK_ADDRESS_TYPE => a.value.clone(),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();

    // One bad listener must not take down its siblings: skip it here and let
    // validate_gateway report it (Accepted=False/UnsupportedProtocol) while
    // the rest of the Gateway keeps serving.
    let listeners = gw
        .spec
        .listeners
        .iter()
        .filter_map(|l| match convert_listener(l, gw_ns, cert_data) {
            Ok(lc) => Some(lc),
            Err(e) => {
                warn!("Skipping listener '{}': {}", l.name, e);
                None
            }
        })
        .collect();

    Ok(GatewayConfig {
        name,
        listeners,
        addresses,
    })
}

fn convert_listener(
    l: &GatewayListeners,
    gw_ns: &str,
    cert_data: &CertData,
) -> Result<ListenerConfig> {
    let protocol = match l.protocol.as_str() {
        "HTTP" => Protocol::HTTP,
        "HTTPS" => Protocol::HTTPS,
        "TLS" => Protocol::TLS,
        "TCP" => Protocol::TCP,
        "UDP" => Protocol::UDP,
        other => return Err(anyhow!("unsupported listener protocol: {}", other)),
    };
    if !super::reconciler::listener_is_served(l) {
        return Err(anyhow!("TLS listener not in Passthrough mode"));
    }

    let tls = l.tls.as_ref().map(|t| {
        let mode = match t.mode {
            Some(GatewayListenersTlsMode::Passthrough) => TlsMode::Passthrough,
            _ => TlsMode::Terminate,
        };
        let certificate_refs = t
            .certificate_refs
            .as_ref()
            .map(|refs| {
                refs.iter()
                    .map(|r| {
                        let secret_ns = r.namespace.as_deref().unwrap_or(gw_ns);
                        let key = (r.name.clone(), secret_ns.to_string());
                        let (cert_pem, key_pem) = cert_data
                            .get(&key)
                            .map(|(c, k)| (Some(c.clone()), Some(k.clone())))
                            .unwrap_or((None, None));
                        CertificateRef {
                            name: r.name.clone(),
                            namespace: Some(secret_ns.to_string()),
                            hostname: l.hostname.clone(),
                            cert_pem,
                            key_pem,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        TlsConfig {
            mode,
            certificate_refs,
        }
    });

    Ok(ListenerConfig {
        name: l.name.clone(),
        protocol,
        port: l.port as u16,
        target_port: None,
        hostname: l.hostname.clone(),
        address: None,
        interface: None,
        tls,
        // Populated later from RateLimitPolicy resources targeting this
        // Gateway (reconcile_to_config), never from the Gateway spec.
        rate_limit: None,
    })
}

// ---------------------------------------------------------------------------
// HTTP route conversion
// ---------------------------------------------------------------------------

/// `Err` names the first part the proxy cannot represent: a filter type it
/// does not support, or one missing its config. That part is left out and
/// the rest converted, so the config keeps every backendRef for the
/// ResolvedRefs check. The route itself is not served.
pub(crate) fn convert_http_route(route: &HTTPRoute) -> (HttpRouteConfig, Result<(), RouteError>) {
    let ns = route_namespace(&route.metadata);
    let mut first = None;
    let rules = route
        .spec
        .rules
        .iter()
        .flatten()
        .enumerate()
        .map(|(i, r)| {
            let (rule, err) = convert_http_rule(r, ns);
            if let Some(e) = err {
                first.get_or_insert(e.at(format_args!("rule {}", i)));
            }
            rule
        })
        .collect();

    let config = HttpRouteConfig {
        parent_refs: Vec::new(),
        hostnames: route.spec.hostnames.clone().unwrap_or_default(),
        rules,
    };
    (config, first.map_or(Ok(()), Err))
}

fn convert_http_rule(rule: &HTTPRouteRules, ns: &str) -> (HttpRouteRule, Option<RouteError>) {
    let matches = rule
        .matches
        .as_ref()
        .map(|m| m.iter().map(convert_http_match).collect::<Vec<_>>())
        .unwrap_or_default();

    // A filter this proxy cannot represent fails the route, never dropped
    // while the route serves without it.
    let mut first = None;
    let filters: Vec<HttpRouteFilter> = rule
        .filters
        .iter()
        .flatten()
        .enumerate()
        .filter_map(|(i, f)| match convert_http_filter(f, ns) {
            Ok(filter) => Some(filter),
            Err(e) => {
                first.get_or_insert(e.at(format_args!("filter {}", i)));
                None
            }
        })
        .collect();

    // The CRD refuses backendRefs beside a redirect; a cluster that skips
    // CEL can send them. Unused, they are left out: never checked for
    // ResolvedRefs, resolved or probed.
    let redirects = filters
        .iter()
        .any(|f| matches!(f, HttpRouteFilter::RequestRedirect { .. }));
    let backend_refs = rule
        .backend_refs
        .iter()
        .flatten()
        .filter(|_| !redirects)
        .enumerate()
        .map(|(i, br)| {
            let (backend, err) = convert_http_backend_ref(br, ns);
            if let Some(e) = err {
                first.get_or_insert(e.at(format_args!("backendRef {}", i)));
            }
            backend
        })
        .collect();

    let timeouts = rule.timeouts.as_ref().map(convert_timeouts);

    let rule = HttpRouteRule {
        matches,
        filters,
        backend_refs,
        timeouts,
    };
    (rule, first)
}

fn convert_http_match(m: &HTTPRouteRulesMatches) -> HttpRouteMatch {
    let method = m.method.as_ref().map(|m| {
        match m {
            HTTPRouteRulesMatchesMethod::Get => "GET",
            HTTPRouteRulesMatchesMethod::Head => "HEAD",
            HTTPRouteRulesMatchesMethod::Post => "POST",
            HTTPRouteRulesMatchesMethod::Put => "PUT",
            HTTPRouteRulesMatchesMethod::Delete => "DELETE",
            HTTPRouteRulesMatchesMethod::Connect => "CONNECT",
            HTTPRouteRulesMatchesMethod::Options => "OPTIONS",
            HTTPRouteRulesMatchesMethod::Trace => "TRACE",
            HTTPRouteRulesMatchesMethod::Patch => "PATCH",
        }
        .to_string()
    });

    let path = m.path.as_ref().map(|p| {
        let match_type = match p.r#type {
            Some(HTTPRouteRulesMatchesPathType::Exact) => HttpPathMatchType::Exact,
            Some(HTTPRouteRulesMatchesPathType::RegularExpression) => {
                HttpPathMatchType::RegularExpression
            }
            _ => HttpPathMatchType::PathPrefix,
        };
        HttpPathMatch {
            match_type,
            value: p.value.clone().unwrap_or_else(|| "/".to_string()),
        }
    });

    let headers = m
        .headers
        .as_ref()
        .map(|h| h.iter().map(convert_header_match).collect())
        .unwrap_or_default();

    let query_params = m
        .query_params
        .as_ref()
        .map(|q| q.iter().map(convert_query_param_match).collect())
        .unwrap_or_default();

    HttpRouteMatch {
        method,
        path,
        headers,
        query_params,
    }
}

fn convert_header_match(h: &HTTPRouteRulesMatchesHeaders) -> HttpHeaderMatch {
    let match_type = match h.r#type {
        Some(HTTPRouteRulesMatchesHeadersType::RegularExpression) => {
            StringMatchType::RegularExpression
        }
        _ => StringMatchType::Exact,
    };
    HttpHeaderMatch {
        name: h.name.clone(),
        value: h.value.clone(),
        match_type,
    }
}

fn convert_query_param_match(q: &HTTPRouteRulesMatchesQueryParams) -> HttpQueryParamMatch {
    let match_type = match q.r#type {
        Some(HTTPRouteRulesMatchesQueryParamsType::RegularExpression) => {
            StringMatchType::RegularExpression
        }
        _ => StringMatchType::Exact,
    };
    HttpQueryParamMatch {
        name: q.name.clone(),
        value: q.value.clone(),
        match_type,
    }
}

// ---------------------------------------------------------------------------
// HTTP filters
// ---------------------------------------------------------------------------

/// Convert any K8s header modifier struct with add/set/remove fields into HeaderModifierConfig.
macro_rules! convert_header_mod {
    ($hm:expr) => {
        HeaderModifierConfig {
            add: $hm
                .add
                .as_ref()
                .map(|v| {
                    v.iter()
                        .map(|h| HttpHeader {
                            name: h.name.clone(),
                            value: h.value.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            set: $hm
                .set
                .as_ref()
                .map(|v| {
                    v.iter()
                        .map(|h| HttpHeader {
                            name: h.name.clone(),
                            value: h.value.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            remove: $hm.remove.clone().unwrap_or_default(),
        }
    };
}

/// Gateway API: a filter type the implementation does not support sets
/// Accepted=False with reason UnsupportedValue.
fn convert_http_filter(f: &HTTPRouteRulesFilters, ns: &str) -> Result<HttpRouteFilter, RouteError> {
    let missing = |what: &str| RouteError::unsupported(format!("{} filter missing config", what));
    Ok(match f.r#type {
        HTTPRouteRulesFiltersType::RequestHeaderModifier => {
            let hm = f
                .request_header_modifier
                .as_ref()
                .ok_or_else(|| missing("RequestHeaderModifier"))?;
            HttpRouteFilter::RequestHeaderModifier {
                config: convert_header_mod!(hm),
            }
        }
        HTTPRouteRulesFiltersType::ResponseHeaderModifier => {
            let hm = f
                .response_header_modifier
                .as_ref()
                .ok_or_else(|| missing("ResponseHeaderModifier"))?;
            HttpRouteFilter::ResponseHeaderModifier {
                config: convert_header_mod!(hm),
            }
        }
        HTTPRouteRulesFiltersType::RequestRedirect => {
            let rr = f
                .request_redirect
                .as_ref()
                .ok_or_else(|| missing("RequestRedirect"))?;
            HttpRouteFilter::RequestRedirect {
                config: convert_redirect_config(rr),
            }
        }
        HTTPRouteRulesFiltersType::UrlRewrite => {
            let ur = f
                .url_rewrite
                .as_ref()
                .ok_or_else(|| missing("URLRewrite"))?;
            HttpRouteFilter::URLRewrite {
                config: convert_url_rewrite_config(ur),
            }
        }
        HTTPRouteRulesFiltersType::RequestMirror => {
            let rm = f
                .request_mirror
                .as_ref()
                .ok_or_else(|| missing("RequestMirror"))?;
            HttpRouteFilter::RequestMirror {
                config: convert_mirror_config(rm, ns),
            }
        }
        ref other => {
            return Err(RouteError::unsupported(format!(
                "unsupported filter type {:?}",
                other
            )))
        }
    })
}

fn convert_redirect_config(rr: &HTTPRouteRulesFiltersRequestRedirect) -> RequestRedirectConfig {
    let scheme = rr.scheme.as_ref().map(|s| match s {
        HTTPRouteRulesFiltersRequestRedirectScheme::Http => "http".to_string(),
        HTTPRouteRulesFiltersRequestRedirectScheme::Https => "https".to_string(),
    });

    let path = rr.path.as_ref().and_then(convert_path_modifier);

    RequestRedirectConfig {
        scheme,
        hostname: rr.hostname.clone(),
        port: rr.port.map(|p| p as u16),
        path,
        status_code: rr.status_code.map(|c| c as u16).unwrap_or(302),
    }
}

fn convert_path_modifier(
    p: &HTTPRouteRulesFiltersRequestRedirectPath,
) -> Option<HttpURLRewritePath> {
    match p.r#type {
        HTTPRouteRulesFiltersRequestRedirectPathType::ReplaceFullPath => p
            .replace_full_path
            .as_ref()
            .map(|v| HttpURLRewritePath::ReplaceFullPath { value: v.clone() }),
        HTTPRouteRulesFiltersRequestRedirectPathType::ReplacePrefixMatch => p
            .replace_prefix_match
            .as_ref()
            .map(|v| HttpURLRewritePath::ReplacePrefixMatch { value: v.clone() }),
    }
}

fn convert_url_rewrite_config(ur: &HTTPRouteRulesFiltersUrlRewrite) -> URLRewriteConfig {
    let path = ur.path.as_ref().and_then(|p| match p.r#type {
        HTTPRouteRulesFiltersUrlRewritePathType::ReplaceFullPath => p
            .replace_full_path
            .as_ref()
            .map(|v| HttpURLRewritePath::ReplaceFullPath { value: v.clone() }),
        HTTPRouteRulesFiltersUrlRewritePathType::ReplacePrefixMatch => p
            .replace_prefix_match
            .as_ref()
            .map(|v| HttpURLRewritePath::ReplacePrefixMatch { value: v.clone() }),
    });

    URLRewriteConfig {
        hostname: ur.hostname.clone(),
        path,
    }
}

fn convert_mirror_config(rm: &HTTPRouteRulesFiltersRequestMirror, ns: &str) -> RequestMirrorConfig {
    let fraction = rm.fraction.as_ref().map(|f| crate::config::MirrorFraction {
        numerator: f.numerator.max(0) as u32,
        denominator: f.denominator.map(|d| d.max(1) as u32).unwrap_or(100),
    });
    let percent = rm.percent.map(|p| p.clamp(0, 100) as u32);
    RequestMirrorConfig {
        backend_ref: BackendRef {
            name: backend_dns_name(
                &rm.backend_ref.name,
                rm.backend_ref.namespace.as_deref(),
                ns,
            ),
            port: rm.backend_ref.port.unwrap_or(80) as u16,
            weight: 1,
            group: rm.backend_ref.group.clone().unwrap_or_default(),
            kind: rm
                .backend_ref
                .kind
                .clone()
                .unwrap_or_else(|| "Service".to_string()),
            filters: vec![],
            app_protocol: None,
            invalid: false,
        },
        percent,
        fraction,
    }
}

// ---------------------------------------------------------------------------
// HTTP backend refs
// ---------------------------------------------------------------------------

/// Like `convert_http_rule`: a filter that cannot be represented is left
/// out and named.
fn convert_http_backend_ref(
    br: &HTTPRouteRulesBackendRefs,
    ns: &str,
) -> (BackendRef, Option<RouteError>) {
    let mut first = None;
    let filters = br
        .filters
        .iter()
        .flatten()
        .enumerate()
        .filter_map(|(i, f)| match convert_backend_ref_filter(f) {
            Ok(filter) => Some(filter),
            Err(e) => {
                first.get_or_insert(e.at(format_args!("filter {}", i)));
                None
            }
        })
        .collect();
    let backend = BackendRef {
        name: backend_dns_name(&br.name, br.namespace.as_deref(), ns),
        port: br.port.unwrap_or(80) as u16,
        weight: br.weight.unwrap_or(1) as u32,
        group: br.group.clone().unwrap_or_default(),
        kind: br.kind.clone().unwrap_or_else(|| "Service".to_string()),
        filters,
        app_protocol: None,
        invalid: false,
    };
    (backend, first)
}

/// Per-backend filters (e.g. BackendRequestHeaderModifier). The data plane
/// applies only header modifiers per backend: any other type fails the
/// route like an unsupported rule filter, instead of serving without it.
fn convert_backend_ref_filter(
    f: &HTTPRouteRulesBackendRefsFilters,
) -> Result<HttpRouteFilter, RouteError> {
    let missing = |what: &str| RouteError::unsupported(format!("{} filter missing config", what));
    Ok(match f.r#type {
        HTTPRouteRulesBackendRefsFiltersType::RequestHeaderModifier => {
            let hm = f
                .request_header_modifier
                .as_ref()
                .ok_or_else(|| missing("RequestHeaderModifier"))?;
            HttpRouteFilter::RequestHeaderModifier {
                config: convert_header_mod!(hm),
            }
        }
        HTTPRouteRulesBackendRefsFiltersType::ResponseHeaderModifier => {
            let hm = f
                .response_header_modifier
                .as_ref()
                .ok_or_else(|| missing("ResponseHeaderModifier"))?;
            HttpRouteFilter::ResponseHeaderModifier {
                config: convert_header_mod!(hm),
            }
        }
        ref other => {
            return Err(RouteError::unsupported(format!(
                "{:?} is not supported on a backendRef",
                other
            )))
        }
    })
}

// ---------------------------------------------------------------------------
// Timeouts
// ---------------------------------------------------------------------------

fn convert_timeouts(t: &HTTPRouteRulesTimeouts) -> HttpRouteTimeouts {
    HttpRouteTimeouts {
        request: t
            .request
            .as_ref()
            .and_then(|s| crate::config::parsing::parse_duration(s).ok()),
        backend_request: t
            .backend_request
            .as_ref()
            .and_then(|s| crate::config::parsing::parse_duration(s).ok()),
    }
}

// ---------------------------------------------------------------------------
// L4 route conversion (TCP, TLS, UDP)
// ---------------------------------------------------------------------------

/// Shared conversion for L4 backend refs (TCP/TLS/UDP all have the same fields).
/// `default_port`: protocol-implied default (TCP=80, TLS=443) or `None` if port is required (UDP).
pub(crate) fn convert_l4_backend_ref<B: L4BackendRefAccess>(
    br: &B,
    ns: &str,
    protocol: &str,
    default_port: Option<i32>,
) -> BackendRef {
    let port = match br.br_port() {
        Some(p) => p as u16,
        None => match default_port {
            Some(dp) => {
                warn!(
                    "{} backend ref '{}' missing port, defaulting to {}",
                    protocol,
                    br.br_name(),
                    dp
                );
                dp as u16
            }
            None => {
                warn!(
                    "{} backend ref '{}' missing required port, using 0",
                    protocol,
                    br.br_name()
                );
                0
            }
        },
    };
    BackendRef {
        name: backend_dns_name(br.br_name(), br.br_namespace(), ns),
        port,
        weight: br.br_weight().unwrap_or(1) as u32,
        group: br.br_group().unwrap_or_default().to_string(),
        kind: br
            .br_kind()
            .map(String::from)
            .unwrap_or_else(|| "Service".to_string()),
        filters: vec![],
        app_protocol: None,
        invalid: false,
    }
}

/// Convert a slice of L4 backend refs (TCP/TLS/UDP) to internal `BackendRef`s.
fn convert_l4_backend_refs<B: L4BackendRefAccess>(
    refs: &[B],
    ns: &str,
    proto: &str,
    default_port: Option<i32>,
) -> Vec<BackendRef> {
    refs.iter()
        .map(|br| convert_l4_backend_ref(br, ns, proto, default_port))
        .collect()
}

pub(crate) fn convert_tcp_route(route: &TCPRoute) -> (TcpRouteConfig, Result<(), RouteError>) {
    let ns = route_namespace(&route.metadata);
    let rules = route
        .spec
        .rules
        .iter()
        .map(|r| TcpRouteRule {
            backend_refs: convert_l4_backend_refs(&r.backend_refs, ns, "TCP", Some(80)),
        })
        .collect();
    let config = TcpRouteConfig {
        parent_refs: Vec::new(),
        rules,
    };
    (config, Ok(()))
}

pub(crate) fn convert_tls_route(route: &TLSRoute) -> (TlsRouteConfig, Result<(), RouteError>) {
    let ns = route_namespace(&route.metadata);
    let hostnames = route.spec.hostnames.clone();
    let rules = route
        .spec
        .rules
        .iter()
        .map(|r| TlsRouteRule {
            backend_refs: convert_l4_backend_refs(&r.backend_refs, ns, "TLS", Some(443)),
        })
        .collect();
    let config = TlsRouteConfig {
        parent_refs: Vec::new(),
        hostnames,
        rules,
    };
    (config, Ok(()))
}

pub(crate) fn convert_udp_route(route: &UDPRoute) -> (UdpRouteConfig, Result<(), RouteError>) {
    let ns = route_namespace(&route.metadata);
    let rules = route
        .spec
        .rules
        .iter()
        .map(|r| UdpRouteRule {
            backend_refs: convert_l4_backend_refs(&r.backend_refs, ns, "UDP", None),
        })
        .collect();
    let config = UdpRouteConfig {
        parent_refs: Vec::new(),
        rules,
    };
    (config, Ok(()))
}

// ---------------------------------------------------------------------------
// Utility helpers
// ---------------------------------------------------------------------------

/// Format backend name as `{service}.{namespace}.svc` for DNS resolution.
/// The Service override maps are keyed by this name: build both here.
pub(crate) fn backend_dns_name(
    name: &str,
    namespace: Option<&str>,
    route_namespace: &str,
) -> String {
    let ns = namespace.unwrap_or(route_namespace);
    format!("{}.{}.svc", name, ns)
}

/// Parse a DNS-style backend name back into (service_name, namespace).
pub(crate) fn parse_backend_dns_name(dns_name: &str) -> Option<(String, String)> {
    let stripped = dns_name.strip_suffix(".svc")?;
    let (svc_name, ns) = stripped.rsplit_once('.')?;
    Some((svc_name.to_string(), ns.to_string()))
}

/// Extract namespace from ObjectMeta, defaulting to "default".
pub(crate) fn route_namespace(meta: &kube::core::ObjectMeta) -> &str {
    meta.namespace.as_deref().unwrap_or("default")
}
