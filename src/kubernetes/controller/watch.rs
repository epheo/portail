//! Reflector/watch plumbing: store+stream construction (plain and
//! optional-CRD), the initial-list bookkeeping the caches-synced signal
//! reads, and the secondary-resource → Gateway mappers.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::core::v1::Secret;
use kube::api::Api;
use kube::runtime::reflector::{ObjectRef, Store};
use kube::runtime::watcher;
use kube::runtime::{reflector, WatchStreamExt};
use kube::Client;

use gateway_api::gateways::Gateway;

use crate::kubernetes::crds::RateLimitPolicy;
use crate::kubernetes::parent_ref::ParentRefAccess;
use crate::logging::{info, warn};

/// Every watched kind's initial list: done, or what holds it. Nothing is
/// published before every kind listed: a translation would miss the
/// objects of one not listed yet.
#[derive(Clone, Default)]
pub(super) struct Listings(Arc<ListingsInner>);

#[derive(Default)]
struct ListingsInner {
    kinds: Mutex<BTreeMap<String, Listing>>,
    changed: tokio::sync::Notify,
    /// Wakes every [`Listings::synced`] waiter on each change; `changed`
    /// keeps its one waiter's permit.
    any_change: tokio::sync::Notify,
}

struct Listing {
    /// A refused list (404, 403) counts as listed, empty: the kind is a CRD
    /// a cluster may lack or not grant, and every Gateway would wait on it.
    optional: bool,
    listed: bool,
    /// The last error while not listed.
    cause: Option<String>,
}

