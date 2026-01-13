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

pub fn supported_kinds_for_protocol(protocol: &str) -> Vec<serde_json::Value> {
    match protocol {
        "HTTP" => vec![serde_json::json!({"group": "gateway.networking.k8s.io", "kind": "HTTPRoute"})],
        "HTTPS" => vec![serde_json::json!({"group": "gateway.networking.k8s.io", "kind": "HTTPRoute"})],
        "TLS" => vec![serde_json::json!({"group": "gateway.networking.k8s.io", "kind": "TLSRoute"})],
        "TCP" => vec![serde_json::json!({"group": "gateway.networking.k8s.io", "kind": "TCPRoute"})],
        "UDP" => vec![serde_json::json!({"group": "gateway.networking.k8s.io", "kind": "UDPRoute"})],
        _ => vec![],
    }
}

/// Per-listener validation status computed during reconciliation.
#[derive(Clone, Debug)]
pub struct ListenerStatus {
    pub accepted: bool,
    pub accepted_reason: String,
    pub accepted_message: String,
    pub programmed: bool,
    pub programmed_reason: String,
    pub programmed_message: String,
    pub resolved_refs: bool,
    pub resolved_refs_reason: String,
    pub resolved_refs_message: String,
    pub conflicted: bool,
    pub conflicted_reason: String,
    pub conflicted_message: String,
    /// The supportedKinds list for this listener (computed from allowedRoutes.kinds)
    pub supported_kinds: Vec<serde_json::Value>,
}

impl Default for ListenerStatus {
    fn default() -> Self {
        Self {
            accepted: false,
            accepted_reason: "Pending".into(),
            accepted_message: "Listener validation pending".into(),
            programmed: false,
            programmed_reason: "Pending".into(),
            programmed_message: "Listener programming pending".into(),
            resolved_refs: false,
            resolved_refs_reason: "Pending".into(),
            resolved_refs_message: "Reference resolution pending".into(),
            conflicted: false,
            conflicted_reason: "NoConflicts".into(),
            conflicted_message: "No conflicts detected".into(),
            supported_kinds: vec![],
        }
    }
}

