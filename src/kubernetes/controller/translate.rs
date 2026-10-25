//! A Gateway and the cluster objects it draws on, translated into what
//! its data planes serve and the status this controller owes: reflector
//! caches in, one [`Plan`] out. The only API reads are the ones no cache
//! holds: NetworkAttachmentDefinitions and named targetPorts.

use kube::runtime::reflector::Store;
use kube::ResourceExt;
use std::collections::HashMap;
use std::sync::Arc;

use gateway_api::gateways::Gateway;

use super::validate::{resolve_gateway_certs, validate_gateway};
use super::watch::StatusReports;
use super::{ControllerCtx, ResourceCache};
use crate::config::PortailConfig;
use crate::kubernetes::addresses::resolve_network_cidrs;
use crate::kubernetes::crds::RateLimitPolicy;
use crate::kubernetes::parent_ref::route_targets_gateway;
use crate::kubernetes::reconciler::{
    reconcile_to_config, ClusterSnapshot, GatewayRoute, PolicyAcceptance, RouteAcceptance,
};
use crate::kubernetes::services::{
    referenced_backends, resolve_named_target_ports, resolve_services, retain_referenced,
};
use crate::kubernetes::status;
use crate::logging::error;
use crate::sync::NetworkCidrs;

/// One Gateway, translated.
pub(crate) struct Plan {
    /// What its data planes serve, PEMs and Service overrides still in
    /// their serde-skipped fields: [`Snapshot::from_config`] moves them onto
    /// the wire.
    ///
    /// [`Snapshot::from_config`]: crate::sync::Snapshot::from_config
    pub config: PortailConfig,
    /// The CIDRs of the networks its Network addresses name.
    pub networks: Vec<NetworkCidrs>,
    pub statuses: Statuses,
}

/// Status from translation alone; Programmed comes from reports.
pub(crate) struct Statuses {
    pub accepted: status::GatewayCondition,
    pub listeners: HashMap<String, status::ListenerStatus>,
    pub routes: Vec<RouteAcceptance>,
    pub policies: Vec<PolicyAcceptance>,
    /// Every route this pass owes status, as the store holds it.
    pub observed_routes: Vec<Observed>,
    /// Every policy this pass owes status, as the store holds it.
    pub observed_policies: Vec<Observed>,
}

/// Translation failed: nothing new to serve, the snapshot served stays.
/// The Gateway's listener statuses, and why.
pub(crate) struct Rejected {
    pub statuses: Statuses,
    pub error: String,
}

/// Read all objects from a reflector store, cloning each. Only used for
/// ReferenceGrants — small and few; routes and Services stay behind `Arc`.
fn cloned<K>(store: &Store<K>) -> Vec<K>
where
    K: kube::Resource + Clone,
    K::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
    store.state().iter().map(|arc| (**arc).clone()).collect()
}

/// Routes from a reflector store that target this Gateway, as `Arc`s straight
/// out of the store — the per-Gateway filter runs before anything is copied,
/// so a cluster full of other Gateways' routes costs pointer moves only.
fn gateway_routes<R>(store: &Store<R>, gw_name: &str, gw_ns: &str) -> Vec<Arc<R>>
where
    R: GatewayRoute + kube::Resource + Clone,
    R::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
    store
        .state()
        .into_iter()
        .filter(|r| route_targets_gateway(r.parent_refs(), r.route_namespace(), gw_name, gw_ns))
        .collect()
}

/// Policies from the reflector store that target this Gateway — `Arc`s
/// straight out of the store, filtered before anything is cloned, like
/// `gateway_routes`.
fn gateway_policies(
    store: &Store<RateLimitPolicy>,
    gw_name: &str,
    gw_ns: &str,
) -> Vec<Arc<RateLimitPolicy>> {
    store
        .state()
        .into_iter()
        .filter(|p| p.targets_gateway(gw_ns, gw_name))
        .collect()
}

/// A route's `status.parents` or a policy's `status.ancestors` as the store
/// holds it.
pub(crate) struct Observed {
    pub kind: &'static str,
    pub namespace: String,
    pub name: String,
    pub entries: status::ObservedEntries,
}

