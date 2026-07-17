use anyhow::Result;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use gateway_api::experimental::tcproutes::*;
use gateway_api::experimental::tlsroutes::*;
use gateway_api::experimental::udproutes::*;
use gateway_api::gateways::{Gateway, GatewayListeners};
use gateway_api::httproutes::*;
use gateway_api::referencegrants::ReferenceGrant;

use k8s_openapi::api::core::v1::Service;

use crate::config::*;
use crate::logging::warn;

use super::converters::{
    convert_gateway, convert_http_route, convert_tcp_route, convert_tls_route, convert_udp_route,
    parse_backend_dns_name, route_namespace, CertData,
};
use super::parent_ref::{parent_ref_matches_gateway, route_targets_gateway, ParentRefAccess};
use super::reference_grants::{is_reference_allowed, listeners_for_parent_ref};
use super::services::ServiceState;

// ---------------------------------------------------------------------------
// Reconciliation input types — structured parameters for reconcile_to_config
// ---------------------------------------------------------------------------

/// Snapshot of all cluster resources needed for reconciliation.
/// Built once per reconcile from reflector stores. Routes and Services are
/// held as `Arc`s straight out of the reflector stores — snapshotting is
/// pointer work, not a deep copy of every object in the cluster. Routes may
/// additionally be pre-filtered to this Gateway by the snapshot builder;
/// `collect_routes` re-checks `route_targets_gateway` either way.
pub(crate) struct ClusterSnapshot {
    pub http_routes: Vec<Arc<HTTPRoute>>,
    pub tcp_routes: Vec<Arc<TCPRoute>>,
    pub tls_routes: Vec<Arc<TLSRoute>>,
    pub udp_routes: Vec<Arc<UDPRoute>>,
    pub namespace_labels: HashMap<String, BTreeMap<String, String>>,
    pub reference_grants: Vec<ReferenceGrant>,
    pub services: Vec<Arc<Service>>,
}

// ---------------------------------------------------------------------------
// GatewayRoute trait — replaces the 8 closure parameters on collect_routes
// ---------------------------------------------------------------------------

/// Abstraction over the four Gateway API route types (HTTP, TCP, TLS, UDP).
/// Each method corresponds to a former closure parameter of `collect_routes`.
pub(crate) trait GatewayRoute: Sized {
    type ParentRef: ParentRefAccess;
    type Config;
    const KIND: &'static str;

    fn parent_refs(&self) -> &Option<Vec<Self::ParentRef>>;
    fn route_namespace(&self) -> &str;
    fn identity(&self) -> (&str, Option<i64>);
    fn hostnames(&self) -> Vec<String>;
    fn convert(&self, gateway_name: &str) -> Result<Self::Config>;
    fn backend_refs(config: &Self::Config) -> Vec<(&str, u16, &str, &str)>;
    fn remove_invalid_backends(config: &mut Self::Config, invalid: &HashSet<(String, u16)>);
    /// Mirror-filter backend refs (HTTPRoute only). Validated like forward
    /// refs, but an invalid mirror drops the filter instead of the backends:
    /// a mirror the route may not reference must not break primary traffic.
    fn mirror_backend_refs(_config: &Self::Config) -> Vec<(&str, u16, &str, &str)> {
        vec![]
    }
    fn remove_invalid_mirrors(_config: &mut Self::Config, _invalid: &HashSet<(String, u16)>) {}
    /// The route's current `status.parents` entries as JSON values — exactly
    /// what serializing the whole route and reading `/status/parents` yields,
    /// without serializing the whole route.
    fn status_parents_json(&self) -> Vec<serde_json::Value>;
}

/// Implement GatewayRoute for a concrete route type. Only the three type-specific
/// expressions (parent_refs field path, hostnames body, convert function) differ;
/// the remaining five methods are identical across HTTP/TCP/TLS/UDP. `extras`
/// splices type-specific overrides of the defaulted trait methods into the impl.
macro_rules! impl_gateway_route {
    ($ty:ty, $parent:ty, $config:ty, $kind:literal,
     parent_refs: self.spec.$pr:ident,
     hostnames: $hn:expr,
     convert: $cv:path) => {
        impl_gateway_route!($ty, $parent, $config, $kind,
            parent_refs: self.spec.$pr,
            hostnames: $hn,
            convert: $cv,
            extras: {});
    };
    ($ty:ty, $parent:ty, $config:ty, $kind:literal,
     parent_refs: self.spec.$pr:ident,
     hostnames: $hn:expr,
     convert: $cv:path,
     extras: { $($extra:item)* }) => {
        impl GatewayRoute for $ty {
            type ParentRef = $parent;
            type Config = $config;
            const KIND: &'static str = $kind;

            fn parent_refs(&self) -> &Option<Vec<Self::ParentRef>> {
                &self.spec.$pr
            }
            fn route_namespace(&self) -> &str {
                route_namespace(&self.metadata)
            }
            fn identity(&self) -> (&str, Option<i64>) {
                (
                    self.metadata.name.as_deref().unwrap_or("unknown"),
                    self.metadata.generation,
                )
            }
            fn hostnames(&self) -> Vec<String> {
                $hn(self)
            }
            fn convert(&self, gw: &str) -> Result<Self::Config> {
                $cv(self, gw)
            }
            fn backend_refs(config: &Self::Config) -> Vec<(&str, u16, &str, &str)> {
                config
                    .rules
                    .iter()
                    .flat_map(|r| {
                        r.backend_refs
                            .iter()
                            .map(|b| (b.name.as_str(), b.port, b.group.as_str(), b.kind.as_str()))
                    })
                    .collect()
            }
            fn remove_invalid_backends(
                config: &mut Self::Config,
                invalid: &HashSet<(String, u16)>,
            ) {
                for rule in &mut config.rules {
                    rule.backend_refs
                        .retain(|b| !invalid.contains(&(b.name.clone(), b.port)));
                }
            }
            fn status_parents_json(&self) -> Vec<serde_json::Value> {
                self.status
                    .as_ref()
                    .map(|s| {
                        s.parents
                            .iter()
                            .filter_map(|p| serde_json::to_value(p).ok())
                            .collect()
                    })
                    .unwrap_or_default()
            }
            $($extra)*
        }
    };
}

