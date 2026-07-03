//! Service metadata resolution — everything the route table needs to know
//! about Services beyond their name: ExternalName endpoint overrides,
//! appProtocol overrides, and headless service-port → targetPort mappings.
//!
//! Gateway-independent: computed once per reconcile pass from the Service
//! reflector store, then threaded into `reconcile_to_config`.

use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::Api;
use kube::Client;
use std::collections::{HashMap, HashSet};

use crate::logging::{debug, info, warn};

/// Resolved service metadata needed for route table construction.
pub(crate) struct ServiceState {
    pub known_services: HashSet<(String, String)>,
    pub endpoint_overrides: HashMap<(String, u16), Vec<(String, u16)>>,
    pub app_protocol_overrides: HashMap<(String, u16), String>,
    /// (backend_fqdn, service_port) → targetPort for headless services. Used so the
    /// data plane connects to DNS-resolved pod IPs on the targetPort, not the
    /// service port (headless has no VIP and DNS carries no port).
    pub headless_target_ports: HashMap<(String, u16), u16>,
}

/// A headless Service whose `targetPort` is *named*. The numeric value lives in
/// the pod/endpoint data, not the Service spec, so it is resolved by the caller
/// with a one-shot, label-filtered EndpointSlice list (see
/// [`resolve_named_target_ports`]) — a targeted read, not a watch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NamedTargetPortReq {
    ns: String,
    svc: String,
    service_port: u16,
    port_name: String,
}

