//! The reconcile pass of one Gateway: translate it, provision its data
//! plane, publish its snapshot to the hub, then write the status that
//! differs from what the store holds, Programmed from its data planes'
//! reports. [`publish`] and [`finish`] are the two halves; the sync
//! protocol puts the data planes' applies between them.

use kube::runtime::controller::Action;
use kube::runtime::reflector::ObjectRef;
use kube::ResourceExt;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use gateway_api::experimental::tcproutes::TCPRoute;
use gateway_api::experimental::tlsroutes::TLSRoute;
use gateway_api::experimental::udproutes::UDPRoute;
use gateway_api::gatewayclasses::GatewayClass;
use gateway_api::gateways::Gateway;
use gateway_api::httproutes::HTTPRoute;

use super::provision::Provisioned;
use super::translate::{translate, Observed, Plan, Rejected, Statuses};
use super::{
    params, programmed, provision, skip_requeue_secs, success_requeue_secs, ControllerCtx,
    ReconcileError,
};
use crate::kubernetes::crds::RateLimitPolicy;
use crate::kubernetes::naming::FIELD_MANAGER;
use crate::kubernetes::reconciler::{PolicyAcceptance, RouteAcceptance};
use crate::kubernetes::status;
use crate::logging::{debug, info};
use crate::sync::{GatewayKey, GatewayRef, Snapshot};

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
    gateway: &GatewayKey,
    route_status: &[RouteAcceptance],
    observed: &[Observed],
    programmed: bool,
    writes: &mut Writes,
) {
    let owner = status::Owner {
        controller: &ctx.controller_name,
        gw_name: &gateway.name,
        gw_ns: &gateway.namespace,
    };
    let mut desired: HashMap<(&str, &str, &str), Vec<serde_json::Value>> = HashMap::new();
    for ra in route_status {
        desired
            .entry((ra.kind, &ra.namespace, &ra.name))
            .or_default()
            .push(status::route_parent_entry(&status::RouteParentStatus {
                controller_name: ctx.controller_name.clone(),
                gateway_name: gateway.name.clone(),
                gateway_namespace: gateway.namespace.clone(),
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
                    FIELD_MANAGER,
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
    gateway: &GatewayKey,
    policy_status: &[PolicyAcceptance],
    observed: &[Observed],
    writes: &mut Writes,
) {
    let owner = status::Owner {
        controller: &ctx.controller_name,
        gw_name: &gateway.name,
        gw_ns: &gateway.namespace,
    };
    for o in observed {
        let entries: Vec<serde_json::Value> = policy_status
            .iter()
            .filter(|pa| pa.namespace == o.namespace && pa.name == o.name)
            .map(|pa| {
                status::policy_ancestor_entry(
                    &ctx.controller_name,
                    &gateway.name,
                    &gateway.namespace,
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
                FIELD_MANAGER,
            )
            .await,
        );
    }
}

/// Whose a Gateway is, by its class.
#[derive(Debug, PartialEq, Eq)]
enum ClassOwner {
    Ours,
    /// The class names another controller: the Gateway is not ours to
    /// serve, write status for, or provision.
    Other,
    /// No such class: nothing changes until it returns, so a deleted class
    /// never takes its Gateways down.
    Missing,
}

fn class_owner(
    classes: &[Arc<GatewayClass>],
    class_name: &str,
    controller_name: &str,
) -> ClassOwner {
    match classes
        .iter()
        .find(|gc| gc.metadata.name.as_deref() == Some(class_name))
    {
        Some(gc) if gc.spec.controller_name == controller_name => ClassOwner::Ours,
        Some(_) => ClassOwner::Other,
        None => ClassOwner::Missing,
    }
}

/// Stop serving the Gateways gone from the store and forget what was kept
/// for them. Deletion runs no reconcile; a relist may also have dropped
/// Gateways deleted while the watch was down.
pub(super) fn forget_deleted_gateways(ctx: &ControllerCtx) {
    let gateways = ctx.cache.gateways.state();
    let live: HashSet<GatewayKey> = gateways
        .iter()
        .filter_map(|g| Some(GatewayKey::new(g.namespace()?, g.metadata.name.clone()?)))
        .collect();
    ctx.hub.retain(|key| live.contains(key));
    ctx.reports.retain(|key| live.contains(key));
    ctx.route_verdicts
        .retain(|(ns, name)| live.contains(&GatewayKey::new(ns, name)));
    ctx.networks.retain_named(&gateways);
}

/// What [`publish`] leaves for [`finish`].
pub(super) enum Step {
    /// Nothing more this pass: not the leader, or not our Gateway.
    Done(Action),
    Continue(Box<Pass>),
}

pub(super) struct Pass {
    key: GatewayKey,
    /// The translation, or why there is none.
    plan: Result<Plan, Box<Rejected>>,
    provisioned: Provisioned,
    /// Why the Gateway's parameters cannot be used: nothing was published
    /// or provisioned.
    invalid_parameters: Option<String>,
    /// The hub's version moved.
    published: bool,
}

/// The first half of a pass: translate, provision, publish.
pub(super) async fn publish(gateway: &Gateway, ctx: &ControllerCtx) -> Step {
    let key = GatewayKey::new(
        gateway.namespace().unwrap_or_else(|| "default".to_string()),
        gateway.name_any(),
    );
    debug!("Reconciling Gateway {}", key);
    if !ctx.leader.is_leader() {
        return Step::Done(Action::await_change());
    }
    match class_owner(
        &ctx.cache.gateway_classes.state(),
        &gateway.spec.gateway_class_name,
        &ctx.controller_name,
    ) {
        ClassOwner::Ours => {}
        ClassOwner::Other => {
            if ctx.hub.remove(&key) {
                info!(
                    "Gateway {} class '{}' names another controller; no longer served",
                    key, gateway.spec.gateway_class_name
                );
            }
            provision::release(ctx, gateway).await;
            return Step::Done(Action::await_change());
        }
        ClassOwner::Missing => {
            debug!(
                "Gateway {} class '{}' not found; left as it is",
                key, gateway.spec.gateway_class_name
            );
            return Step::Done(Action::await_change());
        }
    }

    let params = params::resolve(ctx, gateway);
    let mut plan = translate(ctx, gateway).await;
    let (provisioned, published) = match (&params, &mut plan) {
        (Ok(params), Ok(plan)) => {
            let provisioned = provision::reconcile(ctx, gateway, params, plan).await;
            provisioned.stamp(&mut plan.config);
            let snapshot = Snapshot::from_config(
                GatewayRef {
                    namespace: key.namespace.clone(),
                    name: key.name.clone(),
                    uid: gateway.metadata.uid.clone().unwrap_or_default(),
                    generation: gateway.metadata.generation.unwrap_or_default(),
                },
                std::mem::take(&mut plan.config),
                std::mem::take(&mut plan.networks),
            );
            (provisioned, ctx.hub.publish(snapshot))
        }
        _ => (Provisioned::unmanaged(), false),
    };
    // A delete in the meantime already ran its forget: a slot published
    // now would serve a deleted Gateway until the next delete event.
    let deleted = ctx
        .cache
        .gateways
        .get(&ObjectRef::new(&key.name).within(&key.namespace))
        .is_none();
    if deleted {
        ctx.hub.remove(&key);
        return Step::Done(Action::await_change());
    }
    Step::Continue(Box::new(Pass {
        key,
        plan,
        provisioned,
        invalid_parameters: params.err(),
        published,
    }))
}

/// The second half of a pass: status, Programmed from the reports of the
/// data planes serving the hub's snapshot.
pub(super) async fn finish(
    gateway: &Gateway,
    ctx: &ControllerCtx,
    pass: Pass,
) -> Result<Action, ReconcileError> {
    let Pass {
        key,
        plan,
        provisioned,
        invalid_parameters,
        published,
    } = pass;
    let (mut statuses, error): (Statuses, Option<String>) = match plan {
        Ok(plan) => (plan.statuses, None),
        Err(rejected) => (rejected.statuses, Some(rejected.error)),
    };
    if let Some(message) = invalid_parameters {
        statuses.accepted = status::GatewayCondition {
            ok: false,
            reason: "InvalidParameters".into(),
            message,
        };
    }

    let served = ctx.hub.current(&key);
    let reports = ctx.reports.live(&key);
    let mut programmed = programmed::evaluate(
        gateway,
        &key,
        &ctx.hub,
        served.as_deref(),
        &reports,
        &mut statuses.listeners,
    );
    if let Some(e) = &error {
        programmed.condition =
            status::GatewayCondition::invalid(format!("Reconciliation failed: {}", e));
    } else if let Some(problem) = &provisioned.problem {
        programmed.condition = status::GatewayCondition::invalid(problem.clone());
    }
    let is_programmed = programmed.condition.ok;

    // Status is compared with what the store holds, object by object, so a
    // spec bump, a recreated Gateway or a wiped status is written again, and
    // the echo of our own write is not.
    let listener_route_counts = count_attached_routes(&gateway.spec.listeners, &statuses.routes);
    let mut writes = Writes::default();
    writes.record(
        status::write_gateway_status(
            &ctx.client,
            gateway,
            status::desired_gateway_status(
                gateway,
                &statuses.accepted,
                &programmed.condition,
                &listener_route_counts,
                &statuses.listeners,
                &programmed.usable,
                true,
            ),
        )
        .await,
    );
    write_route_statuses(
        ctx,
        &key,
        &statuses.routes,
        &statuses.observed_routes,
        is_programmed,
        &mut writes,
    )
    .await;
    write_policy_statuses(
        ctx,
        &key,
        &statuses.policies,
        &statuses.observed_policies,
        &mut writes,
    )
    .await;

    if let Some(e) = error {
        return Err(ReconcileError(e));
    }
    if writes.failed {
        // A status PATCH failed (apiserver blip, or a stale read). Without a
        // fast retry the wrong status would sit in the cluster until the slow
        // safety-net requeue.
        return Ok(Action::requeue(Duration::from_secs(10)));
    }

    // Requeue cadence is convergence-driven — see `success_requeue_secs`.
    let all_routes_accepted = statuses.routes.iter().all(|ra| ra.accepted);
    let requeue_secs = if published || writes.patched > 0 {
        crate::metrics::METRICS.reconcile_runs_total.inc();
        success_requeue_secs(is_programmed, all_routes_accepted)
    } else {
        crate::metrics::METRICS.reconcile_skips_total.inc();
        debug!("Gateway {} unchanged: nothing published or written", key);
        skip_requeue_secs(is_programmed, all_routes_accepted)
    };
    Ok(Action::requeue(Duration::from_secs(requeue_secs)))
}

pub(super) async fn reconcile(
    gateway: Arc<Gateway>,
    ctx: Arc<ControllerCtx>,
) -> Result<Action, ReconcileError> {
    match publish(&gateway, &ctx).await {
        Step::Done(action) => Ok(action),
        Step::Continue(pass) => finish(&gateway, &ctx, *pass).await,
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::{self, apply, Pipeline};
    use super::*;
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
        let mut p = Pipeline::new();
        let gw = testkit::gateway("gw", 1);
        apply(&mut p.cluster.gateways, gw.clone());

        p.pass(&gw).await.unwrap();
        let written = p
            .api
            .status_patch("gateways", "gw")
            .expect("first pass writes");
        let mut gw = testkit::with_gateway_status(gw, &written);

        // Infrastructure labels reach the pod, not the config.
        gw.metadata.generation = Some(2);
        gw.spec.infrastructure = Some(gateway_api::gateways::GatewayInfrastructure {
            labels: Some([("team".to_string(), "a".to_string())].into()),
            ..Default::default()
        });
        apply(&mut p.cluster.gateways, gw.clone());
        p.api.clear();
        p.pass(&gw).await.unwrap();

        let rewritten = p
            .api
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
        let mut p = Pipeline::new();
        let gw = testkit::gateway("gw", 1);
        apply(&mut p.cluster.gateways, gw.clone());

        p.pass(&gw).await.unwrap();
        let written = p.api.status_patch("gateways", "gw").unwrap();
        let stored = testkit::with_gateway_status(gw, &written);
        apply(&mut p.cluster.gateways, stored.clone());

        p.api.clear();
        p.pass(&stored).await.unwrap();
        assert!(
            p.api.status_patch("gateways", "gw").is_none(),
            "echo of our own write"
        );

        let mut wiped = stored;
        wiped.status = None;
        apply(&mut p.cluster.gateways, wiped.clone());
        p.api.clear();
        p.pass(&wiped).await.unwrap();
        assert!(
            p.api.status_patch("gateways", "gw").is_some(),
            "wiped status healed"
        );

        let mut recreated = testkit::gateway("gw", 1);
        recreated.metadata.uid = Some("gw-uid-2".into());
        apply(&mut p.cluster.gateways, recreated.clone());
        p.api.clear();
        p.pass(&recreated).await.unwrap();
        assert!(
            p.api.status_patch("gateways", "gw").is_some(),
            "recreated Gateway gets status"
        );
    }

    /// Route status is written when it changes and not on the echo of the
    /// write, which the old fingerprint counted as a change. Every write
    /// carries the resourceVersion it was computed from.
    #[tokio::test]
    async fn route_status_writes_only_what_changed() {
        let mut p = Pipeline::new();
        let gw = testkit::gateway("gw", 1);
        apply(&mut p.cluster.gateways, gw.clone());
        let route = testkit::http_route("r", &["gw"], 1);
        apply(&mut p.cluster.http_routes, route.clone());

        p.pass(&gw).await.unwrap();
        let patch = p
            .api
            .status_patch("httproutes", "r")
            .expect("route gets status");
        assert_eq!(patch["metadata"]["resourceVersion"], "1");
        let route = testkit::with_route_status(route, &patch);
        apply(&mut p.cluster.http_routes, route.clone());
        let gw = testkit::with_gateway_status(gw, &p.api.status_patch("gateways", "gw").unwrap());
        apply(&mut p.cluster.gateways, gw.clone());

        p.api.clear();
        p.pass(&gw).await.unwrap();
        let patches: Vec<_> = p
            .api
            .requests()
            .into_iter()
            .filter(|r| r.method == "PATCH")
            .map(|r| r.path)
            .collect();
        assert!(patches.is_empty(), "nothing changed: {patches:?}");

        let mut bumped = route;
        bumped.metadata.generation = Some(2);
        apply(&mut p.cluster.http_routes, bumped);
        p.api.clear();
        p.pass(&gw).await.unwrap();
        let patch = p
            .api
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
        let mut p = Pipeline::new();
        let gw = testkit::gateway("gw", 1);
        apply(&mut p.cluster.gateways, gw.clone());
        let mut route = testkit::http_route("r", &["other-gw"], 2);
        route.status = Some(
            serde_json::from_value(serde_json::json!({ "parents": [
                testkit::status_entry(testkit::CONTROLLER, "gw", "parentRef"),
                testkit::status_entry("example.com/other", "gw", "parentRef"),
            ]}))
            .unwrap(),
        );
        apply(&mut p.cluster.http_routes, route);

        p.pass(&gw).await.unwrap();
        let patch = p
            .api
            .status_patch("httproutes", "r")
            .expect("our stale entry is removed");
        let parents = patch["status"]["parents"].as_array().unwrap();
        assert_eq!(parents.len(), 1, "{parents:?}");
        assert_eq!(parents[0]["controllerName"], "example.com/other");
    }

    /// Same for a RateLimitPolicy retargeted to another Gateway.
    #[tokio::test]
    async fn policy_that_moved_away_loses_our_ancestor() {
        let mut p = Pipeline::new();
        let gw = testkit::gateway("gw", 1);
        apply(&mut p.cluster.gateways, gw.clone());
        let mut policy = testkit::rate_limit_policy("p", "other-gw");
        policy.status = Some(
            serde_json::from_value(serde_json::json!({ "ancestors": [
                testkit::status_entry(testkit::CONTROLLER, "gw", "ancestorRef"),
            ]}))
            .unwrap(),
        );
        apply(&mut p.cluster.policies, policy);

        p.pass(&gw).await.unwrap();
        let patch = p
            .api
            .status_patch("ratelimitpolicies", "p")
            .expect("our stale ancestor is removed");
        assert_eq!(patch["status"]["ancestors"], serde_json::json!([]));
    }

    /// Listener conditions of the first `status_patch` for `gw`, by type.
    fn listener_conditions(patch: &serde_json::Value) -> HashMap<String, serde_json::Value> {
        listener_conditions_of(patch, 0)
    }

    fn listener_conditions_of(
        patch: &serde_json::Value,
        listener: usize,
    ) -> HashMap<String, serde_json::Value> {
        patch["status"]["listeners"][listener]["conditions"]
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
            let mut p = Pipeline::new();
            let mut gw = testkit::gateway("gw", 1);
            gw.spec.listeners[0].port = held.local_addr().unwrap().port().into();
            apply(&mut p.cluster.gateways, gw.clone());

            p.pass(&gw).await.unwrap();
            let patch = p.api.status_patch("gateways", "gw").unwrap();
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
            apply(&mut p.cluster.gateways, gw.clone());
            p.api.clear();
            p.pass(&gw).await.unwrap();
            let patch = p.api.status_patch("gateways", "gw").unwrap();
            assert_eq!(listener_conditions(&patch)["Programmed"]["status"], "True");
        });
    }

    /// This host's first IPv4 address off loopback: one a Gateway can name.
    fn local_ipv4() -> Option<String> {
        crate::dataplane::addresses::local_interface_ips()
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

    /// Passes `gw` as its `generation`-th spec, with `addrs` for
    /// spec.addresses; returns its first listener's Programmed condition.
    async fn pass_with(
        p: &mut Pipeline,
        gw: &Gateway,
        generation: i64,
        addrs: &[(&str, &str)],
    ) -> serde_json::Value {
        let gw = with_addresses(gw.clone(), generation, addrs);
        apply(&mut p.cluster.gateways, gw.clone());
        p.api.clear();
        p.pass(&gw).await.unwrap();
        let patch = p.api.status_patch("gateways", "gw").unwrap();
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
                let mut p = Pipeline::new();
                let gw = gateway_on("gw", port);

                let programmed = pass_with(&mut p, &gw, 1, &[]).await;
                assert_eq!(programmed["status"], "True", "{programmed}");
                assert!(accepts("127.0.0.1"));

                let address = [("IPAddress", ip.as_str())];
                let programmed = pass_with(&mut p, &gw, 2, &address).await;
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
                let mut p = Pipeline::new();
                let gw = gateway_on("gw", port);

                let address = [("IPAddress", ip.as_str())];
                let programmed = pass_with(&mut p, &gw, 1, &address).await;
                assert_eq!(programmed["status"], "True", "{programmed}");
                assert!(!accepts("127.0.0.1"));

                let programmed = pass_with(&mut p, &gw, 2, &[]).await;
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

    fn nad_unreadable(p: &Pipeline) {
        p.api.reply_to(
            NAD_UDN_A,
            500,
            serde_json::json!({
                "kind": "Status", "apiVersion": "v1", "status": "Failure",
                "reason": "InternalError", "code": 500,
                "message": "etcdserver: request timed out",
            }),
        );
    }

    /// A NetworkAttachmentDefinition that cannot be read keeps its network's
    /// last CIDRs. An apiserver blip used to close every listener of the
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
            let mut p = Pipeline::new();
            p.api
                .reply_to(NAD_UDN_A, 200, nad_udn_a(&format!("{ip}/32")));
            let gw = gateway_on("gw", port);
            let network = [(NETWORK_ADDRESS_TYPE, "udn-a")];

            let programmed = pass_with(&mut p, &gw, 1, &network).await;
            assert_eq!(programmed["status"], "True", "{programmed}");
            assert!(accepts(&ip));

            nad_unreadable(&p);
            let programmed = pass_with(&mut p, &gw, 2, &network).await;
            assert_eq!(programmed["status"], "True", "{programmed}");
            assert!(accepts(&ip), "still bound");
        });
    }

    /// The Programmed condition of Gateway `name`'s last status write.
    fn gateway_programmed(p: &Pipeline, name: &str) -> serde_json::Value {
        let patch = p.api.status_patch("gateways", name).unwrap();
        let conditions = patch["status"]["conditions"].as_array().unwrap();
        conditions
            .iter()
            .find(|c| c["type"] == "Programmed")
            .unwrap()
            .clone()
    }

    /// A network dropped from spec.addresses forgets its CIDRs. They were
    /// kept, and a failed read of the network once re-added bound on an
    /// answer from a spec the Gateway no longer had.
    #[test]
    fn a_dropped_network_forgets_its_ip() {
        alone("a_dropped_network_forgets_its_ip", async {
            // Accepting a probe builds a backend pool: rustls needs its provider.
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            let Some(ip) = local_ipv4() else {
                return; // no address off loopback to name
            };
            let port = free_port();
            let accepts = |addr: &str| std::net::TcpStream::connect((addr, port)).is_ok();
            let mut p = Pipeline::new();
            p.api
                .reply_to(NAD_UDN_A, 200, nad_udn_a(&format!("{ip}/32")));
            let gw = gateway_on("gw", port);
            let network = [(NETWORK_ADDRESS_TYPE, "udn-a")];

            let programmed = pass_with(&mut p, &gw, 1, &network).await;
            assert_eq!(programmed["status"], "True", "{programmed}");
            assert!(accepts(&ip));
            let programmed = pass_with(&mut p, &gw, 2, &[]).await;
            assert_eq!(programmed["status"], "True", "{programmed}");

            nad_unreadable(&p);
            pass_with(&mut p, &gw, 3, &network).await;
            assert_eq!(gateway_programmed(&p, "gw")["reason"], "AddressNotUsable");
            assert!(!accepts(&ip), "bound on a forgotten answer");
        });
    }

    /// A deleted Gateway's networks go with it. They were kept for the life
    /// of the process, and a Gateway created again under its name bound the
    /// old IP on a failed read.
    #[test]
    fn a_deleted_gateway_forgets_its_networks() {
        alone("a_deleted_gateway_forgets_its_networks", async {
            // Accepting a probe builds a backend pool: rustls needs its provider.
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            let Some(ip) = local_ipv4() else {
                return; // no address off loopback to name
            };
            let port = free_port();
            let accepts = |addr: &str| std::net::TcpStream::connect((addr, port)).is_ok();
            let mut p = Pipeline::new();
            p.api
                .reply_to(NAD_UDN_A, 200, nad_udn_a(&format!("{ip}/32")));
            let network = [(NETWORK_ADDRESS_TYPE, "udn-a")];
            let gw = with_addresses(gateway_on("gw", port), 1, &network);
            apply(&mut p.cluster.gateways, gw.clone());
            p.pass(&gw).await.unwrap();
            assert!(accepts(&ip));

            p.delete(&gw).await;

            nad_unreadable(&p);
            apply(&mut p.cluster.gateways, gw.clone());
            p.api.clear();
            p.pass(&gw).await.unwrap();
            assert_eq!(gateway_programmed(&p, "gw")["reason"], "AddressNotUsable");
            assert!(!accepts(&ip), "bound on a deleted Gateway's answer");
        });
    }

    /// A network with no IP on the pod binds nothing, every interface least
    /// of all.
    #[test]
    fn an_absent_network_binds_nothing() {
        alone("an_absent_network_binds_nothing", async {
            // Accepting a probe builds a backend pool: rustls needs its provider.
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            let port = free_port();
            let accepts = |port: u16| std::net::TcpStream::connect(("127.0.0.1", port)).is_ok();
            let mut p = Pipeline::new();
            // TEST-NET-1: no pod has an address there.
            p.api.reply_to(NAD_UDN_A, 200, nad_udn_a("192.0.2.0/24"));
            let network = [(NETWORK_ADDRESS_TYPE, "udn-a")];
            let gw = with_addresses(gateway_on("gw", port), 1, &network);
            apply(&mut p.cluster.gateways, gw.clone());

            p.pass(&gw).await.unwrap();
            assert!(
                !accepts(port),
                "bound nowhere, every interface least of all"
            );
            assert_eq!(gateway_programmed(&p, "gw")["reason"], "AddressNotUsable");
            assert!(!p.ready.load(std::sync::atomic::Ordering::Acquire));
        });
    }

    fn endpoint_slice_lists(p: &Pipeline) -> usize {
        p.api
            .requests()
            .iter()
            .filter(|r| r.method == "GET" && r.path.contains("/endpointslices"))
            .count()
    }

    /// Services no route of this Gateway references cost nothing. A headless
    /// one with a named targetPort cost an EndpointSlice LIST per pass, and
    /// any change to one rebuilt the table and rewrote status, in every pod.
    #[tokio::test]
    async fn unreferenced_services_cost_nothing() {
        let mut p = Pipeline::new();
        let gw = testkit::gateway("gw", 1);
        apply(&mut p.cluster.gateways, gw.clone());
        let named = serde_json::json!("http");
        apply(
            &mut p.cluster.services,
            testkit::service("elsewhere", "web", 80, named),
        );

        p.pass(&gw).await.unwrap();
        assert_eq!(endpoint_slice_lists(&p), 0);
        let gw = testkit::with_gateway_status(gw, &p.api.status_patch("gateways", "gw").unwrap());
        apply(&mut p.cluster.gateways, gw.clone());
        let served = p.served(&gw).unwrap();

        let mut changed = testkit::service("elsewhere", "web", 80, serde_json::json!(8080));
        changed.spec.as_mut().unwrap().ports.as_mut().unwrap()[0].app_protocol =
            Some("https".into());
        apply(&mut p.cluster.services, changed);
        p.api.clear();
        p.pass(&gw).await.unwrap();
        assert!(
            Arc::ptr_eq(&served, &p.served(&gw).unwrap()),
            "an unrelated Service changed the snapshot"
        );
        assert!(p.api.requests().is_empty(), "{:?}", p.api.requests());
    }

    /// A backendRef this route may not use reads nothing about its Service:
    /// without a ReferenceGrant, the other namespace is not this Gateway's
    /// to list.
    #[tokio::test]
    async fn refused_backend_reads_no_endpoint_slices() {
        let mut p = Pipeline::new();
        let gw = testkit::gateway("gw", 1);
        apply(&mut p.cluster.gateways, gw.clone());
        let mut svc = testkit::service("db", "web", 80, serde_json::json!("http"));
        svc.metadata.namespace = Some("other".into());
        apply(&mut p.cluster.services, svc);
        let mut route = testkit::http_route("r", &["gw"], 1);
        route.spec.rules = Some(vec![serde_json::from_value(serde_json::json!({
            "backendRefs": [{"name": "db", "namespace": "other", "port": 80}],
        }))
        .unwrap()]);
        apply(&mut p.cluster.http_routes, route);

        p.pass(&gw).await.unwrap();
        let patch = p.api.status_patch("httproutes", "r").unwrap();
        assert_eq!(
            patch["status"]["parents"][0]["conditions"][1]["reason"],
            "RefNotPermitted"
        );
        assert_eq!(endpoint_slice_lists(&p), 0);
    }

    fn endpoint_slices(ports: serde_json::Value) -> serde_json::Value {
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
    }

    const SLICES: &str = "/apis/discovery.k8s.io/v1/namespaces/default/endpointslices";

    /// A Pipeline with Gateway `gw` routing to headless Service `db`, whose
    /// targetPort is named.
    fn named_target_port_cluster() -> (Pipeline, Gateway) {
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
        (p, gw)
    }

    /// The served snapshot's headless targetPort of `db`, port 80.
    fn served_target_port(p: &Pipeline, gw: &Gateway) -> Option<u16> {
        p.served(gw)
            .unwrap()
            .to_config()
            .headless_target_ports
            .get(&("db.default.svc".to_string(), 80))
            .copied()
    }

    /// A referenced headless backend with a named targetPort connects on
    /// the number its EndpointSlices publish.
    #[tokio::test]
    async fn referenced_named_target_port_is_resolved() {
        let (mut p, gw) = named_target_port_cluster();
        p.api.reply_to(
            SLICES,
            200,
            endpoint_slices(serde_json::json!([{"name": "web", "port": 8080}])),
        );

        p.pass(&gw).await.unwrap();
        assert_eq!(served_target_port(&p, &gw), Some(8080));
        assert_eq!(endpoint_slice_lists(&p), 1);
    }

    /// A named targetPort is read once per Service version and DNS epoch;
    /// every pass used to list EndpointSlices again.
    #[tokio::test]
    async fn named_target_port_reads_wait_for_a_change() {
        let (mut p, gw) = named_target_port_cluster();
        p.api.reply_to(
            SLICES,
            200,
            endpoint_slices(serde_json::json!([{"name": "web", "port": 8080}])),
        );
        assert!(
            !p.ctx.named_ports.dns_changed(),
            "nothing cached: a DNS change is not worth a reconcile"
        );

        p.pass(&gw).await.unwrap();
        assert_eq!(endpoint_slice_lists(&p), 1);

        p.api.clear();
        p.pass(&gw).await.unwrap();
        assert_eq!(endpoint_slice_lists(&p), 0, "nothing moved");

        let mut changed = testkit::service("db", "web", 80, serde_json::json!("http"));
        changed.metadata.resource_version = Some("2".into());
        apply(&mut p.cluster.services, changed);
        p.api.clear();
        p.pass(&gw).await.unwrap();
        assert_eq!(endpoint_slice_lists(&p), 1, "the Service changed");

        assert!(p.ctx.named_ports.dns_changed());
        p.api.clear();
        p.pass(&gw).await.unwrap();
        assert_eq!(endpoint_slice_lists(&p), 1, "the pods changed");
    }

    /// With DNS refresh off, a read taken before the Service had pods kept
    /// the service port forever. It now expires at the resync interval.
    #[tokio::test]
    async fn named_target_port_read_expires_at_the_resync_interval() {
        let (mut p, gw) = named_target_port_cluster();
        p.api
            .reply_to(SLICES, 200, endpoint_slices(serde_json::json!([])));

        p.pass(&gw).await.unwrap();
        assert_eq!(
            served_target_port(&p, &gw),
            Some(80),
            "no pods yet: the service port"
        );

        p.api.reply_to(
            SLICES,
            200,
            endpoint_slices(serde_json::json!([{"name": "web", "port": 8080}])),
        );
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(super::super::RESYNC_SECS - 1)).await;
        p.api.clear();
        p.pass(&gw).await.unwrap();
        assert_eq!(endpoint_slice_lists(&p), 0, "still fresh");

        tokio::time::advance(Duration::from_secs(1)).await;
        p.pass(&gw).await.unwrap();
        assert_eq!(endpoint_slice_lists(&p), 1, "expired");
        assert_eq!(served_target_port(&p, &gw), Some(8080));
    }

    /// A moved DNS epoch in a report reads named targetPorts again, for
    /// that Gateway: the DNS refresh now runs in the data plane.
    #[tokio::test]
    async fn a_report_with_a_moved_dns_epoch_triggers_its_gateway() {
        use futures::StreamExt;
        let (mut p, gw) = named_target_port_cluster();
        let mut triggers = p.ctx.reports.triggers().unwrap();
        p.pass(&gw).await.unwrap();
        let first = triggers.next().await.unwrap();
        assert!(!first.dns_moved);
        assert_eq!(first.gateway, testkit::key(&gw));

        let mut report = p.ctx.reports.live(&testkit::key(&gw))[0].1.clone();
        report.dns_epoch += 1;
        p.ctx.reports.record(
            &testkit::key(&gw),
            super::super::reports::InstanceId("pipeline-pod".into()),
            report,
        );
        assert!(triggers.next().await.unwrap().dns_moved);
    }

    /// Deleting a Gateway closes its listener and empties the table: its
    /// snapshot leaves the hub and its data plane, answered 404, clears.
    /// Deletion ran no reconcile, so they served on.
    #[tokio::test]
    async fn deleted_gateway_stops_serving() {
        let port = free_port();
        let mut p = Pipeline::new();
        let mut gw = testkit::gateway("gw", 1);
        gw.spec.listeners[0].port = port.into();
        apply(&mut p.cluster.gateways, gw.clone());
        p.pass(&gw).await.unwrap();
        assert!(std::net::TcpListener::bind(("0.0.0.0", port)).is_err());

        p.delete(&gw).await;
        assert!(p.served(&gw).is_none());
        assert!(p.ctx.reports.live(&testkit::key(&gw)).is_empty());
        assert!(
            std::net::TcpListener::bind(("0.0.0.0", port)).is_ok(),
            "the deleted Gateway's port is released"
        );
    }

    /// A pass of a Gateway deleted since it was queued serves nothing: the
    /// delete's forget ran before this pass published, and the slot it
    /// left served the deleted Gateway until the next delete event.
    #[tokio::test]
    async fn a_pass_after_the_delete_serves_nothing() {
        let mut p = Pipeline::new();
        let gw = testkit::gateway("gw", 1);
        apply(&mut p.cluster.gateways, gw.clone());
        p.pass(&gw).await.unwrap();
        p.delete(&gw).await;

        p.api.clear();
        p.pass(&gw).await.unwrap();
        assert!(p.served(&gw).is_none());
        assert!(!p.ready.load(std::sync::atomic::Ordering::Acquire));
        assert!(
            p.api.status_patch("gateways", "gw").is_none(),
            "no status for a deleted Gateway"
        );
    }

    /// A TCP listener carries the oldest route; the newer one is Conflicted
    /// yet still attached, and attachedRoutes counts both. It counted only
    /// accepted routes.
    #[tokio::test]
    async fn attached_routes_count_conflicted_routes() {
        let mut p = Pipeline::new();
        let mut gw = testkit::gateway("gw", 1);
        gw.spec.listeners[0].protocol = "TCP".into();
        apply(&mut p.cluster.gateways, gw.clone());
        for (name, created) in [
            ("old", "2020-01-01T00:00:00Z"),
            ("new", "2021-01-01T00:00:00Z"),
        ] {
            let route: TCPRoute = serde_json::from_value(serde_json::json!({
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
            apply(&mut p.cluster.tcp_routes, route);
        }

        p.pass(&gw).await.unwrap();
        let newer = p.api.status_patch("tcproutes", "new").unwrap();
        assert_eq!(
            newer["status"]["parents"][0]["conditions"][0]["reason"],
            "Conflicted"
        );
        let gateway = p.api.status_patch("gateways", "gw").unwrap();
        assert_eq!(gateway["status"]["listeners"][0]["attachedRoutes"], 2);
    }

    /// A write computed from a stale read is refused (409), not applied
    /// over a concurrent writer's entry; the pass retries soon.
    #[tokio::test]
    async fn stale_route_read_retries_instead_of_clobbering() {
        let mut p = Pipeline::new();
        p.api.reply_to(
            "/apis/gateway.networking.k8s.io/v1/namespaces/default/httproutes/r/status",
            409,
            serde_json::json!({
                "kind": "Status", "apiVersion": "v1", "status": "Failure",
                "reason": "Conflict", "code": 409, "message": "object was modified",
            }),
        );
        let gw = testkit::gateway("gw", 1);
        apply(&mut p.cluster.gateways, gw.clone());
        apply(
            &mut p.cluster.http_routes,
            testkit::http_route("r", &["gw"], 1),
        );

        let action = p.pass(&gw).await.unwrap();
        assert_eq!(action, Action::requeue(Duration::from_secs(10)));
    }

    /// A class naming another controller takes its Gateways out of the hub
    /// (their data planes get 404); a missing class leaves them served.
    #[tokio::test]
    async fn only_a_class_of_another_controller_takes_a_gateway_away() {
        let mut p = Pipeline::new();
        let gw = testkit::gateway("gw", 1);
        apply(&mut p.cluster.gateways, gw.clone());
        p.pass(&gw).await.unwrap();
        assert!(p.served(&gw).is_some());

        p.cluster
            .gateway_classes
            .apply_watcher_event(&kube::runtime::watcher::Event::Delete(
                testkit::gateway_class("portail", testkit::CONTROLLER),
            ));
        p.api.clear();
        p.pass(&gw).await.unwrap();
        assert!(p.served(&gw).is_some(), "a deleted class changes nothing");
        assert!(p.api.requests().is_empty(), "nor writes anything");

        apply(
            &mut p.cluster.gateway_classes,
            testkit::gateway_class("portail", "example.com/other"),
        );
        p.pass(&gw).await.unwrap();
        assert!(p.served(&gw).is_none());
        assert!(!p.ready.load(std::sync::atomic::Ordering::Acquire));
    }

    /// The pipeline end to end: HTTPS, HTTP and TCP listeners bind, the
    /// report reaches the store, and status says Programmed; the Gateway's
    /// deletion clears its data plane.
    #[test]
    fn a_gateway_reaches_programmed_through_its_report() {
        alone("a_gateway_reaches_programmed_through_its_report", async {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            let mut p = Pipeline::new();
            let cert =
                rcgen::generate_simple_self_signed(vec!["secure.example".to_string()]).unwrap();
            let secret: k8s_openapi::api::core::v1::Secret =
                serde_json::from_value(serde_json::json!({
                    "metadata": {"name": "tls", "namespace": "default"},
                    "type": "kubernetes.io/tls",
                    "stringData": {},
                }))
                .unwrap();
            let mut secret = secret;
            secret.data = Some(
                [
                    ("tls.crt", cert.cert.pem()),
                    ("tls.key", cert.signing_key.serialize_pem()),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), k8s_openapi::ByteString(v.into_bytes())))
                .collect(),
            );
            apply(&mut p.cluster.secrets, secret);

            let ports: Vec<u16> = {
                let mut ports = Vec::new();
                while ports.len() < 3 {
                    let port = free_port();
                    if !ports.contains(&port) {
                        ports.push(port);
                    }
                }
                ports
            };
            let mut gw = testkit::gateway("gw", 1);
            gw.spec.listeners = serde_json::from_value(serde_json::json!([
                {"name": "https", "protocol": "HTTPS", "port": ports[0], "hostname": "secure.example",
                 "tls": {"mode": "Terminate", "certificateRefs": [{"name": "tls"}]}},
                {"name": "http", "protocol": "HTTP", "port": ports[1]},
                {"name": "tcp", "protocol": "TCP", "port": ports[2]},
            ]))
            .unwrap();
            apply(&mut p.cluster.gateways, gw.clone());

            p.pass(&gw).await.unwrap();
            let snapshot = p.served(&gw).unwrap();
            assert_eq!(snapshot.certificates.len(), 1, "the PEMs ride the wire");
            let reports = p.ctx.reports.live(&testkit::key(&gw));
            assert_eq!(reports.len(), 1);
            assert_eq!(reports[0].1.applied, snapshot.version);
            let patch = p.api.status_patch("gateways", "gw").unwrap();
            assert_eq!(gateway_programmed(&p, "gw")["status"], "True", "{patch}");
            for listener in 0..3 {
                let programmed = &listener_conditions_of(&patch, listener)["Programmed"];
                assert_eq!(programmed["status"], "True", "{programmed}");
            }
            for port in &ports {
                assert!(std::net::TcpStream::connect(("127.0.0.1", *port)).is_ok());
            }

            p.delete(&gw).await;
            for port in &ports {
                assert!(
                    std::net::TcpListener::bind(("0.0.0.0", *port)).is_ok(),
                    "port {port} released"
                );
            }
            assert!(!p.ready.load(std::sync::atomic::Ordering::Acquire));
        });
    }

    fn gateway_class_named(name: &str, controller: &str) -> Arc<GatewayClass> {
        Arc::new(testkit::gateway_class(name, controller))
    }

    /// Only a class naming this controller makes a Gateway ours; one
    /// naming another controller makes it someone else's; an absent class
    /// is neither.
    #[test]
    fn gateway_class_ownership() {
        let classes = vec![
            gateway_class_named("portail", "epheo.eu/portail"),
            gateway_class_named("other", "example.com/other"),
        ];
        assert_eq!(
            class_owner(&classes, "portail", "epheo.eu/portail"),
            ClassOwner::Ours
        );
        assert_eq!(
            class_owner(&classes, "other", "epheo.eu/portail"),
            ClassOwner::Other
        );
        assert_eq!(
            class_owner(&classes, "missing", "epheo.eu/portail"),
            ClassOwner::Missing
        );
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
}
