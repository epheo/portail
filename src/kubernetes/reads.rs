//! What each Gateway's reconcile pass read, so a change to an object wakes
//! the Gateways that read it, not every Gateway.
//!
//! A pass reads the stores of the kinds below only through [`Recorded`],
//! which notes each read into the running pass's set as it happens: an
//! event that lands while the pass still runs finds the read already
//! there. A read outside any pass (sync serving, reports) notes nothing.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use kube::runtime::reflector::{ObjectRef, Store};
use kube::Resource;

use crate::sync::GatewayKey;

/// The kinds whose events wake only the Gateways that read them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    Service,
    Namespace,
    ReferenceGrant,
    GatewayParameters,
    BackendTlsPolicy,
    ConfigMap,
}

/// What one read covered.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Read {
    /// One object by name, found or not: a missing one wakes its reader
    /// when it is created. A cluster-scoped kind has an empty namespace.
    Object(Kind, String, String),
    /// Every object of the kind in one namespace.
    InNamespace(Kind, String),
    /// Every object of the kind.
    All(Kind),
}

impl Read {
    fn covers(&self, kind: Kind, namespace: &str, name: &str) -> bool {
        match self {
            Read::Object(k, ns, n) => *k == kind && ns == namespace && n == name,
            Read::InNamespace(k, ns) => *k == kind && ns == namespace,
            Read::All(k) => *k == kind,
        }
    }
}

#[derive(Default)]
struct Entry {
    /// What the last pass that succeeded read, and what failed ones added.
    last: HashSet<Read>,
    /// What the running pass has read so far.
    running: Option<HashSet<Read>>,
}

/// Each Gateway's reads: its last pass's, and its running pass's so far.
#[derive(Default)]
pub(crate) struct ReadIndex {
    gateways: Mutex<HashMap<GatewayKey, Entry>>,
}

impl ReadIndex {
    fn lock(&self) -> MutexGuard<'_, HashMap<GatewayKey, Entry>> {
        self.gateways.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Run `pass` as `gateway`'s pass: what it reads through [`Recorded`]
    /// is noted as it happens. One that succeeds leaves its reads as the
    /// Gateway's. One that fails or is dropped (shutdown) adds them to the
    /// last: it may have stopped before reading what the last pass did.
    ///
    /// A pass that ends early succeeds with no reads: caches unsynced, a
    /// standby, a class not ours or missing. That set is safe only because
    /// each of those ends on an event that re-runs every Gateway: the
    /// caches' sync, `leader.gained()`, a GatewayClass change. Drop one of
    /// those wakes and its Gateways sleep through the changes they read.
    pub(crate) async fn during<T, E>(
        self: &Arc<Self>,
        gateway: GatewayKey,
        pass: impl std::future::Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        self.lock().entry(gateway.clone()).or_default().running = Some(HashSet::new());
        let running = Running {
            index: self.clone(),
            gateway,
            succeeded: std::sync::atomic::AtomicBool::new(false),
        };
        let running = Arc::new(running);
        let out = PASS.scope(running.clone(), pass).await;
        running
            .succeeded
            .store(out.is_ok(), std::sync::atomic::Ordering::Relaxed);
        out
    }

    fn record(&self, gateway: &GatewayKey, read: Read) {
        if let Some(running) = self
            .lock()
            .get_mut(gateway)
            .and_then(|e| e.running.as_mut())
        {
            running.insert(read);
        }
    }

    fn end(&self, gateway: &GatewayKey, succeeded: bool) {
        if let Some(entry) = self.lock().get_mut(gateway) {
            if let Some(reads) = entry.running.take() {
                if succeeded {
                    entry.last = reads;
                } else {
                    entry.last.extend(reads);
                }
            }
        }
    }

    /// Drop the reads of the Gateways `live` refuses: they are gone.
    pub(crate) fn retain(&self, live: impl Fn(&GatewayKey) -> bool) {
        self.lock().retain(|key, _| live(key));
    }

    /// The Gateways among `gateways` that read `kind` `namespace`/`name`,
    /// in their last pass or their running one, and those with no pass
    /// recorded yet: a Gateway never read has no set to rule it out.
    pub(crate) fn readers<'a>(
        &self,
        kind: Kind,
        namespace: &str,
        name: &str,
        gateways: impl IntoIterator<Item = &'a GatewayKey>,
    ) -> Vec<GatewayKey> {
        let index = self.lock();
        gateways
            .into_iter()
            .filter(|key| match index.get(key) {
                None => true,
                Some(entry) => entry
                    .last
                    .iter()
                    .chain(entry.running.iter().flatten())
                    .any(|read| read.covers(kind, namespace, name)),
            })
            .cloned()
            .collect()
    }
}

/// The pass of the task polling it, if any.
struct Running {
    index: Arc<ReadIndex>,
    gateway: GatewayKey,
    succeeded: std::sync::atomic::AtomicBool,
}

impl Drop for Running {
    fn drop(&mut self) {
        let succeeded = self.succeeded.load(std::sync::atomic::Ordering::Relaxed);
        self.index.end(&self.gateway, succeeded);
    }
}

