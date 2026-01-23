use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use arc_swap::ArcSwap;
use futures::StreamExt;
use kube::api::{Api, ListParams};
use kube::runtime::controller::Action;
use kube::runtime::watcher;
use kube::runtime::{reflector, Controller, WatchStreamExt};
use kube::runtime::reflector::{ObjectRef, Store};
use kube::Client;
use kube::ResourceExt;
use k8s_openapi::api::core::v1::{Namespace, Secret, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use tokio_util::sync::CancellationToken;

use gateway_api::gatewayclasses::GatewayClass;
use gateway_api::gateways::Gateway;
use gateway_api::httproutes::HTTPRoute;
use gateway_api::referencegrants::ReferenceGrant;
use gateway_api::experimental::tcproutes::TCPRoute;
use gateway_api::experimental::tlsroutes::TLSRoute;
use gateway_api::experimental::udproutes::UDPRoute;

use crate::routing::RouteTable;
use crate::logging::{info, warn, error, debug};

/// Sync-safe Stream wrapper over an mpsc receiver.
/// Required because `reconcile_all_on` needs `Send + Sync`.
struct ReconcileStream(std::sync::Mutex<tokio::sync::mpsc::Receiver<()>>);

impl futures::Stream for ReconcileStream {
    type Item = ();
    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match self.0.lock() {
            Ok(mut rx) => {
                match rx.poll_recv(cx) {
                    std::task::Poll::Ready(Some(())) => {
                        // Drain all buffered events so bursts coalesce into one reconcile
                        while rx.try_recv().is_ok() {}
                        std::task::Poll::Ready(Some(()))
                    }
                    other => other,
                }
            }
            Err(_) => std::task::Poll::Ready(None),
        }
    }
}

use super::reconciler::reconcile_to_config;
use super::status;

#[derive(Debug)]
#[allow(dead_code)]
enum ReconcileError {
    Kube(kube::Error),
    Reconcile(String),
}

impl std::fmt::Display for ReconcileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Kube(e) => write!(f, "Kubernetes API error: {}", e),
            Self::Reconcile(msg) => write!(f, "Reconciliation error: {}", msg),
        }
    }
}

impl std::error::Error for ReconcileError {}

impl From<kube::Error> for ReconcileError {
    fn from(e: kube::Error) -> Self {
        Self::Kube(e)
    }
}

#[derive(Clone)]
struct ControllerCtx {
    client: Client,
    routes: Arc<ArcSwap<RouteTable>>,
    controller_name: String,
    data_plane: Arc<std::sync::Mutex<crate::data_plane::DataPlane>>,
    worker_count: usize,
    performance_config: crate::config::PerformanceConfig,
    // Reflector stores — cached local copies of cluster resources
    store_http_routes: Store<HTTPRoute>,
    store_tcp_routes: Store<TCPRoute>,
    store_tls_routes: Store<TLSRoute>,
    store_udp_routes: Store<UDPRoute>,
    store_namespaces: Store<Namespace>,
    store_services: Store<Service>,
    store_endpoint_slices: Store<EndpointSlice>,
    store_reference_grants: Store<ReferenceGrant>,
    store_secrets: Store<Secret>,
    store_gateways: Store<Gateway>,
}

/// Reconcile a GatewayClass: accept ours, reject others with same controllerName
async fn reconcile_gateway_class(
    gc: Arc<GatewayClass>,
    ctx: Arc<ControllerCtx>,
) -> Result<Action, ReconcileError> {
    let gc_name = gc.name_any();
    debug!("Reconciling GatewayClass {}", gc_name);

    if gc.spec.controller_name != ctx.controller_name {
        debug!("GatewayClass {} has controller '{}', not ours", gc_name, gc.spec.controller_name);
        return Ok(Action::await_change());
    }

    // Accept this GatewayClass
    status::update_gateway_class_status(&ctx.client, &gc, true, "Accepted by portail").await;

    // Reject other GatewayClasses with same controllerName
    let gc_api: Api<GatewayClass> = Api::all(ctx.client.clone());
    if let Ok(all_gcs) = gc_api.list(&ListParams::default()).await {
        for other_gc in &all_gcs.items {
            if other_gc.spec.controller_name == ctx.controller_name
                && other_gc.name_any() != gc_name
            {
                status::update_gateway_class_status(
                    &ctx.client,
                    other_gc,
                    false,
                    &format!(
                        "Another GatewayClass '{}' is already accepted by this controller",
                        gc_name
                    ),
                )
                .await;
            }
        }
    }

    Ok(Action::await_change())
}

