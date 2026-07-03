use arc_swap::{ArcSwap, ArcSwapOption};
use futures::StreamExt;
use k8s_openapi::api::core::v1::{Namespace, Secret, Service};
use kube::api::Api;
use kube::runtime::controller::Action;
use kube::runtime::reflector::{ObjectRef, Store};
use kube::runtime::watcher;
use kube::runtime::{reflector, Controller, WatchStreamExt};
use kube::Client;
use kube::ResourceExt;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use gateway_api::experimental::tcproutes::TCPRoute;
use gateway_api::experimental::tlsroutes::TLSRoute;
use gateway_api::experimental::udproutes::UDPRoute;
use gateway_api::gateways::Gateway;
use gateway_api::httproutes::HTTPRoute;
use gateway_api::referencegrants::ReferenceGrant;

use crate::logging::{debug, error, info, warn};
use crate::routing::RouteTable;

use super::addresses::{
    compute_bind_addresses, discover_usable_addresses, resolve_network_addresses, UsableAddresses,
    NETWORK_ADDRESS_TYPE,
};
use super::parent_ref::ParentRefAccess;
use super::reconciler::{reconcile_to_config, ClusterSnapshot, RouteAcceptance};
use super::reference_grants::is_reference_allowed;
use super::services::{resolve_named_target_ports, resolve_services};
use super::status;

/// A reconcile pass failed; the controller runtime retries via `error_policy`.
#[derive(Debug)]
struct ReconcileError(String);

impl std::fmt::Display for ReconcileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Reconciliation error: {}", self.0)
    }
}

impl std::error::Error for ReconcileError {}

/// Semantic grouping of reflector stores — cached local copies of cluster resources.
#[derive(Clone)]
struct ResourceCache {
    gateways: Store<Gateway>,
    http_routes: Store<HTTPRoute>,
    tcp_routes: Store<TCPRoute>,
    tls_routes: Store<TLSRoute>,
    udp_routes: Store<UDPRoute>,
    namespaces: Store<Namespace>,
    services: Store<Service>,
    secrets: Store<Secret>,
    reference_grants: Store<ReferenceGrant>,
}

/// Read all objects from a reflector store, cloning each.
fn snapshot<K>(store: &Store<K>) -> Vec<K>
where
    K: kube::Resource + Clone,
    K::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
    store.state().iter().map(|arc| (**arc).clone()).collect()
}

#[derive(Clone)]
struct ControllerCtx {
    client: Client,
    controller_name: String,
    cache: ResourceCache,
    routes: Arc<ArcSwap<RouteTable>>,
    /// Last config the reconciler built, published for the background DNS-refresh
    /// task to re-resolve without an apiserver round-trip. `None` until the first
    /// successful reconcile.
    config_cell: Arc<ArcSwapOption<crate::config::PortailConfig>>,
    data_plane: Arc<std::sync::Mutex<crate::data_plane::DataPlane>>,
    performance_config: crate::config::PerformanceConfig,
    /// When false, the operator owns Gateway/GatewayClass lifecycle status
    /// (Accepted/Programmed/addresses); portail reports only listener + route status.
    manage_gateway_status: bool,
    /// Flipped to true once the data plane has bound this instance's listener
    /// ports. Surfaced via the readiness endpoint for the pod's readinessProbe.
    ready: Arc<std::sync::atomic::AtomicBool>,
    /// Fingerprint of the last reconcile pass that was fully applied — config
    /// built + route table swapped + every status PATCH accepted. When a new
    /// pass computes the same fingerprint, the rebuild and all status writes
    /// are skipped: this is what keeps a permanently-unaccepted route (or our
    /// own status-write watch echoes) from re-running regex compilation, DNS
    /// resolution, and N apiserver PATCHes on every requeue.
    last_applied: Arc<std::sync::Mutex<Option<u64>>>,
    /// Last built config per Gateway. In scoped mode this holds exactly one
    /// entry; in legacy unscoped mode (one process watching every Gateway)
    /// the entries are merged before each route-table build so reconciling
    /// Gateway A cannot clobber Gateway B's routes in the shared table.
    gateway_configs:
        Arc<std::sync::Mutex<HashMap<(String, String), Arc<crate::config::PortailConfig>>>>,
}

/// Create a reflector for a resource type, returning the store and a stream.
/// The stream is consumed by `Controller::watches_stream()` which both drives
/// the reflector (keeping the store up to date) and triggers reconciliation
/// of mapped Gateway(s) when resources change.
fn create_reflector<K>(
    api: Api<K>,
) -> (
    Store<K>,
    impl futures::Stream<Item = Result<K, watcher::Error>> + Send + 'static,
)
where
    K: kube::Resource
        + Clone
        + std::fmt::Debug
        + serde::de::DeserializeOwned
        + Send
        + Sync
        + 'static,
    K::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
    let writer = reflector::store::Writer::default();
    let reader = writer.as_reader();
    // Stream the initial list via the WatchList API instead of a single blocking
    // LIST. Each per-Gateway pod cold-starts by listing ~6 cluster-wide resource
    // types; with N pods watching + route/Service churn, concurrent blocking LISTs
    // saturate the apiserver and a fresh pod's reflector sync stalls (measured 1s
    // isolated → 20s+/never under load), which is the cold-start convergence flake.
    // Streaming lists are far lighter on the apiserver under that load. Requires
    // server WatchList support (k8s >= 1.27; default-on >= 1.30).
    let stream = reflector::reflector(
        writer,
        watcher::watcher(api, watcher::Config::default().streaming_lists()),
    )
    .default_backoff()
    .touched_objects(); // touched_objects includes deletes; applied_objects drops them
    (reader, stream)
}

/// Like `create_reflector`, but first probes the API endpoint with a lightweight
/// list request. If the CRD is not installed (404), returns an empty store and
/// a stream that never yields, so the controller can still start without it.
async fn create_optional_reflector<K>(
    api: Api<K>,
) -> (
    Store<K>,
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<K, watcher::Error>> + Send + 'static>>,
)
where
    K: kube::Resource
        + Clone
        + std::fmt::Debug
        + serde::de::DeserializeOwned
        + Send
        + Sync
        + 'static,
    K::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
    // Probe with limit=0 — just checks the endpoint exists
    let probe = api.list(&kube::api::ListParams::default().limit(1)).await;
    match probe {
        Err(kube::Error::Api(ref status)) if status.code == 404 => {
            let kind = std::any::type_name::<K>()
                .rsplit("::")
                .next()
                .unwrap_or("Unknown");
            warn!("CRD not installed, skipping watcher: {kind}");
            let writer = reflector::store::Writer::default();
            let reader = writer.as_reader();
            (reader, Box::pin(futures::stream::pending()))
        }
        _ => {
            let (store, stream) = create_reflector(api);
            (store, Box::pin(stream))
        }
    }
}

// ---------------------------------------------------------------------------
// Mapper functions: map secondary resource events to Gateway ObjectRef(s)
// ---------------------------------------------------------------------------

/// Map a route to the Gateway(s) it targets via parentRefs.
fn map_route_to_gateways<R: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope>>(
    route: &R,
    parent_refs: &Option<Vec<impl ParentRefAccess>>,
    store: &Store<Gateway>,
) -> Vec<ObjectRef<Gateway>> {
    let route_ns = route.namespace().unwrap_or_default();
    let mut refs = Vec::new();
    if let Some(prs) = parent_refs {
        for pr in prs {
            if pr
                .ref_group()
                .is_some_and(|g| g != "gateway.networking.k8s.io")
            {
                continue;
            }
            if pr.ref_kind().is_some_and(|k| k != "Gateway") {
                continue;
            }
            let gw_ns = pr.ref_namespace().unwrap_or(&route_ns);
            let obj_ref = ObjectRef::<Gateway>::new(pr.ref_name()).within(gw_ns);
            if store.get(&obj_ref).is_some() {
                refs.push(obj_ref);
            }
        }
    }
    refs
}

