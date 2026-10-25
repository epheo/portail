//! Portail's own CRDs: RateLimitPolicy (GEP-713 direct policy attachment)
//! and GatewayParameters (the parametersRef target of a GatewayClass or a
//! Gateway's infrastructure).
//!
//! Gateway API defines no rate-limit filter, so the limit rides portail's
//! API group as a policy attached to a Gateway (optionally narrowed to one
//! listener via `sectionName`). The Rust types here are the single source
//! of truth: the checked-in CRD manifests under deploy/crds/ are generated
//! from them by `portail --print-crd` and a test asserts they never drift.

use k8s_openapi::api::core::v1::ResourceRequirements;
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

/// How a Gateway's data plane is run. Built-in defaults, then the
/// GatewayClass's parameters, then the Gateway's: a set field wins field by
/// field; `networks` and `resources` replace whole.
#[derive(CustomResource, Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "portail.epheo.eu",
    version = "v1alpha1",
    kind = "GatewayParameters",
    // The derive would say "gatewayparameterses".
    plural = "gatewayparameters",
    namespaced,
    shortname = "gwp",
    printcolumn = r#"{"name":"Provisioning","type":"string","jsonPath":".spec.provisioning"}"#,
    printcolumn = r#"{"name":"Replicas","type":"integer","jsonPath":".spec.replicas"}"#,
    printcolumn = r#"{"name":"Service","type":"string","jsonPath":".spec.service.type"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct GatewayParametersSpec {
    /// Managed: the controller creates the Gateway's ServiceAccount,
    /// Deployment, Service and PodDisruptionBudget. Unmanaged: it serves
    /// the configuration and writes status; the pods are the owner's.
    /// Default Managed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provisioning: Option<Provisioning>,

    /// Data-plane pods (Managed). Default 1; static `networks[].ips`
    /// require 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub replicas: Option<i32>,

    /// Data-plane image (Managed). Default: the controller's
    /// `--dataplane-image`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,

    /// Data-plane container resources (Managed), a core/v1
    /// ResourceRequirements; its worker threads follow the cpu limit.
    /// Default requests 100m/64Mi, limits 2/256Mi.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "resource_requirements_schema")]
    pub resources: Option<ResourceRequirements>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<ServiceParameters>,

    /// Multus attachments of the data-plane pods (Managed), as
    /// NetworkSelectionElements.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub networks: Option<Vec<NetworkSelection>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Provisioning {
    Managed,
    Unmanaged,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServiceParameters {
    /// The Service fronting the data plane; None creates none. Unset:
    /// None when the Gateway has a Network address or `networks`, else
    /// LoadBalancer.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub type_: Option<ServiceType>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ServiceType {
    LoadBalancer,
    ClusterIP,
    NodePort,
    None,
}

/// The Multus NetworkSelectionElement fields a data plane needs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NetworkSelection {
    /// The NetworkAttachmentDefinition.
    pub name: String,
    /// Its namespace; default the pod's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Static addresses in CIDR form, e.g. 172.16.87.21/24.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ips: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    /// The interface name in the pod.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
}

/// core/v1 ResourceRequirements as the apiserver's own CRDs describe it:
/// k8s-openapi types carry no JsonSchema. `claims` is left out, so it is
/// pruned: DRA claims do not apply to a data plane.
fn resource_requirements_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let quantities = schemars::json_schema!({
        "type": "object",
        "additionalProperties": {
            "anyOf": [{"type": "integer"}, {"type": "string"}],
            "pattern": r"^(\+|-)?(([0-9]+(\.[0-9]*)?)|(\.[0-9]+))(([KMGTPE]i)|[numkMGTPE]|([eE](\+|-)?(([0-9]+(\.[0-9]*)?)|(\.[0-9]+))))?$",
            "x-kubernetes-int-or-string": true,
        },
    });
    schemars::json_schema!({
        "type": "object",
        "nullable": true,
        "properties": {
            "limits": quantities.clone(),
            "requests": quantities,
        },
    })
}

/// A CRD's manifest as YAML.
fn crd_yaml_of<K: kube::CustomResourceExt>() -> anyhow::Result<String> {
    Ok(serde_yaml::to_string(&K::crd())?)
}

/// All portail CRD manifests as one YAML document stream, for
/// `portail --print-crd`. Each is checked in on its own under deploy/crds/.
pub fn crd_yaml() -> anyhow::Result<String> {
    Ok(format!(
        "{}---\n{}",
        crd_yaml_of::<GatewayParameters>()?,
        crd_yaml_of::<RateLimitPolicy>()?
    ))
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

    fn assert_checked_in<K: kube::CustomResourceExt>(checked_in: &str, file: &str) {
        assert_eq!(
            checked_in,
            crd_yaml_of::<K>().unwrap(),
            "{file} drifted from the Rust types; regenerate it from `portail --print-crd`, \
             one document per file"
        );
    }

    #[test]
    fn checked_in_crds_match_generated() {
        assert_checked_in::<RateLimitPolicy>(
            include_str!("../../deploy/crds/ratelimitpolicy.yaml"),
            "deploy/crds/ratelimitpolicy.yaml",
        );
        assert_checked_in::<GatewayParameters>(
            include_str!("../../deploy/crds/gatewayparameters.yaml"),
            "deploy/crds/gatewayparameters.yaml",
        );
    }

    #[test]
    fn print_crd_streams_both() {
        let stream = crd_yaml().unwrap();
        let kinds: Vec<String> = serde_yaml::Deserializer::from_str(&stream)
            .map(|doc| {
                let crd = serde_yaml::Value::deserialize(doc).unwrap();
                crd["spec"]["names"]["kind"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(kinds, ["GatewayParameters", "RateLimitPolicy"]);
    }

    /// Design 4.2's example, resources and all.
    #[test]
    fn gateway_parameters_parse() {
        let spec: GatewayParametersSpec = serde_yaml::from_str(
            "provisioning: Managed
replicas: 1
image: ghcr.io/epheo/portail:0.2.0
resources: {limits: {cpu: \"2\", memory: 256Mi}, requests: {cpu: 100m}}
service: {type: None}
networks:
- {name: br-ex-network, namespace: default, ips: [172.16.87.21/24], gateway: [172.16.87.1]}
",
        )
        .unwrap();
        assert_eq!(spec.provisioning, Some(Provisioning::Managed));
        assert_eq!(spec.service.unwrap().type_, Some(ServiceType::None));
        let limits = spec.resources.unwrap().limits.unwrap();
        assert_eq!(limits["memory"].0, "256Mi");
        assert_eq!(
            spec.networks.unwrap()[0].ips.as_deref().unwrap(),
            ["172.16.87.21/24"]
        );
    }
}
