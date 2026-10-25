//! Controller test harness: a kube Client over an in-process tower service
//! that records every request, reflector stores the tests fill directly,
//! and [`Pipeline`], the controller and one data plane wired in-process.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use k8s_openapi::api::core::v1::{Namespace, Secret, Service};
use kube::runtime::controller::Action;
use kube::runtime::reflector::store::Writer;
use kube::runtime::watcher;
use kube::Client;
use serde_json::{json, Value};

use gateway_api::experimental::tcproutes::TCPRoute;
use gateway_api::experimental::tlsroutes::TLSRoute;
use gateway_api::experimental::udproutes::UDPRoute;
use gateway_api::gatewayclasses::{GatewayClass, GatewayClassSpec};
use gateway_api::gateways::{Gateway, GatewayListeners, GatewaySpec};
use gateway_api::httproutes::{HTTPRoute, HTTPRouteParentRefs, HTTPRouteSpec};
use gateway_api::referencegrants::ReferenceGrant;

use super::reconcile::{finish, forget_deleted_gateways, publish, Step};
use super::reports::InstanceId;
use super::{ControllerCtx, ReconcileError, ResourceCache};
use crate::dataplane::Applier;
use crate::kubernetes::crds::{GatewayParameters, RateLimitPolicy};
use crate::sync::{GatewayKey, Snapshot};

pub(super) const CONTROLLER: &str = "epheo.eu/portail";

/// One request the mock apiserver received.
#[derive(Clone, Debug)]
pub(super) struct Recorded {
    pub method: String,
    pub path: String,
    pub body: Value,
}

/// A stand-in apiserver: answers every write with a minimal object of the
/// written kind, a LIST with an empty list, or a reply a test registered
/// for a path prefix.
#[derive(Clone, Default)]
pub(super) struct MockApi {
    requests: Arc<Mutex<Vec<Recorded>>>,
    replies: Arc<Mutex<Vec<(String, u16, Value)>>>,
}

impl MockApi {
    pub fn client(&self) -> Client {
        let api = self.clone();
        let service = tower::service_fn(move |req: http::Request<kube::client::Body>| {
            let api = api.clone();
            async move {
                let (parts, body) = req.into_parts();
                let bytes = body.collect_bytes().await.unwrap_or_default();
                let recorded = Recorded {
                    method: parts.method.to_string(),
                    path: parts
                        .uri
                        .path_and_query()
                        .map(|p| p.to_string())
                        .unwrap_or_default(),
                    body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
                };
                let (code, reply) = api.reply(&recorded);
                api.requests.lock().unwrap().push(recorded);
                Ok::<_, std::convert::Infallible>(
                    http::Response::builder()
                        .status(code)
                        .body(kube::client::Body::from(
                            serde_json::to_vec(&reply).unwrap(),
                        ))
                        .unwrap(),
                )
            }
        });
        Client::new(service, "default")
    }

    /// Answer requests whose path starts with `prefix` with `body`, in place
    /// of any earlier reply for that prefix.
    pub fn reply_to(&self, prefix: &str, code: u16, body: Value) {
        let mut replies = self.replies.lock().unwrap();
        replies.retain(|(p, _, _)| p != prefix);
        replies.push((prefix.to_string(), code, body));
    }