impl_gateway_route!(HTTPRoute, HTTPRouteParentRefs, HttpRouteConfig, "HTTPRoute",
parent_refs: self.spec.parent_refs,
hostnames:   |s: &HTTPRoute| s.spec.hostnames.clone().unwrap_or_default(),
convert:     convert_http_route,
extras: {
    fn mirror_backend_refs(config: &HttpRouteConfig) -> Vec<(&str, u16, &str, &str)> {
        config
            .rules
            .iter()
            .flat_map(|r| {
                r.filters.iter().filter_map(|f| match f {
                    HttpRouteFilter::RequestMirror { config: mc } => Some((
                        mc.backend_ref.name.as_str(),
                        mc.backend_ref.port,
                        mc.backend_ref.group.as_str(),
                        mc.backend_ref.kind.as_str(),
                    )),
                    _ => None,
                })
            })
            .collect()
    }
    fn remove_invalid_mirrors(config: &mut HttpRouteConfig, invalid: &HashSet<(String, u16)>) {
        for rule in &mut config.rules {
            rule.filters.retain(|f| match f {
                HttpRouteFilter::RequestMirror { config: mc } => {
                    !invalid.contains(&(mc.backend_ref.name.clone(), mc.backend_ref.port))
                }
                _ => true,
            });
        }
    }
});

impl_gateway_route!(TCPRoute, TCPRouteParentRefs, TcpRouteConfig, "TCPRoute",
    parent_refs: self.spec.parent_refs,
    hostnames:   |_: &TCPRoute| vec![],
    convert:     convert_tcp_route);

impl_gateway_route!(TLSRoute, TLSRouteParentRefs, TlsRouteConfig, "TLSRoute",
    parent_refs: self.spec.parent_refs,
    hostnames:   |s: &TLSRoute| s.spec.hostnames.clone(),
    convert:     convert_tls_route);

impl_gateway_route!(UDPRoute, UDPRouteParentRefs, UdpRouteConfig, "UDPRoute",
    parent_refs: self.spec.parent_refs,
    hostnames:   |_: &UDPRoute| vec![],
    convert:     convert_udp_route);

pub struct ReconcileResult {
    pub config: PortailConfig,
    pub route_status: Vec<RouteAcceptance>,
}

#[derive(Debug)]
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
    /// The parentRef sectionName (for route status reporting)
    pub section_name: Option<String>,
    /// The parentRef port (status parentRef must mirror the spec's ref exactly)
    pub port: Option<i32>,
    /// The listener names this parentRef was accepted by (for attached route counting)
    pub listener_names: Vec<String>,
}

/// Convert Kubernetes Gateway API resources into a PortailConfig.
/// This reuses all existing validation, conversion, hostname intersection,
/// and regex compilation logic via `to_route_table()`.
pub(crate) fn reconcile_to_config(
    gateway: &Gateway,
    snapshot: &ClusterSnapshot,
    cert_data: &CertData,
    services: &ServiceState,
) -> Result<ReconcileResult> {
    let mut gateway_config = convert_gateway(gateway, cert_data)?;
    let gateway_name = gateway.metadata.name.as_deref().unwrap_or("default");
    let gateway_ns = gateway.metadata.namespace.as_deref().unwrap_or("default");

    // Bind the fronting Service's targetPort: the operator maps the published
    // (possibly privileged) port -> an unprivileged target the pod binds, so no
    // NET_BIND_SERVICE is needed. Empty in multi-network mode (no Service), where
    // listeners fall back to binding their published port directly.
    let target_ports = resolve_listener_target_ports(&snapshot.services, gateway_ns, gateway_name);
    for l in &mut gateway_config.listeners {
        l.target_port = target_ports.get(&l.port).copied();
    }

    let mut route_status = Vec::new();

    let http_route_configs = collect_routes::<HTTPRoute>(
        &snapshot.http_routes,
        gateway_name,
        gateway_ns,
        &gateway.spec.listeners,
        &snapshot.namespace_labels,
        &snapshot.reference_grants,
        &services.known_services,
        &mut route_status,
    );

    let tcp_route_configs = collect_routes::<TCPRoute>(
        &snapshot.tcp_routes,
        gateway_name,
        gateway_ns,
        &gateway.spec.listeners,
        &snapshot.namespace_labels,
        &snapshot.reference_grants,
        &services.known_services,
        &mut route_status,
    );

    let tls_route_configs = collect_routes::<TLSRoute>(
        &snapshot.tls_routes,
        gateway_name,
        gateway_ns,
        &gateway.spec.listeners,
        &snapshot.namespace_labels,
        &snapshot.reference_grants,
        &services.known_services,
        &mut route_status,
    );

    let udp_route_configs = collect_routes::<UDPRoute>(
        &snapshot.udp_routes,
        gateway_name,
        gateway_ns,
        &gateway.spec.listeners,
        &snapshot.namespace_labels,
        &snapshot.reference_grants,
        &services.known_services,
        &mut route_status,
    );

    Ok(ReconcileResult {
        config: PortailConfig {
            gateway: gateway_config,
            http_routes: http_route_configs,
            tcp_routes: tcp_route_configs,
            tls_routes: tls_route_configs,
            udp_routes: udp_route_configs,
            endpoint_overrides: services.endpoint_overrides.clone(),
            app_protocol_overrides: services.app_protocol_overrides.clone(),
            headless_target_ports: services.headless_target_ports.clone(),
            ..Default::default()
        },
        route_status,
    })
}

