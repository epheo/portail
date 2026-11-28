//! A Gateway and the cluster objects it draws on, translated into what
//! its data planes serve and the status this controller owes: reflector
//! caches in, one [`Plan`] out. The only API reads are the ones no cache
//! holds: NetworkAttachmentDefinitions and named targetPorts.

use kube::runtime::reflector::Store;
use kube::ResourceExt;
use std::collections::HashMap;
use std::sync::Arc;

use gateway_api::gateways::Gateway;

use super::gateway_backends;
use super::reconcile::{class_owner, ClassOwner};
use super::validate::{resolve_gateway_certs, served_gateway, validate_gateway};
use super::watch::StatusReports;
use super::{ControllerCtx, ResourceCache};
use crate::config::PortailConfig;
use crate::kubernetes::addresses::resolve_network_cidrs;
use crate::kubernetes::backend_tls;
use crate::kubernetes::crds::{ExternalNameBackends, RateLimitPolicy};
use crate::kubernetes::parent_ref::route_targets_gateway;
use crate::kubernetes::reconciler::{
    reconcile_to_config, ClusterSnapshot, GatewayRoute, PolicyAcceptance, RouteAcceptance,
};
use crate::kubernetes::services::{
    referenced_backends, resolve_named_target_ports, retain_referenced,
};
use crate::kubernetes::status;
use crate::logging::error;
use crate::sync::{GatewayKey, NetworkCidrs};
use gateway_api::backendtlspolicies::BackendTlsPolicy;

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
    /// The BackendTLSPolicies naming this Gateway's Service backends.
    pub backend_tls: Vec<crate::kubernetes::backend_tls::PolicyStatus>,
    /// Every BackendTLSPolicy this pass owes status: those, and the ones
    /// still carrying our entry for this Gateway.
    pub observed_backend_tls: Vec<Observed>,
}

impl Statuses {
    /// What a pass that failed past validation still says.
    fn translation_only(
        accepted: status::GatewayCondition,
        listeners: HashMap<String, status::ListenerStatus>,
    ) -> Self {
        Statuses {
            accepted,
            listeners,
            routes: Vec::new(),
            policies: Vec::new(),
            observed_routes: Vec::new(),
            observed_policies: Vec::new(),
            backend_tls: Vec::new(),
            observed_backend_tls: Vec::new(),
        }
    }
}

/// Translation failed: nothing new to serve, the snapshot served stays.
/// The Gateway's listener statuses, and why.
pub(crate) struct Rejected {
    pub statuses: Statuses,
    pub error: String,
}

/// Why a pass has no plan.
pub(crate) enum Unplanned {
    /// The Gateway cannot be served as the cluster has it: status says why.
    Rejected(Box<Rejected>),
    /// A read failed that may not fail again. It says nothing of the
    /// Gateway: the status written and the snapshot served are still true,
    /// so nothing is written and the pass comes back.
    Retry(String),
}

