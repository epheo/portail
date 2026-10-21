//! The reconcile pass: snapshot the reflector caches, build this Gateway's
//! config, apply it to the data plane when its fingerprint moved, and write
//! the status that differs from what the store holds. Pure helpers
//! (fingerprint, endpoint math, legacy multi-Gateway merge) live here beside
//! the orchestrator that calls them.

use kube::runtime::controller::Action;
use kube::runtime::reflector::Store;
use kube::ResourceExt;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use gateway_api::experimental::tcproutes::TCPRoute;
use gateway_api::experimental::tlsroutes::TLSRoute;
use gateway_api::experimental::udproutes::UDPRoute;
use gateway_api::gateways::Gateway;
use gateway_api::httproutes::HTTPRoute;

use crate::logging::{debug, error, info, warn};
use crate::routing::RouteTable;

use super::validate::{resolve_gateway_certs, validate_gateway};
use super::watch::StatusReports;
use super::{
    skip_requeue_secs, success_requeue_secs, ControllerCtx, ReconcileError, ResourceCache,
};
use crate::kubernetes::addresses::{
    compute_bind_addresses, discover_usable_addresses, resolve_network_addresses,
    substitute_network_ips, unresolved_networks,
};
use crate::kubernetes::crds::RateLimitPolicy;
use crate::kubernetes::dns_refresh::Published;
use crate::kubernetes::parent_ref::route_targets_gateway;
use crate::kubernetes::reconciler::{
    reconcile_to_config, ClusterSnapshot, GatewayRoute, PolicyAcceptance, RouteAcceptance,
};
use crate::kubernetes::services::{
    referenced_backends, resolve_named_target_ports, resolve_services, retain_referenced,
};
use crate::kubernetes::status;

/// Read all objects from a reflector store, cloning each. Only used for
/// ReferenceGrants — small and few; routes and Services stay behind `Arc`.
fn snapshot<K>(store: &Store<K>) -> Vec<K>
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
struct Observed {
    kind: &'static str,
    namespace: String,
    name: String,
    entries: status::ObservedEntries,
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

/// Bind the data plane to `binds` (see [`Served::binds`]) and close what
/// they no longer hold. Called after the config is built so ListenerConfig
/// has resolved TLS cert data. Returns the bind errors, keyed by published
/// port.
async fn ensure_data_plane_listeners(
    ctx: &ControllerCtx,
    binds: &[crate::config::ListenerConfig],
) -> Vec<(u16, String)> {
    info!(
        "Ensuring data plane listeners for ports: {:?}",
        binds.iter().map(|l| l.port).collect::<Vec<_>>()
    );
    let closing = match ctx.data_plane.lock() {
        Ok(mut dp) => dp.close_unwanted(binds, &[]),
        Err(e) => {
            warn!("Failed to lock data plane: {}", e);
            return Vec::new();
        }
    };
    // A listener moving between the wildcard and a specific address binds
    // once its old socket is gone, in this pass: the port is dark until
    // then, and the next pass is seconds away.
    for accept_loop in closing {
        let _ = accept_loop.await;
    }
    match ctx.data_plane.lock() {
        Ok(mut dp) => {
            let (opened, errors) =
                dp.add_tcp_listeners(binds, &[], ctx.routes.clone(), &ctx.performance_config);
            if opened > 0 {
                info!("Opened {} new port(s)", opened);
            }
            for (port, err) in &errors {
                warn!("Failed to bind port {}: {}", port, err);
            }
            errors
        }
        Err(e) => {
            warn!("Failed to lock data plane: {}", e);
            Vec::new()
        }
    }
}

/// Listener Programmed follows the data plane: an accepted listener whose
/// socket has no live accept loop is not ready for traffic. Gateway API
/// v1.4.1 ListenerReasonPending: the listener is "not yet online and ready
/// to accept client traffic".
fn apply_bind_state(
    ctx: &ControllerCtx,
    statuses: &mut HashMap<String, status::ListenerStatus>,
    listeners: &[crate::config::ListenerConfig],
    bind: Option<&[String]>,
    bind_errors: &[(u16, String)],
) {
    let Ok(dp) = ctx.data_plane.lock() else {
        return;
    };
    for l in listeners {
        let Some(ls) = statuses.get_mut(&l.name).filter(|ls| ls.programmed) else {
            continue;
        };
        let message = match bind {
            None => "Not bound: no address of this Gateway is on the pod".to_string(),
            Some(bind) => {
                let endpoints = required_endpoints(std::slice::from_ref(l), bind);
                if dp.is_ready_for_endpoints(&endpoints) {
                    continue;
                }
                match bind_errors.iter().find(|(port, _)| *port == l.port) {
                    Some((_, err)) => format!("Listener port not bound: {}", err),
                    None => "Listener port not bound yet".to_string(),
                }
            }
        };
        ls.programmed = false;
        ls.programmed_reason = "Pending".into();
        ls.programmed_message = message;
    }
}

/// (address, bound-port) pairs the data plane must be bound to for this Gateway
/// to be considered Programmed. Keyed on each listener's bound port
/// (`target_port` when the fronting Service decouples it, else the published
/// port) so a deferred privileged listener keeps the Gateway not-Programmed
/// until its real socket is up.
fn required_endpoints(
    listeners: &[crate::config::ListenerConfig],
    bind_addresses: &[String],
) -> Vec<(Option<String>, u16)> {
    if bind_addresses.is_empty() {
        listeners
            .iter()
            .map(|l| (None, l.target_port.unwrap_or(l.port)))
            .collect()
    } else {
        bind_addresses
            .iter()
            .flat_map(|addr| {
                listeners
                    .iter()
                    .map(move |l| (Some(addr.clone()), l.target_port.unwrap_or(l.port)))
            })
            .collect()
    }
}

/// Compute the `Programmed` condition from data-plane readiness. The operator
/// owns address-usability semantics for status.addresses; here we just report
/// whether the data plane has bound this Gateway's listener ports.
fn compute_programmed_condition(dp_ready: bool) -> status::GatewayCondition {
    if dp_ready {
        status::GatewayCondition::ok("Programmed", "Programmed")
    } else {
        warn!("Data plane not ready: not all listener ports are bound");
        status::GatewayCondition::invalid("Data plane not ready: not all listener ports are bound")
    }
}

/// Build the RouteTable from this Gateway's PortailConfig on a blocking thread
/// (`to_route_table` compiles regexes and resolves backend DNS — both sync).
async fn build_route_table(
    config: Arc<crate::config::PortailConfig>,
) -> anyhow::Result<RouteTable> {
    tokio::task::spawn_blocking(move || config.to_route_table())
        .await
        .map_err(|e| anyhow::anyhow!("route table build panicked: {}", e))?
}

/// Fingerprint of what the data plane runs: the effective config (its JSON
/// plus the cert/key bytes and k8s override maps that serde skips) and the
/// bind plan. Status is not hashed: it is compared with the observed status
/// per object instead. Hash inputs with non-deterministic iteration order
/// (HashMaps) are canonicalized by sorting.
fn data_plane_fingerprint(
    config: &crate::config::PortailConfig,
    binds: &[crate::config::ListenerConfig],
) -> u64 {
    use std::hash::Hasher;

    fn write_sorted(h: &mut fnv::FnvHasher, mut items: Vec<String>) {
        items.sort_unstable();
        for item in &items {
            h.write(item.as_bytes());
            h.write(&[0]);
        }
    }

    let mut h = fnv::FnvHasher::default();
    if let Ok(bytes) = serde_json::to_vec(config) {
        h.write(&bytes);
    }
    // Fields the JSON serialization skips but a reconcile still applies:
    // cert/key PEM bytes and the k8s side-channel override maps.
    for l in &config.gateway.listeners {
        if let Some(tls) = &l.tls {
            for cr in &tls.certificate_refs {
                h.write(cr.name.as_bytes());
                if let Some(pem) = &cr.cert_pem {
                    h.write(pem);
                }
                if let Some(pem) = &cr.key_pem {
                    h.write(pem);
                }
            }
        }
    }
    write_sorted(
        &mut h,
        config
            .endpoint_overrides
            .iter()
            .map(|(k, v)| format!("ep{:?}={:?}", k, v))
            .chain(
                config
                    .app_protocol_overrides
                    .iter()
                    .map(|(k, v)| format!("ap{:?}={}", k, v)),
            )
            .chain(
                config
                    .headless_target_ports
                    .iter()
                    .map(|(k, v)| format!("ht{:?}={}", k, v)),
            )
            .collect(),
    );
    for b in binds {
        h.write(format!("{:?}:{}", b.address, b.port).as_bytes());
        h.write(&[0]);
    }
    h.finish()
}

/// Per-listener attachedRoutes. The spec counts ROUTES attached to a
/// listener, so a route whose several parentRefs land on the same listener
/// still counts once — status entries are per parentRef, hence the dedupe.
/// Attachment is allowedRoutes and parentRef alone: the count "MUST count
/// successfully attached Routes that may themselves have Accepted: false
/// conditions" (Listener.attachedRoutes).
fn count_attached_routes(
    listeners: &[gateway_api::gateways::GatewayListeners],
    route_status: &[RouteAcceptance],
) -> HashMap<String, i32> {
    let mut counts: HashMap<String, i32> = listeners.iter().map(|l| (l.name.clone(), 0)).collect();
    let mut counted: HashSet<(&str, &str, &str, &str)> = HashSet::new();
    for ra in route_status {
        for listener_name in &ra.listener_names {
            if counted.insert((ra.kind, &ra.namespace, &ra.name, listener_name)) {
                *counts.entry(listener_name.clone()).or_insert(0) += 1;
            }
        }
    }
    counts
}

/// Merge per-Gateway configs into one data-plane config (legacy unscoped
/// mode, where a single process serves every Gateway).
///
/// Listener names and route parentRef sectionNames are prefixed with their
/// Gateway's identity so a route can never cross-match a same-named listener
/// on another Gateway.
fn merge_gateway_configs(configs: &GatewayConfigs) -> crate::config::PortailConfig {
    use crate::config::{ParentRef, PortailConfig};

    let mut ordered: Vec<_> = configs.iter().collect();
    ordered.sort_by(|a, b| a.0.cmp(b.0));

    let mut merged = PortailConfig::default();
    merged.gateway.name = "portail-merged".to_string();
    merged.gateway.listeners.clear();

    for ((ns, name), served) in ordered {
        let prefix = format!("{}/{}/", ns, name);
        let mut c: PortailConfig = (*served.config).clone();

        for l in &mut c.gateway.listeners {
            l.name = format!("{}{}", prefix, l.name);
        }
        // The reconciler names every attached listener. A ref without a
        // sectionName would select every merged Gateway's listeners.
        let scope = |refs: &mut Vec<ParentRef>| {
            refs.retain_mut(|pr| match &mut pr.section_name {
                Some(section) => {
                    section.insert_str(0, &prefix);
                    true
                }
                None => false,
            })
        };
        for r in &mut c.http_routes {
            scope(&mut r.parent_refs);
        }
        for r in &mut c.tcp_routes {
            scope(&mut r.parent_refs);
        }
        for r in &mut c.udp_routes {
            scope(&mut r.parent_refs);
        }
        for r in &mut c.tls_routes {
            scope(&mut r.parent_refs);
        }

        merged.gateway.listeners.extend(c.gateway.listeners);
        merged.gateway.addresses.extend(c.gateway.addresses);
        merged.http_routes.extend(c.http_routes);
        merged.tcp_routes.extend(c.tcp_routes);
        merged.udp_routes.extend(c.udp_routes);
        merged.tls_routes.extend(c.tls_routes);
        merged.endpoint_overrides.extend(c.endpoint_overrides);
        merged
            .app_protocol_overrides
            .extend(c.app_protocol_overrides);
        merged.headless_target_ports.extend(c.headless_target_ports);
    }
    merged.gateway.addresses.dedup();
    merged
}

/// What one pass's status writes did.
#[derive(Default)]
struct Writes {
    patched: usize,
    failed: bool,
}

impl Writes {
    fn record(&mut self, write: status::Write) {
        match write {
            status::Write::Current => {}
            status::Write::Patched => self.patched += 1,
            status::Write::Failed => self.failed = true,
        }
    }
}

/// Route status: one `status.parents` write per observed route whose entries
/// for this Gateway differ from what `route_status` says. All of a route's
/// entries go in one write: the list is atomic.
async fn write_route_statuses(
    ctx: &ControllerCtx,
    gw_name: &str,
    gw_ns: &str,
    route_status: &[RouteAcceptance],
    observed: &[Observed],
    programmed: bool,
    writes: &mut Writes,
) {
    let field_manager = format!("portail-{}-{}", gw_ns, gw_name);
    let owner = status::Owner {
        controller: &ctx.controller_name,
        gw_name,
        gw_ns,
    };
    let mut desired: HashMap<(&str, &str, &str), Vec<serde_json::Value>> = HashMap::new();
    for ra in route_status {
        desired
            .entry((ra.kind, &ra.namespace, &ra.name))
            .or_default()
            .push(status::route_parent_entry(&status::RouteParentStatus {
                controller_name: ctx.controller_name.clone(),
                gateway_name: gw_name.to_string(),
                gateway_namespace: gw_ns.to_string(),
                section_name: ra.section_name.clone(),
                port: ra.port,
                accepted: ra.accepted,
                accepted_reason: ra.accepted_reason,
                message: ra.message.clone(),
                refs_resolved: ra.refs_resolved,
                refs_reason: ra.refs_reason,
                refs_message: ra.refs_message.clone(),
                programmed: ra.accepted && programmed,
                generation: ra.generation,
            }));
    }

    for o in observed {
        let entries = desired
            .remove(&(o.kind, o.namespace.as_str(), o.name.as_str()))
            .unwrap_or_default();
        macro_rules! write_parents {
            ($ty:ty) => {
                status::write_owned_entries::<$ty>(
                    &ctx.client,
                    &o.namespace,
                    &o.name,
                    ("parents", "parentRef"),
                    &owner,
                    &o.entries,
                    entries,
                    &field_manager,
                )
                .await
            };
        }
        writes.record(match o.kind {
            "HTTPRoute" => write_parents!(HTTPRoute),
            "TCPRoute" => write_parents!(TCPRoute),
            "TLSRoute" => write_parents!(TLSRoute),
            "UDPRoute" => write_parents!(UDPRoute),
            _ => continue,
        });
    }
}

/// Policy status: one `status.ancestors` write per observed policy whose
/// entry for this Gateway differs from what `policy_status` says.
async fn write_policy_statuses(
    ctx: &ControllerCtx,
    gw_name: &str,
    gw_ns: &str,
    policy_status: &[PolicyAcceptance],
    observed: &[Observed],
    writes: &mut Writes,
) {
    let field_manager = format!("portail-{}-{}", gw_ns, gw_name);
    let owner = status::Owner {
        controller: &ctx.controller_name,
        gw_name,
        gw_ns,
    };
    for o in observed {
        let entries: Vec<serde_json::Value> = policy_status
            .iter()
            .filter(|pa| pa.namespace == o.namespace && pa.name == o.name)
            .map(|pa| {
                status::policy_ancestor_entry(
                    &ctx.controller_name,
                    gw_name,
                    gw_ns,
                    pa.accepted,
                    pa.reason,
                    &pa.message,
                    pa.generation,
                )
            })
            .collect();
        writes.record(
            status::write_owned_entries::<RateLimitPolicy>(
                &ctx.client,
                &o.namespace,
                &o.name,
                ("ancestors", "ancestorRef"),
                &owner,
                &o.entries,
                entries,
                &field_manager,
            )
            .await,
        );
    }
}

/// Whether `class_name` resolves to a GatewayClass whose controllerName is
/// ours. Pure — the unscoped-mode ownership filter.
fn gateway_class_is_ours(
    classes: &[Arc<gateway_api::gatewayclasses::GatewayClass>],
    class_name: &str,
    controller_name: &str,
) -> bool {
    classes.iter().any(|gc| {
        gc.metadata.name.as_deref() == Some(class_name)
            && gc.spec.controller_name == controller_name
    })
}

/// One Gateway as the data plane serves it.
#[derive(Clone)]
pub(super) struct Served {
    config: Arc<crate::config::PortailConfig>,
    /// Where its listeners bind: [`compute_bind_addresses`] of its own
    /// addresses, so a Gateway that may bind nothing closes only its own.
    bind: Option<Vec<String>>,
}

impl Served {
    /// Its listeners as the data plane binds them: one per bind address,
    /// with `address` set to it; as they are on every interface; none when
    /// nothing may bind.
    fn binds(&self) -> Vec<crate::config::ListenerConfig> {
        let listeners = &self.config.gateway.listeners;
        match self.bind.as_deref() {
            None => Vec::new(),
            Some([]) => listeners.clone(),
            Some(addresses) => listeners
                .iter()
                .flat_map(|l| {
                    addresses.iter().map(|a| crate::config::ListenerConfig {
                        address: Some(a.clone()),
                        ..l.clone()
                    })
                })
                .collect(),
        }
    }