/// Helper: create a reflector store for a resource type, spawn a background task
/// to drive it, and optionally send a reconcile trigger on each event.
fn spawn_reflector<K>(
    api: Api<K>,
    reconcile_tx: Option<tokio::sync::mpsc::Sender<()>>,
) -> Store<K>
where
    K: kube::Resource + Clone + std::fmt::Debug + serde::de::DeserializeOwned + Send + Sync + 'static,
    K::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
    let writer = reflector::store::Writer::default();
    let reader = writer.as_reader();
    let rf = reflector::reflector(writer, watcher::watcher(api, watcher::Config::default()));
    tokio::spawn(async move {
        let mut stream = std::pin::pin!(rf.applied_objects());
        while stream.next().await.is_some() {
            if let Some(tx) = &reconcile_tx {
                let _ = tx.try_send(());
            }
        }
    });
    reader
}

pub async fn run_controller(
    routes: Arc<ArcSwap<RouteTable>>,
    controller_name: String,
    shutdown: CancellationToken,
    data_plane: Arc<std::sync::Mutex<crate::data_plane::DataPlane>>,
    worker_count: usize,
    performance_config: crate::config::PerformanceConfig,
) -> anyhow::Result<()> {
    let client = Client::try_default().await?;
    info!("Kubernetes client connected, starting Gateway API controller");

    let gateway_classes: Api<GatewayClass> = Api::all(client.clone());
    let gateways: Api<Gateway> = Api::all(client.clone());

    // Channel to trigger full reconciliation when secondary resources change
    let (reconcile_tx, reconcile_rx) = tokio::sync::mpsc::channel::<()>(16);

    // Create reflector stores — cached local copies of all cluster resources.
    // Each reflector maintains a single watch stream to the API server and keeps
    // the store up to date. ALL reflectors trigger reconciliation via the channel
    // so we do NOT use Controller::watches() — this avoids duplicate watch streams.
    let store_http_routes = spawn_reflector(Api::<HTTPRoute>::all(client.clone()), Some(reconcile_tx.clone()));
    let store_tcp_routes = spawn_reflector(Api::<TCPRoute>::all(client.clone()), Some(reconcile_tx.clone()));
    let store_tls_routes = spawn_reflector(Api::<TLSRoute>::all(client.clone()), Some(reconcile_tx.clone()));
    let store_udp_routes = spawn_reflector(Api::<UDPRoute>::all(client.clone()), Some(reconcile_tx.clone()));
    let store_namespaces = spawn_reflector(Api::<Namespace>::all(client.clone()), Some(reconcile_tx.clone()));
    let store_services = spawn_reflector(Api::<Service>::all(client.clone()), Some(reconcile_tx.clone()));
    let store_endpoint_slices = spawn_reflector(Api::<EndpointSlice>::all(client.clone()), Some(reconcile_tx.clone()));
    let store_reference_grants = spawn_reflector(Api::<ReferenceGrant>::all(client.clone()), Some(reconcile_tx.clone()));

    // Only watch TLS secrets — avoids watching all service account tokens etc.
    let secrets_api = Api::<Secret>::all(client.clone());
    let store_secrets = {
        let writer = reflector::store::Writer::default();
        let reader = writer.as_reader();
        let secret_watcher = watcher::watcher(
            secrets_api,
            watcher::Config::default().fields("type=kubernetes.io/tls"),
        );
        let rf = reflector::reflector(writer, secret_watcher);
        let tx = reconcile_tx.clone();
        tokio::spawn(async move {
            let mut stream = std::pin::pin!(rf.applied_objects());
            while stream.next().await.is_some() {
                let _ = tx.try_send(());
            }
        });
        reader
    };

    // Gateway reflector — also triggers reconciliation so we don't need a
    // separate Controller watch for it.
    let store_gateways = spawn_reflector(Api::<Gateway>::all(client.clone()), Some(reconcile_tx.clone()));

    let ctx = Arc::new(ControllerCtx {
        client: client.clone(),
        routes,
        controller_name,
        data_plane,
        worker_count,
        performance_config,
        store_http_routes,
        store_tcp_routes,
        store_tls_routes,
        store_udp_routes,
        store_namespaces,
        store_services,
        store_endpoint_slices,
        store_reference_grants,
        store_secrets,
        store_gateways,
    });

    let reconcile_stream = ReconcileStream(std::sync::Mutex::new(reconcile_rx));

    // Spawn GatewayClass controller so we accept GatewayClasses proactively
    let gc_ctx = ctx.clone();
    let gc_api = gateway_classes.clone();
    tokio::spawn(async move {
        let gc_controller = Controller::new(gc_api, watcher::Config::default())
            .shutdown_on_signal()
            .run(reconcile_gateway_class, error_policy_gc, gc_ctx);
        gc_controller.for_each(|result| async move {
            match result {
                Ok(_) => {}
                Err(e) => warn!("GatewayClass reconciliation error: {}", e),
            }
        }).await;
    });

    let controller = Controller::new(gateways.clone(), watcher::Config::default())
        .reconcile_all_on(reconcile_stream)
        .shutdown_on_signal()
        .run(reconcile, error_policy, ctx);

    info!("Gateway API controller started, watching for resource changes");

    tokio::select! {
        _ = controller.for_each(|result| async move {
            match result {
                Ok((_obj_ref, _action)) => {}
                Err(e) => warn!("Reconciliation error: {}", e),
            }
        }) => {
            info!("Controller stream ended");
        }
        _ = shutdown.cancelled() => {
            info!("Controller received shutdown signal");
        }
    }

    Ok(())
}