impl Listings {
    fn kinds(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Listing>> {
        self.0.kinds.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Follow `kind`'s initial list.
    pub(super) fn track(&self, kind: &str, optional: bool) -> ListTracker {
        self.kinds().insert(
            kind.to_string(),
            Listing {
                optional,
                listed: false,
                cause: None,
            },
        );
        ListTracker {
            listings: self.clone(),
            kind: kind.to_string(),
        }
    }

    /// Every tracked kind listed.
    pub(super) fn is_synced(&self) -> bool {
        self.kinds().values().all(|l| l.listed)
    }

    /// The kinds not listed yet, each with what holds it.
    pub(super) fn unlisted(&self) -> Vec<(String, String)> {
        self.kinds()
            .iter()
            .filter(|(_, l)| !l.listed)
            .map(|(kind, l)| {
                let cause = l.cause.as_deref().unwrap_or("not listed yet");
                (kind.clone(), cause.to_string())
            })
            .collect()
    }

    /// Wakes after a change to any kind.
    pub(super) async fn changed(&self) {
        self.0.changed.notified().await
    }

    /// Resolves once every tracked kind listed. Any number of waiters.
    pub(super) async fn synced(&self) {
        loop {
            let changed = self.0.any_change.notified();
            let mut changed = std::pin::pin!(changed);
            // Registered before the check: a change in between still wakes.
            changed.as_mut().enable();
            if self.is_synced() {
                return;
            }
            changed.await;
        }
    }

    fn update(&self, kind: &str, change: impl FnOnce(&mut Listing)) {
        if let Some(listing) = self.kinds().get_mut(kind) {
            change(listing);
        }
        // One waiter, the synced task: a permit kept for it is a wake it
        // would otherwise miss between its check and its wait.
        self.0.changed.notify_one();
        self.0.any_change.notify_waiters();
    }
}

/// One kind's entry in [`Listings`].
#[derive(Clone)]
pub(super) struct ListTracker {
    listings: Listings,
    kind: String,
}

impl ListTracker {
    /// Record what a watch event says about the initial list.
    pub(super) fn observe<K>(&self, event: &Result<watcher::Event<K>, watcher::Error>) {
        match event {
            Ok(watcher::Event::InitDone) => self.listings.update(&self.kind, |l| l.listed = true),
            Err(e) => self.failed(&e.to_string(), status_code(e)),
            Ok(_) => {}
        }
    }

    /// The kind holds no object: its CRD is not installed.
    fn listed_empty(&self, why: &str) {
        warn!("{} counted as listed and empty: {}", self.kind, why);
        self.listings.update(&self.kind, |l| l.listed = true);
    }

    /// Something holds the initial list: `code` is the HTTP status, if any.
    fn failed(&self, cause: &str, code: Option<u16>) {
        let mut refused = false;
        self.listings.update(&self.kind, |l| {
            if l.listed {
                return;
            }
            if l.optional && matches!(code, Some(403 | 404)) {
                l.listed = true;
                refused = true;
            } else {
                l.cause = Some(cause.to_string());
            }
        });
        if refused {
            warn!(
                "{} list refused ({}): counted as listed and empty; its watch retries",
                self.kind, cause
            );
        }
    }
}

/// The HTTP status of a watch error the apiserver answered.
fn status_code(e: &watcher::Error) -> Option<u16> {
    match e {
        watcher::Error::InitialListFailed(kube::Error::Api(r))
        | watcher::Error::WatchStartFailed(kube::Error::Api(r))
        | watcher::Error::WatchFailed(kube::Error::Api(r)) => Some(r.code),
        watcher::Error::WatchError(r) => Some(r.code),
        _ => None,
    }
}

/// Create a reflector for a resource type, returning the store and a stream.
/// The stream is consumed by `Controller::watches_stream()` which both drives
/// the reflector (keeping the store up to date) and triggers reconciliation
/// of mapped Gateway(s) when resources change.
///
/// Streams the initial list via the WatchList API instead of a single blocking
/// LIST: cluster-wide lists of every Service, route and Secret are the
/// controller's cold start, and concurrent blocking LISTs under churn stall
/// on a loaded apiserver (measured 1s isolated, 20s+ or never under load).
/// Streaming lists are far lighter on the apiserver under that load. Requires
/// server WatchList support (k8s >= 1.27; default-on >= 1.30).
pub(super) fn create_reflector<K>(
    api: Api<K>,
    listings: &Listings,
) -> (Store<K>, BoxedReflectorStream<K>)
where
    K: kube::Resource
        + Clone
        + std::fmt::Debug
        + serde::de::DeserializeOwned
        + Send
        + Sync
        + 'static,
    K::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
    create_reflector_with_config(api, watcher::Config::default().streaming_lists(), listings)
}

/// Like `create_reflector`, for a CRD that may not be installed or
/// readable, which counts as listed and empty (see [`Listing::optional`]).
/// API discovery decides whether to watch: when it does not serve the CRD,
/// the store stays empty and the stream waits until
/// [`PendingWatches::recheck_every`] finds it served, so a CRD installed
/// after startup is watched without a restart. A discovery that fails says
/// neither: it is retried, never taken as an answer.
pub(super) fn create_optional_reflector<K>(
    api: Api<K>,
    client: &Client,
    pending: &Arc<PendingWatches>,
    listings: &Listings,
) -> (Store<K>, BoxedReflectorStream<K>)
where
    K: kube::Resource
        + Clone
        + std::fmt::Debug
        + serde::de::DeserializeOwned
        + Send
        + Sync
        + 'static,
    K::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
    let dt = K::DynamicType::default();
    let watch = PendingWatch {
        api_version: K::api_version(&dt).into_owned(),
        plural: K::plural(&dt).into_owned(),
        kind: K::kind(&dt).into_owned(),
    };
    let tracker = listings.track(&watch.kind, true);
    let writer = reflector::store::Writer::default();
    let reader = writer.as_reader();
    let (start, started) = tokio::sync::oneshot::channel();
    tokio::spawn(discover(
        client.clone(),
        watch,
        start,
        pending.clone(),
        tracker.clone(),
    ));
    let stream = futures::stream::once(async move {
        match started.await {
            Ok(()) => reflector_stream(
                writer,
                api,
                watcher::Config::default().streaming_lists(),
                tracker,
            ),
            Err(_) => Box::pin(futures::stream::pending()),
        }
    })
    .flatten();
    (reader, Box::pin(stream))
}

