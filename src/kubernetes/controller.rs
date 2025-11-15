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
struct ReconcileStream(tokio::sync::mpsc::Receiver<()>);

impl futures::Stream for ReconcileStream {
    type Item = ();
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.0.poll_recv(cx)
    }
}

// Safety: Receiver is Send, and we only access it through Pin<&mut Self>
unsafe impl Sync for ReconcileStream {}

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

struct ControllerCtx {
    client: Client,
    routes: Arc<ArcSwap<RouteTable>>,
    controller_name: String,
}

pub async fn run_controller(
    routes: Arc<ArcSwap<RouteTable>>,
    controller_name: String,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let client = Client::try_default().await?;
    info!("Kubernetes client connected, starting Gateway API controller");

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

    let reconcile_stream = ReconcileStream(reconcile_rx);

    let controller = Controller::new(gateways.clone(), watcher::Config::default())
        .watches(http_routes, watcher::Config::default(), |route| {
            map_route_to_gateways(&route.spec.parent_refs)
        })
        .watches(tcp_routes, watcher::Config::default(), |route| {
            map_route_to_gateways(&route.spec.parent_refs)
        })
        .watches(tls_routes, watcher::Config::default(), |route| {
            map_route_to_gateways(&route.spec.parent_refs)
        })
        .watches(udp_routes, watcher::Config::default(), |route| {
            map_route_to_gateways(&route.spec.parent_refs)
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
) -> Vec<ObjectRef<Gateway>> {
    parent_refs
        .as_ref()
        .map(|refs| {
            refs.iter()
                .filter(|pr| pr.kind().map_or(true, |k| k == "Gateway"))
                .map(|pr| {
                    let ns = pr.namespace().unwrap_or("default");
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

    // Fetch all routes across all namespaces
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

    // Fetch TLS certificate data from Kubernetes Secrets
    let mut cert_data: HashMap<(String, String), (Vec<u8>, Vec<u8>)> = HashMap::new();
    for listener in &gateway.spec.listeners {
        if let Some(tls) = &listener.tls {
            if let Some(cert_refs) = &tls.certificate_refs {
                for cert_ref in cert_refs {
                    let secret_ns = cert_ref.namespace.as_deref().unwrap_or(&gw_ns);
                    let secret_api: Api<Secret> = Api::namespaced(ctx.client.clone(), secret_ns);
                    match secret_api.get(&cert_ref.name).await {
                        Ok(secret) => {
                            if let Some(data) = &secret.data {
                                let cert_pem = data.get("tls.crt").map(|b| b.0.clone());
                                let key_pem = data.get("tls.key").map(|b| b.0.clone());
                                if let (Some(cert), Some(key)) = (cert_pem, key_pem) {
                                    cert_data.insert((cert_ref.name.clone(), secret_ns.to_string()), (cert, key));
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

    // Fetch ReferenceGrants for cross-namespace authorization
    let grants_api: Api<ReferenceGrant> = Api::all(ctx.client.clone());
    let reference_grants = grants_api
        .list(&ListParams::default())
        .await
        .map(|list| list.items)
        .unwrap_or_default();

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

    // Build config from K8s resources
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
            )
            .await;
            return Ok(Action::await_change());
        }
    };

    let config = &result.config;

    // Compute per-listener attached route counts from accepted routes
    let mut listener_route_counts: HashMap<String, i32> = HashMap::new();
    for listener in &gateway.spec.listeners {
        listener_route_counts.insert(listener.name.clone(), 0);
    }
    for ra in &result.route_status {
        if ra.accepted {
            if let Some(ref section) = ra.section_name {
                *listener_route_counts.entry(section.clone()).or_insert(0) += 1;
            }
        }
    }

    // Convert to route table (spawn_blocking: to_route_table does sync DNS resolution)
    let config_clone = config.clone();
    let route_table = tokio::task::spawn_blocking(move || config_clone.to_route_table())
        .await
        .map_err(|e| ReconcileError::Reconcile(e.to_string()))?;

    match route_table {
        Ok(route_table) => {
            ctx.routes.store(Arc::new(route_table));
            info!(
                "Gateway {}/{} reconciled: {} HTTP, {} TCP, {} TLS, {} UDP routes",
                gw_ns,
                gw_name,
                config.http_routes.len(),
                config.tcp_routes.len(),
                config.tls_routes.len(),
                config.udp_routes.len(),
            );
            status::update_gateway_status(
                &ctx.client,
                &gateway,
                true,
                "Programmed",
                &listener_route_counts,
            )
            .await;
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
            )
            .await;
        }
    }

    // Update per-route status
    for ra in &result.route_status {
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
                    &ra.message,
                    ra.refs_resolved,
                    &ra.refs_message,
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
                    &ra.message,
                    ra.refs_resolved,
                    &ra.refs_message,
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
                    &ra.message,
                    ra.refs_resolved,
                    &ra.refs_message,
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
                    &ra.message,
                    ra.refs_resolved,
                    &ra.refs_message,
                    ra.generation,
                )
                .await;
            }
            _ => {}
        }
    }

    Ok(Action::await_change())
}

fn error_policy(
    _obj: Arc<Gateway>,
    _error: &ReconcileError,
    _ctx: Arc<ControllerCtx>,
) -> Action {
    warn!("Controller reconciliation error, requeueing");
    Action::requeue(std::time::Duration::from_secs(5))
}