tokio::task_local! {
    static PASS: Arc<Running>;
}

/// Note `read` into the running pass, if this task runs one.
fn record(read: Read) {
    let _ = PASS.try_with(|pass| pass.index.record(&pass.gateway, read));
}

/// A failed translation read an unknown part of what it would have: every
/// object of the four kinds wakes the Gateway until a pass translates.
pub(crate) fn record_everything() {
    for kind in [
        Kind::Service,
        Kind::Namespace,
        Kind::ReferenceGrant,
        Kind::GatewayParameters,
        Kind::BackendTlsPolicy,
        Kind::ConfigMap,
    ] {
        record(Read::All(kind));
    }
}

/// A store of a recorded kind: the only way a pass reads it.
pub(crate) struct Recorded<K: Resource<DynamicType = ()> + Clone + 'static> {
    store: Store<K>,
    kind: Kind,
}

impl<K: Resource<DynamicType = ()> + Clone + 'static> Clone for Recorded<K> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            kind: self.kind,
        }
    }
}

impl<K: Resource<DynamicType = ()> + Clone + 'static> Recorded<K> {
    pub(crate) fn new(store: Store<K>, kind: Kind) -> Self {
        Self { store, kind }
    }

    /// `namespace`/`name`, noted whether it exists or not.
    pub(crate) fn get(&self, namespace: &str, name: &str) -> Option<Arc<K>> {
        record(Read::Object(self.kind, namespace.into(), name.into()));
        self.store.get(&ObjectRef::new(name).within(namespace))
    }

    /// A cluster-scoped object by name, noted whether it exists or not.
    pub(crate) fn get_cluster(&self, name: &str) -> Option<Arc<K>> {
        record(Read::Object(self.kind, String::new(), name.into()));
        self.store.get(&ObjectRef::new(name))
    }

    /// Every object of `namespace`.
    pub(crate) fn in_namespace(&self, namespace: &str) -> Vec<Arc<K>> {
        record(Read::InNamespace(self.kind, namespace.into()));
        self.store
            .state()
            .into_iter()
            .filter(|o| o.meta().namespace.as_deref().unwrap_or("default") == namespace)
            .collect()
    }

    /// Every object: the Gateway then wakes on any change of the kind.
    #[cfg(test)]
    pub(crate) fn all(&self) -> Vec<Arc<K>> {
        record(Read::All(self.kind));
        self.store.state()
    }

    /// Every object, noted nowhere: for bookkeeping no output of a pass
    /// depends on, such as pruning a cache by what is still named, or for
    /// objects another mapping already wakes the pass on.
    pub(crate) fn all_unrecorded(&self) -> Vec<Arc<K>> {
        self.store.state()
    }
}

#[cfg(test)]
impl<K: Resource<DynamicType = ()> + Clone + 'static> Recorded<K> {
    /// A store holding `objects`.
    pub(crate) fn of(kind: Kind, objects: impl IntoIterator<Item = K>) -> Self {
        let mut writer = kube::runtime::reflector::store::Writer::default();
        for o in objects {
            writer.apply_watcher_event(&kube::runtime::watcher::Event::Apply(o));
        }
        Self::new(writer.as_reader(), kind)
    }
}

/// A store of ReferenceGrants holding `grants`.
#[cfg(test)]
pub(crate) fn grants(
    grants: impl IntoIterator<Item = gateway_api::referencegrants::v1beta1::ReferenceGrant>,
) -> Recorded<gateway_api::referencegrants::v1beta1::ReferenceGrant> {
    Recorded::of(Kind::ReferenceGrant, grants)
}