impl Unplanned {
    fn rejected(statuses: Statuses, error: String) -> Self {
        Unplanned::Rejected(Box::new(Rejected { statuses, error }))
    }
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

pub(super) fn observed_route<R: GatewayRoute + kube::Resource>(r: &R) -> Observed {
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

pub(super) fn observed_policy(p: &RateLimitPolicy) -> Observed {
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
    push(&mut out, &snapshot.grpc_routes, &cache.grpc_routes, owner);
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

/// Every BackendTLSPolicy this pass owes status: the ones judged for this
/// Gateway, and those whose status still holds our entry for it. The
/// latter is bookkeeping: a policy's change wakes the Gateways it names.
fn observed_backend_tls(
    judged: &[crate::kubernetes::backend_tls::PolicyStatus],
    store: &crate::kubernetes::reads::Recorded<BackendTlsPolicy>,
    controller: &str,
    gw_name: &str,
    gw_ns: &str,
) -> Vec<Observed> {
    let mut out: Vec<Observed> = store
        .all_unrecorded()
        .iter()
        .filter(|p| {
            let (ns, name) = (p.namespace().unwrap_or_default(), p.name_any());
            judged.iter().any(|j| j.namespace == ns && j.name == name)
                || p.status.iter().flat_map(|s| s.ancestors.iter()).any(|a| {
                    a.controller_name == controller
                        && a.ancestor_ref.name == gw_name
                        && a.ancestor_ref.namespace.as_deref().unwrap_or(&ns) == gw_ns
                })
        })
        .map(|p| Observed {
            kind: "BackendTLSPolicy",
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
        })
        .collect();
    out.sort_by(by_identity);
    out
}

/// This controller's Gateways with their listener ports: what a backendRef
/// of kind Gateway may name.
fn served_gateways(ctx: &ControllerCtx) -> HashMap<(String, String), Vec<u16>> {
    let classes = ctx.cache.gateway_classes.state();
    ctx.cache
        .gateways
        .state()
        .iter()
        .filter(|g| {
            matches!(
                class_owner(&classes, &g.spec.gateway_class_name, &ctx.controller_name),
                ClassOwner::Ours
            )
        })
        .filter_map(|g| {
            let ports = g.spec.listeners.iter().map(|l| l.port as u16).collect();
            Some(((g.namespace()?, g.metadata.name.clone()?), ports))
        })
        .collect()
}

/// Translate `gateway` from the reflector caches.
/// `trusted`: the Gateways its parameters trust, named like its Gateway
/// backends, so a move of where one is reachable wakes this Gateway too.
pub(crate) async fn translate(
    ctx: &ControllerCtx,
    gateway: &Gateway,
    external_name_backends: ExternalNameBackends,
    trusted: &[GatewayKey],
    egress: &[(String, String)],
) -> Result<Plan, Unplanned> {
    let gw_name = gateway.name_any();
    let gw_ns = gateway.namespace().unwrap_or_else(|| "default".to_string());

    // Snapshot reflector caches in one shot — no API server calls, and no deep
    // copies: routes are filtered to this Gateway before anything is cloned,
    // and Services ride along as `Arc`s.
    let snapshot = ClusterSnapshot {
        http_routes: gateway_routes(&ctx.cache.http_routes, &gw_name, &gw_ns),
        grpc_routes: gateway_routes(&ctx.cache.grpc_routes, &gw_name, &gw_ns),
        tcp_routes: gateway_routes(&ctx.cache.tcp_routes, &gw_name, &gw_ns),
        tls_routes: gateway_routes(&ctx.cache.tls_routes, &gw_name, &gw_ns),
        udp_routes: gateway_routes(&ctx.cache.udp_routes, &gw_name, &gw_ns),
        namespaces: ctx.cache.namespaces.clone(),
        reference_grants: ctx.cache.reference_grants.clone(),
        rate_limit_policies: gateway_policies(&ctx.cache.rate_limit_policies, &gw_name, &gw_ns),
        gateways: served_gateways(ctx),
    };

    // Bookkeeping, no output of this pass: the sets read unrecorded.
    ctx.certs.retain_referenced(
        &ctx.cache.gateways.state(),
        &ctx.cache.listener_sets.all_unrecorded(),
    );
    ctx.ca_bundles.retain(&ctx.cache.gateways.state());
    ctx.read_retries.retain(&ctx.cache.gateways.state());
    let resolved_certs = resolve_gateway_certs(
        gateway,
        &gw_ns,
        &snapshot.reference_grants,
        &ctx.cache.secrets,
        &ctx.certs,
    );
    let (listeners, accepted) = validate_gateway(gateway, &gw_ns, &resolved_certs);

    let mut result = match reconcile_to_config(
        &served_gateway(gateway, &resolved_certs),
        &snapshot,
        &resolved_certs.valid,
        &ctx.cache.services,
        &ctx.route_verdicts,
        external_name_backends,
        !egress.is_empty(),
    ) {
        Ok(r) => r,
        Err(e) => {
            // What it would have read is unknown: anything wakes it.
            crate::kubernetes::reads::record_everything();
            error!("Failed to translate Gateway {}/{}: {}", gw_ns, gw_name, e);
            return Err(Unplanned::rejected(
                Statuses::translation_only(accepted, listeners),
                e.to_string(),
            ));
        }
    };

    // Service metadata for the backends this Gateway routes to, and only
    // those: API reads and version churn then scale with the Gateway.
    let referenced = referenced_backends(&result.config);
    retain_referenced(&mut result.config, &referenced);
    // A headless Service with a *named* targetPort needs endpoint data the Service
    // spec lacks; resolve those with a one-shot EndpointSlice list (no watch).
    let mut named_reqs = std::mem::take(&mut result.named_target_ports);
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
    ctx.networks.retain_named(
        &ctx.cache.gateways.state(),
        // Pruning by what is still named: no output of this pass.
        &ctx.cache.gateway_parameters.all_unrecorded(),
    );
    let networks =
        match resolve_network_cidrs(&ctx.client, gateway, &gw_ns, egress, &ctx.networks).await {
            Ok(networks) => networks,
            Err(e) if e.transient => return Err(Unplanned::Retry(e.message)),
            Err(e) => {
                error!(
                    "Failed to translate Gateway {}/{}: {}",
                    gw_ns, gw_name, e.message
                );
                return Err(Unplanned::rejected(
                    Statuses::translation_only(accepted, listeners),
                    e.message,
                ));
            }
        };
    let me = GatewayKey::new(gw_ns.clone(), gw_name.clone());
    let services = &ctx.cache.services;
    let policies = &ctx.cache.backend_tls_policies;
    let https = crate::kubernetes::services::verifiable_backends(&result.config);
    let wanted = backend_tls::wanted_configmaps(&https, services, policies);
    let cas = match ctx
        .ca_bundles
        .resolve(&ctx.client, &ctx.cache.configmaps, &me, wanted)
        .await
    {
        Ok(cas) => cas,
        Err(e) => {
            let why = format!("reading the CA bundles of BackendTLSPolicies: {e}");
            if crate::kubernetes::transient(&e) {
                return Err(Unplanned::Retry(why));
            }
            error!("Failed to translate Gateway {}/{}: {}", gw_ns, gw_name, why);
            return Err(Unplanned::rejected(
                Statuses::translation_only(accepted, listeners),
                why,
            ));
        }
    };
    let backend_tls = backend_tls::for_backends(&https, services, policies, |ns, name| {
        let key = (ns.to_string(), name.to_string());
        cas.get(&key)
            .cloned()
            .unwrap_or(backend_tls::CaRef::Missing)
    });
    backend_tls::warn_unverified(
        &result.config.app_protocol_overrides,
        &backend_tls.decisions,
        &https,
    );
    backend_tls::emit(&mut result.config, &backend_tls.decisions);
    gateway_backends::resolve(&ctx.hub, &ctx.reports, &mut result.config, &networks);
    let targets = std::mem::take(&mut result.gateway_targets)
        .into_iter()
        .map(|(namespace, name)| GatewayKey::new(namespace, name))
        .chain(trusted.iter().cloned())
        .filter(|named| *named != me)
        .collect();
    ctx.backends.record(me, targets);
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
        observed_backend_tls: observed_backend_tls(
            &backend_tls.statuses,
            &ctx.cache.backend_tls_policies,
            &ctx.controller_name,
            &gw_name,
            &gw_ns,
        ),
        backend_tls: backend_tls.statuses,
        routes: result.route_status,
        policies: result.policy_status,
    };
    Ok(Plan {
        config: result.config,
        networks,
        statuses,
    })
}
