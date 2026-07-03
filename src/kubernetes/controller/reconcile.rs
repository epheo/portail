//! The reconcile pass: snapshot the reflector caches, build this Gateway's
//! config, ensure data-plane listeners, fingerprint-gate the apply, and write
//! Gateway/route status. Pure helpers (fingerprint, endpoint math, legacy
//! multi-Gateway merge) live here beside the orchestrator that calls them.

use kube::runtime::controller::Action;
use kube::runtime::reflector::{ObjectRef, Store};
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
use super::{skip_requeue_secs, success_requeue_secs, ControllerCtx, ReconcileError};
use crate::kubernetes::addresses::{
    compute_bind_addresses, discover_usable_addresses, resolve_network_addresses, UsableAddresses,
};
use crate::kubernetes::reconciler::{reconcile_to_config, ClusterSnapshot, RouteAcceptance};
use crate::kubernetes::services::{resolve_named_target_ports, resolve_services};
use crate::kubernetes::status;

/// Read all objects from a reflector store, cloning each.
fn snapshot<K>(store: &Store<K>) -> Vec<K>
where
    K: kube::Resource + Clone,
    K::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
    store.state().iter().map(|arc| (**arc).clone()).collect()
}

/// Open data-plane TCP listeners for this Gateway's ports. Called after the
/// per-Gateway config is built so ListenerConfig has resolved TLS cert data.
fn ensure_data_plane_listeners(
    ctx: &ControllerCtx,
    listeners: &[crate::config::ListenerConfig],
    bind_addresses: &[String],
) {
    info!(
        "Ensuring data plane listeners for ports: {:?}",
        listeners.iter().map(|l| l.port).collect::<Vec<_>>()
    );
    match ctx.data_plane.lock() {
        Ok(mut dp) => {
            let (opened, errors) = dp.add_tcp_listeners(
                listeners,
                bind_addresses,
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
        Err(e) => warn!("Failed to lock data plane: {}", e),
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
        status::GatewayCondition {
            ok: true,
            reason: "Programmed".into(),
            message: "Programmed".into(),
        }
    } else {
        warn!("Data plane not ready: not all listener ports are bound");
        status::GatewayCondition {
            ok: false,
            reason: "Invalid".into(),
            message: "Data plane not ready: not all listener ports are bound".into(),
        }
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

/// Fingerprint of everything a reconcile pass would apply: the built config
/// (its JSON plus the cert/key bytes and k8s override maps that serde skips),
/// the status conditions about to be written, and the data-plane readiness
/// that feeds Programmed. Hash inputs with non-deterministic iteration order
/// (HashMaps, store-ordered route lists) are canonicalized by sorting.
fn reconcile_fingerprint(
    config: &crate::config::PortailConfig,
    accepted_cond: &status::GatewayCondition,
    listener_statuses: &HashMap<String, status::ListenerStatus>,
    usable: &UsableAddresses,
    route_status: &[RouteAcceptance],
    observed_route_parents: Vec<String>,
    dp_ready: bool,
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
    h.write(format!("{:?}", accepted_cond).as_bytes());
    write_sorted(
        &mut h,
        listener_statuses
            .iter()
            .map(|(name, ls)| format!("{}{:?}", name, ls))
            .collect(),
    );
    write_sorted(&mut h, usable.interface_ips.clone());
    write_sorted(
        &mut h,
        usable
            .network_ips
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect(),
    );
    write_sorted(
        &mut h,
        route_status.iter().map(|ra| format!("{:?}", ra)).collect(),
    );
    // Observed route status: `status.parents` is an atomic list written
    // read-modify-write, so a concurrent writer clobbering our entry must
    // change the fingerprint — otherwise the skip path would leave the
    // clobbered status in place until the slow safety-net requeue.
    write_sorted(&mut h, observed_route_parents);
    h.write(&[dp_ready as u8]);
    h.finish()
}

/// Merge per-Gateway configs into one data-plane config (legacy unscoped
/// mode, where a single process serves every Gateway).
///
/// Listener names and route parentRef sectionNames are prefixed with their
/// Gateway's identity so a route can never cross-match a same-named listener
/// on another Gateway; parentRefs without a sectionName are expanded to one
/// ref per listener of *their own* Gateway for the same reason (an unscoped
/// ref would attach to every merged listener).
fn merge_gateway_configs(
    configs: &HashMap<(String, String), Arc<crate::config::PortailConfig>>,
) -> crate::config::PortailConfig {
    use crate::config::{ParentRef, PortailConfig};

    let mut ordered: Vec<_> = configs.iter().collect();
    ordered.sort_by(|a, b| a.0.cmp(b.0));

    let mut merged = PortailConfig::default();
    merged.gateway.name = "portail-merged".to_string();
    merged.gateway.listeners.clear();

    for ((ns, name), cfg) in ordered {
        let prefix = format!("{}/{}/", ns, name);
        let mut c: PortailConfig = (**cfg).clone();

        for l in &mut c.gateway.listeners {
            l.name = format!("{}{}", prefix, l.name);
        }
        // (name, port) pairs of this Gateway's listeners, post-prefixing,
        // used to expand sectionName-less parentRefs.
        let listener_names: Vec<(String, u16)> = c
            .gateway
            .listeners
            .iter()
            .map(|l| (l.name.clone(), l.port))
            .collect();
        let expand = |refs: &mut Vec<ParentRef>| {
            let mut out = Vec::with_capacity(refs.len());
            for pr in refs.drain(..) {
                match &pr.section_name {
                    Some(section) => out.push(ParentRef {
                        section_name: Some(format!("{}{}", prefix, section)),
                        ..pr
                    }),
                    None => {
                        for (lname, lport) in &listener_names {
                            if pr.port.is_none_or(|p| *lport == p as u16) {
                                out.push(ParentRef {
                                    name: pr.name.clone(),
                                    section_name: Some(lname.clone()),
                                    port: pr.port,
                                });
                            }
                        }
                    }
                }
            }
            *refs = out;
        };
        for r in &mut c.http_routes {
            expand(&mut r.parent_refs);
        }
        for r in &mut c.tcp_routes {
            expand(&mut r.parent_refs);
        }
        for r in &mut c.udp_routes {
            expand(&mut r.parent_refs);
        }
        for r in &mut c.tls_routes {
            expand(&mut r.parent_refs);
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

pub(super) async fn reconcile(
    gateway: Arc<Gateway>,
    ctx: Arc<ControllerCtx>,
) -> Result<Action, ReconcileError> {
    let gw_name = gateway.name_any();
    let gw_ns = gateway.namespace().unwrap_or_else(|| "default".to_string());
    debug!("Reconciling Gateway {}/{}", gw_ns, gw_name);

    // GatewayClass acceptance is owned by portail-operator; it only provisions a
    // portail Deployment for Gateways whose class references this controller, so
    // we can trust the scope and skip an in-process class check here.

    // Snapshot reflector caches in one shot — no API server calls.
    let snapshot = ClusterSnapshot {
        http_routes: snapshot(&ctx.cache.http_routes),
        tcp_routes: snapshot(&ctx.cache.tcp_routes),
        tls_routes: snapshot(&ctx.cache.tls_routes),
        udp_routes: snapshot(&ctx.cache.udp_routes),
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
        services: snapshot(&ctx.cache.services),
    };

    let cert_data = resolve_gateway_certs(
        &gateway,
        &gw_ns,
        &snapshot.reference_grants,
        &ctx.cache.secrets,
    );
    let (mut services, named_reqs) = resolve_services(&snapshot.services);
    // A headless Service with a *named* targetPort needs endpoint data the Service
    // spec lacks; resolve those with a one-shot EndpointSlice list (no watch).
    if !named_reqs.is_empty() {
        resolve_named_target_ports(
            &ctx.client,
            &named_reqs,
            &mut services.headless_target_ports,
        )
        .await;
    }

    let (listener_statuses, accepted_cond) =
        validate_gateway(&gateway, &gw_ns, &cert_data, &snapshot.reference_grants);

    let mut usable = discover_usable_addresses();
    resolve_network_addresses(&ctx.client, &gateway, &gw_ns, &mut usable).await;

    // Build per-Gateway config; bail out with a failing Programmed condition on error.
    let mut result = match reconcile_to_config(&gateway, &snapshot, &cert_data, &services) {
        Ok(r) => r,
        Err(e) => {
            error!("Failed to reconcile Gateway {}/{}: {}", gw_ns, gw_name, e);
            let failure = status::GatewayCondition {
                ok: false,
                reason: "Invalid".into(),
                message: format!("Reconciliation failed: {}", e),
            };
            status::update_gateway_status(
                &ctx.client,
                &gateway,
                &accepted_cond,
                &failure,
                &HashMap::new(),
                &listener_statuses,
                &usable,
                ctx.manage_gateway_status,
            )
            .await;
            return Err(ReconcileError(e.to_string()));
        }
    };

    // Substitute resolved IPs for portail.epheo.eu/Network entries (reconcile_to_config
    // copies spec.addresses values as-is).
    if !usable.network_ips.is_empty() {
        result.config.gateway.addresses = result
            .config
            .gateway
            .addresses
            .iter()
            .map(|addr| {
                usable
                    .network_ips
                    .get(addr)
                    .cloned()
                    .unwrap_or_else(|| addr.clone())
            })
            .collect();
    }

    let bind_addresses = compute_bind_addresses(&result.config.gateway.addresses, &usable);
    ensure_data_plane_listeners(&ctx, &result.config.gateway.listeners, &bind_addresses);

    // Readiness keys on each listener's bound port (`target_port` when the
    // fronting Service decouples it, else the published port). Captured before
    // `result.config` is moved into the route-table build below. In the brief
    // window where a LoadBalancer pod has no NET_BIND_SERVICE and its Service is
    // not yet observed, a privileged published bind fails harmlessly and
    // readiness stays down until the Service (and its targetPort) appear.
    let required = required_endpoints(&result.config.gateway.listeners, &bind_addresses);

    // Data-plane readiness is computed before the (expensive) route-table
    // build: it depends only on bound ports, and it feeds both the readiness
    // latch and the reconcile fingerprint below.
    let dp_ready = ctx
        .data_plane
        .lock()
        .map(|dp| dp.is_ready_for_endpoints(&required))
        .unwrap_or(false);
    // Once the data plane has bound this gateway's listener ports, the
    // pod is serving — latch readiness on (never flips back).
    if dp_ready {
        ctx.ready.store(true, std::sync::atomic::Ordering::Release);
    }

    // Track this Gateway's config and derive the config the data plane
    // should actually run. In scoped mode (operator-managed, one Gateway per
    // process) that is simply this Gateway's config; in legacy unscoped mode
    // one process serves EVERY Gateway, so publishing only this Gateway's
    // config would clobber the others' routes in the shared table.
    let config = Arc::new(std::mem::take(&mut result.config));
    let effective_config = {
        let mut map = match ctx.gateway_configs.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        map.insert((gw_ns.clone(), gw_name.clone()), config.clone());
        // Prune Gateways that no longer exist — deletions don't run this
        // reconciler, so stale entries are collected on any later pass.
        let live: HashSet<(String, String)> = ctx
            .cache
            .gateways
            .state()
            .iter()
            .filter_map(|g| Some((g.namespace()?, g.metadata.name.clone()?)))
            .collect();
        map.retain(|k, _| live.contains(k));
        if map.len() <= 1 {
            config.clone()
        } else {
            Arc::new(merge_gateway_configs(&map))
        }
    };

    // Observed route status parents feed the fingerprint (see
    // `reconcile_fingerprint`) so external clobbers of the atomic
    // `status.parents` list are detected and re-applied.
    let observed_route_parents: Vec<String> = result
        .route_status
        .iter()
        .map(|ra| {
            let parents = match ra.kind {
                "HTTPRoute" => get_existing_route_parents_from_store(
                    &ctx.cache.http_routes,
                    &ra.namespace,
                    &ra.name,
                ),
                "TCPRoute" => get_existing_route_parents_from_store(
                    &ctx.cache.tcp_routes,
                    &ra.namespace,
                    &ra.name,
                ),
                "TLSRoute" => get_existing_route_parents_from_store(
                    &ctx.cache.tls_routes,
                    &ra.namespace,
                    &ra.name,
                ),
                "UDPRoute" => get_existing_route_parents_from_store(
                    &ctx.cache.udp_routes,
                    &ra.namespace,
                    &ra.name,
                ),
                _ => Vec::new(),
            };
            format!("{}/{}/{}:{:?}", ra.kind, ra.namespace, ra.name, parents)
        })
        .collect();

    // Short-circuit: when this pass would apply exactly what the last fully
    // successful pass already applied — same config (certs and override maps
    // included), same status conditions, same readiness — skip the route-table
    // rebuild and every status PATCH. This is what makes the watch echo of our
    // own status writes cheap, and turns the permanently-unaccepted-route case
    // from a 5s rebuild/PATCH loop into a slow safety-net re-check.
    let fingerprint = reconcile_fingerprint(
        &effective_config,
        &accepted_cond,
        &listener_statuses,
        &usable,
        &result.route_status,
        observed_route_parents,
        dp_ready,
    );
    if ctx
        .last_applied
        .lock()
        .map(|last| *last == Some(fingerprint))
        .unwrap_or(false)
    {
        let all_routes_accepted = result.route_status.iter().all(|ra| ra.accepted);
        debug!(
            "Gateway {}/{} unchanged since last applied reconcile; skipping rebuild and status writes",
            gw_ns, gw_name
        );
        return Ok(Action::requeue(Duration::from_secs(skip_requeue_secs(
            dp_ready,
            all_routes_accepted,
        ))));
    }

    // Per-listener attached-route counts, restricted to this Gateway's routes.
    let mut listener_route_counts: HashMap<String, i32> = gateway
        .spec
        .listeners
        .iter()
        .map(|l| (l.name.clone(), 0))
        .collect();
    for ra in &result.route_status {
        if ra.accepted {
            for listener_name in &ra.listener_names {
                *listener_route_counts
                    .entry(listener_name.clone())
                    .or_insert(0) += 1;
            }
        }
    }

    // Build + swap the RouteTable; on failure produce a failing Programmed
    // condition (route table build is the last step that can fail).
    let http_count = config.http_routes.len();
    let tcp_count = config.tcp_routes.len();
    let tls_count = config.tls_routes.len();
    let udp_count = config.udp_routes.len();

    let programmed_cond = match build_route_table(effective_config.clone()).await {
        Ok(route_table) => {
            ctx.routes.store(Arc::new(route_table));
            // Publish the config only after the table it produced is live:
            // the background DNS-refresh task re-resolves from this cell, and
            // must never pick up a config whose build failed — nor one whose
            // table hasn't been swapped in yet (the refresh CAS compares
            // against the live table).
            ctx.config_cell.store(Some(effective_config));
            info!(
                "Gateway {}/{} reconciled: {} HTTP, {} TCP, {} TLS, {} UDP routes",
                gw_ns, gw_name, http_count, tcp_count, tls_count, udp_count,
            );
            compute_programmed_condition(dp_ready)
        }
        Err(e) => {
            error!(
                "Failed to build route table for Gateway {}/{}: {}",
                gw_ns, gw_name, e
            );
            status::GatewayCondition {
                ok: false,
                reason: "Invalid".into(),
                message: format!("Route table conversion failed: {}", e),
            }
        }
    };
    let programmed = programmed_cond.ok;

    let mut statuses_ok = status::update_gateway_status(
        &ctx.client,
        &gateway,
        &accepted_cond,
        &programmed_cond,
        &listener_route_counts,
        &listener_statuses,
        &usable,
        ctx.manage_gateway_status,
    )
    .await;

    // Update per-route status — group by (kind, ns, name) so SSA writes all
    // parent entries together (otherwise multi-listener routes lose previous
    // entries on each patch).
    {
        use std::collections::BTreeMap;
        let field_manager = format!("portail-{}-{}", gw_ns, gw_name);
        let mut grouped: BTreeMap<(&str, &str, &str), Vec<status::RouteParentStatus>> =
            BTreeMap::new();
        for ra in &result.route_status {
            let route_programmed = ra.accepted && programmed;
            grouped
                .entry((ra.kind, &ra.namespace, &ra.name))
                .or_default()
                .push(status::RouteParentStatus {
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
                });
        }

        for ((kind, ns, name), parents) in &grouped {
            macro_rules! patch_route_status {
                ($store:expr, $ty:ty) => {{
                    let existing = get_existing_route_parents_from_store(&$store, ns, name);
                    statuses_ok &= status::update_route_status::<$ty>(
                        &ctx.client,
                        name,
                        ns,
                        parents,
                        &field_manager,
                        &existing,
                    )
                    .await;
                }};
            }
            match *kind {
                "HTTPRoute" => patch_route_status!(ctx.cache.http_routes, HTTPRoute),
                "TCPRoute" => patch_route_status!(ctx.cache.tcp_routes, TCPRoute),
                "TLSRoute" => patch_route_status!(ctx.cache.tls_routes, TLSRoute),
                "UDPRoute" => patch_route_status!(ctx.cache.udp_routes, UDPRoute),
                _ => {}
            }
        }
    }

    if !statuses_ok {
        // A status PATCH failed (apiserver blip). Without a fast retry the
        // wrong status would sit in the cluster until the slow safety-net
        // requeue. Leave `last_applied` unset so the next pass re-applies.
        if let Ok(mut last) = ctx.last_applied.lock() {
            *last = None;
        }
        return Ok(Action::requeue(Duration::from_secs(10)));
    }

    // Everything applied: record the fingerprint so identical future passes
    // (watch echoes, unconverged requeues with unchanged inputs) short-circuit.
    if let Ok(mut last) = ctx.last_applied.lock() {
        *last = Some(fingerprint);
    }

    // Requeue cadence is convergence-driven — see `success_requeue_secs`.
    let all_routes_accepted = result.route_status.iter().all(|ra| ra.accepted);
    Ok(Action::requeue(Duration::from_secs(success_requeue_secs(
        programmed,
        all_routes_accepted,
    ))))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ListenerConfig, Protocol};

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
        }
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

    /// Unscoped mode: merging two Gateways' configs must keep every route,
    /// prefix listener names per Gateway, and expand sectionName-less
    /// parentRefs to their own Gateway's listeners only — never the other's.
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
                    }],
                    addresses: vec![],
                },
                http_routes: vec![HttpRouteConfig {
                    // No sectionName: attaches to all of THIS gateway's listeners.
                    parent_refs: vec![ParentRef {
                        name: gw.to_string(),
                        section_name: None,
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
            Arc::new(gw_config("gw-a", "http", 8080, "a.example.com")),
        );
        map.insert(
            ("ns-b".to_string(), "gw-b".to_string()),
            Arc::new(gw_config("gw-b", "http", 9090, "b.example.com")),
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

        // Both routes survive; each parentRef was expanded to exactly its own
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
}
