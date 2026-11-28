//! Service metadata resolution — everything the route table needs to know
//! about Services beyond their name: ExternalName endpoint overrides,
//! appProtocol overrides, and headless service-port → targetPort mappings.
//!
//! Computed once per reconcile pass for the Services the Gateway's routes
//! name, each read through the recording store; the controller then keeps
//! what this Gateway's routes reference ([`retain_referenced`]).

use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::Api;
use kube::Client;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use super::converters::{backend_dns_name, parse_backend_dns_name};
use super::reads::Recorded;
use crate::config::PortailConfig;
use crate::logging::{debug, info, warn};

/// Resolved service metadata needed for route table construction.
#[derive(Default)]
pub(crate) struct ServiceState {
    pub endpoint_overrides: HashMap<(String, u16), Vec<(String, u16)>>,
    pub app_protocol_overrides: HashMap<(String, u16), String>,
    /// (backend_fqdn, service_port) → targetPort for headless services. Used so the
    /// data plane connects to DNS-resolved pod IPs on the targetPort, not the
    /// service port (headless has no VIP and DNS carries no port).
    pub headless_target_ports: HashMap<(String, u16), u16>,
    /// Backend names of the Services cluster DNS answers with a VIP: a real
    /// clusterIP, not None and not ExternalName.
    pub cluster_ip_backends: HashSet<String>,
}

/// A headless Service whose `targetPort` is *named*. The numeric value lives in
/// the pod/endpoint data, not the Service spec, so it is resolved by the caller
/// with a one-shot, label-filtered EndpointSlice list (see
/// [`resolve_named_target_ports`]) — a targeted read, not a watch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NamedTargetPortReq {
    ns: String,
    svc: String,
    service_version: Option<String>,
    service_port: u16,
    /// What the EndpointSlice port is named: slices carry the Service port's
    /// name (Service.ports[].name), not the targetPort's. Empty when unnamed.
    service_port_name: String,
    target_port_name: String,
}

/// The port numbers a Service's EndpointSlices publish, by port name, read
/// once per Service version and DNS epoch, and again once `ttl` has passed.
pub(crate) struct NamedPortCache {
    /// Bumped on every DNS refresh swap; an entry from an older epoch is stale.
    epoch: AtomicU64,
    /// With DNS refresh off no epoch moves: a read taken while the Service
    /// had no pods would otherwise stand for good.
    ttl: Duration,
    entries: Mutex<HashMap<(String, String), CachedPorts>>,
}

struct CachedPorts {
    service_version: Option<String>,
    epoch: u64,
    read_at: tokio::time::Instant,
    ports: HashMap<String, u16>,
}

impl NamedPortCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            epoch: AtomicU64::new(0),
            ttl,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// DNS refresh saw the resolved backends change: pods came or went, and
    /// a named port's number lives on the pods. Returns whether anything was
    /// cached, so whether a re-read is worth a reconcile.
    pub fn dns_changed(&self) -> bool {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let had_entries = !entries.is_empty();
        entries.clear();
        had_entries
    }

    fn get(&self, req: &NamedTargetPortReq, epoch: u64) -> Option<HashMap<String, u16>> {
        let entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        entries
            .get(&(req.ns.clone(), req.svc.clone()))
            .filter(|c| {
                c.epoch == epoch
                    && c.service_version == req.service_version
                    && c.read_at.elapsed() < self.ttl
            })
            .map(|c| c.ports.clone())
    }

    fn put(&self, req: &NamedTargetPortReq, epoch: u64, ports: HashMap<String, u16>) {
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        entries.insert(
            (req.ns.clone(), req.svc.clone()),
            CachedPorts {
                service_version: req.service_version.clone(),
                epoch,
                read_at: tokio::time::Instant::now(),
                ports,
            },
        );
    }
}

