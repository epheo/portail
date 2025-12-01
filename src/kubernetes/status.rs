use std::collections::HashMap;
use std::fmt::Debug;

use kube::api::{Api, PatchParams, Patch};
use kube::Client;
use kube::ResourceExt;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use k8s_openapi::chrono::Utc;
use serde::de::DeserializeOwned;
use serde::Serialize;

use gateway_api::gateways::Gateway;
use gateway_api::gatewayclasses::GatewayClass;

use crate::logging::{debug, warn};

fn supported_kinds_for_protocol(protocol: &str) -> Vec<serde_json::Value> {
    match protocol {
        "HTTP" => vec![serde_json::json!({"group": "gateway.networking.k8s.io", "kind": "HTTPRoute"})],
        "HTTPS" => vec![serde_json::json!({"group": "gateway.networking.k8s.io", "kind": "HTTPRoute"})],
        "TLS" => vec![serde_json::json!({"group": "gateway.networking.k8s.io", "kind": "TLSRoute"})],
        "TCP" => vec![serde_json::json!({"group": "gateway.networking.k8s.io", "kind": "TCPRoute"})],
        "UDP" => vec![serde_json::json!({"group": "gateway.networking.k8s.io", "kind": "UDPRoute"})],
        _ => vec![],
    }
}

pub async fn update_gateway_status(
    client: &Client,
    gateway: &Gateway,
    programmed: bool,
    message: &str,
    listener_route_counts: &HashMap<String, i32>,
) {
    let name = gateway.name_any();
    let ns = gateway.namespace().unwrap_or_else(|| "default".to_string());
    let api: Api<Gateway> = Api::namespaced(client.clone(), &ns);

    let now = k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(Utc::now());
    let generation = gateway.metadata.generation;

    let conditions = vec![
        Condition {
            type_: "Accepted".to_string(),
            status: "True".to_string(),
            reason: "Accepted".to_string(),
            message: "Gateway accepted by portail controller".to_string(),
            last_transition_time: now.clone(),
            observed_generation: generation,
        },
        Condition {
            type_: "Programmed".to_string(),
            status: if programmed { "True" } else { "False" }.to_string(),
            reason: if programmed { "Programmed" } else { "Invalid" }.to_string(),
            message: message.to_string(),
            last_transition_time: now.clone(),
            observed_generation: generation,
        },
    ];

    // Detect listener conflicts: same port, different protocols
    let mut port_protocols: HashMap<i32, String> = HashMap::new();
    let mut conflicted_ports: std::collections::HashSet<i32> = std::collections::HashSet::new();
    for l in &gateway.spec.listeners {
        match port_protocols.get(&l.port) {
            Some(existing_proto) if *existing_proto != l.protocol => {
                conflicted_ports.insert(l.port);
            }
            None => { port_protocols.insert(l.port, l.protocol.clone()); }
            _ => {}
        }
    }

    let listeners: Vec<serde_json::Value> = gateway
        .spec
        .listeners
        .iter()
        .map(|l| {
            let attached = listener_route_counts.get(&l.name).copied().unwrap_or(0);
            let is_conflicted = conflicted_ports.contains(&l.port);
            serde_json::json!({
                "name": l.name,
                "attachedRoutes": attached,
                "supportedKinds": supported_kinds_for_protocol(&l.protocol),
                "conditions": [
                    {
                        "type": "Accepted",
                        "status": if is_conflicted { "False" } else { "True" },
                        "reason": if is_conflicted { "PortConflict" } else { "Accepted" },
                        "message": if is_conflicted { "Listener port conflicts with another listener using a different protocol" } else { "Listener accepted" },
                        "lastTransitionTime": now,
                        "observedGeneration": generation,
                    },
                    {
                        "type": "Programmed",
                        "status": if programmed && !is_conflicted { "True" } else { "False" },
                        "reason": if programmed && !is_conflicted { "Programmed" } else { "Invalid" },
                        "message": if is_conflicted { "Listener not programmed due to port conflict" } else { message },
                        "lastTransitionTime": now,
                        "observedGeneration": generation,
                    },
                    {
                        "type": "Conflicted",
                        "status": if is_conflicted { "True" } else { "False" },
                        "reason": if is_conflicted { "ProtocolConflict" } else { "NoConflicts" },
                        "message": if is_conflicted {
                            "Listener port shared with another listener using different protocol"
                        } else {
                            "No conflicts detected"
                        },
                        "lastTransitionTime": now,
                        "observedGeneration": generation,
                    },
                ],
            })
        })
        .collect();

    // Build status.addresses — use NODE_IP or POD_IP from Downward API if available
    let gateway_ip = std::env::var("NODE_IP")
        .or_else(|_| std::env::var("POD_IP"))
        .unwrap_or_else(|_| "0.0.0.0".to_string());
    let addresses = vec![serde_json::json!({
        "type": "IPAddress",
        "value": gateway_ip,
    })];

    let status = serde_json::json!({
        "status": {
            "conditions": conditions,
            "listeners": listeners,
            "addresses": addresses,
        }
    });

    match api
        .patch_status(&name, &PatchParams::apply("portail").force(), &Patch::Apply(status))
        .await
    {
        Ok(_) => debug!("Updated Gateway {}/{} status", ns, name),
        Err(e) => warn!("Failed to update Gateway {}/{} status: {}", ns, name, e),
    }
}