fn by_identity(a: &Observed, b: &Observed) -> std::cmp::Ordering {
    (a.kind, &a.namespace, &a.name).cmp(&(b.kind, &b.namespace, &b.name))
}

fn observed_route<R: GatewayRoute + kube::Resource>(r: &R) -> Observed {
    Observed {
        kind: R::KIND,
        namespace: r.route_namespace().to_string(),
        name: r.identity().0.to_string(),
        entries: status::ObservedEntries {
            resource_version: r.meta().resource_version.clone(),
            entries: r.status_parents_json(),
        },
    }
}

fn observed_policy(p: &RateLimitPolicy) -> Observed {
    Observed {
        kind: "RateLimitPolicy",
        namespace: p.namespace().unwrap_or_default(),
        name: p.name_any(),
        entries: status::ObservedEntries {
            resource_version: p.metadata.resource_version.clone(),
            entries: p
                .status
                .as_ref()
                .map(|s| {
                    s.ancestors
                        .iter()
                        .filter_map(|a| serde_json::to_value(a).ok())
                        .collect()
                })
                .unwrap_or_default(),
        },
    }
}

/// Every route this pass owes status: the snapshot's, which target this
/// Gateway, and the store's routes whose status still carries our entry for
/// it although they no longer do. Those get no entry, so ours is removed:
/// Gateway API RouteParentStatus.controllerName, "Controllers should ensure
/// that entries to status populated with their ControllerName are cleaned up
/// when they are no longer necessary".
fn observed_routes(
    snapshot: &ClusterSnapshot,
    cache: &ResourceCache,
    controller: &str,
    gw_name: &str,
    gw_ns: &str,
) -> Vec<Observed> {
    fn push<R>(
        out: &mut Vec<Observed>,
        targeting: &[Arc<R>],
        store: &Store<R>,
        (controller, gw_name, gw_ns): (&str, &str, &str),
    ) where
        R: GatewayRoute + StatusReports + kube::Resource + Clone,
        R::DynamicType: Default + Eq + std::hash::Hash + Clone,
    {
        out.extend(targeting.iter().map(|r| observed_route(&**r)));
        out.extend(
            store
                .state()
                .iter()
                .filter(|r| {
                    !route_targets_gateway(r.parent_refs(), r.route_namespace(), gw_name, gw_ns)
                        && r.reports(controller, gw_ns, gw_name)
                })
                .map(|r| observed_route(&**r)),
        );
    }
    let owner = (controller, gw_name, gw_ns);
    let mut out = Vec::new();
    push(&mut out, &snapshot.http_routes, &cache.http_routes, owner);
    push(&mut out, &snapshot.tcp_routes, &cache.tcp_routes, owner);
    push(&mut out, &snapshot.tls_routes, &cache.tls_routes, owner);
    push(&mut out, &snapshot.udp_routes, &cache.udp_routes, owner);
    out.sort_by(by_identity);
    out
}

/// Every policy this pass owes status: the snapshot's, which target this
/// Gateway, and the ones still carrying our entry for it, as for routes.
fn observed_policies(
    targeting: &[Arc<RateLimitPolicy>],
    store: &Store<RateLimitPolicy>,
    controller: &str,
    gw_name: &str,
    gw_ns: &str,
) -> Vec<Observed> {
    let stale = store
        .state()
        .into_iter()
        .filter(|p| !p.targets_gateway(gw_ns, gw_name) && p.reports(controller, gw_ns, gw_name));
    let mut out: Vec<Observed> = targeting
        .iter()
        .cloned()
        .chain(stale)
        .map(|p| observed_policy(&p))
        .collect();
    out.sort_by(by_identity);
    out
}

