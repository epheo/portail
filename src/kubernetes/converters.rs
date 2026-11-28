//! Gateway API type → internal config type converters.
//!
//! Pure functions that convert kube-rs generated Gateway API structs
//! into portail's internal config types. No I/O, no K8s API calls.

use anyhow::{anyhow, Result};

// The experimental channel's: a superset of the standard types, so every
// route the cluster's CRDs admit decodes, and what this proxy does not
// implement is refused rather than dropped. A standard type lacks
// ExternalAuth, and one such route failed its decode: unseen, or served at
// its last version. useDefaultGateways needs no refusal: no Gateway of
// portail's is a default one.
use gateway_api::experimental::grpcroutes::v1::*;
use gateway_api::experimental::httproutes::v1::*;
use gateway_api::experimental::tcproutes::v1::*;
use gateway_api::experimental::tlsroutes::v1::*;
use gateway_api::experimental::udproutes::v1::*;
use gateway_api::gateways::{
    Gateway, GatewayListeners, GatewayListenersTlsCertificateRefs, GatewayListenersTlsMode,
};

use crate::config::*;
use crate::logging::warn;

use super::parent_ref::L4BackendRefAccess;

// ---------------------------------------------------------------------------
// Type alias for TLS certificate data
// ---------------------------------------------------------------------------

/// TLS certificate data keyed by (secret_name, namespace) → (cert_pem, key_pem).
pub(crate) type CertData = std::collections::HashMap<(String, String), (Vec<u8>, Vec<u8>)>;

