//! Portail's own policy CRDs (GEP-713 direct policy attachment).
//!
//! Gateway API defines no rate-limit filter, so the limit rides portail's
//! API group as a policy attached to a Gateway (optionally narrowed to one
//! listener via `sectionName`). The Rust types here are the single source
//! of truth: the checked-in CRD manifest is generated from them by
//! `portail --print-crd` and a test asserts the two never drift.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Per-client-IP token-bucket rate limit for one Gateway or one listener.
///
/// Clients are keyed by source address (IPv4) or /64 prefix (IPv6). The
/// bucket holds `burst` requests and refills at `requestsPerSecond`; a
/// request finding the bucket empty is answered 429 without touching any
/// backend. Policy edits keep existing per-client state: a throttled
/// crawler stays throttled across a rate change.
#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "portail.epheo.eu",
    version = "v1alpha1",
    kind = "RateLimitPolicy",
    namespaced,
    status = "RateLimitPolicyStatus",
    shortname = "rlp",
    printcolumn = r#"{"name":"Target","type":"string","jsonPath":".spec.targetRef.name"}"#,
    printcolumn = r#"{"name":"Listener","type":"string","jsonPath":".spec.targetRef.sectionName"}"#,
    printcolumn = r#"{"name":"Req/s","type":"integer","jsonPath":".spec.requestsPerSecond"}"#,
    printcolumn = r#"{"name":"Burst","type":"integer","jsonPath":".spec.burst"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitPolicySpec {
    /// The Gateway this policy attaches to. Same-namespace only, per
    /// GEP-713 direct attachment.
    pub target_ref: PolicyTargetRef,

    /// Sustained per-client budget. Bucket refill rate.
    #[schemars(range(min = 1, max = 1_000_000))]
    pub requests_per_second: u32,

    /// Bucket capacity: how many requests a client may spend at once before
    /// the sustained rate applies. Sized to absorb a human page load (a page
    /// plus its assets), not a crawl. Defaults to `requestsPerSecond`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 4_000_000))]
    pub burst: Option<u32>,
}

/// LocalPolicyTargetReferenceWithSectionName (GEP-713 shape).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PolicyTargetRef {
    /// Only "gateway.networking.k8s.io" is meaningful today.
    pub group: String,
    /// Only "Gateway" is meaningful today.
    pub kind: String,
    pub name: String,
    /// Listener name to narrow the policy to. Absent = whole Gateway.
    /// A listener-scoped policy beats a Gateway-wide one for its listener.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section_name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitPolicyStatus {
    /// GEP-713 ancestor status: one entry per Gateway this controller
    /// evaluated the policy against.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ancestors: Vec<PolicyAncestorStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PolicyAncestorStatus {
    pub ancestor_ref: PolicyAncestorRef,
    pub controller_name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<PolicyCondition>,
}

/// ParentReference shape, reduced to what a Gateway ancestor needs.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PolicyAncestorRef {
    pub group: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
}

/// metav1.Condition shape (typed rather than borrowed from k8s-openapi,
/// whose types don't derive JsonSchema under our feature set).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PolicyCondition {
    #[serde(rename = "type")]
    pub type_: String,
    /// "True" | "False" | "Unknown"
    pub status: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    pub last_transition_time: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
}

/// All portail CRD manifests as one YAML document stream, for
/// `portail --print-crd` and the checked-in-manifest drift test.
pub fn crd_yaml() -> anyhow::Result<String> {
    use kube::CustomResourceExt;
    Ok(serde_yaml::to_string(&RateLimitPolicy::crd())?)
}

impl RateLimitPolicy {
    /// GEP-713 direct attachment: the policy must live in the Gateway's
    /// namespace and name it explicitly.
    pub fn targets_gateway(&self, gw_ns: &str, gw_name: &str) -> bool {
        let t = &self.spec.target_ref;
        self.metadata.namespace.as_deref() == Some(gw_ns)
            && t.name == gw_name
            && t.kind == "Gateway"
            && (t.group.is_empty() || t.group == "gateway.networking.k8s.io")
    }

    pub fn effective_burst(&self) -> u32 {
        self.spec.burst.unwrap_or(self.spec.requests_per_second)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(ns: &str, yaml_spec: &str) -> RateLimitPolicy {
        let spec: RateLimitPolicySpec = serde_yaml::from_str(yaml_spec).unwrap();
        let mut p = RateLimitPolicy::new("p", spec);
        p.metadata.namespace = Some(ns.to_string());
        p
    }

    #[test]
    fn targeting_is_same_namespace_and_exact_name() {
        let p = policy(
            "edge",
            "targetRef: {group: gateway.networking.k8s.io, kind: Gateway, name: public}\nrequestsPerSecond: 2",
        );
        assert!(p.targets_gateway("edge", "public"));
        assert!(!p.targets_gateway("other", "public"));
        assert!(!p.targets_gateway("edge", "private"));
    }

    #[test]
    fn burst_defaults_to_rate() {
        let p = policy(
            "edge",
            "targetRef: {group: '', kind: Gateway, name: public}\nrequestsPerSecond: 7",
        );
        assert_eq!(p.effective_burst(), 7);
    }

    #[test]
    fn checked_in_crd_matches_generated() {
        let generated = crd_yaml().unwrap();
        let checked_in = include_str!("../../examples/kubernetes/crds/ratelimitpolicy.yaml");
        assert_eq!(
            checked_in, generated,
            "CRD manifest drifted from the Rust types; regenerate with: \
             cargo run -- --print-crd > examples/kubernetes/crds/ratelimitpolicy.yaml"
        );
    }
}