    /// The endpoints its listeners must have live to be ready. Unbindable
    /// listeners still count, so readiness stays down.
    fn required(&self) -> Vec<(Option<String>, u16)> {
        required_endpoints(
            &self.config.gateway.listeners,
            self.bind.as_deref().unwrap_or_default(),
        )
    }
}

pub(super) type GatewayConfigs = HashMap<(String, String), Served>;

/// Drop the configs of Gateways no longer in the store.
fn retain_live_gateways(ctx: &ControllerCtx, configs: &mut GatewayConfigs) {
    let live: HashSet<(String, String)> = ctx
        .cache
        .gateways
        .state()
        .iter()
        .filter_map(|g| Some((g.namespace()?, g.metadata.name.clone()?)))
        .collect();
    configs.retain(|k, _| live.contains(k));
    ctx.route_verdicts.retain(|k| live.contains(k));
}

/// What the data plane runs: the route table's config, and the listeners
/// to bind (see [`Served::binds`]).
struct Effective {
    config: Arc<crate::config::PortailConfig>,
    binds: Vec<crate::config::ListenerConfig>,
}

/// The one Gateway's config in scoped mode, the merge of all of them in
/// legacy unscoped mode; each Gateway's listeners bound on its own
/// addresses either way.
fn effective_of(configs: &GatewayConfigs) -> Effective {
    let config = match configs.values().next() {
        Some(only) if configs.len() == 1 => only.config.clone(),
        _ => Arc::new(merge_gateway_configs(configs)),
    };
    let mut ordered: Vec<_> = configs.iter().collect();
    ordered.sort_by(|a, b| a.0.cmp(b.0));
    let binds = ordered
        .into_iter()
        .flat_map(|(_, served)| served.binds())
        .collect();
    Effective { config, binds }
}

/// Unscoped mode: stop serving Gateways deleted from the store. Deletion
/// runs no reconcile, and with no Gateway left no later pass would ever
/// close their listeners or drop their routes.
pub(super) async fn prune_deleted_gateways(ctx: &ControllerCtx) {
    let _apply_guard = ctx.apply_lock.lock().await;
    let (effective, required, left) = {
        let mut configs = match ctx.gateway_configs.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let before = configs.len();
        retain_live_gateways(ctx, &mut configs);
        if configs.len() == before {
            return;
        }
        let required: Vec<_> = configs.values().flat_map(Served::required).collect();
        (effective_of(&configs), required, configs.len())
    };
    match apply_data_plane(ctx, &effective, &required)
        .await
        .build_error
    {
        Some(e) => error!(
            "Failed to build route table after a Gateway deletion: {}",
            e
        ),
        None => info!("Deleted Gateway(s) pruned; {} Gateway(s) served", left),
    }
}

/// The data plane's last apply: one per process, since every Gateway it
/// serves shares the route table and the listener set.
#[derive(Clone)]
pub(super) struct Applied {
    fingerprint: u64,
    build_error: Option<String>,
}

/// How a pass left the data plane.
struct DataPlaneState {
    /// This Gateway's required endpoints have live accept loops.
    ready: bool,
    rebuilt: bool,
    build_error: Option<String>,
    /// From this pass's binds, keyed by published port.
    bind_errors: Vec<(u16, String)>,
}

/// Bring the data plane in line with `effective`. The gate is three-way:
/// - unchanged fingerprint, `required` live: nothing to do. The fingerprint
///   covers the cert bytes (no hot-reload can be pending), and readiness is
///   read from live accept loops, so a dead listener takes the bind path.
/// - unchanged, not live: the one thing still owed is a bind retry.
/// - changed: bind, rebuild the table, publish it.
///
/// The data plane gets the EFFECTIVE (every Gateway's) listeners, not this
/// Gateway's: in legacy unscoped mode several Gateways may share a port,
/// and merge_cert_refs_by_port can only merge cert refs it can see. Passing
/// one Gateway's listeners at a time made alternating reconciles rebuild
/// the port's ServerConfig with only that Gateway's certs (v0.1.13
/// regression). Readiness (`required`) stays keyed on this Gateway's own
/// listeners: its Programmed condition should not depend on sibling
/// Gateways' ports.
async fn apply_data_plane(
    ctx: &ControllerCtx,
    effective: &Effective,
    required: &[(Option<String>, u16)],
) -> DataPlaneState {
    let effective_config = &effective.config;
    let fingerprint = data_plane_fingerprint(effective_config, &effective.binds);
    let last = match ctx.last_applied.lock() {
        Ok(last) => last.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    let unchanged = last.filter(|applied| applied.fingerprint == fingerprint);
    let is_ready = || {
        ctx.data_plane
            .lock()
            .map(|dp| dp.is_ready_for_endpoints(required))
            .unwrap_or(false)
    };

    if is_ready() {
        ctx.ready.store(true, std::sync::atomic::Ordering::Release);
        if let Some(applied) = &unchanged {
            return DataPlaneState {
                ready: true,
                rebuilt: false,
                build_error: applied.build_error.clone(),
                bind_errors: Vec::new(),
            };
        }
    }

    let bind_errors = ensure_data_plane_listeners(ctx, &effective.binds).await;
    let ready = is_ready();
    // Store the truth in BOTH directions: readiness must also drop when a
    // required endpoint has no live accept loop and the rebind above failed,
    // not only rise on success.
    let was_ready = ctx.ready.swap(ready, std::sync::atomic::Ordering::AcqRel);
    if ready && !was_ready {
        info!(
            "Data plane ready: {} listener endpoint(s) live, {:.1}s after controller start",
            required.len(),
            ctx.started.elapsed().as_secs_f32()
        );
    }
    if let Some(applied) = unchanged {
        return DataPlaneState {
            ready,
            rebuilt: false,
            build_error: applied.build_error,
            bind_errors,
        };
    }

    let build_error = match build_route_table(effective_config.clone()).await {
        Ok(route_table) => {
            let table = Arc::new(route_table);
            ctx.routes.store(table.clone());
            if let Ok(health) = ctx.data_plane.lock().map(|dp| dp.health().clone()) {
                health.prune(&table);
            }
            // Publish only after the table is live: the DNS-refresh task
            // re-resolves from this pair and must never pick up a config
            // whose build failed. Its CAS against the pair's table drops a
            // tick that read the previous pair.
            ctx.published.store(Some(Arc::new(Published {
                config: effective_config.clone(),
                table,
            })));
            None
        }
        Err(e) => Some(e.to_string()),
    };
    if let Ok(mut last) = ctx.last_applied.lock() {
        *last = Some(Applied {
            fingerprint,
            build_error: build_error.clone(),
        });
    }
    DataPlaneState {
        ready,
        rebuilt: true,
        build_error,
        bind_errors,
    }
}

pub(super) async fn reconcile(
    gateway: Arc<Gateway>,
    ctx: Arc<ControllerCtx>,
) -> Result<Action, ReconcileError> {
    let gw_name = gateway.name_any();
    let gw_ns = gateway.namespace().unwrap_or_else(|| "default".to_string());
    debug!("Reconciling Gateway {}/{}", gw_ns, gw_name);

    // Scoped mode trusts portail-operator: it only provisions a Deployment for
    // Gateways whose class references this controller. Unscoped mode watches
    // EVERY Gateway in the cluster, so claiming one whose GatewayClass names a
    // different controller would hijack it — status writes fighting the real
    // owner, ports bound for traffic we were never asked to serve. Ignore
    // those entirely (no status, no binds); a class watch re-triggers us if
    // ownership changes. A class not in the store yet is also not ours to
    // claim.
    if let Some(classes) = &ctx.cache.gateway_classes {
        if !gateway_class_is_ours(
            &classes.state(),
            &gateway.spec.gateway_class_name,
            &ctx.controller_name,
        ) {
            debug!(
                "Gateway {}/{} class '{}' does not name this controller; ignoring",
                gw_ns, gw_name, gateway.spec.gateway_class_name
            );
            return Ok(Action::await_change());
        }
    }

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
        reference_grants: snapshot(&ctx.cache.reference_grants),
        services: ctx.cache.services.state(),
        rate_limit_policies: gateway_policies(&ctx.cache.rate_limit_policies, &gw_name, &gw_ns),
    };

    let resolved_certs = resolve_gateway_certs(
        &gateway,
        &gw_ns,
        &snapshot.reference_grants,
        &ctx.cache.secrets,
    );
    let (services, mut named_reqs) = resolve_services(&snapshot.services);

    let (mut listener_statuses, accepted_cond) =
        validate_gateway(&gateway, &gw_ns, &resolved_certs);

    let mut usable = discover_usable_addresses();
    resolve_network_addresses(&ctx.client, &gateway, &gw_ns, &mut usable, &ctx.network_ips).await;

    // Build per-Gateway config; bail out with a failing Programmed condition on error.
    let mut result = match reconcile_to_config(
        &gateway,
        &snapshot,
        &resolved_certs.valid,
        &services,
        &ctx.route_verdicts,
    ) {
        Ok(r) => r,
        Err(e) => {
            error!("Failed to reconcile Gateway {}/{}: {}", gw_ns, gw_name, e);
            let failure =
                status::GatewayCondition::invalid(format!("Reconciliation failed: {}", e));
            let desired = status::desired_gateway_status(
                &gateway,
                &accepted_cond,
                &failure,
                &HashMap::new(),
                &listener_statuses,
                &usable,
                ctx.manage_gateway_status,
            );
            status::write_gateway_status(&ctx.client, &gateway, desired).await;
            return Err(ReconcileError(e.to_string()));
        }
    };

    // Service metadata for the backends this Gateway routes to, and only
    // those: API reads and fingerprint churn then scale with the Gateway.
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

    substitute_network_ips(&mut result.config.gateway.addresses, &usable);
    let bind_addresses = compute_bind_addresses(&result.config.gateway.addresses, &usable);
    let unbindable = bind_addresses.is_none().then(|| status::GatewayCondition {
        ok: false,
        reason: "AddressNotUsable".to_string(),
        message: format!(
            "Network address {} has no IP on this pod, or its NetworkAttachmentDefinition \
             could not be read, so no listener of this Gateway is bound; check the \
             NetworkAttachmentDefinition and the pod's network attachment",
            unresolved_networks(&result.config.gateway.addresses).join(", ")
        ),
    });
    let listeners_empty = result.config.gateway.listeners.is_empty();

    // Serialize the apply section: from here on the pass reads and writes
    // SHARED state (gateway_configs, the data plane's listener set, the live
    // route table, the published pair). kube-rs reconciles different Gateways
    // concurrently in unscoped mode, and two interleaved passes could store a
    // table built from a stale merge over a newer one, or tear down a
    // just-added sibling listener. Holding the lock to the end of the pass
    // makes each apply atomic; the later pass re-merges from the map already
    // holding the earlier insert. Uncontended in scoped mode.
    let _apply_guard = ctx.apply_lock.lock().await;
    let gw_key = (gw_ns.clone(), gw_name.clone());

    // Track this Gateway's config and derive the config the data plane
    // should actually run. In scoped mode (operator-managed, one Gateway per
    // process) that is simply this Gateway's config; in legacy unscoped mode
    // one process serves EVERY Gateway, so publishing only this Gateway's
    // config would clobber the others' routes in the shared table.
    let served = Served {
        config: Arc::new(std::mem::take(&mut result.config)),
        bind: bind_addresses.clone(),
    };
    // Readiness keys on each listener's bound port (`target_port` when the
    // fronting Service decouples it, else the published port). In the brief
    // window where a LoadBalancer pod has no NET_BIND_SERVICE and its Service
    // is not yet observed, a privileged published bind fails harmlessly and
    // readiness stays down until the Service (and its targetPort) appear.
    let required = served.required();
    let config = served.config.clone();
    let effective = {
        let mut map = match ctx.gateway_configs.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        map.insert(gw_key.clone(), served);
        retain_live_gateways(&ctx, &mut map);
        effective_of(&map)
    };

    let dp = apply_data_plane(&ctx, &effective, &required).await;
    apply_bind_state(
        &ctx,
        &mut listener_statuses,
        &config.gateway.listeners,
        bind_addresses.as_deref(),
        &dp.bind_errors,
    );
    match &dp.build_error {
        Some(e) if dp.rebuilt => error!(
            "Failed to build route table for Gateway {}/{}: {}",
            gw_ns, gw_name, e
        ),
        None if dp.rebuilt => info!(
            "Gateway {}/{} reconciled: {} HTTP, {} TCP, {} TLS, {} UDP routes",
            gw_ns,
            gw_name,
            config.http_routes.len(),
            config.tcp_routes.len(),
            config.tls_routes.len(),
            config.udp_routes.len(),
        ),
        _ => {}
    }

    // Every listener skipped as unconvertible: zero required endpoints makes
    // readiness vacuously true, but nothing serves.
    let programmed_cond = if let Some(e) = &dp.build_error {
        status::GatewayCondition::invalid(format!("Route table conversion failed: {}", e))
    } else if listeners_empty && !gateway.spec.listeners.is_empty() {
        status::GatewayCondition::invalid("No listener could be programmed")
    } else if let Some(cond) = unbindable {
        cond
    } else {
        compute_programmed_condition(dp.ready)
    };
    let programmed = programmed_cond.ok;

    // Status is compared with what the store holds, object by object, so a
    // spec bump, a recreated Gateway or a wiped status is written again, and
    // the echo of our own write is not.
    let listener_route_counts =
        count_attached_routes(&gateway.spec.listeners, &result.route_status);
    let mut writes = Writes::default();
    writes.record(
        status::write_gateway_status(
            &ctx.client,
            &gateway,
            status::desired_gateway_status(
                &gateway,
                &accepted_cond,
                &programmed_cond,
                &listener_route_counts,
                &listener_statuses,
                &usable,
                ctx.manage_gateway_status,
            ),
        )
        .await,
    );
    write_route_statuses(
        &ctx,
        &gw_name,
        &gw_ns,
        &result.route_status,
        &observed_routes(
            &snapshot,
            &ctx.cache,
            &ctx.controller_name,
            &gw_name,
            &gw_ns,
        ),
        programmed,
        &mut writes,
    )
    .await;
    write_policy_statuses(
        &ctx,
        &gw_name,
        &gw_ns,
        &result.policy_status,
        &observed_policies(
            &snapshot.rate_limit_policies,
            &ctx.cache.rate_limit_policies,
            &ctx.controller_name,
            &gw_name,
            &gw_ns,
        ),
        &mut writes,
    )
    .await;

    if writes.failed {
        // A status PATCH failed (apiserver blip, or a stale read). Without a
        // fast retry the wrong status would sit in the cluster until the slow
        // safety-net requeue.
        return Ok(Action::requeue(Duration::from_secs(10)));
    }

    // Requeue cadence is convergence-driven — see `success_requeue_secs`.
    let all_routes_accepted = result.route_status.iter().all(|ra| ra.accepted);
    let requeue_secs = if dp.rebuilt || writes.patched > 0 {
        crate::metrics::METRICS.reconcile_runs_total.inc();
        success_requeue_secs(programmed, all_routes_accepted)
    } else {
        crate::metrics::METRICS.reconcile_skips_total.inc();
        debug!(
            "Gateway {}/{} unchanged: no rebuild, no status write",
            gw_ns, gw_name
        );
        skip_requeue_secs(programmed, all_routes_accepted)
    };
    Ok(Action::requeue(Duration::from_secs(requeue_secs)))
}

#[cfg(test)]
mod tests {
    use super::super::testkit::{self, apply, MockApi, TestCluster};
    use super::*;
    use crate::config::{ListenerConfig, Protocol};
    use crate::kubernetes::addresses::NETWORK_ADDRESS_TYPE;

