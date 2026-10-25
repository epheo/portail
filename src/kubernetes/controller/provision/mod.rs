//! A Managed Gateway's data-plane objects: its ServiceAccount, Deployment,
//! Service and PodDisruptionBudget, named by `naming::resource_name`,
//! server-side applied, owned by the Gateway.
//!
//! Ours is an object with a controller ownerReference to the Gateway's uid.
//! Any other object under a wanted name is foreign and stops the whole
//! apply: no stray LoadBalancer next to a hand-written Deployment. Only
//! objects that are ours and labeled managed-by portail-controller are ever
//! deleted.

mod ports;
mod render;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use futures::stream::BoxStream;
use futures::StreamExt;
use gateway_api::gateways::Gateway;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{Service, ServiceAccount};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use k8s_openapi::NamespaceResourceScope;
use kube::api::{
    Api, ApiResource, DeleteParams, DynamicObject, Patch, PatchParams, Preconditions,
    PropagationPolicy,
};
use kube::core::PartialObjectMeta;
use kube::runtime::reflector::store::Writer;
use kube::runtime::reflector::{ObjectRef, Store};
use kube::runtime::{metadata_watcher, reflector, watcher, WatchStreamExt};
use kube::{Client, Resource, ResourceExt};
use serde::Serialize;

use super::params::Params;
use super::watch::{drop_unread_metadata, Listings};
use super::ControllerCtx;
use crate::config::PortailConfig;
use crate::kubernetes::crds::Provisioning;
use crate::kubernetes::naming::{resource_name, FIELD_MANAGER, LABEL_MANAGED_BY, MANAGED_BY};
use crate::logging::{info, warn};
use crate::sync::{sha256_hex, Transport};

pub(crate) use render::ControllerEndpoint;

/// What provisioning left for the snapshot and the status.
pub(crate) struct Provisioned {
    /// The port bound per (transport, published port) where a Service maps
    /// one onto the other: the snapshot listener's `targetPort`.
    pub bound_ports: BTreeMap<(Transport, u16), u16>,
    /// Why the data plane cannot be provisioned, e.g. a name conflict:
    /// Programmed=False reason Invalid.
    pub problem: Option<String>,
}

impl Provisioned {
    /// Nothing provisioned: listeners bind their published ports.
    pub(crate) fn unmanaged() -> Self {
        Self {
            bound_ports: BTreeMap::new(),
            problem: None,
        }
    }

    /// Give `config`'s listeners their bound ports.
    pub(crate) fn stamp(&self, config: &mut PortailConfig) {
        for l in &mut config.gateway.listeners {
            l.target_port = self
                .bound_ports
                .get(&(Transport::of(&l.protocol), l.port))
                .copied();
        }
    }
}

/// The kinds provisioned, in apply order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Kind {
    ServiceAccount,
    Deployment,
    Service,
    PodDisruptionBudget,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::ServiceAccount => "ServiceAccount",
            Kind::Deployment => "Deployment",
            Kind::Service => "Service",
            Kind::PodDisruptionBudget => "PodDisruptionBudget",
        }
    }

    fn resource(self) -> ApiResource {
        match self {
            Kind::ServiceAccount => ApiResource::erase::<ServiceAccount>(&()),
            Kind::Deployment => ApiResource::erase::<Deployment>(&()),
            Kind::Service => ApiResource::erase::<Service>(&()),
            Kind::PodDisruptionBudget => ApiResource::erase::<PodDisruptionBudget>(&()),
        }
    }
}

/// An object as this controller last applied it: while the live one keeps
/// that uid and resourceVersion, and the desired one that hash, applying
/// again would change nothing. A pass runs on every route, Service or
/// report change; re-applying four objects each time would load the
/// apiserver with no-op patches.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Applied {
    uid: String,
    resource_version: String,
    hash: String,
}

/// By Gateway uid, then kind.
type AppliedObjects = Arc<Mutex<HashMap<String, HashMap<Kind, Applied>>>>;

/// What provisioning reads: the owned-object caches, and how its data
/// planes reach the controller.
pub(crate) struct Owned {
    service_accounts: Store<PartialObjectMeta<ServiceAccount>>,
    deployments: Store<PartialObjectMeta<Deployment>>,
    pdbs: Store<PartialObjectMeta<PodDisruptionBudget>>,
    controller: ControllerEndpoint,
    applied: AppliedObjects,
}

/// A change to an owned object, as the reflector left its store.
struct Touched {
    kind: Kind,
    meta: ObjectMeta,
    /// Gone from the store: deleted.
    gone: bool,
}

impl Owned {
    /// Start the owned-object reflectors, each tracked in `listings`. The
    /// stream names the Gateway of `gateways` each change to an owned
    /// object concerns. Services come from the controller's own reflector.
    pub(crate) fn start(
        client: &Client,
        gateways: Store<Gateway>,
        listings: &Listings,
        controller: ControllerEndpoint,
    ) -> (Self, BoxStream<'static, ObjectRef<Gateway>>) {
        let (service_accounts, sa_changes) =
            owned_reflector::<ServiceAccount>(client, listings, Kind::ServiceAccount);
        let (deployments, deployment_changes) =
            owned_reflector::<Deployment>(client, listings, Kind::Deployment);
        let (pdbs, pdb_changes) =
            owned_reflector::<PodDisruptionBudget>(client, listings, Kind::PodDisruptionBudget);
        let applied = AppliedObjects::default();
        let triggers = futures::stream::select_all([sa_changes, deployment_changes, pdb_changes])
            .filter_map({
                let applied = applied.clone();
                move |touched| {
                    if touched.gone {
                        forget_deleted(&applied, &touched);
                    }
                    let gateway = controller_gateway(&touched.meta)
                        .filter(|gateway| gateways.get(gateway).is_some());
                    futures::future::ready(gateway)
                }
            })
            .boxed();
        let owned = Owned {
            service_accounts,
            deployments,
            pdbs,
            controller,
            applied,
        };
        (owned, triggers)
    }

    /// Over stores a test fills.
    #[cfg(test)]
    pub(crate) fn new(
        service_accounts: Store<PartialObjectMeta<ServiceAccount>>,
        deployments: Store<PartialObjectMeta<Deployment>>,
        pdbs: Store<PartialObjectMeta<PodDisruptionBudget>>,
        controller: ControllerEndpoint,
    ) -> Self {
        Owned {
            service_accounts,
            deployments,
            pdbs,
            controller,
            applied: AppliedObjects::default(),
        }
    }

