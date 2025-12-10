use std::collections::{BTreeMap, HashMap, HashSet};
use anyhow::{anyhow, Result};

use gateway_api::gateways::{
    Gateway, GatewayListeners, GatewayListenersTlsMode,
    GatewayListenersAllowedRoutesNamespacesFrom,
};
use gateway_api::httproutes::*;
use gateway_api::referencegrants::ReferenceGrant;
use gateway_api::experimental::tcproutes::*;
use gateway_api::experimental::tlsroutes::*;
use gateway_api::experimental::udproutes::*;

use crate::config::*;

pub struct ReconcileResult {
    pub config: PortailConfig,
    pub route_status: Vec<RouteAcceptance>,
}

pub struct RouteAcceptance {
    pub name: String,
    pub namespace: String,
    pub kind: &'static str,
    pub accepted: bool,
    pub accepted_reason: String,
    pub message: String,
    pub refs_resolved: bool,
    pub refs_reason: String,
    pub refs_message: String,
    pub generation: Option<i64>,
    pub section_name: Option<String>,
}

/// Check if a route in `route_ns` is allowed by the listener's allowedRoutes policy.
fn is_route_allowed_by_listener(
    listener: &GatewayListeners,
    gateway_ns: &str,
    route_ns: &str,
    route_kind: &str,
    namespace_labels: &HashMap<String, BTreeMap<String, String>>,
) -> bool {
    // Check protocol ↔ route kind enforcement
    let protocol_allows = match listener.protocol.as_str() {
        "HTTP" | "HTTPS" => route_kind == "HTTPRoute",
        "TLS" => route_kind == "TLSRoute",
        "TCP" => route_kind == "TCPRoute",
        "UDP" => route_kind == "UDPRoute",
        _ => true, // Unknown protocol, allow anything
    };
    if !protocol_allows {
        return false;
    }

    // Check allowedRoutes.kinds if specified
    if let Some(ref allowed) = listener.allowed_routes {
        if let Some(ref kinds) = allowed.kinds {
            if !kinds.is_empty() {
                let kind_allowed = kinds.iter().any(|k| {
                    k.kind == route_kind
                });
                if !kind_allowed {
                    return false;
                }
            }
        }
    }
    let allowed = match &listener.allowed_routes {
        None => return route_ns == gateway_ns,
        Some(ar) => ar,
    };
    let namespaces = match &allowed.namespaces {
        None => return route_ns == gateway_ns,
        Some(ns) => ns,
    };
    match &namespaces.from {
        None | Some(GatewayListenersAllowedRoutesNamespacesFrom::Same) => {
            route_ns == gateway_ns
        }
        Some(GatewayListenersAllowedRoutesNamespacesFrom::All) => true,
        Some(GatewayListenersAllowedRoutesNamespacesFrom::Selector) => {
            let labels = match namespace_labels.get(route_ns) {
                Some(l) => l,
                None => return false,
            };
            let selector = match &namespaces.selector {
                Some(s) => s,
                None => return true,
            };
            // matchLabels: every key=value must be present
            if let Some(match_labels) = &selector.match_labels {
                for (k, v) in match_labels {
                    if labels.get(k) != Some(v) {
                        return false;
                    }
                }
            }
            // matchExpressions: each expression must match
            if let Some(exprs) = &selector.match_expressions {
                for expr in exprs {
                    let has = labels.get(&expr.key);
                    let vals = expr.values.as_deref().unwrap_or_default();
                    match expr.operator.as_str() {
                        "In" => {
                            if !has.map_or(false, |v| vals.contains(v)) {
                                return false;
                            }
                        }
                        "NotIn" => {
                            if has.map_or(false, |v| vals.contains(v)) {
                                return false;
                            }
                        }
                        "Exists" => {
                            if has.is_none() {
                                return false;
                            }
                        }
                        "DoesNotExist" => {
                            if has.is_some() {
                                return false;
                            }
                        }
                        _ => return false,
                    }
                }
            }
            true
        }
    }
}

/// Check if a parentRef matches this gateway (name + group + kind + namespace).
fn parent_ref_matches_gateway<T: ParentRefAccess>(pr: &T, gateway_name: &str, gateway_ns: &str) -> bool {
    if pr.ref_name() != gateway_name {
        return false;
    }
    // group must be "gateway.networking.k8s.io" or unset
    if let Some(group) = pr.ref_group() {
        if group != "gateway.networking.k8s.io" {
            return false;
        }
    }
    // kind must be "Gateway" or unset
    if let Some(kind) = pr.ref_kind() {
        if kind != "Gateway" {
            return false;
        }
    }
    // namespace must match the gateway's namespace (or be unset = route's own ns)
    if let Some(ns) = pr.ref_namespace() {
        if ns != gateway_ns {
            return false;
        }
    }
    true
}

/// Check if a listener hostname and route hostnames have at least one overlap.
/// Per Gateway API spec:
/// - No listener hostname → all routes match
/// - No route hostnames → matches any listener
/// - Wildcard: listener `*.example.com` matches route `foo.example.com`
fn hostnames_intersect(listener_hostname: Option<&str>, route_hostnames: &[String]) -> bool {
    let listener_hn = match listener_hostname {
        None => return true, // No listener hostname restriction
        Some(h) => h,
    };
    if route_hostnames.is_empty() {
        return true; // No route hostname restriction
    }
    route_hostnames.iter().any(|route_hn| {
        hostname_matches(listener_hn, route_hn) || hostname_matches(route_hn, listener_hn)
    })
}

