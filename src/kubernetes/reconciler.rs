use anyhow::{anyhow, Result};

use gateway_api::gateways::{Gateway, GatewayListeners, GatewayListenersTlsMode};
use gateway_api::httproutes::*;
use gateway_api::experimental::tcproutes::*;
use gateway_api::experimental::tlsroutes::*;
use gateway_api::experimental::udproutes::*;

use crate::config::*;

/// Convert Kubernetes Gateway API resources into a UringRessConfig.
/// This reuses all existing validation, conversion, hostname intersection,
/// and regex compilation logic via `to_route_table()`.
pub fn reconcile_to_config(
    gateway: &Gateway,
    http_routes: &[HTTPRoute],
    tcp_routes: &[TCPRoute],
    tls_routes: &[TLSRoute],
    udp_routes: &[UDPRoute],
) -> Result<UringRessConfig> {
    let gateway_config = convert_gateway(gateway)?;
    let gateway_name = gateway.metadata.name.as_deref().unwrap_or("default");

    let http_route_configs: Vec<HttpRouteConfig> = http_routes
        .iter()
        .filter(|r| route_targets_gateway(&r.spec.parent_refs, gateway_name))
        .filter_map(|r| convert_http_route(r, gateway_name).ok())
        .collect();

    let tcp_route_configs: Vec<TcpRouteConfig> = tcp_routes
        .iter()
        .filter(|r| route_targets_gateway(&r.spec.parent_refs, gateway_name))
        .filter_map(|r| convert_tcp_route(r, gateway_name).ok())
        .collect();

    let tls_route_configs: Vec<TlsRouteConfig> = tls_routes
        .iter()
        .filter(|r| route_targets_gateway(&r.spec.parent_refs, gateway_name))
        .filter_map(|r| convert_tls_route(r, gateway_name).ok())
        .collect();

    let udp_route_configs: Vec<UdpRouteConfig> = udp_routes
        .iter()
        .filter(|r| route_targets_gateway(&r.spec.parent_refs, gateway_name))
        .filter_map(|r| convert_udp_route(r, gateway_name).ok())
        .collect();

    Ok(UringRessConfig {
        gateway: gateway_config,
        http_routes: http_route_configs,
        tcp_routes: tcp_route_configs,
        tls_routes: tls_route_configs,
        udp_routes: udp_route_configs,
        ..Default::default()
    })
}

fn convert_gateway(gw: &Gateway) -> Result<GatewayConfig> {
    let name = gw
        .metadata
        .name
        .clone()
        .unwrap_or_else(|| "uringress-gateway".to_string());

    let listeners = gw
        .spec
        .listeners
        .iter()
        .map(convert_listener)
        .collect::<Result<Vec<_>>>()?;

    Ok(GatewayConfig {
        name,
        listeners,
        ..Default::default()
    })
}

