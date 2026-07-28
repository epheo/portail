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
use super::crds::RateLimitPolicy;
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
    /// Pre-filtered to policies targeting this Gateway, like routes.
    pub rate_limit_policies: Vec<Arc<RateLimitPolicy>>,
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
    /// Default covers the hostname-less kinds (TCP/UDP).
    fn hostnames(&self) -> &[String] {
        &[]
    }
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

/// Implement GatewayRoute for a concrete route type. Only the two type-specific
/// expressions (parent_refs field path, convert function) differ; the remaining
/// methods are identical across HTTP/TCP/TLS/UDP. `extras` splices type-specific
/// overrides of the defaulted trait methods (hostnames, mirrors) into the impl.
macro_rules! impl_gateway_route {
    ($ty:ty, $parent:ty, $config:ty, $kind:literal,
     parent_refs: self.spec.$pr:ident,
     convert: $cv:path) => {
        impl_gateway_route!($ty, $parent, $config, $kind,
            parent_refs: self.spec.$pr,
            convert: $cv,
            extras: {});
    };
    ($ty:ty, $parent:ty, $config:ty, $kind:literal,
     parent_refs: self.spec.$pr:ident,
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
convert:     convert_http_route,
extras: {
    fn hostnames(&self) -> &[String] {
        self.spec.hostnames.as_deref().unwrap_or(&[])
    }
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
    convert:     convert_tcp_route);

impl_gateway_route!(TLSRoute, TLSRouteParentRefs, TlsRouteConfig, "TLSRoute",
parent_refs: self.spec.parent_refs,
convert:     convert_tls_route,
extras: {
    fn hostnames(&self) -> &[String] {
        &self.spec.hostnames
    }
});

impl_gateway_route!(UDPRoute, UDPRouteParentRefs, UdpRouteConfig, "UDPRoute",
    parent_refs: self.spec.parent_refs,
    convert:     convert_udp_route);

pub struct ReconcileResult {
    pub config: PortailConfig,
    pub route_status: Vec<RouteAcceptance>,
    pub policy_status: Vec<PolicyAcceptance>,
}

/// Computed Accepted condition for one RateLimitPolicy against this Gateway.
#[derive(Debug)]
pub struct PolicyAcceptance {
    pub name: String,
    pub namespace: String,
    pub accepted: bool,
    pub reason: &'static str,
    pub message: String,
    pub generation: Option<i64>,
}

#[derive(Debug)]
pub struct RouteAcceptance {
    pub name: String,
    pub namespace: String,
    pub kind: &'static str,
    pub accepted: bool,
    pub accepted_reason: &'static str,
    pub message: String,
    pub refs_resolved: bool,
    pub refs_reason: &'static str,
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

    let policy_status = apply_rate_limit_policies(
        &mut gateway_config.listeners,
        &snapshot.rate_limit_policies,
        gateway_ns,
        gateway_name,
    );

    let mut route_status = Vec::new();

    let cx = RouteCollectCtx {
        gateway_name,
        gateway_ns,
        listeners: &gateway.spec.listeners,
        namespace_labels: &snapshot.namespace_labels,
        reference_grants: &snapshot.reference_grants,
        known_services: &services.known_services,
    };
    let http_route_configs =
        collect_routes::<HTTPRoute>(&snapshot.http_routes, &cx, &mut route_status);
    let tcp_route_configs =
        collect_routes::<TCPRoute>(&snapshot.tcp_routes, &cx, &mut route_status);
    let tls_route_configs =
        collect_routes::<TLSRoute>(&snapshot.tls_routes, &cx, &mut route_status);
    let udp_route_configs =
        collect_routes::<UDPRoute>(&snapshot.udp_routes, &cx, &mut route_status);

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
        policy_status,
    })
}

