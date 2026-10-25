//! What follows from which Gateways are ours: GatewayClass status
//! (Accepted, observedGeneration, supportedFeatures) on every class naming
//! this controller, and the removal of our route and policy entries for
//! Gateways that are no longer ours. The leader writes, and only what
//! differs from the store. No finalizer: `gateway-exists-finalizer` is
//! optional in the spec.

use std::sync::Arc;
use std::time::Duration;

use futures::{FutureExt, Stream, StreamExt};
use kube::runtime::reflector::{ObjectRef, Store};
use kube::Client;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use gateway_api::experimental::tcproutes::TCPRoute;
use gateway_api::experimental::tlsroutes::TLSRoute;
use gateway_api::experimental::udproutes::UDPRoute;
use gateway_api::gatewayclasses::GatewayClass;
use gateway_api::httproutes::HTTPRoute;

use super::reconcile::{class_owner, ClassOwner};
use super::translate::{observed_policy, observed_route, Observed};
use super::watch::StatusReports;
use super::{ControllerCtx, ERROR_REQUEUE_SECS, RESYNC_SECS};
use crate::kubernetes::crds::{GatewayParameters, RateLimitPolicy};
use crate::kubernetes::naming::FIELD_MANAGER;
use crate::kubernetes::reconciler::GatewayRoute;
use crate::kubernetes::status::{self, GatewayCondition, Write};
use crate::logging::info;
use crate::sync::GatewayKey;

/// Feeds the class pass: each item of a stream passed through [`Self::on`]
/// wakes it.
pub(crate) struct ClassEvents(futures::channel::mpsc::UnboundedSender<()>);

/// The class pass's events and the stream `run` reads them from.
pub(crate) fn class_events() -> (ClassEvents, futures::channel::mpsc::UnboundedReceiver<()>) {
    let (sender, receiver) = futures::channel::mpsc::unbounded();
    (ClassEvents(sender), receiver)
}

impl ClassEvents {
    /// `stream`, each item of which wakes the class pass.
    pub(crate) fn on<S>(&self, stream: S) -> impl Stream<Item = S::Item> + Send + 'static
    where
        S: Stream + Send + 'static,
    {
        let sender = self.0.clone();
        stream.inspect(move |_| {
            let _ = sender.unbounded_send(());
        })
    }
}

/// Asks the class pass to run: a Gateway left this controller, and the
/// pass removes its entries (retrying a failed removal as its own).
#[derive(Default)]
pub(crate) struct Departures(tokio::sync::Notify);

impl Departures {
    pub(crate) fn wake(&self) {
        // A permit is kept while a pass runs: the next one sees the change.
        self.0.notify_one();
    }
}

/// Write class status and drop departed Gateways' entries on each item of
/// `changes` (a class or GatewayParameters event), on each departure, once
/// caches synced, on each leadership gained and every [`RESYNC_SECS`],
/// until `shutdown`.
pub(crate) async fn run(
    ctx: Arc<ControllerCtx>,
    changes: impl Stream<Item = ()> + Send,
    shutdown: CancellationToken,
) {
    let mut changes = std::pin::pin!(changes);
    let mut gained = std::pin::pin!(ctx.leader.gained());
    let listings = ctx.synced.clone();
    let mut synced = std::pin::pin!(async move { listings.synced().await });
    let mut was_synced = false;
    // The Gateways' safety net.
    let every = Duration::from_secs(RESYNC_SECS);
    let mut resync = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    resync.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut retry: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            change = changes.next() => if change.is_none() { return },
            Some(()) = gained.next() => {}
            () = ctx.departures.0.notified() => {}
            () = &mut synced, if !was_synced => was_synced = true,
            _ = resync.tick() => {}
            _ = tokio::time::sleep_until(retry.unwrap_or_else(tokio::time::Instant::now)),
                if retry.is_some() => {}
        }
        // A burst is one pass: the initial list sends a change per class,
        // and a lease taken with the caches synced wakes twice at once.
        loop {
            match changes.next().now_or_never() {
                Some(Some(())) => {}
                Some(None) => return,
                None => break,
            }
        }
        while let Some(Some(())) = gained.next().now_or_never() {}
        if !was_synced && synced.as_mut().now_or_never().is_some() {
            was_synced = true;
        }
        retry = (!pass(&ctx).await)
            .then(|| tokio::time::Instant::now() + Duration::from_secs(ERROR_REQUEUE_SECS));
    }
}