/// published listener port -> Service `targetPort` for the Service fronting this
/// Gateway, identified by the label `portail.epheo.eu/gateway == gw_name` in
/// `gw_ns`. The operator emits exactly one such LoadBalancer Service with numeric
/// targetPorts; in multi-network mode there is no Service and the result is empty
/// (listeners then bind their published port directly).
fn resolve_listener_target_ports(
    services: &[Arc<Service>],
    gw_ns: &str,
    gw_name: &str,
) -> HashMap<u16, u16> {
    use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

    let mut matching = services.iter().filter(|svc| {
        svc.metadata.namespace.as_deref() == Some(gw_ns)
            && svc
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("portail.epheo.eu/gateway"))
                .is_some_and(|v| v == gw_name)
    });

    let Some(svc) = matching.next() else {
        return HashMap::new();
    };
    if matching.next().is_some() {
        warn!(
            "Multiple Services labelled portail.epheo.eu/gateway={} in {}; using {:?}",
            gw_name,
            gw_ns,
            svc.metadata.name.as_deref().unwrap_or("?")
        );
    }

    let mut map = HashMap::new();
    let Some(spec) = svc.spec.as_ref() else {
        return map;
    };
    for p in spec.ports.iter().flatten() {
        let Ok(published) = u16::try_from(p.port) else {
            continue;
        };
        match &p.target_port {
            Some(IntOrString::Int(t)) => {
                if let Ok(target) = u16::try_from(*t) {
                    map.insert(published, target);
                }
            }
            Some(IntOrString::String(name)) => warn!(
                "Service for gateway {} uses named targetPort {:?} on port {}; binding the \
                 published port (operator should emit numeric targetPorts)",
                gw_name, name, published
            ),
            None => {} // omitted targetPort defaults to `port` -> no decoupling
        }
    }
    map
}