    /// Every observedGeneration in a written status.
    fn observed_generations(v: &serde_json::Value) -> Vec<i64> {
        match v {
            serde_json::Value::Object(map) => map
                .iter()
                .flat_map(|(k, v)| match (k.as_str(), v.as_i64()) {
                    ("observedGeneration", Some(g)) => vec![g],
                    _ => observed_generations(v),
                })
                .collect(),
            serde_json::Value::Array(items) => {
                items.iter().flat_map(observed_generations).collect()
            }
            _ => vec![],
        }
    }

    /// A spec change that leaves the config alone still moves
    /// observedGeneration. The old fingerprint hashed neither the generation
    /// nor the observed status, so this pass wrote nothing.
    #[tokio::test]
    async fn spec_bump_without_config_change_rewrites_observed_generation() {
        let api = MockApi::default();
        let mut cluster = TestCluster::default();
        let gw = testkit::gateway("gw", 1);
        apply(&mut cluster.gateways, gw.clone());
        let ctx = cluster.ctx(api.client());

        reconcile(Arc::new(gw.clone()), ctx.clone()).await.unwrap();
        let written = api
            .status_patch("gateways", "gw")
            .expect("first pass writes");
        let mut gw = testkit::with_gateway_status(gw, &written);

        // Infrastructure labels reach the pod, not the config.
        gw.metadata.generation = Some(2);
        gw.spec.infrastructure = Some(gateway_api::gateways::GatewayInfrastructure {
            labels: Some([("team".to_string(), "a".to_string())].into()),
            ..Default::default()
        });
        apply(&mut cluster.gateways, gw.clone());
        api.clear();
        reconcile(Arc::new(gw), ctx).await.unwrap();

        let rewritten = api
            .status_patch("gateways", "gw")
            .expect("observedGeneration must follow the spec");
        let generations = observed_generations(&rewritten["status"]);
        assert!(
            !generations.is_empty() && generations.iter().all(|g| *g == 2),
            "{generations:?}"
        );
    }

