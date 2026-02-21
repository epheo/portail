//! Gateway API type → internal config type converters.
//!
//! Pure functions that convert kube-rs generated Gateway API structs
//! into portail's internal config types. No I/O, no K8s API calls.

use anyhow::{anyhow, Result};

use gateway_api::gateways::{Gateway, GatewayListeners, GatewayListenersTlsMode};
use gateway_api::httproutes::*;
use gateway_api::experimental::tcproutes::*;
use gateway_api::experimental::tlsroutes::*;
use gateway_api::experimental::udproutes::*;

use crate::config::*;
use crate::logging::warn;

use super::parent_ref::{L4BackendRefAccess, extract_parent_refs};

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
    let listeners = gw
        .spec
        .listeners
        .iter()
        .map(|l| convert_listener(l, gw_ns, cert_data))
        .collect::<Result<Vec<_>>>()?;

    Ok(GatewayConfig {
        name,
        listeners,
        ..Default::default()
    })
}

pub(crate) fn convert_listener(l: &GatewayListeners, gw_ns: &str, cert_data: &CertData) -> Result<ListenerConfig> {
    let protocol = match l.protocol.as_str() {
        "HTTP" => Protocol::HTTP,
        "HTTPS" => Protocol::HTTPS,
        "TLS" => Protocol::TLS,
        "TCP" => Protocol::TCP,
        "UDP" => Protocol::UDP,
        other => return Err(anyhow!("unsupported listener protocol: {}", other)),
    };

    let tls = l.tls.as_ref().map(|t| {
        let mode = match t.mode {
            Some(GatewayListenersTlsMode::Passthrough) => TlsMode::Passthrough,
            _ => TlsMode::Terminate,
        };
        let certificate_refs = t
            .certificate_refs
            .as_ref()
            .map(|refs| refs.iter().map(|r| {
                let secret_ns = r.namespace.as_deref().unwrap_or(gw_ns);
                let key = (r.name.clone(), secret_ns.to_string());
                let (cert_pem, key_pem) = cert_data.get(&key)
                    .map(|(c, k)| (Some(c.clone()), Some(k.clone())))
                    .unwrap_or((None, None));
                CertificateRef { name: r.name.clone(), hostname: l.hostname.clone(), cert_pem, key_pem }
            }).collect())
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
        hostname: l.hostname.clone(),
        address: None,
        interface: None,
        tls,
    })
}

// ---------------------------------------------------------------------------
// HTTP route conversion
// ---------------------------------------------------------------------------

pub(crate) fn convert_http_route(route: &HTTPRoute, gateway_name: &str) -> Result<HttpRouteConfig> {
    let ns = route_namespace(&route.metadata);
    let parent_refs = extract_parent_refs(&route.spec.parent_refs, gateway_name);
    let hostnames = route.spec.hostnames.clone().unwrap_or_default();

    let rules = route
        .spec
        .rules
        .as_ref()
        .map(|rules| rules.iter().map(|r| convert_http_rule(r, ns)).collect::<Result<Vec<_>>>())
        .transpose()?
        .unwrap_or_default();

    Ok(HttpRouteConfig {
        parent_refs,
        hostnames,
        rules,
    })
}

pub(crate) fn convert_http_rule(rule: &HTTPRouteRules, ns: &str) -> Result<HttpRouteRule> {
    let matches = rule
        .matches
        .as_ref()
        .map(|m| m.iter().map(convert_http_match).collect::<Vec<_>>())
        .unwrap_or_default();

    let filters = rule
        .filters
        .as_ref()
        .map(|f| f.iter().filter_map(|f| convert_http_filter(f, ns).ok()).collect::<Vec<_>>())
        .unwrap_or_default();

    let backend_refs = rule
        .backend_refs
        .as_ref()
        .map(|refs| refs.iter().map(|br| convert_http_backend_ref(br, ns)).collect::<Vec<_>>())
        .unwrap_or_default();

    let timeouts = rule.timeouts.as_ref().map(convert_timeouts);

    Ok(HttpRouteRule {
        matches,
        filters,
        backend_refs,
        timeouts,
    })
}