/// Generic route collection with namespace scoping and acceptance tracking.
fn collect_routes<R: GatewayRoute>(
    routes: &[Arc<R>],
    gateway_name: &str,
    gateway_ns: &str,
    listeners: &[GatewayListeners],
    namespace_labels: &HashMap<String, BTreeMap<String, String>>,
    reference_grants: &[ReferenceGrant],
    known_services: &HashSet<(String, String)>,
    route_status: &mut Vec<RouteAcceptance>,
) -> Vec<R::Config> {
    let kind = R::KIND;
    let mut configs = Vec::new();
    for route in routes {
        if !route_targets_gateway(route.parent_refs(), gateway_name, gateway_ns) {
            continue;
        }

        let route_ns = route.route_namespace();
        let (name, generation) = route.identity();

        // Evaluate each Gateway-matching parentRef independently: which
        // listeners it selects and whether they admit the route. One status
        // entry per parentRef — a ref pinned to a rejecting section must not
        // inherit Accepted from a sibling ref (previously every ref got the
        // route-wide verdict, and every entry claimed every listener).
        let route_hostnames = route.hostnames();
        let per_parent: Vec<(
            Option<String>,
            Option<i32>,
            Result<Vec<String>, &'static str>,
        )> = route
            .parent_refs()
            .as_ref()
            .map(|refs| {
                refs.iter()
                    .filter(|pr| parent_ref_matches_gateway(*pr, gateway_name, gateway_ns))
                    .map(|pr| {
                        (
                            pr.ref_section_name().map(String::from),
                            pr.ref_port(),
                            listeners_for_parent_ref(
                                pr,
                                listeners,
                                gateway_ns,
                                route_ns,
                                kind,
                                &route_hostnames,
                                namespace_labels,
                            ),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();

        let rejection = |section_name: &Option<String>, port, reason: &'static str| {
            let message = match reason {
                "NoMatchingListenerHostname" => {
                    "Route hostnames do not intersect with any listener hostname"
                }
                "NoMatchingParent" => "No matching listener found for route parentRef",
                _ => "Route not allowed by any listener policy (namespace/hostname/kind)",
            };
            RouteAcceptance {
                name: name.to_string(),
                namespace: route_ns.to_string(),
                kind,
                accepted: false,
                accepted_reason: reason.to_string(),
                message: message.to_string(),
                refs_resolved: true,
                refs_reason: "ResolvedRefs".to_string(),
                refs_message: "All references resolved".to_string(),
                generation,
                section_name: section_name.clone(),
                port,
                listener_names: vec![],
            }
        };

        if per_parent.iter().all(|(_, _, r)| r.is_err()) {
            for (section_name, port, result) in &per_parent {
                let reason = result.as_ref().err().copied().unwrap_or("NoMatchingParent");
                route_status.push(rejection(section_name, *port, reason));
            }
            continue;
        }

        match route.convert(gateway_name) {
            Ok(mut config) => {
                let mut refs_resolved = true;
                let mut refs_messages = Vec::new();
                let mut refs_reason = "ResolvedRefs".to_string();
                let mut invalid_backends: HashSet<(String, u16)> = HashSet::new();
                let mut invalid_mirrors: HashSet<(String, u16)> = HashSet::new();

                // Group/kind (core Service only), cross-namespace ReferenceGrant,
                // and service existence — one checker for forward and mirror refs.
                let check_ref = |backend_name: &str,
                                 group: &str,
                                 ref_kind: &str|
                 -> Option<(&'static str, String)> {
                    if (!group.is_empty() && group != "core")
                        || (ref_kind != "Service" && !ref_kind.is_empty())
                    {
                        return Some((
                            "InvalidKind",
                            format!("Unsupported backend ref group/kind: {}/{}", group, ref_kind),
                        ));
                    }
                    // backend_name is "{svc}.{ns}.svc"; unparseable names get no
                    // further checks (None here means "no error", not failure)
                    let (svc_name, svc_ns) = parse_backend_dns_name(backend_name)?;
                    if svc_ns != route_ns
                        && !is_reference_allowed(
                            reference_grants,
                            "gateway.networking.k8s.io",
                            kind,
                            route_ns,
                            "",
                            "Service",
                            &svc_ns,
                            &svc_name,
                        )
                    {
                        return Some((
                            "RefNotPermitted",
                            format!(
                                "Cross-namespace reference to {}.{} not allowed by ReferenceGrant",
                                svc_name, svc_ns
                            ),
                        ));
                    }
                    if !known_services.contains(&(svc_name.clone(), svc_ns.clone())) {
                        return Some((
                            "BackendNotFound",
                            format!("Backend service {}.{} not found", svc_name, svc_ns),
                        ));
                    }
                    None
                };

                for (backend_name, port, group, ref_kind) in R::backend_refs(&config) {
                    if let Some((reason, message)) = check_ref(backend_name, group, ref_kind) {
                        refs_resolved = false;
                        refs_reason = reason.to_string();
                        refs_messages.push(message);
                        invalid_backends.insert((backend_name.to_string(), port));
                    }
                }
                for (backend_name, port, group, ref_kind) in R::mirror_backend_refs(&config) {
                    if let Some((reason, message)) = check_ref(backend_name, group, ref_kind) {
                        refs_resolved = false;
                        refs_reason = reason.to_string();
                        refs_messages.push(message);
                        invalid_mirrors.insert((backend_name.to_string(), port));
                    }
                }

                // Remove invalid backends so the data plane returns 500 for them
                if !invalid_backends.is_empty() {
                    R::remove_invalid_backends(&mut config, &invalid_backends);
                }
                if !invalid_mirrors.is_empty() {
                    R::remove_invalid_mirrors(&mut config, &invalid_mirrors);
                }

                let refs_message = if refs_resolved {
                    "All references resolved".to_string()
                } else {
                    refs_messages.join("; ")
                };

                for (section_name, port, result) in &per_parent {
                    match result {
                        Ok(listener_names) => route_status.push(RouteAcceptance {
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
                            port: *port,
                            listener_names: listener_names.clone(),
                        }),
                        Err(reason) => route_status.push(rejection(section_name, *port, reason)),
                    }
                }
                configs.push(config);
            }
            Err(e) => {
                for (section_name, port, result) in &per_parent {
                    match result {
                        // A parentRef the listeners would admit: report the
                        // conversion failure. A ref rejected before conversion
                        // keeps its own rejection reason.
                        Ok(_) => route_status.push(RouteAcceptance {
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
                            port: *port,
                            listener_names: vec![],
                        }),
                        Err(reason) => route_status.push(rejection(section_name, *port, reason)),
                    }
                }
            }
        }
    }
    configs
}

#[cfg(test)]
mod tests {
    use super::super::converters::backend_dns_name;
    use super::*;
    use gateway_api::gateways::*;
    use gateway_api::referencegrants::{ReferenceGrantFrom, ReferenceGrantSpec, ReferenceGrantTo};
    use kube::core::ObjectMeta;

    fn default_ns_labels() -> HashMap<String, BTreeMap<String, String>> {
        let mut m = HashMap::new();
        m.insert("default".to_string(), BTreeMap::new());
        m.insert("db".to_string(), BTreeMap::new());
        m.insert("other-ns".to_string(), BTreeMap::new());
        m
    }

    fn test_known_services() -> HashSet<(String, String)> {
        [
            ("api-svc", "default"),
            ("web-svc", "default"),
            ("db-svc", "default"),
            ("my-svc", "default"),
            ("canary", "default"),
            ("stable", "default"),
            ("coredns", "default"),
            ("backend-tls", "default"),
            ("postgres-primary", "default"),
            ("postgres-replica", "db"),
            ("echo-svc", "default"),
            ("mirror-svc", "default"),
            ("svc", "default"),
            ("v1-svc", "default"),
            ("v2-svc", "default"),
            ("127.0.0.1", "default"),
        ]
        .iter()
        .map(|(s, n)| (s.to_string(), n.to_string()))
        .collect()
    }

    /// Build a test ClusterSnapshot with the given routes and optional reference grants.
    fn test_snapshot(
        http_routes: Vec<HTTPRoute>,
        tcp_routes: Vec<TCPRoute>,
        tls_routes: Vec<TLSRoute>,
        udp_routes: Vec<UDPRoute>,
        ns_labels: &HashMap<String, BTreeMap<String, String>>,
        reference_grants: Vec<ReferenceGrant>,
    ) -> ClusterSnapshot {
        ClusterSnapshot {
            http_routes: http_routes.into_iter().map(Arc::new).collect(),
            tcp_routes: tcp_routes.into_iter().map(Arc::new).collect(),
            tls_routes: tls_routes.into_iter().map(Arc::new).collect(),
            udp_routes: udp_routes.into_iter().map(Arc::new).collect(),
            namespace_labels: ns_labels.clone(),
            reference_grants,
            services: vec![],
        }
    }

    fn empty_services() -> ServiceState {
        ServiceState {
            known_services: test_known_services(),
            endpoint_overrides: HashMap::new(),
            app_protocol_overrides: HashMap::new(),
            headless_target_ports: HashMap::new(),
        }
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
                    if let crate::config::HttpRouteFilter::RequestMirror { config: ref mut mc } = f
                    {
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

        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();

        assert_eq!(result.config.gateway.name, "test-gw");
        assert_eq!(result.config.gateway.listeners.len(), 1);
        assert_eq!(result.config.gateway.listeners[0].port, 8080);
        assert_eq!(result.config.http_routes.len(), 1);
        assert_eq!(
            result.config.http_routes[0].hostnames,
            vec!["api.example.com"]
        );
        assert_eq!(
            result.config.http_routes[0].rules[0].backend_refs[0].name,
            "api-svc.default.svc"
        );
        assert_eq!(
            result.config.http_routes[0].rules[0].backend_refs[0].port,
            3000
        );
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
            &test_snapshot(
                vec![matching_route, other_route],
                vec![],
                vec![],
                vec![],
                &ns_labels,
                vec![],
            ),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();

        assert_eq!(result.config.http_routes.len(), 1);
        assert_eq!(
            result.config.http_routes[0].hostnames,
            vec!["api.example.com"]
        );
    }

    #[test]
    fn test_reconcile_produces_valid_config() {
        let gw = test_gateway();
        let route = test_http_route();
        let ns_labels = default_ns_labels();

        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();

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

        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();
        assert_eq!(result.config.http_routes.len(), 0);
        assert!(!result.route_status[0].accepted);
    }

    #[test]
    fn test_namespace_scoping_all() {
        use gateway_api::gateways::{
            GatewayListenersAllowedRoutes, GatewayListenersAllowedRoutesNamespaces,
        };

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

        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();
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

        let result = reconcile_to_config(
            &gw,
            &test_snapshot(
                vec![route.clone()],
                vec![],
                vec![],
                vec![],
                &ns_labels,
                vec![],
            ),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();
        assert_eq!(result.config.http_routes.len(), 1);

        ns_labels.insert(
            "prod-ns".to_string(),
            BTreeMap::from([("env".to_string(), "staging".to_string())]),
        );
        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();
        assert_eq!(result.config.http_routes.len(), 0);
    }

    // ---- No routes edge case ----

    #[test]
    fn test_reconcile_no_routes() {
        let gw = test_gateway();
        let ns_labels = default_ns_labels();
        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();

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
        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![orphan], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();
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
        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();

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
        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();

        let https = &result.config.gateway.listeners[1];
        let tls = https.tls.as_ref().unwrap();
        assert_eq!(tls.mode, TlsMode::Terminate);
        assert_eq!(tls.certificate_refs[0].name, "my-cert");
    }

    #[test]
    fn test_tls_passthrough_listener() {
        let gw = multi_listener_gateway();
        let ns_labels = default_ns_labels();
        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();

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
        assert!(reconcile_to_config(
            &gw,
            &test_snapshot(vec![], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .is_err());
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

        // ReferenceGrant allowing TCPRoute in default NS to reference Service in db NS
        let ref_grant = ReferenceGrant {
            metadata: ObjectMeta {
                name: Some("allow-db".to_string()),
                namespace: Some("db".to_string()),
                ..Default::default()
            },
            spec: ReferenceGrantSpec {
                from: vec![ReferenceGrantFrom {
                    group: "gateway.networking.k8s.io".to_string(),
                    kind: "TCPRoute".to_string(),
                    namespace: "default".to_string(),
                }],
                to: vec![ReferenceGrantTo {
                    group: "".to_string(),
                    kind: "Service".to_string(),
                    name: None,
                }],
            },
        };

        let result = reconcile_to_config(
            &gw,
            &test_snapshot(
                vec![],
                vec![tcp_route],
                vec![],
                vec![],
                &ns_labels,
                vec![ref_grant],
            ),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();

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
                hostnames: vec![
                    "secure.example.com".to_string(),
                    "*.internal.example.com".to_string(),
                ],
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

        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![], vec![], vec![tls_route], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();

        assert_eq!(result.config.tls_routes.len(), 1);
        assert_eq!(
            result.config.tls_routes[0].hostnames,
            vec!["secure.example.com", "*.internal.example.com"]
        );
        assert_eq!(
            result.config.tls_routes[0].rules[0].backend_refs[0].name,
            "backend-tls.default.svc"
        );
        assert_eq!(
            result.config.tls_routes[0].rules[0].backend_refs[0].port,
            8443
        );
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

        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![], vec![], vec![], vec![udp_route], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();

        assert_eq!(result.config.udp_routes.len(), 1);
        assert_eq!(
            result.config.udp_routes[0].rules[0].backend_refs[0].name,
            "coredns.default.svc"
        );
        assert_eq!(
            result.config.udp_routes[0].rules[0].backend_refs[0].port,
            53
        );
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

        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();
        let m = &result.config.http_routes[0].rules[0].matches[0];
        assert_eq!(
            m.path.as_ref().unwrap().match_type,
            HttpPathMatchType::Exact
        );
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

        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();
        let m = &result.config.http_routes[0].rules[0].matches[0];
        assert_eq!(
            m.path.as_ref().unwrap().match_type,
            HttpPathMatchType::RegularExpression
        );
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

        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();
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
                                r#type: Some(
                                    HTTPRouteRulesMatchesQueryParamsType::RegularExpression,
                                ),
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

        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();
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

        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();
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

        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();
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

        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();
        assert_eq!(result.config.http_routes[0].rules.len(), 2);
        assert_eq!(
            result.config.http_routes[0].rules[0].backend_refs[0].name,
            "v1-svc.default.svc"
        );
        assert_eq!(
            result.config.http_routes[0].rules[1].backend_refs[0].name,
            "v2-svc.default.svc"
        );
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

        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();
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
                        response_header_modifier: Some(
                            HTTPRouteRulesFiltersResponseHeaderModifier {
                                add: Some(vec![HTTPRouteRulesFiltersResponseHeaderModifierAdd {
                                    name: "X-Response".to_string(),
                                    value: "modified".to_string(),
                                }]),
                                set: None,
                                remove: Some(vec!["Server".to_string()]),
                            },
                        ),
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

        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();
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
                                r#type:
                                    HTTPRouteRulesFiltersRequestRedirectPathType::ReplaceFullPath,
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

        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();
        match &result.config.http_routes[0].rules[0].filters[0] {
            HttpRouteFilter::RequestRedirect { config } => {
                assert_eq!(config.scheme.as_deref(), Some("https"));
                assert_eq!(config.hostname.as_deref(), Some("secure.example.com"));
                assert_eq!(config.port, Some(443));
                assert_eq!(config.status_code, 301);
                assert!(
                    matches!(&config.path, Some(HttpURLRewritePath::ReplaceFullPath { value }) if value == "/new-path")
                );
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

        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();
        match &result.config.http_routes[0].rules[0].filters[0] {
            HttpRouteFilter::URLRewrite { config } => {
                assert_eq!(config.hostname.as_deref(), Some("internal.example.com"));
                assert!(
                    matches!(&config.path, Some(HttpURLRewritePath::ReplacePrefixMatch { value }) if value == "/new")
                );
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
                                name: "mirror-svc".to_string(),
                                port: Some(9090),
                                ..Default::default()
                            },
                            ..Default::default()
                        }),
                        ..Default::default()
                    }]),
                    backend_refs: Some(vec![HTTPRouteRulesBackendRefs {
                        name: "api-svc".to_string(),
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
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();
        match &result.config.http_routes[0].rules[0].filters[0] {
            HttpRouteFilter::RequestMirror { config } => {
                assert_eq!(config.backend_ref.name, "mirror-svc.default.svc");
                assert_eq!(config.backend_ref.port, 9090);
            }
            _ => panic!("expected RequestMirror"),
        }
    }

    /// A route with one matching and one dangling parentRef must get one
    /// Accepted and one rejected status entry — previously every parentRef
    /// inherited the route-wide verdict, so the dangling ref read Accepted.
    #[test]
    fn test_multi_parentref_mixed_acceptance() {
        let gw = test_gateway();
        let ns_labels = default_ns_labels();
        let mut route = test_http_route();
        route
            .spec
            .parent_refs
            .as_mut()
            .unwrap()
            .push(HTTPRouteParentRefs {
                name: "test-gw".to_string(),
                section_name: Some("nonexistent".to_string()),
                ..Default::default()
            });

        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();

        assert_eq!(result.route_status.len(), 2);
        let ok = result
            .route_status
            .iter()
            .find(|ra| ra.section_name.as_deref() == Some("http"))
            .unwrap();
        assert!(ok.accepted);
        assert_eq!(ok.listener_names, vec!["http".to_string()]);

        let dangling = result
            .route_status
            .iter()
            .find(|ra| ra.section_name.as_deref() == Some("nonexistent"))
            .unwrap();
        assert!(!dangling.accepted);
        assert_eq!(dangling.accepted_reason, "NoMatchingParent");
        assert!(dangling.listener_names.is_empty());

        // The route still serves via its valid parentRef.
        assert_eq!(result.config.http_routes.len(), 1);
    }

    /// Cross-namespace mirror ref helper: HTTPRoute in default mirroring to
    /// postgres-replica in db (a known service).
    fn mirror_route_to_db() -> HTTPRoute {
        HTTPRoute {
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
                                name: "postgres-replica".to_string(),
                                port: Some(9090),
                                namespace: Some("db".to_string()),
                                ..Default::default()
                            },
                            ..Default::default()
                        }),
                        ..Default::default()
                    }]),
                    backend_refs: Some(vec![HTTPRouteRulesBackendRefs {
                        name: "api-svc".to_string(),
                        port: Some(80),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
            },
            status: None,
        }
    }

    fn http_mirror_grant() -> ReferenceGrant {
        ReferenceGrant {
            metadata: ObjectMeta {
                name: Some("allow-mirror".to_string()),
                namespace: Some("db".to_string()),
                ..Default::default()
            },
            spec: ReferenceGrantSpec {
                from: vec![ReferenceGrantFrom {
                    group: "gateway.networking.k8s.io".to_string(),
                    kind: "HTTPRoute".to_string(),
                    namespace: "default".to_string(),
                }],
                to: vec![ReferenceGrantTo {
                    group: "".to_string(),
                    kind: "Service".to_string(),
                    name: None,
                }],
            },
        }
    }

    /// A cross-namespace mirror without a ReferenceGrant previously bypassed
    /// grant validation entirely — any route could shadow traffic into any
    /// namespace. The mirror must be dropped (primary traffic unaffected)
    /// and reported as RefNotPermitted.
    #[test]
    fn test_mirror_cross_ns_without_grant_dropped() {
        let gw = test_gateway();
        let ns_labels = default_ns_labels();
        let result = reconcile_to_config(
            &gw,
            &test_snapshot(
                vec![mirror_route_to_db()],
                vec![],
                vec![],
                vec![],
                &ns_labels,
                vec![],
            ),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();

        let rule = &result.config.http_routes[0].rules[0];
        assert!(rule.filters.is_empty(), "ungranted mirror must be dropped");
        assert_eq!(rule.backend_refs.len(), 1, "primary backends untouched");

        let ra = &result.route_status[0];
        assert!(ra.accepted, "route itself stays accepted");
        assert!(!ra.refs_resolved);
        assert_eq!(ra.refs_reason, "RefNotPermitted");
    }

    #[test]
    fn test_mirror_cross_ns_with_grant_kept() {
        let gw = test_gateway();
        let ns_labels = default_ns_labels();
        let result = reconcile_to_config(
            &gw,
            &test_snapshot(
                vec![mirror_route_to_db()],
                vec![],
                vec![],
                vec![],
                &ns_labels,
                vec![http_mirror_grant()],
            ),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();

        let rule = &result.config.http_routes[0].rules[0];
        assert_eq!(rule.filters.len(), 1, "granted mirror is kept");
        assert!(result.route_status[0].refs_resolved);
    }

    #[test]
    fn test_mirror_to_unknown_service_dropped() {
        let gw = test_gateway();
        let ns_labels = default_ns_labels();
        let mut route = mirror_route_to_db();
        if let Some(rules) = route.spec.rules.as_mut() {
            let f = rules[0].filters.as_mut().unwrap();
            let rm = f[0].request_mirror.as_mut().unwrap();
            rm.backend_ref.name = "ghost-svc".to_string();
            rm.backend_ref.namespace = None;
        }
        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();

        let rule = &result.config.http_routes[0].rules[0];
        assert!(rule.filters.is_empty());
        let ra = &result.route_status[0];
        assert!(!ra.refs_resolved);
        assert_eq!(ra.refs_reason, "BackendNotFound");
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

        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();
        let timeouts = result.config.http_routes[0].rules[0]
            .timeouts
            .as_ref()
            .unwrap();
        assert_eq!(timeouts.request, Some(std::time::Duration::from_secs(30)));
        assert_eq!(
            timeouts.backend_request,
            Some(std::time::Duration::from_secs(10))
        );
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

        let result = reconcile_to_config(
            &gw,
            &test_snapshot(
                vec![http_route],
                vec![tcp_route],
                vec![],
                vec![],
                &ns_labels,
                vec![],
            ),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();

        assert_eq!(result.config.http_routes.len(), 1);
        assert_eq!(result.config.tcp_routes.len(), 1);
        assert_eq!(
            result.config.http_routes[0].rules[0].backend_refs[0].name,
            "web-svc.default.svc"
        );
        assert_eq!(
            result.config.tcp_routes[0].rules[0].backend_refs[0].name,
            "db-svc.default.svc"
        );
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
        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();
        let mut config = result.config;
        fixup_backends_for_test(&mut config);
        let rt = config.to_route_table().unwrap();

        let rule = rt
            .find_http_route("app.example.com", "GET", "/health", &[], "", 8080)
            .unwrap();
        assert_eq!(rule.backends[0].socket_addr.port(), 3001);

        let rule = rt
            .find_http_route("app.example.com", "GET", "/api/users", &[], "", 8080)
            .unwrap();
        assert_eq!(rule.backends[0].socket_addr.port(), 3000);

        assert!(rt
            .find_http_route("other.com", "GET", "/api", &[], "", 8080)
            .is_err());
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
        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![], vec![tcp_route], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();
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
        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();
        let mut config = result.config;
        fixup_backends_for_test(&mut config);
        let rt = config.to_route_table().unwrap();

        let rule = rt
            .find_http_route("example.com", "GET", "/anything", &[], "", 8080)
            .unwrap();
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
        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();
        let mut config = result.config;
        fixup_backends_for_test(&mut config);
        let rt = config.to_route_table().unwrap();

        let rule = rt
            .find_http_route("app.com", "GET", "/", &[], "", 8080)
            .unwrap();
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

        let result = reconcile_to_config(
            &gw,
            &test_snapshot(vec![route], vec![], vec![], vec![], &ns_labels, vec![]),
            &HashMap::new(),
            &empty_services(),
        )
        .unwrap();

        assert_eq!(result.route_status.len(), 1);
        assert!(result.route_status[0].accepted);
        assert_eq!(result.route_status[0].kind, "HTTPRoute");
        assert_eq!(result.route_status[0].name, "test-route");
        assert_eq!(result.route_status[0].namespace, "default");
    }

    // --- fronting-Service targetPort resolution ---

    use k8s_openapi::api::core::v1::{ServicePort, ServiceSpec};
    use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

    fn fronting_service(ns: &str, gw: &str, ports: &[(i32, Option<IntOrString>)]) -> Service {
        let mut labels = BTreeMap::new();
        labels.insert("portail.epheo.eu/gateway".to_string(), gw.to_string());
        Service {
            metadata: ObjectMeta {
                name: Some(format!("portail-{gw}")),
                namespace: Some(ns.to_string()),
                labels: Some(labels),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                ports: Some(
                    ports
                        .iter()
                        .map(|(p, t)| ServicePort {
                            port: *p,
                            target_port: t.clone(),
                            ..Default::default()
                        })
                        .collect(),
                ),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn target_ports_maps_privileged_and_unprivileged() {
        let svc = fronting_service(
            "default",
            "gw",
            &[
                (80, Some(IntOrString::Int(8000))),
                (443, Some(IntOrString::Int(8001))),
                (8080, Some(IntOrString::Int(8080))),
            ],
        );
        let map = resolve_listener_target_ports(&[Arc::new(svc)], "default", "gw");
        assert_eq!(map.get(&80), Some(&8000));
        assert_eq!(map.get(&443), Some(&8001));
        assert_eq!(map.get(&8080), Some(&8080));
    }

    #[test]
    fn target_ports_empty_without_matching_service() {
        let wrong_gw = fronting_service("default", "other", &[(80, Some(IntOrString::Int(8000)))]);
        assert!(resolve_listener_target_ports(&[Arc::new(wrong_gw)], "default", "gw").is_empty());

        let wrong_ns = fronting_service("elsewhere", "gw", &[(80, Some(IntOrString::Int(8000)))]);
        assert!(resolve_listener_target_ports(&[Arc::new(wrong_ns)], "default", "gw").is_empty());

        assert!(resolve_listener_target_ports(&[], "default", "gw").is_empty());
    }

    #[test]
    fn target_ports_skips_named_target_port() {
        let svc = fronting_service(
            "default",
            "gw",
            &[
                (80, Some(IntOrString::String("http".to_string()))),
                (443, Some(IntOrString::Int(8001))),
            ],
        );
        let map = resolve_listener_target_ports(&[Arc::new(svc)], "default", "gw");
        assert_eq!(map.get(&80), None);
        assert_eq!(map.get(&443), Some(&8001));
    }

    #[test]
    fn reconcile_stamps_listener_target_port_from_fronting_service() {
        let gw = test_gateway(); // listener "http" published on 8080
        let snapshot = ClusterSnapshot {
            http_routes: vec![],
            tcp_routes: vec![],
            tls_routes: vec![],
            udp_routes: vec![],
            namespace_labels: default_ns_labels(),
            reference_grants: vec![],
            services: vec![Arc::new(fronting_service(
                "default",
                "test-gw",
                &[(8080, Some(IntOrString::Int(18080)))],
            ))],
        };
        let result =
            reconcile_to_config(&gw, &snapshot, &HashMap::new(), &empty_services()).unwrap();
        let http = result
            .config
            .gateway
            .listeners
            .iter()
            .find(|l| l.port == 8080)
            .expect("http listener present");
        assert_eq!(
            http.target_port,
            Some(18080),
            "listener must carry the fronting Service's targetPort"
        );
    }

    /// `status_parents_json` must yield exactly what serializing the whole
    /// route and reading `/status/parents` used to yield — the fingerprint's
    /// clobber detection depends on that equivalence.
    #[test]
    fn status_parents_json_matches_full_route_serialization() {
        let mut route = test_http_route();
        assert!(
            route.status_parents_json().is_empty(),
            "no status -> no parents"
        );

        route.status = Some(HTTPRouteStatus {
            parents: vec![HTTPRouteStatusParents {
                controller_name: "epheo.eu/portail".to_string(),
                parent_ref: HTTPRouteStatusParentsParentRef {
                    name: "test-gw".to_string(),
                    section_name: Some("http".to_string()),
                    ..Default::default()
                },
                conditions: vec![],
            }],
        });

        let via_full = serde_json::to_value(&route)
            .unwrap()
            .pointer("/status/parents")
            .and_then(|v| v.as_array().cloned())
            .unwrap();
        assert_eq!(route.status_parents_json(), via_full);
    }
}
