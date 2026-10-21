//! Reflector/watch plumbing: store+stream construction (plain, optional-CRD,
//! and shape-gated variants), the secondary-resource → Gateway mappers, and
//! the operator-provided `WatchShape` that decides which gate-able watches a
//! scoped data plane opens at all.

use k8s_openapi::api::core::v1::Secret;
use kube::api::Api;
use kube::runtime::reflector::{ObjectRef, Store};
use kube::runtime::watcher;
use kube::runtime::{reflector, WatchStreamExt};

use gateway_api::gateways::Gateway;

use crate::kubernetes::crds::RateLimitPolicy;
use crate::kubernetes::parent_ref::ParentRefAccess;
use crate::logging::warn;

/// Create a reflector for a resource type, returning the store and a stream.
/// The stream is consumed by `Controller::watches_stream()` which both drives
/// the reflector (keeping the store up to date) and triggers reconciliation
/// of mapped Gateway(s) when resources change.
///
/// Streams the initial list via the WatchList API instead of a single blocking
/// LIST. Each per-Gateway pod cold-starts by listing ~6 cluster-wide resource
/// types; with N pods watching + route/Service churn, concurrent blocking LISTs
/// saturate the apiserver and a fresh pod's reflector sync stalls (measured 1s
/// isolated → 20s+/never under load), which is the cold-start convergence flake.
/// Streaming lists are far lighter on the apiserver under that load. Requires
/// server WatchList support (k8s >= 1.27; default-on >= 1.30).
pub(super) fn create_reflector<K>(api: Api<K>) -> (Store<K>, BoxedReflectorStream<K>)
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
    create_reflector_gated_with_config(true, api, watcher::Config::default().streaming_lists())
}

/// An empty store and a stream that never yields; no LIST/WATCH is opened.
fn empty_reflector<K>() -> (Store<K>, BoxedReflectorStream<K>)
where
    K: kube::Resource + Clone + Send + 'static,
    K::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
    let writer = reflector::store::Writer::default();
    (writer.as_reader(), Box::pin(futures::stream::pending()))
}

/// Like `create_reflector`, but first probes the API endpoint with a lightweight
/// list request. If the CRD is not installed (404), returns an empty store and
/// a stream that never yields, so the controller can still start without it.
pub(super) async fn create_optional_reflector<K>(api: Api<K>) -> (Store<K>, BoxedReflectorStream<K>)
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
    // Probe with limit=0 — just checks the endpoint exists
    let probe = api.list(&kube::api::ListParams::default().limit(1)).await;
    match probe {
        Err(kube::Error::Api(ref status)) if status.code == 404 => {
            let kind = std::any::type_name::<K>()
                .rsplit("::")
                .next()
                .unwrap_or("Unknown");
            warn!("CRD not installed, skipping watcher: {kind}");
            empty_reflector()
        }
        _ => create_reflector(api),
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

/// The served Gateways among `candidates`. The store check is what makes
/// scoped pods ignore objects for Gateways they do not serve.
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

/// Which *gate-able* secondary resources a single-Gateway data plane needs to
/// watch. Derived by portail-operator from the Gateway's listeners and passed
/// via `--watch-shape`, so a scoped pod skips cluster-wide watches it will never
/// consume. The default (`--watch-shape` absent → `WatchShape::all()`) keeps the
/// legacy broad behavior, so an older operator that does not set the flag still
/// works unchanged.
///
/// IMPORTANT: only resources that never `parentRef` a Gateway may be gated.
/// Route watches (HTTP/TCP/TLS/UDP) are deliberately NOT here: a route in any
/// namespace, of any kind, may `parentRef` this Gateway and must receive a
/// status — attached, or rejected with `NotAllowedByListeners` /
/// `NoMatchingParent` — which requires the data plane to observe it. Secrets and
/// Namespace labels do not `parentRef` Gateways, so they are safe to drop.
#[derive(Clone, Copy, Debug)]
pub struct WatchShape {
    pub tls_secrets: bool,
    pub namespaces: bool,
}

impl WatchShape {
    /// Watch every gate-able secondary resource (legacy / unscoped mode).
    pub fn all() -> Self {
        Self {
            tls_secrets: true,
            namespaces: true,
        }
    }

    /// Parse the operator's `--watch-shape` token set. `None` → `all()` (broad,
    /// legacy). `Some(spec)` → enable only the listed gate-able watches: `tls`
    /// (a TLS-terminate listener) and `ns-labels` (an `allowedRoutes: Selector`
    /// listener).
    pub fn parse(spec: Option<&str>) -> Self {
        let Some(spec) = spec else {
            return Self::all();
        };
        let mut s = Self {
            tls_secrets: false,
            namespaces: false,
        };
        for token in spec.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            match token {
                "tls" => s.tls_secrets = true,
                "ns-labels" => s.namespaces = true,
                other => warn!("ignoring unknown --watch-shape token: {other}"),
            }
        }
        s
    }
}