    /// The cached metadata of `kind` `namespace/name`: the label-selected
    /// caches, the Service reflector for Services.
    fn cached(
        &self,
        ctx: &ControllerCtx,
        kind: Kind,
        namespace: &str,
        name: &str,
    ) -> Option<ObjectMeta> {
        match kind {
            Kind::ServiceAccount => meta_of(&self.service_accounts, namespace, name),
            Kind::Deployment => meta_of(&self.deployments, namespace, name),
            Kind::PodDisruptionBudget => meta_of(&self.pdbs, namespace, name),
            Kind::Service => ctx
                .cache
                .services
                .get(&ObjectRef::new(name).within(namespace))
                .map(|s| s.metadata.clone()),
        }
    }

    /// Every object of `namespace` that is ours to delete for the Gateway
    /// of `gateway_uid`, in apply order.
    fn deletable(
        &self,
        ctx: &ControllerCtx,
        namespace: &str,
        gateway_uid: &str,
    ) -> Vec<(Kind, ObjectMeta)> {
        let (service_accounts, deployments, services, pdbs) = (
            self.service_accounts.state(),
            self.deployments.state(),
            ctx.cache.services.state(),
            self.pdbs.state(),
        );
        service_accounts
            .iter()
            .map(|o| (Kind::ServiceAccount, &o.metadata))
            .chain(deployments.iter().map(|o| (Kind::Deployment, &o.metadata)))
            .chain(services.iter().map(|s| (Kind::Service, &s.metadata)))
            .chain(
                pdbs.iter()
                    .map(|o| (Kind::PodDisruptionBudget, &o.metadata)),
            )
            .filter(|(_, m)| {
                m.namespace.as_deref() == Some(namespace)
                    && is_ours(m, gateway_uid)
                    && m.labels
                        .as_ref()
                        .and_then(|l| l.get(LABEL_MANAGED_BY))
                        .is_some_and(|v| v == MANAGED_BY)
            })
            .map(|(kind, m)| (kind, m.clone()))
            .collect()
    }

    fn applied(&self, gateway_uid: &str, kind: Kind) -> Option<Applied> {
        let applied = self.applied.lock().unwrap_or_else(|p| p.into_inner());
        applied.get(gateway_uid)?.get(&kind).cloned()
    }

    fn record(&self, gateway_uid: &str, kind: Kind, record: Option<Applied>) {
        let mut applied = self.applied.lock().unwrap_or_else(|p| p.into_inner());
        let kinds = applied.entry(gateway_uid.to_string()).or_default();
        match record {
            Some(record) => {
                kinds.insert(kind, record);
            }
            None => {
                kinds.remove(&kind);
            }
        }
        if kinds.is_empty() {
            applied.remove(gateway_uid);
        }
    }
}

/// A label-selected metadata reflector of `K` across namespaces.
fn owned_reflector<K>(
    client: &Client,
    listings: &Listings,
    kind: Kind,
) -> (Store<PartialObjectMeta<K>>, BoxStream<'static, Touched>)
where
    K: Resource<DynamicType = (), Scope = NamespaceResourceScope>
        + Clone
        + std::fmt::Debug
        + serde::de::DeserializeOwned
        + Send
        + Sync
        + 'static,
{
    let tracker = listings.track(kind.name(), false);
    let writer = Writer::default();
    let store = writer.as_reader();
    let config = watcher::Config::default().labels(&format!("{LABEL_MANAGED_BY}={MANAGED_BY}"));
    let changes = reflector(
        writer,
        metadata_watcher(Api::<K>::all(client.clone()), config)
            .modify(drop_unread_metadata)
            .inspect(move |event| tracker.observe(event)),
    )
    .default_backoff()
    .touched_objects()
    .filter_map({
        let store = store.clone();
        move |touched| {
            let touched = match touched {
                Ok(obj) => Some(Touched {
                    kind,
                    gone: store.get(&ObjectRef::from_obj(&obj)).is_none(),
                    meta: obj.metadata,
                }),
                Err(e) => {
                    warn!("Owned {} watch: {}", kind.name(), e);
                    None
                }
            };
            futures::future::ready(touched)
        }
    })
    .boxed();
    (store, changes)
}

/// A deleted Deployment is the end of a Managed data plane (its Gateway
/// deleted, released, or the Deployment removed by hand): every record of
/// its Gateway goes, including the Service's, whose deletions this module
/// does not watch.
fn forget_deleted(applied: &AppliedObjects, touched: &Touched) {
    let Some(owner) = controller_owner(&touched.meta) else {
        return;
    };
    let mut applied = applied.lock().unwrap_or_else(|p| p.into_inner());
    if touched.kind == Kind::Deployment {
        applied.remove(&owner.uid);
    } else if let Some(kinds) = applied.get_mut(&owner.uid) {
        kinds.remove(&touched.kind);
        if kinds.is_empty() {
            applied.remove(&owner.uid);
        }
    }
}

fn meta_of<K>(
    store: &Store<PartialObjectMeta<K>>,
    namespace: &str,
    name: &str,
) -> Option<ObjectMeta>
where
    K: Resource<DynamicType = ()> + Clone,
{
    store
        .get(&ObjectRef::new(name).within(namespace))
        .map(|o| o.metadata.clone())
}

/// The controller ownerReference, if it names a Gateway.
fn controller_owner(meta: &ObjectMeta) -> Option<&OwnerReference> {
    meta.owner_references.iter().flatten().find(|o| {
        o.controller == Some(true)
            && o.kind == "Gateway"
            && o.api_version.starts_with("gateway.networking.k8s.io/")
    })
}

/// The Gateway an owned object's controller ownerReference names.
fn controller_gateway(meta: &ObjectMeta) -> Option<ObjectRef<Gateway>> {
    let owner = controller_owner(meta)?;
    Some(ObjectRef::new(&owner.name).within(meta.namespace.as_deref()?))
}

fn is_ours(meta: &ObjectMeta, gateway_uid: &str) -> bool {
    controller_owner(meta).is_some_and(|o| !gateway_uid.is_empty() && o.uid == gateway_uid)
}

/// An object under a wanted name, as this Gateway sees it.
enum Live {
    Absent,
    Ours {
        uid: String,
        resource_version: String,
    },
    /// Its controller is a deleted Gateway of the same name: foreign until
    /// garbage collection takes it.
    Stale,
    Foreign,
}

fn judge(meta: Option<ObjectMeta>, gateway: &Gateway) -> Live {
    let Some(meta) = meta else {
        return Live::Absent;
    };
    let uid = gateway.metadata.uid.as_deref().unwrap_or_default();
    if is_ours(&meta, uid) {
        return Live::Ours {
            uid: meta.uid.unwrap_or_default(),
            resource_version: meta.resource_version.unwrap_or_default(),
        };
    }
    match controller_owner(&meta) {
        Some(o) if o.name == gateway.name_any() => Live::Stale,
        _ => Live::Foreign,
    }
}

/// Why a call failed, and what that means for the pass.
enum Failure {
    /// A retry may cure it: the pass is skipped and retried.
    Retry(anyhow::Error),
    /// The apiserver refused the call itself (RBAC, validation): status
    /// says why until something changes.
    Refused(String),
}

impl Failure {
    fn of(e: kube::Error, what: String) -> Self {
        match &e {
            // 408 and 429 pass with time; a 409 is a writer that raced
            // this pass.
            kube::Error::Api(r)
                if (400..500).contains(&r.code) && !matches!(r.code, 408 | 409 | 429) =>
            {
                Failure::Refused(format!("{what}: {}", r.message))
            }
            _ => Failure::Retry(anyhow::Error::new(e).context(what)),
        }
    }

    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Failure::Retry(e) => e,
            Failure::Refused(message) => anyhow::anyhow!(message),
        }
    }
}

