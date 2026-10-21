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
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;

use crate::config::compile::compile_http_route;
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
    /// TCP and UDP listeners carry one route: their rules have no match to
    /// choose between routes, so the oldest takes the listener
    /// (Listener.allowedRoutes precedence) and later ones report Conflicted.
    const ONE_ROUTE_PER_LISTENER: bool = false;

    fn parent_refs(&self) -> &Option<Vec<Self::ParentRef>>;
    fn route_namespace(&self) -> &str;
    fn identity(&self) -> (&str, Option<i64>);
    fn uid(&self) -> Option<&str>;
    fn creation_timestamp(&self) -> Option<&Time>;
    /// Default covers the hostname-less kinds (TCP/UDP).
    fn hostnames(&self) -> &[String] {
        &[]
    }
    /// The converted config carries no parentRefs: see `attach`. `Err`
    /// names a part the config could not represent and leaves out; every
    /// backendRef is still in it.
    fn convert(&self) -> (Self::Config, Result<(), RouteError>);
    /// The table build's own check of a converted route: acceptance then
    /// rejects, with the spec reason, every route the build would leave out.
    fn check(_config: &Self::Config) -> Result<(), RouteError> {
        Ok(())
    }
    /// The one source of attachment for the table: one ref per listener
    /// the route was accepted on, never re-derived from the spec's refs.
    fn attach(config: &mut Self::Config, parent_refs: Vec<ParentRef>);
    fn backend_refs_mut(config: &mut Self::Config) -> Vec<&mut BackendRef>;
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
            fn uid(&self) -> Option<&str> {
                self.metadata.uid.as_deref()
            }
            fn creation_timestamp(&self) -> Option<&Time> {
                self.metadata.creation_timestamp.as_ref()
            }
            fn convert(&self) -> (Self::Config, Result<(), RouteError>) {
                $cv(self)
            }
            fn attach(config: &mut Self::Config, parent_refs: Vec<ParentRef>) {
                config.parent_refs = parent_refs;
            }
            fn backend_refs_mut(config: &mut Self::Config) -> Vec<&mut BackendRef> {
                config
                    .rules
                    .iter_mut()
                    .flat_map(|r| r.backend_refs.iter_mut())
                    .collect()
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
    fn check(config: &HttpRouteConfig) -> Result<(), RouteError> {
        compile_http_route(config).map(drop)
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
convert:     convert_tcp_route,
extras: {
    const ONE_ROUTE_PER_LISTENER: bool = true;
});

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
convert:     convert_udp_route,
extras: {
    const ONE_ROUTE_PER_LISTENER: bool = true;
});

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
    /// The listeners this parentRef attaches to by allowedRoutes, whether or
    /// not the route is accepted on them (Listener.attachedRoutes counts them)
    pub listener_names: Vec<String>,
}

type Verdict = Result<(), RouteError>;

/// Route check verdicts per Gateway, by route uid, with the generation they
/// were taken at. The check compiles every regex of a route; a pass whose
/// routes did not change (a status echo, a requeue) takes the verdict
/// instead. The table build still compiles for the table.
#[derive(Default)]
pub(crate) struct RouteVerdicts(
    std::sync::Mutex<HashMap<(String, String), HashMap<String, (i64, Verdict)>>>,
);

impl RouteVerdicts {
    fn take(&self, gateway: &(String, String)) -> HashMap<String, (i64, Verdict)> {
        let mut by_gateway = self.0.lock().unwrap_or_else(|p| p.into_inner());
        by_gateway.remove(gateway).unwrap_or_default()
    }

    fn put(&self, gateway: (String, String), verdicts: HashMap<String, (i64, Verdict)>) {
        let mut by_gateway = self.0.lock().unwrap_or_else(|p| p.into_inner());
        by_gateway.insert(gateway, verdicts);
    }

