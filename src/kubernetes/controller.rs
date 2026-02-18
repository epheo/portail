use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use arc_swap::ArcSwap;
use futures::StreamExt;
use kube::api::Api;
use kube::runtime::controller::Action;
use kube::runtime::watcher;
use kube::runtime::{reflector, Controller, WatchStreamExt, predicates};
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

use super::reconciler::reconcile_to_config;
use super::status;

/// Sync-safe Stream wrapper over an mpsc receiver for reconcile_all_on.
struct ReconcileStream(std::sync::Mutex<tokio::sync::mpsc::Receiver<()>>);

impl futures::Stream for ReconcileStream {
    type Item = ();
    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match self.0.lock() {
            Ok(mut rx) => rx.poll_recv(cx),
            Err(_) => std::task::Poll::Ready(None),
        }
    }
}

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
    store_gateway_classes: Store<GatewayClass>,
}

/// Reconcile all GatewayClasses from the store: accept ours, reject others.
/// Uses reflector store — zero API server calls.
async fn reconcile_gateway_classes(ctx: &ControllerCtx) {
    let mut accepted_name: Option<String> = None;

    // First pass: find the first GatewayClass that matches our controller
    for gc_arc in ctx.store_gateway_classes.state() {
        if gc_arc.spec.controller_name == ctx.controller_name {
            accepted_name = Some(gc_arc.name_any());
            break;
        }
    }

    let Some(accepted) = &accepted_name else {
        return; // No GatewayClass for our controller
    };

    // Second pass: accept ours, reject duplicates
    for gc_arc in ctx.store_gateway_classes.state() {
        if gc_arc.spec.controller_name != ctx.controller_name {
            continue;
        }
        if gc_arc.name_any() == *accepted {
            status::update_gateway_class_status(&ctx.client, &gc_arc, true, "Accepted by portail").await;
        } else {
            status::update_gateway_class_status(
                &ctx.client,
                &gc_arc,
                false,
                &format!("Another GatewayClass '{}' is already accepted by this controller", accepted),
            ).await;
        }
    }
}