    /// A wiped or recreated Gateway status is written again; the echo of
    /// our own write is not. The old fingerprint saw no difference in
    /// either case.
    #[tokio::test]
    async fn wiped_or_recreated_gateway_status_is_rewritten() {
        let api = MockApi::default();
        let mut cluster = TestCluster::default();
        let gw = testkit::gateway("gw", 1);
        apply(&mut cluster.gateways, gw.clone());
        let ctx = cluster.ctx(api.client());

        reconcile(Arc::new(gw.clone()), ctx.clone()).await.unwrap();
        let written = api.status_patch("gateways", "gw").unwrap();
        let stored = testkit::with_gateway_status(gw, &written);
        apply(&mut cluster.gateways, stored.clone());

        api.clear();
        reconcile(Arc::new(stored.clone()), ctx.clone())
            .await
            .unwrap();
        assert!(
            api.status_patch("gateways", "gw").is_none(),
            "echo of our own write"
        );

        let mut wiped = stored;
        wiped.status = None;
        apply(&mut cluster.gateways, wiped.clone());
        api.clear();
        reconcile(Arc::new(wiped), ctx.clone()).await.unwrap();
        assert!(
            api.status_patch("gateways", "gw").is_some(),
            "wiped status healed"
        );

        let mut recreated = testkit::gateway("gw", 1);
        recreated.metadata.uid = Some("gw-uid-2".into());
        apply(&mut cluster.gateways, recreated.clone());
        api.clear();
        reconcile(Arc::new(recreated), ctx).await.unwrap();
        assert!(
            api.status_patch("gateways", "gw").is_some(),
            "recreated Gateway gets status"
        );
    }

    /// Route status is written when it changes and not on the echo of the
    /// write, which the old fingerprint counted as a change. Every write
    /// carries the resourceVersion it was computed from.
    #[tokio::test]
    async fn route_status_writes_only_what_changed() {
        let api = MockApi::default();
        let mut cluster = TestCluster::default();
        let gw = testkit::gateway("gw", 1);
        apply(&mut cluster.gateways, gw.clone());
        let route = testkit::http_route("r", &["gw"], 1);
        apply(&mut cluster.http_routes, route.clone());
        let ctx = cluster.ctx(api.client());

        reconcile(Arc::new(gw.clone()), ctx.clone()).await.unwrap();
        let patch = api
            .status_patch("httproutes", "r")
            .expect("route gets status");
        assert_eq!(patch["metadata"]["resourceVersion"], "1");
        let route = testkit::with_route_status(route, &patch);
        apply(&mut cluster.http_routes, route.clone());
        let gw = testkit::with_gateway_status(gw, &api.status_patch("gateways", "gw").unwrap());
        apply(&mut cluster.gateways, gw.clone());

        api.clear();
        reconcile(Arc::new(gw.clone()), ctx.clone()).await.unwrap();
        let patches: Vec<_> = api
            .requests()
            .into_iter()
            .filter(|r| r.method == "PATCH")
            .map(|r| r.path)
            .collect();
        assert!(patches.is_empty(), "nothing changed: {patches:?}");

        let mut bumped = route;
        bumped.metadata.generation = Some(2);
        apply(&mut cluster.http_routes, bumped);
        api.clear();
        reconcile(Arc::new(gw), ctx).await.unwrap();
        let patch = api
            .status_patch("httproutes", "r")
            .expect("a spec bump rewrites the route status");
        assert_eq!(patch["metadata"]["resourceVersion"], "2");
        assert!(observed_generations(&patch["status"])
            .iter()
            .all(|g| *g == 2));
    }