pub(crate) fn convert_http_match(m: &HTTPRouteRulesMatches) -> HttpRouteMatch {
    let method = m.method.as_ref().map(|m| match m {
        HTTPRouteRulesMatchesMethod::Get => "GET",
        HTTPRouteRulesMatchesMethod::Head => "HEAD",
        HTTPRouteRulesMatchesMethod::Post => "POST",
        HTTPRouteRulesMatchesMethod::Put => "PUT",
        HTTPRouteRulesMatchesMethod::Delete => "DELETE",
        HTTPRouteRulesMatchesMethod::Connect => "CONNECT",
        HTTPRouteRulesMatchesMethod::Options => "OPTIONS",
        HTTPRouteRulesMatchesMethod::Trace => "TRACE",
        HTTPRouteRulesMatchesMethod::Patch => "PATCH",
    }.to_string());

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
            add: $hm.add.as_ref()
                .map(|v| v.iter().map(|h| HttpHeader { name: h.name.clone(), value: h.value.clone() }).collect())
                .unwrap_or_default(),
            set: $hm.set.as_ref()
                .map(|v| v.iter().map(|h| HttpHeader { name: h.name.clone(), value: h.value.clone() }).collect())
                .unwrap_or_default(),
            remove: $hm.remove.clone().unwrap_or_default(),
        }
    };
}

pub(crate) fn convert_http_filter(f: &HTTPRouteRulesFilters, ns: &str) -> Result<HttpRouteFilter> {
    match f.r#type {
        HTTPRouteRulesFiltersType::RequestHeaderModifier => {
            let hm = f
                .request_header_modifier
                .as_ref()
                .ok_or_else(|| anyhow!("RequestHeaderModifier filter missing config"))?;
            Ok(HttpRouteFilter::RequestHeaderModifier {
                config: convert_header_mod!(hm),
            })
        }
        HTTPRouteRulesFiltersType::ResponseHeaderModifier => {
            let hm = f
                .response_header_modifier
                .as_ref()
                .ok_or_else(|| anyhow!("ResponseHeaderModifier filter missing config"))?;
            Ok(HttpRouteFilter::ResponseHeaderModifier {
                config: convert_header_mod!(hm),
            })
        }
        HTTPRouteRulesFiltersType::RequestRedirect => {
            let rr = f
                .request_redirect
                .as_ref()
                .ok_or_else(|| anyhow!("RequestRedirect filter missing config"))?;
            Ok(HttpRouteFilter::RequestRedirect {
                config: convert_redirect_config(rr),
            })
        }
        HTTPRouteRulesFiltersType::UrlRewrite => {
            let ur = f
                .url_rewrite
                .as_ref()
                .ok_or_else(|| anyhow!("URLRewrite filter missing config"))?;
            Ok(HttpRouteFilter::URLRewrite {
                config: convert_url_rewrite_config(ur),
            })
        }
        HTTPRouteRulesFiltersType::RequestMirror => {
            let rm = f
                .request_mirror
                .as_ref()
                .ok_or_else(|| anyhow!("RequestMirror filter missing config"))?;
            Ok(HttpRouteFilter::RequestMirror {
                config: convert_mirror_config(rm, ns),
            })
        }
        _ => Err(anyhow!("unsupported filter type")),
    }
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

fn convert_path_modifier(p: &HTTPRouteRulesFiltersRequestRedirectPath) -> Option<HttpURLRewritePath> {
    match p.r#type {
        HTTPRouteRulesFiltersRequestRedirectPathType::ReplaceFullPath => {
            p.replace_full_path.as_ref().map(|v| HttpURLRewritePath::ReplaceFullPath {
                value: v.clone(),
            })
        }
        HTTPRouteRulesFiltersRequestRedirectPathType::ReplacePrefixMatch => {
            p.replace_prefix_match.as_ref().map(|v| HttpURLRewritePath::ReplacePrefixMatch {
                value: v.clone(),
            })
        }
    }
}

