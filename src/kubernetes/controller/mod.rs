//! The Gateway API controller (`portail controller`): cluster-wide
//! reflectors, one reconcile per Gateway, the hub of snapshots data planes
//! sync from, and the reports they send back.
//!
//! Submodules: [`watch`] (reflector plumbing), [`translate`] (Gateway to
//! config and status), [`reconcile`] (the pass), [`hub`] and [`reports`]
//! (the sync state), [`validate`] (cert resolution and listener
//! conditions); `leader`, `server`, `auth`, `ca`, `params`, `provision`,
//! `programmed`, `gatewayclass` hold the runtime the split adds.

mod auth;
mod ca;
mod gatewayclass;
mod hub;
mod leader;
mod params;
mod programmed;
mod provision;
mod reconcile;
mod reports;
mod server;
#[cfg(test)]
mod testkit;
mod translate;
mod validate;
mod watch;

use futures::StreamExt;
use k8s_openapi::api::core::v1::{Namespace, Secret, Service};
use kube::api::Api;
use kube::runtime::controller::Action;
use kube::runtime::reflector::{ObjectRef, Store};
use kube::runtime::watcher;
use kube::runtime::{reflector, Controller, WatchStreamExt};
use kube::Client;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use gateway_api::experimental::tcproutes::TCPRoute;
use gateway_api::experimental::tlsroutes::TLSRoute;
use gateway_api::experimental::udproutes::UDPRoute;
use gateway_api::gatewayclasses::GatewayClass;
use gateway_api::gateways::Gateway;
use gateway_api::httproutes::HTTPRoute;
use gateway_api::referencegrants::ReferenceGrant;

use crate::cli::ControllerArgs;
use crate::kubernetes::addresses::NetworkCidrCache;
use crate::kubernetes::crds::{GatewayParameters, RateLimitPolicy};
use crate::kubernetes::reconciler::RouteVerdicts;
use crate::kubernetes::services::NamedPortCache;
use crate::logging::{info, warn};

use hub::Hub;
use reconcile::reconcile;
use reports::ReportStore;
use watch::{
    all_gateway_refs, create_optional_reflector, create_reflector, create_reflector_with_config,
    drop_unread_metadata, map_policy_to_gateways, map_route_to_gateways, map_secret_to_gateways,
    PendingWatches,
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
    /// Gateways whose class names this controller are ours.
    gateway_classes: Store<GatewayClass>,
    gateways: Store<Gateway>,
    http_routes: Store<HTTPRoute>,
    tcp_routes: Store<TCPRoute>,
    tls_routes: Store<TLSRoute>,
    udp_routes: Store<UDPRoute>,
    namespaces: Store<Namespace>,
    services: Store<Service>,
    secrets: Store<Secret>,
    reference_grants: Store<ReferenceGrant>,
    /// Portail's own policy CRD; empty store when the CRD is not installed.
    rate_limit_policies: Store<RateLimitPolicy>,
    /// Empty store when the CRD is not installed.
    #[allow(dead_code)] // Read by params::resolve once it resolves refs.
    gateway_parameters: Store<GatewayParameters>,
}

struct ControllerCtx {
    client: Client,
    controller_name: String,
    /// The data-plane image of a Managed Gateway whose parameters name none.
    dataplane_image: String,
    cache: ResourceCache,
    route_verdicts: Arc<RouteVerdicts>,
    named_ports: Arc<NamedPortCache>,
    networks: Arc<NetworkCidrCache>,
    hub: Arc<Hub>,
    reports: Arc<ReportStore>,
    leader: leader::Leader,
    #[allow(dead_code)] // Read by provision::reconcile once it provisions.
    owned: provision::Owned,
}