    /// A route whose parentRef moved to another Gateway loses our entry for
    /// this one; another controller's entry stays. The route used to drop
    /// out of this Gateway's snapshot, so Accepted=True stayed forever.
    #[tokio::test]
    async fn route_that_moved_away_loses_our_entry() {
        let api = MockApi::default();
        let mut cluster = TestCluster::default();
        let gw = testkit::gateway("gw", 1);
        apply(&mut cluster.gateways, gw.clone());
        let mut route = testkit::http_route("r", &["other-gw"], 2);
        route.status = Some(
            serde_json::from_value(serde_json::json!({ "parents": [
                testkit::status_entry(testkit::CONTROLLER, "gw", "parentRef"),
                testkit::status_entry("example.com/other", "gw", "parentRef"),
            ]}))
            .unwrap(),
        );
        apply(&mut cluster.http_routes, route);
        let ctx = cluster.ctx(api.client());

        reconcile(Arc::new(gw), ctx).await.unwrap();
        let patch = api
            .status_patch("httproutes", "r")
            .expect("our stale entry is removed");
        let parents = patch["status"]["parents"].as_array().unwrap();
        assert_eq!(parents.len(), 1, "{parents:?}");
        assert_eq!(parents[0]["controllerName"], "example.com/other");
    }

    /// Same for a RateLimitPolicy retargeted to another Gateway.
    #[tokio::test]
    async fn policy_that_moved_away_loses_our_ancestor() {
        let api = MockApi::default();
        let mut cluster = TestCluster::default();
        let gw = testkit::gateway("gw", 1);
        apply(&mut cluster.gateways, gw.clone());
        let mut policy = testkit::rate_limit_policy("p", "other-gw");
        policy.status = Some(
            serde_json::from_value(serde_json::json!({ "ancestors": [
                testkit::status_entry(testkit::CONTROLLER, "gw", "ancestorRef"),
            ]}))
            .unwrap(),
        );
        apply(&mut cluster.policies, policy);
        let ctx = cluster.ctx(api.client());

        reconcile(Arc::new(gw), ctx).await.unwrap();
        let patch = api
            .status_patch("ratelimitpolicies", "p")
            .expect("our stale ancestor is removed");
        assert_eq!(patch["status"]["ancestors"], serde_json::json!([]));
    }

    /// Listener conditions of the first `status_patch` for `gw`, by type.
    fn listener_conditions(patch: &serde_json::Value) -> HashMap<String, serde_json::Value> {
        patch["status"]["listeners"][0]["conditions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["type"].as_str().unwrap().to_string(), c.clone()))
            .collect()
    }

    /// A listener whose port cannot be bound is not Programmed. It used to
    /// report Programmed=True from the spec alone.
    #[test]
    fn unbound_listener_is_not_programmed() {
        alone("unbound_listener_is_not_programmed", async {
            let held = std::net::TcpListener::bind(("0.0.0.0", free_port())).unwrap();
            let api = MockApi::default();
            let mut cluster = TestCluster::default();
            let mut gw = testkit::gateway("gw", 1);
            gw.spec.listeners[0].port = held.local_addr().unwrap().port().into();
            apply(&mut cluster.gateways, gw.clone());
            let ctx = cluster.ctx(api.client());

            reconcile(Arc::new(gw.clone()), ctx.clone()).await.unwrap();
            let patch = api.status_patch("gateways", "gw").unwrap();
            let programmed = &listener_conditions(&patch)["Programmed"];
            assert_eq!(programmed["status"], "False", "{programmed}");
            assert_eq!(programmed["reason"], "Pending");
            assert!(
                programmed["message"]
                    .as_str()
                    .unwrap()
                    .contains("not bound"),
                "{programmed}"
            );

            // Once the port frees up, the bind retry programs it.
            drop(held);
            let gw = testkit::with_gateway_status(gw, &patch);
            apply(&mut cluster.gateways, gw.clone());
            api.clear();
            reconcile(Arc::new(gw), ctx).await.unwrap();
            let patch = api.status_patch("gateways", "gw").unwrap();
            assert_eq!(listener_conditions(&patch)["Programmed"]["status"], "True");
        });
    }

    /// This host's first IPv4 address off loopback: one a Gateway can name.
    fn local_ipv4() -> Option<String> {
        discover_usable_addresses()
            .interface_ips
            .into_iter()
            .find(|ip| ip.parse::<std::net::Ipv4Addr>().is_ok())
    }

    fn free_port() -> u16 {
        crate::proxy::data_plane::test_ports::wildcard()
    }

    fn with_addresses(mut gw: Gateway, generation: i64, addrs: &[(&str, &str)]) -> Gateway {
        gw.metadata.generation = Some(generation);
        gw.spec.addresses = Some(
            addrs
                .iter()
                .map(|(kind, value)| gateway_api::gateways::GatewayAddresses {
                    r#type: Some(kind.to_string()),
                    value: Some(value.to_string()),
                })
                .collect(),
        );
        gw
    }

    /// Reconciles `gw` as its `generation`-th spec, with `addrs` for
    /// spec.addresses; returns its first listener's Programmed condition.
    async fn reconcile_with(
        api: &MockApi,
        cluster: &mut TestCluster,
        ctx: &Arc<ControllerCtx>,
        gw: &Gateway,
        generation: i64,
        addrs: &[(&str, &str)],
    ) -> serde_json::Value {
        let gw = with_addresses(gw.clone(), generation, addrs);
        apply(&mut cluster.gateways, gw.clone());
        api.clear();
        reconcile(Arc::new(gw), ctx.clone()).await.unwrap();
        let patch = api.status_patch("gateways", "gw").unwrap();
        listener_conditions(&patch)["Programmed"].clone()
    }

    /// Runs `test` in a process of its own, on a current-thread runtime.
    /// These tests close a socket and bind an overlapping one in one pass,
    /// and a child another test spawns holds this process's sockets until
    /// its exec.
    fn alone(name: &str, test: impl std::future::Future<Output = ()>) {
        if crate::proxy::test_util::in_own_process(module_path!(), name) {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(test);
        }
    }

    /// A Gateway named `name` with its one listener on `port`.
    fn gateway_on(name: &str, port: u16) -> Gateway {
        let mut gw = testkit::gateway(name, 1);
        gw.spec.listeners[0].port = port.into();
        gw
    }

    /// A Gateway that gains a local address moves its listener off the
    /// wildcard in one pass. The wildcard stayed bound beside the address:
    /// open on every interface, while Programmed.
    #[test]
    fn a_listener_leaves_the_wildcard_for_an_address_in_one_pass() {
        alone(
            "a_listener_leaves_the_wildcard_for_an_address_in_one_pass",
            async {
                // Accepting a probe builds a backend pool: rustls needs its provider.
                let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
                let Some(ip) = local_ipv4() else {
                    return; // no address off loopback to name
                };
                let port = free_port();
                let accepts = |addr: &str| std::net::TcpStream::connect((addr, port)).is_ok();
                let api = MockApi::default();
                let mut cluster = TestCluster::default();
                let ctx = cluster.ctx(api.client());
                let gw = gateway_on("gw", port);

                let programmed = reconcile_with(&api, &mut cluster, &ctx, &gw, 1, &[]).await;
                assert_eq!(programmed["status"], "True", "{programmed}");
                assert!(accepts("127.0.0.1"));

                let address = [("IPAddress", ip.as_str())];
                let programmed = reconcile_with(&api, &mut cluster, &ctx, &gw, 2, &address).await;
                assert_eq!(programmed["status"], "True", "{programmed}");
                assert!(accepts(&ip));
                assert!(!accepts("127.0.0.1"), "the wildcard is closed");
            },
        );
    }

    /// And back. The wildcard bind failed against the specific socket,
    /// still open until its accept loop saw the cancel, and the port stayed
    /// dark until a later pass.
    #[test]
    fn a_listener_leaves_an_address_for_the_wildcard_in_one_pass() {
        alone(
            "a_listener_leaves_an_address_for_the_wildcard_in_one_pass",
            async {
                // Accepting a probe builds a backend pool: rustls needs its provider.
                let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
                let Some(ip) = local_ipv4() else {
                    return; // no address off loopback to name
                };
                let port = free_port();
                let accepts = |addr: &str| std::net::TcpStream::connect((addr, port)).is_ok();
                let api = MockApi::default();
                let mut cluster = TestCluster::default();
                let ctx = cluster.ctx(api.client());
                let gw = gateway_on("gw", port);

                let address = [("IPAddress", ip.as_str())];
                let programmed = reconcile_with(&api, &mut cluster, &ctx, &gw, 1, &address).await;
                assert_eq!(programmed["status"], "True", "{programmed}");
                assert!(!accepts("127.0.0.1"));

                let programmed = reconcile_with(&api, &mut cluster, &ctx, &gw, 2, &[]).await;
                assert_eq!(programmed["status"], "True", "{programmed}");
                assert!(accepts("127.0.0.1"), "the wildcard is bound in this pass");
            },
        );
    }

    const NAD_UDN_A: &str =
        "/apis/k8s.cni.cncf.io/v1/namespaces/default/network-attachment-definitions/udn-a";

    /// NetworkAttachmentDefinition udn-a in `default`, on `subnet`.
    fn nad_udn_a(subnet: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "k8s.cni.cncf.io/v1",
            "kind": "NetworkAttachmentDefinition",
            "metadata": {"name": "udn-a", "namespace": "default"},
            "spec": {
                "config": serde_json::json!({"cniVersion": "0.3.1", "subnets": subnet}).to_string(),
            },
        })
    }