/// Translate `gateway` from the reflector caches.
pub(crate) async fn translate(
    ctx: &ControllerCtx,
    gateway: &Gateway,
) -> Result<Plan, Box<Rejected>> {
    let gw_name = gateway.name_any();
    let gw_ns = gateway.namespace().unwrap_or_else(|| "default".to_string());

    // Snapshot reflector caches in one shot — no API server calls, and no deep
    // copies: routes are filtered to this Gateway before anything is cloned,
    // and Services ride along as `Arc`s.
    let snapshot = ClusterSnapshot {
        http_routes: gateway_routes(&ctx.cache.http_routes, &gw_name, &gw_ns),
        tcp_routes: gateway_routes(&ctx.cache.tcp_routes, &gw_name, &gw_ns),
        tls_routes: gateway_routes(&ctx.cache.tls_routes, &gw_name, &gw_ns),
        udp_routes: gateway_routes(&ctx.cache.udp_routes, &gw_name, &gw_ns),
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
        reference_grants: cloned(&ctx.cache.reference_grants),
        services: ctx.cache.services.state(),
        rate_limit_policies: gateway_policies(&ctx.cache.rate_limit_policies, &gw_name, &gw_ns),
    };

    ctx.certs.retain_referenced(&ctx.cache.gateways.state());
    let resolved_certs = resolve_gateway_certs(
        gateway,
        &gw_ns,
        &snapshot.reference_grants,
        &ctx.cache.secrets,
        &ctx.certs,
    );
    let (services, mut named_reqs) = resolve_services(&snapshot.services);
    let (listeners, accepted) = validate_gateway(gateway, &gw_ns, &resolved_certs);

    let mut result = match reconcile_to_config(
        gateway,
        &snapshot,
        &resolved_certs.valid,
        &services,
        &ctx.route_verdicts,
    ) {
        Ok(r) => r,
        Err(e) => {
            error!("Failed to translate Gateway {}/{}: {}", gw_ns, gw_name, e);
            return Err(Box::new(Rejected {
                statuses: Statuses {
                    accepted,
                    listeners,
                    routes: Vec::new(),
                    policies: Vec::new(),
                    observed_routes: Vec::new(),
                    observed_policies: Vec::new(),
                },
                error: e.to_string(),
            }));
        }
    };

    // Service metadata for the backends this Gateway routes to, and only
    // those: API reads and version churn then scale with the Gateway.
    let referenced = referenced_backends(&result.config);
    retain_referenced(&mut result.config, &referenced);
    // A headless Service with a *named* targetPort needs endpoint data the Service
    // spec lacks; resolve those with a one-shot EndpointSlice list (no watch).
    named_reqs.retain(|req| referenced.contains(&req.backend()));
    if !named_reqs.is_empty() {
        resolve_named_target_ports(
            &ctx.client,
            &ctx.named_ports,
            &named_reqs,
            &mut result.config.headless_target_ports,
        )
        .await;
    }

    // A network dropped from spec.addresses forgets its CIDRs: a failed read
    // once it is named again must not bind on an answer for a spec the
    // Gateway no longer had.
    ctx.networks.retain_named(&ctx.cache.gateways.state());
    let networks = match resolve_network_cidrs(&ctx.client, gateway, &gw_ns, &ctx.networks).await {
        Ok(networks) => networks,
        Err(e) => {
            error!("Failed to translate Gateway {}/{}: {}", gw_ns, gw_name, e);
            return Err(Box::new(Rejected {
                statuses: Statuses {
                    accepted,
                    listeners,
                    routes: Vec::new(),
                    policies: Vec::new(),
                    observed_routes: Vec::new(),
                    observed_policies: Vec::new(),
                },
                error: e,
            }));
        }
    };
    let statuses = Statuses {
        accepted,
        listeners,
        observed_routes: observed_routes(
            &snapshot,
            &ctx.cache,
            &ctx.controller_name,
            &gw_name,
            &gw_ns,
        ),
        observed_policies: observed_policies(
            &snapshot.rate_limit_policies,
            &ctx.cache.rate_limit_policies,
            &ctx.controller_name,
            &gw_name,
            &gw_ns,
        ),
        routes: result.route_status,
        policies: result.policy_status,
    };
    Ok(Plan {
        config: result.config,
        networks,
        statuses,
    })
}