/// A boxed reflector stream — the uniform type used by gated watches so the
/// `Controller` wiring stays identical whether or not the watch is active.
pub(super) type BoxedReflectorStream<K> =
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<K, watcher::Error>> + Send + 'static>>;

/// Like `create_reflector` but gated, with a caller-supplied watcher config:
/// when `enabled` is false, returns an empty store and a stream that never yields
/// — the controller opens no LIST/WATCH for it — while keeping the `Controller`
/// wiring uniform. `config` lets callers add a field selector etc. (e.g. the TLS
/// Secret watch's `type=kubernetes.io/tls`).
pub(super) fn create_reflector_gated_with_config<K>(
    enabled: bool,
    api: Api<K>,
    config: watcher::Config,
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
    if !enabled {
        return empty_reflector();
    }
    let writer = reflector::store::Writer::default();
    let reader = writer.as_reader();
    let stream = reflector::reflector(
        writer,
        watcher::watcher(api, config).modify(drop_unread_metadata),
    )
    .default_backoff()
    .touched_objects(); // touched_objects includes deletes; applied_objects drops them
    (reader, Box::pin(stream))
}

const LAST_APPLIED_ANNOTATION: &str = "kubectl.kubernetes.io/last-applied-configuration";

/// Drop metadata no reconcile reads before the reflector caches the object.
/// Every pod caches every Service and TLS Secret in the cluster, and
/// managedFields often outweighs the spec. On a Secret, the last-applied
/// annotation repeats the certificate and key.
pub(super) fn drop_unread_metadata<K: kube::Resource>(obj: &mut K) {
    let meta = obj.meta_mut();
    meta.managed_fields = None;
    if let Some(annotations) = meta.annotations.as_mut() {
        annotations.remove(LAST_APPLIED_ANNOTATION);
    }
}

/// The gated counterpart of `create_reflector`: same default streaming-list
/// config, but opens no LIST/WATCH when `enabled` is false.
pub(super) fn create_reflector_gated<K>(
    enabled: bool,
    api: Api<K>,
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
    create_reflector_gated_with_config(enabled, api, watcher::Config::default().streaming_lists())
}

#[cfg(test)]
mod tests {
    use super::super::testkit::{self, apply, CONTROLLER};
    use super::*;

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

    #[test]
    fn watch_shape_parse() {
        // Absent → broad (legacy): every gate-able watch on.
        let all = WatchShape::parse(None);
        assert!(all.tls_secrets && all.namespaces);

        // Present-but-empty → minimal: no gate-able extras (routes stay
        // cluster-wide regardless — they are never gated).
        let min = WatchShape::parse(Some(""));
        assert!(!min.tls_secrets && !min.namespaces);

        // TLS-terminate listener only.
        let tls = WatchShape::parse(Some("tls"));
        assert!(tls.tls_secrets && !tls.namespaces);

        // Unknown / retired tokens (e.g. the old route-scoping tokens) are
        // ignored; recognized ones still apply.
        let u = WatchShape::parse(Some("routes-same, udp-routes, bogus , ns-labels"));
        assert!(u.namespaces && !u.tls_secrets);
    }
}