    fn reply(&self, req: &Recorded) -> (u16, Value) {
        if let Some((_, code, body)) = self
            .replies
            .lock()
            .unwrap()
            .iter()
            .find(|(prefix, _, _)| req.path.starts_with(prefix.as_str()))
        {
            return (*code, body.clone());
        }
        if req.method == "GET" {
            return (
                200,
                json!({"apiVersion": "v1", "kind": "List", "metadata": {}, "items": []}),
            );
        }
        (200, object_at(&req.path))
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    pub fn clear(&self) {
        self.requests.lock().unwrap().clear();
    }

    /// PATCH bodies sent to `/{plural}/{name}/status`.
    pub fn status_patches(&self, plural: &str, name: &str) -> Vec<Value> {
        let suffix = format!("/{}/{}/status", plural, name);
        self.requests()
            .into_iter()
            .filter(|r| r.method == "PATCH" && r.path.split('?').next().unwrap().ends_with(&suffix))
            .map(|r| r.body)
            .collect()
    }

    /// The one status PATCH sent for `plural/name`, if any.
    pub fn status_patch(&self, plural: &str, name: &str) -> Option<Value> {
        let mut patches = self.status_patches(plural, name);
        assert!(
            patches.len() <= 1,
            "more than one write for {plural}/{name}"
        );
        patches.pop()
    }
}

/// A minimal object of the kind a write path names, enough for kube to
/// deserialize the reply.
fn object_at(path: &str) -> Value {
    let path = path.split('?').next().unwrap();
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    // apis/{group}/{version}/namespaces/{ns}/{plural}/{name}[/status]
    let (group, version, ns, plural, name) = (parts[1], parts[2], parts[4], parts[5], parts[6]);
    let (kind, spec) = match plural {
        "gateways" => (
            "Gateway",
            json!({"gatewayClassName": "portail", "listeners": []}),
        ),
        "httproutes" => ("HTTPRoute", json!({})),
        "tcproutes" => ("TCPRoute", json!({"rules": []})),
        "tlsroutes" => ("TLSRoute", json!({"hostnames": [], "rules": []})),
        "udproutes" => ("UDPRoute", json!({"rules": []})),
        "ratelimitpolicies" => (
            "RateLimitPolicy",
            json!({"targetRef": {"group": "", "kind": "Gateway", "name": "gw"}, "requestsPerSecond": 1}),
        ),
        other => panic!("mock apiserver has no object for {other}"),
    };
    json!({
        "apiVersion": format!("{group}/{version}"),
        "kind": kind,
        "metadata": {"name": name, "namespace": ns},
        "spec": spec,
    })
}

/// Reflector stores a test fills by hand, as watch events would.
#[derive(Default)]
pub(super) struct TestCluster {
    pub gateway_classes: Writer<GatewayClass>,
    pub gateways: Writer<Gateway>,
    pub http_routes: Writer<HTTPRoute>,
    pub tcp_routes: Writer<TCPRoute>,
    pub tls_routes: Writer<TLSRoute>,
    pub udp_routes: Writer<UDPRoute>,
    pub namespaces: Writer<Namespace>,
    pub services: Writer<Service>,
    pub secrets: Writer<Secret>,
    pub reference_grants: Writer<ReferenceGrant>,
    pub policies: Writer<RateLimitPolicy>,
    pub parameters: Writer<GatewayParameters>,
}

/// Store `obj` as a watch Apply event would.
pub(super) fn apply<K>(writer: &mut Writer<K>, obj: K)
where
    K: kube::Resource + Clone + 'static,
    K::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
    writer.apply_watcher_event(&watcher::Event::Apply(obj));
}

impl TestCluster {
    /// A controller context over these stores; it leads, its caches synced
    /// (no kind is tracked).
    pub fn ctx(&self, client: Client) -> Arc<ControllerCtx> {
        Arc::new(ControllerCtx {
            client,
            controller_name: CONTROLLER.to_string(),
            dataplane_image: "ghcr.io/epheo/portail:test".to_string(),
            cache: ResourceCache {
                gateway_classes: self.gateway_classes.as_reader(),
                gateways: self.gateways.as_reader(),
                http_routes: self.http_routes.as_reader(),
                tcp_routes: self.tcp_routes.as_reader(),
                tls_routes: self.tls_routes.as_reader(),
                udp_routes: self.udp_routes.as_reader(),
                namespaces: self.namespaces.as_reader(),
                services: self.services.as_reader(),
                secrets: self.secrets.as_reader(),
                reference_grants: self.reference_grants.as_reader(),
                rate_limit_policies: self.policies.as_reader(),
                gateway_parameters: self.parameters.as_reader(),
            },
            route_verdicts: Arc::default(),
            networks: Arc::default(),
            named_ports: Arc::new(crate::kubernetes::services::NamedPortCache::new(
                std::time::Duration::from_secs(super::RESYNC_SECS),
            )),
            hub: Arc::default(),
            reports: Arc::default(),
            leader: super::leader::Leader::always(),
            synced: super::watch::Listings::default(),
            owned: super::provision::Owned,
            departures: Default::default(),
        })
    }
}

