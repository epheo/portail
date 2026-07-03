//! Reflector/watch plumbing: store+stream construction (plain, optional-CRD,
//! and shape-gated variants), the secondary-resource → Gateway mappers, and
//! the operator-provided `WatchShape` that decides which gate-able watches a
//! scoped data plane opens at all.

use kube::api::Api;
use kube::runtime::reflector::{ObjectRef, Store};
use kube::runtime::watcher;
use kube::runtime::{reflector, WatchStreamExt};
use kube::ResourceExt;

use gateway_api::gateways::Gateway;

use crate::kubernetes::parent_ref::ParentRefAccess;
use crate::logging::warn;

/// Create a reflector for a resource type, returning the store and a stream.
/// The stream is consumed by `Controller::watches_stream()` which both drives
/// the reflector (keeping the store up to date) and triggers reconciliation
/// of mapped Gateway(s) when resources change.
pub(super) fn create_reflector<K>(
    api: Api<K>,
) -> (
    Store<K>,
    impl futures::Stream<Item = Result<K, watcher::Error>> + Send + 'static,
)
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
    let writer = reflector::store::Writer::default();
    let reader = writer.as_reader();
    // Stream the initial list via the WatchList API instead of a single blocking
    // LIST. Each per-Gateway pod cold-starts by listing ~6 cluster-wide resource
    // types; with N pods watching + route/Service churn, concurrent blocking LISTs
    // saturate the apiserver and a fresh pod's reflector sync stalls (measured 1s
    // isolated → 20s+/never under load), which is the cold-start convergence flake.
    // Streaming lists are far lighter on the apiserver under that load. Requires
    // server WatchList support (k8s >= 1.27; default-on >= 1.30).
    let stream = reflector::reflector(
        writer,
        watcher::watcher(api, watcher::Config::default().streaming_lists()),
    )
    .default_backoff()
    .touched_objects(); // touched_objects includes deletes; applied_objects drops them
    (reader, stream)
}

/// Like `create_reflector`, but first probes the API endpoint with a lightweight
/// list request. If the CRD is not installed (404), returns an empty store and
/// a stream that never yields, so the controller can still start without it.
pub(super) async fn create_optional_reflector<K>(
    api: Api<K>,
) -> (
    Store<K>,
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<K, watcher::Error>> + Send + 'static>>,
)
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
            let writer = reflector::store::Writer::default();
            let reader = writer.as_reader();
            (reader, Box::pin(futures::stream::pending()))
        }
        _ => {
            let (store, stream) = create_reflector(api);
            (store, Box::pin(stream))
        }
    }
}

// ---------------------------------------------------------------------------
// Mapper functions: map secondary resource events to Gateway ObjectRef(s)
// ---------------------------------------------------------------------------

/// Map a route to the Gateway(s) it targets via parentRefs.
pub(super) fn map_route_to_gateways<
    R: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope>,
>(
    route: &R,
    parent_refs: &Option<Vec<impl ParentRefAccess>>,
    store: &Store<Gateway>,
) -> Vec<ObjectRef<Gateway>> {
    let route_ns = route.namespace().unwrap_or_default();
    let mut refs = Vec::new();
    if let Some(prs) = parent_refs {
        for pr in prs {
            if pr
                .ref_group()
                .is_some_and(|g| g != "gateway.networking.k8s.io")
            {
                continue;
            }
            if pr.ref_kind().is_some_and(|k| k != "Gateway") {
                continue;
            }
            let gw_ns = pr.ref_namespace().unwrap_or(&route_ns);
            let obj_ref = ObjectRef::<Gateway>::new(pr.ref_name()).within(gw_ns);
            if store.get(&obj_ref).is_some() {
                refs.push(obj_ref);
            }
        }
    }
    refs
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
        let writer = reflector::store::Writer::default();
        return (writer.as_reader(), Box::pin(futures::stream::pending()));
    }
    let writer = reflector::store::Writer::default();
    let reader = writer.as_reader();
    let stream = reflector::reflector(writer, watcher::watcher(api, config))
        .default_backoff()
        .touched_objects();
    (reader, Box::pin(stream))
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
    use super::*;

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
