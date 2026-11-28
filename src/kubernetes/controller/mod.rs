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
mod ca_bundles;
mod external_ips;
mod gateway_backends;
mod gatewayclass;
mod hub;
mod issuer;
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

use crate::kubernetes::reads::{Kind as ReadKind, ReadIndex, Recorded};
use futures::StreamExt;
use gateway_api::backendtlspolicies::BackendTlsPolicy;
use k8s_openapi::api::core::v1::{ConfigMap, Namespace, Secret, Service};
use kube::api::Api;
use kube::core::PartialObjectMeta;
use kube::runtime::controller::Action;
use kube::runtime::reflector::{ObjectRef, Store};
use kube::runtime::watcher;
use kube::runtime::{reflector, Controller, WatchStreamExt};
use kube::{Client, ResourceExt};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use gateway_api::experimental::tcproutes::v1::TcpRoute;
use gateway_api::experimental::tlsroutes::v1::TlsRoute;
use gateway_api::experimental::udproutes::v1::UdpRoute;
use gateway_api::gatewayclasses::GatewayClass;
use gateway_api::gateways::Gateway;
use gateway_api::httproutes::HttpRoute;
use gateway_api::referencegrants::v1beta1::ReferenceGrant;

use crate::cli::ControllerArgs;
use crate::kubernetes::addresses::NetworkCidrCache;
use crate::kubernetes::crds::{GatewayParameters, RateLimitPolicy};
use crate::kubernetes::reconciler::RouteVerdicts;
use crate::kubernetes::services::NamedPortCache;
use crate::logging::{info, warn};
use crate::sync::GatewayKey;

use hub::Hub;
use reconcile::reconcile;
use reports::ReportStore;
use watch::{
    all_gateway_refs, create_optional_reflector, create_reflector, create_reflector_with_config,
    drop_unread_metadata, map_policy_to_gateways, map_route_to_gateways, map_secret_to_gateways,
    Listings, PendingWatches,
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
    http_routes: Store<HttpRoute>,
    tcp_routes: Store<TcpRoute>,
    tls_routes: Store<TlsRoute>,
    udp_routes: Store<UdpRoute>,
    namespaces: Recorded<Namespace>,
    services: Recorded<Service>,
    secrets: Store<Secret>,
    reference_grants: Recorded<ReferenceGrant>,
    /// Portail's own policy CRD; empty store when the CRD is not installed.
    rate_limit_policies: Store<RateLimitPolicy>,
    /// Empty store when the CRD is not installed.
    gateway_parameters: Recorded<GatewayParameters>,
    /// Empty store when the CRD is not installed.
    backend_tls_policies: Recorded<BackendTlsPolicy>,
    configmaps: Recorded<PartialObjectMeta<ConfigMap>>,
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
    certs: Arc<validate::LastGoodCerts>,
    ca_bundles: ca_bundles::CaBundles,
    read_retries: reconcile::ReadRetries,
    hub: Arc<Hub>,
    reports: Arc<ReportStore>,
    /// Who names whom as a backend of kind Gateway, and what each exposes.
    backends: Arc<gateway_backends::GatewayBackends>,
    /// What each Gateway's passes read of the recorded kinds.
    reads: Arc<ReadIndex>,
    /// The externalIPs each Gateway asks for and is allowed.
    claims: Arc<external_ips::Claims>,
    leader: leader::Leader,
    /// Whether every reflector delivered its initial list, and which did
    /// not. The sync server answers 503 until then, /readyz names them.
    synced: Listings,
    owned: provision::Owned,
    /// Wakes the class pass, which removes departed Gateways' entries.
    departures: gatewayclass::Departures,
    /// Whether sync tokens are verified locally, which the class status
    /// says as portail.epheo.eu/GatewaysIsolated.
    isolated: tokio::sync::watch::Receiver<bool>,
}