/// Resolve RateLimitPolicies onto this Gateway's listeners (GEP-713 direct
/// attachment). A `sectionName`-scoped policy beats a Gateway-wide one for
/// its listener; among equal specificity the OLDEST policy wins
/// (creationTimestamp, then name — the GEP-713 conflict rule) and later
/// ones report Conflicted. Runs before the unscoped-mode merge prefixes
/// listener names, so sectionName matches the Gateway's own listener names.
pub(crate) fn apply_rate_limit_policies(
    listeners: &mut [ListenerConfig],
    policies: &[Arc<RateLimitPolicy>],
    gw_ns: &str,
    gw_name: &str,
) -> Vec<PolicyAcceptance> {
    use kube::ResourceExt;

    let mut ordered: Vec<&Arc<RateLimitPolicy>> = policies
        .iter()
        .filter(|p| p.targets_gateway(gw_ns, gw_name))
        .collect();
    ordered.sort_by_key(|p| (p.metadata.creation_timestamp.clone(), p.name_any()));

    // Winner per listener: first (oldest) sectionName match, else first
    // Gateway-wide policy.
    let mut applied: HashSet<String> = HashSet::new();
    for l in listeners.iter_mut() {
        let winner = ordered
            .iter()
            .find(|p| p.spec.target_ref.section_name.as_deref() == Some(l.name.as_str()))
            .or_else(|| {
                ordered
                    .iter()
                    .find(|p| p.spec.target_ref.section_name.is_none())
            });
        if let Some(p) = winner {
            l.rate_limit = Some(RateLimitSpec {
                requests_per_second: p.spec.requests_per_second,
                burst: Some(p.effective_burst()),
            });
            applied.insert(p.name_any());
        }
    }

    ordered
        .iter()
        .map(|p| {
            let name = p.name_any();
            let (accepted, reason, message) = match &p.spec.target_ref.section_name {
                Some(section) if !listeners.iter().any(|l| l.name == *section) => (
                    false,
                    "TargetNotFound",
                    format!("Gateway has no listener named '{}'", section),
                ),
                _ if !applied.contains(&name) => (
                    false,
                    "Conflicted",
                    "An older policy already limits every listener this policy targets".to_string(),
                ),
                _ => (true, "Accepted", "Rate limit programmed".to_string()),
            };
            PolicyAcceptance {
                name,
                namespace: p.namespace().unwrap_or_default(),
                accepted,
                reason,
                message,
                generation: p.metadata.generation,
            }
        })
        .collect()
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    use crate::kubernetes::crds::{PolicyTargetRef, RateLimitPolicySpec};
    use kube::ResourceExt;

    fn listener(name: &str, port: u16) -> ListenerConfig {
        ListenerConfig {
            name: name.to_string(),
            protocol: Protocol::HTTP,
            port,
            target_port: None,
            hostname: None,
            address: None,
            interface: None,
            tls: None,
            rate_limit: None,
        }
    }

    fn policy(name: &str, section: Option<&str>, rps: u32, age_secs: i64) -> Arc<RateLimitPolicy> {
        let mut p = RateLimitPolicy::new(
            name,
            RateLimitPolicySpec {
                target_ref: PolicyTargetRef {
                    group: "gateway.networking.k8s.io".into(),
                    kind: "Gateway".into(),
                    name: "gw".into(),
                    section_name: section.map(str::to_string),
                },
                requests_per_second: rps,
                burst: None,
            },
        );
        p.metadata.namespace = Some("edge".into());
        p.metadata.creation_timestamp = Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
            k8s_openapi::chrono::DateTime::from_timestamp(age_secs, 0).unwrap(),
        ));
        Arc::new(p)
    }

    #[test]
    fn section_scoped_beats_gateway_wide() {
        let mut listeners = vec![listener("web", 443), listener("alt", 8443)];
        let policies = vec![
            policy("wide", None, 10, 100),
            policy("web-only", Some("web"), 2, 200),
        ];
        let status = apply_rate_limit_policies(&mut listeners, &policies, "edge", "gw");
        assert_eq!(
            listeners[0]
                .rate_limit
                .as_ref()
                .unwrap()
                .requests_per_second,
            2
        );
        assert_eq!(
            listeners[1]
                .rate_limit
                .as_ref()
                .unwrap()
                .requests_per_second,
            10
        );
        assert!(status.iter().all(|pa| pa.accepted));
    }

    #[test]
    fn oldest_policy_wins_conflicts() {
        let mut listeners = vec![listener("web", 443)];
        let policies = vec![policy("newer", None, 5, 200), policy("older", None, 1, 100)];
        let status = apply_rate_limit_policies(&mut listeners, &policies, "edge", "gw");
        assert_eq!(
            listeners[0]
                .rate_limit
                .as_ref()
                .unwrap()
                .requests_per_second,
            1
        );
        let by_name = |n: &str| status.iter().find(|pa| pa.name == n).unwrap();
        assert!(by_name("older").accepted);
        assert!(!by_name("newer").accepted);
        assert_eq!(by_name("newer").reason, "Conflicted");
    }

    #[test]
    fn unknown_section_reports_target_not_found() {
        let mut listeners = vec![listener("web", 443)];
        let policies = vec![policy("typo", Some("wbe"), 2, 100)];
        let status = apply_rate_limit_policies(&mut listeners, &policies, "edge", "gw");
        assert!(listeners[0].rate_limit.is_none());
        assert!(!status[0].accepted);
        assert_eq!(status[0].reason, "TargetNotFound");
    }

    #[test]
    fn wrong_gateway_is_ignored_entirely() {
        let mut listeners = vec![listener("web", 443)];
        let policies = vec![policy("other", None, 2, 100)];
        let status = apply_rate_limit_policies(&mut listeners, &policies, "edge", "not-gw");
        assert!(listeners[0].rate_limit.is_none());
        assert!(status.is_empty());
        // Sanity: the fixture policy does target "gw".
        assert!(policies[0].targets_gateway("edge", "gw"));
        assert_eq!(policies[0].name_any(), "other");
    }
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