/// A store of Namespaces with these labels, by name.
#[cfg(test)]
pub(crate) fn namespaces(
    labels: &HashMap<String, std::collections::BTreeMap<String, String>>,
) -> Recorded<k8s_openapi::api::core::v1::Namespace> {
    Recorded::of(
        Kind::Namespace,
        labels.iter().map(|(name, labels)| {
            let mut ns = k8s_openapi::api::core::v1::Namespace::default();
            ns.metadata.name = Some(name.clone());
            ns.metadata.labels = Some(labels.clone());
            ns
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::Service;

    fn key(name: &str) -> GatewayKey {
        GatewayKey::new("ns", name)
    }

    /// `reads` as `gw`'s pass, which succeeds.
    async fn passed<T>(
        index: &Arc<ReadIndex>,
        gw: &GatewayKey,
        reads: impl std::future::Future<Output = T>,
    ) -> T {
        let pass = async { Ok::<_, ()>(reads.await) };
        index.during(gw.clone(), pass).await.unwrap()
    }

    fn service(ns: &str, name: &str) -> Service {
        let mut s = Service::default();
        s.metadata.namespace = Some(ns.into());
        s.metadata.name = Some(name.into());
        s
    }

    /// The readers of each shape of read, and a Gateway with no pass yet.
    #[tokio::test]
    async fn an_event_wakes_the_gateways_that_read_it() {
        let index = Arc::new(ReadIndex::default());
        let services = Recorded::of(Kind::Service, [service("a", "one")]);
        let (exact, in_ns, all, none) = (key("exact"), key("in-ns"), key("all"), key("none"));
        passed(&index, &exact, async { services.get("a", "one") }).await;
        passed(&index, &in_ns, async { services.in_namespace("a") }).await;
        passed(&index, &all, async { services.all() }).await;
        let gateways = [exact.clone(), in_ns.clone(), all.clone(), none.clone()];

        let woken = |ns: &str, name: &str| index.readers(Kind::Service, ns, name, &gateways);
        assert_eq!(woken("a", "one"), gateways);
        assert_eq!(
            woken("a", "two"),
            [in_ns.clone(), all.clone(), none.clone()]
        );
        assert_eq!(woken("b", "one"), [all.clone(), none.clone()]);
        assert_eq!(
            index.readers(Kind::Namespace, "", "a", &gateways),
            [none],
            "a Service read says nothing of Namespaces"
        );
    }

    /// A Service looked up and missing wakes its reader once created.
    #[tokio::test]
    async fn a_missing_object_is_read_too() {
        let index = Arc::new(ReadIndex::default());
        let services = Recorded::<Service>::of(Kind::Service, []);
        let gw = key("gw");
        assert!(passed(&index, &gw, async { services.get("a", "late") })
            .await
            .is_none());
        assert_eq!(index.readers(Kind::Service, "a", "late", [&gw]), [gw]);
    }

    /// An event during a pass sees what the pass has read so far: the
    /// last pass never read X, the running one did, and X's change wakes
    /// the Gateway before the pass ends.
    #[tokio::test]
    async fn an_event_during_a_pass_sees_its_reads_so_far() {
        let index = Arc::new(ReadIndex::default());
        let services = Recorded::of(Kind::Service, [service("a", "x")]);
        let gw = key("gw");
        passed(&index, &gw, async {}).await;
        assert!(index.readers(Kind::Service, "a", "x", [&gw]).is_empty());

        let (read, release) = (tokio::sync::Notify::new(), tokio::sync::Notify::new());
        let pass = passed(&index, &gw, async {
            services.get("a", "x");
            read.notify_one();
            release.notified().await;
        });
        let check = async {
            read.notified().await;
            let woken = index.readers(Kind::Service, "a", "x", [&gw]);
            release.notify_one();
            woken
        };
        let ((), woken) = tokio::join!(pass, check);
        assert_eq!(
            woken,
            std::slice::from_ref(&gw),
            "woken while the pass still runs"
        );
        assert_eq!(index.readers(Kind::Service, "a", "x", [&gw]), [gw]);
    }

    /// A finished pass replaces the last pass's reads: what it no longer
    /// read wakes it no more.
    #[tokio::test]
    async fn a_finished_pass_replaces_the_last_reads() {
        let index = Arc::new(ReadIndex::default());
        let services = Recorded::of(Kind::Service, [service("a", "old"), service("a", "new")]);
        let gw = key("gw");
        passed(&index, &gw, async { services.get("a", "old") }).await;
        passed(&index, &gw, async { services.get("a", "new") }).await;
        assert!(index.readers(Kind::Service, "a", "old", [&gw]).is_empty());
        assert_eq!(index.readers(Kind::Service, "a", "new", [&gw]), [gw]);
    }

    /// A pass that fails may have stopped short of what the last one read:
    /// both still wake the Gateway.
    #[tokio::test]
    async fn a_failed_pass_keeps_the_last_reads() {
        let index = Arc::new(ReadIndex::default());
        let services = Recorded::of(Kind::Service, [service("a", "old"), service("a", "new")]);
        let gw = key("gw");
        passed(&index, &gw, async { services.get("a", "old") }).await;
        let failed = async {
            services.get("a", "new");
            Err::<(), _>("apiserver")
        };
        assert!(index.during(gw.clone(), failed).await.is_err());
        let gws = std::slice::from_ref(&gw);
        assert_eq!(index.readers(Kind::Service, "a", "old", [&gw]), gws);
        assert_eq!(index.readers(Kind::Service, "a", "new", [&gw]), gws);
    }

    /// A translation that fails records every kind: it is woken by anything.
    #[tokio::test]
    async fn a_failed_translation_is_woken_by_anything() {
        let index = Arc::new(ReadIndex::default());
        let gw = key("gw");
        passed(&index, &gw, async { record_everything() }).await;
        for kind in [
            Kind::Service,
            Kind::Namespace,
            Kind::ReferenceGrant,
            Kind::GatewayParameters,
        ] {
            assert_eq!(
                index.readers(kind, "any", "thing", [&gw]),
                std::slice::from_ref(&gw)
            );
        }
    }

    /// Outside a pass, a read notes nothing.
    #[test]
    fn a_read_outside_a_pass_notes_nothing() {
        let index = ReadIndex::default();
        let services = Recorded::of(Kind::Service, [service("a", "one")]);
        assert!(services.get("a", "one").is_some());
        assert!(index.lock().is_empty());
    }
}