/// GatewayClass `name` naming `controller`.
pub(super) fn gateway_class(name: &str, controller: &str) -> GatewayClass {
    GatewayClass {
        metadata: kube::core::ObjectMeta {
            name: Some(name.into()),
            generation: Some(1),
            ..Default::default()
        },
        spec: GatewayClassSpec {
            controller_name: controller.into(),
            ..Default::default()
        },
        status: None,
    }
}

/// The regression anchor of the controller split: a reconcile with one
/// data plane in the loop, in-process. Each pass translates and publishes,
/// applies the hub's snapshot (or clears on a 404), records the report and
/// writes status from it, as the sync protocol orders them.
pub(super) struct Pipeline {
    pub api: MockApi,
    pub cluster: TestCluster,
    pub ctx: Arc<ControllerCtx>,
    pub applier: Applier,
    /// The data plane's /readyz.
    pub ready: Arc<AtomicBool>,
}

impl Pipeline {
    /// Over an empty cluster but for GatewayClass `portail` of this
    /// controller, which [`gateway`] uses.
    pub fn new() -> Self {
        let api = MockApi::default();
        let mut cluster = TestCluster::default();
        apply(
            &mut cluster.gateway_classes,
            gateway_class("portail", CONTROLLER),
        );
        let ctx = cluster.ctx(api.client());
        let perf = crate::config::PerformanceConfig::default();
        let data_plane =
            crate::proxy::data_plane::DataPlane::new(&[], &perf, std::path::Path::new("/unused"))
                .unwrap();
        let ready = Arc::new(AtomicBool::new(false));
        let applier = Applier::new(
            Arc::new(Mutex::new(data_plane)),
            Arc::new(ArcSwap::from_pointee(crate::routing::RouteTable::new())),
            perf,
            ready.clone(),
        );
        Self {
            api,
            cluster,
            ctx,
            applier,
            ready,
        }
    }

    /// One reconcile of `gw`, its data plane in the loop.
    pub async fn pass(&mut self, gw: &Gateway) -> Result<Action, ReconcileError> {
        let step = publish(gw, &self.ctx).await;
        self.sync(gw).await;
        match step {
            Step::Done(action) => Ok(action),
            Step::Continue(pass) => finish(gw, &self.ctx, *pass).await,
        }
    }

    /// `gw` deleted: the store's delete event, the controller forgetting
    /// it, its data plane answered 404.
    pub async fn delete(&mut self, gw: &Gateway) {
        self.cluster
            .gateways
            .apply_watcher_event(&watcher::Event::Delete(gw.clone()));
        forget_deleted_gateways(&self.ctx);
        self.sync(gw).await;
    }

    /// What the hub serves for `gw`.
    pub fn served(&self, gw: &Gateway) -> Option<Arc<Snapshot>> {
        self.ctx.hub.current(&key(gw))
    }

    /// The data plane's poll: apply what the hub serves, or clear.
    async fn sync(&mut self, gw: &Gateway) {
        let key = key(gw);
        match self.ctx.hub.current(&key) {
            Some(snapshot) => {
                let report = self.applier.apply(&snapshot).await;
                self.ctx
                    .reports
                    .record(&key, InstanceId("pipeline-pod".into()), report);
            }
            None => self.applier.clear().await,
        }
    }
}

pub(super) fn key(gw: &Gateway) -> GatewayKey {
    GatewayKey::new(
        gw.metadata.namespace.as_deref().unwrap_or("default"),
        gw.metadata.name.as_deref().unwrap_or_default(),
    )
}

/// A Gateway with one HTTP listener on an ephemeral port, so binds never
/// collide with other tests.
pub(super) fn gateway(name: &str, generation: i64) -> Gateway {
    Gateway {
        metadata: kube::core::ObjectMeta {
            name: Some(name.into()),
            namespace: Some("default".into()),
            generation: Some(generation),
            resource_version: Some("1".into()),
            uid: Some(format!("{name}-uid")),
            ..Default::default()
        },
        spec: GatewaySpec {
            gateway_class_name: "portail".into(),
            listeners: vec![GatewayListeners {
                name: "http".into(),
                port: 0,
                protocol: "HTTP".into(),
                hostname: None,
                tls: None,
                allowed_routes: None,
            }],
            ..Default::default()
        },
        status: None,
    }
}