    /// Forget the Gateways `live` rejects.
    pub fn retain(&self, live: impl Fn(&(String, String)) -> bool) {
        let mut by_gateway = self.0.lock().unwrap_or_else(|p| p.into_inner());
        by_gateway.retain(|gateway, _| live(gateway));
    }
}

/// One pass's verdicts: the last pass's while a route's generation stands,
/// a new check otherwise. A route the pass does not see drops out.
struct Checks {
    last: HashMap<String, (i64, Verdict)>,
    now: HashMap<String, (i64, Verdict)>,
}

impl Checks {
    fn verdict<R: GatewayRoute>(&mut self, route: &R, config: &R::Config) -> Verdict {
        let (Some(uid), (_, Some(generation))) = (route.uid(), route.identity()) else {
            return R::check(config);
        };
        let verdict = match self.last.remove(uid) {
            Some((taken_at, verdict)) if taken_at == generation => verdict,
            _ => R::check(config),
        };
        self.now
            .insert(uid.to_string(), (generation, verdict.clone()));
        verdict
    }
}

/// Convert Kubernetes Gateway API resources into a PortailConfig.
/// This reuses all existing validation, conversion, hostname intersection,
/// and regex compilation logic via `to_route_table()`.
pub(crate) fn reconcile_to_config(
    gateway: &Gateway,
    snapshot: &ClusterSnapshot,
    cert_data: &CertData,
    services: &ServiceState,
    verdicts: &RouteVerdicts,
) -> Result<ReconcileResult> {
    let mut gateway_config = convert_gateway(gateway, cert_data)?;
    let gateway_name = gateway.metadata.name.as_deref().unwrap_or("default");
    let gateway_ns = gateway.metadata.namespace.as_deref().unwrap_or("default");

    // Gateway API v1.4.1 GatewaySpec.listeners: "ALL indistinct Listeners
    // must not be accepted for processing". A conflicted listener binds
    // nothing and takes no route; no winner is picked among them.
    let conflicts = listener_conflicts(&gateway.spec.listeners);
    gateway_config
        .listeners
        .retain(|l| !conflicts.contains_key(l.name.as_str()));
    let listeners: Vec<GatewayListeners> = gateway
        .spec
        .listeners
        .iter()
        .filter(|l| !conflicts.contains_key(l.name.as_str()))
        .cloned()
        .collect();

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
        listeners: &listeners,
        namespace_labels: &snapshot.namespace_labels,
        reference_grants: &snapshot.reference_grants,
        known_services: &services.known_services,
    };
    let gateway_key = (gateway_ns.to_string(), gateway_name.to_string());
    let mut checks = Checks {
        last: verdicts.take(&gateway_key),
        now: HashMap::new(),
    };
    let http_route_configs =
        collect_routes::<HTTPRoute>(&snapshot.http_routes, &cx, &mut checks, &mut route_status);
    let tcp_route_configs =
        collect_routes::<TCPRoute>(&snapshot.tcp_routes, &cx, &mut checks, &mut route_status);
    let tls_route_configs =
        collect_routes::<TLSRoute>(&snapshot.tls_routes, &cx, &mut checks, &mut route_status);
    let udp_route_configs =
        collect_routes::<UDPRoute>(&snapshot.udp_routes, &cx, &mut checks, &mut route_status);
    verdicts.put(gateway_key, checks.now);

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
                    format!("Gateway has no accepted listener named '{}'", section),
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

/// Listeners that are not distinct, with their Conflicted reason. Gateway
/// API, Listener: listeners sharing a port need distinct protocols, or
/// distinct hostnames for HTTP, HTTPS and TLS; TCP and UDP have none. All
/// indistinct listeners are rejected: the spec forbids picking a winner.
pub(crate) fn listener_conflicts(listeners: &[GatewayListeners]) -> HashMap<&str, &'static str> {
    let mut conflicts: HashMap<&str, &'static str> = HashMap::new();
    for (i, a) in listeners.iter().enumerate() {
        for b in listeners[i + 1..].iter().filter(|b| b.port == a.port) {
            let reason = if a.protocol != b.protocol {
                "ProtocolConflict"
            } else if matches!(a.protocol.as_str(), "TCP" | "UDP") || a.hostname == b.hostname {
                "HostnameConflict"
            } else {
                continue;
            };
            for l in [a, b] {
                let slot = conflicts.entry(l.name.as_str()).or_insert(reason);
                if reason == "ProtocolConflict" {
                    *slot = reason;
                }
            }
        }
    }
    conflicts
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