    /// A NetworkAttachmentDefinition that cannot be read keeps its network's
    /// last IP. An apiserver blip used to close every listener of the
    /// Gateway.
    #[test]
    fn a_failed_network_read_keeps_the_last_ip() {
        alone("a_failed_network_read_keeps_the_last_ip", async {
            // Accepting a probe builds a backend pool: rustls needs its provider.
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            let Some(ip) = local_ipv4() else {
                return; // no address off loopback to name
            };
            let port = free_port();
            let accepts = |addr: &str| std::net::TcpStream::connect((addr, port)).is_ok();
            let api = MockApi::default();
            api.reply_to(NAD_UDN_A, 200, nad_udn_a(&format!("{ip}/32")));
            let mut cluster = TestCluster::default();
            let ctx = cluster.ctx(api.client());
            let gw = gateway_on("gw", port);
            let network = [(NETWORK_ADDRESS_TYPE, "udn-a")];

            let programmed = reconcile_with(&api, &mut cluster, &ctx, &gw, 1, &network).await;
            assert_eq!(programmed["status"], "True", "{programmed}");
            assert!(accepts(&ip));

            api.reply_to(
                NAD_UDN_A,
                500,
                serde_json::json!({
                    "kind": "Status", "apiVersion": "v1", "status": "Failure",
                    "reason": "InternalError", "code": 500,
                    "message": "etcdserver: request timed out",
                }),
            );
            let programmed = reconcile_with(&api, &mut cluster, &ctx, &gw, 2, &network).await;
            assert_eq!(programmed["status"], "True", "{programmed}");
            assert!(accepts(&ip), "still bound");
        });
    }

    /// Unscoped mode: a network with no IP on the pod unbinds its own
    /// Gateway's listeners, no other Gateway's, and never falls back to
    /// every interface. One such name closed every listener of the process.
    #[test]
    fn an_absent_network_unbinds_only_its_gateway() {
        alone("an_absent_network_unbinds_only_its_gateway", async {
            // Accepting a probe builds a backend pool: rustls needs its provider.
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            let port_a = free_port();
            let port_b = std::iter::repeat_with(free_port)
                .find(|&port| port != port_a)
                .unwrap();
            let accepts = |port: u16| std::net::TcpStream::connect(("127.0.0.1", port)).is_ok();
            let api = MockApi::default();
            // TEST-NET-1: no pod has an address there.
            api.reply_to(NAD_UDN_A, 200, nad_udn_a("192.0.2.0/24"));
            let mut cluster = TestCluster::default();
            let ctx = cluster.ctx(api.client());
            let network = [(NETWORK_ADDRESS_TYPE, "udn-a")];
            let gw_a = with_addresses(gateway_on("a", port_a), 1, &network);
            let gw_b = gateway_on("b", port_b);
            apply(&mut cluster.gateways, gw_a.clone());
            apply(&mut cluster.gateways, gw_b.clone());

            reconcile(Arc::new(gw_b), ctx.clone()).await.unwrap();
            assert!(accepts(port_b));
            reconcile(Arc::new(gw_a), ctx).await.unwrap();
            assert!(accepts(port_b), "the other Gateway keeps its listener");
            assert!(
                !accepts(port_a),
                "bound nowhere, every interface least of all"
            );
            let patch = api.status_patch("gateways", "a").unwrap();
            let conditions = patch["status"]["conditions"].as_array().unwrap();
            let programmed = conditions
                .iter()
                .find(|c| c["type"] == "Programmed")
                .unwrap();
            assert_eq!(programmed["reason"], "AddressNotUsable");
        });
    }

    fn endpoint_slice_lists(api: &MockApi) -> usize {
        api.requests()
            .iter()
            .filter(|r| r.method == "GET" && r.path.contains("/endpointslices"))
            .count()
    }

    /// Services no route of this Gateway references cost nothing. A headless
    /// one with a named targetPort cost an EndpointSlice LIST per pass, and
    /// any change to one rebuilt the table and rewrote status, in every pod.
    #[tokio::test]
    async fn unreferenced_services_cost_nothing() {
        let api = MockApi::default();
        let mut cluster = TestCluster::default();
        let gw = testkit::gateway("gw", 1);
        apply(&mut cluster.gateways, gw.clone());
        let named = serde_json::json!("http");
        apply(
            &mut cluster.services,
            testkit::service("elsewhere", "web", 80, named),
        );
        let ctx = cluster.ctx(api.client());

        reconcile(Arc::new(gw.clone()), ctx.clone()).await.unwrap();
        assert_eq!(endpoint_slice_lists(&api), 0);
        let gw = testkit::with_gateway_status(gw, &api.status_patch("gateways", "gw").unwrap());
        apply(&mut cluster.gateways, gw.clone());
        let built = ctx.published.load_full().unwrap();

        let mut changed = testkit::service("elsewhere", "web", 80, serde_json::json!(8080));
        changed.spec.as_mut().unwrap().ports.as_mut().unwrap()[0].app_protocol =
            Some("https".into());
        apply(&mut cluster.services, changed);
        api.clear();
        reconcile(Arc::new(gw), ctx.clone()).await.unwrap();
        assert!(
            Arc::ptr_eq(&built, &ctx.published.load_full().unwrap()),
            "an unrelated Service rebuilt the table"
        );
        assert!(api.requests().is_empty(), "{:?}", api.requests());
    }

    /// A backendRef this route may not use reads nothing about its Service:
    /// without a ReferenceGrant, the other namespace is not this Gateway's
    /// to list.
    #[tokio::test]
    async fn refused_backend_reads_no_endpoint_slices() {
        let api = MockApi::default();
        let mut cluster = TestCluster::default();
        let gw = testkit::gateway("gw", 1);
        apply(&mut cluster.gateways, gw.clone());
        let mut svc = testkit::service("db", "web", 80, serde_json::json!("http"));
        svc.metadata.namespace = Some("other".into());
        apply(&mut cluster.services, svc);
        let mut route = testkit::http_route("r", &["gw"], 1);
        route.spec.rules = Some(vec![serde_json::from_value(serde_json::json!({
            "backendRefs": [{"name": "db", "namespace": "other", "port": 80}],
        }))
        .unwrap()]);
        apply(&mut cluster.http_routes, route);
        let ctx = cluster.ctx(api.client());

        reconcile(Arc::new(gw), ctx).await.unwrap();
        let patch = api.status_patch("httproutes", "r").unwrap();
        assert_eq!(
            patch["status"]["parents"][0]["conditions"][1]["reason"],
            "RefNotPermitted"
        );
        assert_eq!(endpoint_slice_lists(&api), 0);
    }

    /// A referenced headless backend with a named targetPort connects on
    /// the number its EndpointSlices publish.
    #[tokio::test]
    async fn referenced_named_target_port_is_resolved() {
        let api = MockApi::default();
        api.reply_to(
            "/apis/discovery.k8s.io/v1/namespaces/default/endpointslices",
            200,
            serde_json::json!({
                "apiVersion": "discovery.k8s.io/v1",
                "kind": "EndpointSliceList",
                "metadata": {},
                "items": [{
                    "metadata": {"name": "db-1"},
                    "addressType": "IPv4",
                    "endpoints": [],
                    "ports": [{"name": "web", "port": 8080}],
                }],
            }),
        );
        let mut cluster = TestCluster::default();
        let gw = testkit::gateway("gw", 1);
        apply(&mut cluster.gateways, gw.clone());
        apply(
            &mut cluster.services,
            testkit::service("db", "web", 80, serde_json::json!("http")),
        );
        apply(
            &mut cluster.http_routes,
            testkit::with_backend(testkit::http_route("r", &["gw"], 1), "db", 80),
        );
        let ctx = cluster.ctx(api.client());

        reconcile(Arc::new(gw), ctx.clone()).await.unwrap();
        let config = ctx.published.load_full().unwrap().config.clone();
        assert_eq!(
            config
                .headless_target_ports
                .get(&("db.default.svc".to_string(), 80)),
            Some(&8080)
        );
        assert_eq!(endpoint_slice_lists(&api), 1);
    }