/// `portail controller`: run until `shutdown`, the lease is lost or the
/// Gateway watch ends.
pub async fn run(args: ControllerArgs, shutdown: CancellationToken) -> anyhow::Result<()> {
    let client = watch::client().await?;
    info!("Kubernetes client connected, starting Gateway API controller");
    // /livez from here on; /readyz once the CA is loaded and the caches
    // synced. A standby is ready: it answers 503 and the client dials again.
    let ready = Arc::new(AtomicBool::new(false));
    tokio::spawn(crate::admin::serve(
        SocketAddr::new(args.admin.admin_address, args.admin.readiness_port),
        ready.clone(),
        shutdown.clone(),
    ));
    let Some(ca) = load_ca(&client, &args, &shutdown).await? else {
        return Ok(());
    };
    let elector = leader::start(&client, &args);
    let leader = elector.leader();

    // --- Primary resource: Gateway ---
    // No `predicate_filter(predicates::generation)` here: it caches generation
    // per key and never clears on delete, so deleting and recreating a Gateway
    // with the same name (recreates restart at generation 1) would be
    // suppressed and the recreated Gateway never reconciled. Status-only watch
    // echoes of our own writes are instead absorbed by reconcile: an
    // unchanged snapshot is not published again, and status that already
    // matches the store is not written.
    let listings = Listings::default();
    let gw_listing = listings.track("Gateway", false);
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
        .modify(drop_unread_metadata)
        .inspect(move |event| gw_listing.observe(event)),
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
    // NoMatchingParent), which requires observing it.
    let (store_http_routes, http_route_stream) =
        create_reflector(Api::<HttpRoute>::all(client.clone()), &listings);
    // RateLimitPolicy and GatewayParameters ride the optional-CRD path like
    // the experimental routes: a plain Gateway API install lacks the routes,
    // a missing GatewayParameters CRD must not hold every Gateway either.
    let pending_watches = Arc::new(PendingWatches::default());
    let (store_tcp_routes, tcp_route_stream) = create_optional_reflector(
        Api::<TcpRoute>::all(client.clone()),
        &client,
        &pending_watches,
        &listings,
    );
    let (store_tls_routes, tls_route_stream) = create_optional_reflector(
        Api::<TlsRoute>::all(client.clone()),
        &client,
        &pending_watches,
        &listings,
    );
    let (store_udp_routes, udp_route_stream) = create_optional_reflector(
        Api::<UdpRoute>::all(client.clone()),
        &client,
        &pending_watches,
        &listings,
    );
    let (store_rate_limit_policies, policy_stream) = create_optional_reflector(
        Api::<RateLimitPolicy>::all(client.clone()),
        &client,
        &pending_watches,
        &listings,
    );
    let (store_gateway_parameters, params_stream) = create_optional_reflector(
        Api::<GatewayParameters>::all(client.clone()),
        &client,
        &pending_watches,
        &listings,
    );
    let (store_backend_tls_policies, backend_tls_stream) = create_optional_reflector(
        Api::<BackendTlsPolicy>::all(client.clone()),
        &client,
        &pending_watches,
        &listings,
    );
    tokio::spawn(pending_watches.recheck_every(
        client.clone(),
        Duration::from_secs(RESYNC_SECS),
        shutdown.clone(),
    ));

    let (store_namespaces, namespace_stream) =
        create_reflector(Api::<Namespace>::all(client.clone()), &listings);
    // A BackendTLSPolicy's caCertificateRefs name ConfigMaps of its
    // namespace, any namespace: by metadata, see `ca_bundles`.
    let (store_configmaps, configmap_stream) = create_reflector_with_config(
        Api::<PartialObjectMeta<ConfigMap>>::all(client.clone()),
        watcher::Config::default(),
        &listings,
    );
    let (store_services, service_stream) =
        create_reflector(Api::<Service>::all(client.clone()), &listings);
    let (store_reference_grants, reference_grant_stream) =
        create_reflector(Api::<ReferenceGrant>::all(client.clone()), &listings);
    // Every Gateway is watched, other controllers' too: the class tells
    // which are ours.
    let (store_gateway_classes, gateway_class_stream) =
        create_reflector(Api::<GatewayClass>::all(client.clone()), &listings);
    let (class_events, class_changes) = gatewayclass::class_events();
    let issuer = Arc::new(issuer::IssuerKeys::new(client.clone()));
    tokio::spawn(issuer.clone().run(shutdown.clone()));
    let isolated = issuer.watch();
    tokio::spawn(
        class_events
            .clone()
            .on_change(issuer.watch(), shutdown.clone()),
    );
    let gateway_class_stream = class_events.on(gateway_class_stream);
    // A class seen before its parameters is judged again when they come.
    let params_stream = class_events.on(params_stream);
    // TLS secrets, field-filtered to type=kubernetes.io/tls.
    let (store_secrets, secret_stream) = create_reflector_with_config(
        Api::<Secret>::all(client.clone()),
        watcher::Config::default()
            .fields("type=kubernetes.io/tls")
            .streaming_lists(),
        &listings,
    );
    let (owned, owned_triggers) = provision::Owned::start(
        &client,
        store_gateways.clone(),
        &listings,
        provision::ControllerEndpoint::new(&args, &ca),
    );

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
            namespaces: Recorded::new(store_namespaces, ReadKind::Namespace),
            services: Recorded::new(store_services, ReadKind::Service),
            secrets: store_secrets,
            reference_grants: Recorded::new(store_reference_grants, ReadKind::ReferenceGrant),
            rate_limit_policies: store_rate_limit_policies,
            gateway_parameters: Recorded::new(
                store_gateway_parameters,
                ReadKind::GatewayParameters,
            ),
            backend_tls_policies: Recorded::new(
                store_backend_tls_policies,
                ReadKind::BackendTlsPolicy,
            ),
            configmaps: Recorded::new(store_configmaps, ReadKind::ConfigMap),
        },
        reads: Arc::default(),
        claims: Arc::default(),
        route_verdicts: Arc::default(),
        // A stale read lasts as long as before the cache: every requeue
        // listed the slices again.
        named_ports: Arc::new(NamedPortCache::new(Duration::from_secs(RESYNC_SECS))),
        networks: Arc::default(),
        certs: Arc::default(),
        ca_bundles: Default::default(),
        read_retries: Default::default(),
        hub: Arc::default(),
        reports: Arc::default(),
        backends: Arc::default(),
        leader: leader.clone(),
        synced: listings.clone(),
        owned,
        departures: Default::default(),
        isolated,
    });

    // Every Gateway is reconciled once the caches are synced: passes before
    // that returned without a trace. Until then the kinds holding it are
    // named, with why.
    let (caches_synced, synced_trigger) = futures::channel::mpsc::unbounded::<()>();
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            let mut warn_every = tokio::time::interval(Duration::from_secs(UNLISTED_WARN_SECS));
            warn_every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // interval()'s first tick fires immediately.
            warn_every.tick().await;
            while !listings.is_synced() {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = listings.changed() => {}
                    _ = warn_every.tick() => warn!(
                        "Caches not synced, nothing is published: {}",
                        listings
                            .unlisted()
                            .iter()
                            .map(|(kind, cause)| format!("{kind} ({cause})"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                }
            }
            info!("Caches synced");
            ready.store(true, Ordering::Release);
            let _ = caches_synced.unbounded_send(());
        }
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

    let tls = ca.server_config();
    let signer = ca.serving_key();
    tokio::spawn(ca.keep_renewed(shutdown.clone()));
    let (lapses, lapse_wakes) = server::Lapses::new();
    let sync_server = server::SyncServer::new(
        ctx.clone(),
        auth::Authenticator::new(
            client.clone(),
            &args.namespace,
            &args.service_name,
            auth::own_service_account(),
            issuer,
        ),
        signer,
        lapses,
        shutdown.clone(),
        args.sync_peer_connections,
        args.sync_gateway_polls,
    );
    let listener = tokio::net::TcpListener::bind(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        args.sync_port,
    ))
    .await?;
    let sync = tokio::spawn(server::serve(sync_server, tls, listener));

    let report_ctx = ctx.clone();
    let report_triggers = ctx
        .reports
        .triggers()
        .expect("a fresh report store has its triggers")
        .map(move |trigger| report_trigger(&report_ctx, trigger));
    let lapse_ctx = ctx.clone();
    let lapse_triggers = lapse_wakes.map(move |gateway| {
        lapse_ctx
            .backends
            .refresh(&gateway, &lapse_ctx.hub, &lapse_ctx.reports);
        gateway_ref(&gateway)
    });
    let dependent_triggers = ctx
        .backends
        .wakes()
        .expect("a fresh backend index has its wakes")
        .map(|gateway| gateway_ref(&gateway));
    let claim_triggers = ctx
        .claims
        .wakes()
        .expect("a fresh claims index has its wakes")
        .map(|gateway| gateway_ref(&gateway));

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
    let [reads1, reads2, reads3, reads4, reads5, reads6] =
        std::array::from_fn::<_, 6, _>(|_| ctx.reads.clone());
    let [gw12, gw13] = std::array::from_fn::<_, 2, _>(|_| store_gateways.clone());
    let controller_name = args.controller_name.clone();

    let gw_controller = Controller::for_stream(gw_stream, store_gateways)
        // Routes: targeted, to the Gateway(s) its parentRefs name and the
        // ones its status says we reported on.
        .watches_stream(http_route_stream, move |route: HttpRoute| {
            map_route_to_gateways(&route, &route.spec.parent_refs, &gw1, &cn1)
        })
        .watches_stream(tcp_route_stream, move |route: TcpRoute| {
            map_route_to_gateways(&route, &route.spec.parent_refs, &gw2, &cn2)
        })
        .watches_stream(tls_route_stream, move |route: TlsRoute| {
            map_route_to_gateways(&route, &route.spec.parent_refs, &gw3, &cn3)
        })
        .watches_stream(udp_route_stream, move |route: UdpRoute| {
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
        // These have multi-hop mappings.
        // (No EndpointSlice watch: backend pod churn is picked up by the data
        // planes' DNS refresh, which their reports carry back.)
        // Only the Gateways whose passes read the object, and those with
        // none recorded yet.
        .watches_stream(service_stream, move |svc: Service| {
            service_readers(&reads1, &gw7, &svc)
        })
        .watches_stream(namespace_stream, move |ns: Namespace| {
            readers(&reads2, &gw8, ReadKind::Namespace, &ns)
        })
        .watches_stream(reference_grant_stream, move |grant: ReferenceGrant| {
            readers(&reads3, &gw9, ReadKind::ReferenceGrant, &grant)
        })
        // A class appearing or changing controllerName flips which Gateways
        // are ours, so re-evaluate all of them.
        .watches_stream(gateway_class_stream, move |_: GatewayClass| {
            all_gateway_refs(&gw10)
        })
        .watches_stream(params_stream, move |params: GatewayParameters| {
            readers(&reads4, &gw11, ReadKind::GatewayParameters, &params)
        })
        // A policy wakes its readers, and the Gateways its status names, so
        // a policy that moved to another Service drops its old entries.
        .watches_stream(backend_tls_stream, move |policy: BackendTlsPolicy| {
            let mut woken = readers(&reads5, &gw12, ReadKind::BackendTlsPolicy, &policy);
            for gateway in reported_ancestors(&policy, &gw12, &controller_name) {
                if !woken.contains(&gateway) {
                    woken.push(gateway);
                }
            }
            woken
        })
        .watches_stream(
            configmap_stream,
            move |configmap: PartialObjectMeta<ConfigMap>| {
                readers(&reads6, &gw13, ReadKind::ConfigMap, &configmap)
            },
        )
        .reconcile_on(report_triggers)
        .reconcile_on(lapse_triggers)
        .reconcile_on(dependent_triggers)
        .reconcile_on(claim_triggers)
        .reconcile_on(owned_triggers)
        // Also what makes an early-ended pass's empty read set safe: see
        // ReadIndex::during.
        .reconcile_all_on(futures::stream::select(leader.gained(), synced_trigger))
        // No debounce: a burst on one Gateway is bounded by its pass in
        // flight, the runner holding one rerun at most, never by a timer.
        // In-flight passes finish before the lease is released below.
        .graceful_shutdown_on(shutdown.clone().cancelled_owned())
        .run(timed_reconcile, error_policy, ctx);

    info!("Gateway API controller started, watching for resource changes");

    let controller = gw_controller.for_each(|result| async move {
        if let Err(e) = result {
            warn!("Gateway controller: {:#}", anyhow::Error::new(e));
        }
    });
    until_stopped(controller, sync, leader.lost(), &shutdown).await?;
    elector.stop().await;
    Ok(())
}

/// Until the controller stream ends: on shutdown, once its passes in flight
/// finished. A stopped sync server is an error unless shutting down; a lost
/// lease ends it at once, while draining too: a stuck pass must not write
/// after a standby took the lease.
async fn until_stopped(
    controller: impl std::future::Future<Output = ()>,
    sync: impl std::future::Future<Output = Result<anyhow::Result<()>, tokio::task::JoinError>>,
    lost: impl std::future::Future<Output = ()>,
    shutdown: &CancellationToken,
) -> anyhow::Result<()> {
    let mut controller = std::pin::pin!(controller);
    let mut lost = std::pin::pin!(lost);
    enum Exit {
        Controller,
        Sync(Result<anyhow::Result<()>, tokio::task::JoinError>),
        Lost,
    }
    let exit = tokio::select! {
        _ = controller.as_mut() => Exit::Controller,
        exit = sync => Exit::Sync(exit),
        _ = lost.as_mut() => Exit::Lost,
    };
    match exit {
        Exit::Controller => info!("Gateway controller stopped"),
        // A standby's turn: kubelet restarts this process as one.
        Exit::Lost => anyhow::bail!("leader election lost"),
        Exit::Sync(_) if shutdown.is_cancelled() => tokio::select! {
            _ = controller => info!("Gateway controller stopped"),
            _ = lost => anyhow::bail!("leader election lost"),
        },
        Exit::Sync(Ok(Ok(()))) => anyhow::bail!("sync server stopped"),
        Exit::Sync(Ok(Err(e))) => return Err(e.context("sync server")),
        Exit::Sync(Err(e)) => anyhow::bail!("sync server task failed: {}", e),
    }
    Ok(())
}

/// The CA, retried at the election's cadence while the apiserver does not
/// answer; `None` on shutdown. A stored CA that cannot be used is fatal:
/// someone must delete its Secret.
async fn load_ca(
    client: &Client,
    args: &ControllerArgs,
    shutdown: &CancellationToken,
) -> anyhow::Result<Option<ca::ControllerCa>> {
    loop {
        match ca::load_or_create(client, &args.namespace, &args.service_name).await {
            Ok(ca) => return Ok(Some(ca)),
            Err(e @ ca::CaError::Invalid(_)) => return Err(e.into()),
            Err(e) => warn!("{}; retrying in {:?}", e, leader::RETRY_PERIOD),
        }
        tokio::select! {
            _ = shutdown.cancelled() => return Ok(None),
            _ = tokio::time::sleep(leader::RETRY_PERIOD) => {}
        }
    }
}

/// The reconcile a report change asks for. Moved DNS means backend pods
/// came or went, and a named targetPort's number lives on the pods. The
/// Gateways naming this one wake only if its exposure moved.
fn report_trigger(ctx: &ControllerCtx, trigger: reports::Trigger) -> ObjectRef<Gateway> {
    if trigger.dns_moved {
        ctx.named_ports.dns_changed();
    }
    ctx.backends
        .refresh(&trigger.gateway, &ctx.hub, &ctx.reports);
    gateway_ref(&trigger.gateway)
}

fn gateway_ref(gateway: &GatewayKey) -> ObjectRef<Gateway> {
    ObjectRef::new(&gateway.name).within(&gateway.namespace)
}

/// The safety-net re-check of a converged Gateway, in seconds.
const RESYNC_SECS: u64 = 600;

/// How often the kinds holding the caches unsynced are logged: the
/// cold-start diagnostic cadence the in-pod controller's not-ready
/// watchdog used.
const UNLISTED_WARN_SECS: u64 = 30;

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

/// A pass, timed whatever ends it: the histogram counts every pass.
async fn timed_reconcile(
    gateway: Arc<Gateway>,
    ctx: Arc<ControllerCtx>,
) -> Result<Action, ReconcileError> {
    let start = std::time::Instant::now();
    let key = GatewayKey::new(gateway.namespace().unwrap_or_default(), gateway.name_any());
    let reads = ctx.reads.clone();
    let done = reads.during(key, reconcile(gateway, ctx)).await;
    crate::metrics::RECONCILE_DURATION.observe(start.elapsed());
    done
}

/// The Gateways a change to `object` of `kind` wakes: those whose passes
/// read it, and those with no pass recorded yet.
fn readers<K: kube::Resource>(
    reads: &ReadIndex,
    gateways: &Store<Gateway>,
    kind: ReadKind,
    object: &K,
) -> Vec<ObjectRef<Gateway>> {
    let meta = object.meta();
    let namespace = meta.namespace.as_deref().unwrap_or_default();
    let name = meta.name.as_deref().unwrap_or_default();
    let keys: Vec<GatewayKey> = gateways
        .state()
        .iter()
        .map(|gw| GatewayKey::new(gw.namespace().unwrap_or_default(), gw.name_any()))
        .collect();
    reads
        .readers(kind, namespace, name, &keys)
        .into_iter()
        .map(|key| ObjectRef::new(&key.name).within(&key.namespace))
        .collect()
}

/// [`readers`], and the Service's owner: a pass deletes the Services it
/// owns, and reads no other Service of its namespace to find them.
fn service_readers(
    reads: &ReadIndex,
    gateways: &Store<Gateway>,
    svc: &Service,
) -> Vec<ObjectRef<Gateway>> {
    let mut woken = readers(reads, gateways, ReadKind::Service, svc);
    let owner = provision::controller_gateway(&svc.metadata)
        .filter(|gw| gateways.get(gw).is_some() && !woken.contains(gw));
    woken.extend(owner);
    woken
}

/// The Gateways of `store` a BackendTLSPolicy's status says `controller`
/// wrote an ancestor entry for.
fn reported_ancestors(
    policy: &BackendTlsPolicy,
    store: &Store<Gateway>,
    controller: &str,
) -> Vec<ObjectRef<Gateway>> {
    policy
        .status
        .iter()
        .flat_map(|s| s.ancestors.iter())
        .filter(|a| a.controller_name == controller)
        .filter(|a| a.ancestor_ref.kind.as_deref().unwrap_or("Gateway") == "Gateway")
        .filter_map(|a| {
            let namespace = a
                .ancestor_ref
                .namespace
                .clone()
                .or_else(|| policy.namespace())?;
            let gateway = ObjectRef::new(&a.ancestor_ref.name).within(&namespace);
            store.get(&gateway).map(|_| gateway)
        })
        .collect()
}

/// The retry of a failed pass: the cadence v0.1.x error_policy used, slow
/// enough not to press a failing apiserver, well inside the safety net.
const ERROR_REQUEUE_SECS: u64 = 30;

fn error_policy(_obj: Arc<Gateway>, _error: &ReconcileError, _ctx: Arc<ControllerCtx>) -> Action {
    warn!(
        "Gateway reconciliation error, requeueing in {}s",
        ERROR_REQUEUE_SECS
    );
    Action::requeue(Duration::from_secs(ERROR_REQUEUE_SECS))
}

#[cfg(test)]
mod tests {
    use super::testkit::{self, apply, Pipeline};
    use super::*;
    use serde_json::json;

    fn woken<K: kube::Resource>(
        p: &Pipeline,
        kind: ReadKind,
        object: &K,
    ) -> Vec<ObjectRef<Gateway>> {
        readers(&p.ctx.reads, &p.ctx.cache.gateways, kind, object)
    }

    fn gw_ref() -> Vec<ObjectRef<Gateway>> {
        vec![ObjectRef::new("gw").within("default")]
    }

    /// A Pipeline holding Gateway `gw` and `route`, after one pass.
    async fn passed(gw: Gateway, route: HttpRoute, setup: impl FnOnce(&mut Pipeline)) -> Pipeline {
        let mut p = Pipeline::new();
        apply(&mut p.cluster.gateways, gw.clone());
        apply(&mut p.cluster.http_routes, route);
        setup(&mut p);
        p.pass(&gw).await.unwrap();
        p
    }

    /// The Services a route names wake its Gateway, found or not; any
    /// other Service does not.
    #[tokio::test]
    async fn a_pass_is_woken_by_the_services_it_routes_to() {
        let route = testkit::http_route("r", &["gw"], 1);
        let mut route = testkit::with_backend(route, "web", 80);
        let rule = &mut route.spec.rules.as_mut().unwrap()[0];
        rule.backend_refs
            .as_mut()
            .unwrap()
            .push(serde_json::from_value(json!({"name": "later", "port": 80})).unwrap());
        let web = testkit::service("web", "http", 80, json!(8080));
        let other = testkit::service("other", "http", 80, json!(8080));
        let mut p = Pipeline::new();
        apply(&mut p.cluster.gateways, testkit::gateway("gw", 1));
        assert_eq!(
            woken(&p, ReadKind::Service, &other),
            gw_ref(),
            "no pass yet"
        );

        let p = passed(testkit::gateway("gw", 1), route, |p| {
            apply(&mut p.cluster.services, web.clone());
            apply(&mut p.cluster.services, other.clone());
        })
        .await;
        let later = testkit::service("later", "http", 80, json!(8080));
        assert_eq!(woken(&p, ReadKind::Service, &web), gw_ref());
        assert_eq!(woken(&p, ReadKind::Service, &later), gw_ref(), "created");
        assert!(woken(&p, ReadKind::Service, &other).is_empty());
    }

    /// A Service the Gateway owns wakes it, read or not: its pass deletes
    /// the ones it no longer wants.
    #[tokio::test]
    async fn an_owned_service_wakes_its_owner() {
        let gw = testkit::gateway("gw", 1);
        let p = passed(gw.clone(), testkit::http_route("r", &["gw"], 1), |_| ()).await;
        let mut owned = testkit::service("gw-portail-old", "http", 80, json!(8080));
        owned.metadata.owner_references = Some(vec![serde_json::from_value(json!({
            "apiVersion": "gateway.networking.k8s.io/v1", "kind": "Gateway",
            "name": "gw", "uid": "gw-uid", "controller": true,
        }))
        .unwrap()]);
        assert_eq!(
            service_readers(&p.ctx.reads, &p.ctx.cache.gateways, &owned),
            gw_ref()
        );
        let foreign = testkit::service("gw-portail-old", "http", 80, json!(8080));
        assert!(service_readers(&p.ctx.reads, &p.ctx.cache.gateways, &foreign).is_empty());
    }

    /// A standby's pass reads nothing, so no Service wakes it: safe only
    /// because gaining the lease re-runs every Gateway, and that pass reads.
    #[tokio::test]
    async fn a_standby_pass_reads_nothing_until_the_lease_is_gained() {
        use futures::FutureExt;
        let leader = leader::Leader::standby();
        let mut p = Pipeline::led_by(leader.clone());
        let gw = testkit::gateway("gw", 1);
        let web = testkit::service("web", "http", 80, json!(8080));
        apply(&mut p.cluster.gateways, gw.clone());
        apply(&mut p.cluster.services, web.clone());
        let route = testkit::with_backend(testkit::http_route("r", &["gw"], 1), "web", 80);
        apply(&mut p.cluster.http_routes, route);
        p.pass(&gw).await.unwrap();
        assert!(woken(&p, ReadKind::Service, &web).is_empty());

        let mut gained = leader.gained();
        assert_eq!(gained.next().now_or_never(), None);
        leader.lead();
        assert_eq!(
            gained.next().now_or_never(),
            Some(Some(())),
            "the wake that re-runs every Gateway"
        );
        p.pass(&gw).await.unwrap();
        assert_eq!(woken(&p, ReadKind::Service, &web), gw_ref());
    }

    /// A cross-namespace backend reads the grants of its namespace alone.
    #[tokio::test]
    async fn a_pass_is_woken_by_the_grants_of_namespaces_it_reaches() {
        let mut route = testkit::http_route("r", &["gw"], 1);
        route.spec.rules = Some(vec![serde_json::from_value(json!({
            "backendRefs": [{"name": "db", "namespace": "data", "port": 5432}],
        }))
        .unwrap()]);
        let p = passed(testkit::gateway("gw", 1), route, |_| ()).await;
        let grant = |ns: &str| {
            let mut grant = ReferenceGrant::new("g", Default::default());
            grant.metadata.namespace = Some(ns.into());
            grant
        };
        assert_eq!(
            woken(&p, ReadKind::ReferenceGrant, &grant("data")),
            gw_ref()
        );
        assert!(woken(&p, ReadKind::ReferenceGrant, &grant("elsewhere")).is_empty());
    }

    /// The Gateway's own parameters wake it; another's do not.
    #[tokio::test]
    async fn a_pass_is_woken_by_its_own_parameters() {
        let p = passed(
            testkit::gateway("gw", 1),
            testkit::http_route("r", &["gw"], 1),
            |_| (),
        )
        .await;
        let params = |name: &str| {
            let mut params = GatewayParameters::new(name, Default::default());
            params.metadata.namespace = Some("default".into());
            params
        };
        assert_eq!(
            woken(&p, ReadKind::GatewayParameters, &params("unmanaged")),
            gw_ref()
        );
        assert!(woken(&p, ReadKind::GatewayParameters, &params("another")).is_empty());
    }

    /// A Selector listener reads the labels of the namespaces its routes
    /// live in, and of no other.
    #[tokio::test]
    async fn a_pass_is_woken_by_the_namespaces_its_selector_looked_up() {
        let mut gw = testkit::gateway("gw", 1);
        gw.spec.listeners[0].allowed_routes = Some(
            serde_json::from_value(json!({
                "namespaces": {"from": "Selector", "selector": {"matchLabels": {"team": "a"}}},
            }))
            .unwrap(),
        );
        let mut route = testkit::http_route("r", &["gw"], 1);
        route.metadata.namespace = Some("apps".into());
        route.spec.parent_refs.as_mut().unwrap()[0].namespace = Some("default".into());
        let p = passed(gw, route, |_| ()).await;
        let namespace = |name: &str| {
            let mut ns = Namespace::default();
            ns.metadata.name = Some(name.into());
            ns
        };
        assert_eq!(woken(&p, ReadKind::Namespace, &namespace("apps")), gw_ref());
        assert!(woken(&p, ReadKind::Namespace, &namespace("other")).is_empty());
    }

    #[tokio::test]
    async fn a_report_trigger_reconciles_its_gateway() {
        let p = Pipeline::new();
        let trigger = reports::Trigger {
            gateway: GatewayKey::new("apps", "web"),
            dns_moved: false,
        };
        assert_eq!(
            report_trigger(&p.ctx, trigger),
            ObjectRef::<Gateway>::new("web").within("apps")
        );
    }

    /// What a pass hands on: the snapshot it serves and what it wrote.
    fn output(p: &Pipeline, gw: &Gateway) -> serde_json::Value {
        fn untimed(v: &mut serde_json::Value) {
            match v {
                serde_json::Value::Object(map) => {
                    map.remove("lastTransitionTime");
                    map.values_mut().for_each(untimed);
                }
                serde_json::Value::Array(items) => items.iter_mut().for_each(untimed),
                _ => {}
            }
        }
        let writes: Vec<_> = p
            .api
            .requests()
            .into_iter()
            .filter(|r| r.method != "GET")
            .map(|r| json!([r.method, r.path, r.body]))
            .collect();
        let mut out = json!({
            "served": p.served(gw).map(|s| serde_json::to_value(&*s).unwrap()),
            "writes": writes,
        });
        untimed(&mut out);
        out
    }

    fn named<K>(ns: Option<&str>, name: &str, body: serde_json::Value) -> K
    where
        K: kube::Resource<DynamicType = ()> + serde::de::DeserializeOwned,
    {
        let mut body = body;
        body.as_object_mut()
            .unwrap()
            .entry("metadata")
            .or_insert_with(|| json!({}));
        let mut object: K = serde_json::from_value(body).unwrap();
        object.meta_mut().name = Some(name.into());
        object.meta_mut().namespace = ns.map(String::from);
        object.meta_mut().resource_version = Some("1".into());
        object
    }

    fn namespace(name: &str, labels: serde_json::Value) -> Namespace {
        named(None, name, json!({"metadata": {"labels": labels}}))
    }

    fn svc(ns: &str, name: &str, spec: serde_json::Value) -> Service {
        named(Some(ns), name, json!({"spec": spec}))
    }

    fn grant(ns: &str, name: &str) -> ReferenceGrant {
        named(
            Some(ns),
            name,
            json!({"spec": {
                "from": [{"group": "gateway.networking.k8s.io", "kind": "HTTPRoute", "namespace": "default"}],
                "to": [{"group": "", "kind": "Service"}],
            }}),
        )
    }

    fn params(name: &str, labels: serde_json::Value) -> GatewayParameters {
        let mut params: GatewayParameters = named(
            Some("default"),
            name,
            json!({"spec": {"provisioning": "Unmanaged", "externalNameBackends": "Allowed"}}),
        );
        params.metadata.labels = serde_json::from_value(labels).unwrap();
        params
    }

    /// A Gateway whose routes read each recorded kind: a Selector listener,
    /// a route in another namespace, a cross-namespace backend, Services of
    /// each type, and objects of each kind it reads none of.
    fn read_everywhere(p: &mut Pipeline) -> Gateway {
        let mut gw = testkit::gateway("gw", 1);
        gw.spec.listeners[0].allowed_routes = Some(
            serde_json::from_value(json!({
                "namespaces": {"from": "Selector", "selector": {"matchLabels": {"team": "a"}}},
            }))
            .unwrap(),
        );
        apply(&mut p.cluster.gateways, gw.clone());
        apply(
            &mut p.cluster.namespaces,
            namespace("default", json!({"team": "a"})),
        );
        apply(&mut p.cluster.namespaces, namespace("apps", json!({})));
        apply(&mut p.cluster.namespaces, namespace("data", json!({})));
        let mut local = testkit::http_route("local", &["gw"], 1);
        local.spec.rules = Some(vec![serde_json::from_value(json!({"backendRefs": [
            {"name": "web", "port": 80},
            {"name": "hl", "port": 81},
            {"name": "ext", "port": 443},
            {"name": "db", "namespace": "data", "port": 5432},
        ]}))
        .unwrap()]);
        apply(&mut p.cluster.http_routes, local);
        let mut away = testkit::with_backend(testkit::http_route("away", &["gw"], 1), "web2", 80);
        away.metadata.namespace = Some("apps".into());
        away.spec.parent_refs.as_mut().unwrap()[0].namespace = Some("default".into());
        apply(&mut p.cluster.http_routes, away);
        for service in [
            svc(
                "default",
                "web",
                json!({"clusterIP": "10.96.0.1", "ports": [{"port": 80}]}),
            ),
            svc(
                "default",
                "hl",
                json!({"clusterIP": "None", "ports": [{"port": 81, "targetPort": 8081}]}),
            ),
            svc(
                "default",
                "ext",
                json!({"type": "ExternalName", "externalName": "x.example", "ports": [{"port": 443}]}),
            ),
            svc(
                "default",
                "noise",
                json!({"clusterIP": "10.96.0.9", "ports": [{"port": 80}]}),
            ),
            svc(
                "apps",
                "web2",
                json!({"clusterIP": "10.96.0.2", "ports": [{"port": 80}]}),
            ),
        ] {
            apply(&mut p.cluster.services, service);
        }
        apply(&mut p.cluster.reference_grants, grant("elsewhere", "g"));
        apply(&mut p.cluster.parameters, params("unmanaged", json!({})));
        apply(&mut p.cluster.parameters, params("another", json!({})));
        gw
    }

    /// The index is complete: a change it wakes a Gateway for none leaves
    /// that Gateway's next pass exactly as it was. And each object a pass
    /// read does wake it.
    #[tokio::test]
    async fn a_change_that_wakes_nothing_changes_no_pass() {
        use kube::runtime::watcher::Event;
        type Change = fn(&mut Pipeline, &ObjectRef<Gateway>) -> bool;
        fn put<K>(p: &Pipeline, gw: &ObjectRef<Gateway>, object: &K, kind: ReadKind) -> bool
        where
            K: kube::Resource,
        {
            woken(p, kind, object).contains(gw)
        }
        let changes: [(&str, bool, Change); 17] = [
            ("a routed Service changes", true, |p, gw| {
                let s = svc(
                    "default",
                    "web",
                    json!({"clusterIP": "10.96.0.1", "ports": [{"port": 80, "appProtocol": "kubernetes.io/h2c"}]}),
                );
                let woken = service_readers(&p.ctx.reads, &p.ctx.cache.gateways, &s).contains(gw);
                apply(&mut p.cluster.services, s);
                woken
            }),
            ("a headless backend's targetPort changes", true, |p, gw| {
                let s = svc(
                    "default",
                    "hl",
                    json!({"clusterIP": "None", "ports": [{"port": 81, "targetPort": 9091}]}),
                );
                let woken = service_readers(&p.ctx.reads, &p.ctx.cache.gateways, &s).contains(gw);
                apply(&mut p.cluster.services, s);
                woken
            }),
            ("an ExternalName backend's target changes", true, |p, gw| {
                let s = svc(
                    "default",
                    "ext",
                    json!({"type": "ExternalName", "externalName": "y.example", "ports": [{"port": 443}]}),
                );
                let woken = service_readers(&p.ctx.reads, &p.ctx.cache.gateways, &s).contains(gw);
                apply(&mut p.cluster.services, s);
                woken
            }),
            ("a routed Service is deleted", true, |p, gw| {
                let s = svc(
                    "default",
                    "web",
                    json!({"clusterIP": "10.96.0.1", "ports": [{"port": 80}]}),
                );
                let woken = service_readers(&p.ctx.reads, &p.ctx.cache.gateways, &s).contains(gw);
                p.cluster.services.apply_watcher_event(&Event::Delete(s));
                woken
            }),
            ("a refused backend's Service is created", false, |p, gw| {
                let s = svc(
                    "data",
                    "db",
                    json!({"clusterIP": "10.96.0.3", "ports": [{"port": 5432}]}),
                );
                let woken = service_readers(&p.ctx.reads, &p.ctx.cache.gateways, &s).contains(gw);
                apply(&mut p.cluster.services, s);
                woken
            }),
            ("an unrouted Service changes", false, |p, gw| {
                let s = svc(
                    "default",
                    "noise",
                    json!({"clusterIP": "10.96.0.9", "ports": [{"port": 8080}]}),
                );
                let woken = service_readers(&p.ctx.reads, &p.ctx.cache.gateways, &s).contains(gw);
                apply(&mut p.cluster.services, s);
                woken
            }),
            ("an unrouted Service is created", false, |p, gw| {
                let s = svc(
                    "default",
                    "late",
                    json!({"clusterIP": "10.96.0.8", "ports": [{"port": 80}]}),
                );
                let woken = service_readers(&p.ctx.reads, &p.ctx.cache.gateways, &s).contains(gw);
                apply(&mut p.cluster.services, s);
                woken
            }),
            (
                "a route's namespace gets the selected label",
                true,
                |p, gw| {
                    let ns = namespace("apps", json!({"team": "a"}));
                    let woken = put(p, gw, &ns, ReadKind::Namespace);
                    apply(&mut p.cluster.namespaces, ns);
                    woken
                },
            ),
            ("the Gateway's namespace loses it", true, |p, gw| {
                let ns = namespace("default", json!({}));
                let woken = put(p, gw, &ns, ReadKind::Namespace);
                apply(&mut p.cluster.namespaces, ns);
                woken
            }),
            ("a backend's namespace is labelled", false, |p, gw| {
                let ns = namespace("data", json!({"team": "a"}));
                let woken = put(p, gw, &ns, ReadKind::Namespace);
                apply(&mut p.cluster.namespaces, ns);
                woken
            }),
            (
                "a namespace no route lives in is created",
                false,
                |p, gw| {
                    let ns = namespace("fresh", json!({"team": "a"}));
                    let woken = put(p, gw, &ns, ReadKind::Namespace);
                    apply(&mut p.cluster.namespaces, ns);
                    woken
                },
            ),
            ("a grant lets a backend in", true, |p, gw| {
                let g = grant("data", "g");
                let woken = put(p, gw, &g, ReadKind::ReferenceGrant);
                apply(&mut p.cluster.reference_grants, g);
                woken
            }),
            ("a grant in a namespace no ref reaches", false, |p, gw| {
                let mut g = grant("elsewhere", "g");
                g.metadata.labels = Some([("v".to_string(), "2".to_string())].into());
                let woken = put(p, gw, &g, ReadKind::ReferenceGrant);
                apply(&mut p.cluster.reference_grants, g);
                woken
            }),
            ("a grant in the Gateway's own namespace", false, |p, gw| {
                let g = grant("default", "g");
                let woken = put(p, gw, &g, ReadKind::ReferenceGrant);
                apply(&mut p.cluster.reference_grants, g);
                woken
            }),
            ("the Gateway's parameters change", true, |p, gw| {
                let params = params("unmanaged", json!({"v": "2"}));
                let woken = put(p, gw, &params, ReadKind::GatewayParameters);
                apply(&mut p.cluster.parameters, params);
                woken
            }),
            ("other parameters change", false, |p, gw| {
                let params = params("another", json!({"v": "2"}));
                let woken = put(p, gw, &params, ReadKind::GatewayParameters);
                apply(&mut p.cluster.parameters, params);
                woken
            }),
            ("other parameters are created", false, |p, gw| {
                let params = params("fresh", json!({}));
                let woken = put(p, gw, &params, ReadKind::GatewayParameters);
                apply(&mut p.cluster.parameters, params);
                woken
            }),
        ];
        let mut changed = 0;
        for (what, expected, change) in changes {
            let mut p = Pipeline::new();
            let gw = read_everywhere(&mut p);
            let gw_ref = ObjectRef::from(&gw);
            p.pass(&gw).await.unwrap();
            p.api.clear();
            p.pass(&gw).await.unwrap();
            let before = output(&p, &gw);
            p.api.clear();
            let woken = change(&mut p, &gw_ref);
            p.pass(&gw).await.unwrap();
            let after = output(&p, &gw);
            assert_eq!(woken, expected, "{what}: woken");
            if !woken {
                assert_eq!(before, after, "{what}: not woken, yet the pass changed");
            }
            changed += usize::from(before != after);
        }
        assert_eq!(changed, 7, "the changes that reach the output");
    }

    /// A data plane's DNS refresh saw backend pods move: the next pass reads
    /// the named targetPorts its Gateway's headless backends use again.
    #[tokio::test]
    async fn a_moved_dns_epoch_reads_named_target_ports_again() {
        let mut p = Pipeline::new();
        let gw = testkit::gateway("gw", 1);
        apply(&mut p.cluster.gateways, gw.clone());
        apply(
            &mut p.cluster.services,
            testkit::service("db", "web", 80, serde_json::json!("http")),
        );
        apply(
            &mut p.cluster.http_routes,
            testkit::with_backend(testkit::http_route("r", &["gw"], 1), "db", 80),
        );
        p.api.reply_to(
            "/apis/discovery.k8s.io/v1/namespaces/default/endpointslices",
            200,
            serde_json::json!({
                "apiVersion": "discovery.k8s.io/v1", "kind": "EndpointSliceList",
                "metadata": {},
                "items": [{"metadata": {"name": "db-1"}, "addressType": "IPv4",
                           "endpoints": [], "ports": [{"name": "web", "port": 8080}]}],
            }),
        );
        let slice_lists = |p: &Pipeline| {
            p.api
                .requests()
                .iter()
                .filter(|r| r.method == "GET" && r.path.contains("/endpointslices"))
                .count()
        };
        let trigger = |dns_moved| reports::Trigger {
            gateway: testkit::key(&gw),
            dns_moved,
        };
        p.pass(&gw).await.unwrap();
        assert_eq!(slice_lists(&p), 1);

        report_trigger(&p.ctx, trigger(false));
        p.api.clear();
        p.pass(&gw).await.unwrap();
        assert_eq!(slice_lists(&p), 0, "same pods, read from the cache");

        report_trigger(&p.ctx, trigger(true));
        p.api.clear();
        p.pass(&gw).await.unwrap();
        assert_eq!(slice_lists(&p), 1, "the pods moved");
    }

    /// Shutting down, the sync server stopped first and a pass is stuck: a
    /// lost lease still ends the wait, so the pass cannot write after a
    /// standby took over.
    #[tokio::test(start_paused = true)]
    async fn a_lost_lease_ends_a_graceful_stop() {
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let stopping = until_stopped(
            futures::future::pending(),
            futures::future::ready(Ok(Ok(()))),
            tokio::time::sleep(Duration::from_secs(10)),
            &shutdown,
        );
        let stopped = tokio::time::timeout(Duration::from_secs(60), stopping)
            .await
            .expect("the lost lease ended the drain");
        assert!(stopped
            .unwrap_err()
            .to_string()
            .contains("leader election lost"));
    }

    #[tokio::test]
    async fn a_finished_drain_stops_cleanly() {
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        until_stopped(
            futures::future::ready(()),
            futures::future::pending(),
            futures::future::pending(),
            &shutdown,
        )
        .await
        .unwrap();
        // Not shutting down: a sync server that stops is an error.
        let running = CancellationToken::new();
        assert!(until_stopped(
            futures::future::pending(),
            futures::future::ready(Ok(Ok(()))),
            futures::future::pending(),
            &running,
        )
        .await
        .is_err());
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
}
