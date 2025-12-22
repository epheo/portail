use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use arc_swap::ArcSwap;
use futures::StreamExt;
use kube::api::{Api, ListParams};
use kube::runtime::controller::Action;
use kube::runtime::watcher;
use kube::runtime::Controller;
use kube::runtime::reflector::ObjectRef;
use kube::Client;
use kube::ResourceExt;
use k8s_openapi::api::core::v1::{Namespace, Secret, Service};
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
            Ok(mut rx) => rx.poll_recv(cx),
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
    let http_routes: Api<HTTPRoute> = Api::all(client.clone());
    let tcp_routes: Api<TCPRoute> = Api::all(client.clone());
    let tls_routes: Api<TLSRoute> = Api::all(client.clone());
    let udp_routes: Api<UDPRoute> = Api::all(client.clone());
    let secrets: Api<Secret> = Api::all(client.clone());
    let services: Api<Service> = Api::all(client.clone());
    let reference_grants: Api<gateway_api::referencegrants::ReferenceGrant> = Api::all(client.clone());

    let ctx = Arc::new(ControllerCtx {
        client: client.clone(),
        routes,
        controller_name,
        data_plane,
        worker_count,
        performance_config,
    });

    // Channel to trigger full reconciliation when Secrets/Services/ReferenceGrants change
    let (reconcile_tx, reconcile_rx) = tokio::sync::mpsc::channel::<()>(16);

    // Spawn background watchers that feed into the reconcile trigger
    for watcher_stream in [
        watcher::watcher(secrets, watcher::Config::default()).map(|_| ()).boxed(),
        watcher::watcher(services, watcher::Config::default()).map(|_| ()).boxed(),
        watcher::watcher(reference_grants, watcher::Config::default()).map(|_| ()).boxed(),
    ] {
        let tx = reconcile_tx.clone();
        tokio::spawn(async move {
            let mut stream = std::pin::pin!(watcher_stream);
            while stream.next().await.is_some() {
                let _ = tx.send(()).await;
            }
        });
    }

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
        .watches(http_routes, watcher::Config::default(), |route| {
            let route_ns = route.metadata.namespace.as_deref().unwrap_or("default");
            map_route_to_gateways(&route.spec.parent_refs, route_ns)
        })
        .watches(tcp_routes, watcher::Config::default(), |route| {
            let route_ns = route.metadata.namespace.as_deref().unwrap_or("default");
            map_route_to_gateways(&route.spec.parent_refs, route_ns)
        })
        .watches(tls_routes, watcher::Config::default(), |route| {
            let route_ns = route.metadata.namespace.as_deref().unwrap_or("default");
            map_route_to_gateways(&route.spec.parent_refs, route_ns)
        })
        .watches(udp_routes, watcher::Config::default(), |route| {
            let route_ns = route.metadata.namespace.as_deref().unwrap_or("default");
            map_route_to_gateways(&route.spec.parent_refs, route_ns)
        })
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

/// Map a route's parentRefs to Gateway ObjectRefs to trigger reconciliation.
fn map_route_to_gateways<T: ParentRefLike>(
    parent_refs: &Option<Vec<T>>,
    route_namespace: &str,
) -> Vec<ObjectRef<Gateway>> {
    parent_refs
        .as_ref()
        .map(|refs| {
            refs.iter()
                .filter(|pr| pr.kind().map_or(true, |k| k == "Gateway"))
                .map(|pr| {
                    let ns = pr.namespace().unwrap_or(route_namespace);
                    ObjectRef::new(pr.name()).within(ns)
                })
                .collect()
        })
        .unwrap_or_default()
}

trait ParentRefLike {
    fn name(&self) -> &str;
    fn namespace(&self) -> Option<&str>;
    fn kind(&self) -> Option<&str>;
}

impl ParentRefLike for gateway_api::httproutes::HTTPRouteParentRefs {
    fn name(&self) -> &str { &self.name }
    fn namespace(&self) -> Option<&str> { self.namespace.as_deref() }
    fn kind(&self) -> Option<&str> { self.kind.as_deref() }
}