    /// A named targetPort is read once per Service version and DNS epoch;
    /// every pass used to list EndpointSlices again.
    #[tokio::test]
    async fn named_target_port_reads_wait_for_a_change() {
        let api = MockApi::default();
        api.reply_to(
            "/apis/discovery.k8s.io/v1/namespaces/default/endpointslices",
            200,
            serde_json::json!({
                "apiVersion": "discovery.k8s.io/v1",
                "kind": "EndpointSliceList",
                "metadata": {},
                "items": [{
                    "metadata": {"name": "db-1"},
                    "addressType": "IPv4",
                    "endpoints": [],
                    "ports": [{"name": "web", "port": 8080}],
                }],
            }),
        );
        let mut cluster = TestCluster::default();
        let gw = testkit::gateway("gw", 1);
        apply(&mut cluster.gateways, gw.clone());
        let svc = testkit::service("db", "web", 80, serde_json::json!("http"));
        apply(&mut cluster.services, svc.clone());
        apply(
            &mut cluster.http_routes,
            testkit::with_backend(testkit::http_route("r", &["gw"], 1), "db", 80),
        );
        let ctx = cluster.ctx(api.client());
        assert!(
            !ctx.named_ports.dns_changed(),
            "nothing cached: a DNS change is not worth a reconcile"
        );

        reconcile(Arc::new(gw.clone()), ctx.clone()).await.unwrap();
        assert_eq!(endpoint_slice_lists(&api), 1);

        api.clear();
        reconcile(Arc::new(gw.clone()), ctx.clone()).await.unwrap();
        assert_eq!(endpoint_slice_lists(&api), 0, "nothing moved");

        let mut changed = svc;
        changed.metadata.resource_version = Some("2".into());
        apply(&mut cluster.services, changed);
        api.clear();
        reconcile(Arc::new(gw.clone()), ctx.clone()).await.unwrap();
        assert_eq!(endpoint_slice_lists(&api), 1, "the Service changed");

        assert!(ctx.named_ports.dns_changed());
        api.clear();
        reconcile(Arc::new(gw), ctx).await.unwrap();
        assert_eq!(endpoint_slice_lists(&api), 1, "the pods changed");
    }

    /// With DNS refresh off, a read taken before the Service had pods kept
    /// the service port forever. It now expires at the resync interval.
    #[tokio::test]
    async fn named_target_port_read_expires_at_the_resync_interval() {
        let slices = |ports: serde_json::Value| {
            serde_json::json!({
                "apiVersion": "discovery.k8s.io/v1",
                "kind": "EndpointSliceList",
                "metadata": {},
                "items": [{
                    "metadata": {"name": "db-1"},
                    "addressType": "IPv4",
                    "endpoints": [],
                    "ports": ports,
                }],
            })
        };
        const SLICES: &str = "/apis/discovery.k8s.io/v1/namespaces/default/endpointslices";
        let api = MockApi::default();
        api.reply_to(SLICES, 200, slices(serde_json::json!([])));
        let mut cluster = TestCluster::default();
        let gw = testkit::gateway("gw", 1);
        apply(&mut cluster.gateways, gw.clone());
        apply(
            &mut cluster.services,
            testkit::service("db", "web", 80, serde_json::json!("http")),
        );
        apply(
            &mut cluster.http_routes,
            testkit::with_backend(testkit::http_route("r", &["gw"], 1), "db", 80),
        );
        let ctx = cluster.ctx(api.client());
        let target_port = |ctx: &ControllerCtx| {
            ctx.published
                .load_full()
                .unwrap()
                .config
                .headless_target_ports[&("db.default.svc".to_string(), 80)]
        };

        reconcile(Arc::new(gw.clone()), ctx.clone()).await.unwrap();
        assert_eq!(target_port(&ctx), 80, "no pods yet: the service port");

        api.reply_to(
            SLICES,
            200,
            slices(serde_json::json!([{"name": "web", "port": 8080}])),
        );
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(super::super::RESYNC_SECS - 1)).await;
        api.clear();
        reconcile(Arc::new(gw.clone()), ctx.clone()).await.unwrap();
        assert_eq!(endpoint_slice_lists(&api), 0, "still fresh");