/// Ask API discovery whether `watch`'s CRD is served, with the backoff kube
/// watchers use until it answers: start the watch, or park it on `pending`.
async fn discover(
    client: Client,
    watch: PendingWatch,
    mut start: tokio::sync::oneshot::Sender<()>,
    pending: Arc<PendingWatches>,
    tracker: ListTracker,
) {
    let mut backoff = watcher::DefaultBackoff::default();
    loop {
        match crd_served(&client, &watch.api_version, &watch.plural).await {
            Ok(true) => {
                let _ = start.send(());
                return;
            }
            Ok(false) => {
                tracker.listed_empty("its CRD is not installed; its watch starts once it is");
                pending.add(watch, start);
                return;
            }
            Err(e) => {
                let delay = backoff
                    .next()
                    .unwrap_or(Duration::from_secs(RETRY_CAP_SECS));
                let cause = format!("API discovery of {} failed: {}", watch.api_version, e);
                warn!("{}; retrying in {:.1}s", cause, delay.as_secs_f32());
                tracker.failed(&cause, None);
                tokio::select! {
                    // The stream is gone: the controller stopped.
                    _ = start.closed() => return,
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

/// The retry delay should kube's backoff end, which it does not: its own
/// cap.
const RETRY_CAP_SECS: u64 = 30;

/// Whether API discovery serves `plural` in `api_version`; `Err` when
/// discovery itself failed, which says neither.
async fn crd_served(client: &Client, api_version: &str, plural: &str) -> Result<bool, kube::Error> {
    match client.list_api_group_resources(api_version).await {
        Ok(list) => Ok(list.resources.iter().any(|r| r.name == plural)),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(false),
        Err(e) => Err(e),
    }
}

struct PendingWatch {
    api_version: String,
    plural: String,
    kind: String,
}

/// Optional-CRD watches waiting for their CRD. Discovery is readable by
/// every authenticated client, so the recheck needs no RBAC.
#[derive(Default)]
pub(super) struct PendingWatches {
    watches: std::sync::Mutex<Vec<(PendingWatch, tokio::sync::oneshot::Sender<()>)>>,
}

impl PendingWatches {
    fn add(&self, watch: PendingWatch, start: tokio::sync::oneshot::Sender<()>) {
        let mut watches = self.watches.lock().unwrap_or_else(|p| p.into_inner());
        watches.push((watch, start));
    }

    /// Recheck every `interval` until shutdown. Its own timer, not
    /// reconcile's: a cluster with no Gateway of ours never reconciles. It
    /// runs while none waits too: a failed discovery parks one later.
    pub async fn recheck_every(
        self: Arc<Self>,
        client: Client,
        interval: Duration,
        shutdown: tokio_util::sync::CancellationToken,
    ) {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(interval) => self.recheck(&client).await,
            }
        }
    }

    /// Start the watches whose CRD discovery now serves.
    async fn recheck(&self, client: &Client) {
        let waiting: Vec<(String, String)> = self
            .watches
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(w, _)| (w.api_version.clone(), w.plural.clone()))
            .collect();
        for (api_version, plural) in waiting {
            if !matches!(crd_served(client, &api_version, &plural).await, Ok(true)) {
                continue;
            }
            let mut watches = self.watches.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(i) = watches
                .iter()
                .position(|(w, _)| w.api_version == api_version && w.plural == plural)
            {
                let (watch, start) = watches.swap_remove(i);
                info!("CRD installed, starting its watcher: {}", watch.kind);
                let _ = start.send(());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Mapper functions: map secondary resource events to Gateway ObjectRef(s)
// ---------------------------------------------------------------------------

/// The Gateways an object's status says `controller` reported on: route
/// `status.parents` and policy `status.ancestors` entries carrying its
/// controllerName.
pub(super) trait StatusReports {
    /// (namespace, name) of each such Gateway.
    fn reported_gateways<'a>(
        &'a self,
        controller: &'a str,
    ) -> impl Iterator<Item = (&'a str, &'a str)> + 'a;

    fn reports(&self, controller: &str, gw_ns: &str, gw_name: &str) -> bool {
        self.reported_gateways(controller)
            .any(|gw| gw == (gw_ns, gw_name))
    }
}

macro_rules! impl_route_status_reports {
    ($($ty:ty),*) => {$(
        impl StatusReports for $ty {
            fn reported_gateways<'a>(
                &'a self,
                controller: &'a str,
            ) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
                let own_ns = self.metadata.namespace.as_deref().unwrap_or("default");
                self.status
                    .iter()
                    .flat_map(|s| &s.parents)
                    .filter(move |p| p.controller_name == controller)
                    .map(move |p| {
                        (
                            p.parent_ref.namespace.as_deref().unwrap_or(own_ns),
                            p.parent_ref.name.as_str(),
                        )
                    })
            }
        }
    )*};
}

impl_route_status_reports!(
    gateway_api::httproutes::HTTPRoute,
    gateway_api::experimental::tcproutes::TCPRoute,
    gateway_api::experimental::tlsroutes::TLSRoute,
    gateway_api::experimental::udproutes::UDPRoute
);

impl StatusReports for RateLimitPolicy {
    fn reported_gateways<'a>(
        &'a self,
        controller: &'a str,
    ) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
        self.status
            .iter()
            .flat_map(|s| &s.ancestors)
            .filter(move |a| a.controller_name == controller)
            .map(|a| {
                (
                    a.ancestor_ref.namespace.as_str(),
                    a.ancestor_ref.name.as_str(),
                )
            })
    }
}

/// The Gateways among `candidates` that are in the store.
fn served<'a>(
    candidates: impl Iterator<Item = (&'a str, &'a str)>,
    store: &Store<Gateway>,
) -> Vec<ObjectRef<Gateway>> {
    let mut refs: Vec<ObjectRef<Gateway>> = Vec::new();
    for (ns, name) in candidates {
        let obj_ref = ObjectRef::<Gateway>::new(name).within(ns);
        if store.get(&obj_ref).is_some() && !refs.contains(&obj_ref) {
            refs.push(obj_ref);
        }
    }
    refs
}