/// Map any resource to ALL managed gateways (used for infrequently-changing
/// resources like Service, Namespace, ReferenceGrant where targeted mapping
/// adds complexity for negligible CPU gain).
fn all_gateway_refs(store: &Store<Gateway>) -> Vec<ObjectRef<Gateway>> {
    store
        .state()
        .into_iter()
        .map(|gw| ObjectRef::from_obj(&*gw))
        .collect()
}

/// Which *gate-able* secondary resources a single-Gateway data plane needs to
/// watch. Derived by portail-operator from the Gateway's listeners and passed
/// via `--watch-shape`, so a scoped pod skips cluster-wide watches it will never
/// consume. The default (`--watch-shape` absent → `WatchShape::all()`) keeps the
/// legacy broad behavior, so an older operator that does not set the flag still
/// works unchanged.
///
/// IMPORTANT: only resources that never `parentRef` a Gateway may be gated.
/// Route watches (HTTP/TCP/TLS/UDP) are deliberately NOT here: a route in any
/// namespace, of any kind, may `parentRef` this Gateway and must receive a
/// status — attached, or rejected with `NotAllowedByListeners` /
/// `NoMatchingParent` — which requires the data plane to observe it. Secrets and
/// Namespace labels do not `parentRef` Gateways, so they are safe to drop.
#[derive(Clone, Copy, Debug)]
pub struct WatchShape {
    pub tls_secrets: bool,
    pub namespaces: bool,
}

impl WatchShape {
    /// Watch every gate-able secondary resource (legacy / unscoped mode).
    pub fn all() -> Self {
        Self {
            tls_secrets: true,
            namespaces: true,
        }
    }

    /// Parse the operator's `--watch-shape` token set. `None` → `all()` (broad,
    /// legacy). `Some(spec)` → enable only the listed gate-able watches: `tls`
    /// (a TLS-terminate listener) and `ns-labels` (an `allowedRoutes: Selector`
    /// listener).
    pub fn parse(spec: Option<&str>) -> Self {
        let Some(spec) = spec else {
            return Self::all();
        };
        let mut s = Self {
            tls_secrets: false,
            namespaces: false,
        };
        for token in spec.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            match token {
                "tls" => s.tls_secrets = true,
                "ns-labels" => s.namespaces = true,
                other => warn!("ignoring unknown --watch-shape token: {other}"),
            }
        }
        s
    }
}

/// A boxed reflector stream — the uniform type used by gated watches so the
/// `Controller` wiring stays identical whether or not the watch is active.
type BoxedReflectorStream<K> =
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<K, watcher::Error>> + Send + 'static>>;

/// Like `create_reflector` but gated, with a caller-supplied watcher config:
/// when `enabled` is false, returns an empty store and a stream that never yields
/// — the controller opens no LIST/WATCH for it — while keeping the `Controller`
/// wiring uniform. `config` lets callers add a field selector etc. (e.g. the TLS
/// Secret watch's `type=kubernetes.io/tls`).
fn create_reflector_gated_with_config<K>(
    enabled: bool,
    api: Api<K>,
    config: watcher::Config,
) -> (Store<K>, BoxedReflectorStream<K>)
where
    K: kube::Resource
        + Clone
        + std::fmt::Debug
        + serde::de::DeserializeOwned
        + Send
        + Sync
        + 'static,
    K::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
    if !enabled {
        let writer = reflector::store::Writer::default();
        return (writer.as_reader(), Box::pin(futures::stream::pending()));
    }
    let writer = reflector::store::Writer::default();
    let reader = writer.as_reader();
    let stream = reflector::reflector(writer, watcher::watcher(api, config))
        .default_backoff()
        .touched_objects();
    (reader, Box::pin(stream))
}

/// The gated counterpart of `create_reflector`: same default streaming-list
/// config, but opens no LIST/WATCH when `enabled` is false.
fn create_reflector_gated<K>(enabled: bool, api: Api<K>) -> (Store<K>, BoxedReflectorStream<K>)
where
    K: kube::Resource
        + Clone
        + std::fmt::Debug
        + serde::de::DeserializeOwned
        + Send
        + Sync
        + 'static,
    K::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
    create_reflector_gated_with_config(enabled, api, watcher::Config::default().streaming_lists())
}