/// Resolve service metadata: known services set, ExternalName overrides,
/// appProtocol overrides, and the headless service-port → targetPort map.
///
/// Headless pod IPs are not special-cased: their per-pod A-records are resolved
/// at the data plane via STRICT_DNS ([`crate::routing::Backend::all_with_weight`])
/// and refreshed by the DNS-refresh task, so no cluster-wide EndpointSlice watch
/// is needed. The one thing the Service spec cannot give us is a *named*
/// targetPort's numeric value; those are returned as [`NamedTargetPortReq`]s for
/// the caller to resolve with a one-shot EndpointSlice list (see
/// [`resolve_named_target_ports`]) — a targeted read, still no watch.
pub(crate) fn resolve_services(
    all_services: &[std::sync::Arc<Service>],
) -> (ServiceState, Vec<NamedTargetPortReq>) {
    let known_services: HashSet<(String, String)> = all_services
        .iter()
        .filter_map(|svc| {
            let name = svc.metadata.name.as_ref()?;
            let ns = svc.metadata.namespace.as_deref().unwrap_or("default");
            Some((name.clone(), ns.to_string()))
        })
        .collect();

    // Headless services have no VIP: the data plane connects directly to the pod
    // IPs DNS returns, so it must use the targetPort, not the service port. DNS
    // A-records carry no port, so derive the service-port → targetPort mapping from
    // the Service spec (which we still watch — no EndpointSlices). A *named*
    // targetPort is the one value the spec lacks; it is recorded for a one-shot
    // EndpointSlice list by the caller (no watch) and seeded with a service-port
    // fallback here so an absent/failed lookup degrades to prior behavior.
    let mut headless_target_ports: HashMap<(String, u16), u16> = HashMap::new();
    let mut named_target_ports: Vec<NamedTargetPortReq> = Vec::new();
    for svc in all_services {
        let spec = match svc.spec.as_ref() {
            Some(s) => s,
            None => continue,
        };
        // Headless services are marked clusterIP: None (stored as the literal "None").
        // ClusterIP/LoadBalancer resolve to a VIP and keep the service port (kube-proxy
        // maps it); ExternalName has no clusterIP and is handled via endpoint_overrides.
        if spec.cluster_ip.as_deref() != Some("None") {
            continue;
        }
        let svc_name = match svc.metadata.name.as_deref() {
            Some(n) => n,
            None => continue,
        };
        let svc_ns = svc.metadata.namespace.as_deref().unwrap_or("default");
        let svc_fqdn = format!("{}.{}.svc", svc_name, svc_ns);
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
                        service_port,
                        port_name: name.clone(),
                    });
                    service_port
                }
                // An omitted targetPort defaults to the service port.
                None => service_port,
            };
            headless_target_ports.insert((svc_fqdn.clone(), service_port), target_port);
        }
    }

    // ExternalName services contribute override entries below (built purely from the
    // Service spec — no EndpointSlices). Headless pod IPs come from DNS; only their
    // targetPort mapping (above) is taken from the Service spec.
    let mut endpoint_overrides: HashMap<(String, u16), Vec<(String, u16)>> = HashMap::new();

    // ExternalName services: resolve externalName DNS targets
    for svc in all_services {
        let spec = match svc.spec.as_ref() {
            Some(s) => s,
            None => continue,
        };
        if spec.type_.as_deref().unwrap_or("") != "ExternalName" {
            continue;
        }
        let external_name = match spec.external_name.as_deref() {
            Some(n) if !n.is_empty() => n,
            _ => continue,
        };
        let svc_name = match svc.metadata.name.as_deref() {
            Some(n) => n,
            None => continue,
        };
        let svc_ns = svc.metadata.namespace.as_deref().unwrap_or("default");
        let svc_fqdn = format!("{}.{}.svc", svc_name, svc_ns);

        if let Some(ports) = spec.ports.as_ref() {
            for sp in ports {
                let service_port = sp.port as u16;
                let key = (svc_fqdn.clone(), service_port);
                endpoint_overrides
                    .entry(key)
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

    // Build appProtocol overrides from Service port specs
    let mut app_protocol_overrides: HashMap<(String, u16), String> = HashMap::new();
    for svc in all_services {
        let spec = match svc.spec.as_ref() {
            Some(s) => s,
            None => continue,
        };
        let svc_name = match svc.metadata.name.as_deref() {
            Some(n) => n,
            None => continue,
        };
        let svc_ns = svc.metadata.namespace.as_deref().unwrap_or("default");
        let svc_fqdn = format!("{}.{}.svc", svc_name, svc_ns);
        if let Some(ports) = spec.ports.as_ref() {
            for sp in ports {
                if let Some(ref app_proto) = sp.app_protocol {
                    let key = (svc_fqdn.clone(), sp.port as u16);
                    app_protocol_overrides.insert(key, app_proto.clone());
                }
            }
        }
    }

    (
        ServiceState {
            known_services,
            endpoint_overrides,
            app_protocol_overrides,
            headless_target_ports,
        },
        named_target_ports,
    )
}

/// Pick the numeric value of a named port from a service's EndpointSlices.
/// All slices of a Service share the same port mapping, so the first match wins.
/// Returns `None` if no slice publishes a port with that name.
fn pick_named_port(slices: &[EndpointSlice], port_name: &str) -> Option<u16> {
    slices
        .iter()
        .filter_map(|s| s.ports.as_ref())
        .flatten()
        .find(|p| p.name.as_deref() == Some(port_name))
        .and_then(|p| p.port)
        .map(|p| p as u16)
}

/// Resolve headless *named* targetPorts via a one-shot, label-filtered
/// EndpointSlice list per service — a targeted read, NOT a watch (so it does not
/// reintroduce the cluster-wide EndpointSlice firehose this branch removed). On
/// success, overwrites the `(fqdn, service_port)` entry in `out` with the numeric
/// targetPort; on a miss or API error it leaves the service-port fallback seeded
/// by [`resolve_services`] in place and warns. Never fatal.
pub(crate) async fn resolve_named_target_ports(
    client: &Client,
    reqs: &[NamedTargetPortReq],
    out: &mut HashMap<(String, u16), u16>,
) {
    for req in reqs {
        let api: Api<EndpointSlice> = Api::namespaced(client.clone(), &req.ns);
        let lp = kube::api::ListParams::default()
            .labels(&format!("kubernetes.io/service-name={}", req.svc));
        match api.list(&lp).await {
            Ok(list) => match pick_named_port(&list.items, &req.port_name) {
                Some(port) => {
                    let fqdn = format!("{}.{}.svc", req.svc, req.ns);
                    out.insert((fqdn, req.service_port), port);
                    debug!(
                        "Headless {}/{} port {} named targetPort {:?} -> {}",
                        req.ns, req.svc, req.service_port, req.port_name, port
                    );
                }
                None => warn!(
                    "Headless {}/{} port {}: named targetPort {:?} not found in \
                     EndpointSlices; using the service port",
                    req.ns, req.svc, req.service_port, req.port_name
                ),
            },
            Err(e) => warn!(
                "Headless {}/{} port {}: EndpointSlice list for named targetPort \
                 {:?} failed ({}); using the service port",
                req.ns, req.svc, req.service_port, req.port_name, e
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_named_port_matches_by_name() {
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
            pick_named_port(std::slice::from_ref(&slice), "http"),
            Some(8080)
        );
        // Unknown name → None (caller keeps the service-port fallback).
        assert_eq!(pick_named_port(&[slice], "grpc"), None);
        // No slices at all → None.
        assert_eq!(pick_named_port(&[], "http"), None);
    }

    #[test]
    fn pick_named_port_scans_across_slices() {
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
        assert_eq!(pick_named_port(&[empty, has], "http"), Some(3000));
    }

    #[test]
    fn resolve_services_defers_named_targetport() {
        use k8s_openapi::api::core::v1::{Service, ServicePort, ServiceSpec};
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

        fn headless(name: &str, ns: &str, port: i32, target: IntOrString) -> Service {
            Service {
                metadata: ObjectMeta {
                    name: Some(name.to_string()),
                    namespace: Some(ns.to_string()),
                    ..Default::default()
                },
                spec: Some(ServiceSpec {
                    cluster_ip: Some("None".to_string()),
                    ports: Some(vec![ServicePort {
                        port,
                        target_port: Some(target),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            }
        }

        // Named targetPort: emit a resolve request AND seed the service-port
        // fallback, so an absent/failed EndpointSlice list degrades to prior behavior.
        let named = headless("db", "prod", 5432, IntOrString::String("pg".to_string()));
        let (state, reqs) = resolve_services(&[std::sync::Arc::new(named)]);
        assert_eq!(reqs.len(), 1);
        assert_eq!(
            (
                reqs[0].ns.as_str(),
                reqs[0].svc.as_str(),
                reqs[0].service_port,
                reqs[0].port_name.as_str()
            ),
            ("prod", "db", 5432, "pg")
        );
        assert_eq!(
            state
                .headless_target_ports
                .get(&("db.prod.svc".to_string(), 5432)),
            Some(&5432),
            "fallback to the service port until the EndpointSlice list resolves it"
        );

        // Numeric targetPort: resolved directly from the spec, no request emitted.
        let numeric = headless("cache", "prod", 6379, IntOrString::Int(6380));
        let (state, reqs) = resolve_services(&[std::sync::Arc::new(numeric)]);
        assert!(reqs.is_empty());
        assert_eq!(
            state
                .headless_target_ports
                .get(&("cache.prod.svc".to_string(), 6379)),
            Some(&6380)
        );
    }
}