/// One pass over every class of ours and every departed entry. Returns
/// false when a write failed and the pass is due again.
async fn pass(ctx: &ControllerCtx) -> bool {
    // A standby writes nothing; unsynced stores would read a missing
    // GatewayParameters as an invalid one and every Gateway as departed.
    if !ctx.leader.is_leader() || !ctx.synced.is_synced() {
        return true;
    }
    let mut ok = true;
    for class in ctx.cache.gateway_classes.state() {
        if class.spec.controller_name != ctx.controller_name
            || class.metadata.deletion_timestamp.is_some()
        {
            continue;
        }
        let accepted = accepted(&class, &ctx.cache.gateway_parameters);
        let desired = status::desired_class_status(&class, &accepted);
        ok &= status::write_class_status(&ctx.client, &class, desired).await != Write::Failed;
    }
    // A route event maps to served Gateways only: no other pass retries a
    // removal refused on a stale read.
    ok &= remove_departed(&ctx.client, &ctx.controller_name, departed(ctx)).await;
    ok
}

/// Accepted, or InvalidParameters when the class names parameters that
/// cannot be found.
fn accepted(class: &GatewayClass, parameters: &Store<GatewayParameters>) -> GatewayCondition {
    let accepted = GatewayCondition::ok("Accepted", "GatewayClass accepted by portail controller");
    let Some(r) = &class.spec.parameters_ref else {
        return accepted;
    };
    let problem = if r.group != "portail.epheo.eu" || r.kind != "GatewayParameters" {
        format!(
            "parametersRef names {}/{}; only portail.epheo.eu/GatewayParameters is supported",
            r.group, r.kind
        )
    } else if let Some(ns) = &r.namespace {
        if parameters
            .get(&ObjectRef::new(&r.name).within(ns))
            .is_some()
        {
            return accepted;
        }
        format!("GatewayParameters {}/{} not found", ns, r.name)
    } else {
        // Gateway API GatewayClass.parametersRef: the namespace "MUST be
        // set for namespace-scoped resources".
        format!(
            "parametersRef to GatewayParameters {} has no namespace",
            r.name
        )
    };
    GatewayCondition {
        ok: false,
        reason: "InvalidParameters".into(),
        message: problem,
    }
}

/// An object whose status holds our entries for departed Gateways.
struct Departed {
    observed: Observed,
    gateways: Vec<GatewayKey>,
}

/// Every route parent and policy ancestor of ours naming a Gateway that is
/// gone from the store or whose class names another controller. No pass of
/// such a Gateway removes them: deletion runs none, and a pass of another
/// controller's Gateway writes nothing. A missing class keeps them, as it
/// keeps the Gateway served.
fn departed(ctx: &ControllerCtx) -> Vec<Departed> {
    let classes = ctx.cache.gateway_classes.state();
    let gone = |ns: &str, name: &str| match ctx.cache.gateways.get(&ObjectRef::new(name).within(ns))
    {
        None => true,
        Some(gw) => {
            class_owner(&classes, &gw.spec.gateway_class_name, &ctx.controller_name)
                == ClassOwner::Other
        }
    };
    let controller = ctx.controller_name.as_str();
    let mut out = Vec::new();
    routes(&ctx.cache.http_routes, controller, &gone, &mut out);
    routes(&ctx.cache.tcp_routes, controller, &gone, &mut out);
    routes(&ctx.cache.tls_routes, controller, &gone, &mut out);
    routes(&ctx.cache.udp_routes, controller, &gone, &mut out);
    for policy in ctx.cache.rate_limit_policies.state() {
        if let Some(gateways) = departed_of(&*policy, controller, &gone) {
            out.push(Departed {
                observed: observed_policy(&policy),
                gateways,
            });
        }
    }
    out
}