pub async fn run_controller(
    routes: Arc<ArcSwap<RouteTable>>,
    controller_name: String,
    shutdown: CancellationToken,
    data_plane: Arc<std::sync::Mutex<crate::data_plane::DataPlane>>,
    performance_config: crate::config::PerformanceConfig,
    manage_gateway_status: bool,
    ready: Arc<std::sync::atomic::AtomicBool>,
    gateway_scope: Option<(String, String)>,
    watch_shape: WatchShape,
) -> anyhow::Result<()> {
    let client = Client::try_default().await?;
    info!("Kubernetes client connected, starting Gateway API controller");

    // --- Primary resource: Gateway ---
    // No `predicate_filter(predicates::generation)` here: it caches generation
    // per key and never clears on delete, so deleting and recreating a Gateway
    // with the same name (recreates restart at generation 1) would be
    // suppressed and the recreated Gateway never reconciled. Status-only watch
    // echoes of our own writes are instead absorbed by the reconcile
    // fingerprint short-circuit, which skips the rebuild and all PATCHes.
    //
    // When `gateway_scope` is set (operator-managed Deployments), narrow the
    // watch to that single Gateway via namespace + `metadata.name` field
    // selector. The reflector store then holds exactly one object, and every
    // downstream mapper that already filters by `route_targets_gateway` /
    // `all_gateway_refs` naturally collapses to "this Gateway or nothing."
    // Unset = legacy unscoped mode (watch all Gateways cluster-wide).
    if let Some((ns, name)) = &gateway_scope {
        info!("Controller scoped to Gateway {}/{}", ns, name);
    }
    let gw_writer = reflector::store::Writer::default();
    let store_gateways = gw_writer.as_reader();
    let (gw_api, gw_watcher_config) = match &gateway_scope {
        Some((ns, name)) => (
            Api::<Gateway>::namespaced(client.clone(), ns),
            watcher::Config::default().fields(&format!("metadata.name={}", name)),
        ),
        None => (
            Api::<Gateway>::all(client.clone()),
            watcher::Config::default(),
        ),
    };
    let gw_stream = reflector::reflector(gw_writer, watcher::watcher(gw_api, gw_watcher_config))
        .default_backoff()
        .applied_objects();

    // --- Secondary resources ---
    // Each resource gets a reflector (store + stream). The stream feeds into
    // Controller::watches_stream() with a mapper that targets affected Gateway(s).
    // This is the standard kube-rs/controller-runtime pattern: secondary resource
    // events go through a separate path unaffected by the primary predicate_filter.

    // CRD reflectors — generation-filtered to ignore status-only changes.
    // HTTPRoute is required (GA); TCP/TLS/UDPRoute are experimental (optional).
    // All route watches stay CLUSTER-WIDE and unconditional: a route in any
    // namespace, of any kind, may parentRef this Gateway and must get a status
    // (attached, or rejected with NotAllowedByListeners / NoMatchingParent) —
    // which requires observing it, so route watches cannot be shape-scoped. The
    // three optional CRDs are probed concurrently so a contended apiserver does
    // not serialize the cold start.
    let (store_http_routes, http_route_stream) =
        create_reflector(Api::<HTTPRoute>::all(client.clone()));
    let (tcp_reflector, tls_reflector, udp_reflector) = tokio::join!(
        create_optional_reflector(Api::<TCPRoute>::all(client.clone())),
        create_optional_reflector(Api::<TLSRoute>::all(client.clone())),
        create_optional_reflector(Api::<UDPRoute>::all(client.clone())),
    );
    let (store_tcp_routes, tcp_route_stream) = tcp_reflector;
    let (store_tls_routes, tls_route_stream) = tls_reflector;
    let (store_udp_routes, udp_route_stream) = udp_reflector;
    // GatewayClass status is owned by portail-operator now; portail no longer
    // runs its own GatewayClass controller.

    // Core resource reflectors — no generation filter (core types don't reliably set it).
    // The Namespace watch is opened only when a listener uses
    // `allowedRoutes: Selector` (the only consumer of namespace labels).
    let (store_namespaces, namespace_stream) = create_reflector_gated(
        watch_shape.namespaces,
        Api::<Namespace>::all(client.clone()),
    );
    let (store_services, service_stream) = create_reflector(Api::<Service>::all(client.clone()));
    let (store_reference_grants, reference_grant_stream) =
        create_reflector(Api::<ReferenceGrant>::all(client.clone()));

    // TLS secrets — field-filtered to type=kubernetes.io/tls, and opened only
    // when the Gateway has a TLS-terminate listener (watch shape).
    let (store_secrets, secret_stream) = create_reflector_gated_with_config(
        watch_shape.tls_secrets,
        Api::<Secret>::all(client.clone()),
        watcher::Config::default()
            .fields("type=kubernetes.io/tls")
            .streaming_lists(),
    );

    let store_gateways_for_controller = store_gateways.clone();

    // Shared cell the reconciler publishes its last-built config into, read by the
    // background DNS-refresh task to re-resolve backend FQDNs off the apiserver.
    let config_cell = Arc::new(ArcSwapOption::<crate::config::PortailConfig>::empty());

    let ctx = Arc::new(ControllerCtx {
        client: client.clone(),
        controller_name: controller_name.clone(),
        cache: ResourceCache {
            gateways: store_gateways,
            http_routes: store_http_routes,
            tcp_routes: store_tcp_routes,
            tls_routes: store_tls_routes,
            udp_routes: store_udp_routes,
            namespaces: store_namespaces,
            services: store_services,
            secrets: store_secrets,
            reference_grants: store_reference_grants,
        },
        routes,
        config_cell: config_cell.clone(),
        data_plane,
        performance_config,
        manage_gateway_status,
        ready,
        last_applied: Arc::new(std::sync::Mutex::new(None)),
        gateway_configs: Arc::new(std::sync::Mutex::new(HashMap::new())),
    });

    // STRICT_DNS freshness: re-resolve headless/Service FQDNs on an interval and
    // swap the route table only when the resolved set changes. This is why the
    // cluster-wide EndpointSlice watch is unnecessary — DNS already publishes the
    // per-pod A-records, so pod churn is picked up here without an apiserver watch.
    crate::dns_refresh::spawn_dns_refresh(
        config_cell,
        ctx.routes.clone(),
        ctx.performance_config.dns_refresh_interval,
        shutdown.clone(),
    );

    // --- Gateway controller ---
    // Primary: Gateway stream with generation predicate (breaks feedback loop).
    // Secondary: watches_stream() per resource type with targeted mappers.
    //
    // Routes → map to parent Gateway(s) via parentRefs (targeted)
    // Secret → map to Gateways referencing it in TLS config (targeted)
    // Service, Namespace, ReferenceGrant → all Gateways (broad, infrequent changes)

    // Each watches_stream closure captures an owned gateway store clone via move.
    let [gw1, gw2, gw3, gw4, gw5, gw6, gw7] =
        std::array::from_fn::<_, 7, _>(|_| store_gateways_for_controller.clone());

    let gw_controller = Controller::for_stream(gw_stream, store_gateways_for_controller)
        // Routes: targeted — only reconcile the Gateway(s) referenced in parentRefs.
        // No predicate_filter here: it caches generation per object key and never
        // clears on delete, so delete+recreate with the same name (common in
        // conformance tests) would be suppressed. The status feedback loop is
        // already broken by the primary Gateway stream's predicate filter.
        .watches_stream(http_route_stream, move |route: HTTPRoute| {
            map_route_to_gateways(&route, &route.spec.parent_refs, &gw1)
        })
        .watches_stream(tcp_route_stream, move |route: TCPRoute| {
            map_route_to_gateways(&route, &route.spec.parent_refs, &gw2)
        })
        .watches_stream(tls_route_stream, move |route: TLSRoute| {
            map_route_to_gateways(&route, &route.spec.parent_refs, &gw3)
        })
        .watches_stream(udp_route_stream, move |route: UDPRoute| {
            map_route_to_gateways(&route, &route.spec.parent_refs, &gw4)
        })
        // Secret: targeted — only reconcile Gateways referencing this secret in TLS
        .watches_stream(
            secret_stream,
            move |secret: Secret| -> Vec<ObjectRef<Gateway>> {
                let secret_name = secret.metadata.name.as_deref().unwrap_or("");
                let secret_ns = secret.metadata.namespace.as_deref().unwrap_or("default");
                gw5.state()
                    .into_iter()
                    .filter(|g| {
                        let gw_ns = g.metadata.namespace.as_deref().unwrap_or("default");
                        g.spec.listeners.iter().any(|l| {
                            l.tls.as_ref().is_some_and(|tls| {
                                tls.certificate_refs.as_ref().is_some_and(|refs| {
                                    refs.iter().any(|cr| {
                                        cr.name == secret_name
                                            && cr.namespace.as_deref().unwrap_or(gw_ns) == secret_ns
                                    })
                                })
                            })
                        })
                    })
                    .map(|g| ObjectRef::from_obj(&*g))
                    .collect()
            },
        )
        // Service, Namespace, ReferenceGrant: broad — reconcile all Gateways.
        // These have complex multi-hop mappings; the 1s debounce coalesces bursts.
        // (No EndpointSlice watch: backend pod churn is picked up by the DNS-refresh
        // task, since a headless Service publishes per-pod A-records at its FQDN.)
        .watches_stream(service_stream, move |_: Service| all_gateway_refs(&gw6))
        .watches_stream(namespace_stream, move |_: Namespace| all_gateway_refs(&gw7))
        .watches_stream(reference_grant_stream, {
            let gw = ctx.cache.gateways.clone();
            move |_: ReferenceGrant| all_gateway_refs(&gw)
        })
        .with_config(kube::runtime::controller::Config::default().debounce(Duration::from_secs(1)))
        .shutdown_on_signal()
        .run(reconcile, error_policy, ctx);

    info!("Gateway API controller started, watching for resource changes");

    tokio::select! {
        _ = gw_controller.for_each(|result| async move {
            match result {
                Ok((_obj_ref, _action)) => {}
                Err(e) => warn!("Gateway reconciliation error: {}", e),
            }
        }) => {
            info!("Gateway controller stream ended");
        }
        _ = shutdown.cancelled() => {
            info!("Controller received shutdown signal");
        }
    }

    Ok(())
}

