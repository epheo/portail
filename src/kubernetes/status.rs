//! The status this controller owns: desired payloads built without
//! timestamps, compared with the observed status, written only when they
//! differ. Timestamps are added at write time.

use std::collections::HashMap;
use std::fmt::Debug;

use k8s_openapi::chrono::Utc;
use kube::api::{Api, Patch, PatchParams};
use kube::Client;
use kube::ResourceExt;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use gateway_api::gateways::Gateway;

use crate::kubernetes::addresses::{UsableAddresses, NETWORK_ADDRESS_TYPE};

use crate::logging::{debug, warn};

pub fn supported_kinds_for_protocol(protocol: &str) -> Vec<serde_json::Value> {
    let kind = match protocol {
        "HTTP" | "HTTPS" => "HTTPRoute",
        "TLS" => "TLSRoute",
        "TCP" => "TCPRoute",
        "UDP" => "UDPRoute",
        _ => return vec![],
    };
    vec![serde_json::json!({"group": "gateway.networking.k8s.io", "kind": kind})]
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

/// What one status write did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Write {
    /// The observed status already carried ours; nothing was sent.
    Current,
    Patched,
    Failed,
}

/// One condition without `lastTransitionTime`, so a desired status compares
/// with an observed one; [`stamp_conditions`] adds the time at write.
fn condition(
    cond_type: &str,
    ok: bool,
    reason: &str,
    message: &str,
    generation: Option<i64>,
) -> Value {
    let mut c = json!({
        "type": cond_type,
        "status": if ok { "True" } else { "False" },
        "reason": reason,
        "message": message,
    });
    if let Some(g) = generation {
        c["observedGeneration"] = json!(g);
    }
    c
}

