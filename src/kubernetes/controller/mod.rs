//! Gateway API controller: kube-rs Controller wiring and shared context.
//!
//! Submodules: [`watch`] (reflector plumbing + WatchShape), [`reconcile`]
//! (the per-Gateway reconcile pass), [`validate`] (cert resolution and
//! listener/Gateway condition computation).

mod reconcile;
mod validate;
mod watch;

pub use watch::WatchShape;

use arc_swap::{ArcSwap, ArcSwapOption};
use futures::StreamExt;
use k8s_openapi::api::core::v1::{Namespace, Secret, Service};
use kube::api::Api;
use kube::runtime::controller::Action;
use kube::runtime::reflector::{ObjectRef, Store};
use kube::runtime::watcher;
use kube::runtime::{reflector, Controller, WatchStreamExt};
use kube::Client;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use gateway_api::experimental::tcproutes::TCPRoute;
use gateway_api::experimental::tlsroutes::TLSRoute;
use gateway_api::experimental::udproutes::UDPRoute;
use gateway_api::gateways::Gateway;
use gateway_api::httproutes::HTTPRoute;
use gateway_api::referencegrants::ReferenceGrant;

use crate::logging::{info, warn};
use crate::routing::RouteTable;

use reconcile::reconcile;
use watch::{
    all_gateway_refs, create_optional_reflector, create_reflector, create_reflector_gated,
    create_reflector_gated_with_config, map_route_to_gateways,
};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