/// Resolve TLS certificates for a Gateway from the reflector Secret store.
/// Iterates all listeners, checks cross-namespace ReferenceGrant permissions,
/// looks up Secrets, and validates PEM format.
fn resolve_gateway_certs(
    gateway: &Gateway,
    gw_ns: &str,
    reference_grants: &[ReferenceGrant],
    store_secrets: &Store<Secret>,
) -> HashMap<(String, String), (Vec<u8>, Vec<u8>)> {
    let mut cert_data: HashMap<(String, String), (Vec<u8>, Vec<u8>)> = HashMap::new();
    for listener in &gateway.spec.listeners {
        let Some(tls) = &listener.tls else { continue };
        let Some(cert_refs) = &tls.certificate_refs else {
            continue;
        };
        for cert_ref in cert_refs {
            let secret_ns = cert_ref.namespace.as_deref().unwrap_or(gw_ns);

            // Cross-namespace cert ref requires a ReferenceGrant
            if secret_ns != gw_ns
                && !is_reference_allowed(
                    reference_grants,
                    "gateway.networking.k8s.io",
                    "Gateway",
                    gw_ns,
                    "",
                    "Secret",
                    secret_ns,
                    &cert_ref.name,
                )
            {
                warn!(
                    "Cross-namespace certificate ref {}/{} not allowed by ReferenceGrant",
                    secret_ns, cert_ref.name
                );
                continue;
            }

            // Look up secret from reflector cache
            let secret_ref = ObjectRef::<Secret>::new(&cert_ref.name).within(secret_ns);
            match store_secrets.get(&secret_ref) {
                Some(secret) => {
                    if let Some(data) = &secret.data {
                        let cert_pem = data.get("tls.crt").map(|b| b.0.clone());
                        let key_pem = data.get("tls.key").map(|b| b.0.clone());
                        if let (Some(cert), Some(key)) = (cert_pem, key_pem) {
                            let cert_str = String::from_utf8_lossy(&cert);
                            let key_str = String::from_utf8_lossy(&key);
                            if cert_str.contains("BEGIN CERTIFICATE")
                                && (key_str.contains("BEGIN PRIVATE KEY")
                                    || key_str.contains("BEGIN RSA PRIVATE KEY")
                                    || key_str.contains("BEGIN EC PRIVATE KEY"))
                            {
                                cert_data.insert(
                                    (cert_ref.name.clone(), secret_ns.to_string()),
                                    (cert, key),
                                );
                            } else {
                                warn!(
                                    "Secret {}/{} has malformed PEM data",
                                    secret_ns, cert_ref.name
                                );
                            }
                        } else {
                            warn!(
                                "Secret {}/{} missing tls.crt or tls.key",
                                secret_ns, cert_ref.name
                            );
                        }
                    }
                }
                None => {
                    debug!("Secret {}/{} not found in cache", secret_ns, cert_ref.name);
                }
            }
        }
    }
    cert_data
}

/// Validate Gateway listeners and compute gateway-level acceptance.
fn validate_gateway(
    gateway: &Gateway,
    gw_ns: &str,
    cert_data: &HashMap<(String, String), (Vec<u8>, Vec<u8>)>,
    reference_grants: &[ReferenceGrant],
) -> (
    HashMap<String, status::ListenerStatus>,
    status::GatewayCondition,
) {
    let mut listener_statuses: HashMap<String, status::ListenerStatus> = HashMap::new();

    // Detect protocol conflicts: same port, different protocols
    let mut port_protocols: HashMap<i32, String> = HashMap::new();
    let mut conflicted_ports: HashSet<i32> = HashSet::new();
    for l in &gateway.spec.listeners {
        match port_protocols.get(&l.port) {
            Some(existing_proto) if *existing_proto != l.protocol => {
                conflicted_ports.insert(l.port);
            }
            None => {
                port_protocols.insert(l.port, l.protocol.clone());
            }
            _ => {}
        }
    }

    for listener in &gateway.spec.listeners {
        let mut ls = status::ListenerStatus::default();
        let mut refs_failed = false;

        if conflicted_ports.contains(&listener.port) {
            ls.accepted = false;
            ls.accepted_reason = "ProtocolConflict".into();
            ls.accepted_message =
                "Listener port conflicts with another listener using a different protocol".into();
            ls.conflicted = true;
            ls.conflicted_reason = "ProtocolConflict".into();
            ls.conflicted_message =
                "Listener port shared with another listener using different protocol".into();
        } else {
            ls.accepted = true;
            ls.accepted_reason = "Accepted".into();
            ls.accepted_message = "Listener accepted".into();
        }

        // Validate TLS certificateRefs for HTTPS/TLS listeners
        if let Some(tls) = &listener.tls {
            if let Some(cert_refs) = &tls.certificate_refs {
                for cert_ref in cert_refs {
                    let group = cert_ref.group.as_deref().unwrap_or("");
                    let kind_str = cert_ref.kind.as_deref().unwrap_or("Secret");
                    if !group.is_empty() || kind_str != "Secret" {
                        refs_failed = true;
                        ls.resolved_refs_reason = "InvalidCertificateRef".into();
                        ls.resolved_refs_message = format!(
                            "Unsupported certificateRef group/kind: {}/{}",
                            group, kind_str
                        );
                        continue;
                    }

                    let secret_ns = cert_ref.namespace.as_deref().unwrap_or(gw_ns);

                    // Cross-namespace cert ref requires ReferenceGrant
                    if secret_ns != gw_ns
                        && !is_reference_allowed(
                            reference_grants,
                            "gateway.networking.k8s.io",
                            "Gateway",
                            gw_ns,
                            "",
                            "Secret",
                            secret_ns,
                            &cert_ref.name,
                        )
                    {
                        refs_failed = true;
                        ls.resolved_refs_reason = "RefNotPermitted".into();
                        ls.resolved_refs_message = format!(
                            "Cross-namespace certificate ref {}/{} not allowed by ReferenceGrant",
                            secret_ns, cert_ref.name
                        );
                        continue;
                    }

                    if !cert_data.contains_key(&(cert_ref.name.clone(), secret_ns.to_string())) {
                        refs_failed = true;
                        ls.resolved_refs_reason = "InvalidCertificateRef".into();
                        ls.resolved_refs_message = format!(
                            "Secret {}/{} not found or missing tls.crt/tls.key",
                            secret_ns, cert_ref.name
                        );
                    }
                }
            }
        } else if matches!(listener.protocol.as_str(), "HTTPS" | "TLS") {
            refs_failed = true;
            ls.resolved_refs_reason = "InvalidCertificateRef".into();
            ls.resolved_refs_message = "HTTPS/TLS listener requires TLS configuration".into();
        }

        // Check allowedRoutes.kinds validity and compute supportedKinds
        if let Some(allowed_routes) = &listener.allowed_routes {
            if let Some(kinds) = &allowed_routes.kinds {
                let valid_kinds = ["HTTPRoute", "TCPRoute", "TLSRoute", "UDPRoute", "GRPCRoute"];
                let mut supported = Vec::new();
                let mut has_invalid = false;
                for k in kinds {
                    let group = k.group.as_deref().unwrap_or("gateway.networking.k8s.io");
                    let kind_str = &k.kind;
                    if group == "gateway.networking.k8s.io"
                        && valid_kinds.contains(&kind_str.as_str())
                    {
                        supported.push(serde_json::json!({
                            "group": "gateway.networking.k8s.io",
                            "kind": kind_str,
                        }));
                    } else {
                        has_invalid = true;
                    }
                }
                ls.supported_kinds = supported;
                if has_invalid {
                    refs_failed = true;
                    ls.resolved_refs_reason = "InvalidRouteKinds".into();
                    ls.resolved_refs_message =
                        "One or more route kinds in allowedRoutes are not supported".into();
                }
            } else {
                ls.supported_kinds = status::supported_kinds_for_protocol(&listener.protocol);
            }
        } else {
            ls.supported_kinds = status::supported_kinds_for_protocol(&listener.protocol);
        }

        if !refs_failed {
            ls.resolved_refs = true;
            ls.resolved_refs_reason = "ResolvedRefs".into();
            ls.resolved_refs_message = "All references resolved".into();
        }

        if ls.accepted && ls.resolved_refs {
            ls.programmed = true;
            ls.programmed_reason = "Programmed".into();
            ls.programmed_message = "Programmed".into();
        } else {
            ls.programmed = false;
            ls.programmed_reason = "Invalid".into();
            ls.programmed_message = if !ls.accepted {
                "Listener not programmed due to acceptance failure".into()
            } else {
                "Listener has unresolved references".into()
            };
        }

        listener_statuses.insert(listener.name.clone(), ls);
    }

    // Gateway-level acceptance
    let supported_protocols = ["HTTP", "HTTPS", "TLS", "TCP", "UDP"];
    let mut accepted = true;
    let mut reason = "Accepted".to_string();
    let mut message = "Gateway accepted by portail controller".to_string();

    for listener in &gateway.spec.listeners {
        if !supported_protocols.contains(&listener.protocol.as_str()) {
            accepted = false;
            reason = "InvalidParameters".to_string();
            message = format!(
                "Listener '{}' uses unsupported protocol '{}'",
                listener.name, listener.protocol
            );
            break;
        }
    }

    if accepted {
        if let Some(addresses) = &gateway.spec.addresses {
            for addr in addresses {
                let addr_type = addr.r#type.as_deref().unwrap_or("IPAddress");
                match addr_type {
                    "IPAddress" | "Hostname" => {}
                    t if t == NETWORK_ADDRESS_TYPE => {}
                    _ => {
                        accepted = false;
                        reason = "UnsupportedAddress".to_string();
                        message =
                            format!("Unsupported address type '{}' in spec.addresses", addr_type);
                        break;
                    }
                }
            }
        }
    }

    if accepted && !listener_statuses.is_empty() {
        let all_rejected = listener_statuses.values().all(|ls| !ls.accepted);
        if all_rejected {
            accepted = false;
            reason = "InvalidListeners".to_string();
            message = "All listeners are rejected due to conflicts or errors".to_string();
        }
    }

    (
        listener_statuses,
        status::GatewayCondition {
            ok: accepted,
            reason,
            message,
        },
    )
}