impl ParentRefLike for gateway_api::experimental::tcproutes::TCPRouteParentRefs {
    fn name(&self) -> &str { &self.name }
    fn namespace(&self) -> Option<&str> { self.namespace.as_deref() }
    fn kind(&self) -> Option<&str> { self.kind.as_deref() }
}

impl ParentRefLike for gateway_api::experimental::tlsroutes::TLSRouteParentRefs {
    fn name(&self) -> &str { &self.name }
    fn namespace(&self) -> Option<&str> { self.namespace.as_deref() }
    fn kind(&self) -> Option<&str> { self.kind.as_deref() }
}

impl ParentRefLike for gateway_api::experimental::udproutes::UDPRouteParentRefs {
    fn name(&self) -> &str { &self.name }
    fn namespace(&self) -> Option<&str> { self.namespace.as_deref() }
    fn kind(&self) -> Option<&str> { self.kind.as_deref() }
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
    // Fetch all routes across all namespaces
    // Note: dynamic listener creation happens after reconciliation
    // so we can pass full ListenerConfig with TLS data.

    let http_routes_api: Api<HTTPRoute> = Api::all(ctx.client.clone());
    let tcp_routes_api: Api<TCPRoute> = Api::all(ctx.client.clone());
    let tls_routes_api: Api<TLSRoute> = Api::all(ctx.client.clone());
    let udp_routes_api: Api<UDPRoute> = Api::all(ctx.client.clone());

    let http_routes = http_routes_api
        .list(&ListParams::default())
        .await
        .map(|list| list.items)
        .unwrap_or_default();

    let tcp_routes = tcp_routes_api
        .list(&ListParams::default())
        .await
        .map(|list| list.items)
        .unwrap_or_default();

    let tls_routes = tls_routes_api
        .list(&ListParams::default())
        .await
        .map(|list| list.items)
        .unwrap_or_default();

    let udp_routes = udp_routes_api
        .list(&ListParams::default())
        .await
        .map(|list| list.items)
        .unwrap_or_default();

    // Pre-fetch namespace labels for allowedRoutes selector matching
    let ns_api: Api<Namespace> = Api::all(ctx.client.clone());
    let namespace_labels: HashMap<String, BTreeMap<String, String>> = ns_api
        .list(&ListParams::default())
        .await
        .map(|list| {
            list.items
                .into_iter()
                .filter_map(|ns| {
                    let name = ns.metadata.name?;
                    let labels = ns.metadata.labels.unwrap_or_default();
                    Some((name, labels))
                })
                .collect()
        })
        .unwrap_or_default();

    // Fetch ReferenceGrants for cross-namespace authorization (needed for cert refs too)
    let grants_api: Api<ReferenceGrant> = Api::all(ctx.client.clone());
    let reference_grants = grants_api
        .list(&ListParams::default())
        .await
        .map(|list| list.items)
        .unwrap_or_default();

    // Fetch TLS certificate data from Kubernetes Secrets
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
                                t.group == "" && t.kind == "Secret"
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