/// An HTTPRoute with no rules attached to the Gateways named in `parents`.
pub(super) fn http_route(name: &str, parents: &[&str], generation: i64) -> HTTPRoute {
    HTTPRoute {
        metadata: kube::core::ObjectMeta {
            name: Some(name.into()),
            namespace: Some("default".into()),
            generation: Some(generation),
            resource_version: Some("1".into()),
            ..Default::default()
        },
        spec: HTTPRouteSpec {
            parent_refs: Some(
                parents
                    .iter()
                    .map(|p| HTTPRouteParentRefs {
                        name: p.to_string(),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        },
        status: None,
    }
}

/// `route` with one rule sending everything to Service `backend` in
/// `default` on `port`.
pub(super) fn with_backend(mut route: HTTPRoute, backend: &str, port: i32) -> HTTPRoute {
    route.spec.rules = Some(vec![serde_json::from_value(json!({
        "backendRefs": [{"name": backend, "port": port}],
    }))
    .unwrap()]);
    route
}

/// A Service in `default` with one port; headless when `target` is a
/// named targetPort.
pub(super) fn service(name: &str, port_name: &str, port: i32, target: Value) -> Service {
    let headless = target.is_string();
    let mut svc: Service = serde_json::from_value(json!({
        "metadata": {"name": name, "namespace": "default", "resourceVersion": "1"},
        "spec": {
            "ports": [{"name": port_name, "port": port, "targetPort": target}],
        },
    }))
    .unwrap();
    if headless {
        svc.spec.as_mut().unwrap().cluster_ip = Some("None".into());
    }
    svc
}

/// One condition-bearing status entry by `controller` for Gateway `gw` in
/// `default`: a route parent when `ref_field` is "parentRef", a policy
/// ancestor when "ancestorRef".
pub(super) fn status_entry(controller: &str, gw: &str, ref_field: &str) -> Value {
    let mut entry = json!({
        "controllerName": controller,
        "conditions": [{
            "type": "Accepted",
            "status": "True",
            "reason": "Accepted",
            "message": "Accepted",
            "lastTransitionTime": "2020-01-01T00:00:00Z",
        }],
    });
    entry[ref_field] = json!({
        "group": "gateway.networking.k8s.io",
        "kind": "Gateway",
        "name": gw,
        "namespace": "default",
    });
    entry
}

/// A RateLimitPolicy in `default` targeting Gateway `gw`.
pub(super) fn rate_limit_policy(name: &str, gw: &str) -> RateLimitPolicy {
    use crate::kubernetes::crds::{PolicyTargetRef, RateLimitPolicySpec};
    let mut policy = RateLimitPolicy::new(
        name,
        RateLimitPolicySpec {
            target_ref: PolicyTargetRef {
                group: "gateway.networking.k8s.io".into(),
                kind: "Gateway".into(),
                name: gw.into(),
                section_name: None,
            },
            requests_per_second: 1,
            burst: None,
        },
    );
    policy.metadata.namespace = Some("default".into());
    policy.metadata.resource_version = Some("1".into());
    policy
}

/// `gw` as the store holds it once the apiserver applied `patch` (a
/// server-side-apply body of ours).
pub(super) fn with_gateway_status(mut gw: Gateway, patch: &Value) -> Gateway {
    gw.status = Some(serde_json::from_value(patch["status"].clone()).unwrap());
    gw
}

/// `route` as the store holds it once the apiserver applied `patch` (a
/// merge patch of ours), with a new resourceVersion.
pub(super) fn with_route_status(mut route: HTTPRoute, patch: &Value) -> HTTPRoute {
    route.status = Some(serde_json::from_value(patch["status"].clone()).unwrap());
    let rv: u64 = route
        .metadata
        .resource_version
        .as_deref()
        .unwrap_or("1")
        .parse()
        .unwrap();
    route.metadata.resource_version = Some((rv + 1).to_string());
    route
}