/// Open data-plane TCP listeners for this Gateway's ports. Called after the
/// per-Gateway config is built so ListenerConfig has resolved TLS cert data.
fn ensure_data_plane_listeners(
    ctx: &ControllerCtx,
    listeners: &[crate::config::ListenerConfig],
    bind_addresses: &[String],
) {
    info!(
        "Ensuring data plane listeners for ports: {:?}",
        listeners.iter().map(|l| l.port).collect::<Vec<_>>()
    );
    match ctx.data_plane.lock() {
        Ok(mut dp) => {
            let (opened, errors) = dp.add_tcp_listeners(
                listeners,
                bind_addresses,
                ctx.routes.clone(),
                &ctx.performance_config,
            );
            if opened > 0 {
                info!("Opened {} new port(s)", opened);
            }
            for (port, err) in &errors {
                warn!("Failed to bind port {}: {}", port, err);
            }
        }
        Err(e) => warn!("Failed to lock data plane: {}", e),
    }
}

/// (address, bound-port) pairs the data plane must be bound to for this Gateway
/// to be considered Programmed. Keyed on each listener's bound port
/// (`target_port` when the fronting Service decouples it, else the published
/// port) so a deferred privileged listener keeps the Gateway not-Programmed
/// until its real socket is up.
fn required_endpoints(
    listeners: &[crate::config::ListenerConfig],
    bind_addresses: &[String],
) -> Vec<(Option<String>, u16)> {
    if bind_addresses.is_empty() {
        listeners
            .iter()
            .map(|l| (None, l.target_port.unwrap_or(l.port)))
            .collect()
    } else {
        bind_addresses
            .iter()
            .flat_map(|addr| {
                listeners
                    .iter()
                    .map(move |l| (Some(addr.clone()), l.target_port.unwrap_or(l.port)))
            })
            .collect()
    }
}

/// Compute the `Programmed` condition from data-plane readiness. The operator
/// owns address-usability semantics for status.addresses; here we just report
/// whether the data plane has bound this Gateway's listener ports.
fn compute_programmed_condition(dp_ready: bool) -> status::GatewayCondition {
    if dp_ready {
        status::GatewayCondition {
            ok: true,
            reason: "Programmed".into(),
            message: "Programmed".into(),
        }
    } else {
        warn!("Data plane not ready: not all listener ports are bound");
        status::GatewayCondition {
            ok: false,
            reason: "Invalid".into(),
            message: "Data plane not ready: not all listener ports are bound".into(),
        }
    }
}

/// Build the RouteTable from this Gateway's PortailConfig on a blocking thread
/// (`to_route_table` compiles regexes and resolves backend DNS — both sync).
async fn build_route_table(
    config: Arc<crate::config::PortailConfig>,
) -> anyhow::Result<RouteTable> {
    tokio::task::spawn_blocking(move || config.to_route_table())
        .await
        .map_err(|e| anyhow::anyhow!("route table build panicked: {}", e))?
}

/// Fingerprint of everything a reconcile pass would apply: the built config
/// (its JSON plus the cert/key bytes and k8s override maps that serde skips),
/// the status conditions about to be written, and the data-plane readiness
/// that feeds Programmed. Hash inputs with non-deterministic iteration order
/// (HashMaps, store-ordered route lists) are canonicalized by sorting.
fn reconcile_fingerprint(
    config: &crate::config::PortailConfig,
    accepted_cond: &status::GatewayCondition,
    listener_statuses: &HashMap<String, status::ListenerStatus>,
    usable: &UsableAddresses,
    route_status: &[RouteAcceptance],
    observed_route_parents: Vec<String>,
    dp_ready: bool,
) -> u64 {
    use std::hash::Hasher;

    fn write_sorted(h: &mut fnv::FnvHasher, mut items: Vec<String>) {
        items.sort_unstable();
        for item in &items {
            h.write(item.as_bytes());
            h.write(&[0]);
        }
    }

    let mut h = fnv::FnvHasher::default();
    if let Ok(bytes) = serde_json::to_vec(config) {
        h.write(&bytes);
    }
    // Fields the JSON serialization skips but a reconcile still applies:
    // cert/key PEM bytes and the k8s side-channel override maps.
    for l in &config.gateway.listeners {
        if let Some(tls) = &l.tls {
            for cr in &tls.certificate_refs {
                h.write(cr.name.as_bytes());
                if let Some(pem) = &cr.cert_pem {
                    h.write(pem);
                }
                if let Some(pem) = &cr.key_pem {
                    h.write(pem);
                }
            }
        }
    }
    write_sorted(
        &mut h,
        config
            .endpoint_overrides
            .iter()
            .map(|(k, v)| format!("ep{:?}={:?}", k, v))
            .chain(
                config
                    .app_protocol_overrides
                    .iter()
                    .map(|(k, v)| format!("ap{:?}={}", k, v)),
            )
            .chain(
                config
                    .headless_target_ports
                    .iter()
                    .map(|(k, v)| format!("ht{:?}={}", k, v)),
            )
            .collect(),
    );
    h.write(format!("{:?}", accepted_cond).as_bytes());
    write_sorted(
        &mut h,
        listener_statuses
            .iter()
            .map(|(name, ls)| format!("{}{:?}", name, ls))
            .collect(),
    );
    write_sorted(&mut h, usable.interface_ips.clone());
    write_sorted(
        &mut h,
        usable
            .network_ips
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect(),
    );
    write_sorted(
        &mut h,
        route_status.iter().map(|ra| format!("{:?}", ra)).collect(),
    );
    // Observed route status: `status.parents` is an atomic list written
    // read-modify-write, so a concurrent writer clobbering our entry must
    // change the fingerprint — otherwise the skip path would leave the
    // clobbered status in place until the slow safety-net requeue.
    write_sorted(&mut h, observed_route_parents);
    h.write(&[dp_ready as u8]);
    h.finish()
}