/// The [`CertData`] key of a certificateRef naming a core Secret; `None`
/// for any other group or kind, which nothing reads: a Secret of the same
/// name must not stand in for it.
pub(crate) fn secret_ref_key(
    r: &GatewayListenersTlsCertificateRefs,
    gw_ns: &str,
) -> Option<(String, String)> {
    let core_secret = r.group.as_deref().unwrap_or("").is_empty()
        && r.kind.as_deref().unwrap_or("Secret") == "Secret";
    core_secret.then(|| {
        (
            r.name.clone(),
            r.namespace.as_deref().unwrap_or(gw_ns).to_string(),
        )
    })
}

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
        proxy_protocol: None,
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
        // A ref without a resolved cert is left out, never sent without
        // PEM: the data plane would take the unchanged set of refs for a
        // bad rotation and keep serving a revoked or deleted cert.
        let certificate_refs = t
            .certificate_refs
            .iter()
            .flatten()
            .filter_map(|r| {
                let key = secret_ref_key(r, gw_ns)?;
                let (cert_pem, key_pem) = cert_data.get(&key)?;
                Some(CertificateRef {
                    name: key.0,
                    namespace: Some(key.1),
                    hostname: l.hostname.clone(),
                    cert_pem: Some(cert_pem.clone()),
                    key_pem: Some(key_pem.clone()),
                })
            })
            .collect();
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
pub(crate) fn convert_http_route(route: &HttpRoute) -> (HttpRouteConfig, Result<(), RouteError>) {
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

fn convert_http_rule(rule: &HttpRouteRules, ns: &str) -> (HttpRouteRule, Option<RouteError>) {
    let matches = rule
        .matches
        .as_ref()
        .map(|m| m.iter().map(convert_http_match).collect::<Vec<_>>())
        .unwrap_or_default();

    // A filter this proxy cannot represent fails the route, never dropped
    // while the route serves without it.
    let mut first = None;
    if rule.session_persistence.is_some() {
        first = Some(RouteError::unsupported(
            "sessionPersistence is not supported",
        ));
    }
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
    // Like a filter: a retry this proxy cannot follow fails the route.
    let retry = rule.retry.as_ref().and_then(|r| match convert_retry(r) {
        Ok(retry) => Some(retry),
        Err(e) => {
            first.get_or_insert(e.at(format_args!("retry")));
            None
        }
    });

    let rule = HttpRouteRule {
        matches,
        filters,
        backend_refs,
        timeouts,
        retry,
    };
    (rule, first)
}

fn convert_http_match(m: &HttpRouteRulesMatches) -> HttpRouteMatch {
    let method = m.method.as_ref().map(|m| {
        match m {
            HttpRouteRulesMatchesMethod::Get => "GET",
            HttpRouteRulesMatchesMethod::Head => "HEAD",
            HttpRouteRulesMatchesMethod::Post => "POST",
            HttpRouteRulesMatchesMethod::Put => "PUT",
            HttpRouteRulesMatchesMethod::Delete => "DELETE",
            HttpRouteRulesMatchesMethod::Connect => "CONNECT",
            HttpRouteRulesMatchesMethod::Options => "OPTIONS",
            HttpRouteRulesMatchesMethod::Trace => "TRACE",
            HttpRouteRulesMatchesMethod::Patch => "PATCH",
        }
        .to_string()
    });

    let path = m.path.as_ref().map(|p| {
        let match_type = match p.r#type {
            Some(HttpRouteRulesMatchesPathType::Exact) => HttpPathMatchType::Exact,
            Some(HttpRouteRulesMatchesPathType::RegularExpression) => {
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

fn convert_header_match(h: &HttpRouteRulesMatchesHeaders) -> HttpHeaderMatch {
    let match_type = match h.r#type {
        Some(HttpRouteRulesMatchesHeadersType::RegularExpression) => {
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

fn convert_query_param_match(q: &HttpRouteRulesMatchesQueryParams) -> HttpQueryParamMatch {
    let match_type = match q.r#type {
        Some(HttpRouteRulesMatchesQueryParamsType::RegularExpression) => {
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
fn convert_http_filter(f: &HttpRouteRulesFilters, ns: &str) -> Result<HttpRouteFilter, RouteError> {
    let missing = |what: &str| RouteError::unsupported(format!("{} filter missing config", what));
    Ok(match f.r#type {
        HttpRouteRulesFiltersType::RequestHeaderModifier => {
            let hm = f
                .request_header_modifier
                .as_ref()
                .ok_or_else(|| missing("RequestHeaderModifier"))?;
            HttpRouteFilter::RequestHeaderModifier {
                config: convert_header_mod!(hm),
            }
        }
        HttpRouteRulesFiltersType::ResponseHeaderModifier => {
            let hm = f
                .response_header_modifier
                .as_ref()
                .ok_or_else(|| missing("ResponseHeaderModifier"))?;
            HttpRouteFilter::ResponseHeaderModifier {
                config: convert_header_mod!(hm),
            }
        }
        HttpRouteRulesFiltersType::RequestRedirect => {
            let rr = f
                .request_redirect
                .as_ref()
                .ok_or_else(|| missing("RequestRedirect"))?;
            HttpRouteFilter::RequestRedirect {
                config: convert_redirect_config(rr),
            }
        }
        HttpRouteRulesFiltersType::UrlRewrite => {
            let ur = f
                .url_rewrite
                .as_ref()
                .ok_or_else(|| missing("URLRewrite"))?;
            HttpRouteFilter::URLRewrite {
                config: convert_url_rewrite_config(ur),
            }
        }
        HttpRouteRulesFiltersType::RequestMirror => {
            let rm = f
                .request_mirror
                .as_ref()
                .ok_or_else(|| missing("RequestMirror"))?;
            HttpRouteFilter::RequestMirror {
                config: convert_mirror_config(rm, ns),
            }
        }
        HttpRouteRulesFiltersType::Cors => {
            let cors = f.cors.as_ref().ok_or_else(|| missing("CORS"))?;
            let list = |names: &Option<Vec<String>>| names.clone().unwrap_or_default();
            HttpRouteFilter::Cors {
                config: crate::config::CorsConfig {
                    allow_origins: list(&cors.allow_origins),
                    allow_methods: list(&cors.allow_methods),
                    allow_headers: list(&cors.allow_headers),
                    expose_headers: list(&cors.expose_headers),
                    allow_credentials: cors.allow_credentials.unwrap_or(false),
                    max_age: cors.max_age.and_then(|age| u32::try_from(age).ok()),
                },
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

fn convert_redirect_config(rr: &HttpRouteRulesFiltersRequestRedirect) -> RequestRedirectConfig {
    let scheme = rr.scheme.as_ref().map(|s| match s {
        HttpRouteRulesFiltersRequestRedirectScheme::Http => "http".to_string(),
        HttpRouteRulesFiltersRequestRedirectScheme::Https => "https".to_string(),
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
    p: &HttpRouteRulesFiltersRequestRedirectPath,
) -> Option<HttpURLRewritePath> {
    match p.r#type {
        HttpRouteRulesFiltersRequestRedirectPathType::ReplaceFullPath => p
            .replace_full_path
            .as_ref()
            .map(|v| HttpURLRewritePath::ReplaceFullPath { value: v.clone() }),
        HttpRouteRulesFiltersRequestRedirectPathType::ReplacePrefixMatch => p
            .replace_prefix_match
            .as_ref()
            .map(|v| HttpURLRewritePath::ReplacePrefixMatch { value: v.clone() }),
    }
}

fn convert_url_rewrite_config(ur: &HttpRouteRulesFiltersUrlRewrite) -> URLRewriteConfig {
    let path = ur.path.as_ref().and_then(|p| match p.r#type {
        HttpRouteRulesFiltersUrlRewritePathType::ReplaceFullPath => p
            .replace_full_path
            .as_ref()
            .map(|v| HttpURLRewritePath::ReplaceFullPath { value: v.clone() }),
        HttpRouteRulesFiltersUrlRewritePathType::ReplacePrefixMatch => p
            .replace_prefix_match
            .as_ref()
            .map(|v| HttpURLRewritePath::ReplacePrefixMatch { value: v.clone() }),
    });

    URLRewriteConfig {
        hostname: ur.hostname.clone(),
        path,
    }
}

/// A RequestMirror filter, HTTPRoute's or GRPCRoute's: the two generated
/// types differ in name only.
macro_rules! convert_mirror {
    ($rm:expr, $ns:expr) => {{
        let rm = $rm;
        let fraction = rm.fraction.as_ref().map(|f| crate::config::MirrorFraction {
            numerator: f.numerator.max(0) as u32,
            denominator: f.denominator.map(|d| d.max(1) as u32).unwrap_or(100),
        });
        let percent = rm.percent.map(|p| p.clamp(0, 100) as u32);
        RequestMirrorConfig {
            backend_ref: BackendRef {
                name: backend_ref_name(
                    &rm.backend_ref.name,
                    rm.backend_ref.namespace.as_deref(),
                    $ns,
                    rm.backend_ref.group.as_deref(),
                    rm.backend_ref.kind.as_deref(),
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
                tls: None,
            },
            percent,
            fraction,
        }
    }};
}

fn convert_mirror_config(rm: &HttpRouteRulesFiltersRequestMirror, ns: &str) -> RequestMirrorConfig {
    convert_mirror!(rm, ns)
}

// ---------------------------------------------------------------------------
// HTTP backend refs
// ---------------------------------------------------------------------------

/// Like `convert_http_rule`: a filter that cannot be represented is left
/// out and named.
fn convert_http_backend_ref(
    br: &HttpRouteRulesBackendRefs,
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
        name: backend_ref_name(
            &br.name,
            br.namespace.as_deref(),
            ns,
            br.group.as_deref(),
            br.kind.as_deref(),
        ),
        port: br.port.unwrap_or(80) as u16,
        weight: br.weight.unwrap_or(1) as u32,
        group: br.group.clone().unwrap_or_default(),
        kind: br.kind.clone().unwrap_or_else(|| "Service".to_string()),
        filters,
        app_protocol: None,
        invalid: false,
        tls: None,
    };
    (backend, first)
}

/// Per-backend filters (e.g. BackendRequestHeaderModifier). The data plane
/// applies only header modifiers per backend: any other type fails the
/// route like an unsupported rule filter, instead of serving without it.
fn convert_backend_ref_filter(
    f: &HttpRouteRulesBackendRefsFilters,
) -> Result<HttpRouteFilter, RouteError> {
    let missing = |what: &str| RouteError::unsupported(format!("{} filter missing config", what));
    Ok(match f.r#type {
        HttpRouteRulesBackendRefsFiltersType::RequestHeaderModifier => {
            let hm = f
                .request_header_modifier
                .as_ref()
                .ok_or_else(|| missing("RequestHeaderModifier"))?;
            HttpRouteFilter::RequestHeaderModifier {
                config: convert_header_mod!(hm),
            }
        }
        HttpRouteRulesBackendRefsFiltersType::ResponseHeaderModifier => {
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

fn convert_timeouts(t: &HttpRouteRulesTimeouts) -> HttpRouteTimeouts {
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

/// The API's bounds, checked where its integers become the config's: codes
/// 400 to 599, attempts at least 0 and one when not written (the CRD's
/// default, which an older CRD may not have applied), a backoff that is a
/// duration. The timeouts' silent drop of a bad duration would here mean
/// retrying with no wait where one was asked for.
fn convert_retry(r: &HttpRouteRulesRetry) -> Result<crate::config::HttpRouteRetry, RouteError> {
    let codes = r
        .codes
        .iter()
        .flatten()
        .map(|&code| match u16::try_from(code) {
            Ok(code @ 400..=599) => Ok(code),
            _ => Err(RouteError::unsupported(format!(
                "retry code {code}: only 400 to 599"
            ))),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let attempts = match r.attempts {
        None => 1,
        Some(n) => u32::try_from(n)
            .map_err(|_| RouteError::unsupported(format!("retry attempts {n}: at least 0")))?,
    };
    let backoff = r
        .backoff
        .as_deref()
        .map(|s| {
            crate::config::parsing::parse_duration(s)
                .map_err(|e| RouteError::unsupported(format!("retry backoff {s:?}: {e}")))
        })
        .transpose()?;
    Ok(crate::config::HttpRouteRetry {
        codes,
        attempts,
        backoff,
    })
}

// ---------------------------------------------------------------------------
// GRPC route conversion
// ---------------------------------------------------------------------------

/// As [`convert_http_route`]: `Err` names the first part the proxy cannot
/// represent, left out of a config that keeps every backendRef.
pub(crate) fn convert_grpc_route(route: &GrpcRoute) -> (GrpcRouteConfig, Result<(), RouteError>) {
    let ns = route_namespace(&route.metadata);
    let mut first = None;
    let rules = route
        .spec
        .rules
        .iter()
        .flatten()
        .enumerate()
        .map(|(i, r)| {
            let (rule, err) = convert_grpc_rule(r, ns);
            if let Some(e) = err {
                first.get_or_insert(e.at(format_args!("rule {}", i)));
            }
            rule
        })
        .collect();
    let config = GrpcRouteConfig {
        parent_refs: Vec::new(),
        hostnames: route.spec.hostnames.clone().unwrap_or_default(),
        rules,
    };
    (config, first.map_or(Ok(()), Err))
}

fn convert_grpc_rule(rule: &GrpcRouteRules, ns: &str) -> (GrpcRouteRule, Option<RouteError>) {
    let mut first = None;
    if rule.session_persistence.is_some() {
        first = Some(RouteError::unsupported(
            "sessionPersistence is not supported",
        ));
    }
    let matches = rule
        .matches
        .iter()
        .flatten()
        .map(|m| GrpcRouteMatch {
            method: m.method.as_ref().map(|method| GrpcMethodMatch {
                match_type: match method.r#type {
                    Some(GrpcRouteRulesMatchesMethodType::RegularExpression) => {
                        StringMatchType::RegularExpression
                    }
                    _ => StringMatchType::Exact,
                },
                service: method.service.clone().filter(|s| !s.is_empty()),
                method: method.method.clone().filter(|m| !m.is_empty()),
            }),
            headers: m
                .headers
                .iter()
                .flatten()
                .map(|h| HttpHeaderMatch {
                    name: h.name.clone(),
                    value: h.value.clone(),
                    match_type: match h.r#type {
                        Some(GrpcRouteRulesMatchesHeadersType::RegularExpression) => {
                            StringMatchType::RegularExpression
                        }
                        _ => StringMatchType::Exact,
                    },
                })
                .collect(),
        })
        .collect();
    let filters = rule
        .filters
        .iter()
        .flatten()
        .enumerate()
        .filter_map(|(i, f)| match convert_grpc_filter(f, ns) {
            Ok(filter) => Some(filter),
            Err(e) => {
                first.get_or_insert(e.at(format_args!("filter {}", i)));
                None
            }
        })
        .collect();
    let backend_refs = rule
        .backend_refs
        .iter()
        .flatten()
        .enumerate()
        .map(|(i, br)| {
            let (backend, err) = convert_grpc_backend_ref(br, ns);
            if let Some(e) = err {
                first.get_or_insert(e.at(format_args!("backendRef {}", i)));
            }
            backend
        })
        .collect();
    let rule = GrpcRouteRule {
        matches,
        filters,
        backend_refs,
    };
    (rule, first)
}

fn convert_grpc_filter(f: &GrpcRouteRulesFilters, ns: &str) -> Result<HttpRouteFilter, RouteError> {
    let missing = |what: &str| RouteError::unsupported(format!("{} filter missing config", what));
    Ok(match f.r#type {
        GrpcRouteRulesFiltersType::RequestHeaderModifier => {
            let hm = f
                .request_header_modifier
                .as_ref()
                .ok_or_else(|| missing("RequestHeaderModifier"))?;
            HttpRouteFilter::RequestHeaderModifier {
                config: convert_header_mod!(hm),
            }
        }
        GrpcRouteRulesFiltersType::ResponseHeaderModifier => {
            let hm = f
                .response_header_modifier
                .as_ref()
                .ok_or_else(|| missing("ResponseHeaderModifier"))?;
            HttpRouteFilter::ResponseHeaderModifier {
                config: convert_header_mod!(hm),
            }
        }
        GrpcRouteRulesFiltersType::RequestMirror => {
            let rm = f
                .request_mirror
                .as_ref()
                .ok_or_else(|| missing("RequestMirror"))?;
            HttpRouteFilter::RequestMirror {
                config: convert_mirror!(rm, ns),
            }
        }
        GrpcRouteRulesFiltersType::ExtensionRef => {
            return Err(RouteError::unsupported(
                "unsupported filter type ExtensionRef",
            ))
        }
    })
}

/// A backendRef's own filters are header modifiers only, as HTTPRoute's.
fn convert_grpc_backend_ref(
    br: &GrpcRouteRulesBackendRefs,
    ns: &str,
) -> (BackendRef, Option<RouteError>) {
    let mut first = None;
    let missing = |what: &str| RouteError::unsupported(format!("{} filter missing config", what));
    let filters = br
        .filters
        .iter()
        .flatten()
        .enumerate()
        .filter_map(|(i, f)| {
            let converted = match f.r#type {
                GrpcRouteRulesBackendRefsFiltersType::RequestHeaderModifier => f
                    .request_header_modifier
                    .as_ref()
                    .map(|hm| HttpRouteFilter::RequestHeaderModifier {
                        config: convert_header_mod!(hm),
                    })
                    .ok_or_else(|| missing("RequestHeaderModifier")),
                GrpcRouteRulesBackendRefsFiltersType::ResponseHeaderModifier => f
                    .response_header_modifier
                    .as_ref()
                    .map(|hm| HttpRouteFilter::ResponseHeaderModifier {
                        config: convert_header_mod!(hm),
                    })
                    .ok_or_else(|| missing("ResponseHeaderModifier")),
                ref other => Err(RouteError::unsupported(format!(
                    "{:?} is not supported on a backendRef",
                    other
                ))),
            };
            match converted {
                Ok(filter) => Some(filter),
                Err(e) => {
                    first.get_or_insert(e.at(format_args!("filter {}", i)));
                    None
                }
            }
        })
        .collect();
    let backend = BackendRef {
        name: backend_ref_name(
            &br.name,
            br.namespace.as_deref(),
            ns,
            br.group.as_deref(),
            br.kind.as_deref(),
        ),
        port: br.port.unwrap_or(80) as u16,
        weight: br.weight.unwrap_or(1) as u32,
        group: br.group.clone().unwrap_or_default(),
        kind: br.kind.clone().unwrap_or_else(|| "Service".to_string()),
        filters,
        app_protocol: None,
        invalid: false,
        tls: None,
    };
    (backend, first)
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
        name: backend_ref_name(
            br.br_name(),
            br.br_namespace(),
            ns,
            br.br_group(),
            br.br_kind(),
        ),
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
        tls: None,
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

pub(crate) fn convert_tcp_route(route: &TcpRoute) -> (TcpRouteConfig, Result<(), RouteError>) {
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

pub(crate) fn convert_tls_route(route: &TlsRoute) -> (TlsRouteConfig, Result<(), RouteError>) {
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

pub(crate) fn convert_udp_route(route: &UdpRoute) -> (UdpRouteConfig, Result<(), RouteError>) {
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

/// A backendRef's name: a Service's DNS name, or for another Gateway
/// `{gateway}.{namespace}.gateway`, which no Service's overrides are keyed
/// by.
pub(crate) fn backend_ref_name(
    name: &str,
    namespace: Option<&str>,
    route_namespace: &str,
    group: Option<&str>,
    kind: Option<&str>,
) -> String {
    if group == Some(crate::config::GATEWAY_GROUP) && kind == Some("Gateway") {
        crate::config::gateway_backend_name(name, namespace.unwrap_or(route_namespace))
    } else {
        backend_dns_name(name, namespace, route_namespace)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The data plane dials a backend named by an IP address unchecked, so
    /// no backendRef may convert to one, whatever its name.
    #[test]
    fn a_backend_ref_name_is_never_an_ip_address() {
        let forms = [
            (None, None),
            (Some(""), Some("Service")),
            (Some(crate::config::GATEWAY_GROUP), Some("Gateway")),
        ];
        for name in ["127.0.0.1", "::1", "169.254.169.254"] {
            for namespace in [None, Some("p")] {
                for (group, kind) in forms {
                    let built = backend_ref_name(name, namespace, "default", group, kind);
                    assert!(built.parse::<std::net::IpAddr>().is_err(), "{built}");
                }
            }
        }
    }
    use gateway_api::gateways::{GatewayListenersTls, GatewaySpec};

    /// A listener carries the refs whose cert resolved, and only those:
    /// one sent without PEM kept a revoked cert served (see
    /// `convert_listener`).
    #[test]
    fn a_listener_carries_only_refs_with_a_resolved_cert() {
        let refs = ["good", "revoked"].map(|name| GatewayListenersTlsCertificateRefs {
            name: name.into(),
            namespace: Some("certs".into()),
            ..Default::default()
        });
        let gw = Gateway {
            metadata: kube::core::ObjectMeta {
                name: Some("gw".into()),
                namespace: Some("default".into()),
                ..Default::default()
            },
            spec: GatewaySpec {
                gateway_class_name: "portail".into(),
                listeners: vec![GatewayListeners {
                    name: "https".into(),
                    port: 443,
                    protocol: "HTTPS".into(),
                    hostname: Some("app.example.com".into()),
                    tls: Some(GatewayListenersTls {
                        mode: Some(GatewayListenersTlsMode::Terminate),
                        certificate_refs: Some(refs.to_vec()),
                        ..Default::default()
                    }),
                    allowed_routes: None,
                }],
                ..Default::default()
            },
            status: None,
        };
        let certs = CertData::from([(
            ("good".to_string(), "certs".to_string()),
            (b"cert".to_vec(), b"key".to_vec()),
        )]);

        let config = convert_gateway(&gw, &certs).unwrap();
        let tls = config.listeners[0].tls.as_ref().unwrap();
        let carried: Vec<_> = tls
            .certificate_refs
            .iter()
            .map(|r| {
                (
                    r.name.as_str(),
                    r.namespace.as_deref(),
                    r.hostname.as_deref(),
                    r.cert_pem.as_deref(),
                    r.key_pem.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            carried,
            vec![(
                "good",
                Some("certs"),
                Some("app.example.com"),
                Some(b"cert".as_slice()),
                Some(b"key".as_slice())
            )]
        );
    }

    /// A route the experimental CRDs admit decodes, alone or in a list,
    /// where the standard types refused it; what it asks that the proxy
    /// does not do fails it, naming the part.
    #[test]
    fn an_experimental_route_decodes_and_what_is_not_done_is_refused() {
        let route = |rule: serde_json::Value| {
            serde_json::json!({
                "apiVersion": "gateway.networking.k8s.io/v1",
                "kind": "HTTPRoute",
                "metadata": {"name": "r", "namespace": "apps", "resourceVersion": "1"},
                "spec": {"parentRefs": [{"name": "gw"}], "rules": [rule]},
            })
        };
        let auth = route(serde_json::json!({
            "filters": [{"type": "ExternalAuth", "externalAuth": {
                "protocol": "HTTP", "backendRef": {"name": "auth", "port": 8080}}}],
            "backendRefs": [{"name": "web", "port": 80}],
        }));
        let sticky = route(serde_json::json!({
            "sessionPersistence": {"type": "Cookie", "sessionName": "s"},
            "backendRefs": [{"name": "web", "port": 80}],
        }));
        // Why the types changed: the standard ones cannot read the first.
        assert!(
            serde_json::from_value::<gateway_api::httproutes::HttpRoute>(auth.clone()).is_err()
        );
        let list: kube::core::ObjectList<HttpRoute> = serde_json::from_value(serde_json::json!({
            "apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRouteList",
            "metadata": {"resourceVersion": "2"}, "items": [auth.clone(), sticky.clone()],
        }))
        .expect("a list holding them decodes whole");
        assert_eq!(list.items.len(), 2);

        for (route, part) in [(auth, "ExternalAuth"), (sticky, "sessionPersistence")] {
            let route: HttpRoute = serde_json::from_value(route).unwrap();
            let err = convert_http_route(&route).1.unwrap_err();
            assert_eq!(err.reason.as_str(), "UnsupportedValue", "{err}");
            assert!(err.to_string().contains(part), "{err}");
        }
    }

    /// A GRPCRoute reaches the config as the API writes it: service and
    /// method matches, one left out matching any, headers, its three
    /// filters, backendRefs with their header modifiers. ExtensionRef and
    /// a mirror on a backendRef fail the route, naming the part.
    #[test]
    fn a_grpc_route_converts_as_written() {
        let route: GrpcRoute = serde_json::from_value(serde_json::json!({
            "metadata": {"name": "g", "namespace": "apps"},
            "spec": {
                "hostnames": ["grpc.test"],
                "rules": [{
                    "matches": [
                        {"method": {"service": "echo.Echo", "method": "Say"},
                         "headers": [{"name": "version", "value": "2"}]},
                        {"method": {"type": "RegularExpression", "method": "Say.*"}},
                    ],
                    "filters": [
                        {"type": "RequestHeaderModifier",
                         "requestHeaderModifier": {"set": [{"name": "x-a", "value": "1"}]}},
                        {"type": "RequestMirror",
                         "requestMirror": {"backendRef": {"name": "shadow", "port": 9000}}},
                    ],
                    "backendRefs": [{
                        "name": "echo", "port": 9000, "weight": 3,
                        "filters": [{"type": "ResponseHeaderModifier",
                                     "responseHeaderModifier": {"remove": ["x-b"]}}],
                    }],
                }],
            },
        }))
        .unwrap();
        let (config, result) = convert_grpc_route(&route);
        result.unwrap();
        assert_eq!(config.hostnames, ["grpc.test"]);
        let rule = &config.rules[0];
        let exact = rule.matches[0].method.as_ref().unwrap();
        assert_eq!(exact.match_type, StringMatchType::Exact);
        assert_eq!(
            (exact.service.as_deref(), exact.method.as_deref()),
            (Some("echo.Echo"), Some("Say"))
        );
        assert_eq!(rule.matches[0].headers[0].name, "version");
        let regex = rule.matches[1].method.as_ref().unwrap();
        assert_eq!(regex.match_type, StringMatchType::RegularExpression);
        assert_eq!(
            (regex.service.as_deref(), regex.method.as_deref()),
            (None, Some("Say.*"))
        );
        assert!(matches!(
            rule.filters[0],
            HttpRouteFilter::RequestHeaderModifier { .. }
        ));
        let HttpRouteFilter::RequestMirror { config: mirror } = &rule.filters[1] else {
            panic!("the mirror filter");
        };
        assert_eq!(mirror.backend_ref.name, "shadow.apps.svc");
        let backend = &rule.backend_refs[0];
        assert_eq!(
            (backend.name.as_str(), backend.port, backend.weight),
            ("echo.apps.svc", 9000, 3)
        );
        assert!(matches!(
            backend.filters[0],
            HttpRouteFilter::ResponseHeaderModifier { .. }
        ));

        for (spec, part) in [
            (
                serde_json::json!({"rules": [{"filters": [{"type": "ExtensionRef",
                    "extensionRef": {"group": "x.test", "kind": "X", "name": "x"}}]}]}),
                "filter 0",
            ),
            (
                serde_json::json!({"rules": [{"backendRefs": [{"name": "echo", "port": 9000,
                    "filters": [{"type": "RequestMirror",
                                 "requestMirror": {"backendRef": {"name": "s", "port": 1}}}]}]}]}),
                "backendRef 0",
            ),
            (
                serde_json::json!({"rules": [{"sessionPersistence": {"type": "Cookie"},
                    "backendRefs": [{"name": "echo", "port": 9000}]}]}),
                "sessionPersistence",
            ),
        ] {
            let route: GrpcRoute = serde_json::from_value(serde_json::json!({
                "metadata": {"name": "g", "namespace": "apps"}, "spec": spec,
            }))
            .unwrap();
            let err = convert_grpc_route(&route).1.unwrap_err();
            assert_eq!(err.reason.as_str(), "UnsupportedValue");
            assert!(err.to_string().contains(part), "{err}");
        }
    }

    /// A rule's retry reaches the config as the API means it: attempts one
    /// when not written, none at zero; anything outside the API's bounds
    /// fails the route rather than retry otherwise than asked.
    #[test]
    fn a_retry_converts_within_the_apis_bounds() {
        let convert = |retry: serde_json::Value| {
            let rule: HttpRouteRules = serde_json::from_value(serde_json::json!({
                "backendRefs": [{"name": "web", "port": 80}],
                "retry": retry,
            }))
            .unwrap();
            convert_http_rule(&rule, "default")
        };
        let (rule, err) =
            convert(serde_json::json!({"codes": [500, 502], "attempts": 3, "backoff": "250ms"}));
        assert!(err.is_none(), "{err:?}");
        let retry = rule.retry.unwrap();
        assert_eq!((retry.codes, retry.attempts), (vec![500, 502], 3));
        assert_eq!(retry.backoff, Some(std::time::Duration::from_millis(250)));

        let (rule, _) = convert(serde_json::json!({}));
        let retry = rule.retry.unwrap();
        assert_eq!(
            (retry.codes.len(), retry.attempts, retry.backoff),
            (0, 1, None)
        );
        assert_eq!(
            convert(serde_json::json!({"attempts": 0}))
                .0
                .retry
                .unwrap()
                .attempts,
            0
        );

        for bad in [
            serde_json::json!({"codes": [399]}),
            serde_json::json!({"codes": [600]}),
            serde_json::json!({"attempts": -1}),
            serde_json::json!({"backoff": "soon"}),
        ] {
            let (rule, err) = convert(bad.clone());
            let err = err.unwrap_or_else(|| panic!("{bad} accepted"));
            assert_eq!(err.reason.as_str(), "UnsupportedValue", "{bad}");
            assert!(rule.retry.is_none(), "{bad}");
        }

        let rule: HttpRouteRules = serde_json::from_value(
            serde_json::json!({"backendRefs": [{"name": "web", "port": 80}]}),
        )
        .unwrap();
        assert!(convert_http_rule(&rule, "default").0.retry.is_none());
    }

    /// The filter reaches the config as the route writes it; what the API
    /// leaves out is left out, for the data plane's defaults.
    #[test]
    fn a_cors_filter_converts_field_for_field() {
        let filter = |cors: serde_json::Value| -> HttpRouteRulesFilters {
            serde_json::from_value(serde_json::json!({"type": "CORS", "cors": cors})).unwrap()
        };
        let full = filter(serde_json::json!({
            "allowOrigins": ["https://app.test", "https://*.example.org"],
            "allowMethods": ["GET", "PUT"],
            "allowHeaders": ["content-type"],
            "exposeHeaders": ["x-request-id"],
            "allowCredentials": true,
            "maxAge": 600
        }));
        let converted = convert_http_filter(&full, "default").unwrap();
        assert_eq!(
            serde_json::to_value(&converted).unwrap(),
            serde_json::json!({"type": "CORS", "cors": {
                "allowOrigins": ["https://app.test", "https://*.example.org"],
                "allowMethods": ["GET", "PUT"],
                "allowHeaders": ["content-type"],
                "exposeHeaders": ["x-request-id"],
                "allowCredentials": true,
                "maxAge": 600
            }})
        );

        let bare = convert_http_filter(&filter(serde_json::json!({})), "default").unwrap();
        assert_eq!(
            serde_json::to_value(&bare).unwrap(),
            serde_json::json!({"type": "CORS", "cors": {}})
        );
    }
}