/// Helper: create a reflector store for a resource type, spawn a background task
/// to drive it, and optionally send a reconcile trigger.
/// When `generation_filter` is true, only triggers reconciliation on spec changes
/// (metadata.generation change). Set to true for CRDs, false for core resources
/// like Secrets/Services which don't reliably set generation.
fn spawn_reflector<K>(
    api: Api<K>,
    reconcile_tx: Option<tokio::sync::mpsc::Sender<()>>,
    generation_filter: bool,
) -> Store<K>
where
    K: kube::Resource + Clone + std::fmt::Debug + serde::de::DeserializeOwned + Send + Sync + 'static,
    K::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
    let writer = reflector::store::Writer::default();
    let reader = writer.as_reader();
    let rf = reflector::reflector(writer, watcher::watcher(api, watcher::Config::default()));
    tokio::spawn(async move {
        let mut generations: HashMap<String, i64> = HashMap::new();
        // Process the raw reflector stream (not applied_objects()) to receive
        // both Apply and Delete events. Without handling deletes, delete+recreate
        // of a resource with the same name retains the old generation in the
        // HashMap and suppresses the reconcile trigger.
        let mut stream = std::pin::pin!(rf);
        while let Some(result) = stream.next().await {
            let event = match result {
                Ok(e) => e,
                Err(_) => continue,
            };
            if let Some(tx) = &reconcile_tx {
                match &event {
                    watcher::Event::Delete(obj) => {
                        // Remove stale generation entry so the next create triggers correctly
                        let key = format!("{}/{}",
                            obj.meta().namespace.as_deref().unwrap_or(""),
                            obj.meta().name.as_deref().unwrap_or(""));
                        generations.remove(&key);
                        let _ = tx.try_send(());
                    }
                    watcher::Event::Apply(obj) | watcher::Event::InitApply(obj) => {
                        if generation_filter {
                            let key = format!("{}/{}",
                                obj.meta().namespace.as_deref().unwrap_or(""),
                                obj.meta().name.as_deref().unwrap_or(""));
                            let gen = obj.meta().generation.unwrap_or(0);
                            let prev = generations.insert(key, gen);
                            if prev.is_none() || prev != Some(gen) {
                                let _ = tx.try_send(());
                            }
                        } else {
                            let _ = tx.try_send(());
                        }
                    }
                    _ => {} // Init, InitDone — no action needed
                }
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

    // Channel for secondary resources to trigger reconciliation
    let (reconcile_tx, reconcile_rx) = tokio::sync::mpsc::channel::<()>(16);

    // --- Primary resource: Gateway ---
    // Use Controller::for_stream() with predicate_filter(predicates::generation)
    // to filter out status-only changes at the stream level.
    let gw_writer = reflector::store::Writer::default();
    let store_gateways = gw_writer.as_reader();
    let gw_stream = reflector::reflector(gw_writer,
        watcher::watcher(Api::<Gateway>::all(client.clone()), watcher::Config::default()))
        .default_backoff()
        .applied_objects()
        .predicate_filter(predicates::generation);

    // --- Secondary CRD reflectors (generation-filtered via spawn_reflector) ---
    let store_http_routes = spawn_reflector(Api::<HTTPRoute>::all(client.clone()), Some(reconcile_tx.clone()), true);
    let store_tcp_routes = spawn_reflector(Api::<TCPRoute>::all(client.clone()), Some(reconcile_tx.clone()), true);
    let store_tls_routes = spawn_reflector(Api::<TLSRoute>::all(client.clone()), Some(reconcile_tx.clone()), true);
    let store_udp_routes = spawn_reflector(Api::<UDPRoute>::all(client.clone()), Some(reconcile_tx.clone()), true);
    let store_gateway_classes = spawn_reflector(Api::<GatewayClass>::all(client.clone()), Some(reconcile_tx.clone()), true);

    // --- Core resource reflectors (no generation filter) ---
    let store_namespaces = spawn_reflector(Api::<Namespace>::all(client.clone()), Some(reconcile_tx.clone()), false);
    let store_services = spawn_reflector(Api::<Service>::all(client.clone()), Some(reconcile_tx.clone()), false);
    let store_endpoint_slices = spawn_reflector(Api::<EndpointSlice>::all(client.clone()), Some(reconcile_tx.clone()), false);
    let store_reference_grants = spawn_reflector(Api::<ReferenceGrant>::all(client.clone()), Some(reconcile_tx.clone()), false);

    // Only watch TLS secrets
    let store_secrets = {
        let writer = reflector::store::Writer::default();
        let reader = writer.as_reader();
        let rf = reflector::reflector(writer,
            watcher::watcher(Api::<Secret>::all(client.clone()),
                watcher::Config::default().fields("type=kubernetes.io/tls")));
        let tx = reconcile_tx.clone();
        tokio::spawn(async move {
            let mut stream = std::pin::pin!(rf.applied_objects());
            while stream.next().await.is_some() {
                let _ = tx.try_send(());
            }
        });
        reader
    };

    // Clone the gateway store for both ControllerCtx and Controller::for_stream()
    let store_gateways_for_controller = store_gateways.clone();

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
        store_gateway_classes,
    });

    // Wrap the secondary resource channel as a Stream for reconcile_all_on
    let reconcile_stream = ReconcileStream(std::sync::Mutex::new(reconcile_rx));

    // Controller::for_stream() reuses our reflector stream (no duplicate watcher).
    // predicate_filter(predicates::generation) on the stream filters status-only
    // changes before they reach the Controller — breaking the feedback loop.
    // Built-in 1s debounce coalesces bursts of events.
    let controller = Controller::for_stream(gw_stream, store_gateways_for_controller)
        .reconcile_all_on(reconcile_stream)
        .with_config(
            kube::runtime::controller::Config::default()
                .debounce(Duration::from_secs(1))
        )
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
    // Reconcile GatewayClasses on each pass (cheap — reads from store)
    reconcile_gateway_classes(&ctx).await;

    let gw_name = gateway.name_any();
    let gw_ns = gateway.namespace().unwrap_or_else(|| "default".to_string());
    debug!("Reconciling Gateway {}/{}", gw_ns, gw_name);

    // Verify this Gateway's class is managed by us — read from store, no API call
    let gateway_class_name = &gateway.spec.gateway_class_name;
    let gc_ref = ObjectRef::<GatewayClass>::new(gateway_class_name);
    let gc = match ctx.store_gateway_classes.get(&gc_ref) {
        Some(gc) => gc,
        None => {
            debug!("GatewayClass '{}' not in store", gateway_class_name);
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
            return Err(ReconcileError::Reconcile(e.to_string()));
        }
    };

    // Dynamically open TCP listeners for ports defined in this Gateway's spec.
    // Done after reconciliation so ListenerConfig has TLS cert data populated.
    // Also updates TLS configs for already-bound ports (cert hot-reload).
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
                // Hot-reload TLS certs for all HTTPS/TLS listeners on every reconcile.
                // Picks up Secret changes from cert-manager, external-secrets, etc.
                let (tls_updated, tls_errors) = dp.update_tls_configs(listener_configs);
                if tls_updated > 0 {
                    debug!("Refreshed TLS config for {} listener(s)", tls_updated);
                }
                for (port, err) in &tls_errors {
                    warn!("TLS config update failed for port {}: {}", port, err);
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
                    let existing = get_existing_route_parents_from_store(&ctx.store_http_routes, ns, name);
                    status::update_route_status::<HTTPRoute>(
                        &ctx.client, name, ns, parents, &field_manager, &existing,
                    ).await;
                }
                "TCPRoute" => {
                    let existing = get_existing_route_parents_from_store(&ctx.store_tcp_routes, ns, name);
                    status::update_route_status::<TCPRoute>(
                        &ctx.client, name, ns, parents, &field_manager, &existing,
                    ).await;
                }
                "TLSRoute" => {
                    let existing = get_existing_route_parents_from_store(&ctx.store_tls_routes, ns, name);
                    status::update_route_status::<TLSRoute>(
                        &ctx.client, name, ns, parents, &field_manager, &existing,
                    ).await;
                }
                "UDPRoute" => {
                    let existing = get_existing_route_parents_from_store(&ctx.store_udp_routes, ns, name);
                    status::update_route_status::<UDPRoute>(
                        &ctx.client, name, ns, parents, &field_manager, &existing,
                    ).await;
                }
                _ => {}
            }
        }
    }

    Ok(Action::await_change())
}

fn error_policy(
    _obj: Arc<Gateway>,
    _error: &ReconcileError,
    _ctx: Arc<ControllerCtx>,
) -> Action {
    warn!("Controller reconciliation error, requeueing in 30s");
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