        tokio::time::advance(Duration::from_secs(1)).await;
        reconcile(Arc::new(gw), ctx.clone()).await.unwrap();
        assert_eq!(endpoint_slice_lists(&api), 1, "expired");
        assert_eq!(target_port(&ctx), 8080);
    }

    /// Unscoped mode: deleting the last Gateway closes its listener and
    /// empties the table. Deletion ran no reconcile, so they served on.
    #[tokio::test]
    async fn deleted_gateway_stops_serving() {
        let port = std::net::TcpListener::bind("0.0.0.0:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let api = MockApi::default();
        let mut cluster = TestCluster::default();
        let mut gw = testkit::gateway("gw", 1);
        gw.spec.listeners[0].port = port.into();
        apply(&mut cluster.gateways, gw.clone());
        let ctx = cluster.ctx(api.client());
        reconcile(Arc::new(gw.clone()), ctx.clone()).await.unwrap();
        assert!(std::net::TcpListener::bind(("0.0.0.0", port)).is_err());

        cluster
            .gateways
            .apply_watcher_event(&kube::runtime::watcher::Event::Delete(gw));
        prune_deleted_gateways(&ctx).await;
        assert!(ctx
            .published
            .load_full()
            .unwrap()
            .config
            .gateway
            .listeners
            .is_empty());
        // The accept loop drops its socket once it sees the cancel.
        let released = tokio::time::timeout(Duration::from_secs(5), async {
            while std::net::TcpListener::bind(("0.0.0.0", port)).is_err() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(released.is_ok(), "the deleted Gateway's port is released");
    }

    /// A TCP listener carries the oldest route; the newer one is Conflicted
    /// yet still attached, and attachedRoutes counts both. It counted only
    /// accepted routes.
    #[tokio::test]
    async fn attached_routes_count_conflicted_routes() {
        let api = MockApi::default();
        let mut cluster = TestCluster::default();
        let mut gw = testkit::gateway("gw", 1);
        gw.spec.listeners[0].protocol = "TCP".into();
        apply(&mut cluster.gateways, gw.clone());
        for (name, created) in [
            ("old", "2020-01-01T00:00:00Z"),
            ("new", "2021-01-01T00:00:00Z"),
        ] {
            let route: gateway_api::experimental::tcproutes::TCPRoute =
                serde_json::from_value(serde_json::json!({
                    "apiVersion": "gateway.networking.k8s.io/v1alpha2",
                    "kind": "TCPRoute",
                    "metadata": {
                        "name": name, "namespace": "default", "generation": 1,
                        "resourceVersion": "1", "creationTimestamp": created,
                    },
                    "spec": {
                        "parentRefs": [{"name": "gw"}],
                        "rules": [{"backendRefs": [{"name": "svc", "port": 80}]}],
                    },
                }))
                .unwrap();
            apply(&mut cluster.tcp_routes, route);
        }
        let ctx = cluster.ctx(api.client());

        reconcile(Arc::new(gw), ctx).await.unwrap();
        let newer = api.status_patch("tcproutes", "new").unwrap();
        assert_eq!(
            newer["status"]["parents"][0]["conditions"][0]["reason"],
            "Conflicted"
        );
        let gateway = api.status_patch("gateways", "gw").unwrap();
        assert_eq!(gateway["status"]["listeners"][0]["attachedRoutes"], 2);
    }

    /// A write computed from a stale read is refused (409), not applied
    /// over a concurrent writer's entry; the pass retries soon.
    #[tokio::test]
    async fn stale_route_read_retries_instead_of_clobbering() {
        let api = MockApi::default();
        api.reply_to(
            "/apis/gateway.networking.k8s.io/v1/namespaces/default/httproutes/r/status",
            409,
            serde_json::json!({
                "kind": "Status", "apiVersion": "v1", "status": "Failure",
                "reason": "Conflict", "code": 409, "message": "object was modified",
            }),
        );
        let mut cluster = TestCluster::default();
        let gw = testkit::gateway("gw", 1);
        apply(&mut cluster.gateways, gw.clone());
        apply(
            &mut cluster.http_routes,
            testkit::http_route("r", &["gw"], 1),
        );
        let ctx = cluster.ctx(api.client());

        let action = reconcile(Arc::new(gw), ctx).await.unwrap();
        assert_eq!(action, Action::requeue(Duration::from_secs(10)));
    }

    fn lc(port: u16, target_port: Option<u16>) -> ListenerConfig {
        ListenerConfig {
            name: "l".to_string(),
            protocol: Protocol::HTTP,
            port,
            target_port,
            hostname: None,
            address: None,
            interface: None,
            tls: None,
            rate_limit: None,
        }
    }

    /// Unscoped mode must only claim Gateways whose class names this
    /// controller; an absent class is not ours to claim either.
    #[test]
    fn gateway_class_ownership_filter() {
        use gateway_api::gatewayclasses::{GatewayClass, GatewayClassSpec};
        let gc = |name: &str, controller: &str| {
            Arc::new(GatewayClass {
                metadata: kube::core::ObjectMeta {
                    name: Some(name.into()),
                    ..Default::default()
                },
                spec: GatewayClassSpec {
                    controller_name: controller.into(),
                    ..Default::default()
                },
                status: None,
            })
        };
        let classes = vec![
            gc("portail", "epheo.eu/portail"),
            gc("other", "example.com/other"),
        ];
        assert!(gateway_class_is_ours(
            &classes,
            "portail",
            "epheo.eu/portail"
        ));
        assert!(!gateway_class_is_ours(
            &classes,
            "other",
            "epheo.eu/portail"
        ));
        assert!(!gateway_class_is_ours(
            &classes,
            "missing",
            "epheo.eu/portail"
        ));
    }

    fn gwl(name: &str) -> gateway_api::gateways::GatewayListeners {
        gateway_api::gateways::GatewayListeners {
            name: name.to_string(),
            port: 80,
            protocol: "HTTP".to_string(),
            hostname: None,
            tls: None,
            allowed_routes: None,
        }
    }

    fn accepted_ra(name: &str, section: Option<&str>, listeners: &[&str]) -> RouteAcceptance {
        RouteAcceptance {
            name: name.to_string(),
            namespace: "default".to_string(),
            kind: "HTTPRoute",
            accepted: true,
            accepted_reason: "Accepted",
            message: "Accepted".to_string(),
            refs_resolved: true,
            refs_reason: "ResolvedRefs",
            refs_message: "ok".to_string(),
            generation: None,
            section_name: section.map(String::from),
            port: None,
            listener_names: listeners.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// attachedRoutes counts routes, not parentRefs: two refs of one route
    /// landing on the same listener count once (the old per-entry sum
    /// double-counted every multi-parentRef route).
    #[test]
    fn attached_routes_dedupes_parent_refs_per_route() {
        let listeners = [gwl("http"), gwl("https")];
        let status = [
            accepted_ra("a", Some("http"), &["http"]),
            accepted_ra("a", None, &["http", "https"]),
            accepted_ra("b", None, &["https"]),
            {
                let mut rejected = accepted_ra("c", None, &[]);
                rejected.accepted = false;
                rejected
            },
            {
                // Attached by allowedRoutes and parentRef, then rejected.
                let mut conflicted = accepted_ra("d", None, &["https"]);
                conflicted.accepted = false;
                conflicted
            },
        ];
        let counts = count_attached_routes(&listeners, &status);
        assert_eq!(counts["http"], 1, "route a counted once on http");
        assert_eq!(counts["https"], 3, "routes a, b and the rejected d");
    }

    #[test]
    fn required_endpoints_keys_on_bound_port() {
        let listeners = [lc(80, Some(8000)), lc(8080, None)];
        let eps = required_endpoints(&listeners, &[]);
        assert!(eps.contains(&(None, 8000)));
        assert!(eps.contains(&(None, 8080)));
        assert!(
            !eps.contains(&(None, 80)),
            "published port must not be the readiness key"
        );
    }

    #[test]
    fn required_endpoints_products_bind_addresses() {
        let listeners = [lc(443, Some(8001))];
        let eps = required_endpoints(
            &listeners,
            &["10.0.0.1".to_string(), "10.0.0.2".to_string()],
        );
        assert_eq!(eps.len(), 2);
        assert!(eps.contains(&(Some("10.0.0.1".to_string()), 8001)));
        assert!(eps.contains(&(Some("10.0.0.2".to_string()), 8001)));
    }

    /// The skip gate is only sound if the fingerprint is deterministic and
    /// moves with everything the data plane applies, serde-skipped fields
    /// and the bind plan included.
    #[test]
    fn fingerprint_deterministic_and_sensitive() {
        let config = crate::config::PortailConfig::default();
        let bind = vec!["10.0.0.1".to_string()];
        let fp = |c: &crate::config::PortailConfig, bind: Option<&[String]>| {
            let served = Served {
                config: Arc::new(c.clone()),
                bind: bind.map(<[String]>::to_vec),
            };
            data_plane_fingerprint(c, &served.binds())
        };

        assert_eq!(fp(&config, Some(&bind)), fp(&config, Some(&bind)));

        // Wildcard, specific address and binding nothing are three plans.
        assert_ne!(fp(&config, Some(&bind)), fp(&config, Some(&[])));
        assert_ne!(fp(&config, Some(&[])), fp(&config, None));

        // Override maps are serde-skipped but change what the table resolves.
        let mut headless = config.clone();
        headless
            .headless_target_ports
            .insert(("db.prod.svc".into(), 5432), 15432);
        assert_ne!(fp(&config, None), fp(&headless, None));

        // So are the cert bytes a hot-reload must pick up.
        let with_cert = |pem: &[u8]| {
            let mut c = config.clone();
            c.gateway.listeners[0].tls = Some(crate::config::TlsConfig {
                mode: crate::config::TlsMode::Terminate,
                certificate_refs: vec![crate::config::CertificateRef {
                    name: "cert".into(),
                    hostname: None,
                    cert_pem: Some(pem.to_vec()),
                    key_pem: Some(b"K".to_vec()),
                }],
            });
            c
        };
        assert_ne!(fp(&with_cert(b"A"), None), fp(&with_cert(b"B"), None));
    }

    /// `config` served on every interface.
    fn wildcard(config: crate::config::PortailConfig) -> Served {
        Served {
            config: Arc::new(config),
            bind: Some(vec![]),
        }
    }

    /// Unscoped mode: merging two Gateways' configs must keep every route,
    /// and prefix listener and parentRef names per Gateway so each route
    /// stays on its own Gateway's listeners only, never the other's.
    #[test]
    fn merge_gateway_configs_isolates_gateways() {
        use crate::config::{
            GatewayConfig, HttpRouteConfig, ListenerConfig, ParentRef, PortailConfig,
            Protocol as CfgProtocol,
        };

        fn gw_config(gw: &str, listener: &str, port: u16, host: &str) -> PortailConfig {
            PortailConfig {
                gateway: GatewayConfig {
                    name: gw.to_string(),
                    listeners: vec![ListenerConfig {
                        name: listener.to_string(),
                        protocol: CfgProtocol::HTTP,
                        port,
                        target_port: None,
                        hostname: None,
                        address: None,
                        interface: None,
                        tls: None,
                        rate_limit: None,
                    }],
                    addresses: vec![],
                },
                http_routes: vec![HttpRouteConfig {
                    // As the reconciler attaches: one ref per accepted listener.
                    parent_refs: vec![ParentRef {
                        name: gw.to_string(),
                        section_name: Some(listener.to_string()),
                        port: None,
                    }],
                    hostnames: vec![host.to_string()],
                    rules: vec![],
                }],
                ..Default::default()
            }
        }

        let mut map = HashMap::new();
        // Both Gateways use the SAME listener name "http" — the cross-match trap.
        map.insert(
            ("ns-a".to_string(), "gw-a".to_string()),
            wildcard(gw_config("gw-a", "http", 8080, "a.example.com")),
        );
        map.insert(
            ("ns-b".to_string(), "gw-b".to_string()),
            wildcard(gw_config("gw-b", "http", 9090, "b.example.com")),
        );

        let merged = merge_gateway_configs(&map);

        // Both Gateways' listeners survive, disambiguated by prefix.
        let names: Vec<&str> = merged
            .gateway
            .listeners
            .iter()
            .map(|l| l.name.as_str())
            .collect();
        assert_eq!(names, vec!["ns-a/gw-a/http", "ns-b/gw-b/http"]);

        // Both routes survive; each parentRef names exactly its own
        // Gateway's (prefixed) listener.
        assert_eq!(merged.http_routes.len(), 2);
        let route_a = merged
            .http_routes
            .iter()
            .find(|r| r.hostnames == ["a.example.com"])
            .unwrap();
        assert_eq!(route_a.parent_refs.len(), 1);
        assert_eq!(
            route_a.parent_refs[0].section_name.as_deref(),
            Some("ns-a/gw-a/http"),
        );
        let route_b = merged
            .http_routes
            .iter()
            .find(|r| r.hostnames == ["b.example.com"])
            .unwrap();
        assert_eq!(
            route_b.parent_refs[0].section_name.as_deref(),
            Some("ns-b/gw-b/http"),
        );
    }

    /// Unscoped mode, two Gateways sharing port 443: the data plane must be
    /// ensured with the MERGED listeners so the per-port cert merge sees both
    /// Gateways' refs. Passing one Gateway's listeners at a time let each
    /// reconcile rebuild the port's ServerConfig with only its own certs,
    /// clobbering the sibling's SNI entries — and the fingerprint gate then
    /// froze whichever wrote last (v0.1.13 regression, two host-network
    /// Gateways on one node).
    #[test]
    fn merged_listeners_carry_every_gateways_certs_per_port() {
        use crate::config::{
            CertificateRef, GatewayConfig, ListenerConfig, PortailConfig, Protocol as CfgProtocol,
            TlsConfig, TlsMode,
        };

        fn tls_gw(gw: &str, cert: &str, host: &str) -> PortailConfig {
            PortailConfig {
                gateway: GatewayConfig {
                    name: gw.to_string(),
                    listeners: vec![ListenerConfig {
                        name: "https".to_string(),
                        protocol: CfgProtocol::HTTPS,
                        port: 443,
                        target_port: None,
                        hostname: Some(host.to_string()),
                        address: None,
                        interface: None,
                        tls: Some(TlsConfig {
                            mode: TlsMode::Terminate,
                            certificate_refs: vec![CertificateRef {
                                name: cert.to_string(),
                                hostname: Some(host.to_string()),
                                cert_pem: Some(b"C".to_vec()),
                                key_pem: Some(b"K".to_vec()),
                            }],
                        }),
                        rate_limit: None,
                    }],
                    addresses: vec![],
                },
                ..Default::default()
            }
        }

        let mut map = HashMap::new();
        map.insert(
            ("ns-a".to_string(), "gw-a".to_string()),
            wildcard(tls_gw("gw-a", "cert-a", "a.example.com")),
        );
        map.insert(
            ("ns-b".to_string(), "gw-b".to_string()),
            wildcard(tls_gw("gw-b", "cert-b", "b.example.com")),
        );

        let merged = merge_gateway_configs(&map);
        let by_port = crate::proxy::data_plane::merge_cert_refs_by_port(&merged.gateway.listeners);
        let mut names: Vec<&str> = by_port[&443].iter().map(|r| r.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["cert-a", "cert-b"],
            "the shared port's ServerConfig inputs must include every Gateway's refs"
        );
    }
}