/// One object to apply.
struct Wanted {
    kind: Kind,
    body: serde_json::Value,
}

impl Wanted {
    fn of(kind: Kind, object: &impl Serialize) -> Self {
        Self {
            kind,
            body: serde_json::to_value(object).unwrap_or_default(),
        }
    }
}

/// Bring `gateway`'s objects in line with `params` before its snapshot is
/// published. `Err` is a failure worth retrying (a transport error, 5xx,
/// a conflict): nothing is published this pass. One a retry will not cure
/// is `Provisioned::problem`.
pub(crate) async fn reconcile(
    ctx: &ControllerCtx,
    gateway: &Gateway,
    params: &Params,
) -> anyhow::Result<Provisioned> {
    if params.provisioning == Provisioning::Unmanaged {
        return match delete_all_but(ctx, gateway, &[]).await {
            Ok(()) => Ok(Provisioned::unmanaged()),
            Err(Failure::Refused(problem)) => Ok(Provisioned {
                problem: Some(problem),
                ..Provisioned::unmanaged()
            }),
            Err(Failure::Retry(e)) => Err(e),
        };
    }
    let owned = &ctx.owned;
    let namespace = gateway.namespace().unwrap_or_else(|| "default".into());
    let uid = gateway.metadata.uid.clone().unwrap_or_default();
    let name = resource_name(&gateway.name_any());

    let existing = owned_service(ctx, gateway)
        .map(|s| ports::existing_targets(&s))
        .unwrap_or_default();
    let (objects, bound_ports) = render::desired(gateway, params, &existing, &owned.controller);
    // A foreground deletion waits on the children: applying them again
    // would race the garbage collector until the Gateway is gone.
    if gateway.metadata.deletion_timestamp.is_some() {
        return Ok(Provisioned {
            bound_ports,
            problem: None,
        });
    }
    let mut wanted = vec![
        Wanted::of(Kind::ServiceAccount, &objects.service_account),
        Wanted::of(Kind::Deployment, &objects.deployment),
    ];
    wanted.extend(objects.service.iter().map(|s| Wanted::of(Kind::Service, s)));
    wanted.extend(
        objects
            .pdb
            .iter()
            .map(|p| Wanted::of(Kind::PodDisruptionBudget, p)),
    );

    // All or nothing: every wanted name is ours or free before anything
    // is applied.
    let mut live = Vec::with_capacity(wanted.len());
    let mut conflicts = Vec::new();
    let mut stale = false;
    for w in &wanted {
        let meta = match owned.cached(ctx, w.kind, &namespace, &name) {
            Some(meta) => Some(meta),
            // Not labeled ours, or not listed yet; Services are all cached.
            None if w.kind == Kind::Service => None,
            None => match get_metadata(&ctx.client, w.kind, &namespace, &name).await {
                Ok(meta) => meta,
                Err(e) => {
                    return refused_or_retry(
                        Failure::of(e, format!("reading {}/{name}", w.kind.name())),
                        bound_ports,
                    )
                }
            },
        };
        match judge(meta, gateway) {
            Live::Stale => {
                stale = true;
                conflicts.push(format!("{}/{name}", w.kind.name()));
            }
            Live::Foreign => conflicts.push(format!("{}/{name}", w.kind.name())),
            state => live.push(state),
        }
    }
    if !conflicts.is_empty() {
        let mut problem = match conflicts.as_slice() {
            [one] => format!("name conflict: {one} is not owned by this Gateway"),
            many => format!(
                "name conflict: {} are not owned by this Gateway",
                many.join(", ")
            ),
        };
        if stale {
            problem.push_str(
                " (a deleted Gateway of the same name owns some; they go with its garbage \
                 collection)",
            );
        }
        return Ok(Provisioned {
            bound_ports,
            problem: Some(problem),
        });
    }

    for (w, state) in wanted.iter().zip(live) {
        let hash = sha256_hex(w.body.to_string().as_bytes());
        let force = match state {
            Live::Ours {
                uid: object_uid,
                resource_version,
            } => {
                let current = Applied {
                    uid: object_uid,
                    resource_version,
                    hash: hash.clone(),
                };
                if owned.applied(&uid, w.kind).as_ref() == Some(&current) {
                    continue;
                }
                true
            }
            // A foreign object created since the preflight conflicts
            // instead of being taken over.
            _ => false,
        };
        match apply(&ctx.client, w, &namespace, &name, force).await {
            Ok(meta) => {
                info!(
                    "Gateway {}/{}: applied {} {}",
                    namespace,
                    gateway.name_any(),
                    w.kind.name(),
                    name
                );
                owned.record(
                    &uid,
                    w.kind,
                    Some(Applied {
                        uid: meta.uid.unwrap_or_default(),
                        resource_version: meta.resource_version.unwrap_or_default(),
                        hash,
                    }),
                );
            }
            Err(e) => {
                owned.record(&uid, w.kind, None);
                return refused_or_retry(
                    Failure::of(e, format!("applying {}/{name}", w.kind.name())),
                    bound_ports,
                );
            }
        }
    }

    let keep: Vec<(Kind, &str)> = wanted.iter().map(|w| (w.kind, name.as_str())).collect();
    match delete_all_but(ctx, gateway, &keep).await {
        Ok(()) => Ok(Provisioned {
            bound_ports,
            problem: None,
        }),
        Err(failure) => refused_or_retry(failure, bound_ports),
    }
}