/// Check if `pattern` matches `candidate`.
/// Supports wildcard prefixes: `*.example.com` matches `foo.example.com`.
fn hostname_matches(pattern: &str, candidate: &str) -> bool {
    if pattern == candidate {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix("*.") {
        // candidate must be in a subdomain of suffix
        if let Some(cand_rest) = candidate.strip_suffix(suffix) {
            // cand_rest must end with '.' and have at least one char before it
            // e.g. "foo." for "foo.example.com" matching "*.example.com"
            return cand_rest.ends_with('.') && cand_rest.len() > 1;
        }
        // Also match exact suffix (*.example.com matches example.com per spec is actually NOT a match)
        // Per spec, *.example.com does NOT match example.com itself, only subdomains
    }
    false
}

/// Check if a route targets a specific listener by sectionName, and if so, whether
/// the listener's namespace/hostname/kind policy allows the route.
fn route_allowed_for_listener<T: ParentRefAccess>(
    parent_refs: &Option<Vec<T>>,
    gateway_name: &str,
    listener: &GatewayListeners,
    gateway_ns: &str,
    route_ns: &str,
    route_kind: &str,
    route_hostnames: &[String],
    namespace_labels: &HashMap<String, BTreeMap<String, String>>,
) -> bool {
    let refs = match parent_refs.as_ref() {
        Some(r) => r,
        None => return false,
    };
    refs.iter().any(|pr| {
        if !parent_ref_matches_gateway(pr, gateway_name, gateway_ns) {
            return false;
        }
        // If sectionName is specified, it must match this listener
        if let Some(section) = pr.ref_section_name() {
            if section != listener.name {
                return false;
            }
        }
        // If port is specified, it must match this listener's port
        if let Some(port) = pr.ref_port() {
            if port != listener.port as i32 {
                return false;
            }
        }
        // Check hostname intersection (only for HTTP/HTTPS/TLS listeners)
        if !hostnames_intersect(listener.hostname.as_deref(), route_hostnames) {
            return false;
        }
        is_route_allowed_by_listener(listener, gateway_ns, route_ns, route_kind, namespace_labels)
    })
}

/// Convert Kubernetes Gateway API resources into a PortailConfig.
/// This reuses all existing validation, conversion, hostname intersection,
/// and regex compilation logic via `to_route_table()`.
pub fn reconcile_to_config(
    gateway: &Gateway,
    http_routes: &[HTTPRoute],
    tcp_routes: &[TCPRoute],
    tls_routes: &[TLSRoute],
    udp_routes: &[UDPRoute],
    namespace_labels: &HashMap<String, BTreeMap<String, String>>,
    reference_grants: &[ReferenceGrant],
    known_services: &HashSet<(String, String)>,
    cert_data: &HashMap<(String, String), (Vec<u8>, Vec<u8>)>,
) -> Result<ReconcileResult> {
    let gateway_config = convert_gateway(gateway, cert_data)?;
    let gateway_name = gateway.metadata.name.as_deref().unwrap_or("default");
    let gateway_ns = gateway.metadata.namespace.as_deref().unwrap_or("default");

    let mut route_status = Vec::new();

    let http_route_configs = collect_routes(
        http_routes,
        gateway_name,
        gateway_ns,
        &gateway.spec.listeners,
        namespace_labels,
        reference_grants,
        known_services,
        "HTTPRoute",
        |r| &r.spec.parent_refs,
        |r| route_namespace(&r.metadata),
        |r| (r.metadata.name.as_deref().unwrap_or("unknown"), r.metadata.generation),
        |r| convert_http_route(r, gateway_name),
        |r| all_section_names_for_gateway(&r.spec.parent_refs, gateway_name, gateway_ns),
        |r| r.spec.hostnames.clone().unwrap_or_default(),
        |c| c.rules.iter().flat_map(|r| r.backend_refs.iter().map(|b| (b.name.as_str(), b.port, b.group.as_str(), b.kind.as_str()))).collect(),
        &mut route_status,
    );

    let tcp_route_configs = collect_routes(
        tcp_routes,
        gateway_name,
        gateway_ns,
        &gateway.spec.listeners,
        namespace_labels,
        reference_grants,
        known_services,
        "TCPRoute",
        |r| &r.spec.parent_refs,
        |r| route_namespace(&r.metadata),
        |r| (r.metadata.name.as_deref().unwrap_or("unknown"), r.metadata.generation),
        |r| convert_tcp_route(r, gateway_name),
        |r| all_section_names_for_gateway(&r.spec.parent_refs, gateway_name, gateway_ns),
        |_r| vec![],  // TCPRoute has no hostname matching
        |c| c.rules.iter().flat_map(|r| r.backend_refs.iter().map(|b| (b.name.as_str(), b.port, b.group.as_str(), b.kind.as_str()))).collect(),
        &mut route_status,
    );

    let tls_route_configs = collect_routes(
        tls_routes,
        gateway_name,
        gateway_ns,
        &gateway.spec.listeners,
        namespace_labels,
        reference_grants,
        known_services,
        "TLSRoute",
        |r| &r.spec.parent_refs,
        |r| route_namespace(&r.metadata),
        |r| (r.metadata.name.as_deref().unwrap_or("unknown"), r.metadata.generation),
        |r| convert_tls_route(r, gateway_name),
        |r| all_section_names_for_gateway(&r.spec.parent_refs, gateway_name, gateway_ns),
        |r| r.spec.hostnames.clone(),
        |c| c.rules.iter().flat_map(|r| r.backend_refs.iter().map(|b| (b.name.as_str(), b.port, b.group.as_str(), b.kind.as_str()))).collect(),
        &mut route_status,
    );

    let udp_route_configs = collect_routes(
        udp_routes,
        gateway_name,
        gateway_ns,
        &gateway.spec.listeners,
        namespace_labels,
        reference_grants,
        known_services,
        "UDPRoute",
        |r| &r.spec.parent_refs,
        |r| route_namespace(&r.metadata),
        |r| (r.metadata.name.as_deref().unwrap_or("unknown"), r.metadata.generation),
        |r| convert_udp_route(r, gateway_name),
        |r| all_section_names_for_gateway(&r.spec.parent_refs, gateway_name, gateway_ns),
        |_r| vec![],  // UDPRoute has no hostname matching
        |c| c.rules.iter().flat_map(|r| r.backend_refs.iter().map(|b| (b.name.as_str(), b.port, b.group.as_str(), b.kind.as_str()))).collect(),
        &mut route_status,
    );

    Ok(ReconcileResult {
        config: PortailConfig {
            gateway: gateway_config,
            http_routes: http_route_configs,
            tcp_routes: tcp_route_configs,
            tls_routes: tls_route_configs,
            udp_routes: udp_route_configs,
            ..Default::default()
        },
        route_status,
    })
}

/// Generic route collection with namespace scoping and acceptance tracking.
fn collect_routes<R, P, C>(
    routes: &[R],
    gateway_name: &str,
    gateway_ns: &str,
    listeners: &[GatewayListeners],
    namespace_labels: &HashMap<String, BTreeMap<String, String>>,
    reference_grants: &[ReferenceGrant],
    known_services: &HashSet<(String, String)>,
    kind: &'static str,
    get_parent_refs: impl Fn(&R) -> &Option<Vec<P>>,
    get_namespace: impl Fn(&R) -> &str,
    get_identity: impl Fn(&R) -> (&str, Option<i64>),
    convert: impl Fn(&R) -> Result<C>,
    get_parent_section_names: impl Fn(&R) -> Vec<Option<String>>,
    get_hostnames: impl Fn(&R) -> Vec<String>,
    get_backend_refs: impl Fn(&C) -> Vec<(&str, u16, &str, &str)>,
    route_status: &mut Vec<RouteAcceptance>,
) -> Vec<C>
where
    P: ParentRefAccess,
{
    let mut configs = Vec::new();
    for route in routes {
        if !route_targets_gateway(get_parent_refs(route), gateway_name, gateway_ns) {
            continue;
        }

        let route_ns = get_namespace(route);
        let (name, generation) = get_identity(route);
        let section_names = get_parent_section_names(route);

        // Check namespace + hostname scoping against at least one listener
        let route_hostnames = get_hostnames(route);
        let allowed = listeners.iter().any(|l| {
            route_allowed_for_listener(
                get_parent_refs(route),
                gateway_name,
                l,
                gateway_ns,
                route_ns,
                kind,
                &route_hostnames,
                namespace_labels,
            )
        });

        if !allowed {
            for section_name in &section_names {
                route_status.push(RouteAcceptance {
                    name: name.to_string(),
                    namespace: route_ns.to_string(),
                    kind,
                    accepted: false,
                    accepted_reason: "NotAllowedByListeners".to_string(),
                    message: "Route not allowed by any listener policy (namespace/hostname/kind)".to_string(),
                    refs_resolved: true,
                    refs_reason: "ResolvedRefs".to_string(),
                    refs_message: "All references resolved".to_string(),
                    generation,
                    section_name: section_name.clone(),
                });
            }
            continue;
        }

        match convert(route) {
            Ok(config) => {
                let mut refs_resolved = true;
                let mut refs_messages = Vec::new();
                let mut refs_reason = "ResolvedRefs".to_string();

                // Validate backend refs: check group/kind, cross-namespace grants, and service existence
                for (backend_name, _port, group, ref_kind) in get_backend_refs(&config) {
                    // Check group/kind — must be core ("") group and "Service" kind (or defaults)
                    if (group != "" && group != "core") || (ref_kind != "Service" && ref_kind != "") {
                        refs_resolved = false;
                        refs_reason = "InvalidKind".to_string();
                        refs_messages.push(format!(
                            "Unsupported backend ref group/kind: {}/{}", group, ref_kind
                        ));
                        continue;
                    }

                    // backend_name is "{svc}.{ns}.svc"
                    if let Some((svc_name, svc_ns)) = parse_backend_dns_name(backend_name) {
                        // Cross-namespace check
                        if svc_ns != route_ns && !is_reference_allowed(
                            reference_grants,
                            "gateway.networking.k8s.io", kind, route_ns,
                            "", "Service", &svc_ns, &svc_name,
                        ) {
                            refs_resolved = false;
                            refs_reason = "RefNotPermitted".to_string();
                            refs_messages.push(format!(
                                "Cross-namespace reference to {}.{} not allowed by ReferenceGrant",
                                svc_name, svc_ns
                            ));
                            continue;
                        }
                        // Service existence check
                        if !known_services.contains(&(svc_name.clone(), svc_ns.clone())) {
                            refs_resolved = false;
                            refs_reason = "BackendNotFound".to_string();
                            refs_messages.push(format!(
                                "Backend service {}.{} not found", svc_name, svc_ns
                            ));
                        }
                    }
                }

                let refs_message = if refs_resolved {
                    "All references resolved".to_string()
                } else {
                    refs_messages.join("; ")
                };

                for section_name in &section_names {
                    route_status.push(RouteAcceptance {
                        name: name.to_string(),
                        namespace: route_ns.to_string(),
                        kind,
                        accepted: true,
                        accepted_reason: "Accepted".to_string(),
                        message: "Accepted".to_string(),
                        refs_resolved,
                        refs_reason: refs_reason.clone(),
                        refs_message: refs_message.clone(),
                        generation,
                        section_name: section_name.clone(),
                    });
                }
                configs.push(config);
            }
            Err(e) => {
                for section_name in &section_names {
                    route_status.push(RouteAcceptance {
                        name: name.to_string(),
                        namespace: route_ns.to_string(),
                        kind,
                        accepted: false,
                        accepted_reason: "InvalidRoute".to_string(),
                        message: format!("Conversion failed: {}", e),
                        refs_resolved: false,
                        refs_reason: "InvalidKind".to_string(),
                        refs_message: format!("Conversion failed: {}", e),
                        generation,
                        section_name: section_name.clone(),
                    });
                }
            }
        }
    }
    configs
}

fn all_section_names_for_gateway<T: ParentRefAccess>(
    parent_refs: &Option<Vec<T>>,
    gateway_name: &str,
    gateway_ns: &str,
) -> Vec<Option<String>> {
    parent_refs.as_ref()
        .map(|refs| {
            refs.iter()
                .filter(|pr| parent_ref_matches_gateway(*pr, gateway_name, gateway_ns))
                .map(|pr| pr.ref_section_name().map(String::from))
                .collect()
        })
        .unwrap_or_else(|| vec![None])
}

fn convert_gateway(gw: &Gateway, cert_data: &HashMap<(String, String), (Vec<u8>, Vec<u8>)>) -> Result<GatewayConfig> {
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

fn convert_listener(l: &GatewayListeners, gw_ns: &str, cert_data: &HashMap<(String, String), (Vec<u8>, Vec<u8>)>) -> Result<ListenerConfig> {
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
                CertificateRef { name: r.name.clone(), cert_pem, key_pem }
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

/// Check if any parentRef in the route targets the given gateway.
fn route_targets_gateway<T: ParentRefAccess>(parent_refs: &Option<Vec<T>>, gateway_name: &str, gateway_ns: &str) -> bool {
    parent_refs
        .as_ref()
        .map(|refs| refs.iter().any(|pr| parent_ref_matches_gateway(pr, gateway_name, gateway_ns)))
        .unwrap_or(false)
}

/// Trait to abstract over the different per-route ParentRef types.
trait ParentRefAccess {
    fn ref_name(&self) -> &str;
    fn ref_section_name(&self) -> Option<&str>;
    fn ref_group(&self) -> Option<&str>;
    fn ref_kind(&self) -> Option<&str>;
    fn ref_namespace(&self) -> Option<&str>;
    fn ref_port(&self) -> Option<i32>;
}

impl ParentRefAccess for HTTPRouteParentRefs {
    fn ref_name(&self) -> &str { &self.name }
    fn ref_section_name(&self) -> Option<&str> { self.section_name.as_deref() }
    fn ref_group(&self) -> Option<&str> { self.group.as_deref() }
    fn ref_kind(&self) -> Option<&str> { self.kind.as_deref() }
    fn ref_namespace(&self) -> Option<&str> { self.namespace.as_deref() }
    fn ref_port(&self) -> Option<i32> { self.port }
}

impl ParentRefAccess for TCPRouteParentRefs {
    fn ref_name(&self) -> &str { &self.name }
    fn ref_section_name(&self) -> Option<&str> { self.section_name.as_deref() }
    fn ref_group(&self) -> Option<&str> { self.group.as_deref() }
    fn ref_kind(&self) -> Option<&str> { self.kind.as_deref() }
    fn ref_namespace(&self) -> Option<&str> { self.namespace.as_deref() }
    fn ref_port(&self) -> Option<i32> { self.port }
}

impl ParentRefAccess for TLSRouteParentRefs {
    fn ref_name(&self) -> &str { &self.name }
    fn ref_section_name(&self) -> Option<&str> { self.section_name.as_deref() }
    fn ref_group(&self) -> Option<&str> { self.group.as_deref() }
    fn ref_kind(&self) -> Option<&str> { self.kind.as_deref() }
    fn ref_namespace(&self) -> Option<&str> { self.namespace.as_deref() }
    fn ref_port(&self) -> Option<i32> { self.port }
}

impl ParentRefAccess for UDPRouteParentRefs {
    fn ref_name(&self) -> &str { &self.name }
    fn ref_section_name(&self) -> Option<&str> { self.section_name.as_deref() }
    fn ref_group(&self) -> Option<&str> { self.group.as_deref() }
    fn ref_kind(&self) -> Option<&str> { self.kind.as_deref() }
    fn ref_namespace(&self) -> Option<&str> { self.namespace.as_deref() }
    fn ref_port(&self) -> Option<i32> { self.port }
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

/// Parse a DNS-style backend name back into (service_name, namespace).
fn parse_backend_dns_name(dns_name: &str) -> Option<(String, String)> {
    let stripped = dns_name.strip_suffix(".svc")?;
    let (svc_name, ns) = stripped.rsplit_once('.')?;
    Some((svc_name.to_string(), ns.to_string()))
}

/// Check if a cross-namespace reference is allowed by a ReferenceGrant.
/// Same-namespace references are always allowed.
fn is_reference_allowed(
    grants: &[ReferenceGrant],
    from_group: &str,
    from_kind: &str,
    from_namespace: &str,
    to_group: &str,
    to_kind: &str,
    to_namespace: &str,
    to_name: &str,
) -> bool {
    if from_namespace == to_namespace {
        return true;
    }

    // Scan grants that live in the target namespace
    for grant in grants {
        let grant_ns = grant.metadata.namespace.as_deref().unwrap_or("default");
        if grant_ns != to_namespace {
            continue;
        }

        let from_match = grant.spec.from.iter().any(|f| {
            f.group == from_group && f.kind == from_kind && f.namespace == from_namespace
        });

        let to_match = grant.spec.to.iter().any(|t| {
            t.group == to_group
                && t.kind == to_kind
                && t.name.as_ref().is_none_or(|n| n == to_name)
        });

        if from_match && to_match {
            return true;
        }
    }

    false
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
        },
    }
}

fn convert_http_backend_ref(br: &HTTPRouteRulesBackendRefs, ns: &str) -> BackendRef {
    BackendRef {
        name: backend_dns_name(&br.name, br.namespace.as_deref(), ns),
        port: br.port.unwrap_or(80) as u16,
        weight: br.weight.unwrap_or(1) as u32,
        group: br.group.clone().unwrap_or_default(),
        kind: br.kind.clone().unwrap_or_else(|| "Service".to_string()),
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
                    group: br.group.clone().unwrap_or_default(),
                    kind: br.kind.clone().unwrap_or_else(|| "Service".to_string()),
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
                    group: br.group.clone().unwrap_or_default(),
                    kind: br.kind.clone().unwrap_or_else(|| "Service".to_string()),
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
                    group: br.group.clone().unwrap_or_default(),
                    kind: br.kind.clone().unwrap_or_else(|| "Service".to_string()),
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

    fn default_ns_labels() -> HashMap<String, BTreeMap<String, String>> {
        let mut m = HashMap::new();
        m.insert("default".to_string(), BTreeMap::new());
        m
    }

    /// Replace DNS-style backend names (e.g. "127.0.0.1.default.svc") with raw
    /// IPs so to_route_table() can resolve them without real DNS.
    fn fixup_backends_for_test(config: &mut crate::config::PortailConfig) {
        fn strip_svc_suffix(name: &str) -> String {
            // "127.0.0.1.default.svc" → "127.0.0.1"
            name.strip_suffix(".svc")
                .and_then(|s| s.rsplit_once('.'))
                .map(|(ip, _ns)| ip.to_string())
                .unwrap_or_else(|| name.to_string())
        }
        for route in &mut config.http_routes {
            for rule in &mut route.rules {
                for b in &mut rule.backend_refs {
                    b.name = strip_svc_suffix(&b.name);
                }
                for f in &mut rule.filters {
                    if let crate::config::HttpRouteFilter::RequestMirror { config: ref mut mc } = f {
                        mc.backend_ref.name = strip_svc_suffix(&mc.backend_ref.name);
                    }
                }
            }
        }
        for route in &mut config.tcp_routes {
            for rule in &mut route.rules {
                for b in &mut rule.backend_refs {
                    b.name = strip_svc_suffix(&b.name);
                }
            }
        }
        for route in &mut config.udp_routes {
            for rule in &mut route.rules {
                for b in &mut rule.backend_refs {
                    b.name = strip_svc_suffix(&b.name);
                }
            }
        }
        for route in &mut config.tls_routes {
            for rule in &mut route.rules {
                for b in &mut rule.backend_refs {
                    b.name = strip_svc_suffix(&b.name);
                }
            }
        }
    }

    fn test_gateway() -> Gateway {
        Gateway {
            metadata: ObjectMeta {
                name: Some("test-gw".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: GatewaySpec {
                gateway_class_name: "portail".to_string(),
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
        let ns_labels = default_ns_labels();

        let result =
            reconcile_to_config(&gw, &[route], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();

        assert_eq!(result.config.gateway.name, "test-gw");
        assert_eq!(result.config.gateway.listeners.len(), 1);
        assert_eq!(result.config.gateway.listeners[0].port, 8080);
        assert_eq!(result.config.http_routes.len(), 1);
        assert_eq!(result.config.http_routes[0].hostnames, vec!["api.example.com"]);
        assert_eq!(result.config.http_routes[0].rules[0].backend_refs[0].name, "api-svc.default.svc");
        assert_eq!(result.config.http_routes[0].rules[0].backend_refs[0].port, 3000);
        assert!(result.route_status.iter().all(|r| r.accepted));
    }

    #[test]
    fn test_reconcile_filters_routes_by_gateway() {
        let gw = test_gateway();
        let matching_route = test_http_route();
        let ns_labels = default_ns_labels();

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

        let result = reconcile_to_config(
            &gw,
            &[matching_route, other_route],
            &[],
            &[],
            &[],
            &ns_labels,
            &[],
            &HashSet::new(),
            &HashMap::new(),
        )
        .unwrap();

        assert_eq!(result.config.http_routes.len(), 1);
        assert_eq!(result.config.http_routes[0].hostnames, vec!["api.example.com"]);
    }

    #[test]
    fn test_reconcile_produces_valid_config() {
        let gw = test_gateway();
        let route = test_http_route();
        let ns_labels = default_ns_labels();

        let result =
            reconcile_to_config(&gw, &[route], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();

        assert_eq!(result.config.gateway.listeners[0].protocol, Protocol::HTTP);
        assert_eq!(result.config.http_routes[0].rules.len(), 1);
        let rule = &result.config.http_routes[0].rules[0];
        assert_eq!(rule.matches.len(), 1);
        assert_eq!(
            rule.matches[0].path.as_ref().unwrap().match_type,
            HttpPathMatchType::PathPrefix
        );
        assert_eq!(rule.matches[0].path.as_ref().unwrap().value, "/v1");
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

    #[test]
    fn test_namespace_scoping_same() {
        let mut gw = test_gateway();
        gw.metadata.namespace = Some("default".to_string());

        let mut route = test_http_route();
        route.metadata.namespace = Some("other-ns".to_string());

        let mut ns_labels = default_ns_labels();
        ns_labels.insert("other-ns".to_string(), BTreeMap::new());

        let result = reconcile_to_config(&gw, &[route], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();
        assert_eq!(result.config.http_routes.len(), 0);
        assert!(!result.route_status[0].accepted);
    }

    #[test]
    fn test_namespace_scoping_all() {
        use gateway_api::gateways::{GatewayListenersAllowedRoutes, GatewayListenersAllowedRoutesNamespaces};

        let mut gw = test_gateway();
        gw.spec.listeners[0].allowed_routes = Some(GatewayListenersAllowedRoutes {
            namespaces: Some(GatewayListenersAllowedRoutesNamespaces {
                from: Some(GatewayListenersAllowedRoutesNamespacesFrom::All),
                selector: None,
            }),
            kinds: None,
        });

        let mut route = test_http_route();
        route.metadata.namespace = Some("other-ns".to_string());

        let mut ns_labels = default_ns_labels();
        ns_labels.insert("other-ns".to_string(), BTreeMap::new());

        let result = reconcile_to_config(&gw, &[route], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();
        assert_eq!(result.config.http_routes.len(), 1);
        assert!(result.route_status[0].accepted);
    }

    #[test]
    fn test_namespace_scoping_selector() {
        use gateway_api::gateways::*;

        let mut gw = test_gateway();
        gw.spec.listeners[0].allowed_routes = Some(GatewayListenersAllowedRoutes {
            namespaces: Some(GatewayListenersAllowedRoutesNamespaces {
                from: Some(GatewayListenersAllowedRoutesNamespacesFrom::Selector),
                selector: Some(GatewayListenersAllowedRoutesNamespacesSelector {
                    match_labels: Some(BTreeMap::from([("env".to_string(), "prod".to_string())])),
                    match_expressions: None,
                }),
            }),
            kinds: None,
        });

        let mut route = test_http_route();
        route.metadata.namespace = Some("prod-ns".to_string());

        let mut ns_labels = default_ns_labels();
        ns_labels.insert(
            "prod-ns".to_string(),
            BTreeMap::from([("env".to_string(), "prod".to_string())]),
        );

        let result = reconcile_to_config(&gw, &[route.clone()], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();
        assert_eq!(result.config.http_routes.len(), 1);

        ns_labels.insert("prod-ns".to_string(), BTreeMap::from([("env".to_string(), "staging".to_string())]));
        let result = reconcile_to_config(&gw, &[route], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();
        assert_eq!(result.config.http_routes.len(), 0);
    }

    // ---- No routes edge case ----

    #[test]
    fn test_reconcile_no_routes() {
        let gw = test_gateway();
        let ns_labels = default_ns_labels();
        let result = reconcile_to_config(&gw, &[], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();

        assert_eq!(result.config.gateway.name, "test-gw");
        assert!(result.config.http_routes.is_empty());
        assert!(result.config.tcp_routes.is_empty());
        assert!(result.config.tls_routes.is_empty());
        assert!(result.config.udp_routes.is_empty());
        assert!(result.route_status.is_empty());
    }

    #[test]
    fn test_route_without_parent_refs_excluded() {
        let gw = test_gateway();
        let ns_labels = default_ns_labels();
        let orphan = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("orphan".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: HTTPRouteSpec {
                parent_refs: None,
                hostnames: Some(vec!["orphan.example.com".to_string()]),
                rules: Some(vec![HTTPRouteRules {
                    backend_refs: Some(vec![HTTPRouteRulesBackendRefs {
                        name: "orphan-svc".to_string(),
                        port: Some(80),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
            },
            status: None,
        };
        let result = reconcile_to_config(&gw, &[orphan], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();
        assert!(result.config.http_routes.is_empty());
    }

    // ---- Backend DNS naming ----

    #[test]
    fn test_backend_dns_name_cross_namespace() {
        assert_eq!(
            backend_dns_name("db-svc", Some("database"), "app"),
            "db-svc.database.svc"
        );
    }

    // ---- Multiple listeners ----

    fn multi_listener_gateway() -> Gateway {
        Gateway {
            metadata: ObjectMeta {
                name: Some("multi-gw".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: GatewaySpec {
                gateway_class_name: "portail".to_string(),
                listeners: vec![
                    GatewayListeners {
                        name: "http".to_string(),
                        port: 8080,
                        protocol: "HTTP".to_string(),
                        hostname: None,
                        tls: None,
                        allowed_routes: None,
                    },
                    GatewayListeners {
                        name: "https".to_string(),
                        port: 8443,
                        protocol: "HTTPS".to_string(),
                        hostname: None,
                        tls: Some(GatewayListenersTls {
                            certificate_refs: Some(vec![GatewayListenersTlsCertificateRefs {
                                name: "my-cert".to_string(),
                                ..Default::default()
                            }]),
                            mode: Some(GatewayListenersTlsMode::Terminate),
                            ..Default::default()
                        }),
                        allowed_routes: None,
                    },
                    GatewayListeners {
                        name: "tcp".to_string(),
                        port: 5432,
                        protocol: "TCP".to_string(),
                        hostname: None,
                        tls: None,
                        allowed_routes: None,
                    },
                    GatewayListeners {
                        name: "tls-passthrough".to_string(),
                        port: 6443,
                        protocol: "TLS".to_string(),
                        hostname: None,
                        tls: Some(GatewayListenersTls {
                            mode: Some(GatewayListenersTlsMode::Passthrough),
                            ..Default::default()
                        }),
                        allowed_routes: None,
                    },
                    GatewayListeners {
                        name: "udp".to_string(),
                        port: 5353,
                        protocol: "UDP".to_string(),
                        hostname: None,
                        tls: None,
                        allowed_routes: None,
                    },
                ],
                ..Default::default()
            },
            status: None,
        }
    }

    #[test]
    fn test_multiple_listeners() {
        let gw = multi_listener_gateway();
        let ns_labels = default_ns_labels();
        let result = reconcile_to_config(&gw, &[], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();

        assert_eq!(result.config.gateway.listeners.len(), 5);
        assert_eq!(result.config.gateway.listeners[0].protocol, Protocol::HTTP);
        assert_eq!(result.config.gateway.listeners[1].protocol, Protocol::HTTPS);
        assert_eq!(result.config.gateway.listeners[2].protocol, Protocol::TCP);
        assert_eq!(result.config.gateway.listeners[3].protocol, Protocol::TLS);
        assert_eq!(result.config.gateway.listeners[4].protocol, Protocol::UDP);
    }

    #[test]
    fn test_tls_terminate_listener() {
        let gw = multi_listener_gateway();
        let ns_labels = default_ns_labels();
        let result = reconcile_to_config(&gw, &[], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();

        let https = &result.config.gateway.listeners[1];
        let tls = https.tls.as_ref().unwrap();
        assert_eq!(tls.mode, TlsMode::Terminate);
        assert_eq!(tls.certificate_refs[0].name, "my-cert");
    }

    #[test]
    fn test_tls_passthrough_listener() {
        let gw = multi_listener_gateway();
        let ns_labels = default_ns_labels();
        let result = reconcile_to_config(&gw, &[], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();

        let tls_listener = &result.config.gateway.listeners[3];
        let tls = tls_listener.tls.as_ref().unwrap();
        assert_eq!(tls.mode, TlsMode::Passthrough);
        assert!(tls.certificate_refs.is_empty());
    }

    #[test]
    fn test_unsupported_protocol_rejected() {
        let gw = Gateway {
            metadata: ObjectMeta {
                name: Some("bad-gw".to_string()),
                ..Default::default()
            },
            spec: GatewaySpec {
                gateway_class_name: "portail".to_string(),
                listeners: vec![GatewayListeners {
                    name: "grpc".to_string(),
                    port: 9090,
                    protocol: "GRPC".to_string(),
                    hostname: None,
                    tls: None,
                    allowed_routes: None,
                }],
                ..Default::default()
            },
            status: None,
        };
        let ns_labels = default_ns_labels();
        assert!(reconcile_to_config(&gw, &[], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).is_err());
    }

    // ---- TCP route conversion ----

    #[test]
    fn test_tcp_route_conversion() {
        let gw = multi_listener_gateway();
        let ns_labels = default_ns_labels();
        let tcp_route = TCPRoute {
            metadata: ObjectMeta {
                name: Some("db-route".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: TCPRouteSpec {
                parent_refs: Some(vec![TCPRouteParentRefs {
                    name: "multi-gw".to_string(),
                    section_name: Some("tcp".to_string()),
                    ..Default::default()
                }]),
                rules: vec![TCPRouteRules {
                    backend_refs: vec![
                        TCPRouteRulesBackendRefs {
                            name: "postgres-primary".to_string(),
                            port: Some(5432),
                            weight: Some(3),
                            ..Default::default()
                        },
                        TCPRouteRulesBackendRefs {
                            name: "postgres-replica".to_string(),
                            port: Some(5432),
                            weight: Some(1),
                            namespace: Some("db".to_string()),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            },
            status: None,
        };

        let result = reconcile_to_config(&gw, &[], &[tcp_route], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();

        assert_eq!(result.config.tcp_routes.len(), 1);
        let rule = &result.config.tcp_routes[0].rules[0];
        assert_eq!(rule.backend_refs.len(), 2);
        assert_eq!(rule.backend_refs[0].name, "postgres-primary.default.svc");
        assert_eq!(rule.backend_refs[0].port, 5432);
        assert_eq!(rule.backend_refs[0].weight, 3);
        assert_eq!(rule.backend_refs[1].name, "postgres-replica.db.svc");
    }

    // ---- TLS route conversion ----

    #[test]
    fn test_tls_route_conversion() {
        let gw = multi_listener_gateway();
        let ns_labels = default_ns_labels();
        let tls_route = TLSRoute {
            metadata: ObjectMeta {
                name: Some("tls-route".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: TLSRouteSpec {
                parent_refs: Some(vec![TLSRouteParentRefs {
                    name: "multi-gw".to_string(),
                    section_name: Some("tls-passthrough".to_string()),
                    ..Default::default()
                }]),
                hostnames: vec!["secure.example.com".to_string(), "*.internal.example.com".to_string()],
                rules: vec![TLSRouteRules {
                    backend_refs: vec![TLSRouteRulesBackendRefs {
                        name: "backend-tls".to_string(),
                        port: Some(8443),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            },
            status: None,
        };

        let result = reconcile_to_config(&gw, &[], &[], &[tls_route], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();

        assert_eq!(result.config.tls_routes.len(), 1);
        assert_eq!(result.config.tls_routes[0].hostnames, vec!["secure.example.com", "*.internal.example.com"]);
        assert_eq!(result.config.tls_routes[0].rules[0].backend_refs[0].name, "backend-tls.default.svc");
        assert_eq!(result.config.tls_routes[0].rules[0].backend_refs[0].port, 8443);
    }

    // ---- UDP route conversion ----

    #[test]
    fn test_udp_route_conversion() {
        let gw = multi_listener_gateway();
        let ns_labels = default_ns_labels();
        let udp_route = UDPRoute {
            metadata: ObjectMeta {
                name: Some("dns-route".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: UDPRouteSpec {
                parent_refs: Some(vec![UDPRouteParentRefs {
                    name: "multi-gw".to_string(),
                    section_name: Some("udp".to_string()),
                    ..Default::default()
                }]),
                rules: vec![UDPRouteRules {
                    backend_refs: vec![UDPRouteRulesBackendRefs {
                        name: "coredns".to_string(),
                        port: Some(53),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            },
            status: None,
        };

        let result = reconcile_to_config(&gw, &[], &[], &[], &[udp_route], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();

        assert_eq!(result.config.udp_routes.len(), 1);
        assert_eq!(result.config.udp_routes[0].rules[0].backend_refs[0].name, "coredns.default.svc");
        assert_eq!(result.config.udp_routes[0].rules[0].backend_refs[0].port, 53);
    }

    // ---- HTTP path match types ----

    #[test]
    fn test_exact_path_match() {
        let gw = test_gateway();
        let ns_labels = default_ns_labels();
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("exact-route".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: HTTPRouteSpec {
                parent_refs: Some(vec![HTTPRouteParentRefs {
                    name: "test-gw".to_string(),
                    ..Default::default()
                }]),
                hostnames: Some(vec!["api.example.com".to_string()]),
                rules: Some(vec![HTTPRouteRules {
                    matches: Some(vec![HTTPRouteRulesMatches {
                        path: Some(HTTPRouteRulesMatchesPath {
                            r#type: Some(HTTPRouteRulesMatchesPathType::Exact),
                            value: Some("/health".to_string()),
                        }),
                        ..Default::default()
                    }]),
                    backend_refs: Some(vec![HTTPRouteRulesBackendRefs {
                        name: "health-svc".to_string(),
                        port: Some(80),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
            },
            status: None,
        };

        let result = reconcile_to_config(&gw, &[route], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();
        let m = &result.config.http_routes[0].rules[0].matches[0];
        assert_eq!(m.path.as_ref().unwrap().match_type, HttpPathMatchType::Exact);
        assert_eq!(m.path.as_ref().unwrap().value, "/health");
    }

    #[test]
    fn test_regex_path_match() {
        let gw = test_gateway();
        let ns_labels = default_ns_labels();
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("regex-route".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: HTTPRouteSpec {
                parent_refs: Some(vec![HTTPRouteParentRefs {
                    name: "test-gw".to_string(),
                    ..Default::default()
                }]),
                hostnames: Some(vec!["api.example.com".to_string()]),
                rules: Some(vec![HTTPRouteRules {
                    matches: Some(vec![HTTPRouteRulesMatches {
                        path: Some(HTTPRouteRulesMatchesPath {
                            r#type: Some(HTTPRouteRulesMatchesPathType::RegularExpression),
                            value: Some("/users/\\d+".to_string()),
                        }),
                        ..Default::default()
                    }]),
                    backend_refs: Some(vec![HTTPRouteRulesBackendRefs {
                        name: "users-svc".to_string(),
                        port: Some(80),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
            },
            status: None,
        };

        let result = reconcile_to_config(&gw, &[route], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();
        let m = &result.config.http_routes[0].rules[0].matches[0];
        assert_eq!(m.path.as_ref().unwrap().match_type, HttpPathMatchType::RegularExpression);
        assert_eq!(m.path.as_ref().unwrap().value, "/users/\\d+");
    }

    // ---- Header and query param matching ----

    #[test]
    fn test_header_match_exact_and_regex() {
        let gw = test_gateway();
        let ns_labels = default_ns_labels();
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("header-route".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: HTTPRouteSpec {
                parent_refs: Some(vec![HTTPRouteParentRefs {
                    name: "test-gw".to_string(),
                    ..Default::default()
                }]),
                hostnames: Some(vec!["api.example.com".to_string()]),
                rules: Some(vec![HTTPRouteRules {
                    matches: Some(vec![HTTPRouteRulesMatches {
                        path: Some(HTTPRouteRulesMatchesPath {
                            r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                            value: Some("/".to_string()),
                        }),
                        headers: Some(vec![
                            HTTPRouteRulesMatchesHeaders {
                                name: "X-Env".to_string(),
                                value: "canary".to_string(),
                                r#type: Some(HTTPRouteRulesMatchesHeadersType::Exact),
                            },
                            HTTPRouteRulesMatchesHeaders {
                                name: "X-Request-Id".to_string(),
                                value: "^[a-f0-9-]+$".to_string(),
                                r#type: Some(HTTPRouteRulesMatchesHeadersType::RegularExpression),
                            },
                        ]),
                        ..Default::default()
                    }]),
                    backend_refs: Some(vec![HTTPRouteRulesBackendRefs {
                        name: "canary-svc".to_string(),
                        port: Some(80),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
            },
            status: None,
        };

        let result = reconcile_to_config(&gw, &[route], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();
        let headers = &result.config.http_routes[0].rules[0].matches[0].headers;
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0].name, "X-Env");
        assert_eq!(headers[0].match_type, StringMatchType::Exact);
        assert_eq!(headers[1].name, "X-Request-Id");
        assert_eq!(headers[1].match_type, StringMatchType::RegularExpression);
    }

    #[test]
    fn test_query_param_match() {
        let gw = test_gateway();
        let ns_labels = default_ns_labels();
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("qp-route".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: HTTPRouteSpec {
                parent_refs: Some(vec![HTTPRouteParentRefs {
                    name: "test-gw".to_string(),
                    ..Default::default()
                }]),
                hostnames: Some(vec!["api.example.com".to_string()]),
                rules: Some(vec![HTTPRouteRules {
                    matches: Some(vec![HTTPRouteRulesMatches {
                        path: Some(HTTPRouteRulesMatchesPath {
                            r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                            value: Some("/search".to_string()),
                        }),
                        query_params: Some(vec![
                            HTTPRouteRulesMatchesQueryParams {
                                name: "format".to_string(),
                                value: "json".to_string(),
                                r#type: Some(HTTPRouteRulesMatchesQueryParamsType::Exact),
                            },
                            HTTPRouteRulesMatchesQueryParams {
                                name: "id".to_string(),
                                value: "^\\d+$".to_string(),
                                r#type: Some(HTTPRouteRulesMatchesQueryParamsType::RegularExpression),
                            },
                        ]),
                        ..Default::default()
                    }]),
                    backend_refs: Some(vec![HTTPRouteRulesBackendRefs {
                        name: "search-svc".to_string(),
                        port: Some(80),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
            },
            status: None,
        };

        let result = reconcile_to_config(&gw, &[route], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();
        let qps = &result.config.http_routes[0].rules[0].matches[0].query_params;
        assert_eq!(qps.len(), 2);
        assert_eq!(qps[0].name, "format");
        assert_eq!(qps[0].match_type, StringMatchType::Exact);
        assert_eq!(qps[1].name, "id");
        assert_eq!(qps[1].match_type, StringMatchType::RegularExpression);
    }

    // ---- Weighted backends ----

    #[test]
    fn test_weighted_backends() {
        let gw = test_gateway();
        let ns_labels = default_ns_labels();
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("weighted-route".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: HTTPRouteSpec {
                parent_refs: Some(vec![HTTPRouteParentRefs {
                    name: "test-gw".to_string(),
                    ..Default::default()
                }]),
                hostnames: Some(vec!["api.example.com".to_string()]),
                rules: Some(vec![HTTPRouteRules {
                    backend_refs: Some(vec![
                        HTTPRouteRulesBackendRefs {
                            name: "stable".to_string(),
                            port: Some(80),
                            weight: Some(90),
                            ..Default::default()
                        },
                        HTTPRouteRulesBackendRefs {
                            name: "canary".to_string(),
                            port: Some(80),
                            weight: Some(10),
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                }]),
            },
            status: None,
        };

        let result = reconcile_to_config(&gw, &[route], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();
        let backends = &result.config.http_routes[0].rules[0].backend_refs;
        assert_eq!(backends.len(), 2);
        assert_eq!(backends[0].name, "stable.default.svc");
        assert_eq!(backends[0].weight, 90);
        assert_eq!(backends[1].name, "canary.default.svc");
        assert_eq!(backends[1].weight, 10);
    }

    #[test]
    fn test_default_weight_and_port() {
        let gw = test_gateway();
        let ns_labels = default_ns_labels();
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("defaults-route".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: HTTPRouteSpec {
                parent_refs: Some(vec![HTTPRouteParentRefs {
                    name: "test-gw".to_string(),
                    ..Default::default()
                }]),
                hostnames: Some(vec!["api.example.com".to_string()]),
                rules: Some(vec![HTTPRouteRules {
                    backend_refs: Some(vec![HTTPRouteRulesBackendRefs {
                        name: "svc".to_string(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
            },
            status: None,
        };

        let result = reconcile_to_config(&gw, &[route], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();
        let br = &result.config.http_routes[0].rules[0].backend_refs[0];
        assert_eq!(br.port, 80);
        assert_eq!(br.weight, 1);
    }

    // ---- Multiple rules per route ----

    #[test]
    fn test_multiple_rules_per_route() {
        let gw = test_gateway();
        let ns_labels = default_ns_labels();
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("multi-rule".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: HTTPRouteSpec {
                parent_refs: Some(vec![HTTPRouteParentRefs {
                    name: "test-gw".to_string(),
                    ..Default::default()
                }]),
                hostnames: Some(vec!["api.example.com".to_string()]),
                rules: Some(vec![
                    HTTPRouteRules {
                        matches: Some(vec![HTTPRouteRulesMatches {
                            path: Some(HTTPRouteRulesMatchesPath {
                                r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                                value: Some("/v1".to_string()),
                            }),
                            ..Default::default()
                        }]),
                        backend_refs: Some(vec![HTTPRouteRulesBackendRefs {
                            name: "v1-svc".to_string(),
                            port: Some(80),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    },
                    HTTPRouteRules {
                        matches: Some(vec![HTTPRouteRulesMatches {
                            path: Some(HTTPRouteRulesMatchesPath {
                                r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                                value: Some("/v2".to_string()),
                            }),
                            ..Default::default()
                        }]),
                        backend_refs: Some(vec![HTTPRouteRulesBackendRefs {
                            name: "v2-svc".to_string(),
                            port: Some(80),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    },
                ]),
            },
            status: None,
        };

        let result = reconcile_to_config(&gw, &[route], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();
        assert_eq!(result.config.http_routes[0].rules.len(), 2);
        assert_eq!(result.config.http_routes[0].rules[0].backend_refs[0].name, "v1-svc.default.svc");
        assert_eq!(result.config.http_routes[0].rules[1].backend_refs[0].name, "v2-svc.default.svc");
    }

    // ---- HTTP filters ----

    #[test]
    fn test_request_header_modifier_filter() {
        let gw = test_gateway();
        let ns_labels = default_ns_labels();
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("hdr-mod-route".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: HTTPRouteSpec {
                parent_refs: Some(vec![HTTPRouteParentRefs {
                    name: "test-gw".to_string(),
                    ..Default::default()
                }]),
                hostnames: Some(vec!["api.example.com".to_string()]),
                rules: Some(vec![HTTPRouteRules {
                    filters: Some(vec![HTTPRouteRulesFilters {
                        r#type: HTTPRouteRulesFiltersType::RequestHeaderModifier,
                        request_header_modifier: Some(HTTPRouteRulesFiltersRequestHeaderModifier {
                            add: Some(vec![HTTPRouteRulesFiltersRequestHeaderModifierAdd {
                                name: "X-Added".to_string(),
                                value: "yes".to_string(),
                            }]),
                            set: Some(vec![HTTPRouteRulesFiltersRequestHeaderModifierSet {
                                name: "X-Set".to_string(),
                                value: "always".to_string(),
                            }]),
                            remove: Some(vec!["X-Remove".to_string()]),
                        }),
                        ..Default::default()
                    }]),
                    backend_refs: Some(vec![HTTPRouteRulesBackendRefs {
                        name: "svc".to_string(),
                        port: Some(80),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
            },
            status: None,
        };

        let result = reconcile_to_config(&gw, &[route], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();
        let filters = &result.config.http_routes[0].rules[0].filters;
        assert_eq!(filters.len(), 1);
        match &filters[0] {
            HttpRouteFilter::RequestHeaderModifier { config } => {
                assert_eq!(config.add[0].name, "X-Added");
                assert_eq!(config.set[0].name, "X-Set");
                assert_eq!(config.remove, vec!["X-Remove"]);
            }
            _ => panic!("expected RequestHeaderModifier"),
        }
    }

    #[test]
    fn test_response_header_modifier_filter() {
        let gw = test_gateway();
        let ns_labels = default_ns_labels();
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("resp-hdr-route".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: HTTPRouteSpec {
                parent_refs: Some(vec![HTTPRouteParentRefs {
                    name: "test-gw".to_string(),
                    ..Default::default()
                }]),
                hostnames: Some(vec!["api.example.com".to_string()]),
                rules: Some(vec![HTTPRouteRules {
                    filters: Some(vec![HTTPRouteRulesFilters {
                        r#type: HTTPRouteRulesFiltersType::ResponseHeaderModifier,
                        response_header_modifier: Some(HTTPRouteRulesFiltersResponseHeaderModifier {
                            add: Some(vec![HTTPRouteRulesFiltersResponseHeaderModifierAdd {
                                name: "X-Response".to_string(),
                                value: "modified".to_string(),
                            }]),
                            set: None,
                            remove: Some(vec!["Server".to_string()]),
                        }),
                        ..Default::default()
                    }]),
                    backend_refs: Some(vec![HTTPRouteRulesBackendRefs {
                        name: "svc".to_string(),
                        port: Some(80),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
            },
            status: None,
        };

        let result = reconcile_to_config(&gw, &[route], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();
        match &result.config.http_routes[0].rules[0].filters[0] {
            HttpRouteFilter::ResponseHeaderModifier { config } => {
                assert_eq!(config.add[0].name, "X-Response");
                assert_eq!(config.remove, vec!["Server"]);
            }
            _ => panic!("expected ResponseHeaderModifier"),
        }
    }

    #[test]
    fn test_request_redirect_filter() {
        let gw = test_gateway();
        let ns_labels = default_ns_labels();
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("redirect-route".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: HTTPRouteSpec {
                parent_refs: Some(vec![HTTPRouteParentRefs {
                    name: "test-gw".to_string(),
                    ..Default::default()
                }]),
                hostnames: Some(vec!["api.example.com".to_string()]),
                rules: Some(vec![HTTPRouteRules {
                    filters: Some(vec![HTTPRouteRulesFilters {
                        r#type: HTTPRouteRulesFiltersType::RequestRedirect,
                        request_redirect: Some(HTTPRouteRulesFiltersRequestRedirect {
                            scheme: Some(HTTPRouteRulesFiltersRequestRedirectScheme::Https),
                            hostname: Some("secure.example.com".to_string()),
                            port: Some(443),
                            path: Some(HTTPRouteRulesFiltersRequestRedirectPath {
                                r#type: HTTPRouteRulesFiltersRequestRedirectPathType::ReplaceFullPath,
                                replace_full_path: Some("/new-path".to_string()),
                                replace_prefix_match: None,
                            }),
                            status_code: Some(301),
                        }),
                        ..Default::default()
                    }]),
                    backend_refs: None,
                    ..Default::default()
                }]),
            },
            status: None,
        };

        let result = reconcile_to_config(&gw, &[route], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();
        match &result.config.http_routes[0].rules[0].filters[0] {
            HttpRouteFilter::RequestRedirect { config } => {
                assert_eq!(config.scheme.as_deref(), Some("https"));
                assert_eq!(config.hostname.as_deref(), Some("secure.example.com"));
                assert_eq!(config.port, Some(443));
                assert_eq!(config.status_code, 301);
                assert!(matches!(&config.path, Some(HttpURLRewritePath::ReplaceFullPath { value }) if value == "/new-path"));
            }
            _ => panic!("expected RequestRedirect"),
        }
    }

    #[test]
    fn test_url_rewrite_filter() {
        let gw = test_gateway();
        let ns_labels = default_ns_labels();
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("rewrite-route".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: HTTPRouteSpec {
                parent_refs: Some(vec![HTTPRouteParentRefs {
                    name: "test-gw".to_string(),
                    ..Default::default()
                }]),
                hostnames: Some(vec!["api.example.com".to_string()]),
                rules: Some(vec![HTTPRouteRules {
                    filters: Some(vec![HTTPRouteRulesFilters {
                        r#type: HTTPRouteRulesFiltersType::UrlRewrite,
                        url_rewrite: Some(HTTPRouteRulesFiltersUrlRewrite {
                            hostname: Some("internal.example.com".to_string()),
                            path: Some(HTTPRouteRulesFiltersUrlRewritePath {
                                r#type: HTTPRouteRulesFiltersUrlRewritePathType::ReplacePrefixMatch,
                                replace_prefix_match: Some("/new".to_string()),
                                replace_full_path: None,
                            }),
                        }),
                        ..Default::default()
                    }]),
                    backend_refs: Some(vec![HTTPRouteRulesBackendRefs {
                        name: "svc".to_string(),
                        port: Some(80),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
            },
            status: None,
        };

        let result = reconcile_to_config(&gw, &[route], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();
        match &result.config.http_routes[0].rules[0].filters[0] {
            HttpRouteFilter::URLRewrite { config } => {
                assert_eq!(config.hostname.as_deref(), Some("internal.example.com"));
                assert!(matches!(&config.path, Some(HttpURLRewritePath::ReplacePrefixMatch { value }) if value == "/new"));
            }
            _ => panic!("expected URLRewrite"),
        }
    }

    #[test]
    fn test_request_mirror_filter() {
        let gw = test_gateway();
        let ns_labels = default_ns_labels();
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("mirror-route".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: HTTPRouteSpec {
                parent_refs: Some(vec![HTTPRouteParentRefs {
                    name: "test-gw".to_string(),
                    ..Default::default()
                }]),
                hostnames: Some(vec!["api.example.com".to_string()]),
                rules: Some(vec![HTTPRouteRules {
                    filters: Some(vec![HTTPRouteRulesFilters {
                        r#type: HTTPRouteRulesFiltersType::RequestMirror,
                        request_mirror: Some(HTTPRouteRulesFiltersRequestMirror {
                            backend_ref: HTTPRouteRulesFiltersRequestMirrorBackendRef {
                                name: "shadow-svc".to_string(),
                                port: Some(9090),
                                namespace: Some("staging".to_string()),
                                ..Default::default()
                            },
                            ..Default::default()
                        }),
                        ..Default::default()
                    }]),
                    backend_refs: Some(vec![HTTPRouteRulesBackendRefs {
                        name: "prod-svc".to_string(),
                        port: Some(80),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
            },
            status: None,
        };

        let result = reconcile_to_config(&gw, &[route], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();
        match &result.config.http_routes[0].rules[0].filters[0] {
            HttpRouteFilter::RequestMirror { config } => {
                assert_eq!(config.backend_ref.name, "shadow-svc.staging.svc");
                assert_eq!(config.backend_ref.port, 9090);
            }
            _ => panic!("expected RequestMirror"),
        }
    }

    // ---- Timeouts ----

    #[test]
    fn test_timeouts_conversion() {
        let gw = test_gateway();
        let ns_labels = default_ns_labels();
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("timeout-route".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: HTTPRouteSpec {
                parent_refs: Some(vec![HTTPRouteParentRefs {
                    name: "test-gw".to_string(),
                    ..Default::default()
                }]),
                hostnames: Some(vec!["api.example.com".to_string()]),
                rules: Some(vec![HTTPRouteRules {
                    backend_refs: Some(vec![HTTPRouteRulesBackendRefs {
                        name: "svc".to_string(),
                        port: Some(80),
                        ..Default::default()
                    }]),
                    timeouts: Some(HTTPRouteRulesTimeouts {
                        request: Some("30s".to_string()),
                        backend_request: Some("10s".to_string()),
                    }),
                    ..Default::default()
                }]),
            },
            status: None,
        };

        let result = reconcile_to_config(&gw, &[route], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();
        let timeouts = result.config.http_routes[0].rules[0].timeouts.as_ref().unwrap();
        assert_eq!(timeouts.request, Some(std::time::Duration::from_secs(30)));
        assert_eq!(timeouts.backend_request, Some(std::time::Duration::from_secs(10)));
    }

    // ---- Mixed route types ----

    #[test]
    fn test_mixed_route_types() {
        let gw = multi_listener_gateway();
        let ns_labels = default_ns_labels();

        let http_route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("http-route".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: HTTPRouteSpec {
                parent_refs: Some(vec![HTTPRouteParentRefs {
                    name: "multi-gw".to_string(),
                    section_name: Some("http".to_string()),
                    ..Default::default()
                }]),
                hostnames: Some(vec!["web.example.com".to_string()]),
                rules: Some(vec![HTTPRouteRules {
                    backend_refs: Some(vec![HTTPRouteRulesBackendRefs {
                        name: "web-svc".to_string(),
                        port: Some(80),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
            },
            status: None,
        };

        let tcp_route = TCPRoute {
            metadata: ObjectMeta {
                name: Some("tcp-route".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: TCPRouteSpec {
                parent_refs: Some(vec![TCPRouteParentRefs {
                    name: "multi-gw".to_string(),
                    section_name: Some("tcp".to_string()),
                    ..Default::default()
                }]),
                rules: vec![TCPRouteRules {
                    backend_refs: vec![TCPRouteRulesBackendRefs {
                        name: "db-svc".to_string(),
                        port: Some(5432),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            },
            status: None,
        };

        let result = reconcile_to_config(&gw, &[http_route], &[tcp_route], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();

        assert_eq!(result.config.http_routes.len(), 1);
        assert_eq!(result.config.tcp_routes.len(), 1);
        assert_eq!(result.config.http_routes[0].rules[0].backend_refs[0].name, "web-svc.default.svc");
        assert_eq!(result.config.tcp_routes[0].rules[0].backend_refs[0].name, "db-svc.default.svc");
        assert_eq!(result.route_status.len(), 2);
        assert!(result.route_status.iter().all(|r| r.accepted));
    }

    // ---- Integration: reconcile to route table (localhost backends to avoid DNS) ----

    #[test]
    fn test_reconcile_to_route_table_http() {
        let gw = Gateway {
            metadata: ObjectMeta {
                name: Some("rt-gw".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: GatewaySpec {
                gateway_class_name: "portail".to_string(),
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
        };

        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("rt-route".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: HTTPRouteSpec {
                parent_refs: Some(vec![HTTPRouteParentRefs {
                    name: "rt-gw".to_string(),
                    section_name: Some("http".to_string()),
                    ..Default::default()
                }]),
                hostnames: Some(vec!["app.example.com".to_string()]),
                rules: Some(vec![
                    HTTPRouteRules {
                        matches: Some(vec![HTTPRouteRulesMatches {
                            path: Some(HTTPRouteRulesMatchesPath {
                                r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                                value: Some("/api".to_string()),
                            }),
                            ..Default::default()
                        }]),
                        backend_refs: Some(vec![HTTPRouteRulesBackendRefs {
                            name: "127.0.0.1".to_string(),
                            port: Some(3000),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    },
                    HTTPRouteRules {
                        matches: Some(vec![HTTPRouteRulesMatches {
                            path: Some(HTTPRouteRulesMatchesPath {
                                r#type: Some(HTTPRouteRulesMatchesPathType::Exact),
                                value: Some("/health".to_string()),
                            }),
                            ..Default::default()
                        }]),
                        backend_refs: Some(vec![HTTPRouteRulesBackendRefs {
                            name: "127.0.0.1".to_string(),
                            port: Some(3001),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    },
                ]),
            },
            status: None,
        };

        let ns_labels = default_ns_labels();
        let result = reconcile_to_config(&gw, &[route], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();
        let mut config = result.config;
        fixup_backends_for_test(&mut config);
        let rt = config.to_route_table().unwrap();

        let rule = rt.find_http_route("app.example.com", "GET", "/health", &[], "").unwrap();
        assert_eq!(rule.backends[0].socket_addr.port(), 3001);

        let rule = rt.find_http_route("app.example.com", "GET", "/api/users", &[], "").unwrap();
        assert_eq!(rule.backends[0].socket_addr.port(), 3000);

        assert!(rt.find_http_route("other.com", "GET", "/api", &[], "").is_err());
    }

    #[test]
    fn test_reconcile_to_route_table_tcp() {
        let gw = Gateway {
            metadata: ObjectMeta {
                name: Some("tcp-gw".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: GatewaySpec {
                gateway_class_name: "portail".to_string(),
                listeners: vec![GatewayListeners {
                    name: "tcp".to_string(),
                    port: 5432,
                    protocol: "TCP".to_string(),
                    hostname: None,
                    tls: None,
                    allowed_routes: None,
                }],
                ..Default::default()
            },
            status: None,
        };

        let tcp_route = TCPRoute {
            metadata: ObjectMeta {
                name: Some("db-route".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: TCPRouteSpec {
                parent_refs: Some(vec![TCPRouteParentRefs {
                    name: "tcp-gw".to_string(),
                    section_name: Some("tcp".to_string()),
                    ..Default::default()
                }]),
                rules: vec![TCPRouteRules {
                    backend_refs: vec![TCPRouteRulesBackendRefs {
                        name: "127.0.0.1".to_string(),
                        port: Some(5432),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            },
            status: None,
        };

        let ns_labels = default_ns_labels();
        let result = reconcile_to_config(&gw, &[], &[tcp_route], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();
        let mut config = result.config;
        fixup_backends_for_test(&mut config);
        let rt = config.to_route_table().unwrap();

        let backends = rt.find_tcp_backends(5432).unwrap();
        assert_eq!(backends[0].socket_addr.port(), 5432);
    }

    #[test]
    fn test_reconcile_to_route_table_with_filters() {
        let gw = Gateway {
            metadata: ObjectMeta {
                name: Some("filter-gw".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: GatewaySpec {
                gateway_class_name: "portail".to_string(),
                listeners: vec![GatewayListeners {
                    name: "http".to_string(),
                    port: 8080,
                    protocol: "HTTP".to_string(),
                    hostname: None,
                    tls: None,
                    allowed_routes: None,
                }],
                ..Default::default()
            },
            status: None,
        };

        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("filter-route".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: HTTPRouteSpec {
                parent_refs: Some(vec![HTTPRouteParentRefs {
                    name: "filter-gw".to_string(),
                    section_name: Some("http".to_string()),
                    ..Default::default()
                }]),
                hostnames: Some(vec!["example.com".to_string()]),
                rules: Some(vec![HTTPRouteRules {
                    matches: Some(vec![HTTPRouteRulesMatches {
                        path: Some(HTTPRouteRulesMatchesPath {
                            r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                            value: Some("/".to_string()),
                        }),
                        ..Default::default()
                    }]),
                    filters: Some(vec![HTTPRouteRulesFilters {
                        r#type: HTTPRouteRulesFiltersType::RequestHeaderModifier,
                        request_header_modifier: Some(HTTPRouteRulesFiltersRequestHeaderModifier {
                            add: Some(vec![HTTPRouteRulesFiltersRequestHeaderModifierAdd {
                                name: "X-Gateway".to_string(),
                                value: "portail".to_string(),
                            }]),
                            set: None,
                            remove: None,
                        }),
                        ..Default::default()
                    }]),
                    backend_refs: Some(vec![HTTPRouteRulesBackendRefs {
                        name: "127.0.0.1".to_string(),
                        port: Some(8001),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
            },
            status: None,
        };

        let ns_labels = default_ns_labels();
        let result = reconcile_to_config(&gw, &[route], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();
        let mut config = result.config;
        fixup_backends_for_test(&mut config);
        let rt = config.to_route_table().unwrap();

        let rule = rt.find_http_route("example.com", "GET", "/anything", &[], "").unwrap();
        assert_eq!(rule.filters.len(), 1);
        assert!(rule.has_filters);
    }

    #[test]
    fn test_reconcile_to_route_table_weighted_backends() {
        let gw = Gateway {
            metadata: ObjectMeta {
                name: Some("wt-gw".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: GatewaySpec {
                gateway_class_name: "portail".to_string(),
                listeners: vec![GatewayListeners {
                    name: "http".to_string(),
                    port: 8080,
                    protocol: "HTTP".to_string(),
                    hostname: None,
                    tls: None,
                    allowed_routes: None,
                }],
                ..Default::default()
            },
            status: None,
        };

        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("wt-route".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: HTTPRouteSpec {
                parent_refs: Some(vec![HTTPRouteParentRefs {
                    name: "wt-gw".to_string(),
                    section_name: Some("http".to_string()),
                    ..Default::default()
                }]),
                hostnames: Some(vec!["app.com".to_string()]),
                rules: Some(vec![HTTPRouteRules {
                    matches: Some(vec![HTTPRouteRulesMatches {
                        path: Some(HTTPRouteRulesMatchesPath {
                            r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                            value: Some("/".to_string()),
                        }),
                        ..Default::default()
                    }]),
                    backend_refs: Some(vec![
                        HTTPRouteRulesBackendRefs {
                            name: "127.0.0.1".to_string(),
                            port: Some(8001),
                            weight: Some(3),
                            ..Default::default()
                        },
                        HTTPRouteRulesBackendRefs {
                            name: "127.0.0.1".to_string(),
                            port: Some(8002),
                            weight: Some(1),
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                }]),
            },
            status: None,
        };

        let ns_labels = default_ns_labels();
        let result = reconcile_to_config(&gw, &[route], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();
        let mut config = result.config;
        fixup_backends_for_test(&mut config);
        let rt = config.to_route_table().unwrap();

        let rule = rt.find_http_route("app.com", "GET", "/", &[], "").unwrap();
        assert_eq!(rule.backends.len(), 2);
        assert_eq!(rule.total_weight, 4);
        assert_eq!(rule.cumulative_weights, vec![3, 4]);
    }

    // ---- Route acceptance tracking ----

    #[test]
    fn test_route_acceptance_tracked() {
        let gw = test_gateway();
        let ns_labels = default_ns_labels();
        let route = test_http_route();

        let result = reconcile_to_config(&gw, &[route], &[], &[], &[], &ns_labels, &[], &HashSet::new(), &HashMap::new()).unwrap();

        assert_eq!(result.route_status.len(), 1);
        assert!(result.route_status[0].accepted);
        assert_eq!(result.route_status[0].kind, "HTTPRoute");
        assert_eq!(result.route_status[0].name, "test-route");
        assert_eq!(result.route_status[0].namespace, "default");
    }
}