/// What the Services `backends` name tell the data plane: ExternalName
/// targets, appProtocols, headless targetPorts by service port, and
/// ClusterIPs. Each Service is looked up through `services`, so a pass
/// reads the Services it routes to, and no other.
///
/// Headless pod IPs are not special-cased: their per-pod A-records are resolved
/// at the data plane via STRICT_DNS ([`crate::routing::Backend::all_with_weight`])
/// and refreshed by the DNS-refresh task, so no cluster-wide EndpointSlice watch
/// is needed. The one thing the Service spec cannot give us is a *named*
/// targetPort's numeric value; those are returned as [`NamedTargetPortReq`]s for
/// the caller to resolve with a one-shot EndpointSlice list (see
/// [`resolve_named_target_ports`]) — a targeted read, still no watch.
pub(crate) fn resolve_backends<'a>(
    services: &Recorded<Service>,
    backends: impl IntoIterator<Item = &'a str>,
) -> (ServiceState, Vec<NamedTargetPortReq>) {
    let mut state = ServiceState::default();
    let mut named_target_ports: Vec<NamedTargetPortReq> = Vec::new();
    let mut seen = HashSet::new();
    for backend in backends {
        let Some((svc_name, svc_ns)) = parse_backend_dns_name(backend) else {
            continue;
        };
        if !seen.insert((svc_name.clone(), svc_ns.clone())) {
            continue;
        }
        if let Some(svc) = services.get(&svc_ns, &svc_name) {
            resolve_service(&svc, &mut state, &mut named_target_ports);
        }
    }
    (state, named_target_ports)
}