/// Map a route to the Gateway(s) it targets via parentRefs, and to those
/// its status says we reported on: a parentRef moved away from a Gateway
/// must reconcile that Gateway too, to stop serving the route and remove
/// our entry.
pub(super) fn map_route_to_gateways<R>(
    route: &R,
    parent_refs: &Option<Vec<impl ParentRefAccess>>,
    store: &Store<Gateway>,
    controller: &str,
) -> Vec<ObjectRef<Gateway>>
where
    R: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope> + StatusReports,
{
    let route_ns = route.meta().namespace.as_deref().unwrap_or_default();
    let targeted = parent_refs
        .iter()
        .flatten()
        .filter(|pr| {
            pr.ref_group()
                .is_none_or(|g| g == "gateway.networking.k8s.io")
                && pr.ref_kind().is_none_or(|k| k == "Gateway")
        })
        .map(|pr| (pr.ref_namespace().unwrap_or(route_ns), pr.ref_name()));
    served(targeted.chain(route.reported_gateways(controller)), store)
}

/// Map a RateLimitPolicy to the Gateway its targetRef names (GEP-713 direct
/// attachment is same-namespace, so the Gateway namespace is the policy's),
/// and to the Gateways its status says we reported on, for the same reason
/// as routes.
pub(super) fn map_policy_to_gateways(
    policy: &RateLimitPolicy,
    store: &Store<Gateway>,
    controller: &str,
) -> Vec<ObjectRef<Gateway>> {
    let ns = policy.metadata.namespace.as_deref().unwrap_or_default();
    let t = &policy.spec.target_ref;
    let targeted = (t.kind == "Gateway"
        && (t.group.is_empty() || t.group == "gateway.networking.k8s.io"))
        .then_some((ns, t.name.as_str()));
    served(
        targeted
            .into_iter()
            .chain(policy.reported_gateways(controller)),
        store,
    )
}

