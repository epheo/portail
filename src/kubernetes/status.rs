use std::collections::HashMap;
use std::fmt::Debug;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use k8s_openapi::chrono::Utc;
use kube::api::{Api, Patch, PatchParams};
use kube::Client;
use kube::ResourceExt;
use serde::de::DeserializeOwned;
use serde::Serialize;

use gateway_api::gateways::Gateway;

use crate::kubernetes::addresses::{UsableAddresses, NETWORK_ADDRESS_TYPE};

use crate::logging::{debug, warn};

pub fn supported_kinds_for_protocol(protocol: &str) -> Vec<serde_json::Value> {
    match protocol {
        "HTTP" => {
            vec![serde_json::json!({"group": "gateway.networking.k8s.io", "kind": "HTTPRoute"})]
        }
        "HTTPS" => {
            vec![serde_json::json!({"group": "gateway.networking.k8s.io", "kind": "HTTPRoute"})]
        }
        "TLS" => {
            vec![serde_json::json!({"group": "gateway.networking.k8s.io", "kind": "TLSRoute"})]
        }
        "TCP" => {
            vec![serde_json::json!({"group": "gateway.networking.k8s.io", "kind": "TCPRoute"})]
        }
        "UDP" => {
            vec![serde_json::json!({"group": "gateway.networking.k8s.io", "kind": "UDPRoute"})]
        }
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

/// Look up the lastTransitionTime for a condition from existing conditions.
/// Returns the existing time if the condition status+reason haven't changed,
/// otherwise returns `now` (a real transition occurred).
fn transition_time(
    existing: &[Condition],
    type_: &str,
    new_status: &str,
    new_reason: &str,
    now: &k8s_openapi::apimachinery::pkg::apis::meta::v1::Time,
) -> k8s_openapi::apimachinery::pkg::apis::meta::v1::Time {
    for c in existing {
        if c.type_ == type_ && c.status == new_status && c.reason == new_reason {
            return c.last_transition_time.clone();
        }
    }
    now.clone()
}

/// Look up a condition's lastTransitionTime from a JSON array of conditions.
/// Build one JSON status condition, preserving `lastTransitionTime` from
/// `existing` when (status, reason) are unchanged.
fn condition_json(
    cond_type: &str,
    ok: bool,
    reason: &str,
    message: &str,
    existing: &[serde_json::Value],
    now: &str,
    generation: Option<i64>,
) -> serde_json::Value {
    let status = if ok { "True" } else { "False" };
    serde_json::json!({
        "type": cond_type,
        "status": status,
        "reason": reason,
        "message": message,
        "lastTransitionTime": transition_time_json(existing, cond_type, status, reason, now),
        "observedGeneration": generation,
    })
}

fn transition_time_json(
    existing: &[serde_json::Value],
    type_: &str,
    new_status: &str,
    new_reason: &str,
    now: &str,
) -> String {
    for c in existing {
        if c.get("type").and_then(|v| v.as_str()) == Some(type_)
            && c.get("status").and_then(|v| v.as_str()) == Some(new_status)
            && c.get("reason").and_then(|v| v.as_str()) == Some(new_reason)
        {
            if let Some(t) = c.get("lastTransitionTime").and_then(|v| v.as_str()) {
                return t.to_string();
            }
        }
    }
    now.to_string()
}

/// One Gateway status condition, as computed by the reconciler.
/// Bundles the previously-loose `(bool, reason, message)` triple.
#[derive(Clone, Debug)]
pub(crate) struct GatewayCondition {
    pub ok: bool,
    pub reason: String,
    pub message: String,
}

pub(crate) async fn update_gateway_status(
    client: &Client,
    gateway: &Gateway,
    accepted: &GatewayCondition,
    programmed: &GatewayCondition,
    listener_route_counts: &HashMap<String, i32>,
    listener_statuses: &HashMap<String, ListenerStatus>,
    usable: &UsableAddresses,
    manage_conditions: bool,
) -> bool {
    let name = gateway.name_any();
    let ns = gateway.namespace().unwrap_or_else(|| "default".to_string());
    let api: Api<Gateway> = Api::namespaced(client.clone(), &ns);

    let now = k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(Utc::now());
    let generation = gateway.metadata.generation;

    // Read existing conditions to preserve lastTransitionTime when status hasn't changed
    let existing_conditions: Vec<Condition> = gateway
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .cloned()
        .unwrap_or_default();

    let accepted_status = if accepted.ok { "True" } else { "False" };
    let accepted_reason_str = if accepted.ok {
        "Accepted"
    } else {
        accepted.reason.as_str()
    };
    let programmed_status = if programmed.ok { "True" } else { "False" };
    let programmed_reason_str = if programmed.ok {
        "Programmed"
    } else {
        programmed.reason.as_str()
    };

    let conditions = vec![
        Condition {
            type_: "Accepted".to_string(),
            status: accepted_status.to_string(),
            reason: accepted_reason_str.to_string(),
            message: accepted.message.clone(),
            last_transition_time: transition_time(
                &existing_conditions,
                "Accepted",
                accepted_status,
                accepted_reason_str,
                &now,
            ),
            observed_generation: generation,
        },
        Condition {
            type_: "Programmed".to_string(),
            status: programmed_status.to_string(),
            reason: programmed_reason_str.to_string(),
            message: programmed.message.clone(),
            last_transition_time: transition_time(
                &existing_conditions,
                "Programmed",
                programmed_status,
                programmed_reason_str,
                &now,
            ),
            observed_generation: generation,
        },
    ];

    // Read existing listener conditions to preserve their timestamps too
    let existing_listener_conditions: HashMap<String, Vec<serde_json::Value>> = gateway
        .status
        .as_ref()
        .and_then(|s| s.listeners.as_ref())
        .map(|listeners| {
            // Serialize the typed struct to JSON to access fields dynamically
            let listeners_json = serde_json::to_value(listeners).unwrap_or_default();
            listeners_json
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|l| {
                            let name = l.get("name")?.as_str()?.to_string();
                            let conds = l.get("conditions")?.as_array()?.clone();
                            Some((name, conds))
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
        .unwrap_or_default();

    let now_str = now.0.format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let listeners: Vec<serde_json::Value> = gateway
        .spec
        .listeners
        .iter()
        .map(|l| {
            let attached = listener_route_counts.get(&l.name).copied().unwrap_or(0);
            let ls = listener_statuses.get(&l.name).cloned().unwrap_or_default();

            let existing_lconds = existing_listener_conditions.get(&l.name)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);

            serde_json::json!({
                "name": l.name,
                "attachedRoutes": attached,
                "supportedKinds": if ls.supported_kinds.is_empty() && ls.resolved_refs {
                    supported_kinds_for_protocol(&l.protocol)
                } else {
                    ls.supported_kinds.clone()
                },
                "conditions": [
                    condition_json("Accepted", ls.accepted, &ls.accepted_reason, &ls.accepted_message, existing_lconds, &now_str, generation),
                    condition_json("Programmed", ls.programmed, &ls.programmed_reason, &ls.programmed_message, existing_lconds, &now_str, generation),
                    condition_json("ResolvedRefs", ls.resolved_refs, &ls.resolved_refs_reason, &ls.resolved_refs_message, existing_lconds, &now_str, generation),
                    condition_json("Conflicted", ls.conflicted, &ls.conflicted_reason, &ls.conflicted_message, existing_lconds, &now_str, generation),
                ],
            })
        })
        .collect();

    // Build status.addresses using pre-computed address discovery.
    // LB VIPs are preferred (externally reachable), then local interface IPs.
    let addresses: Vec<serde_json::Value> = if let Some(spec_addrs) = &gateway.spec.addresses {
        if !spec_addrs.is_empty() {
            {
                let known = usable.all();
                spec_addrs
                    .iter()
                    .filter_map(|a| {
                        let addr_type = a.r#type.as_deref().unwrap_or("IPAddress");
                        match addr_type {
                            t if t == NETWORK_ADDRESS_TYPE => {
                                // Resolve network name to IPAddress for status
                                let name = a.value.as_deref().unwrap_or("");
                                let ip = usable.network_ips.get(name)?;
                                Some(serde_json::json!({
                                    "type": "IPAddress",
                                    "value": ip,
                                }))
                            }
                            "IPAddress" => {
                                let v = a.value.as_deref().unwrap_or("");
                                if usable.is_empty() || known.contains(v) {
                                    Some(serde_json::json!({
                                        "type": "IPAddress",
                                        "value": v,
                                    }))
                                } else {
                                    None
                                }
                            }
                            "Hostname" => Some(serde_json::json!({
                                "type": "Hostname",
                                "value": a.value,
                            })),
                            _ => None,
                        }
                    })
                    .collect()
            }
        } else {
            vec![serde_json::json!({
                "type": "IPAddress",
                "value": usable.preferred_ip(),
            })]
        }
    } else {
        vec![serde_json::json!({
            "type": "IPAddress",
            "value": usable.preferred_ip(),
        })]
    };

    // portail always owns per-listener status. When the operator manages
    // lifecycle status, it owns conditions + addresses (written under a
    // separate field manager); portail omits them so SSA ownership stays
    // disjoint and the two writers don't clobber each other.
    let mut status_inner = serde_json::json!({ "listeners": listeners });
    if manage_conditions {
        status_inner["conditions"] = serde_json::json!(conditions);
        status_inner["addresses"] = serde_json::json!(addresses);
    }
    let status = serde_json::json!({
        "apiVersion": "gateway.networking.k8s.io/v1",
        "kind": "Gateway",
        "metadata": {
            "name": name,
            "namespace": ns,
        },
        "status": status_inner,
    });

    match api
        .patch_status(
            &name,
            &PatchParams::apply("portail").force(),
            &Patch::Apply(status),
        )
        .await
    {
        Ok(_) => {
            debug!("Updated Gateway {}/{} status", ns, name);
            true
        }
        Err(e) => {
            warn!("Failed to update Gateway {}/{} status: {}", ns, name, e);
            false
        }
    }
}

/// Per-parent status entry for a route.
pub struct RouteParentStatus {
    pub controller_name: String,
    pub gateway_name: String,
    pub gateway_namespace: String,
    pub section_name: Option<String>,
    pub port: Option<i32>,
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
///
/// `status.parents` is `listType=atomic` in the Gateway API CRDs, so a
/// server-side apply containing only our entries would wipe every entry
/// written by anyone else — with per-Gateway data planes (one portail process
/// per Gateway) two pods sharing a route would flip-flop each other's status
/// forever. Instead this composes the full list read-modify-write style:
/// entries belonging to other controllers or other Gateways are preserved
/// verbatim from the observed status, our entries (this controller + this
/// Gateway) are replaced wholesale, and the result is written with a merge
/// patch. The reconcile fingerprint includes the observed parents, so a
/// concurrent writer clobbering our entry is detected and re-applied.
pub async fn update_route_status<K>(
    client: &Client,
    route_name: &str,
    route_namespace: &str,
    parents: &[RouteParentStatus],
    field_manager: &str,
    existing_parents_json: &[serde_json::Value],
) -> bool
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
        if let Some(port) = p.port {
            parent_ref["port"] = serde_json::json!(port);
        }

        // Find existing conditions for this parent to preserve timestamps
        let existing_conds = find_existing_parent_conditions(
            existing_parents_json,
            &p.gateway_name,
            &p.gateway_namespace,
            p.section_name.as_deref(),
            p.port,
        );

        let programmed_reason = if p.programmed { "Programmed" } else { "Invalid" };
        let programmed_message = if p.programmed {
            "Route programmed into data plane"
        } else {
            "Route not yet programmed"
        };

        serde_json::json!({
            "controllerName": p.controller_name,
            "parentRef": parent_ref,
            "conditions": [
                condition_json("Accepted", p.accepted, &p.accepted_reason, &p.message, &existing_conds, &now, p.generation),
                condition_json("ResolvedRefs", p.refs_resolved, &p.refs_reason, &p.refs_message, &existing_conds, &now, p.generation),
                condition_json("Programmed", p.programmed, programmed_reason, programmed_message, &existing_conds, &now, p.generation),
            ],
        })
    }).collect();

    // Preserve foreign entries: anything not written by this controller for
    // this Gateway. Our own stale entries (e.g. a sectionName the route no
    // longer attaches to) are dropped and replaced by `parent_entries`.
    let ours = |entry: &serde_json::Value| -> bool {
        let controller = entry.get("controllerName").and_then(|v| v.as_str());
        let pref = entry.get("parentRef");
        let name = pref.and_then(|p| p.get("name")).and_then(|v| v.as_str());
        let ns = pref
            .and_then(|p| p.get("namespace"))
            .and_then(|v| v.as_str());
        parents.iter().any(|p| {
            controller == Some(p.controller_name.as_str())
                && name == Some(p.gateway_name.as_str())
                && ns == Some(p.gateway_namespace.as_str())
        })
    };
    let merged_parents: Vec<serde_json::Value> = existing_parents_json
        .iter()
        .filter(|e| !ours(e))
        .cloned()
        .chain(parent_entries)
        .collect();

    let status = serde_json::json!({
        "status": {
            "parents": merged_parents,
        }
    });

    match api
        .patch_status(
            route_name,
            &PatchParams::apply(field_manager),
            &Patch::Merge(status),
        )
        .await
    {
        Ok(_) => {
            debug!(
                "Updated {} {}/{} status ({} parents)",
                std::any::type_name::<K>()
                    .rsplit("::")
                    .next()
                    .unwrap_or("Route"),
                route_namespace,
                route_name,
                parents.len()
            );
            true
        }
        Err(e) => {
            warn!(
                "Failed to update route {}/{} status: {}",
                route_namespace, route_name, e
            );
            false
        }
    }
}

/// Find existing conditions for a specific parent entry in the route status.
/// Matches the full parentRef identity — two refs may differ only by port.
fn find_existing_parent_conditions(
    existing_parents: &[serde_json::Value],
    gateway_name: &str,
    gateway_namespace: &str,
    section_name: Option<&str>,
    port: Option<i32>,
) -> Vec<serde_json::Value> {
    for parent in existing_parents {
        let pref = match parent.get("parentRef") {
            Some(p) => p,
            None => continue,
        };
        let name_match = pref.get("name").and_then(|v| v.as_str()) == Some(gateway_name);
        let ns_match = pref.get("namespace").and_then(|v| v.as_str()) == Some(gateway_namespace);
        let section_match = match section_name {
            Some(sn) => pref.get("sectionName").and_then(|v| v.as_str()) == Some(sn),
            None => pref.get("sectionName").is_none(),
        };
        let port_match = match port {
            Some(p) => pref.get("port").and_then(|v| v.as_i64()) == Some(p as i64),
            None => pref.get("port").is_none(),
        };
        if name_match && ns_match && section_match && port_match {
            return parent
                .get("conditions")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
        }
    }
    vec![]
}