/// Merge per-Gateway configs into one data-plane config (legacy unscoped
/// mode, where a single process serves every Gateway).
///
/// Listener names and route parentRef sectionNames are prefixed with their
/// Gateway's identity so a route can never cross-match a same-named listener
/// on another Gateway; parentRefs without a sectionName are expanded to one
/// ref per listener of *their own* Gateway for the same reason (an unscoped
/// ref would attach to every merged listener).
fn merge_gateway_configs(
    configs: &HashMap<(String, String), Arc<crate::config::PortailConfig>>,
) -> crate::config::PortailConfig {
    use crate::config::{ParentRef, PortailConfig};

    let mut ordered: Vec<_> = configs.iter().collect();
    ordered.sort_by(|a, b| a.0.cmp(b.0));

    let mut merged = PortailConfig::default();
    merged.gateway.name = "portail-merged".to_string();
    merged.gateway.listeners.clear();

    for ((ns, name), cfg) in ordered {
        let prefix = format!("{}/{}/", ns, name);
        let mut c: PortailConfig = (**cfg).clone();

        for l in &mut c.gateway.listeners {
            l.name = format!("{}{}", prefix, l.name);
        }
        // (name, port) pairs of this Gateway's listeners, post-prefixing,
        // used to expand sectionName-less parentRefs.
        let listener_names: Vec<(String, u16)> = c
            .gateway
            .listeners
            .iter()
            .map(|l| (l.name.clone(), l.port))
            .collect();
        let expand = |refs: &mut Vec<ParentRef>| {
            let mut out = Vec::with_capacity(refs.len());
            for pr in refs.drain(..) {
                match &pr.section_name {
                    Some(section) => out.push(ParentRef {
                        section_name: Some(format!("{}{}", prefix, section)),
                        ..pr
                    }),
                    None => {
                        for (lname, lport) in &listener_names {
                            if pr.port.is_none_or(|p| *lport == p as u16) {
                                out.push(ParentRef {
                                    name: pr.name.clone(),
                                    section_name: Some(lname.clone()),
                                    port: pr.port,
                                });
                            }
                        }
                    }
                }
            }
            *refs = out;
        };
        for r in &mut c.http_routes {
            expand(&mut r.parent_refs);
        }
        for r in &mut c.tcp_routes {
            expand(&mut r.parent_refs);
        }
        for r in &mut c.udp_routes {
            expand(&mut r.parent_refs);
        }
        for r in &mut c.tls_routes {
            expand(&mut r.parent_refs);
        }

        merged.gateway.listeners.extend(c.gateway.listeners);
        merged.gateway.addresses.extend(c.gateway.addresses);
        merged.http_routes.extend(c.http_routes);
        merged.tcp_routes.extend(c.tcp_routes);
        merged.udp_routes.extend(c.udp_routes);
        merged.tls_routes.extend(c.tls_routes);
        merged.endpoint_overrides.extend(c.endpoint_overrides);
        merged
            .app_protocol_overrides
            .extend(c.app_protocol_overrides);
        merged.headless_target_ports.extend(c.headless_target_ports);
    }
    merged.gateway.addresses.dedup();
    merged
}