fn convert_url_rewrite_config(ur: &HTTPRouteRulesFiltersUrlRewrite) -> URLRewriteConfig {
    let path = ur.path.as_ref().and_then(|p| match p.r#type {
        HTTPRouteRulesFiltersUrlRewritePathType::ReplaceFullPath => {
            p.replace_full_path.as_ref().map(|v| HttpURLRewritePath::ReplaceFullPath {
                value: v.clone(),
            })
        }
        HTTPRouteRulesFiltersUrlRewritePathType::ReplacePrefixMatch => {
            p.replace_prefix_match.as_ref().map(|v| HttpURLRewritePath::ReplacePrefixMatch {
                value: v.clone(),
            })
        }
    });

    URLRewriteConfig {
        hostname: ur.hostname.clone(),
        path,
    }
}

fn convert_mirror_config(
    rm: &HTTPRouteRulesFiltersRequestMirror,
    ns: &str,
) -> RequestMirrorConfig {
    RequestMirrorConfig {
        backend_ref: BackendRef {
            name: backend_dns_name(&rm.backend_ref.name, rm.backend_ref.namespace.as_deref(), ns),
            port: rm.backend_ref.port.unwrap_or(80) as u16,
            weight: 1,
            group: rm.backend_ref.group.clone().unwrap_or_default(),
            kind: rm.backend_ref.kind.clone().unwrap_or_else(|| "Service".to_string()),
            filters: vec![],
        },
    }
}

// ---------------------------------------------------------------------------
// HTTP backend refs
// ---------------------------------------------------------------------------

pub(crate) fn convert_http_backend_ref(br: &HTTPRouteRulesBackendRefs, ns: &str) -> BackendRef {
    BackendRef {
        name: backend_dns_name(&br.name, br.namespace.as_deref(), ns),
        port: br.port.unwrap_or(80) as u16,
        weight: br.weight.unwrap_or(1) as u32,
        group: br.group.clone().unwrap_or_default(),
        kind: br.kind.clone().unwrap_or_else(|| "Service".to_string()),
        filters: convert_backend_ref_filters(&br.filters),
    }
}

/// Convert per-backend filters (e.g. BackendRequestHeaderModifier).
pub(crate) fn convert_backend_ref_filters(filters: &Option<Vec<HTTPRouteRulesBackendRefsFilters>>) -> Vec<HttpRouteFilter> {
    let Some(filters) = filters else { return vec![] };
    filters.iter().filter_map(|f| {
        match f.r#type {
            HTTPRouteRulesBackendRefsFiltersType::RequestHeaderModifier => {
                let hm = f.request_header_modifier.as_ref()?;
                Some(HttpRouteFilter::RequestHeaderModifier {
                    config: HeaderModifierConfig {
                        add: hm.add.as_ref().map(|v| v.iter().map(|h| HttpHeader { name: h.name.clone(), value: h.value.clone() }).collect()).unwrap_or_default(),
                        set: hm.set.as_ref().map(|v| v.iter().map(|h| HttpHeader { name: h.name.clone(), value: h.value.clone() }).collect()).unwrap_or_default(),
                        remove: hm.remove.clone().unwrap_or_default(),
                    }
                })
            }
            HTTPRouteRulesBackendRefsFiltersType::ResponseHeaderModifier => {
                let hm = f.response_header_modifier.as_ref()?;
                Some(HttpRouteFilter::ResponseHeaderModifier {
                    config: HeaderModifierConfig {
                        add: hm.add.as_ref().map(|v| v.iter().map(|h| HttpHeader { name: h.name.clone(), value: h.value.clone() }).collect()).unwrap_or_default(),
                        set: hm.set.as_ref().map(|v| v.iter().map(|h| HttpHeader { name: h.name.clone(), value: h.value.clone() }).collect()).unwrap_or_default(),
                        remove: hm.remove.clone().unwrap_or_default(),
                    }
                })
            }
            _ => None,  // Other filter types not valid at backend level
        }
    }).collect()
}