fn routes<R>(
    store: &Store<R>,
    controller: &str,
    gone: &impl Fn(&str, &str) -> bool,
    out: &mut Vec<Departed>,
) where
    R: GatewayRoute + StatusReports + kube::Resource + Clone,
    R::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
    for route in store.state() {
        if let Some(gateways) = departed_of(&*route, controller, gone) {
            out.push(Departed {
                observed: observed_route(&*route),
                gateways,
            });
        }
    }
}

/// The departed Gateways `obj` holds our entries for, if any.
fn departed_of(
    obj: &impl StatusReports,
    controller: &str,
    gone: &impl Fn(&str, &str) -> bool,
) -> Option<Vec<GatewayKey>> {
    let mut gateways: Vec<GatewayKey> = obj
        .reported_gateways(controller)
        .filter(|(ns, name)| gone(ns, name))
        .map(|(ns, name)| GatewayKey::new(ns, name))
        .collect();
    gateways.sort();
    gateways.dedup();
    (!gateways.is_empty()).then_some(gateways)
}

/// Remove `departed`'s entries. Returns false when a write failed.
async fn remove_departed(client: &Client, controller: &str, departed: Vec<Departed>) -> bool {
    let mut ok = true;
    for d in departed {
        let o = &d.observed;
        let ref_field = if o.kind == "RateLimitPolicy" {
            "ancestorRef"
        } else {
            "parentRef"
        };
        let names_departed = |e: &Value| {
            e["controllerName"].as_str() == Some(controller)
                && d.gateways.iter().any(|k| {
                    let gateway = &e[ref_field];
                    gateway["name"].as_str() == Some(k.name.as_str())
                        && gateway["namespace"].as_str().unwrap_or(&o.namespace) == k.namespace
                })
        };
        macro_rules! remove {
            ($ty:ty, $list:literal) => {
                status::remove_entries::<$ty>(
                    client,
                    &o.namespace,
                    &o.name,
                    $list,
                    &o.entries,
                    names_departed,
                    FIELD_MANAGER,
                )
                .await
            };
        }
        let write = match o.kind {
            "HTTPRoute" => remove!(HTTPRoute, "parents"),
            "TCPRoute" => remove!(TCPRoute, "parents"),
            "TLSRoute" => remove!(TLSRoute, "parents"),
            "UDPRoute" => remove!(UDPRoute, "parents"),
            "RateLimitPolicy" => remove!(RateLimitPolicy, "ancestors"),
            _ => continue,
        };
        if write == Write::Patched {
            info!(
                "{} {}/{}: removed our status for departed Gateway(s) {}",
                o.kind,
                o.namespace,
                o.name,
                d.gateways
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        ok &= write != Write::Failed;
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::super::testkit::{self, apply, Pipeline, Recorded, CONTROLLER};
    use super::*;
    use kube::runtime::reflector::store::Writer;
    use kube::runtime::watcher;
    use serde_json::json;

    const CLASSES: &str = "/apis/gateway.networking.k8s.io/v1/gatewayclasses";
    const ROUTE_R: &str = "/apis/gateway.networking.k8s.io/v1/namespaces/default/httproutes/r";

    /// A Pipeline whose class `portail` has a uid, and whose apiserver
    /// answers class writes at resourceVersion 8.
    fn pipeline() -> Pipeline {
        let mut p = Pipeline::new();
        apply(&mut p.cluster.gateway_classes, class("portail", CONTROLLER));
        p.api.reply_to(
            CLASSES,
            200,
            json!({
                "apiVersion": "gateway.networking.k8s.io/v1",
                "kind": "GatewayClass",
                "metadata": {"name": "portail", "resourceVersion": "8"},
                "spec": {"controllerName": CONTROLLER},
            }),
        );
        p
    }

    fn class(name: &str, controller: &str) -> GatewayClass {
        let mut class = testkit::gateway_class(name, controller);
        class.metadata.uid = Some(format!("{name}-uid"));
        class.metadata.resource_version = Some("7".into());
        class
    }

    fn class_writes(p: &Pipeline) -> Vec<Recorded> {
        p.api
            .requests()
            .into_iter()
            .filter(|r| r.path.starts_with(CLASSES))
            .collect()
    }

    /// The server-side apply of a class's conditions; the merge patch of
    /// its features list carries no apiVersion and no force.
    fn is_apply(write: &Recorded) -> bool {
        write.path.contains("force=true")
    }

    fn feature_list(status: &Value) -> Vec<&str> {
        status["supportedFeatures"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect()
    }

    /// The class loop over `p`, until the token is cancelled, past its
    /// first pass (the caches are synced): the class written.
    async fn start(p: &Pipeline) -> (CancellationToken, tokio::task::JoinHandle<()>) {
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(run(
            p.ctx.clone(),
            futures::stream::pending(),
            shutdown.clone(),
        ));
        for _ in 0..1000 {
            if class_writes(p).len() == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(class_writes(p).len(), 2, "the first pass");
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        (shutdown, task)
    }

    /// Our class only: Accepted at its generation applied by our field
    /// manager on the uid and resourceVersion read, to the status alone (no
    /// finalizer); the features, sorted, replace the list on the
    /// resourceVersion the apply left. The echo is not written again.
    #[tokio::test]
    async fn a_class_of_ours_is_accepted_once() {
        let mut p = pipeline();
        apply(
            &mut p.cluster.gateway_classes,
            class("other", "example.com/other"),
        );
        assert!(pass(&p.ctx).await);

        let writes = class_writes(&p);
        assert_eq!(writes.len(), 2, "{writes:?}");
        let (applied, features) = (&writes[0], &writes[1]);
        assert!(is_apply(applied), "{}", applied.path);
        assert!(applied
            .path
            .starts_with(&format!("{CLASSES}/portail/status?")));
        assert!(applied.path.contains("fieldManager=portail-controller"));
        assert_eq!(applied.body["metadata"]["uid"], "portail-uid");
        assert_eq!(applied.body["metadata"]["resourceVersion"], "7");
        assert!(applied.body["metadata"].get("finalizers").is_none());
        assert!(applied.body["status"].get("supportedFeatures").is_none());
        let accepted = &applied.body["status"]["conditions"][0];
        assert_eq!(accepted["type"], "Accepted");
        assert_eq!(accepted["status"], "True");
        assert_eq!(accepted["observedGeneration"], 1);

        assert!(!is_apply(features), "{}", features.path);
        assert!(features
            .path
            .starts_with(&format!("{CLASSES}/portail/status?")));
        assert!(features.body.get("apiVersion").is_none());
        assert_eq!(features.body["metadata"]["resourceVersion"], "8");
        let listed = feature_list(&features.body["status"]);
        let mut sorted = listed.clone();
        sorted.sort_unstable();
        assert_eq!(listed, sorted);
        assert!(listed.contains(&"HTTPRoute"));

        let mut stored = class("portail", CONTROLLER);
        let mut status = applied.body["status"].clone();
        status["supportedFeatures"] = features.body["status"]["supportedFeatures"].clone();
        stored.status = Some(serde_json::from_value(status).unwrap());
        apply(&mut p.cluster.gateway_classes, stored.clone());
        p.api.clear();
        assert!(pass(&p.ctx).await);
        assert!(class_writes(&p).is_empty(), "echo of our own write");

        stored.metadata.generation = Some(2);
        apply(&mut p.cluster.gateway_classes, stored);
        assert!(pass(&p.ctx).await);
        let bumped = p.api.status_patch("gatewayclasses", "portail").unwrap();
        assert_eq!(bumped["status"]["conditions"][0]["observedGeneration"], 2);
    }

    /// A list an older writer froze keeps a feature dropped since; an
    /// apply would leave that entry, its owner's, and be sent again on
    /// every pass. The list is replaced whole, the conditions left alone.
    #[tokio::test]
    async fn a_frozen_feature_list_is_replaced_whole() {
        let mut p = pipeline();
        let desired = status::desired_class_status(
            &class("portail", CONTROLLER),
            &GatewayCondition::ok("Accepted", "GatewayClass accepted by portail controller"),
        );
        let mut frozen = desired.clone();
        frozen["conditions"][0]["lastTransitionTime"] = json!("2026-03-01T00:00:00Z");
        frozen["supportedFeatures"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name": "GatewayStaticAddresses"}));
        let mut stored = class("portail", CONTROLLER);
        stored.status = Some(serde_json::from_value(frozen).unwrap());
        apply(&mut p.cluster.gateway_classes, stored);

        assert!(pass(&p.ctx).await);
        let writes = class_writes(&p);
        assert_eq!(writes.len(), 1, "{writes:?}");
        assert!(!is_apply(&writes[0]));
        assert_eq!(writes[0].body["metadata"]["resourceVersion"], "7");
        assert_eq!(
            writes[0].body["status"]["supportedFeatures"],
            desired["supportedFeatures"]
        );
        assert!(writes[0].body["status"].get("conditions").is_none());
    }

    fn parameters(name: &str) -> GatewayParameters {
        let mut params = GatewayParameters::new(name, Default::default());
        params.metadata.namespace = Some("portail-system".into());
        params
    }

    fn with_ref(group: &str, kind: &str, namespace: Option<&str>) -> GatewayClass {
        let mut class = class("portail", CONTROLLER);
        class.spec.parameters_ref = Some(gateway_api::gatewayclasses::GatewayClassParametersRef {
            group: group.into(),
            kind: kind.into(),
            name: "public".into(),
            namespace: namespace.map(Into::into),
        });
        class
    }

    /// Parameters named but not usable: InvalidParameters, with why.
    #[test]
    fn class_parameters_that_cannot_be_found_are_invalid() {
        let mut writer = Writer::<GatewayParameters>::default();
        apply(&mut writer, parameters("public"));
        let store = writer.as_reader();
        let group = "portail.epheo.eu";

        assert!(accepted(&class("portail", CONTROLLER), &store).ok);
        assert!(
            accepted(
                &with_ref(group, "GatewayParameters", Some("portail-system")),
                &store
            )
            .ok
        );
        for (class, why) in [
            (
                with_ref("example.com", "Config", Some("portail-system")),
                "example.com/Config",
            ),
            (with_ref(group, "GatewayParameters", None), "no namespace"),
            (
                with_ref(group, "GatewayParameters", Some("elsewhere")),
                "not found",
            ),
        ] {
            let condition = accepted(&class, &store);
            assert!(!condition.ok, "{why}");
            assert_eq!(condition.reason, "InvalidParameters");
            assert!(condition.message.contains(why), "{}", condition.message);
        }
    }

    /// The first PATCH to `plural/name` once one is sent.
    async fn eventually(p: &Pipeline, plural: &str, name: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(patch) = p.api.status_patches(plural, name).into_iter().next() {
                    return patch;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no write to {plural}/{name}"))
    }

    /// Nothing is written while a kind has not listed; its listing wakes
    /// the pass.
    #[tokio::test]
    async fn the_caches_syncing_wakes_the_class_pass() {
        let p = pipeline();
        let routes = p.ctx.synced.track("HTTPRoute", false);
        let (events, changes) = class_events();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(run(p.ctx.clone(), changes, shutdown.clone()));
        let mut classes = events.on(futures::stream::iter([()]));
        assert!(classes.next().await.is_some());
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert!(
            class_writes(&p).is_empty(),
            "written before the caches synced"
        );

        routes.observe::<HTTPRoute>(&Ok(watcher::Event::InitDone));
        let written = eventually(&p, "gatewayclasses", "portail").await;
        assert_eq!(written["status"]["conditions"][0]["status"], "True");
        shutdown.cancel();
        task.await.unwrap();
    }

    /// A class seen before its GatewayParameters is judged again when they
    /// come. Only class events woke the pass: the class stayed
    /// InvalidParameters until the resync.
    #[tokio::test]
    async fn a_parameters_event_judges_the_class_again() {
        let mut p = pipeline();
        apply(
            &mut p.cluster.gateway_classes,
            with_ref(
                "portail.epheo.eu",
                "GatewayParameters",
                Some("portail-system"),
            ),
        );
        let (events, changes) = class_events();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(run(p.ctx.clone(), changes, shutdown.clone()));
        let mut classes = events.on(futures::stream::iter([()]));
        classes.next().await;
        let first = eventually(&p, "gatewayclasses", "portail").await;
        assert_eq!(
            first["status"]["conditions"][0]["reason"],
            "InvalidParameters"
        );

        let mut stored = with_ref(
            "portail.epheo.eu",
            "GatewayParameters",
            Some("portail-system"),
        );
        stored.status = Some(serde_json::from_value(first["status"].clone()).unwrap());
        apply(&mut p.cluster.gateway_classes, stored);
        apply(&mut p.cluster.parameters, parameters("public"));
        p.api.clear();
        let mut params = events.on(futures::stream::iter([parameters("public")]));
        params.next().await;
        let judged = eventually(&p, "gatewayclasses", "portail").await;
        assert_eq!(judged["status"]["conditions"][0]["reason"], "Accepted");
        shutdown.cancel();
        task.await.unwrap();
    }

    /// A route whose status holds our entries for `gateways`, a foreign
    /// controller's for the first, and that targets `target`.
    fn route_reporting(target: &str, gateways: &[&str]) -> HTTPRoute {
        let mut entries: Vec<Value> = gateways
            .iter()
            .map(|gw| testkit::status_entry(CONTROLLER, gw, "parentRef"))
            .collect();
        entries.push(testkit::status_entry(
            "example.com/other",
            gateways[0],
            "parentRef",
        ));
        let mut route = testkit::http_route("r", &[target], 1);
        route.status = Some(serde_json::from_value(json!({ "parents": entries })).unwrap());
        route
    }

    fn parent_names(patch: &Value) -> Vec<(String, String)> {
        patch["status"]["parents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["controllerName"].as_str().unwrap().to_string(),
                    e["parentRef"]["name"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    /// Our class turning to another controller makes its Gateways
    /// someone else's: our entries for them go.
    #[tokio::test]
    async fn a_class_turned_foreign_drops_our_entries() {
        let mut p = pipeline();
        apply(&mut p.cluster.gateways, testkit::gateway("gw", 1));
        apply(&mut p.cluster.http_routes, route_reporting("gw", &["gw"]));
        apply(
            &mut p.cluster.gateway_classes,
            class("portail", "example.com/other"),
        );

        assert!(pass(&p.ctx).await);
        let route = p.api.status_patch("httproutes", "r").unwrap();
        assert_eq!(
            parent_names(&route),
            [("example.com/other".to_string(), "gw".to_string())]
        );
        assert!(class_writes(&p).is_empty(), "not our class any more");
    }

    /// Deleting a Gateway removes our route parent and policy ancestor
    /// entries naming it; another controller's entries and ours for a live
    /// Gateway stay. They stayed forever: deletion runs no pass.
    #[tokio::test]
    async fn a_deleted_gateway_loses_our_entries() {
        let mut p = pipeline();
        let gw = testkit::gateway("gw", 1);
        apply(&mut p.cluster.gateways, gw.clone());
        apply(&mut p.cluster.gateways, testkit::gateway("gw2", 1));
        apply(
            &mut p.cluster.http_routes,
            route_reporting("gw2", &["gw", "gw2"]),
        );
        let mut policy = testkit::rate_limit_policy("p", "other-gw");
        policy.status = Some(
            serde_json::from_value(json!({ "ancestors": [
                testkit::status_entry(CONTROLLER, "gw", "ancestorRef"),
            ]}))
            .unwrap(),
        );
        apply(&mut p.cluster.policies, policy);
        let (shutdown, task) = start(&p).await;

        p.delete(&gw).await;
        let route = eventually(&p, "httproutes", "r").await;
        assert_eq!(
            parent_names(&route),
            [
                (CONTROLLER.to_string(), "gw2".to_string()),
                ("example.com/other".to_string(), "gw".to_string()),
            ]
        );
        assert_eq!(route["metadata"]["resourceVersion"], "1");
        let policy = eventually(&p, "ratelimitpolicies", "p").await;
        assert_eq!(policy["status"]["ancestors"], json!([]));
        shutdown.cancel();
        task.await.unwrap();
    }

    /// A removal refused on a stale read (409) is retried within the
    /// class pass's retry, not at the next resync ten minutes on.
    #[tokio::test]
    async fn a_refused_removal_is_retried_soon() {
        let mut p = pipeline();
        let gw = testkit::gateway("gw", 1);
        apply(&mut p.cluster.gateways, gw.clone());
        apply(&mut p.cluster.http_routes, route_reporting("gw", &["gw"]));
        p.api.reply_to(
            ROUTE_R,
            409,
            json!({
                "kind": "Status", "apiVersion": "v1", "status": "Failure",
                "reason": "Conflict", "code": 409, "message": "object was modified",
            }),
        );
        tokio::time::pause();
        let (shutdown, task) = start(&p).await;
        // The store's delete and the controller's forget, without the
        // data plane's clear: the paused clock would run on while it waits.
        p.cluster
            .gateways
            .apply_watcher_event(&watcher::Event::Delete(gw));
        super::super::reconcile::forget_deleted_gateways(&p.ctx);
        let attempts = |p: &Pipeline| p.api.status_patches("httproutes", "r").len();
        for _ in 0..1000 {
            if attempts(&p) > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(attempts(&p), 1, "refused");
        // The pass ends and sets its retry from the clock as it is now.
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }

        p.api.reply_to(
            ROUTE_R,
            200,
            json!({
                "apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute",
                "metadata": {"name": "r", "namespace": "default"}, "spec": {},
            }),
        );
        // Past the retry: timers round up to the millisecond.
        tokio::time::advance(Duration::from_secs(ERROR_REQUEUE_SECS + 1)).await;
        for _ in 0..1000 {
            if attempts(&p) > 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(attempts(&p), 2, "retried");
        shutdown.cancel();
        task.await.unwrap();
    }

    /// A Gateway moving to another controller's class drops our entries
    /// once the pass that stops serving it wakes the class pass.
    #[tokio::test]
    async fn a_gateway_moved_to_a_foreign_class_drops_our_entries() {
        let mut p = pipeline();
        let gw = testkit::gateway("gw", 1);
        apply(&mut p.cluster.gateways, gw.clone());
        p.pass(&gw).await.unwrap();
        apply(
            &mut p.cluster.gateway_classes,
            class("other", "example.com/other"),
        );
        apply(&mut p.cluster.http_routes, route_reporting("gw", &["gw"]));
        let mut moved = gw;
        moved.spec.gateway_class_name = "other".into();
        apply(&mut p.cluster.gateways, moved.clone());
        let (shutdown, task) = start(&p).await;

        p.api.clear();
        p.pass(&moved).await.unwrap();
        assert!(p.served(&moved).is_none());
        let route = eventually(&p, "httproutes", "r").await;
        assert_eq!(
            parent_names(&route),
            [("example.com/other".to_string(), "gw".to_string())]
        );
        shutdown.cancel();
        task.await.unwrap();
    }

    /// A missing class takes nothing away, our entries included; unsynced
    /// stores remove nothing either.
    #[tokio::test]
    async fn a_missing_class_or_unsynced_stores_keep_our_entries() {
        let mut p = pipeline();
        apply(&mut p.cluster.gateways, testkit::gateway("gw", 1));
        apply(&mut p.cluster.http_routes, route_reporting("gw", &["gw"]));
        p.cluster
            .gateway_classes
            .apply_watcher_event(&watcher::Event::Delete(class("portail", CONTROLLER)));
        assert!(pass(&p.ctx).await);
        assert!(p.api.requests().is_empty(), "{:?}", p.api.requests());

        let _unlisted = p.ctx.synced.track("HTTPRoute", false);
        p.cluster
            .gateways
            .apply_watcher_event(&watcher::Event::Delete(testkit::gateway("gw", 1)));
        assert!(pass(&p.ctx).await);
        assert!(p.api.requests().is_empty(), "{:?}", p.api.requests());
    }
}