async fn reconcile(
    gateway: Arc<Gateway>,
    ctx: Arc<ControllerCtx>,
) -> Result<Action, ReconcileError> {
    let gw_name = gateway.name_any();
    let gw_ns = gateway.namespace().unwrap_or_else(|| "default".to_string());
    debug!("Reconciling Gateway {}/{}", gw_ns, gw_name);

    // GatewayClass acceptance is owned by portail-operator; it only provisions a
    // portail Deployment for Gateways whose class references this controller, so
    // we can trust the scope and skip an in-process class check here.

    // Snapshot reflector caches in one shot — no API server calls.
    let snapshot = ClusterSnapshot {
        http_routes: snapshot(&ctx.cache.http_routes),
        tcp_routes: snapshot(&ctx.cache.tcp_routes),
        tls_routes: snapshot(&ctx.cache.tls_routes),
        udp_routes: snapshot(&ctx.cache.udp_routes),
        namespace_labels: ctx
            .cache
            .namespaces
            .state()
            .iter()
            .filter_map(|ns| {
                let name = ns.metadata.name.clone()?;
                let labels = ns.metadata.labels.clone().unwrap_or_default();
                Some((name, labels))
            })
            .collect(),
        reference_grants: snapshot(&ctx.cache.reference_grants),
        services: snapshot(&ctx.cache.services),
    };

    let cert_data = resolve_gateway_certs(
        &gateway,
        &gw_ns,
        &snapshot.reference_grants,
        &ctx.cache.secrets,
    );
    let (mut services, named_reqs) = resolve_services(&snapshot.services);
    // A headless Service with a *named* targetPort needs endpoint data the Service
    // spec lacks; resolve those with a one-shot EndpointSlice list (no watch).
    if !named_reqs.is_empty() {
        resolve_named_target_ports(
            &ctx.client,
            &named_reqs,
            &mut services.headless_target_ports,
        )
        .await;
    }

    let (listener_statuses, accepted_cond) =
        validate_gateway(&gateway, &gw_ns, &cert_data, &snapshot.reference_grants);

    let mut usable = discover_usable_addresses();
    resolve_network_addresses(&ctx.client, &gateway, &gw_ns, &mut usable).await;

    // Build per-Gateway config; bail out with a failing Programmed condition on error.
    let mut result = match reconcile_to_config(&gateway, &snapshot, &cert_data, &services) {
        Ok(r) => r,
        Err(e) => {
            error!("Failed to reconcile Gateway {}/{}: {}", gw_ns, gw_name, e);
            let failure = status::GatewayCondition {
                ok: false,
                reason: "Invalid".into(),
                message: format!("Reconciliation failed: {}", e),
            };
            status::update_gateway_status(
                &ctx.client,
                &gateway,
                &accepted_cond,
                &failure,
                &HashMap::new(),
                &listener_statuses,
                &usable,
                ctx.manage_gateway_status,
            )
            .await;
            return Err(ReconcileError(e.to_string()));
        }
    };

    // Substitute resolved IPs for portail.epheo.eu/Network entries (reconcile_to_config
    // copies spec.addresses values as-is).
    if !usable.network_ips.is_empty() {
        result.config.gateway.addresses = result
            .config
            .gateway
            .addresses
            .iter()
            .map(|addr| {
                usable
                    .network_ips
                    .get(addr)
                    .cloned()
                    .unwrap_or_else(|| addr.clone())
            })
            .collect();
    }

    let bind_addresses = compute_bind_addresses(&result.config.gateway.addresses, &usable);
    ensure_data_plane_listeners(&ctx, &result.config.gateway.listeners, &bind_addresses);

    // Readiness keys on each listener's bound port (`target_port` when the
    // fronting Service decouples it, else the published port). Captured before
    // `result.config` is moved into the route-table build below. In the brief
    // window where a LoadBalancer pod has no NET_BIND_SERVICE and its Service is
    // not yet observed, a privileged published bind fails harmlessly and
    // readiness stays down until the Service (and its targetPort) appear.
    let required = required_endpoints(&result.config.gateway.listeners, &bind_addresses);

    // Data-plane readiness is computed before the (expensive) route-table
    // build: it depends only on bound ports, and it feeds both the readiness
    // latch and the reconcile fingerprint below.
    let dp_ready = ctx
        .data_plane
        .lock()
        .map(|dp| dp.is_ready_for_endpoints(&required))
        .unwrap_or(false);
    // Once the data plane has bound this gateway's listener ports, the
    // pod is serving — latch readiness on (never flips back).
    if dp_ready {
        ctx.ready.store(true, std::sync::atomic::Ordering::Release);
    }

    // Track this Gateway's config and derive the config the data plane
    // should actually run. In scoped mode (operator-managed, one Gateway per
    // process) that is simply this Gateway's config; in legacy unscoped mode
    // one process serves EVERY Gateway, so publishing only this Gateway's
    // config would clobber the others' routes in the shared table.
    let config = Arc::new(std::mem::take(&mut result.config));
    let effective_config = {
        let mut map = match ctx.gateway_configs.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        map.insert((gw_ns.clone(), gw_name.clone()), config.clone());
        // Prune Gateways that no longer exist — deletions don't run this
        // reconciler, so stale entries are collected on any later pass.
        let live: HashSet<(String, String)> = ctx
            .cache
            .gateways
            .state()
            .iter()
            .filter_map(|g| Some((g.namespace()?, g.metadata.name.clone()?)))
            .collect();
        map.retain(|k, _| live.contains(k));
        if map.len() <= 1 {
            config.clone()
        } else {
            Arc::new(merge_gateway_configs(&map))
        }
    };

    // Observed route status parents feed the fingerprint (see
    // `reconcile_fingerprint`) so external clobbers of the atomic
    // `status.parents` list are detected and re-applied.
    let observed_route_parents: Vec<String> = result
        .route_status
        .iter()
        .map(|ra| {
            let parents = match ra.kind {
                "HTTPRoute" => get_existing_route_parents_from_store(
                    &ctx.cache.http_routes,
                    &ra.namespace,
                    &ra.name,
                ),
                "TCPRoute" => get_existing_route_parents_from_store(
                    &ctx.cache.tcp_routes,
                    &ra.namespace,
                    &ra.name,
                ),
                "TLSRoute" => get_existing_route_parents_from_store(
                    &ctx.cache.tls_routes,
                    &ra.namespace,
                    &ra.name,
                ),
                "UDPRoute" => get_existing_route_parents_from_store(
                    &ctx.cache.udp_routes,
                    &ra.namespace,
                    &ra.name,
                ),
                _ => Vec::new(),
            };
            format!("{}/{}/{}:{:?}", ra.kind, ra.namespace, ra.name, parents)
        })
        .collect();

    // Short-circuit: when this pass would apply exactly what the last fully
    // successful pass already applied — same config (certs and override maps
    // included), same status conditions, same readiness — skip the route-table
    // rebuild and every status PATCH. This is what makes the watch echo of our
    // own status writes cheap, and turns the permanently-unaccepted-route case
    // from a 5s rebuild/PATCH loop into a slow safety-net re-check.
    let fingerprint = reconcile_fingerprint(
        &effective_config,
        &accepted_cond,
        &listener_statuses,
        &usable,
        &result.route_status,
        observed_route_parents,
        dp_ready,
    );
    if ctx
        .last_applied
        .lock()
        .map(|last| *last == Some(fingerprint))
        .unwrap_or(false)
    {
        let all_routes_accepted = result.route_status.iter().all(|ra| ra.accepted);
        debug!(
            "Gateway {}/{} unchanged since last applied reconcile; skipping rebuild and status writes",
            gw_ns, gw_name
        );
        return Ok(Action::requeue(Duration::from_secs(skip_requeue_secs(
            dp_ready,
            all_routes_accepted,
        ))));
    }

    // Per-listener attached-route counts, restricted to this Gateway's routes.
    let mut listener_route_counts: HashMap<String, i32> = gateway
        .spec
        .listeners
        .iter()
        .map(|l| (l.name.clone(), 0))
        .collect();
    for ra in &result.route_status {
        if ra.accepted {
            for listener_name in &ra.listener_names {
                *listener_route_counts
                    .entry(listener_name.clone())
                    .or_insert(0) += 1;
            }
        }
    }

    // Build + swap the RouteTable; on failure produce a failing Programmed
    // condition (route table build is the last step that can fail).
    let http_count = config.http_routes.len();
    let tcp_count = config.tcp_routes.len();
    let tls_count = config.tls_routes.len();
    let udp_count = config.udp_routes.len();

    let programmed_cond = match build_route_table(effective_config.clone()).await {
        Ok(route_table) => {
            ctx.routes.store(Arc::new(route_table));
            // Publish the config only after the table it produced is live:
            // the background DNS-refresh task re-resolves from this cell, and
            // must never pick up a config whose build failed — nor one whose
            // table hasn't been swapped in yet (the refresh CAS compares
            // against the live table).
            ctx.config_cell.store(Some(effective_config));
            info!(
                "Gateway {}/{} reconciled: {} HTTP, {} TCP, {} TLS, {} UDP routes",
                gw_ns, gw_name, http_count, tcp_count, tls_count, udp_count,
            );
            compute_programmed_condition(dp_ready)
        }
        Err(e) => {
            error!(
                "Failed to build route table for Gateway {}/{}: {}",
                gw_ns, gw_name, e
            );
            status::GatewayCondition {
                ok: false,
                reason: "Invalid".into(),
                message: format!("Route table conversion failed: {}", e),
            }
        }
    };
    let programmed = programmed_cond.ok;

    let mut statuses_ok = status::update_gateway_status(
        &ctx.client,
        &gateway,
        &accepted_cond,
        &programmed_cond,
        &listener_route_counts,
        &listener_statuses,
        &usable,
        ctx.manage_gateway_status,
    )
    .await;

    // Update per-route status — group by (kind, ns, name) so SSA writes all
    // parent entries together (otherwise multi-listener routes lose previous
    // entries on each patch).
    {
        use std::collections::BTreeMap;
        let field_manager = format!("portail-{}-{}", gw_ns, gw_name);
        let mut grouped: BTreeMap<(&str, &str, &str), Vec<status::RouteParentStatus>> =
            BTreeMap::new();
        for ra in &result.route_status {
            let route_programmed = ra.accepted && programmed;
            grouped
                .entry((ra.kind, &ra.namespace, &ra.name))
                .or_default()
                .push(status::RouteParentStatus {
                    controller_name: ctx.controller_name.clone(),
                    gateway_name: gw_name.clone(),
                    gateway_namespace: gw_ns.clone(),
                    section_name: ra.section_name.clone(),
                    port: None,
                    accepted: ra.accepted,
                    accepted_reason: ra.accepted_reason.clone(),
                    message: ra.message.clone(),
                    refs_resolved: ra.refs_resolved,
                    refs_reason: ra.refs_reason.clone(),
                    refs_message: ra.refs_message.clone(),
                    programmed: route_programmed,
                    generation: ra.generation,
                });
        }

        for ((kind, ns, name), parents) in &grouped {
            macro_rules! patch_route_status {
                ($store:expr, $ty:ty) => {{
                    let existing = get_existing_route_parents_from_store(&$store, ns, name);
                    statuses_ok &= status::update_route_status::<$ty>(
                        &ctx.client,
                        name,
                        ns,
                        parents,
                        &field_manager,
                        &existing,
                    )
                    .await;
                }};
            }
            match *kind {
                "HTTPRoute" => patch_route_status!(ctx.cache.http_routes, HTTPRoute),
                "TCPRoute" => patch_route_status!(ctx.cache.tcp_routes, TCPRoute),
                "TLSRoute" => patch_route_status!(ctx.cache.tls_routes, TLSRoute),
                "UDPRoute" => patch_route_status!(ctx.cache.udp_routes, UDPRoute),
                _ => {}
            }
        }
    }

    if !statuses_ok {
        // A status PATCH failed (apiserver blip). Without a fast retry the
        // wrong status would sit in the cluster until the slow safety-net
        // requeue. Leave `last_applied` unset so the next pass re-applies.
        if let Ok(mut last) = ctx.last_applied.lock() {
            *last = None;
        }
        return Ok(Action::requeue(Duration::from_secs(10)));
    }

    // Everything applied: record the fingerprint so identical future passes
    // (watch echoes, unconverged requeues with unchanged inputs) short-circuit.
    if let Ok(mut last) = ctx.last_applied.lock() {
        *last = Some(fingerprint);
    }

    // Requeue cadence is convergence-driven — see `success_requeue_secs`.
    let all_routes_accepted = result.route_status.iter().all(|ra| ra.accepted);
    Ok(Action::requeue(Duration::from_secs(success_requeue_secs(
        programmed,
        all_routes_accepted,
    ))))
}