/// Map a Secret to the Gateway(s) whose TLS certificateRefs name it.
pub(super) fn map_secret_to_gateways(
    secret: &Secret,
    store: &Store<Gateway>,
) -> Vec<ObjectRef<Gateway>> {
    let secret_name = secret.metadata.name.as_deref().unwrap_or("");
    let secret_ns = secret.metadata.namespace.as_deref().unwrap_or("default");
    store
        .state()
        .into_iter()
        .filter(|g| {
            let gw_ns = g.metadata.namespace.as_deref().unwrap_or("default");
            g.spec.listeners.iter().any(|l| {
                l.tls.as_ref().is_some_and(|tls| {
                    tls.certificate_refs.as_ref().is_some_and(|refs| {
                        refs.iter().any(|cr| {
                            cr.name == secret_name
                                && cr.namespace.as_deref().unwrap_or(gw_ns) == secret_ns
                        })
                    })
                })
            })
        })
        .map(|g| ObjectRef::from_obj(&*g))
        .collect()
}

/// Map any resource to ALL managed gateways (used for infrequently-changing
/// resources like Service, Namespace, ReferenceGrant where targeted mapping
/// adds complexity for negligible CPU gain).
pub(super) fn all_gateway_refs(store: &Store<Gateway>) -> Vec<ObjectRef<Gateway>> {
    store
        .state()
        .into_iter()
        .map(|gw| ObjectRef::from_obj(&*gw))
        .collect()
}

/// A boxed reflector stream: plain and optional-CRD watches share the type,
/// so the `Controller` wiring treats them alike.
pub(super) type BoxedReflectorStream<K> =
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<K, watcher::Error>> + Send + 'static>>;

/// Like `create_reflector`, with a caller-supplied watcher config: a field
/// selector etc. (e.g. the TLS Secret watch's `type=kubernetes.io/tls`).
pub(super) fn create_reflector_with_config<K>(
    api: Api<K>,
    config: watcher::Config,
    listings: &Listings,
) -> (Store<K>, BoxedReflectorStream<K>)
where
    K: kube::Resource
        + Clone
        + std::fmt::Debug
        + serde::de::DeserializeOwned
        + Send
        + Sync
        + 'static,
    K::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
    let dt = K::DynamicType::default();
    let tracker = listings.track(&K::kind(&dt), false);
    let writer = reflector::store::Writer::default();
    let reader = writer.as_reader();
    (reader, reflector_stream(writer, api, config, tracker))
}

fn reflector_stream<K>(
    writer: reflector::store::Writer<K>,
    api: Api<K>,
    config: watcher::Config,
    tracker: ListTracker,
) -> BoxedReflectorStream<K>
where
    K: kube::Resource
        + Clone
        + std::fmt::Debug
        + serde::de::DeserializeOwned
        + Send
        + Sync
        + 'static,
    K::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
    Box::pin(
        reflector::reflector(
            writer,
            watcher::watcher(api, config)
                .modify(drop_unread_metadata)
                .inspect(move |event| tracker.observe(event)),
        )
        .default_backoff()
        .touched_objects(), // touched_objects includes deletes; applied_objects drops them
    )
}

const LAST_APPLIED_ANNOTATION: &str = "kubectl.kubernetes.io/last-applied-configuration";