/// Gateway API precedence between routes whose matches tie: the oldest
/// creationTimestamp, then "{namespace}/{name}" in alphabetical order
/// (HTTPRouteRule.matches). The string, not the (namespace, name) pair:
/// "a-b/c" sorts before "a/z".
fn precedence<R: GatewayRoute>(a: &R, b: &R) -> std::cmp::Ordering {
    fn key<R: GatewayRoute>(r: &R) -> impl Iterator<Item = u8> + '_ {
        r.route_namespace()
            .bytes()
            .chain(std::iter::once(b'/'))
            .chain(r.identity().0.bytes())
    }
    a.creation_timestamp()
        .cmp(&b.creation_timestamp())
        .then_with(|| key(a).cmp(key(b)))
}

/// Generic route collection with namespace scoping and acceptance tracking.
/// Configs come out in precedence order: the table keeps insertion order
/// among equally specific rules.
fn collect_routes<R: GatewayRoute>(
    routes: &[Arc<R>],
    cx: &RouteCollectCtx,
    checks: &mut Checks,
    route_status: &mut Vec<RouteAcceptance>,
) -> Vec<R::Config> {
    let kind = R::KIND;
    let mut ordered: Vec<&R> = routes
        .iter()
        .map(|r| &**r)
        .filter(|r| {
            route_targets_gateway(
                r.parent_refs(),
                r.route_namespace(),
                cx.gateway_name,
                cx.gateway_ns,
            )
        })
        .collect();
    ordered.sort_by(|a, b| precedence(*a, *b));

    // Ports an older route already serves (ONE_ROUTE_PER_LISTENER kinds).
    // Keyed by port like the table: two listeners on one port share a socket.
    let port_of = |listener: &str| {
        cx.listeners
            .iter()
            .find(|l| l.name == listener)
            .map(|l| l.port)
    };
    let mut claimed: HashSet<i32> = HashSet::new();
    let mut configs = Vec::new();
    for route in ordered {
        let route_ns = route.route_namespace();
        let (name, generation) = route.identity();

        // Evaluate each Gateway-matching parentRef independently: which
        // listeners it selects and whether they admit the route. One status
        // entry per parentRef — a ref pinned to a rejecting section must not
        // inherit Accepted from a sibling ref (previously every ref got the
        // route-wide verdict, and every entry claimed every listener).
        let route_hostnames = route.hostnames();
        // The listeners each ref attaches to by allowedRoutes, before an
        // older route's claim: attachedRoutes counts them, accepted or not.
        let per_parent: Vec<(
            Option<String>,
            Option<i32>,
            Vec<String>,
            Result<Vec<String>, &'static str>,
        )> = route
            .parent_refs()
            .as_ref()
            .map(|refs| {
                refs.iter()
                    .filter(|pr| {
                        parent_ref_matches_gateway(*pr, route_ns, cx.gateway_name, cx.gateway_ns)
                    })
                    .map(|pr| {
                        let mut result = listeners_for_parent_ref(
                            pr,
                            cx.listeners,
                            cx.gateway_ns,
                            route_ns,
                            kind,
                            route_hostnames,
                            cx.namespace_labels,
                        );
                        let attached = result.clone().unwrap_or_default();
                        if R::ONE_ROUTE_PER_LISTENER {
                            result = result.and_then(|mut listeners| {
                                listeners
                                    .retain(|l| !port_of(l).is_some_and(|p| claimed.contains(&p)));
                                if listeners.is_empty() {
                                    Err("Conflicted")
                                } else {
                                    Ok(listeners)
                                }
                            });
                        }
                        (
                            pr.ref_section_name().map(String::from),
                            pr.ref_port(),
                            attached,
                            result,
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
                "Conflicted" => {
                    "An older route already serves every listener this parentRef selects"
                }
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

        if per_parent.iter().all(|(_, _, _, r)| r.is_err()) {
            for (section_name, port, attached, result) in &per_parent {
                let reason = result.as_ref().err().copied().unwrap_or("NoMatchingParent");
                route_status.push(RouteAcceptance {
                    listener_names: attached.clone(),
                    ..rejection(section_name, *port, reason)
                });
            }
            continue;
        }

        let (mut config, converted) = route.convert();
        let verdict = converted.and_then(|()| checks.verdict(route, &config));

        let mut refs_resolved = true;
        let mut refs_messages = Vec::new();
        let mut refs_reason = "ResolvedRefs";
        let mut invalid_mirrors: HashSet<(String, u16)> = HashSet::new();

        // Group/kind (core Service only), cross-namespace ReferenceGrant,
        // and service existence: one checker for forward and mirror refs.
        let check_ref =
            |backend_name: &str, group: &str, ref_kind: &str| -> Option<(&'static str, String)> {
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

        // An invalid ref keeps its weight so its share fails rather
        // than moving to the valid refs (HTTPRouteRule.backendRefs).
        for backend in R::backend_refs_mut(&mut config) {
            if let Some((reason, message)) = check_ref(&backend.name, &backend.group, &backend.kind)
            {
                refs_resolved = false;
                refs_reason = reason;
                refs_messages.push(message);
                backend.invalid = true;
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

        if !invalid_mirrors.is_empty() {
            R::remove_invalid_mirrors(&mut config, &invalid_mirrors);
        }

        let refs_message = if refs_resolved {
            "All references resolved".to_string()
        } else {
            refs_messages.join("; ")
        };

        // A route that fails its check is exactly one the table build
        // would leave out. Its refs still decide ResolvedRefs.
        let (accepted_reason, message) = match &verdict {
            Ok(()) => ("Accepted", "Accepted".to_string()),
            Err(e) => (e.reason.as_str(), format!("Conversion failed: {}", e)),
        };
        for (section_name, port, attached, result) in &per_parent {
            match result {
                // A parentRef the listeners admit. One they reject keeps
                // its own rejection reason.
                Ok(_) => route_status.push(RouteAcceptance {
                    name: name.to_string(),
                    namespace: route_ns.to_string(),
                    kind,
                    accepted: verdict.is_ok(),
                    accepted_reason,
                    message: message.clone(),
                    refs_resolved,
                    refs_reason,
                    refs_message: refs_message.clone(),
                    generation,
                    section_name: section_name.clone(),
                    port: *port,
                    listener_names: attached.clone(),
                }),
                Err(reason) => route_status.push(RouteAcceptance {
                    listener_names: attached.clone(),
                    ..rejection(section_name, *port, reason)
                }),
            }
        }
        if verdict.is_err() {
            continue;
        }

        let mut attached: Vec<&String> = Vec::new();
        for listener in per_parent
            .iter()
            .filter_map(|(_, _, _, result)| result.as_ref().ok())
            .flatten()
        {
            if !attached.contains(&listener) {
                attached.push(listener);
            }
        }
        if R::ONE_ROUTE_PER_LISTENER {
            claimed.extend(attached.iter().filter_map(|l| port_of(l)));
        }
        R::attach(
            &mut config,
            attached
                .into_iter()
                .map(|listener| ParentRef {
                    name: cx.gateway_name.to_string(),
                    section_name: Some(listener.clone()),
                    port: None,
                })
                .collect(),
        );
        configs.push(config);
    }
    configs
}

#[cfg(test)]
mod tests;