/// The Service fronting `gateway`'s data plane, if it is ours: a foreign
/// Service under its name says nothing about it, its addresses included.
pub(crate) fn owned_service(ctx: &ControllerCtx, gateway: &Gateway) -> Option<Arc<Service>> {
    let uid = gateway.metadata.uid.as_deref()?;
    let namespace = gateway.namespace().unwrap_or_else(|| "default".into());
    ctx.cache
        .services
        .get(&ObjectRef::new(&resource_name(&gateway.name_any())).within(&namespace))
        .filter(|s| is_ours(&s.metadata, uid))
}

fn refused_or_retry(
    failure: Failure,
    bound_ports: BTreeMap<(Transport, u16), u16>,
) -> anyhow::Result<Provisioned> {
    match failure {
        Failure::Refused(problem) => Ok(Provisioned {
            bound_ports,
            problem: Some(problem),
        }),
        Failure::Retry(e) => Err(e),
    }
}

/// The Gateway left this controller (its class names another): delete its
/// owned objects. `Err` leaves some for a retry.
pub(crate) async fn release(ctx: &ControllerCtx, gateway: &Gateway) -> anyhow::Result<()> {
    delete_all_but(ctx, gateway, &[])
        .await
        .map_err(Failure::into_anyhow)
}

/// Delete the Gateway's deletable objects but those in `keep`. Nothing
/// deletable, no API call.
async fn delete_all_but(
    ctx: &ControllerCtx,
    gateway: &Gateway,
    keep: &[(Kind, &str)],
) -> Result<(), Failure> {
    let Some(uid) = gateway.metadata.uid.as_deref() else {
        return Ok(());
    };
    let namespace = gateway.namespace().unwrap_or_else(|| "default".into());
    for (kind, meta) in ctx.owned.deletable(ctx, &namespace, uid) {
        let name = meta.name.clone().unwrap_or_default();
        if keep.contains(&(kind, name.as_str())) {
            continue;
        }
        // The uid precondition: never the object another took the name
        // with since the cache saw ours.
        let params = DeleteParams {
            propagation_policy: Some(PropagationPolicy::Background),
            preconditions: Some(Preconditions {
                uid: meta.uid.clone(),
                resource_version: None,
            }),
            ..Default::default()
        };
        let api: Api<DynamicObject> =
            Api::namespaced_with(ctx.client.clone(), &namespace, &kind.resource());
        match api.delete(&name, &params).await {
            Ok(_) => info!(
                "Gateway {}/{}: deleted {} {}",
                namespace,
                gateway.name_any(),
                kind.name(),
                name
            ),
            // Gone already, or no longer the object the cache saw.
            Err(kube::Error::Api(r)) if matches!(r.code, 404 | 409) => {}
            Err(e) => return Err(Failure::of(e, format!("deleting {}/{name}", kind.name()))),
        }
        ctx.owned.record(uid, kind, None);
    }
    Ok(())
}

async fn get_metadata(
    client: &Client,
    kind: Kind,
    namespace: &str,
    name: &str,
) -> Result<Option<ObjectMeta>, kube::Error> {
    let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), namespace, &kind.resource());
    match api.get_metadata(name).await {
        Ok(meta) => Ok(Some(meta.metadata)),
        Err(kube::Error::Api(r)) if r.code == 404 => Ok(None),
        Err(e) => Err(e),
    }
}

async fn apply(
    client: &Client,
    wanted: &Wanted,
    namespace: &str,
    name: &str,
    force: bool,
) -> Result<ObjectMeta, kube::Error> {
    let api: Api<DynamicObject> =
        Api::namespaced_with(client.clone(), namespace, &wanted.kind.resource());
    let mut params = PatchParams::apply(FIELD_MANAGER);
    params.force = force;
    let applied = api
        .patch(name, &params, &Patch::Apply(&wanted.body))
        .await?;
    Ok(applied.metadata)
}

#[cfg(test)]
mod tests {
    use super::super::reconcile::{finish, publish, Step};
    use super::super::testkit::{self, apply, MockApi, TestCluster, CONTROLLER};
    use super::*;
    use crate::kubernetes::crds::ServiceType;
    use serde_json::json;

    const NS: &str = "default";
    const NAME: &str = "portail-gw";

    fn path(kind: Kind, name: &str) -> String {
        let plural = match kind {
            Kind::ServiceAccount => "/api/v1/namespaces/default/serviceaccounts",
            Kind::Service => "/api/v1/namespaces/default/services",
            Kind::Deployment => "/apis/apps/v1/namespaces/default/deployments",
            Kind::PodDisruptionBudget => "/apis/policy/v1/namespaces/default/poddisruptionbudgets",
        };
        format!("{plural}/{name}")
    }

    /// Writes to `kind`/`name` answer this uid and resourceVersion; a read
    /// finds `found`, or nothing. Writes carry a query, reads do not, and
    /// the first registered prefix wins.
    fn answer(api: &MockApi, kind: Kind, uid: &str, rv: &str, found: Option<serde_json::Value>) {
        let ar = kind.resource();
        let written = json!({
            "apiVersion": ar.api_version,
            "kind": ar.kind,
            "metadata": {"name": NAME, "namespace": NS, "uid": uid, "resourceVersion": rv},
        });
        replies(api, kind, NAME, (200, written), found);
    }