fn convert_listener(l: &GatewayListeners) -> Result<ListenerConfig> {
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
            .map(|refs| refs.iter().map(|r| CertificateRef { name: r.name.clone() }).collect())
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

/// Check if any parentRef in the route targets the given gateway name.
fn route_targets_gateway<T: ParentRefAccess>(parent_refs: &Option<Vec<T>>, gateway_name: &str) -> bool {
    parent_refs
        .as_ref()
        .map(|refs| refs.iter().any(|pr| pr.ref_name() == gateway_name))
        .unwrap_or(false)
}

/// Trait to abstract over the different per-route ParentRef types.
trait ParentRefAccess {
    fn ref_name(&self) -> &str;
    fn ref_section_name(&self) -> Option<&str>;
}

impl ParentRefAccess for HTTPRouteParentRefs {
    fn ref_name(&self) -> &str { &self.name }
    fn ref_section_name(&self) -> Option<&str> { self.section_name.as_deref() }
}

impl ParentRefAccess for TCPRouteParentRefs {
    fn ref_name(&self) -> &str { &self.name }
    fn ref_section_name(&self) -> Option<&str> { self.section_name.as_deref() }
}

impl ParentRefAccess for TLSRouteParentRefs {
    fn ref_name(&self) -> &str { &self.name }
    fn ref_section_name(&self) -> Option<&str> { self.section_name.as_deref() }
}

impl ParentRefAccess for UDPRouteParentRefs {
    fn ref_name(&self) -> &str { &self.name }
    fn ref_section_name(&self) -> Option<&str> { self.section_name.as_deref() }
}

fn extract_parent_refs<T: ParentRefAccess>(
    parent_refs: &Option<Vec<T>>,
    gateway_name: &str,
) -> Vec<ParentRef> {
    parent_refs
        .as_ref()
        .map(|refs| {
            refs.iter()
                .filter(|pr| pr.ref_name() == gateway_name)
                .map(|pr| ParentRef {
                    name: pr.ref_name().to_string(),
                    section_name: pr.ref_section_name().map(String::from),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Format backend name as `{service}.{namespace}.svc` for DNS resolution.
fn backend_dns_name(name: &str, namespace: Option<&str>, route_namespace: &str) -> String {
    let ns = namespace.unwrap_or(route_namespace);
    format!("{}.{}.svc", name, ns)
}

fn route_namespace(meta: &kube::core::ObjectMeta) -> &str {
    meta.namespace.as_deref().unwrap_or("default")
}

fn convert_http_route(route: &HTTPRoute, gateway_name: &str) -> Result<HttpRouteConfig> {
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

fn convert_http_rule(rule: &HTTPRouteRules, ns: &str) -> Result<HttpRouteRule> {
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

fn convert_http_match(m: &HTTPRouteRulesMatches) -> HttpRouteMatch {
    let method = m.method.as_ref().map(|m| format!("{:?}", m));

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

fn convert_http_filter(f: &HTTPRouteRulesFilters, ns: &str) -> Result<HttpRouteFilter> {
    match f.r#type {
        HTTPRouteRulesFiltersType::RequestHeaderModifier => {
            let hm = f
                .request_header_modifier
                .as_ref()
                .ok_or_else(|| anyhow!("RequestHeaderModifier filter missing config"))?;
            Ok(HttpRouteFilter::RequestHeaderModifier {
                config: convert_header_modifier_config(hm),
            })
        }
        HTTPRouteRulesFiltersType::ResponseHeaderModifier => {
            let hm = f
                .response_header_modifier
                .as_ref()
                .ok_or_else(|| anyhow!("ResponseHeaderModifier filter missing config"))?;
            Ok(HttpRouteFilter::ResponseHeaderModifier {
                config: convert_response_header_modifier_config(hm),
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

fn convert_header_modifier_config(
    hm: &HTTPRouteRulesFiltersRequestHeaderModifier,
) -> HeaderModifierConfig {
    HeaderModifierConfig {
        add: hm
            .add
            .as_ref()
            .map(|v| v.iter().map(|h| HttpHeader { name: h.name.clone(), value: h.value.clone() }).collect())
            .unwrap_or_default(),
        set: hm
            .set
            .as_ref()
            .map(|v| v.iter().map(|h| HttpHeader { name: h.name.clone(), value: h.value.clone() }).collect())
            .unwrap_or_default(),
        remove: hm.remove.clone().unwrap_or_default(),
    }
}

fn convert_response_header_modifier_config(
    hm: &HTTPRouteRulesFiltersResponseHeaderModifier,
) -> HeaderModifierConfig {
    HeaderModifierConfig {
        add: hm
            .add
            .as_ref()
            .map(|v| v.iter().map(|h| HttpHeader { name: h.name.clone(), value: h.value.clone() }).collect())
            .unwrap_or_default(),
        set: hm
            .set
            .as_ref()
            .map(|v| v.iter().map(|h| HttpHeader { name: h.name.clone(), value: h.value.clone() }).collect())
            .unwrap_or_default(),
        remove: hm.remove.clone().unwrap_or_default(),
    }
}

fn convert_redirect_config(rr: &HTTPRouteRulesFiltersRequestRedirect) -> RequestRedirectConfig {
    let scheme = rr.scheme.as_ref().map(|s| match s {
        HTTPRouteRulesFiltersRequestRedirectScheme::Http => "http".to_string(),
        HTTPRouteRulesFiltersRequestRedirectScheme::Https => "https".to_string(),
    });

    let path = rr.path.as_ref().and_then(|p| convert_path_modifier(p));

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
        },
    }
}

fn convert_http_backend_ref(br: &HTTPRouteRulesBackendRefs, ns: &str) -> BackendRef {
    BackendRef {
        name: backend_dns_name(&br.name, br.namespace.as_deref(), ns),
        port: br.port.unwrap_or(80) as u16,
        weight: br.weight.unwrap_or(1) as u32,
    }
}

fn convert_timeouts(t: &HTTPRouteRulesTimeouts) -> HttpRouteTimeouts {
    HttpRouteTimeouts {
        request: t.request.as_ref().and_then(|s| parse_gateway_duration(s)),
        backend_request: t.backend_request.as_ref().and_then(|s| parse_gateway_duration(s)),
    }
}

/// Parse Gateway API duration string (e.g. "10s", "500ms", "1m").
fn parse_gateway_duration(s: &str) -> Option<std::time::Duration> {
    crate::config::parsing::parse_duration(s).ok()
}

fn convert_tcp_route(route: &TCPRoute, gateway_name: &str) -> Result<TcpRouteConfig> {
    let ns = route_namespace(&route.metadata);
    let parent_refs = extract_parent_refs(&route.spec.parent_refs, gateway_name);

    let rules = route
        .spec
        .rules
        .iter()
        .map(|r| {
            let backend_refs = r
                .backend_refs
                .iter()
                .map(|br| BackendRef {
                    name: backend_dns_name(&br.name, br.namespace.as_deref(), ns),
                    port: br.port.unwrap_or(80) as u16,
                    weight: br.weight.unwrap_or(1) as u32,
                })
                .collect();
            TcpRouteRule { backend_refs }
        })
        .collect();

    Ok(TcpRouteConfig {
        parent_refs,
        rules,
    })
}

fn convert_tls_route(route: &TLSRoute, gateway_name: &str) -> Result<TlsRouteConfig> {
    let ns = route_namespace(&route.metadata);
    let parent_refs = extract_parent_refs(&route.spec.parent_refs, gateway_name);
    let hostnames = route.spec.hostnames.clone();

    let rules = route
        .spec
        .rules
        .iter()
        .map(|r| {
            let backend_refs = r
                .backend_refs
                .iter()
                .map(|br| BackendRef {
                    name: backend_dns_name(&br.name, br.namespace.as_deref(), ns),
                    port: br.port.unwrap_or(443) as u16,
                    weight: br.weight.unwrap_or(1) as u32,
                })
                .collect();
            TlsRouteRule { backend_refs }
        })
        .collect();

    Ok(TlsRouteConfig {
        parent_refs,
        hostnames,
        rules,
    })
}

fn convert_udp_route(route: &UDPRoute, gateway_name: &str) -> Result<UdpRouteConfig> {
    let ns = route_namespace(&route.metadata);
    let parent_refs = extract_parent_refs(&route.spec.parent_refs, gateway_name);

    let rules = route
        .spec
        .rules
        .iter()
        .map(|r| {
            let backend_refs = r
                .backend_refs
                .iter()
                .map(|br| BackendRef {
                    name: backend_dns_name(&br.name, br.namespace.as_deref(), ns),
                    port: br.port.unwrap_or(80) as u16,
                    weight: br.weight.unwrap_or(1) as u32,
                })
                .collect();
            UdpRouteRule { backend_refs }
        })
        .collect();

    Ok(UdpRouteConfig {
        parent_refs,
        rules,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway_api::gateways::*;
    use kube::core::ObjectMeta;

    fn test_gateway() -> Gateway {
        Gateway {
            metadata: ObjectMeta {
                name: Some("test-gw".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: GatewaySpec {
                gateway_class_name: "uringress".to_string(),
                listeners: vec![GatewayListeners {
                    name: "http".to_string(),
                    port: 8080,
                    protocol: "HTTP".to_string(),
                    hostname: Some("*.example.com".to_string()),
                    tls: None,
                    allowed_routes: None,
                }],
                ..Default::default()
            },
            status: None,
        }
    }

    fn test_http_route() -> HTTPRoute {
        HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: HTTPRouteSpec {
                parent_refs: Some(vec![HTTPRouteParentRefs {
                    name: "test-gw".to_string(),
                    section_name: Some("http".to_string()),
                    ..Default::default()
                }]),
                hostnames: Some(vec!["api.example.com".to_string()]),
                rules: Some(vec![HTTPRouteRules {
                    matches: Some(vec![HTTPRouteRulesMatches {
                        path: Some(HTTPRouteRulesMatchesPath {
                            r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                            value: Some("/v1".to_string()),
                        }),
                        ..Default::default()
                    }]),
                    backend_refs: Some(vec![HTTPRouteRulesBackendRefs {
                        name: "api-svc".to_string(),
                        port: Some(3000),
                        weight: Some(1),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
            },
            status: None,
        }
    }

    #[test]
    fn test_reconcile_basic() {
        let gw = test_gateway();
        let route = test_http_route();

        let config =
            reconcile_to_config(&gw, &[route], &[], &[], &[]).unwrap();

        assert_eq!(config.gateway.name, "test-gw");
        assert_eq!(config.gateway.listeners.len(), 1);
        assert_eq!(config.gateway.listeners[0].port, 8080);
        assert_eq!(config.http_routes.len(), 1);
        assert_eq!(config.http_routes[0].hostnames, vec!["api.example.com"]);
        assert_eq!(config.http_routes[0].rules[0].backend_refs[0].name, "api-svc.default.svc");
        assert_eq!(config.http_routes[0].rules[0].backend_refs[0].port, 3000);
    }

    #[test]
    fn test_reconcile_filters_routes_by_gateway() {
        let gw = test_gateway();
        let matching_route = test_http_route();

        // Route targeting a different gateway
        let other_route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("other-route".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: HTTPRouteSpec {
                parent_refs: Some(vec![HTTPRouteParentRefs {
                    name: "other-gw".to_string(),
                    ..Default::default()
                }]),
                hostnames: Some(vec!["other.com".to_string()]),
                rules: Some(vec![HTTPRouteRules {
                    backend_refs: Some(vec![HTTPRouteRulesBackendRefs {
                        name: "other-svc".to_string(),
                        port: Some(80),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
            },
            status: None,
        };

        let config = reconcile_to_config(
            &gw,
            &[matching_route, other_route],
            &[],
            &[],
            &[],
        )
        .unwrap();

        assert_eq!(config.http_routes.len(), 1);
        assert_eq!(config.http_routes[0].hostnames, vec!["api.example.com"]);
    }

    #[test]
    fn test_reconcile_produces_valid_config() {
        let gw = test_gateway();
        let route = test_http_route();

        let config =
            reconcile_to_config(&gw, &[route], &[], &[], &[]).unwrap();

        // Verify the config structure is correct
        assert_eq!(config.gateway.listeners[0].protocol, Protocol::HTTP);
        assert_eq!(config.http_routes[0].rules.len(), 1);
        let rule = &config.http_routes[0].rules[0];
        assert_eq!(rule.matches.len(), 1);
        assert_eq!(
            rule.matches[0].path.as_ref().unwrap().match_type,
            HttpPathMatchType::PathPrefix
        );
        assert_eq!(rule.matches[0].path.as_ref().unwrap().value, "/v1");
        // Backend uses DNS name format for in-cluster resolution
        assert_eq!(rule.backend_refs[0].name, "api-svc.default.svc");
    }

    #[test]
    fn test_backend_dns_name_with_namespace() {
        assert_eq!(
            backend_dns_name("my-svc", Some("prod"), "default"),
            "my-svc.prod.svc"
        );
        assert_eq!(
            backend_dns_name("my-svc", None, "default"),
            "my-svc.default.svc"
        );
    }
}