/// Drop metadata no reconcile reads before the reflector caches the object.
/// The controller caches every Service and TLS Secret in the cluster, and
/// managedFields often outweighs the spec. On a Secret, the last-applied
/// annotation repeats the certificate and key.
pub(super) fn drop_unread_metadata<K: kube::Resource>(obj: &mut K) {
    let meta = obj.meta_mut();
    meta.managed_fields = None;
    if let Some(annotations) = meta.annotations.as_mut() {
        annotations.remove(LAST_APPLIED_ANNOTATION);
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::{self, apply, MockApi, CONTROLLER};
    use super::*;
    use gateway_api::experimental::tcproutes::TCPRoute;

    fn served_gateways(names: &[&str]) -> Store<Gateway> {
        let mut writer = reflector::store::Writer::default();
        for name in names {
            apply(&mut writer, testkit::gateway(name, 1));
        }
        writer.as_reader()
    }

    fn names(refs: &[ObjectRef<Gateway>]) -> Vec<&str> {
        refs.iter().map(|r| r.name.as_str()).collect()
    }

    /// A route event reaches the Gateways its status says we reported on,
    /// not only the ones its parentRefs name now: moving a parentRef away
    /// must reconcile the Gateway it left.
    #[test]
    fn route_maps_to_named_and_reported_gateways() {
        let store = served_gateways(&["a", "b", "c"]);
        let mut route = testkit::http_route("r", &["b"], 1);
        route.status = Some(
            serde_json::from_value(serde_json::json!({ "parents": [
                testkit::status_entry(CONTROLLER, "a", "parentRef"),
                testkit::status_entry("example.com/other", "c", "parentRef"),
                testkit::status_entry(CONTROLLER, "not-served-here", "parentRef"),
            ]}))
            .unwrap(),
        );
        let refs = map_route_to_gateways(&route, &route.spec.parent_refs, &store, CONTROLLER);
        assert_eq!(names(&refs), vec!["b", "a"]);
    }

    #[test]
    fn policy_maps_to_target_and_reported_gateways() {
        let store = served_gateways(&["a", "b"]);
        let mut policy = testkit::rate_limit_policy("p", "b");
        policy.status = Some(
            serde_json::from_value(serde_json::json!({ "ancestors": [
                testkit::status_entry(CONTROLLER, "a", "ancestorRef"),
            ]}))
            .unwrap(),
        );
        let refs = map_policy_to_gateways(&policy, &store, CONTROLLER);
        assert_eq!(names(&refs), vec!["b", "a"]);
    }

    const TCPROUTE_GROUP: &str = "/apis/gateway.networking.k8s.io/v1alpha2";

    fn discovery_serves_tcproutes(api: &MockApi, served: bool) {
        if served {
            api.reply_to(
                TCPROUTE_GROUP,
                200,
                serde_json::json!({
                    "kind": "APIResourceList",
                    "apiVersion": "v1",
                    "groupVersion": "gateway.networking.k8s.io/v1alpha2",
                    "resources": [{
                        "name": "tcproutes",
                        "singularName": "tcproute",
                        "namespaced": true,
                        "kind": "TCPRoute",
                        "verbs": ["get", "list", "watch"],
                    }],
                }),
            );
        } else {
            api.reply_to(
                TCPROUTE_GROUP,
                404,
                serde_json::json!({
                    "kind": "Status", "apiVersion": "v1", "status": "Failure",
                    "reason": "NotFound", "code": 404, "message": "not found",
                }),
            );
        }
    }

    fn discovery_reads(api: &MockApi) -> usize {
        api.requests()
            .iter()
            .filter(|r| r.path.split('?').next() == Some(TCPROUTE_GROUP))
            .count()
    }

    fn tcproutes_watched(api: &MockApi) -> bool {
        let watch_path = format!("{TCPROUTE_GROUP}/tcproutes");
        api.requests()
            .iter()
            .any(|r| r.path.starts_with(&watch_path))
    }

    const RECHECK: Duration = Duration::from_secs(super::super::RESYNC_SECS);

    /// Lets in-process requests and the tasks they wake run to completion
    /// without moving the paused clock.
    async fn settle() {
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
    }

    fn status(code: u16, reason: &str) -> serde_json::Value {
        serde_json::json!({
            "kind": "Status", "apiVersion": "v1", "status": "Failure",
            "reason": reason, "code": code, "message": reason,
        })
    }

    /// A TCPRoute reflector of `api`'s, its stream driven.
    fn tcproutes(
        api: &MockApi,
        pending: &Arc<PendingWatches>,
        listings: &Listings,
    ) -> (Store<TCPRoute>, tokio::task::JoinHandle<()>) {
        let client = api.client();
        let (store, stream) = create_optional_reflector(
            Api::<TCPRoute>::all(client.clone()),
            &client,
            pending,
            listings,
        );
        (store, tokio::spawn(stream.for_each(|_| async {})))
    }

    /// A CRD installed after startup is watched from the next recheck, on
    /// the recheck's own timer. Rechecks ran inside reconcile only: with
    /// no Gateway to reconcile, never.
    #[tokio::test(start_paused = true)]
    async fn optional_crd_installed_later_starts_its_watch() {
        let api = MockApi::default();
        discovery_serves_tcproutes(&api, false);
        let pending = Arc::new(PendingWatches::default());
        let (_store, driver) = tcproutes(&api, &pending, &Listings::default());
        let shutdown = tokio_util::sync::CancellationToken::new();
        tokio::spawn(pending.recheck_every(api.client(), RECHECK, shutdown.clone()));
        settle().await;
        assert!(!tcproutes_watched(&api), "no watch before the CRD exists");

        discovery_serves_tcproutes(&api, true);
        tokio::time::sleep(RECHECK).await;
        settle().await;
        assert!(
            tcproutes_watched(&api),
            "the watch starts once discovery serves the CRD"
        );
        api.clear();
        tokio::time::sleep(RECHECK).await;
        settle().await;
        assert_eq!(discovery_reads(&api), 0, "nothing waits: no more rechecks");
        shutdown.cancel();
        driver.abort();
    }

    /// A missing CRD counts as listed: the caches would otherwise never be
    /// synced on a cluster without it.
    #[tokio::test(start_paused = true)]
    async fn a_missing_crd_counts_as_listed() {
        let api = MockApi::default();
        discovery_serves_tcproutes(&api, false);
        let listings = Listings::default();
        let (store, _driver) = tcproutes(&api, &Arc::default(), &listings);
        assert!(!listings.is_synced(), "not before discovery answered");
        settle().await;
        assert!(listings.is_synced(), "{:?}", listings.unlisted());
        assert!(store.is_empty());
    }

    /// A failed discovery says neither served nor not: it is retried with
    /// backoff, and the kind waits unlisted, named with the failure. It
    /// started a watch blindly, which 404ed forever on a cluster without
    /// the CRD and held the caches unsynced.
    #[tokio::test(start_paused = true)]
    async fn a_failed_discovery_retries_then_starts_the_watch() {
        let api = MockApi::default();
        api.reply_to(TCPROUTE_GROUP, 504, status(504, "Timeout"));
        let listings = Listings::default();
        let (_store, driver) = tcproutes(&api, &Arc::default(), &listings);
        settle().await;
        assert!(!tcproutes_watched(&api), "no watch on a failed discovery");
        let unlisted = listings.unlisted();
        assert_eq!(unlisted.len(), 1);
        assert_eq!(unlisted[0].0, "TCPRoute");
        assert!(unlisted[0].1.contains("discovery"), "{unlisted:?}");

        discovery_serves_tcproutes(&api, true);
        // Past the longest delay of kube's watcher backoff.
        tokio::time::sleep(Duration::from_secs(RETRY_CAP_SECS)).await;
        settle().await;
        assert!(discovery_reads(&api) >= 2, "retried");
        assert!(
            tcproutes_watched(&api),
            "the watch starts once discovery answers"
        );
        driver.abort();
    }

    /// An optional kind whose list is refused, gone (404) or not granted
    /// (403), counts as listed and empty; its watch keeps retrying.
    #[tokio::test(start_paused = true)]
    async fn an_optional_list_refused_counts_as_listed() {
        for (code, reason) in [(404, "NotFound"), (403, "Forbidden")] {
            let api = MockApi::default();
            // First: the longer prefix must win over discovery's.
            api.reply_to(
                &format!("{TCPROUTE_GROUP}/tcproutes"),
                code,
                status(code, reason),
            );
            discovery_serves_tcproutes(&api, true);
            let listings = Listings::default();
            let (store, driver) = tcproutes(&api, &Arc::default(), &listings);
            settle().await;
            assert!(tcproutes_watched(&api), "{code}");
            assert!(listings.is_synced(), "{code}: {:?}", listings.unlisted());
            assert!(store.is_empty());
            driver.abort();
        }
    }

    /// Every waiter of `synced` wakes when the last kind lists, alongside
    /// the one waiter of `changed`.
    #[tokio::test]
    async fn every_synced_waiter_wakes() {
        use gateway_api::httproutes::HTTPRoute;
        let listings = Listings::default();
        let routes = listings.track("HTTPRoute", false);
        let waiters: Vec<_> = (0..2)
            .map(|_| {
                let listings = listings.clone();
                tokio::spawn(async move { listings.synced().await })
            })
            .collect();
        tokio::task::yield_now().await;
        assert!(waiters.iter().all(|w| !w.is_finished()));

        routes.observe::<HTTPRoute>(&Ok(watcher::Event::InitDone));
        listings.changed().await;
        for waiter in waiters {
            tokio::time::timeout(Duration::from_secs(5), waiter)
                .await
                .expect("woken")
                .unwrap();
        }
        listings.synced().await;
    }

    /// A core kind that cannot list holds the caches unsynced, and is
    /// named with why.
    #[tokio::test(start_paused = true)]
    async fn a_core_list_refused_keeps_the_caches_unsynced() {
        use gateway_api::httproutes::HTTPRoute;
        let api = MockApi::default();
        api.reply_to(
            "/apis/gateway.networking.k8s.io/v1/httproutes",
            403,
            status(403, "Forbidden"),
        );
        let listings = Listings::default();
        let (_store, stream) = create_reflector(Api::<HTTPRoute>::all(api.client()), &listings);
        let driver = tokio::spawn(stream.for_each(|_| async {}));
        settle().await;
        assert!(!listings.is_synced());
        let unlisted = listings.unlisted();
        assert_eq!(unlisted.len(), 1);
        assert_eq!(unlisted[0].0, "HTTPRoute");
        assert!(
            unlisted[0].1.contains("403") || unlisted[0].1.contains("Forbidden"),
            "{unlisted:?}"
        );
        driver.abort();
    }

    /// Discovery is read once per resync interval while a CRD is missing.
    #[tokio::test(start_paused = true)]
    async fn discovery_is_read_once_per_interval() {
        let api = MockApi::default();
        discovery_serves_tcproutes(&api, false);
        let pending = Arc::new(PendingWatches::default());
        let _reflector = tcproutes(&api, &pending, &Listings::default());
        settle().await;
        assert_eq!(discovery_reads(&api), 1, "the startup check");
        let shutdown = tokio_util::sync::CancellationToken::new();
        let rechecks = tokio::spawn(pending.recheck_every(api.client(), RECHECK, shutdown.clone()));

        tokio::time::sleep(RECHECK - Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(discovery_reads(&api), 1, "not before the interval");
        tokio::time::sleep(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(discovery_reads(&api), 2, "once at the interval");

        shutdown.cancel();
        settle().await;
        assert!(rechecks.is_finished());
    }

    #[test]
    fn cached_objects_drop_managed_fields_and_last_applied() {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::ManagedFieldsEntry;
        let mut secret = Secret::default();
        secret.metadata.managed_fields = Some(vec![ManagedFieldsEntry::default()]);
        secret.metadata.annotations = Some(
            [
                (
                    LAST_APPLIED_ANNOTATION.to_string(),
                    "{\"data\":{}}".to_string(),
                ),
                ("keep".to_string(), "me".to_string()),
            ]
            .into(),
        );
        drop_unread_metadata(&mut secret);
        assert!(secret.metadata.managed_fields.is_none());
        let annotations = secret.metadata.annotations.unwrap();
        assert!(!annotations.contains_key(LAST_APPLIED_ANNOTATION));
        assert_eq!(annotations["keep"], "me");
    }
}