                    let secret_api: Api<Secret> = Api::namespaced(ctx.client.clone(), secret_ns);
                    match secret_api.get(&cert_ref.name).await {
                        Ok(secret) => {
                            if let Some(data) = &secret.data {
                                let cert_pem = data.get("tls.crt").map(|b| b.0.clone());
                                let key_pem = data.get("tls.key").map(|b| b.0.clone());
                                if let (Some(cert), Some(key)) = (cert_pem, key_pem) {
                                    // Validate PEM format — must contain valid certificate/key markers
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
                        Err(e) => {
                            debug!("Secret {}/{} not found: {}", secret_ns, cert_ref.name, e);
                        }
                    }
                }
            }
        }
    }

    // Fetch Services for backend existence validation
    let services_api: Api<Service> = Api::all(ctx.client.clone());
    let known_services: HashSet<(String, String)> = services_api
        .list(&ListParams::default())
        .await
        .map(|list| {
            list.items
                .into_iter()
                .filter_map(|svc| {
                    let name = svc.metadata.name?;
                    let ns = svc.metadata.namespace.unwrap_or_else(|| "default".to_string());
                    Some((name, ns))
                })
                .collect()
        })
        .unwrap_or_default();

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

        // Check protocol conflict
        if conflicted_ports.contains(&listener.port) {
            ls.accepted = false;
            ls.accepted_reason = "ProtocolConflict".into();
            ls.accepted_message = "Listener port conflicts with another listener using a different protocol".into();
            ls.programmed = false;
            ls.programmed_reason = "Invalid".into();
            ls.programmed_message = "Listener not programmed due to port conflict".into();
            ls.conflicted = true;
            ls.conflicted_reason = "ProtocolConflict".into();
            ls.conflicted_message = "Listener port shared with another listener using different protocol".into();
        }

        // Validate TLS certificateRefs for HTTPS/TLS listeners
        if let Some(tls) = &listener.tls {
            if let Some(cert_refs) = &tls.certificate_refs {
                for cert_ref in cert_refs {
                    // Check group/kind — must be core ("") group and "Secret" kind
                    let group = cert_ref.group.as_deref().unwrap_or("");
                    let kind_str = cert_ref.kind.as_deref().unwrap_or("Secret");
                    if group != "" || kind_str != "Secret" {
                        ls.resolved_refs = false;
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
                                t.group == "" && t.kind == "Secret"
                                    && t.name.as_ref().is_none_or(|n| n == &cert_ref.name)
                            });
                            from_ok && to_ok
                        });
                        if !grant_allows {
                            ls.resolved_refs = false;
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
                        ls.resolved_refs = false;
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
            ls.resolved_refs = false;
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
                    ls.resolved_refs = false;
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

        listener_statuses.insert(listener.name.clone(), ls);
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
    ) {
        Ok(r) => r,
        Err(e) => {
            error!("Failed to reconcile Gateway {}/{}: {}", gw_ns, gw_name, e);
            status::update_gateway_status(
                &ctx.client,
                &gateway,
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

    // Merge routes from OTHER managed Gateways so we don't clobber them
    let mut merged_config = result.config.clone();
    let gateways_api: Api<Gateway> = Api::all(ctx.client.clone());
    let all_gateways = gateways_api.list(&ListParams::default()).await.map(|l| l.items).unwrap_or_default();
    for other_gw in &all_gateways {
        let other_name = other_gw.metadata.name.as_deref().unwrap_or("");
        let other_ns = other_gw.metadata.namespace.as_deref().unwrap_or("default");
        if other_name == gw_name && other_ns == gw_ns {
            continue; // Skip the gateway we just reconciled
        }
        if other_gw.spec.gateway_class_name != *gateway_class_name {
            continue; // Different controller
        }
        // Reconcile the other gateway and merge its routes
        if let Ok(other_result) = reconcile_to_config(
            other_gw,
            &http_routes,
            &tcp_routes,
            &tls_routes,
            &udp_routes,
            &namespace_labels,
            &reference_grants,
            &known_services,
            &cert_data,
        ) {
            // Merge listeners from other gateways
            merged_config.gateway.listeners.extend(other_result.config.gateway.listeners);
            merged_config.http_routes.extend(other_result.config.http_routes);
            merged_config.tcp_routes.extend(other_result.config.tcp_routes);
            merged_config.tls_routes.extend(other_result.config.tls_routes);
            merged_config.udp_routes.extend(other_result.config.udp_routes);
        }
    }

    let _config = &result.config;

    // Compute per-listener attached route counts from accepted routes
    let mut listener_route_counts: HashMap<String, i32> = HashMap::new();
    for listener in &gateway.spec.listeners {
        listener_route_counts.insert(listener.name.clone(), 0);
    }
    for ra in &result.route_status {
        if ra.accepted {
            match &ra.section_name {
                Some(section) => {
                    *listener_route_counts.entry(section.clone()).or_insert(0) += 1;
                }
                None => {
                    // No sectionName means the route attaches to ALL listeners
                    for count in listener_route_counts.values_mut() {
                        *count += 1;
                    }
                }
            }
        }
    }

    // Convert to route table (spawn_blocking: to_route_table does sync DNS resolution)
    let config_clone = merged_config.clone();
    let route_table = tokio::task::spawn_blocking(move || config_clone.to_route_table())
        .await
        .map_err(|e| ReconcileError::Reconcile(e.to_string()))?;

    let route_table_ok = match route_table {
        Ok(route_table) => {
            ctx.routes.store(Arc::new(route_table));
            info!(
                "Gateway {}/{} reconciled: {} HTTP, {} TCP, {} TLS, {} UDP routes",
                gw_ns,
                gw_name,
                merged_config.http_routes.len(),
                merged_config.tcp_routes.len(),
                merged_config.tls_routes.len(),
                merged_config.udp_routes.len(),
            );
            status::update_gateway_status(
                &ctx.client,
                &gateway,
                true,
                "Programmed",
                &listener_route_counts,
                &listener_statuses,
            )
            .await;
            true
        }
        Err(e) => {
            error!(
                "Failed to build route table for Gateway {}/{}: {}",
                gw_ns, gw_name, e
            );
            status::update_gateway_status(
                &ctx.client,
                &gateway,
                false,
                &format!("Route table conversion failed: {}", e),
                &listener_route_counts,
                &listener_statuses,
            )
            .await;
            false
        }
    };

    // Update per-route status
    let programmed = route_table_ok;
    for ra in &result.route_status {
        let route_programmed = ra.accepted && programmed;
        match ra.kind {
            "HTTPRoute" => {
                status::update_route_status::<HTTPRoute>(
                    &ctx.client,
                    &ra.name,
                    &ra.namespace,
                    &ctx.controller_name,
                    &gw_name,
                    &gw_ns,
                    ra.section_name.as_deref(),
                    ra.accepted,
                    &ra.accepted_reason,
                    &ra.message,
                    ra.refs_resolved,
                    &ra.refs_reason,
                    &ra.refs_message,
                    route_programmed,
                    ra.generation,
                )
                .await;
            }
            "TCPRoute" => {
                status::update_route_status::<TCPRoute>(
                    &ctx.client,
                    &ra.name,
                    &ra.namespace,
                    &ctx.controller_name,
                    &gw_name,
                    &gw_ns,
                    ra.section_name.as_deref(),
                    ra.accepted,
                    &ra.accepted_reason,
                    &ra.message,
                    ra.refs_resolved,
                    &ra.refs_reason,
                    &ra.refs_message,
                    route_programmed,
                    ra.generation,
                )
                .await;
            }
            "TLSRoute" => {
                status::update_route_status::<TLSRoute>(
                    &ctx.client,
                    &ra.name,
                    &ra.namespace,
                    &ctx.controller_name,
                    &gw_name,
                    &gw_ns,
                    ra.section_name.as_deref(),
                    ra.accepted,
                    &ra.accepted_reason,
                    &ra.message,
                    ra.refs_resolved,
                    &ra.refs_reason,
                    &ra.refs_message,
                    route_programmed,
                    ra.generation,
                )
                .await;
            }
            "UDPRoute" => {
                status::update_route_status::<UDPRoute>(
                    &ctx.client,
                    &ra.name,
                    &ra.namespace,
                    &ctx.controller_name,
                    &gw_name,
                    &gw_ns,
                    ra.section_name.as_deref(),
                    ra.accepted,
                    &ra.accepted_reason,
                    &ra.message,
                    ra.refs_resolved,
                    &ra.refs_reason,
                    &ra.refs_message,
                    route_programmed,
                    ra.generation,
                )
                .await;
            }
            _ => {}
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