    /// Writes to `kind`/`name` answer `code`; reads find nothing.
    fn fail(api: &MockApi, kind: Kind, name: &str, code: u16) {
        let status = json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                            "reason": "Refused", "code": code,
                            "message": format!("refused with {code}")});
        replies(api, kind, name, (code, status), None);
    }

    fn replies(
        api: &MockApi,
        kind: Kind,
        name: &str,
        (code, written): (u16, serde_json::Value),
        found: Option<serde_json::Value>,
    ) {
        api.reply_to(&format!("{}?", path(kind, name)), code, written);
        match found {
            Some(metadata) => api.reply_to(
                &path(kind, name),
                200,
                json!({"apiVersion": "meta.k8s.io/v1", "kind": "PartialObjectMetadata",
                       "metadata": metadata}),
            ),
            None => api.reply_to(
                &path(kind, name),
                404,
                json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                       "reason": "NotFound", "code": 404, "message": "not found"}),
            ),
        }
    }

    fn gateway() -> Gateway {
        let mut gw = testkit::gateway("gw", 1);
        gw.spec.listeners[0].port = 80;
        gw
    }

    fn params(replicas: i32, service_type: ServiceType) -> Params {
        Params {
            provisioning: Provisioning::Managed,
            replicas,
            image: "ghcr.io/epheo/portail:test".into(),
            resources: Default::default(),
            worker_threads: None,
            service_type,
            networks: vec![],
        }
    }

    /// Metadata of an object whose controller is Gateway `owner` of `uid`.
    fn meta(name: &str, owner: &str, uid: &str, object_uid: &str, labeled: bool) -> ObjectMeta {
        ObjectMeta {
            name: Some(name.into()),
            namespace: Some(NS.into()),
            uid: Some(object_uid.into()),
            resource_version: Some("1".into()),
            labels: labeled
                .then(|| [(LABEL_MANAGED_BY.to_string(), MANAGED_BY.to_string())].into()),
            owner_references: Some(vec![OwnerReference {
                api_version: "gateway.networking.k8s.io/v1".into(),
                kind: "Gateway".into(),
                name: owner.into(),
                uid: uid.into(),
                controller: Some(true),
                block_owner_deletion: None,
            }]),
            ..Default::default()
        }
    }

    fn partial<K>(metadata: ObjectMeta) -> PartialObjectMeta<K> {
        PartialObjectMeta {
            types: None,
            metadata,
            _phantom: std::marker::PhantomData,
        }
    }

    /// The Gateway's owned objects in the caches, each at resourceVersion 1.
    fn own_all(cluster: &mut TestCluster) {
        let ours = |kind: &str| meta(NAME, "gw", "gw-uid", &format!("{kind}-uid"), true);
        apply(&mut cluster.service_accounts, partial(ours("sa")));
        apply(&mut cluster.deployments, partial(ours("deployment")));
        apply(&mut cluster.pdbs, partial(ours("pdb")));
        apply(
            &mut cluster.services,
            Service {
                metadata: ours("service"),
                spec: Some(
                    serde_json::from_value(json!({
                        "ports": [{"name": "tcp-80", "port": 80, "targetPort": 8123}],
                    }))
                    .unwrap(),
                ),
                status: None,
            },
        );
    }

    /// (method, path without query) of every write.
    fn writes(api: &MockApi) -> Vec<(String, String)> {
        api.requests()
            .into_iter()
            .filter(|r| r.method != "GET")
            .map(|r| (r.method, r.path.split('?').next().unwrap().to_string()))
            .collect()
    }

    fn patched(kinds: &[Kind]) -> Vec<(String, String)> {
        kinds
            .iter()
            .map(|k| ("PATCH".to_string(), path(*k, NAME)))
            .collect()
    }

    /// A first pass creates every object, none forced: none was ours. The
    /// Service is not read: its reflector holds every Service.
    #[tokio::test]
    async fn a_new_gateway_gets_its_objects() {
        let api = MockApi::default();
        let cluster = TestCluster::default();
        let ctx = cluster.ctx(api.client());
        for kind in [Kind::ServiceAccount, Kind::Deployment, Kind::Service] {
            answer(&api, kind, "new", "1", None);
        }
        let provisioned = reconcile(&ctx, &gateway(), &params(1, ServiceType::LoadBalancer))
            .await
            .unwrap();
        assert_eq!(provisioned.problem, None);
        assert_eq!(
            provisioned.bound_ports,
            [((Transport::Tcp, 80), 8000)].into_iter().collect()
        );
        assert_eq!(
            writes(&api),
            patched(&[Kind::ServiceAccount, Kind::Deployment, Kind::Service])
        );
        let reads: Vec<String> = api
            .requests()
            .into_iter()
            .filter(|r| r.method == "GET")
            .map(|r| r.path)
            .collect();
        assert_eq!(
            reads,
            [
                path(Kind::ServiceAccount, NAME),
                path(Kind::Deployment, NAME)
            ]
        );
        for r in api.requests().iter().filter(|r| r.method == "PATCH") {
            assert!(
                r.path.contains("fieldManager=portail-controller"),
                "{}",
                r.path
            );
            assert!(!r.path.contains("force=true"), "{}", r.path);
        }
    }

    /// An object as it was applied is not applied again; one changed since
    /// (its resourceVersion moved) is, forced: it is ours.
    #[tokio::test]
    async fn only_a_changed_object_is_applied_again() {
        let api = MockApi::default();
        let mut cluster = TestCluster::default();
        own_all(&mut cluster);
        let ctx = cluster.ctx(api.client());
        for (kind, uid) in [
            (Kind::ServiceAccount, "sa-uid"),
            (Kind::Deployment, "deployment-uid"),
            (Kind::Service, "service-uid"),
        ] {
            answer(&api, kind, uid, "1", None);
        }
        answer(&api, Kind::PodDisruptionBudget, "pdb-uid", "1", None);
        let gw = gateway();
        let p = params(2, ServiceType::LoadBalancer);
        reconcile(&ctx, &gw, &p).await.unwrap();
        assert_eq!(
            writes(&api),
            patched(&[
                Kind::ServiceAccount,
                Kind::Deployment,
                Kind::Service,
                Kind::PodDisruptionBudget
            ])
        );
        assert!(api
            .requests()
            .iter()
            .all(|r| r.method != "PATCH" || r.path.contains("force=true")));

        api.clear();
        let provisioned = reconcile(&ctx, &gw, &p).await.unwrap();
        assert!(api.requests().is_empty(), "{:?}", api.requests());
        assert_eq!(
            provisioned.bound_ports[&(Transport::Tcp, 80)],
            8123,
            "the live Service's target stays"
        );

        let mut moved = meta(NAME, "gw", "gw-uid", "deployment-uid", true);
        moved.resource_version = Some("2".into());
        apply(&mut cluster.deployments, partial(moved));
        answer(&api, Kind::Deployment, "deployment-uid", "2", None);
        reconcile(&ctx, &gw, &p).await.unwrap();
        assert_eq!(writes(&api), patched(&[Kind::Deployment]));
    }

    /// One foreign name stops the whole apply, and every delete, and says
    /// so.
    #[tokio::test]
    async fn a_foreign_object_stops_everything() {
        let api = MockApi::default();
        let mut cluster = TestCluster::default();
        // An obsolete PDB of ours: replicas 1 would delete it.
        apply(
            &mut cluster.pdbs,
            partial(meta(NAME, "gw", "gw-uid", "pdb-uid", true)),
        );
        let ctx = cluster.ctx(api.client());
        answer(&api, Kind::ServiceAccount, "new", "1", None);
        answer(
            &api,
            Kind::Deployment,
            "x",
            "1",
            Some(json!({"name": NAME, "namespace": NS, "uid": "hand-written"})),
        );
        let provisioned = reconcile(&ctx, &gateway(), &params(1, ServiceType::None))
            .await
            .unwrap();
        assert_eq!(
            provisioned.problem.as_deref(),
            Some("name conflict: Deployment/portail-gw is not owned by this Gateway")
        );
        assert!(writes(&api).is_empty(), "{:?}", writes(&api));
    }

    /// An object a deleted Gateway of the same name still owns is foreign
    /// until garbage collection takes it.
    #[tokio::test]
    async fn an_object_of_a_deleted_namesake_is_foreign() {
        let api = MockApi::default();
        let mut cluster = TestCluster::default();
        apply(
            &mut cluster.deployments,
            partial(meta(NAME, "gw", "old-gw-uid", "deployment-uid", true)),
        );
        apply(
            &mut cluster.service_accounts,
            partial(meta(NAME, "gw", "old-gw-uid", "sa-uid", true)),
        );
        let ctx = cluster.ctx(api.client());
        let provisioned = reconcile(&ctx, &gateway(), &params(1, ServiceType::None))
            .await
            .unwrap();
        let problem = provisioned.problem.unwrap();
        assert!(
            problem.starts_with(
                "name conflict: ServiceAccount/portail-gw, Deployment/portail-gw are not owned"
            ),
            "{problem}"
        );
        assert!(
            problem.contains("deleted Gateway of the same name"),
            "{problem}"
        );
        assert!(api.requests().is_empty());
    }

    /// `gw.example` and `gw-example-f8a6900a` share a resource name: the
    /// objects of one are foreign to the other.
    #[tokio::test]
    async fn a_hashed_name_and_a_literal_one_do_not_share_objects() {
        let api = MockApi::default();
        let mut cluster = TestCluster::default();
        let name = resource_name("gw.example");
        assert_eq!(name, resource_name("gw-example-f8a6900a"));
        let mut owned = meta(&name, "gw.example", "dotted-uid", "sa-uid", true);
        owned.name = Some(name.clone());
        apply(&mut cluster.service_accounts, partial(owned));
        let ctx = cluster.ctx(api.client());
        fail(&api, Kind::Deployment, &name, 500);
        let mut literal = testkit::gateway("gw-example-f8a6900a", 1);
        literal.spec.listeners[0].port = 80;
        let provisioned = reconcile(&ctx, &literal, &params(1, ServiceType::None))
            .await
            .unwrap();
        assert_eq!(
            provisioned.problem.unwrap(),
            format!("name conflict: ServiceAccount/{name} is not owned by this Gateway")
        );
        assert!(writes(&api).is_empty());
    }

    /// The uid each delete carries, by path.
    fn deletes(api: &MockApi) -> Vec<(String, String)> {
        api.requests()
            .into_iter()
            .filter(|r| r.method == "DELETE")
            .map(|r| {
                (
                    r.path.split('?').next().unwrap().to_string(),
                    r.body["preconditions"]["uid"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    /// Type None deletes the Service; one replica deletes the PDB.
    #[tokio::test]
    async fn obsolete_objects_are_deleted() {
        let api = MockApi::default();
        let mut cluster = TestCluster::default();
        own_all(&mut cluster);
        let ctx = cluster.ctx(api.client());
        answer(&api, Kind::ServiceAccount, "sa-uid", "1", None);
        answer(&api, Kind::Deployment, "deployment-uid", "1", None);
        answer(&api, Kind::Service, "service-uid", "1", None);
        answer(&api, Kind::PodDisruptionBudget, "pdb-uid", "1", None);
        let provisioned = reconcile(&ctx, &gateway(), &params(1, ServiceType::None))
            .await
            .unwrap();
        assert!(
            provisioned.bound_ports.is_empty(),
            "no Service: published ports"
        );
        assert_eq!(
            deletes(&api),
            [
                (path(Kind::Service, NAME), "service-uid".to_string()),
                (path(Kind::PodDisruptionBudget, NAME), "pdb-uid".to_string()),
            ]
        );
    }

    /// Replicas 2 to 1 deletes the PDB and keeps the Service.
    #[tokio::test]
    async fn one_replica_deletes_the_pdb() {
        let api = MockApi::default();
        let mut cluster = TestCluster::default();
        own_all(&mut cluster);
        let ctx = cluster.ctx(api.client());
        for (kind, uid) in [
            (Kind::ServiceAccount, "sa-uid"),
            (Kind::Deployment, "deployment-uid"),
            (Kind::Service, "service-uid"),
            (Kind::PodDisruptionBudget, "pdb-uid"),
        ] {
            answer(&api, kind, uid, "1", None);
        }
        reconcile(&ctx, &gateway(), &params(1, ServiceType::LoadBalancer))
            .await
            .unwrap();
        assert_eq!(
            deletes(&api),
            [(path(Kind::PodDisruptionBudget, NAME), "pdb-uid".to_string())]
        );
    }

    /// Unmanaged deletes all four; only ours with our label, in the
    /// Gateway's namespace, are touched.
    #[tokio::test]
    async fn unmanaged_deletes_what_is_ours_and_only_that() {
        let api = MockApi::default();
        let mut cluster = TestCluster::default();
        own_all(&mut cluster);
        // Ours by owner but not labeled: someone else's to manage.
        apply(
            &mut cluster.service_accounts,
            partial(meta("unlabeled", "gw", "gw-uid", "unlabeled-uid", false)),
        );
        // Labeled, but another Gateway's.
        apply(
            &mut cluster.deployments,
            partial(meta(
                "portail-other",
                "other",
                "other-uid",
                "other-uid",
                true,
            )),
        );
        // Ours in another namespace cannot be: same uid, other namespace.
        let mut elsewhere = meta(NAME, "gw", "gw-uid", "elsewhere-uid", true);
        elsewhere.namespace = Some("elsewhere".into());
        apply(&mut cluster.pdbs, partial(elsewhere));
        let ctx = cluster.ctx(api.client());
        for kind in [
            Kind::ServiceAccount,
            Kind::Deployment,
            Kind::Service,
            Kind::PodDisruptionBudget,
        ] {
            answer(&api, kind, "x", "1", None);
        }
        let mut unmanaged = params(1, ServiceType::LoadBalancer);
        unmanaged.provisioning = Provisioning::Unmanaged;
        let provisioned = reconcile(&ctx, &gateway(), &unmanaged).await.unwrap();
        assert!(provisioned.bound_ports.is_empty() && provisioned.problem.is_none());
        assert_eq!(
            deletes(&api),
            [
                (path(Kind::ServiceAccount, NAME), "sa-uid".to_string()),
                (path(Kind::Deployment, NAME), "deployment-uid".to_string()),
                (path(Kind::Service, NAME), "service-uid".to_string()),
                (path(Kind::PodDisruptionBudget, NAME), "pdb-uid".to_string()),
            ]
        );
        assert_eq!(writes(&api).len(), 4, "deletes only");
    }

    /// Release (the class names another controller) deletes what is ours;
    /// with nothing ours it makes no API call.
    #[tokio::test]
    async fn release_is_cheap_and_deletes_what_is_ours() {
        let api = MockApi::default();
        let cluster = TestCluster::default();
        let ctx = cluster.ctx(api.client());
        release(&ctx, &gateway()).await.unwrap();
        release(&ctx, &gateway()).await.unwrap();
        assert!(api.requests().is_empty());

        let mut cluster = TestCluster::default();
        own_all(&mut cluster);
        let ctx = cluster.ctx(api.client());
        for kind in [
            Kind::ServiceAccount,
            Kind::Deployment,
            Kind::Service,
            Kind::PodDisruptionBudget,
        ] {
            answer(&api, kind, "x", "1", None);
        }
        release(&ctx, &gateway()).await.unwrap();
        assert_eq!(deletes(&api).len(), 4);
    }

    /// A Gateway being deleted gets nothing applied or deleted.
    #[tokio::test]
    async fn a_deleting_gateway_is_left_to_garbage_collection() {
        let api = MockApi::default();
        let mut cluster = TestCluster::default();
        own_all(&mut cluster);
        let ctx = cluster.ctx(api.client());
        let mut gw = gateway();
        gw.metadata.deletion_timestamp = Some(
            k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(Default::default()),
        );
        let provisioned = reconcile(&ctx, &gw, &params(1, ServiceType::None))
            .await
            .unwrap();
        assert_eq!(provisioned.problem, None);
        assert!(api.requests().is_empty(), "{:?}", api.requests());
    }

    /// Unusable parameters apply and delete nothing, and the snapshot
    /// served stays.
    #[tokio::test]
    async fn invalid_parameters_touch_nothing() {
        let api = MockApi::default();
        let mut cluster = TestCluster::default();
        apply(
            &mut cluster.gateway_classes,
            testkit::gateway_class("portail", CONTROLLER),
        );
        let gw = gateway();
        apply(&mut cluster.gateways, gw.clone());
        let ctx = cluster.ctx(api.client());
        for kind in [Kind::ServiceAccount, Kind::Deployment, Kind::Service] {
            answer(&api, kind, "new", "1", None);
        }
        let Step::Continue(_) = publish(&gw, &ctx).await else {
            panic!("published");
        };
        let served = ctx.hub.current(&testkit::key(&gw)).unwrap().version.clone();

        own_all(&mut cluster);
        let mut broken = gw.clone();
        broken.metadata.generation = Some(2);
        broken.spec.listeners[0].port = 81;
        broken.spec.infrastructure = Some(gateway_api::gateways::GatewayInfrastructure {
            parameters_ref: Some(gateway_api::gateways::GatewayInfrastructureParametersRef {
                group: "portail.epheo.eu".into(),
                kind: "GatewayParameters".into(),
                name: "absent".into(),
            }),
            ..Default::default()
        });
        apply(&mut cluster.gateways, broken.clone());
        api.clear();
        let Step::Continue(pass) = publish(&broken, &ctx).await else {
            panic!("a pass");
        };
        finish(&broken, &ctx, *pass).await.unwrap();
        assert_eq!(
            ctx.hub.current(&testkit::key(&gw)).unwrap().version,
            served,
            "the snapshot served stays"
        );
        assert!(writes(&api)
            .iter()
            .all(|(method, path)| method == "PATCH" && path.ends_with("/gateways/gw/status")));
        let patch = api.status_patch("gateways", "gw").unwrap();
        let accepted = patch["status"]["conditions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["type"] == "Accepted")
            .unwrap()
            .clone();
        assert_eq!(accepted["reason"], "InvalidParameters", "{accepted}");
    }

    /// A class naming another controller releases the Gateway's objects in
    /// the pass; nothing is applied.
    #[tokio::test]
    async fn a_class_of_another_controller_releases_the_objects() {
        let api = MockApi::default();
        let mut cluster = TestCluster::default();
        apply(
            &mut cluster.gateway_classes,
            testkit::gateway_class("portail", "example.com/other"),
        );
        own_all(&mut cluster);
        let gw = gateway();
        apply(&mut cluster.gateways, gw.clone());
        let ctx = cluster.ctx(api.client());
        for kind in [
            Kind::ServiceAccount,
            Kind::Deployment,
            Kind::Service,
            Kind::PodDisruptionBudget,
        ] {
            answer(&api, kind, "x", "1", None);
        }
        let Step::Done(_) = publish(&gw, &ctx).await else {
            panic!("not ours: no pass");
        };
        assert_eq!(deletes(&api).len(), 4);
        assert_eq!(writes(&api).len(), 4, "deletes only");
    }

    /// A delete that finds the object gone or replaced is done; one the
    /// apiserver fails is retried.
    #[tokio::test]
    async fn release_failures() {
        let api = MockApi::default();
        let mut cluster = TestCluster::default();
        apply(
            &mut cluster.deployments,
            partial(meta(NAME, "gw", "gw-uid", "deployment-uid", true)),
        );
        let ctx = cluster.ctx(api.client());
        fail(&api, Kind::Deployment, NAME, 409);
        release(&ctx, &gateway()).await.unwrap();
        fail(&api, Kind::Deployment, NAME, 404);
        release(&ctx, &gateway()).await.unwrap();
        fail(&api, Kind::Deployment, NAME, 503);
        assert!(release(&ctx, &gateway()).await.is_err());
    }

    /// A refusal a retry will not cure is the problem status shows; a 5xx
    /// or a conflict is retried with nothing published.
    #[tokio::test]
    async fn refused_or_retried() {
        let api = MockApi::default();
        let cluster = TestCluster::default();
        let ctx = cluster.ctx(api.client());
        answer(&api, Kind::ServiceAccount, "sa-uid", "1", None);
        answer(&api, Kind::Deployment, "d", "1", None);
        fail(&api, Kind::Deployment, NAME, 403);
        let p = params(1, ServiceType::None);
        let provisioned = reconcile(&ctx, &gateway(), &p).await.unwrap();
        assert_eq!(
            provisioned.problem.as_deref(),
            Some("applying Deployment/portail-gw: refused with 403")
        );
        for code in [409, 500, 503] {
            fail(&api, Kind::Deployment, NAME, code);
            assert!(reconcile(&ctx, &gateway(), &p).await.is_err(), "{code}");
        }
    }

    /// A deleted Deployment drops every record of its Gateway; another
    /// kind only its own.
    #[test]
    fn deletions_forget_what_was_applied() {
        let applied = AppliedObjects::default();
        let record = Applied {
            uid: "u".into(),
            resource_version: "1".into(),
            hash: "h".into(),
        };
        applied.lock().unwrap().insert(
            "gw-uid".into(),
            [
                (Kind::Deployment, record.clone()),
                (Kind::Service, record.clone()),
                (Kind::PodDisruptionBudget, record),
            ]
            .into(),
        );
        let gone = |kind| Touched {
            kind,
            meta: meta(NAME, "gw", "gw-uid", "x", true),
            gone: true,
        };
        forget_deleted(&applied, &gone(Kind::PodDisruptionBudget));
        let kinds: Vec<Kind> = applied.lock().unwrap()["gw-uid"].keys().copied().collect();
        assert!(!kinds.contains(&Kind::PodDisruptionBudget) && kinds.len() == 2);
        forget_deleted(&applied, &gone(Kind::Deployment));
        assert!(applied.lock().unwrap().is_empty());
    }

    /// Only a Service of ours fronts the Gateway: a foreign one under its
    /// name gives neither targets nor addresses.
    #[tokio::test]
    async fn only_our_service_fronts_the_gateway() {
        let mut cluster = TestCluster::default();
        own_all(&mut cluster);
        let ctx = cluster.ctx(MockApi::default().client());
        assert!(owned_service(&ctx, &gateway()).is_some());
        let mut namesake = gateway();
        namesake.metadata.uid = Some("other-uid".into());
        assert!(owned_service(&ctx, &namesake).is_none());
    }

    /// An owned object triggers its Gateway; another object none.
    #[test]
    fn an_owned_object_names_its_gateway() {
        let owned = meta(NAME, "gw", "gw-uid", "x", true);
        assert_eq!(
            controller_gateway(&owned),
            Some(ObjectRef::new("gw").within(NS))
        );
        let mut not_controller = owned.clone();
        not_controller.owner_references.as_mut().unwrap()[0].controller = None;
        assert_eq!(controller_gateway(&not_controller), None);
        let mut deployment_owned = owned;
        deployment_owned.owner_references.as_mut().unwrap()[0].kind = "Deployment".into();
        assert_eq!(controller_gateway(&deployment_owned), None);
    }

    /// A Managed Gateway's snapshot binds what its Service targets.
    #[tokio::test]
    async fn the_snapshot_binds_the_service_targets() {
        let api = MockApi::default();
        let mut cluster = TestCluster::default();
        apply(
            &mut cluster.gateway_classes,
            testkit::gateway_class("portail", CONTROLLER),
        );
        let gw = gateway();
        apply(&mut cluster.gateways, gw.clone());
        let ctx = cluster.ctx(api.client());
        for kind in [Kind::ServiceAccount, Kind::Deployment, Kind::Service] {
            answer(&api, kind, "new", "1", None);
        }
        let Step::Continue(_) = publish(&gw, &ctx).await else {
            panic!("published");
        };
        let snapshot = ctx.hub.current(&testkit::key(&gw)).unwrap();
        assert_eq!(snapshot.config.gateway.listeners[0].target_port, Some(8000));
    }

    /// A name conflict is the Gateway's Programmed=False reason Invalid.
    #[tokio::test]
    async fn a_name_conflict_is_reported_in_programmed() {
        let api = MockApi::default();
        let mut cluster = TestCluster::default();
        apply(
            &mut cluster.gateway_classes,
            testkit::gateway_class("portail", CONTROLLER),
        );
        let gw = gateway();
        apply(&mut cluster.gateways, gw.clone());
        let ctx = cluster.ctx(api.client());
        answer(&api, Kind::ServiceAccount, "new", "1", None);
        answer(
            &api,
            Kind::Deployment,
            "x",
            "1",
            Some(json!({"name": NAME, "namespace": NS, "uid": "hand-written"})),
        );
        let Step::Continue(pass) = publish(&gw, &ctx).await else {
            panic!("a pass");
        };
        finish(&gw, &ctx, *pass).await.unwrap();
        let patch = api.status_patch("gateways", "gw").unwrap();
        let programmed = patch["status"]["conditions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["type"] == "Programmed")
            .unwrap()
            .clone();
        assert_eq!(programmed["status"], "False", "{programmed}");
        assert_eq!(programmed["reason"], "Invalid", "{programmed}");
        assert_eq!(
            programmed["message"],
            "name conflict: Deployment/portail-gw is not owned by this Gateway"
        );
        assert!(writes(&api)
            .iter()
            .all(|(method, path)| method == "PATCH" && path.ends_with("/gateways/gw/status")));
    }
}