async fn reconcile(
    gateway: Arc<Gateway>,
    ctx: Arc<ControllerCtx>,
) -> Result<Action, ReconcileError> {
    let gw_name = gateway.name_any();
    let gw_ns = gateway.namespace().unwrap_or_else(|| "default".to_string());
    debug!("Reconciling Gateway {}/{}", gw_ns, gw_name);

    // Verify this Gateway's class is managed by us
    let gc_api: Api<GatewayClass> = Api::all(ctx.client.clone());
    let gateway_class_name = &gateway.spec.gateway_class_name;
    let gc = match gc_api.get(gateway_class_name).await {
        Ok(gc) => gc,
        Err(e) => {
            debug!("GatewayClass '{}' not found: {}", gateway_class_name, e);
            return Ok(Action::await_change());
        }
    };

    if gc.spec.controller_name != ctx.controller_name {
        debug!(
            "Gateway {}/{} references GatewayClass '{}' with controller '{}', not ours ('{}')",
            gw_ns, gw_name, gateway_class_name, gc.spec.controller_name, ctx.controller_name
        );
        return Ok(Action::await_change());
    }

    status::update_gateway_class_status(&ctx.client, &gc, true, "Accepted by portail").await;

    // Explicitly reject other GatewayClasses with the same controllerName
    if let Ok(all_gcs) = gc_api.list(&ListParams::default()).await {
        for other_gc in &all_gcs.items {
            if other_gc.spec.controller_name == ctx.controller_name
                && other_gc.name_any() != gc.name_any()
            {
                status::update_gateway_class_status(
                    &ctx.client,
                    other_gc,
                    false,
                    &format!(
                        "Another GatewayClass '{}' is already accepted by this controller",
                        gc.name_any()
                    ),
                )
                .await;
            }
        }
    }

    // Read all resources from reflector caches — no API server calls.
    let http_routes: Vec<HTTPRoute> = ctx.store_http_routes.state().iter().map(|arc| (**arc).clone()).collect();
    let tcp_routes: Vec<TCPRoute> = ctx.store_tcp_routes.state().iter().map(|arc| (**arc).clone()).collect();
    let tls_routes: Vec<TLSRoute> = ctx.store_tls_routes.state().iter().map(|arc| (**arc).clone()).collect();
    let udp_routes: Vec<UDPRoute> = ctx.store_udp_routes.state().iter().map(|arc| (**arc).clone()).collect();

    let namespace_labels: HashMap<String, BTreeMap<String, String>> = ctx.store_namespaces.state()
        .iter()
        .filter_map(|ns| {
            let name = ns.metadata.name.clone()?;
            let labels = ns.metadata.labels.clone().unwrap_or_default();
            Some((name, labels))
        })
        .collect();

    let reference_grants: Vec<ReferenceGrant> = ctx.store_reference_grants.state().iter().map(|arc| (**arc).clone()).collect();

    // Fetch TLS certificate data from cached Secrets store
    let mut cert_data: HashMap<(String, String), (Vec<u8>, Vec<u8>)> = HashMap::new();
    for listener in &gateway.spec.listeners {
        if let Some(tls) = &listener.tls {
            if let Some(cert_refs) = &tls.certificate_refs {
                for cert_ref in cert_refs {
                    let secret_ns = cert_ref.namespace.as_deref().unwrap_or(&gw_ns);

                    // Cross-namespace cert ref requires a ReferenceGrant
                    if secret_ns != gw_ns {
                        let grant_allows = reference_grants.iter().any(|grant| {
                            let grant_ns = grant.metadata.namespace.as_deref().unwrap_or("default");
                            if grant_ns != secret_ns {
                                return false;
                            }
                            let from_ok = grant.spec.from.iter().any(|f| {
                                f.group == "gateway.networking.k8s.io"
                                    && f.kind == "Gateway"
                                    && f.namespace == gw_ns
                            });
                            let to_ok = grant.spec.to.iter().any(|t| {
                                t.group.is_empty() && t.kind == "Secret"
                                    && t.name.as_ref().is_none_or(|n| n == &cert_ref.name)
                            });
                            from_ok && to_ok
                        });
                        if !grant_allows {
                            warn!(
                                "Cross-namespace certificate ref {}/{} not allowed by ReferenceGrant",
                                secret_ns, cert_ref.name
                            );
                            continue;
                        }
                    }

                    // Look up secret from reflector cache
                    let secret_ref = ObjectRef::<Secret>::new(&cert_ref.name).within(secret_ns);
                    match ctx.store_secrets.get(&secret_ref) {
                        Some(secret) => {
                            if let Some(data) = &secret.data {
                                let cert_pem = data.get("tls.crt").map(|b| b.0.clone());
                                let key_pem = data.get("tls.key").map(|b| b.0.clone());
                                if let (Some(cert), Some(key)) = (cert_pem, key_pem) {
                                    let cert_str = String::from_utf8_lossy(&cert);
                                    let key_str = String::from_utf8_lossy(&key);
                                    if cert_str.contains("BEGIN CERTIFICATE") &&
                                       (key_str.contains("BEGIN PRIVATE KEY") || key_str.contains("BEGIN RSA PRIVATE KEY") || key_str.contains("BEGIN EC PRIVATE KEY")) {
                                        cert_data.insert((cert_ref.name.clone(), secret_ns.to_string()), (cert, key));
                                    } else {
                                        warn!("Secret {}/{} has malformed PEM data", secret_ns, cert_ref.name);
                                    }
                                } else {
                                    warn!("Secret {}/{} missing tls.crt or tls.key", secret_ns, cert_ref.name);
                                }
                            }
                        }
                        None => {
                            debug!("Secret {}/{} not found in cache", secret_ns, cert_ref.name);
                        }
                    }
                }
            }
        }
    }

    // Read Services from reflector cache
    let all_services: Vec<Service> = ctx.store_services.state().iter().map(|arc| (**arc).clone()).collect();

    let known_services: HashSet<(String, String)> = all_services
        .iter()
        .filter_map(|svc| {
            let name = svc.metadata.name.as_ref()?;
            let ns = svc.metadata.namespace.as_deref().unwrap_or("default");
            Some((name.clone(), ns.to_string()))
        })
        .collect();

    // Detect headless services and fetch their EndpointSlices for pod IP + targetPort resolution.
    // For headless services (clusterIP: None), DNS returns pod IPs but kube-proxy doesn't
    // translate service port → targetPort, so we need EndpointSlice data.
    // Map: (svc_fqdn, service_port) → Vec<(pod_ip, target_port)>
    let mut endpoint_overrides: HashMap<(String, u16), Vec<(String, u16)>> = HashMap::new();

    let headless_services: Vec<(&str, &str)> = all_services
        .iter()
        .filter_map(|svc| {
            let spec = svc.spec.as_ref()?;
            let cluster_ip = spec.cluster_ip.as_deref().unwrap_or("");
            if cluster_ip == "None" || cluster_ip.is_empty() {
                let name = svc.metadata.name.as_deref()?;
                let ns = svc.metadata.namespace.as_deref().unwrap_or("default");
                Some((name, ns))
            } else {
                None
            }
        })
        .collect();

    if !headless_services.is_empty() {
        let all_endpoint_slices: Vec<EndpointSlice> = ctx.store_endpoint_slices.state().iter().map(|arc| (**arc).clone()).collect();

        for (svc_name, svc_ns) in &headless_services {
            // Find EndpointSlices for this service (labeled kubernetes.io/service-name)
            let matching_slices: Vec<&EndpointSlice> = all_endpoint_slices
                .iter()
                .filter(|eps| {
                    let eps_ns = eps.metadata.namespace.as_deref().unwrap_or("default");
                    if eps_ns != *svc_ns {
                        return false;
                    }
                    eps.metadata
                        .labels
                        .as_ref()
                        .and_then(|l| l.get("kubernetes.io/service-name"))
                        .is_some_and(|v| v == *svc_name)
                })
                .collect();

            let svc_fqdn = format!("{}.{}.svc", svc_name, svc_ns);

            // Get the service's declared ports for mapping service_port → targetPort
            let svc_spec = all_services
                .iter()
                .find(|s| {
                    s.metadata.name.as_deref() == Some(svc_name)
                        && s.metadata.namespace.as_deref().unwrap_or("default") == *svc_ns
                })
                .and_then(|s| s.spec.as_ref());

            for eps in &matching_slices {
                // Only use IPv4 slices
                if eps.address_type != "IPv4" {
                    continue;
                }

                let endpoints = &eps.endpoints;

                let eps_ports = eps.ports.as_deref().unwrap_or_default();

                // For each service port, find the corresponding EndpointSlice port
                if let Some(svc_ports) = svc_spec.and_then(|s| s.ports.as_ref()) {
                    for sp in svc_ports {
                        let service_port = sp.port as u16;
                        let port_name = sp.name.as_deref().unwrap_or("");

                        // Find matching endpoint port by name
                        let target_port = eps_ports
                            .iter()
                            .find(|ep| {
                                ep.name.as_deref().unwrap_or("") == port_name
                            })
                            .and_then(|ep| ep.port)
                            .unwrap_or(service_port as i32) as u16;

                        let key = (svc_fqdn.clone(), service_port);
                        let entry = endpoint_overrides.entry(key).or_default();
                        for endpoint in endpoints {
                            // Only include ready endpoints
                            let is_ready = endpoint
                                .conditions
                                .as_ref()
                                .and_then(|c| c.ready)
                                .unwrap_or(true);
                            if is_ready {
                                for addr in &endpoint.addresses {
                                    entry.push((addr.clone(), target_port));
                                }
                            }
                        }
                    }
                }
            }
        }

        if !endpoint_overrides.is_empty() {
            info!(
                "Found {} headless endpoint overrides for {} services",
                endpoint_overrides.values().map(|v| v.len()).sum::<usize>(),
                endpoint_overrides.len(),
            );
        }
    }

    // --- Per-listener validation: compute ListenerStatus for each listener ---
    let mut listener_statuses: HashMap<String, status::ListenerStatus> = HashMap::new();

    // Detect protocol conflicts: same port, different protocols
    let mut port_protocols: HashMap<i32, String> = HashMap::new();
    let mut conflicted_ports: HashSet<i32> = HashSet::new();
    for l in &gateway.spec.listeners {
        match port_protocols.get(&l.port) {
            Some(existing_proto) if *existing_proto != l.protocol => {
                conflicted_ports.insert(l.port);
            }
            None => { port_protocols.insert(l.port, l.protocol.clone()); }
            _ => {}
        }
    }

    for listener in &gateway.spec.listeners {
        let mut ls = status::ListenerStatus::default();
        let mut refs_failed = false;

        // Check protocol conflict
        if conflicted_ports.contains(&listener.port) {
            ls.accepted = false;
            ls.accepted_reason = "ProtocolConflict".into();
            ls.accepted_message = "Listener port conflicts with another listener using a different protocol".into();
            ls.conflicted = true;
            ls.conflicted_reason = "ProtocolConflict".into();
            ls.conflicted_message = "Listener port shared with another listener using different protocol".into();
        } else {
            // No conflict — listener is accepted
            ls.accepted = true;
            ls.accepted_reason = "Accepted".into();
            ls.accepted_message = "Listener accepted".into();
        }

        // Validate TLS certificateRefs for HTTPS/TLS listeners
        if let Some(tls) = &listener.tls {
            if let Some(cert_refs) = &tls.certificate_refs {
                for cert_ref in cert_refs {
                    // Check group/kind — must be core ("") group and "Secret" kind
                    let group = cert_ref.group.as_deref().unwrap_or("");
                    let kind_str = cert_ref.kind.as_deref().unwrap_or("Secret");
                    if !group.is_empty() || kind_str != "Secret" {
                        refs_failed = true;
                        ls.resolved_refs_reason = "InvalidCertificateRef".into();
                        ls.resolved_refs_message = format!(
                            "Unsupported certificateRef group/kind: {}/{}", group, kind_str
                        );
                        continue;
                    }

                    let secret_ns = cert_ref.namespace.as_deref().unwrap_or(&gw_ns);

                    // Cross-namespace cert ref requires ReferenceGrant
                    if secret_ns != gw_ns {
                        let grant_allows = reference_grants.iter().any(|grant| {
                            let grant_ns = grant.metadata.namespace.as_deref().unwrap_or("default");
                            if grant_ns != secret_ns { return false; }
                            let from_ok = grant.spec.from.iter().any(|f| {
                                f.group == "gateway.networking.k8s.io"
                                    && f.kind == "Gateway"
                                    && f.namespace == gw_ns
                            });
                            let to_ok = grant.spec.to.iter().any(|t| {
                                t.group.is_empty() && t.kind == "Secret"
                                    && t.name.as_ref().is_none_or(|n| n == &cert_ref.name)
                            });
                            from_ok && to_ok
                        });
                        if !grant_allows {
                            refs_failed = true;
                            ls.resolved_refs_reason = "RefNotPermitted".into();
                            ls.resolved_refs_message = format!(
                                "Cross-namespace certificate ref {}/{} not allowed by ReferenceGrant",
                                secret_ns, cert_ref.name
                            );
                            continue;
                        }
                    }

                    // Check Secret existence and format
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
            // HTTPS/TLS listener without TLS config
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
                    if group == "gateway.networking.k8s.io" && valid_kinds.contains(&kind_str.as_str()) {
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
                    ls.resolved_refs_message = "One or more route kinds in allowedRoutes are not supported".into();
                }
            } else {
                // No explicit kinds restriction — use protocol-based defaults
                ls.supported_kinds = status::supported_kinds_for_protocol(&listener.protocol);
            }
        } else {
            // No allowedRoutes at all — use protocol-based defaults
            ls.supported_kinds = status::supported_kinds_for_protocol(&listener.protocol);
        }

        // Promote resolved_refs to true if all checks passed
        if !refs_failed {
            ls.resolved_refs = true;
            ls.resolved_refs_reason = "ResolvedRefs".into();
            ls.resolved_refs_message = "All references resolved".into();
        }

        // Programmed depends on both accepted and resolved_refs
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

    // Compute Gateway-level Accepted condition from listener validation
    let supported_protocols = ["HTTP", "HTTPS", "TLS", "TCP", "UDP"];
    let mut gateway_accepted = true;
    let mut gateway_accepted_reason = "Accepted".to_string();
    let mut gateway_accepted_message = "Gateway accepted by portail controller".to_string();

    for listener in &gateway.spec.listeners {
        if !supported_protocols.contains(&listener.protocol.as_str()) {
            gateway_accepted = false;
            gateway_accepted_reason = "InvalidParameters".to_string();
            gateway_accepted_message = format!(
                "Listener '{}' uses unsupported protocol '{}'",
                listener.name, listener.protocol
            );
            break;
        }
    }

    // Validate spec.addresses — reject if any address has an unsupported type
    // Per Gateway API spec, supported types are "IPAddress" (default) and "Hostname"
    if gateway_accepted {
        if let Some(addresses) = &gateway.spec.addresses {
            for addr in addresses {
                let addr_type = addr.r#type.as_deref().unwrap_or("IPAddress");
                if addr_type != "IPAddress" && addr_type != "Hostname" {
                    gateway_accepted = false;
                    gateway_accepted_reason = "UnsupportedAddress".to_string();
                    gateway_accepted_message = format!(
                        "Unsupported address type '{}' in spec.addresses", addr_type
                    );
                    break;
                }
            }
        }
    }

    // If all listeners are in conflict or have unresolved refs, the gateway is not accepted
    if gateway_accepted && !listener_statuses.is_empty() {
        let all_rejected = listener_statuses.values().all(|ls| !ls.accepted);
        if all_rejected {
            gateway_accepted = false;
            gateway_accepted_reason = "InvalidListeners".to_string();
            gateway_accepted_message = "All listeners are rejected due to conflicts or errors".to_string();
        }
    }

    // Build config for the triggered Gateway
    let result = match reconcile_to_config(
        &gateway,
        &http_routes,
        &tcp_routes,
        &tls_routes,
        &udp_routes,
        &namespace_labels,
        &reference_grants,
        &known_services,
        &cert_data,
        &endpoint_overrides,
    ) {
        Ok(r) => r,
        Err(e) => {
            error!("Failed to reconcile Gateway {}/{}: {}", gw_ns, gw_name, e);
            status::update_gateway_status(
                &ctx.client,
                &gateway,
                gateway_accepted,
                &gateway_accepted_reason,
                &gateway_accepted_message,
                false,
                &format!("Reconciliation failed: {}", e),
                &HashMap::new(),
                &listener_statuses,
            )
            .await;
            return Ok(Action::requeue(std::time::Duration::from_secs(30)));
        }
    };

    // Dynamically open TCP listeners for ports defined in this Gateway's spec.
    // Done after reconciliation so ListenerConfig has TLS cert data populated.
    {
        let listener_configs = &result.config.gateway.listeners;
        info!("Ensuring data plane listeners for ports: {:?}",
            listener_configs.iter().map(|l| l.port).collect::<Vec<_>>());
        match ctx.data_plane.lock() {
            Ok(mut dp) => {
                let (opened, errors) = dp.add_tcp_listeners(
                    listener_configs,
                    ctx.worker_count,
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
            Err(e) => {
                warn!("Failed to lock data plane: {}", e);
            }
        }
    }

    // Build a unified route table from ALL managed gateways.
    // Each gateway's routes are properly scoped by its listeners' (port, hostname).
    // Read other gateways from reflector cache — no API server call.
    let mut all_configs = vec![result.config.clone()];
    for other_gw_arc in ctx.store_gateways.state() {
        let other_name = other_gw_arc.metadata.name.as_deref().unwrap_or("");
        let other_ns = other_gw_arc.metadata.namespace.as_deref().unwrap_or("default");
        if other_name == gw_name && other_ns == gw_ns {
            continue; // Skip the gateway we just reconciled
        }
        if other_gw_arc.spec.gateway_class_name != *gateway_class_name {
            continue; // Different controller
        }
        // Reconcile the other gateway into its own scoped config
        if let Ok(other_result) = reconcile_to_config(
            &other_gw_arc,
            &http_routes,
            &tcp_routes,
            &tls_routes,
            &udp_routes,
            &namespace_labels,
            &reference_grants,
            &known_services,
            &cert_data,
            &endpoint_overrides,
        ) {
            all_configs.push(other_result.config);
        }
    }

    // Build a single route table from all configs.
    // Each config's to_route_table() adds routes scoped by listener (port, hostname).

    // Compute per-listener attached route counts from accepted routes (for this gateway only).
    // Use the listener_names field which tracks exactly which listeners each route was accepted by.
    let mut listener_route_counts: HashMap<String, i32> = HashMap::new();
    for listener in &gateway.spec.listeners {
        listener_route_counts.insert(listener.name.clone(), 0);
    }
    for ra in &result.route_status {
        if ra.accepted {
            for listener_name in &ra.listener_names {
                *listener_route_counts.entry(listener_name.clone()).or_insert(0) += 1;
            }
        }
    }

    let route_table = tokio::task::spawn_blocking(move || {
        let mut combined = crate::routing::RouteTable::new();
        for config in &all_configs {
            let rt = config.to_route_table()?;
            // Merge listener scopes from each gateway's route table
            for (port, scopes) in rt.listener_scopes {
                combined.listener_scopes.entry(port).or_default().extend(scopes);
            }
            combined.tcp_routes.extend(rt.tcp_routes);
            combined.udp_routes.extend(rt.udp_routes);
            combined.tls_routes.extend(rt.tls_routes);
            combined.wildcard_tls_routes.extend(rt.wildcard_tls_routes);
        }
        Ok::<_, anyhow::Error>(combined)
    })
    .await
    .map_err(|e| ReconcileError::Reconcile(e.to_string()))?;

    let route_table_ok = match route_table {
        Ok(route_table) => {
            ctx.routes.store(Arc::new(route_table));
            info!(
                "Gateway {}/{} reconciled: {} HTTP, {} TCP, {} TLS, {} UDP routes",
                gw_ns,
                gw_name,
                result.config.http_routes.len(),
                result.config.tcp_routes.len(),
                result.config.tls_routes.len(),
                result.config.udp_routes.len(),
            );

            // Verify data plane has bound all required ports before reporting Programmed
            let required_ports: Vec<u16> = gateway.spec.listeners.iter().map(|l| l.port as u16).collect();
            let dp_ready = match ctx.data_plane.lock() {
                Ok(dp) => dp.is_ready_for_ports(&required_ports),
                Err(_) => false,
            };
            let (programmed, programmed_msg) = if dp_ready {
                (true, "Programmed")
            } else {
                warn!("Data plane not ready: not all listener ports are bound");
                (false, "Data plane not ready: not all listener ports are bound")
            };

            status::update_gateway_status(
                &ctx.client,
                &gateway,
                gateway_accepted,
                &gateway_accepted_reason,
                &gateway_accepted_message,
                programmed,
                programmed_msg,
                &listener_route_counts,
                &listener_statuses,
            )
            .await;
            dp_ready
        }
        Err(e) => {
            error!(
                "Failed to build route table for Gateway {}/{}: {}",
                gw_ns, gw_name, e
            );
            status::update_gateway_status(
                &ctx.client,
                &gateway,
                gateway_accepted,
                &gateway_accepted_reason,
                &gateway_accepted_message,
                false,
                &format!("Route table conversion failed: {}", e),
                &listener_route_counts,
                &listener_statuses,
            )
            .await;
            false
        }
    };

    // Update per-route status — group by (kind, name, namespace) to write all
    // parent statuses in a single patch (prevents SSA from overwriting previous entries
    // when a route references multiple listeners/gateways).
    let programmed = route_table_ok;
    {
        use std::collections::BTreeMap;
        // Use a per-gateway field manager so SSA doesn't overwrite parents from other gateways
        let field_manager = format!("portail-{}-{}", gw_ns, gw_name);
        // Key: (kind, namespace, name) → Vec of parent statuses
        let mut grouped: BTreeMap<(&str, &str, &str), Vec<status::RouteParentStatus>> = BTreeMap::new();
        for ra in &result.route_status {
            let route_programmed = ra.accepted && programmed;
            grouped.entry((ra.kind, &ra.namespace, &ra.name)).or_default().push(
                status::RouteParentStatus {
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
                },
            );
        }

        for ((kind, ns, name), parents) in &grouped {
            match *kind {
                "HTTPRoute" => {
                    status::update_route_status::<HTTPRoute>(
                        &ctx.client, name, ns, parents, &field_manager,
                    ).await;
                }
                "TCPRoute" => {
                    status::update_route_status::<TCPRoute>(
                        &ctx.client, name, ns, parents, &field_manager,
                    ).await;
                }
                "TLSRoute" => {
                    status::update_route_status::<TLSRoute>(
                        &ctx.client, name, ns, parents, &field_manager,
                    ).await;
                }
                "UDPRoute" => {
                    status::update_route_status::<UDPRoute>(
                        &ctx.client, name, ns, parents, &field_manager,
                    ).await;
                }
                _ => {}
            }
        }
    }

    Ok(Action::requeue(std::time::Duration::from_secs(300)))
}

fn error_policy(
    _obj: Arc<Gateway>,
    _error: &ReconcileError,
    _ctx: Arc<ControllerCtx>,
) -> Action {
    warn!("Controller reconciliation error, requeueing");
    Action::requeue(std::time::Duration::from_secs(5))
}

fn error_policy_gc(
    _obj: Arc<GatewayClass>,
    _error: &ReconcileError,
    _ctx: Arc<ControllerCtx>,
) -> Action {
    warn!("GatewayClass reconciliation error, requeueing");
    Action::requeue(std::time::Duration::from_secs(5))
}
