//! Controller test harness: a kube Client over an in-process tower service
//! that records every request, and reflector stores the tests fill directly.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use arc_swap::{ArcSwap, ArcSwapOption};
use k8s_openapi::api::core::v1::{Namespace, Secret, Service};
use kube::runtime::reflector::store::Writer;
use kube::runtime::watcher;
use kube::Client;
use serde_json::{json, Value};

use gateway_api::experimental::tcproutes::TCPRoute;
use gateway_api::experimental::tlsroutes::TLSRoute;
use gateway_api::experimental::udproutes::UDPRoute;
use gateway_api::gateways::{Gateway, GatewayListeners, GatewaySpec};
use gateway_api::httproutes::{HTTPRoute, HTTPRouteParentRefs, HTTPRouteSpec};
use gateway_api::referencegrants::ReferenceGrant;

use super::{ControllerCtx, ResourceCache};
use crate::kubernetes::crds::RateLimitPolicy;

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
    /// A scoped controller context (one Gateway, operator-managed status
    /// off) over these stores, with an empty data plane.
    pub fn ctx(&self, client: Client) -> Arc<ControllerCtx> {
        let perf = crate::config::PerformanceConfig::default();
        let data_plane =
            crate::proxy::data_plane::DataPlane::new(&[], &perf, std::path::Path::new("/unused"))
                .unwrap();
        Arc::new(ControllerCtx {
            client,
            controller_name: CONTROLLER.to_string(),
            cache: ResourceCache {
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
                gateway_classes: None,
            },
            routes: Arc::new(ArcSwap::from_pointee(crate::routing::RouteTable::new())),
            published: Arc::new(ArcSwapOption::empty()),
            data_plane: Arc::new(std::sync::Mutex::new(data_plane)),
            performance_config: perf,
            manage_gateway_status: true,
            ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_applied: Arc::new(std::sync::Mutex::new(None)),
            apply_lock: Arc::new(tokio::sync::Mutex::new(())),
            gateway_configs: Arc::new(std::sync::Mutex::new(HashMap::new())),
            named_ports: Default::default(),
            pending_watches: Arc::new(super::PendingWatches::new(
                std::time::Instant::now() + std::time::Duration::from_secs(super::RESYNC_SECS),
            )),
            started: std::time::Instant::now(),
        })
    }
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