/// Requeue cadence after a reconcile that APPLIED changes. A *converged*
/// Gateway — its data plane Programmed and every attached route Accepted —
/// only needs a slow safety-net re-check (600s). An unconverged one re-runs
/// in 5s so a status write that raced an unsynced reflector cache (or a
/// missed watch event) heals well inside the conformance 60s accept-gate.
/// A route held permanently not-Accepted only pays this fast cadence once:
/// the next pass sees an unchanged fingerprint, skips the rebuild and status
/// writes, and drops to the `skip_requeue_secs` cadence.
fn success_requeue_secs(programmed: bool, all_routes_accepted: bool) -> u64 {
    if programmed && all_routes_accepted {
        600
    } else {
        5
    }
}

/// Requeue cadence after a short-circuited reconcile (fingerprint unchanged):
/// nothing was rebuilt or written, so the fast heal cadence has nothing to
/// heal — back off to a periodic re-check instead.
fn skip_requeue_secs(programmed: bool, all_routes_accepted: bool) -> u64 {
    if programmed && all_routes_accepted {
        600
    } else {
        60
    }
}

fn error_policy(_obj: Arc<Gateway>, _error: &ReconcileError, _ctx: Arc<ControllerCtx>) -> Action {
    warn!("Gateway reconciliation error, requeueing in 30s");
    Action::requeue(Duration::from_secs(30))
}

/// Read existing parent status conditions from a route's reflector store entry.
fn get_existing_route_parents_from_store<K>(
    store: &Store<K>,
    ns: &str,
    name: &str,
) -> Vec<serde_json::Value>
where
    K: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope>
        + serde::Serialize
        + Clone
        + std::fmt::Debug,
    <K as kube::Resource>::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
    let obj_ref = ObjectRef::<K>::new(name).within(ns);
    match store.get(&obj_ref) {
        Some(route) => {
            let val = serde_json::to_value(route.as_ref()).unwrap_or_default();
            val.pointer("/status/parents")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
        }
        None => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ListenerConfig, Protocol};

    fn lc(port: u16, target_port: Option<u16>) -> ListenerConfig {
        ListenerConfig {
            name: "l".to_string(),
            protocol: Protocol::HTTP,
            port,
            target_port,
            hostname: None,
            address: None,
            interface: None,
            tls: None,
        }
    }

    #[test]
    fn required_endpoints_keys_on_bound_port() {
        let listeners = [lc(80, Some(8000)), lc(8080, None)];
        let eps = required_endpoints(&listeners, &[]);
        assert!(eps.contains(&(None, 8000)));
        assert!(eps.contains(&(None, 8080)));
        assert!(
            !eps.contains(&(None, 80)),
            "published port must not be the readiness key"
        );
    }

    #[test]
    fn required_endpoints_products_bind_addresses() {
        let listeners = [lc(443, Some(8001))];
        let eps = required_endpoints(
            &listeners,
            &["10.0.0.1".to_string(), "10.0.0.2".to_string()],
        );
        assert_eq!(eps.len(), 2);
        assert!(eps.contains(&(Some("10.0.0.1".to_string()), 8001)));
        assert!(eps.contains(&(Some("10.0.0.2".to_string()), 8001)));
    }

    #[test]
    fn watch_shape_parse() {
        // Absent → broad (legacy): every gate-able watch on.
        let all = WatchShape::parse(None);
        assert!(all.tls_secrets && all.namespaces);

        // Present-but-empty → minimal: no gate-able extras (routes stay
        // cluster-wide regardless — they are never gated).
        let min = WatchShape::parse(Some(""));
        assert!(!min.tls_secrets && !min.namespaces);

        // TLS-terminate listener only.
        let tls = WatchShape::parse(Some("tls"));
        assert!(tls.tls_secrets && !tls.namespaces);

        // Unknown / retired tokens (e.g. the old route-scoping tokens) are
        // ignored; recognized ones still apply.
        let u = WatchShape::parse(Some("routes-same, udp-routes, bogus , ns-labels"));
        assert!(u.namespaces && !u.tls_secrets);
    }

    #[test]
    fn success_requeue_cadence() {
        // Fully converged: slow safety-net re-check only.
        assert_eq!(success_requeue_secs(true, true), 600);
        // Data plane not yet Programmed, or a route not yet Accepted (e.g. its
        // reflector cache has not synced): fast re-check to converge inside the
        // conformance 60s accept-gate.
        assert_eq!(success_requeue_secs(false, true), 5);
        assert_eq!(success_requeue_secs(true, false), 5);
        assert_eq!(success_requeue_secs(false, false), 5);
    }

    /// Unscoped mode: merging two Gateways' configs must keep every route,
    /// prefix listener names per Gateway, and expand sectionName-less
    /// parentRefs to their own Gateway's listeners only — never the other's.
    #[test]
    fn merge_gateway_configs_isolates_gateways() {
        use crate::config::{
            GatewayConfig, HttpRouteConfig, ListenerConfig, ParentRef, PortailConfig,
            Protocol as CfgProtocol,
        };

        fn gw_config(gw: &str, listener: &str, port: u16, host: &str) -> PortailConfig {
            PortailConfig {
                gateway: GatewayConfig {
                    name: gw.to_string(),
                    listeners: vec![ListenerConfig {
                        name: listener.to_string(),
                        protocol: CfgProtocol::HTTP,
                        port,
                        target_port: None,
                        hostname: None,
                        address: None,
                        interface: None,
                        tls: None,
                    }],
                    addresses: vec![],
                },
                http_routes: vec![HttpRouteConfig {
                    // No sectionName: attaches to all of THIS gateway's listeners.
                    parent_refs: vec![ParentRef {
                        name: gw.to_string(),
                        section_name: None,
                        port: None,
                    }],
                    hostnames: vec![host.to_string()],
                    rules: vec![],
                }],
                ..Default::default()
            }
        }

        let mut map = HashMap::new();
        // Both Gateways use the SAME listener name "http" — the cross-match trap.
        map.insert(
            ("ns-a".to_string(), "gw-a".to_string()),
            Arc::new(gw_config("gw-a", "http", 8080, "a.example.com")),
        );
        map.insert(
            ("ns-b".to_string(), "gw-b".to_string()),
            Arc::new(gw_config("gw-b", "http", 9090, "b.example.com")),
        );

        let merged = merge_gateway_configs(&map);

        // Both Gateways' listeners survive, disambiguated by prefix.
        let names: Vec<&str> = merged
            .gateway
            .listeners
            .iter()
            .map(|l| l.name.as_str())
            .collect();
        assert_eq!(names, vec!["ns-a/gw-a/http", "ns-b/gw-b/http"]);

        // Both routes survive; each parentRef was expanded to exactly its own
        // Gateway's (prefixed) listener.
        assert_eq!(merged.http_routes.len(), 2);
        let route_a = merged
            .http_routes
            .iter()
            .find(|r| r.hostnames == ["a.example.com"])
            .unwrap();
        assert_eq!(route_a.parent_refs.len(), 1);
        assert_eq!(
            route_a.parent_refs[0].section_name.as_deref(),
            Some("ns-a/gw-a/http"),
        );
        let route_b = merged
            .http_routes
            .iter()
            .find(|r| r.hostnames == ["b.example.com"])
            .unwrap();
        assert_eq!(
            route_b.parent_refs[0].section_name.as_deref(),
            Some("ns-b/gw-b/http"),
        );
    }
}
