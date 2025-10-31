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
use tokio_util::sync::CancellationToken;

use gateway_api::gatewayclasses::GatewayClass;
use gateway_api::gateways::Gateway;
use gateway_api::httproutes::HTTPRoute;
use gateway_api::experimental::tcproutes::TCPRoute;
use gateway_api::experimental::tlsroutes::TLSRoute;
use gateway_api::experimental::udproutes::UDPRoute;

use crate::routing::RouteTable;
use crate::logging::{info, warn, error, debug};

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

    let ctx = Arc::new(ControllerCtx {
        client: client.clone(),
        routes,
        controller_name,
    });

    let controller = Controller::new(gateways, watcher::Config::default())
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

    // Build config from K8s resources
    let config = match reconcile_to_config(
        &gateway,
        &http_routes,
        &tcp_routes,
        &tls_routes,
        &udp_routes,
    ) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to reconcile Gateway {}/{}: {}", gw_ns, gw_name, e);
            status::update_gateway_status(
                &ctx.client,
                &gateway,
                false,
                &format!("Reconciliation failed: {}", e),
            )
            .await;
            return Ok(Action::await_change());
        }
    };

    // Convert to route table
    match config.to_route_table() {
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
            status::update_gateway_status(&ctx.client, &gateway, true, "Programmed").await;
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
            )
            .await;
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