pub async fn update_gateway_status(
    client: &Client,
    gateway: &Gateway,
    accepted: bool,
    accepted_reason: &str,
    accepted_message: &str,
    programmed: bool,
    programmed_message: &str,
    listener_route_counts: &HashMap<String, i32>,
    listener_statuses: &HashMap<String, ListenerStatus>,
) {
    let name = gateway.name_any();
    let ns = gateway.namespace().unwrap_or_else(|| "default".to_string());
    let api: Api<Gateway> = Api::namespaced(client.clone(), &ns);

    let now = k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(Utc::now());
    let generation = gateway.metadata.generation;

    let conditions = vec![
        Condition {
            type_: "Accepted".to_string(),
            status: if accepted { "True" } else { "False" }.to_string(),
            reason: if accepted { "Accepted" } else { accepted_reason }.to_string(),
            message: accepted_message.to_string(),
            last_transition_time: now.clone(),
            observed_generation: generation,
        },
        Condition {
            type_: "Programmed".to_string(),
            status: if programmed { "True" } else { "False" }.to_string(),
            reason: if programmed { "Programmed" } else { "Invalid" }.to_string(),
            message: programmed_message.to_string(),
            last_transition_time: now.clone(),
            observed_generation: generation,
        },
    ];

    let listeners: Vec<serde_json::Value> = gateway
        .spec
        .listeners
        .iter()
        .map(|l| {
            let attached = listener_route_counts.get(&l.name).copied().unwrap_or(0);
            let ls = listener_statuses.get(&l.name).cloned().unwrap_or_default();

            serde_json::json!({
                "name": l.name,
                "attachedRoutes": attached,
                "supportedKinds": if ls.supported_kinds.is_empty() && ls.resolved_refs {
                    // Default: use protocol-based kinds if not overridden
                    supported_kinds_for_protocol(&l.protocol)
                } else {
                    ls.supported_kinds.clone()
                },
                "conditions": [
                    {
                        "type": "Accepted",
                        "status": if ls.accepted { "True" } else { "False" },
                        "reason": ls.accepted_reason,
                        "message": ls.accepted_message,
                        "lastTransitionTime": now,
                        "observedGeneration": generation,
                    },
                    {
                        "type": "Programmed",
                        "status": if ls.programmed { "True" } else { "False" },
                        "reason": ls.programmed_reason,
                        "message": ls.programmed_message,
                        "lastTransitionTime": now,
                        "observedGeneration": generation,
                    },
                    {
                        "type": "ResolvedRefs",
                        "status": if ls.resolved_refs { "True" } else { "False" },
                        "reason": ls.resolved_refs_reason,
                        "message": ls.resolved_refs_message,
                        "lastTransitionTime": now,
                        "observedGeneration": generation,
                    },
                    {
                        "type": "Conflicted",
                        "status": if ls.conflicted { "True" } else { "False" },
                        "reason": ls.conflicted_reason,
                        "message": ls.conflicted_message,
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
        "apiVersion": "gateway.networking.k8s.io/v1",
        "kind": "Gateway",
        "metadata": {
            "name": name,
            "namespace": ns,
        },
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

    // Declare supported features from the canonical list
    let supported_features: Vec<serde_json::Value> = super::features::SUPPORTED_FEATURES
        .iter()
        .map(|name| serde_json::json!({"name": name}))
        .collect();

    let status = serde_json::json!({
        "apiVersion": "gateway.networking.k8s.io/v1",
        "kind": "GatewayClass",
        "metadata": {
            "name": name,
        },
        "status": {
            "conditions": conditions,
            "supportedFeatures": supported_features,
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

/// Per-parent status entry for a route.
pub struct RouteParentStatus {
    pub controller_name: String,
    pub gateway_name: String,
    pub gateway_namespace: String,
    pub section_name: Option<String>,
    pub accepted: bool,
    pub accepted_reason: String,
    pub message: String,
    pub refs_resolved: bool,
    pub refs_reason: String,
    pub refs_message: String,
    pub programmed: bool,
    pub generation: Option<i64>,
}

/// Update the status of a route resource with all parent statuses in a single patch.
/// This prevents SSA from overwriting previous parent entries when a route
/// references multiple listeners or gateways.
pub async fn update_route_status<K>(
    client: &Client,
    route_name: &str,
    route_namespace: &str,
    parents: &[RouteParentStatus],
    field_manager: &str,
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

    let parent_entries: Vec<serde_json::Value> = parents.iter().map(|p| {
        let mut parent_ref = serde_json::json!({
            "group": "gateway.networking.k8s.io",
            "kind": "Gateway",
            "name": p.gateway_name,
            "namespace": p.gateway_namespace,
        });
        if let Some(section) = &p.section_name {
            parent_ref["sectionName"] = serde_json::json!(section);
        }

        serde_json::json!({
            "controllerName": p.controller_name,
            "parentRef": parent_ref,
            "conditions": [
                {
                    "type": "Accepted",
                    "status": if p.accepted { "True" } else { "False" },
                    "reason": p.accepted_reason,
                    "message": p.message,
                    "lastTransitionTime": now,
                    "observedGeneration": p.generation,
                },
                {
                    "type": "ResolvedRefs",
                    "status": if p.refs_resolved { "True" } else { "False" },
                    "reason": p.refs_reason,
                    "message": p.refs_message,
                    "lastTransitionTime": now,
                    "observedGeneration": p.generation,
                },
                {
                    "type": "Programmed",
                    "status": if p.programmed { "True" } else { "False" },
                    "reason": if p.programmed { "Programmed" } else { "Invalid" },
                    "message": if p.programmed { "Route programmed into data plane" } else { "Route not yet programmed" },
                    "lastTransitionTime": now,
                    "observedGeneration": p.generation,
                },
            ],
        })
    }).collect();

    // Derive apiVersion and kind from the concrete K type
    let api_version = <K as kube::Resource>::api_version(&Default::default()).to_string();
    let kind = <K as kube::Resource>::kind(&Default::default()).to_string();

    let status = serde_json::json!({
        "apiVersion": api_version,
        "kind": kind,
        "metadata": {
            "name": route_name,
            "namespace": route_namespace,
        },
        "status": {
            "parents": parent_entries,
        }
    });

    match api
        .patch_status(route_name, &PatchParams::apply(field_manager).force(), &Patch::Apply(status))
        .await
    {
        Ok(_) => debug!("Updated {} {}/{} status ({} parents)", std::any::type_name::<K>().rsplit("::").next().unwrap_or("Route"), route_namespace, route_name, parents.len()),
        Err(e) => warn!("Failed to update route {}/{} status: {}", route_namespace, route_name, e),
    }
}