/// `portail controller`: run until `shutdown`, the lease is lost or the
/// Gateway watch ends.
pub async fn run(args: ControllerArgs, shutdown: CancellationToken) -> anyhow::Result<()> {
    let client = Client::try_default().await?;
    info!("Kubernetes client connected, starting Gateway API controller");
    let leader = leader::start(&client, &args, &shutdown);
    let ca = ca::load_or_create(&client, &args).await?;

    // --- Primary resource: Gateway ---
    // No `predicate_filter(predicates::generation)` here: it caches generation
    // per key and never clears on delete, so deleting and recreating a Gateway
    // with the same name (recreates restart at generation 1) would be
    // suppressed and the recreated Gateway never reconciled. Status-only watch
    // echoes of our own writes are instead absorbed by reconcile: an
    // unchanged snapshot is not published again, and status that already
    // matches the store is not written.
    let gw_writer = reflector::store::Writer::default();
    let store_gateways = gw_writer.as_reader();
    // A deletion runs no reconcile. A relist (InitDone) may also have
    // dropped Gateways deleted while the watch was down.
    let gateway_gone = Arc::new(tokio::sync::Notify::new());
    let gw_stream = reflector::reflector(
        gw_writer,
        watcher::watcher(
            Api::<Gateway>::all(client.clone()),
            watcher::Config::default(),
        )
        .modify(drop_unread_metadata),
    )
    .default_backoff()
    .inspect({
        let gone = gateway_gone.clone();
        move |event| {
            if matches!(
                event,
                Ok(watcher::Event::Delete(_) | watcher::Event::InitDone)
            ) {
                gone.notify_one();
            }
        }
    })
    .applied_objects();

    // --- Secondary resources ---
    // Each resource gets a reflector (store + stream). The stream feeds into
    // Controller::watches_stream() with a mapper that targets affected Gateway(s).
    // No watch is predicate-filtered: status-only events (our own writes
    // included) reach reconcile, which absorbs them.

    // HTTPRoute is required (GA); TCP/TLS/UDPRoute are experimental (optional).
    // A route in any namespace, of any kind, may parentRef a Gateway and must
    // get a status (attached, or rejected with NotAllowedByListeners /
    // NoMatchingParent), which requires observing it. The optional CRDs are
    // probed concurrently so a contended apiserver does not serialize the
    // cold start.
    let (store_http_routes, http_route_stream) =
        create_reflector(Api::<HTTPRoute>::all(client.clone()));
    // RateLimitPolicy and GatewayParameters ride the optional-CRD path like
    // the experimental routes.
    let pending_watches = PendingWatches::default();
    let (tcp_reflector, tls_reflector, udp_reflector, policy_reflector, params_reflector) = tokio::join!(
        create_optional_reflector(
            Api::<TCPRoute>::all(client.clone()),
            &client,
            &pending_watches
        ),
        create_optional_reflector(
            Api::<TLSRoute>::all(client.clone()),
            &client,
            &pending_watches
        ),
        create_optional_reflector(
            Api::<UDPRoute>::all(client.clone()),
            &client,
            &pending_watches
        ),
        create_optional_reflector(
            Api::<RateLimitPolicy>::all(client.clone()),
            &client,
            &pending_watches
        ),
        create_optional_reflector(
            Api::<GatewayParameters>::all(client.clone()),
            &client,
            &pending_watches
        ),
    );
    let (store_tcp_routes, tcp_route_stream) = tcp_reflector;
    let (store_tls_routes, tls_route_stream) = tls_reflector;
    let (store_udp_routes, udp_route_stream) = udp_reflector;
    let (store_rate_limit_policies, policy_stream) = policy_reflector;
    let (store_gateway_parameters, params_stream) = params_reflector;
    tokio::spawn(pending_watches.recheck_every(
        client.clone(),
        Duration::from_secs(RESYNC_SECS),
        shutdown.clone(),
    ));

    let (store_namespaces, namespace_stream) =
        create_reflector(Api::<Namespace>::all(client.clone()));
    let (store_services, service_stream) = create_reflector(Api::<Service>::all(client.clone()));
    let (store_reference_grants, reference_grant_stream) =
        create_reflector(Api::<ReferenceGrant>::all(client.clone()));
    // Every Gateway is watched, other controllers' too: the class tells
    // which are ours.
    let (store_gateway_classes, gateway_class_stream) =
        create_reflector(Api::<GatewayClass>::all(client.clone()));
    let (class_changed, class_changes) = futures::channel::mpsc::unbounded::<()>();
    let gateway_class_stream = gateway_class_stream.inspect(move |_| {
        let _ = class_changed.unbounded_send(());
    });
    // TLS secrets, field-filtered to type=kubernetes.io/tls.
    let (store_secrets, secret_stream) = create_reflector_with_config(
        Api::<Secret>::all(client.clone()),
        watcher::Config::default()
            .fields("type=kubernetes.io/tls")
            .streaming_lists(),
    );
    let (owned, owned_triggers) = provision::Owned::start(&client);

    let ctx = Arc::new(ControllerCtx {
        client: client.clone(),
        controller_name: args.controller_name.clone(),
        dataplane_image: args.dataplane_image.clone(),
        cache: ResourceCache {
            gateway_classes: store_gateway_classes,
            gateways: store_gateways.clone(),
            http_routes: store_http_routes,
            tcp_routes: store_tcp_routes,
            tls_routes: store_tls_routes,
            udp_routes: store_udp_routes,
            namespaces: store_namespaces,
            services: store_services,
            secrets: store_secrets,
            reference_grants: store_reference_grants,
            rate_limit_policies: store_rate_limit_policies,
            gateway_parameters: store_gateway_parameters,
        },
        route_verdicts: Arc::default(),
        // A stale read lasts as long as before the cache: every requeue
        // listed the slices again.
        named_ports: Arc::new(NamedPortCache::new(Duration::from_secs(RESYNC_SECS))),
        networks: Arc::default(),
        hub: Arc::default(),
        reports: Arc::default(),
        leader: leader.clone(),
        owned,
    });

    tokio::spawn({
        let ctx = ctx.clone();
        let shutdown = shutdown.clone();
        async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = gateway_gone.notified() => reconcile::forget_deleted_gateways(&ctx),
                }
            }
        }
    });
    tokio::spawn(gatewayclass::run(
        ctx.clone(),
        class_changes,
        shutdown.clone(),
    ));
    let sync = tokio::spawn(server::serve(
        ctx.clone(),
        ca,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), args.sync_port),
        shutdown.clone(),
    ));

    // A report moved: reconcile its Gateway; moved DNS means backend pods
    // came or went, and a named targetPort's number lives on the pods.
    let named_ports = ctx.named_ports.clone();
    let report_triggers = ctx
        .reports
        .triggers()
        .expect("a fresh report store has its triggers")
        .map(move |trigger| {
            if trigger.dns_moved {
                named_ports.dns_changed();
            }
            ObjectRef::<Gateway>::new(&trigger.gateway.name).within(&trigger.gateway.namespace)
        });

    // --- Gateway controller ---
    // Routes → map to parent Gateway(s) via parentRefs (targeted)
    // Secret → map to Gateways referencing it in TLS config (targeted)
    // Service, Namespace, ReferenceGrant, GatewayClass, GatewayParameters →
    // all Gateways (broad, infrequent changes)
    // Each watches_stream closure captures an owned gateway store clone via move.
    let [gw1, gw2, gw3, gw4, gw5, gw6, gw7, gw8, gw9, gw10, gw11] =
        std::array::from_fn::<_, 11, _>(|_| store_gateways.clone());
    let [cn1, cn2, cn3, cn4, cn5] =
        std::array::from_fn::<_, 5, _>(|_| args.controller_name.clone());

    let gw_controller = Controller::for_stream(gw_stream, store_gateways)
        // Routes: targeted, to the Gateway(s) its parentRefs name and the
        // ones its status says we reported on.
        .watches_stream(http_route_stream, move |route: HTTPRoute| {
            map_route_to_gateways(&route, &route.spec.parent_refs, &gw1, &cn1)
        })
        .watches_stream(tcp_route_stream, move |route: TCPRoute| {
            map_route_to_gateways(&route, &route.spec.parent_refs, &gw2, &cn2)
        })
        .watches_stream(tls_route_stream, move |route: TLSRoute| {
            map_route_to_gateways(&route, &route.spec.parent_refs, &gw3, &cn3)
        })
        .watches_stream(udp_route_stream, move |route: UDPRoute| {
            map_route_to_gateways(&route, &route.spec.parent_refs, &gw4, &cn4)
        })
        // Secret: targeted — only reconcile Gateways referencing this secret in TLS
        .watches_stream(secret_stream, move |secret: Secret| {
            map_secret_to_gateways(&secret, &gw5)
        })
        // RateLimitPolicy: targeted like routes, by targetRef and status.
        .watches_stream(policy_stream, move |policy: RateLimitPolicy| {
            map_policy_to_gateways(&policy, &gw6, &cn5)
        })
        // These have multi-hop mappings; the 1s debounce coalesces bursts.
        // (No EndpointSlice watch: backend pod churn is picked up by the data
        // planes' DNS refresh, which their reports carry back.)
        .watches_stream(service_stream, move |_: Service| all_gateway_refs(&gw7))
        .watches_stream(namespace_stream, move |_: Namespace| all_gateway_refs(&gw8))
        .watches_stream(reference_grant_stream, move |_: ReferenceGrant| {
            all_gateway_refs(&gw9)
        })
        // A class appearing or changing controllerName flips which Gateways
        // are ours, so re-evaluate all of them.
        .watches_stream(gateway_class_stream, move |_: GatewayClass| {
            all_gateway_refs(&gw10)
        })
        .watches_stream(params_stream, move |_: GatewayParameters| {
            all_gateway_refs(&gw11)
        })
        .reconcile_on(report_triggers)
        .reconcile_on(owned_triggers)
        .reconcile_all_on(leader.gained())
        .with_config(kube::runtime::controller::Config::default().debounce(Duration::from_secs(1)))
        .shutdown_on_signal()
        .run(reconcile, error_policy, ctx);

    info!("Gateway API controller started, watching for resource changes");

    tokio::select! {
        _ = gw_controller.for_each(|result| async move {
            if let Err(e) = result {
                warn!("Gateway reconciliation error: {}", e);
            }
        }) => {
            info!("Gateway controller stream ended");
        }
        exit = sync => {
            return match exit {
                Ok(result) => result.and_then(|()| Err(anyhow::anyhow!("sync server stopped"))),
                Err(e) => Err(anyhow::anyhow!("sync server task failed: {}", e)),
            };
        }
        _ = shutdown.cancelled() => {
            info!("Controller received shutdown signal");
        }
    }

    Ok(())
}

/// The safety-net re-check of a converged Gateway, in seconds.
const RESYNC_SECS: u64 = 600;

/// Requeue cadence after a reconcile that published or wrote something. A
/// *converged* Gateway — its data plane Programmed and every attached route
/// Accepted — only needs a slow safety-net re-check (600s). An unconverged
/// one re-runs in 5s so a status write that raced an unsynced reflector
/// cache (or a missed watch event) heals well inside the conformance 60s
/// accept-gate. A route held permanently not-Accepted only pays this fast
/// cadence once: the next pass finds nothing to publish or write and drops
/// to the `skip_requeue_secs` cadence.
fn success_requeue_secs(programmed: bool, all_routes_accepted: bool) -> u64 {
    if programmed && all_routes_accepted {
        RESYNC_SECS
    } else {
        5
    }
}

/// Requeue cadence after a reconcile that published and wrote nothing: the
/// fast heal cadence has nothing to heal, so back off to a periodic re-check.
fn skip_requeue_secs(programmed: bool, all_routes_accepted: bool) -> u64 {
    if programmed && all_routes_accepted {
        RESYNC_SECS
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