// ---------------------------------------------------------------------------
// Timeouts
// ---------------------------------------------------------------------------

pub(crate) fn convert_timeouts(t: &HTTPRouteRulesTimeouts) -> HttpRouteTimeouts {
    HttpRouteTimeouts {
        request: t.request.as_ref().and_then(|s| parse_gateway_duration(s)),
        backend_request: t.backend_request.as_ref().and_then(|s| parse_gateway_duration(s)),
    }
}

/// Parse Gateway API duration string (e.g. "10s", "500ms", "1m").
fn parse_gateway_duration(s: &str) -> Option<std::time::Duration> {
    crate::config::parsing::parse_duration(s).ok()
}

// ---------------------------------------------------------------------------
// L4 route conversion (TCP, TLS, UDP)
// ---------------------------------------------------------------------------

/// Shared conversion for L4 backend refs (TCP/TLS/UDP all have the same fields).
/// `default_port`: protocol-implied default (TCP=80, TLS=443) or `None` if port is required (UDP).
pub(crate) fn convert_l4_backend_ref<B: L4BackendRefAccess>(br: &B, ns: &str, protocol: &str, default_port: Option<i32>) -> BackendRef {
    let port = match br.br_port() {
        Some(p) => p as u16,
        None => match default_port {
            Some(dp) => {
                warn!("{} backend ref '{}' missing port, defaulting to {}", protocol, br.br_name(), dp);
                dp as u16
            }
            None => {
                warn!("{} backend ref '{}' missing required port, using 0", protocol, br.br_name());
                0
            }
        }
    };
    BackendRef {
        name: backend_dns_name(br.br_name(), br.br_namespace(), ns),
        port,
        weight: br.br_weight().unwrap_or(1) as u32,
        group: br.br_group().unwrap_or_default().to_string(),
        kind: br.br_kind().map(String::from).unwrap_or_else(|| "Service".to_string()),
        filters: vec![],
    }
}

pub(crate) fn convert_tcp_route(route: &TCPRoute, gateway_name: &str) -> Result<TcpRouteConfig> {
    let ns = route_namespace(&route.metadata);
    let parent_refs = extract_parent_refs(&route.spec.parent_refs, gateway_name);

    let rules = route.spec.rules.iter()
        .map(|r| TcpRouteRule {
            backend_refs: r.backend_refs.iter().map(|br| convert_l4_backend_ref(br, ns, "TCP", Some(80))).collect(),
        })
        .collect();

    Ok(TcpRouteConfig { parent_refs, rules })
}

pub(crate) fn convert_tls_route(route: &TLSRoute, gateway_name: &str) -> Result<TlsRouteConfig> {
    let ns = route_namespace(&route.metadata);
    let parent_refs = extract_parent_refs(&route.spec.parent_refs, gateway_name);
    let hostnames = route.spec.hostnames.clone();

    let rules = route.spec.rules.iter()
        .map(|r| TlsRouteRule {
            backend_refs: r.backend_refs.iter().map(|br| convert_l4_backend_ref(br, ns, "TLS", Some(443))).collect(),
        })
        .collect();

    Ok(TlsRouteConfig { parent_refs, hostnames, rules })
}

pub(crate) fn convert_udp_route(route: &UDPRoute, gateway_name: &str) -> Result<UdpRouteConfig> {
    let ns = route_namespace(&route.metadata);
    let parent_refs = extract_parent_refs(&route.spec.parent_refs, gateway_name);

    let rules = route.spec.rules.iter()
        .map(|r| UdpRouteRule {
            backend_refs: r.backend_refs.iter().map(|br| convert_l4_backend_ref(br, ns, "UDP", None)).collect(),
        })
        .collect();

    Ok(UdpRouteConfig { parent_refs, rules })
}

// ---------------------------------------------------------------------------
// Utility helpers
// ---------------------------------------------------------------------------

/// Format backend name as `{service}.{namespace}.svc` for DNS resolution.
pub(crate) fn backend_dns_name(name: &str, namespace: Option<&str>, route_namespace: &str) -> String {
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