/// What one Service contributes to `state`.
fn resolve_service(
    svc: &Service,
    state: &mut ServiceState,
    named_target_ports: &mut Vec<NamedTargetPortReq>,
) {
    let (Some(spec), Some(svc_name)) = (svc.spec.as_ref(), svc.metadata.name.as_deref()) else {
        return;
    };
    let svc_ns = svc.metadata.namespace.as_deref().unwrap_or("default");
    let svc_fqdn = backend_dns_name(svc_name, None, svc_ns);
    let vip = spec
        .cluster_ip
        .as_deref()
        .is_some_and(|ip| !ip.is_empty() && ip != "None");
    if vip && spec.type_.as_deref() != Some("ExternalName") {
        state.cluster_ip_backends.insert(svc_fqdn.clone());
    }

    // Headless services have no VIP: the data plane connects directly to the pod
    // IPs DNS returns, so it must use the targetPort, not the service port. DNS
    // A-records carry no port, so derive the service-port → targetPort mapping from
    // the Service spec (which we still watch — no EndpointSlices). A *named*
    // targetPort is the one value the spec lacks; it is recorded for a one-shot
    // EndpointSlice list by the caller (no watch) and seeded with a service-port
    // fallback here so an absent/failed lookup degrades to prior behavior.
    // Headless services are marked clusterIP: None (stored as the literal "None").
    // ClusterIP/LoadBalancer resolve to a VIP and keep the service port (kube-proxy
    // maps it); ExternalName has no clusterIP and is handled via endpoint_overrides.
    if spec.cluster_ip.as_deref() == Some("None") {
        for sp in spec.ports.iter().flatten() {
            let service_port = sp.port as u16;
            let target_port = match sp.target_port.as_ref() {
                Some(IntOrString::Int(p)) => *p as u16,
                Some(IntOrString::String(name)) => {
                    // Numeric value lives in endpoint/pod data, not the spec.
                    // Defer to a one-shot EndpointSlice list; seed the service-port
                    // fallback so a failed/absent lookup keeps prior behavior.
                    named_target_ports.push(NamedTargetPortReq {
                        ns: svc_ns.to_string(),
                        svc: svc_name.to_string(),
                        service_version: svc.metadata.resource_version.clone(),
                        service_port,
                        service_port_name: sp.name.clone().unwrap_or_default(),
                        target_port_name: name.clone(),
                    });
                    service_port
                }
                // An omitted targetPort defaults to the service port.
                None => service_port,
            };
            state
                .headless_target_ports
                .insert((svc_fqdn.clone(), service_port), target_port);
        }
    }

    // ExternalName services: resolve externalName DNS targets, built purely
    // from the Service spec (no EndpointSlices).
    if spec.type_.as_deref() == Some("ExternalName") {
        if let Some(external_name) = spec.external_name.as_deref().filter(|n| !n.is_empty()) {
            if let Some(ports) = spec.ports.as_ref() {
                for sp in ports {
                    let service_port = sp.port as u16;
                    state
                        .endpoint_overrides
                        .entry((svc_fqdn.clone(), service_port))
                        .or_default()
                        .push((external_name.to_string(), service_port));
                }
                info!(
                    "ExternalName service {}.{} -> {} (ports: {})",
                    svc_name,
                    svc_ns,
                    external_name,
                    ports
                        .iter()
                        .map(|p| p.port.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
    }

    // appProtocol overrides from the Service's port specs.
    for sp in spec.ports.iter().flatten() {
        if let Some(ref app_proto) = sp.app_protocol {
            state
                .app_protocol_overrides
                .insert((svc_fqdn.clone(), sp.port as u16), app_proto.clone());
        }
    }
}

/// Whether `svc` is of type ExternalName.
pub(crate) fn is_external_name(svc: &Service) -> bool {
    svc.spec.as_ref().and_then(|s| s.type_.as_deref()) == Some("ExternalName")
}

impl NamedTargetPortReq {
    /// The override-map key this request resolves.
    pub fn backend(&self) -> (String, u16) {
        (
            backend_dns_name(&self.svc, None, &self.ns),
            self.service_port,
        )
    }
}

/// (FQDN, port) of every backend a rule of `config` routes or mirrors to:
/// the keys the override maps are read with. An invalid ref is never
/// resolved, and a RefNotPermitted one names a namespace this Gateway may
/// not read.
pub(crate) fn referenced_backends(config: &PortailConfig) -> HashSet<(String, u16)> {
    config
        .http_backend_refs()
        .chain(config.grpc_backend_refs())
        .chain(config.l4_backend_refs())
        .filter(|b| !b.invalid)
        .map(|b| (b.name.clone(), b.port))
        .collect()
}

/// (FQDN, port) of every backend an HTTP or gRPC rule routes or mirrors to:
/// the connections a BackendTLSPolicy verifies. TLSRoute is served in
/// Passthrough only, where the client's TLS reaches the backend.
pub(crate) fn verifiable_backends(config: &PortailConfig) -> BTreeSet<(String, u16)> {
    config
        .http_backend_refs()
        .chain(config.grpc_backend_refs())
        .filter(|b| !b.invalid)
        .map(|b| (b.name.clone(), b.port))
        .collect()
}

/// Drop the override entries of Services `config` does not route to. The
/// maps are fingerprinted, so an unrelated Service change would otherwise
/// rebuild the table and rewrite status in every pod.
pub(crate) fn retain_referenced(config: &mut PortailConfig, referenced: &HashSet<(String, u16)>) {
    config
        .endpoint_overrides
        .retain(|k, _| referenced.contains(k));
    config
        .app_protocol_overrides
        .retain(|k, _| referenced.contains(k));
    config
        .headless_target_ports
        .retain(|k, _| referenced.contains(k));
}

/// The port numbers a Service's EndpointSlices publish, by the Service
/// port's name ("" when unnamed: the slice omits it too). All slices of a
/// Service share the same port mapping, so the first match wins.
fn slice_ports(slices: &[EndpointSlice]) -> HashMap<String, u16> {
    let mut ports = HashMap::new();
    for p in slices.iter().filter_map(|s| s.ports.as_ref()).flatten() {
        if let Some(number) = p.port.and_then(|n| u16::try_from(n).ok()) {
            ports
                .entry(p.name.clone().unwrap_or_default())
                .or_insert(number);
        }
    }
    ports
}

/// Resolve headless *named* targetPorts via a one-shot, label-filtered
/// EndpointSlice list per service — a targeted read, NOT a watch (so it does not
/// reintroduce the cluster-wide EndpointSlice firehose this branch removed). On
/// success, overwrites the `(fqdn, service_port)` entry in `out` with the numeric
/// targetPort; on a miss or API error it leaves the service-port fallback seeded
/// by [`resolve_backends`] in place and warns. Never fatal.
///
/// A read is cached until the Service changes, DNS refresh sees pods change
/// ([`NamedPortCache::dns_changed`]) or the cache's ttl passes; passes in
/// between read nothing.
pub(crate) async fn resolve_named_target_ports(
    client: &Client,
    cache: &NamedPortCache,
    reqs: &[NamedTargetPortReq],
    out: &mut HashMap<(String, u16), u16>,
) {
    let epoch = cache.epoch.load(Ordering::Acquire);
    for req in reqs {
        let (ports, fresh) = match cache.get(req, epoch) {
            Some(ports) => (ports, false),
            None => {
                let api: Api<EndpointSlice> = Api::namespaced(client.clone(), &req.ns);
                let lp = kube::api::ListParams::default()
                    .labels(&format!("kubernetes.io/service-name={}", req.svc));
                match api.list(&lp).await {
                    Ok(list) => {
                        let ports = slice_ports(&list.items);
                        cache.put(req, epoch, ports.clone());
                        (ports, true)
                    }
                    Err(e) => {
                        warn!(
                            "Headless {}/{} port {}: EndpointSlice list for named targetPort \
                             {:?} failed ({}); using the service port",
                            req.ns, req.svc, req.service_port, req.target_port_name, e
                        );
                        continue;
                    }
                }
            }
        };
        match ports.get(&req.service_port_name) {
            Some(&port) => {
                out.insert(req.backend(), port);
                debug!(
                    "Headless {}/{} port {} named targetPort {:?} -> {}",
                    req.ns, req.svc, req.service_port, req.target_port_name, port
                );
            }
            None if fresh => warn!(
                "Headless {}/{} port {}: named targetPort {:?} not found in \
                 EndpointSlices; using the service port",
                req.ns, req.svc, req.service_port, req.target_port_name
            ),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store of `services`.
    fn services(services: impl IntoIterator<Item = Service>) -> Recorded<Service> {
        Recorded::of(crate::kubernetes::reads::Kind::Service, services)
    }

    #[test]
    fn slice_ports_match_by_name() {
        use k8s_openapi::api::discovery::v1::{EndpointPort, EndpointSlice};
        let slice = EndpointSlice {
            ports: Some(vec![
                EndpointPort {
                    name: Some("metrics".to_string()),
                    port: Some(9000),
                    ..Default::default()
                },
                EndpointPort {
                    name: Some("http".to_string()),
                    port: Some(8080),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        assert_eq!(
            slice_ports(std::slice::from_ref(&slice)).get("http"),
            Some(&8080)
        );
        // Unknown name → None (caller keeps the service-port fallback).
        assert_eq!(slice_ports(&[slice]).get("grpc"), None);
        // No slices at all → None.
        assert_eq!(slice_ports(&[]).get("http"), None);
    }

    #[test]
    fn slice_ports_scan_across_slices() {
        use k8s_openapi::api::discovery::v1::{EndpointPort, EndpointSlice};
        // The matching port lives in the second slice; the first has none.
        let empty = EndpointSlice {
            ports: Some(vec![]),
            ..Default::default()
        };
        let has = EndpointSlice {
            ports: Some(vec![EndpointPort {
                name: Some("http".to_string()),
                port: Some(3000),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert_eq!(slice_ports(&[empty, has]).get("http"), Some(&3000));
    }

    fn headless(
        name: &str,
        ns: &str,
        port_name: Option<&str>,
        port: i32,
        target: IntOrString,
    ) -> k8s_openapi::api::core::v1::Service {
        use k8s_openapi::api::core::v1::{Service, ServicePort, ServiceSpec};
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
        Service {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some(ns.to_string()),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                cluster_ip: Some("None".to_string()),
                ports: Some(vec![ServicePort {
                    name: port_name.map(str::to_string),
                    port,
                    target_port: Some(target),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// EndpointSlice ports carry the Service port's name: a port `web` with
    /// targetPort `http` publishes `web` with the container's number. The
    /// lookup used the targetPort name, which only matched when both names
    /// were the same.
    #[test]
    fn named_target_port_resolves_by_service_port_name() {
        use k8s_openapi::api::discovery::v1::{EndpointPort, EndpointSlice};
        let svc = headless(
            "db",
            "prod",
            Some("web"),
            80,
            IntOrString::String("http".to_string()),
        );
        let (_, reqs) = resolve_backends(&services([svc]), ["db.prod.svc"]);
        let slice = EndpointSlice {
            ports: Some(vec![EndpointPort {
                name: Some("web".to_string()),
                port: Some(8080),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert_eq!(
            slice_ports(&[slice]).get(&reqs[0].service_port_name),
            Some(&8080)
        );

        // An unnamed Service port: the slice port is unnamed too.
        let unnamed_slice = EndpointSlice {
            ports: Some(vec![EndpointPort {
                name: None,
                port: Some(3000),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert_eq!(slice_ports(&[unnamed_slice]).get(""), Some(&3000));
    }

    /// The table build reads a mirror's Service through the override maps,
    /// as a backendRef's: the trim dropped its entries.
    #[test]
    fn a_mirrored_service_keeps_its_override_entries() {
        let json = r#"{"httpRoutes": [{"hostnames": ["m.example.com"], "rules": [{
            "filters": [{"type": "RequestMirror", "requestMirror": {"backendRef": {"name": "shadow.prod.svc", "port": 80}}}],
            "backendRefs": [{"name": "web.prod.svc", "port": 80}]}]}]}"#;
        let mut config: PortailConfig = serde_json::from_str(json).unwrap();
        for svc in ["web.prod.svc", "shadow.prod.svc", "other.prod.svc"] {
            config
                .headless_target_ports
                .insert((svc.to_string(), 80), 8080);
        }
        let referenced = referenced_backends(&config);
        retain_referenced(&mut config, &referenced);
        let mut kept: Vec<_> = config
            .headless_target_ports
            .keys()
            .map(|(svc, _)| svc.as_str())
            .collect();
        kept.sort_unstable();
        assert_eq!(kept, ["shadow.prod.svc", "web.prod.svc"]);
    }

    #[test]
    fn an_external_name_backend_is_overridden_by_its_target() {
        use k8s_openapi::api::core::v1::{ServicePort, ServiceSpec};
        let external = Service {
            metadata: kube::core::ObjectMeta {
                name: Some("ext".to_string()),
                namespace: Some("tenant".to_string()),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                type_: Some("ExternalName".to_string()),
                external_name: Some("kubernetes.default.svc.cluster.local".to_string()),
                ports: Some(vec![ServicePort {
                    port: 443,
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let plain = headless("db", "tenant", None, 5432, IntOrString::Int(5432));
        let (state, _) = resolve_backends(
            &services([external, plain]),
            ["ext.tenant.svc", "db.tenant.svc"],
        );
        assert_eq!(
            state.endpoint_overrides,
            HashMap::from([(
                ("ext.tenant.svc".to_string(), 443),
                vec![("kubernetes.default.svc.cluster.local".to_string(), 443)]
            )])
        );
    }

    #[test]
    fn a_named_target_port_is_deferred() {
        // Named targetPort: emit a resolve request AND seed the service-port
        // fallback, so an absent/failed EndpointSlice list degrades to prior behavior.
        let named = headless(
            "db",
            "prod",
            Some("postgres"),
            5432,
            IntOrString::String("pg".to_string()),
        );
        let (state, reqs) = resolve_backends(&services([named]), ["db.prod.svc"]);
        assert_eq!(reqs.len(), 1);
        assert_eq!(
            (
                reqs[0].ns.as_str(),
                reqs[0].svc.as_str(),
                reqs[0].service_port,
                reqs[0].service_port_name.as_str(),
                reqs[0].target_port_name.as_str()
            ),
            ("prod", "db", 5432, "postgres", "pg")
        );
        assert_eq!(
            state
                .headless_target_ports
                .get(&("db.prod.svc".to_string(), 5432)),
            Some(&5432),
            "fallback to the service port until the EndpointSlice list resolves it"
        );

        // Numeric targetPort: resolved directly from the spec, no request emitted.
        let numeric = headless("cache", "prod", None, 6379, IntOrString::Int(6380));
        let (state, reqs) = resolve_backends(&services([numeric]), ["cache.prod.svc"]);
        assert!(reqs.is_empty());
        assert_eq!(
            state
                .headless_target_ports
                .get(&("cache.prod.svc".to_string(), 6379)),
            Some(&6380)
        );
    }
}