/// Per-Gateway inputs shared by every `collect_routes` call of one pass.
struct RouteCollectCtx<'a> {
    gateway_name: &'a str,
    gateway_ns: &'a str,
    listeners: &'a [GatewayListeners],
    namespace_labels: &'a HashMap<String, BTreeMap<String, String>>,
    reference_grants: &'a [ReferenceGrant],
    known_services: &'a HashSet<(String, String)>,
}

/// Generic route collection with namespace scoping and acceptance tracking.
fn collect_routes<R: GatewayRoute>(
    routes: &[Arc<R>],
    cx: &RouteCollectCtx,
    route_status: &mut Vec<RouteAcceptance>,
) -> Vec<R::Config> {
    let kind = R::KIND;
    let mut configs = Vec::new();
    for route in routes {
        if !route_targets_gateway(route.parent_refs(), cx.gateway_name, cx.gateway_ns) {
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
                    .filter(|pr| parent_ref_matches_gateway(*pr, cx.gateway_name, cx.gateway_ns))
                    .map(|pr| {
                        (
                            pr.ref_section_name().map(String::from),
                            pr.ref_port(),
                            listeners_for_parent_ref(
                                pr,
                                cx.listeners,
                                cx.gateway_ns,
                                route_ns,
                                kind,
                                route_hostnames,
                                cx.namespace_labels,
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
                accepted_reason: reason,
                message: message.to_string(),
                refs_resolved: true,
                refs_reason: "ResolvedRefs",
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

        match route.convert(cx.gateway_name) {
            Ok(mut config) => {
                let mut refs_resolved = true;
                let mut refs_messages = Vec::new();
                let mut refs_reason = "ResolvedRefs";
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
                            cx.reference_grants,
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
                    if !cx
                        .known_services
                        .contains(&(svc_name.clone(), svc_ns.clone()))
                    {
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
                        refs_reason = reason;
                        refs_messages.push(message);
                        invalid_backends.insert((backend_name.to_string(), port));
                    }
                }
                for (backend_name, port, group, ref_kind) in R::mirror_backend_refs(&config) {
                    if let Some((reason, message)) = check_ref(backend_name, group, ref_kind) {
                        refs_resolved = false;
                        refs_reason = reason;
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
                            accepted_reason: "Accepted",
                            message: "Accepted".to_string(),
                            refs_resolved,
                            refs_reason,
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
                            accepted_reason: "InvalidRoute",
                            message: format!("Conversion failed: {}", e),
                            refs_resolved: false,
                            refs_reason: "InvalidKind",
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
mod tests;