pub async fn update_gateway_class_status(
    client: &Client,
    gc: &GatewayClass,
    accepted: bool,
    message: &str,
) {
    let name = gc.name_any();
    let api: Api<GatewayClass> = Api::all(client.clone());

    let now = k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(Utc::now());
    let generation = gc.metadata.generation;

    let conditions = vec![Condition {
        type_: "Accepted".to_string(),
        status: if accepted { "True" } else { "False" }.to_string(),
        reason: if accepted { "Accepted" } else { "InvalidParameters" }.to_string(),
        message: message.to_string(),
        last_transition_time: now,
        observed_generation: generation,
    }];

    let status = serde_json::json!({
        "status": {
            "conditions": conditions,
        }
    });

    match api
        .patch_status(&name, &PatchParams::apply("portail").force(), &Patch::Apply(status))
        .await
    {
        Ok(_) => debug!("Updated GatewayClass {} status", name),
        Err(e) => warn!("Failed to update GatewayClass {} status: {}", name, e),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn update_route_status<K>(
    client: &Client,
    route_name: &str,
    route_namespace: &str,
    controller_name: &str,
    gateway_name: &str,
    gateway_namespace: &str,
    section_name: Option<&str>,
    accepted: bool,
    message: &str,
    refs_resolved: bool,
    refs_message: &str,
    programmed: bool,
    generation: Option<i64>,
)
where
    K: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope>
        + Serialize
        + DeserializeOwned
        + Clone
        + Debug,
    <K as kube::Resource>::DynamicType: Default,
{
    let api: Api<K> = Api::namespaced(client.clone(), route_namespace);

    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let mut parent_ref = serde_json::json!({
        "group": "gateway.networking.k8s.io",
        "kind": "Gateway",
        "name": gateway_name,
        "namespace": gateway_namespace,
    });
    if let Some(section) = section_name {
        parent_ref["sectionName"] = serde_json::json!(section);
    }

    let status = serde_json::json!({
        "status": {
            "parents": [
                {
                    "controllerName": controller_name,
                    "parentRef": parent_ref,
                    "conditions": [
                        {
                            "type": "Accepted",
                            "status": if accepted { "True" } else { "False" },
                            "reason": if accepted { "Accepted" } else { "NotAllowedByListeners" },
                            "message": message,
                            "lastTransitionTime": now,
                            "observedGeneration": generation,
                        },
                        {
                            "type": "ResolvedRefs",
                            "status": if refs_resolved { "True" } else { "False" },
                            "reason": if refs_resolved { "ResolvedRefs" } else { "BackendNotFound" },
                            "message": refs_message,
                            "lastTransitionTime": now,
                            "observedGeneration": generation,
                        },
                        {
                            "type": "Programmed",
                            "status": if programmed { "True" } else { "False" },
                            "reason": if programmed { "Programmed" } else { "Invalid" },
                            "message": if programmed { "Route programmed into data plane" } else { "Route not yet programmed" },
                            "lastTransitionTime": now,
                            "observedGeneration": generation,
                        },
                    ],
                }
            ],
        }
    });

    match api
        .patch_status(route_name, &PatchParams::apply("portail").force(), &Patch::Apply(status))
        .await
    {
        Ok(_) => debug!("Updated {} {}/{} status", std::any::type_name::<K>().rsplit("::").next().unwrap_or("Route"), route_namespace, route_name),
        Err(e) => warn!("Failed to update route {}/{} status: {}", route_namespace, route_name, e),
    }
}