fn str_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn now() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Give each condition in `entry["conditions"]` its lastTransitionTime: the
/// observed one while type, status and reason are unchanged, else `now`.
fn stamp_conditions(entry: &mut Value, observed: &[Value], now: &str) {
    let Some(conditions) = entry.get_mut("conditions").and_then(Value::as_array_mut) else {
        return;
    };
    for c in conditions {
        let time = transition_time_json(
            observed,
            str_at(c, "type"),
            str_at(c, "status"),
            str_at(c, "reason"),
            now,
        );
        c["lastTransitionTime"] = json!(time);
    }
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

/// `value` without any lastTransitionTime.
fn without_transition_times(value: &Value) -> Value {
    fn strip(v: &mut Value) {
        match v {
            Value::Object(map) => {
                map.remove("lastTransitionTime");
                map.values_mut().for_each(strip);
            }
            Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    let mut v = value.clone();
    strip(&mut v);
    v
}

/// One Gateway status condition, as computed by the reconciler.
/// Bundles the previously-loose `(bool, reason, message)` triple.
#[derive(Clone, Debug)]
pub(crate) struct GatewayCondition {
    pub ok: bool,
    pub reason: String,
    pub message: String,
}

impl GatewayCondition {
    pub fn ok(reason: &str, message: &str) -> Self {
        Self {
            ok: true,
            reason: reason.into(),
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            reason: "Invalid".into(),
            message: message.into(),
        }
    }
}

/// The Gateway status portail owns, without timestamps: the listeners
/// always; conditions and addresses only with `manage_conditions`, since
/// portail-operator owns them otherwise.
pub(crate) fn desired_gateway_status(
    gateway: &Gateway,
    accepted: &GatewayCondition,
    programmed: &GatewayCondition,
    listener_route_counts: &HashMap<String, i32>,
    listener_statuses: &HashMap<String, ListenerStatus>,
    usable: &UsableAddresses,
    manage_conditions: bool,
) -> Value {
    let generation = gateway.metadata.generation;
    let listeners: Vec<Value> = gateway
        .spec
        .listeners
        .iter()
        .map(|l| {
            let attached = listener_route_counts.get(&l.name).copied().unwrap_or(0);
            let ls = listener_statuses.get(&l.name).cloned().unwrap_or_default();
            json!({
                "name": l.name,
                "attachedRoutes": attached,
                "supportedKinds": if ls.supported_kinds.is_empty() && ls.resolved_refs {
                    supported_kinds_for_protocol(&l.protocol)
                } else {
                    ls.supported_kinds.clone()
                },
                "conditions": [
                    condition("Accepted", ls.accepted, &ls.accepted_reason, &ls.accepted_message, generation),
                    condition("Programmed", ls.programmed, &ls.programmed_reason, &ls.programmed_message, generation),
                    condition("ResolvedRefs", ls.resolved_refs, &ls.resolved_refs_reason, &ls.resolved_refs_message, generation),
                    condition("Conflicted", ls.conflicted, &ls.conflicted_reason, &ls.conflicted_message, generation),
                ],
            })
        })
        .collect();

    let mut status = json!({ "listeners": listeners });
    if manage_conditions {
        // Accepted=True may carry ListenersNotValid: some listeners rejected.
        status["conditions"] = json!([
            condition(
                "Accepted",
                accepted.ok,
                &accepted.reason,
                &accepted.message,
                generation
            ),
            condition(
                "Programmed",
                programmed.ok,
                &programmed.reason,
                &programmed.message,
                generation
            ),
        ]);
        status["addresses"] = json!(status_addresses(gateway, usable));
    }
    status
}

/// status.addresses from local address discovery: the spec's addresses this
/// pod has (Network entries as their resolved IP), else the pod's own IP.
fn status_addresses(gateway: &Gateway, usable: &UsableAddresses) -> Vec<Value> {
    let Some(spec_addrs) = gateway.spec.addresses.as_deref().filter(|a| !a.is_empty()) else {
        return vec![json!({
            "type": "IPAddress",
            "value": usable.preferred_ip(),
        })];
    };
    let known = usable.all();
    spec_addrs
        .iter()
        .filter_map(|a| {
            let addr_type = a.r#type.as_deref().unwrap_or("IPAddress");
            match addr_type {
                t if t == NETWORK_ADDRESS_TYPE => {
                    let name = a.value.as_deref().unwrap_or("");
                    let ip = usable.network_ips.get(name)?;
                    Some(json!({
                        "type": "IPAddress",
                        "value": ip,
                    }))
                }
                "IPAddress" => {
                    let v = a.value.as_deref().unwrap_or("");
                    (usable.is_empty() || known.contains(v)).then(|| {
                        json!({
                            "type": "IPAddress",
                            "value": v,
                        })
                    })
                }
                "Hostname" => a.value.as_ref().map(|v| {
                    json!({
                        "type": "Hostname",
                        "value": v,
                    })
                }),
                _ => None,
            }
        })
        .collect()
}

/// Whether the observed Gateway status already carries `desired`, timestamps
/// aside. Listeners match by name and conditions by type: both lists are
/// maps, so their order is not ours.
fn gateway_status_current(observed: &Value, desired: &Value) -> bool {
    let by_key = |list: Option<&Value>, key: &str| -> Vec<Value> {
        let mut items: Vec<Value> = list
            .and_then(Value::as_array)
            .map(|a| a.iter().map(without_transition_times).collect())
            .unwrap_or_default();
        items.sort_by(|a, b| str_at(a, key).cmp(str_at(b, key)));
        items
    };
    if by_key(observed.get("listeners"), "name") != by_key(desired.get("listeners"), "name") {
        return false;
    }
    if let Some(conditions) = desired.get("conditions").and_then(Value::as_array) {
        let observed_conditions = by_key(observed.get("conditions"), "type");
        if !conditions.iter().all(|c| observed_conditions.contains(c)) {
            return false;
        }
    }
    let empty = json!([]);
    match desired.get("addresses") {
        Some(addresses) => observed.get("addresses").unwrap_or(&empty) == addresses,
        None => true,
    }
}

/// Bring the Gateway status to `desired`, which must come from
/// [`desired_gateway_status`] for this Gateway. Server-side apply: portail's
/// field manager owns exactly what `desired` lists.
pub(crate) async fn write_gateway_status(
    client: &Client,
    gateway: &Gateway,
    mut desired: Value,
) -> Write {
    let observed = gateway
        .status
        .as_ref()
        .and_then(|s| serde_json::to_value(s).ok())
        .unwrap_or_else(|| json!({}));
    if gateway_status_current(&observed, &desired) {
        return Write::Current;
    }

    let now = now();
    let observed_conditions = observed["conditions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    stamp_conditions(&mut desired, &observed_conditions, &now);
    if let Some(listeners) = desired.get_mut("listeners").and_then(Value::as_array_mut) {
        for listener in listeners {
            let prior = observed["listeners"]
                .as_array()
                .and_then(|o| o.iter().find(|p| p["name"] == listener["name"]))
                .and_then(|p| p["conditions"].as_array())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            stamp_conditions(listener, prior, &now);
        }
    }

    let name = gateway.name_any();
    let ns = gateway.namespace().unwrap_or_else(|| "default".to_string());
    let body = json!({
        "apiVersion": "gateway.networking.k8s.io/v1",
        "kind": "Gateway",
        "metadata": {
            "name": name,
            "namespace": ns,
        },
        "status": desired,
    });
    let api: Api<Gateway> = Api::namespaced(client.clone(), &ns);
    match api
        .patch_status(
            &name,
            &PatchParams::apply("portail").force(),
            &Patch::Apply(body),
        )
        .await
    {
        Ok(_) => {
            debug!("Updated Gateway {}/{} status", ns, name);
            Write::Patched
        }
        Err(e) => {
            warn!("Failed to update Gateway {}/{} status: {}", ns, name, e);
            Write::Failed
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
    pub accepted_reason: &'static str,
    pub message: String,
    pub refs_resolved: bool,
    pub refs_reason: &'static str,
    pub refs_message: String,
    pub programmed: bool,
    pub generation: Option<i64>,
}

/// One route `status.parents` entry, without timestamps. The parentRef
/// mirrors the spec's ref: two refs may differ only by port.
pub(crate) fn route_parent_entry(p: &RouteParentStatus) -> Value {
    let mut parent_ref = json!({
        "group": "gateway.networking.k8s.io",
        "kind": "Gateway",
        "name": p.gateway_name,
        "namespace": p.gateway_namespace,
    });
    if let Some(section) = &p.section_name {
        parent_ref["sectionName"] = json!(section);
    }
    if let Some(port) = p.port {
        parent_ref["port"] = json!(port);
    }
    let (programmed_reason, programmed_message) = if p.programmed {
        ("Programmed", "Route programmed into data plane")
    } else {
        ("Invalid", "Route not yet programmed")
    };
    json!({
        "controllerName": p.controller_name,
        "parentRef": parent_ref,
        "conditions": [
            condition("Accepted", p.accepted, p.accepted_reason, &p.message, p.generation),
            condition("ResolvedRefs", p.refs_resolved, p.refs_reason, &p.refs_message, p.generation),
            condition("Programmed", p.programmed, programmed_reason, programmed_message, p.generation),
        ],
    })
}

/// One RateLimitPolicy `status.ancestors` entry (GEP-713), without
/// timestamps: the Gateway is the ancestor this controller evaluated.
pub(crate) fn policy_ancestor_entry(
    controller_name: &str,
    gw_name: &str,
    gw_ns: &str,
    accepted: bool,
    reason: &str,
    message: &str,
    generation: Option<i64>,
) -> Value {
    json!({
        "ancestorRef": {
            "group": "gateway.networking.k8s.io",
            "kind": "Gateway",
            "namespace": gw_ns,
            "name": gw_name,
        },
        "controllerName": controller_name,
        "conditions": [condition("Accepted", accepted, reason, message, generation)],
    })
}

/// The (controller, Gateway) pair whose entries a pass owns in an atomic
/// status list. Gateway API keys route parents and policy ancestors by
/// controllerName plus the Gateway ref.
pub(crate) struct Owner<'a> {
    pub controller: &'a str,
    pub gw_name: &'a str,
    pub gw_ns: &'a str,
}

impl Owner<'_> {
    /// `ref_field` is "parentRef" on routes and "ancestorRef" on policies.
    pub fn owns(&self, entry: &Value, ref_field: &str) -> bool {
        let gateway = &entry[ref_field];
        entry["controllerName"].as_str() == Some(self.controller)
            && gateway["name"].as_str() == Some(self.gw_name)
            && gateway["namespace"].as_str() == Some(self.gw_ns)
    }
}

/// Foreign entries stay verbatim; ours are replaced by `new`.
fn merge_owned(
    existing: &[Value],
    is_ours: impl Fn(&Value) -> bool,
    new: Vec<Value>,
) -> Vec<Value> {
    existing
        .iter()
        .filter(|e| !is_ours(e))
        .cloned()
        .chain(new)
        .collect()
}

/// Stamp each desired entry from our observed entry with the same ref.
fn stamp_owned(
    desired: Vec<Value>,
    observed: &[Value],
    is_ours: impl Fn(&Value) -> bool,
    ref_field: &str,
    now: &str,
) -> Vec<Value> {
    desired
        .into_iter()
        .map(|mut entry| {
            let prior = observed
                .iter()
                .filter(|e| is_ours(e))
                .find(|e| e[ref_field] == entry[ref_field])
                .and_then(|e| e["conditions"].as_array())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            stamp_conditions(&mut entry, prior, now);
            entry
        })
        .collect()
}

/// An object's atomic status list as the reflector last saw it.
pub(crate) struct ObservedEntries {
    pub resource_version: Option<String>,
    pub entries: Vec<Value>,
}

/// Whether our entries in `observed` equal `desired`, timestamps aside.
fn owned_entries_current(
    observed: &[Value],
    is_ours: impl Fn(&Value) -> bool,
    desired: &[Value],
) -> bool {
    let ours: Vec<Value> = observed
        .iter()
        .filter(|e| is_ours(e))
        .map(without_transition_times)
        .collect();
    ours == desired
}

/// Bring our entries in an atomic status list (route `parents`, policy
/// `ancestors`) to `desired`; an empty `desired` removes them.
///
/// The list is atomic, so a write carries every entry: read-modify-write
/// over the reflector's copy, with its resourceVersion as precondition. A
/// stale copy then fails with 409 instead of dropping a concurrent writer's
/// entry, and that writer's update re-triggers the pass. Gateway API
/// RouteStatus.parents: implementations "MUST perform a read-modify-write
/// cycle on this field before modifying it".
pub(crate) async fn write_owned_entries<K>(
    client: &Client,
    namespace: &str,
    name: &str,
    (list_field, ref_field): (&str, &str),
    owner: &Owner<'_>,
    observed: &ObservedEntries,
    desired: Vec<Value>,
    field_manager: &str,
) -> Write
where
    K: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope>
        + Clone
        + DeserializeOwned
        + Debug,
    <K as kube::Resource>::DynamicType: Default,
{
    let is_ours = |e: &Value| owner.owns(e, ref_field);
    if owned_entries_current(&observed.entries, is_ours, &desired) {
        return Write::Current;
    }
    let stamped = stamp_owned(desired, &observed.entries, is_ours, ref_field, &now());
    let mut status = serde_json::Map::new();
    status.insert(
        list_field.to_string(),
        Value::Array(merge_owned(&observed.entries, is_ours, stamped)),
    );
    let mut patch = json!({ "status": status });
    if let Some(rv) = &observed.resource_version {
        patch["metadata"] = json!({ "resourceVersion": rv });
    }

    let kind = K::kind(&Default::default()).to_string();
    let api: Api<K> = Api::namespaced(client.clone(), namespace);
    match api
        .patch_status(
            name,
            &PatchParams::apply(field_manager),
            &Patch::Merge(patch),
        )
        .await
    {
        Ok(_) => {
            debug!("Updated {} {}/{} status", kind, namespace, name);
            Write::Patched
        }
        Err(kube::Error::Api(e)) if e.code == 409 => {
            debug!(
                "{} {}/{} changed since read; its update re-runs this pass",
                kind, namespace, name
            );
            Write::Failed
        }
        Err(e) => {
            warn!(
                "Failed to update {} {}/{} status: {}",
                kind, namespace, name, e
            );
            Write::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind_of(protocol: &str) -> Option<String> {
        let kinds = supported_kinds_for_protocol(protocol);
        kinds
            .first()
            .and_then(|k| k.get("kind"))
            .and_then(|v| v.as_str())
            .map(String::from)
    }

    #[test]
    fn supported_kinds_map_protocols_to_route_kinds() {
        assert_eq!(kind_of("HTTP").as_deref(), Some("HTTPRoute"));
        assert_eq!(kind_of("HTTPS").as_deref(), Some("HTTPRoute"));
        assert_eq!(kind_of("TLS").as_deref(), Some("TLSRoute"));
        assert_eq!(kind_of("TCP").as_deref(), Some("TCPRoute"));
        assert_eq!(kind_of("UDP").as_deref(), Some("UDPRoute"));
        assert!(supported_kinds_for_protocol("SCTP").is_empty());
    }

    #[test]
    fn stamp_preserves_time_and_condition_sets_generation() {
        let existing = vec![json!({
            "type": "Accepted",
            "status": "True",
            "reason": "Accepted",
            "lastTransitionTime": "2020-01-01T00:00:00Z",
        })];

        let mut unchanged =
            json!({"conditions": [condition("Accepted", true, "Accepted", "ok", Some(7))]});
        stamp_conditions(&mut unchanged, &existing, "2026-07-18T00:00:00Z");
        let c = &unchanged["conditions"][0];
        assert_eq!(c["lastTransitionTime"], "2020-01-01T00:00:00Z");
        assert_eq!(c["status"], "True");
        assert_eq!(c["observedGeneration"], 7);

        // Same reason but flipped status is a real transition.
        let mut flipped =
            json!({"conditions": [condition("Accepted", false, "Accepted", "broken", Some(7))]});
        stamp_conditions(&mut flipped, &existing, "2026-07-18T00:00:00Z");
        let c = &flipped["conditions"][0];
        assert_eq!(c["lastTransitionTime"], "2026-07-18T00:00:00Z");
        assert_eq!(c["status"], "False");

        // No generation, no observedGeneration key: the typed observed form
        // omits it too.
        assert!(condition("Accepted", true, "Accepted", "ok", None)
            .get("observedGeneration")
            .is_none());
    }

    fn parent_entry(
        controller: &str,
        gateway: &str,
        section: Option<&str>,
        port: Option<i32>,
        cond_reason: &str,
    ) -> serde_json::Value {
        let mut pref = json!({
            "group": "gateway.networking.k8s.io",
            "kind": "Gateway",
            "name": gateway,
            "namespace": "default",
        });
        if let Some(s) = section {
            pref["sectionName"] = json!(s);
        }
        if let Some(p) = port {
            pref["port"] = json!(p);
        }
        json!({
            "controllerName": controller,
            "parentRef": pref,
            "conditions": [{
                "type": "Accepted",
                "status": "True",
                "reason": cond_reason,
                "lastTransitionTime": format!("2020-01-01T00:00:00Z{}", cond_reason),
            }],
        })
    }

    fn owner() -> Owner<'static> {
        Owner {
            controller: "portail",
            gw_name: "gw",
            gw_ns: "default",
        }
    }

    /// Timestamps carry over only between entries for the same full ref:
    /// two refs may differ only by port.
    #[test]
    fn stamping_matches_the_full_parent_ref() {
        let observed = vec![
            parent_entry("portail", "gw", Some("http"), None, "section-http"),
            parent_entry("portail", "gw", None, Some(8080), "port-8080"),
            parent_entry("portail", "gw", None, Some(9090), "port-9090"),
            parent_entry("other", "gw", None, None, "foreign"),
        ];
        let own = owner();
        let is_ours = |e: &Value| own.owns(e, "parentRef");
        let time_of = |section: Option<&str>, port: Option<i32>, reason: &str| {
            let mut desired = parent_entry("portail", "gw", section, port, reason);
            desired["conditions"][0]
                .as_object_mut()
                .unwrap()
                .remove("lastTransitionTime");
            let stamped = stamp_owned(vec![desired], &observed, is_ours, "parentRef", "now");
            stamped[0]["conditions"][0]["lastTransitionTime"].clone()
        };
        assert_eq!(
            time_of(None, Some(9090), "port-9090"),
            "2020-01-01T00:00:00Zport-9090"
        );
        assert_eq!(
            time_of(Some("http"), None, "section-http"),
            "2020-01-01T00:00:00Zsection-http"
        );
        // The bare ref only exists under another controller: not ours to reuse.
        assert_eq!(time_of(None, None, "foreign"), "now");
    }

    #[test]
    fn merge_keeps_foreign_entries_and_replaces_ours() {
        let existing = vec![
            parent_entry("other-controller", "gw", None, None, "foreign-controller"),
            parent_entry("portail", "other-gw", None, None, "foreign-gateway"),
            parent_entry("portail", "gw", Some("stale-section"), None, "stale"),
        ];
        let own = owner();
        let new_entries = vec![parent_entry("portail", "gw", Some("http"), None, "fresh")];

        let merged = merge_owned(&existing, |e| own.owns(e, "parentRef"), new_entries);

        let reasons: Vec<&str> = merged
            .iter()
            .map(|e| e["conditions"][0]["reason"].as_str().unwrap())
            .collect();
        // Foreign controller and foreign Gateway survive; our stale
        // sectionName entry is replaced wholesale by the fresh one.
        assert_eq!(
            reasons,
            vec!["foreign-controller", "foreign-gateway", "fresh"]
        );
    }

    #[test]
    fn policy_ancestors_use_the_same_ownership() {
        let ours = policy_ancestor_entry("portail", "gw", "default", true, "Accepted", "ok", None);
        let other_gw =
            policy_ancestor_entry("portail", "gw2", "default", true, "Accepted", "ok", None);
        let own = owner();
        assert!(own.owns(&ours, "ancestorRef"));
        assert!(!own.owns(&other_gw, "ancestorRef"));
        assert!(!own.owns(&ours, "parentRef"));
    }

    fn rps(generation: Option<i64>) -> RouteParentStatus {
        RouteParentStatus {
            controller_name: "portail".into(),
            gateway_name: "gw".into(),
            gateway_namespace: "default".into(),
            section_name: Some("http".into()),
            port: None,
            accepted: true,
            accepted_reason: "Accepted",
            message: "Accepted".into(),
            refs_resolved: true,
            refs_reason: "ResolvedRefs",
            refs_message: "All references resolved".into(),
            programmed: true,
            generation,
        }
    }

    /// What the apiserver hands back after we wrote `entries`, through the
    /// typed route status the reflector stores.
    fn stored_route_parents(entries: Vec<Value>) -> Vec<Value> {
        let typed: gateway_api::httproutes::HTTPRouteStatus =
            serde_json::from_value(json!({ "parents": entries })).unwrap();
        typed
            .parents
            .iter()
            .map(|p| serde_json::to_value(p).unwrap())
            .collect()
    }

    /// Written entries read back as current: no PATCH on the echo of our
    /// own write, and none on any later pass while nothing changes.
    #[test]
    fn written_route_parents_read_back_current() {
        let own = owner();
        let is_ours = |e: &Value| own.owns(e, "parentRef");
        let desired = vec![route_parent_entry(&rps(Some(3)))];
        let mut theirs = rps(Some(1));
        theirs.controller_name = "other".into();
        let observed = stamp_owned(
            vec![route_parent_entry(&theirs)],
            &[],
            |_| false,
            "parentRef",
            "2020-01-01T00:00:00Z",
        );

        assert!(!owned_entries_current(&observed, is_ours, &desired));
        let stamped = stamp_owned(desired.clone(), &observed, is_ours, "parentRef", &now());
        let written = merge_owned(&observed, is_ours, stamped);
        let stored = stored_route_parents(written);
        assert!(owned_entries_current(&stored, is_ours, &desired));

        // A spec bump moves observedGeneration: the entry is due again.
        let bumped = vec![route_parent_entry(&rps(Some(4)))];
        assert!(!owned_entries_current(&stored, is_ours, &bumped));
        // Nothing to say any more: our entry must go, the foreign one stays.
        assert!(!owned_entries_current(&stored, is_ours, &[]));
        let stripped = merge_owned(&stored, is_ours, vec![]);
        assert_eq!(stripped.len(), 1);
        assert_eq!(stripped[0]["controllerName"], "other");
    }

    #[test]
    fn written_policy_ancestors_read_back_current() {
        use crate::kubernetes::crds::RateLimitPolicyStatus;
        let own = owner();
        let is_ours = |e: &Value| own.owns(e, "ancestorRef");
        let desired = vec![policy_ancestor_entry(
            "portail",
            "gw",
            "default",
            false,
            "Conflicted",
            "older policy wins",
            Some(2),
        )];
        let stamped = stamp_owned(desired.clone(), &[], is_ours, "ancestorRef", &now());
        let typed: RateLimitPolicyStatus =
            serde_json::from_value(json!({ "ancestors": stamped })).unwrap();
        let stored: Vec<Value> = typed
            .ancestors
            .iter()
            .map(|a| serde_json::to_value(a).unwrap())
            .collect();
        assert!(owned_entries_current(&stored, is_ours, &desired));
    }

    fn gateway() -> Gateway {
        use gateway_api::gateways::{GatewayListeners, GatewaySpec};
        Gateway {
            metadata: kube::core::ObjectMeta {
                name: Some("gw".into()),
                namespace: Some("default".into()),
                generation: Some(5),
                ..Default::default()
            },
            spec: GatewaySpec {
                gateway_class_name: "portail".into(),
                listeners: ["http", "admin"]
                    .iter()
                    .map(|n| GatewayListeners {
                        name: n.to_string(),
                        port: 80,
                        protocol: "HTTP".into(),
                        hostname: None,
                        tls: None,
                        allowed_routes: None,
                    })
                    .collect(),
                ..Default::default()
            },
            status: None,
        }
    }

    fn desired_for(gw: &Gateway, manage: bool) -> Value {
        let statuses: HashMap<String, ListenerStatus> = gw
            .spec
            .listeners
            .iter()
            .map(|l| {
                let ls = ListenerStatus {
                    accepted: true,
                    resolved_refs: true,
                    programmed: true,
                    ..Default::default()
                };
                (l.name.clone(), ls)
            })
            .collect();
        let usable = UsableAddresses {
            interface_ips: vec!["10.0.0.1".into()],
            network_ips: HashMap::new(),
        };
        desired_gateway_status(
            gw,
            &GatewayCondition::ok("Accepted", "ok"),
            &GatewayCondition::ok("Programmed", "ok"),
            &HashMap::new(),
            &statuses,
            &usable,
            manage,
        )
    }

    /// Stamp `desired` as a write would and read it back through the typed
    /// Gateway status, listeners in reverse to show order does not matter.
    fn stored_gateway_status(desired: &Value) -> Value {
        let mut written = desired.clone();
        let now = now();
        stamp_conditions(&mut written, &[], &now);
        for l in written["listeners"].as_array_mut().unwrap() {
            stamp_conditions(l, &[], &now);
        }
        written["listeners"].as_array_mut().unwrap().reverse();
        let typed: gateway_api::gateways::GatewayStatus = serde_json::from_value(written).unwrap();
        serde_json::to_value(typed).unwrap()
    }

    #[test]
    fn written_gateway_status_reads_back_current() {
        let gw = gateway();
        for manage in [true, false] {
            let desired = desired_for(&gw, manage);
            assert!(!gateway_status_current(&json!({}), &desired));
            let stored = stored_gateway_status(&desired);
            assert!(gateway_status_current(&stored, &desired), "manage={manage}");
        }
    }

    /// Generation, a wiped status and a foreign condition type each decide
    /// on their own whether a write is due.
    #[test]
    fn gateway_status_diff_sees_generation_and_wipes() {
        let mut gw = gateway();
        let stored = stored_gateway_status(&desired_for(&gw, true));

        gw.metadata.generation = Some(6);
        assert!(!gateway_status_current(&stored, &desired_for(&gw, true)));
        assert!(!gateway_status_current(&stored, &desired_for(&gw, false)));

        gw.metadata.generation = Some(5);
        let desired = desired_for(&gw, true);
        let mut wiped = stored.clone();
        wiped.as_object_mut().unwrap().remove("listeners");
        assert!(!gateway_status_current(&wiped, &desired));

        // Another writer's condition type does not make ours stale.
        let mut with_foreign = stored.clone();
        with_foreign["conditions"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "type": "Ready",
                "status": "True",
                "reason": "Ready",
                "message": "",
                "lastTransitionTime": "2020-01-01T00:00:00Z",
            }));
        assert!(gateway_status_current(&with_foreign, &desired));
    }
}
