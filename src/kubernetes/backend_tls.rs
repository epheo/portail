//! BackendTLSPolicy: which policy a Service port's backend connections
//! follow, and what each policy's conditions say. Pure: the caller reads
//! the stores, through the recording views, and writes the status.
//!
//! Gateway API v1.7. A policy names a Service, or one of its ports by name,
//! in its own namespace. Of the policies naming one Service and section, the
//! oldest holds it (creationTimestamp, then namespace and name), the others
//! are Accepted=False Conflicted. A port's own policy beats the Service's.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use gateway_api::backendtlspolicies::{
    BackendTlsPolicy, BackendTlsPolicyTargetRefs, BackendTlsPolicyValidation,
    BackendTlsPolicyValidationSubjectAltNamesType,
};
use k8s_openapi::api::core::v1::Service;
use kube::ResourceExt;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;

use super::converters::parse_backend_dns_name;
use super::reads::Recorded;
use crate::logging::warn;

/// The ConfigMap key a CA bundle is read from (Gateway API Core support).
pub(crate) const CA_KEY: &str = "ca.crt";

/// A ConfigMap a caCertificateRef names, as the controller found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CaRef {
    Missing,
    NoKey,
    /// Its `ca.crt`, unchecked.
    Pem(String),
}

/// A ConfigMap by namespace and name.
pub(crate) type ConfigMapKey = (String, String);
const SYSTEM: &str = "System";

/// How a Service port's backend connections are verified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Verify {
    /// The SNI sent, and the name verified unless `subject_alt_names` is set.
    pub hostname: String,
    /// When set, the names that count instead of `hostname`.
    pub subject_alt_names: Vec<San>,
    pub ca: Ca,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum San {
    Hostname(String),
    Uri(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Ca {
    /// PEM certificates, concatenated.
    Bundle(String),
    /// The data plane's well-known roots.
    System,
}

/// What the policy that applies to a port does to its connections.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Verify(Verify),
    /// The policy cannot be applied: connections must fail, never go
    /// unverified. The ref's share answers 5xx.
    Refuse,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Cond {
    pub ok: bool,
    pub reason: &'static str,
    pub message: String,
}

impl Cond {
    fn ok(reason: &'static str, message: &str) -> Self {
        Cond {
            ok: true,
            reason,
            message: message.into(),
        }
    }

    fn not(reason: &'static str, message: String) -> Self {
        Cond {
            ok: false,
            reason,
            message,
        }
    }
}

/// One policy's conditions, as each of its ancestors gets them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PolicyStatus {
    pub namespace: String,
    pub name: String,
    pub generation: Option<i64>,
    pub accepted: Cond,
    pub resolved_refs: Cond,
}

/// The policies naming one Service: each one's status, and, by port, the
/// decision for the ports asked about.
#[derive(Debug, Default)]
pub(crate) struct ServicePolicies {
    pub statuses: Vec<PolicyStatus>,
    pub by_port: BTreeMap<u16, Decision>,
}

/// The policies that name one Gateway's Service backends, each judged once,
/// and the decision for each backend (FQDN and port) a policy applies to.
#[derive(Debug, Default)]
pub(crate) struct GatewayPolicies {
    pub statuses: Vec<PolicyStatus>,
    pub decisions: HashMap<(String, u16), Decision>,
}

/// The Service backends of `referenced`, as FQDN and port, by Service.
fn by_service<'a>(
    referenced: impl IntoIterator<Item = &'a (String, u16)>,
) -> BTreeMap<(String, String), (String, BTreeSet<u16>)> {
    let mut out: BTreeMap<(String, String), (String, BTreeSet<u16>)> = BTreeMap::new();
    for (fqdn, port) in referenced {
        if let Some((name, ns)) = parse_backend_dns_name(fqdn) {
            out.entry((ns, name))
                .or_insert_with(|| (fqdn.clone(), BTreeSet::new()))
                .1
                .insert(*port);
        }
    }
    out
}

fn is_service(target: &BackendTlsPolicyTargetRefs, service: &str) -> bool {
    target.group.is_empty() && target.kind == "Service" && target.name == service
}

fn names_service(policy: &BackendTlsPolicy, service: &str) -> bool {
    policy
        .spec
        .target_refs
        .iter()
        .any(|t| is_service(t, service))
}

/// The ConfigMaps that the policies naming the Service backends of
/// `referenced` name, read as [`for_backends`] reads them.
pub(crate) fn wanted_configmaps<'a>(
    referenced: impl IntoIterator<Item = &'a (String, u16)>,
    services: &Recorded<Service>,
    policies: &Recorded<BackendTlsPolicy>,
) -> BTreeSet<ConfigMapKey> {
    let mut out = BTreeSet::new();
    for (ns, name) in by_service(referenced).into_keys() {
        if services.get(&ns, &name).is_none() {
            continue;
        }
        for policy in policies.in_namespace(&ns) {
            if !names_service(&policy, &name) {
                continue;
            }
            let refs = policy.spec.validation.ca_certificate_refs.iter().flatten();
            for r in refs.filter(|r| r.group.is_empty() && r.kind == "ConfigMap") {
                out.insert((ns.clone(), r.name.clone()));
            }
        }
    }
    out
}

/// For the Service backends `referenced` of one Gateway, as FQDN and port.
/// Each Service is read through `services`, the policies of its namespace
/// through `policies`; `configmap` looks a ConfigMap up by namespace and name.
pub(crate) fn for_backends<'a>(
    referenced: impl IntoIterator<Item = &'a (String, u16)>,
    services: &Recorded<Service>,
    policies: &Recorded<BackendTlsPolicy>,
    configmap: impl Fn(&str, &str) -> CaRef,
) -> GatewayPolicies {
    let mut out = GatewayPolicies::default();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for ((ns, name), (fqdn, ports)) in by_service(referenced) {
        let Some(service) = services.get(&ns, &name) else {
            continue;
        };
        let in_ns = policies.in_namespace(&ns);
        let found = for_service(&in_ns, &service, ports, |cm| configmap(&ns, cm));
        for (port, decision) in found.by_port {
            out.decisions.insert((fqdn.clone(), port), decision);
        }
        // A policy naming two Services of this Gateway is reported once.
        for status in found.statuses {
            if seen.insert((status.namespace.clone(), status.name.clone())) {
                out.statuses.push(status);
            }
        }
    }
    out.statuses
        .sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    out
}

/// Backends already told to connect with TLS unverified, so the warning is
/// once per process.
static UNVERIFIED: Mutex<BTreeSet<(String, u16)>> = Mutex::new(BTreeSet::new());

/// Warn, once per backend and process, of a Service port whose appProtocol
/// asks for TLS that no BackendTLSPolicy verifies: it connects unverified,
/// as before, until a policy names it.
pub(crate) fn warn_unverified<'a>(
    app_protocols: &HashMap<(String, u16), String>,
    decisions: &HashMap<(String, u16), Decision>,
    referenced: impl IntoIterator<Item = &'a (String, u16)>,
) {
    let mut warned = UNVERIFIED.lock().unwrap_or_else(PoisonError::into_inner);
    for backend in referenced {
        let https = app_protocols.get(backend).is_some_and(|p| p == "https");
        if https && !decisions.contains_key(backend) && warned.insert(backend.clone()) {
            warn!(
                "Backend {}:{} asks for TLS by appProtocol https and no BackendTLSPolicy \
                 verifies it: its certificate is not checked",
                backend.0, backend.1
            );
        }
    }
}

/// What a policy targets of a Service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Section {
    Whole,
    Port(u16),
}

/// The conflict order: the smallest holds.
fn order(policy: &BackendTlsPolicy) -> impl Ord {
    (
        policy.metadata.creation_timestamp.clone(),
        policy.namespace().unwrap_or_default(),
        policy.name_any(),
    )
}

/// `policies`, those of `service`'s namespace, judged for `service`; the
/// decisions for `ports`. `configmap` looks a ConfigMap up by name in the
/// Service's namespace (the policy's: its references are local).
pub(crate) fn for_service(
    policies: &[Arc<BackendTlsPolicy>],
    service: &Service,
    ports: impl IntoIterator<Item = u16>,
    configmap: impl Fn(&str) -> CaRef,
) -> ServicePolicies {
    let svc_name = service.name_any();
    let port_named = |name: &str| {
        service
            .spec
            .iter()
            .flat_map(|s| s.ports.iter().flatten())
            .find(|p| p.name.as_deref() == Some(name))
            .map(|p| p.port as u16)
    };
    let mut out = ServicePolicies::default();
    // Every section each targeting policy names, and those it names that
    // the Service does not have.
    let mut claims: HashMap<Section, Vec<&Arc<BackendTlsPolicy>>> = HashMap::new();
    let mut missing: HashMap<(String, String), String> = HashMap::new();
    let mut targeting: Vec<&Arc<BackendTlsPolicy>> = Vec::new();
    for policy in policies {
        let mut targets_it = false;
        for target in &policy.spec.target_refs {
            if !is_service(target, &svc_name) {
                continue;
            }
            targets_it = true;
            match target.section_name.as_deref() {
                None => claims.entry(Section::Whole).or_default().push(policy),
                Some(name) => match port_named(name) {
                    Some(port) => claims.entry(Section::Port(port)).or_default().push(policy),
                    None => {
                        missing.insert(
                            (policy.namespace().unwrap_or_default(), policy.name_any()),
                            format!("Service {svc_name} has no port named {name}"),
                        );
                    }
                },
            }
        }
        if targets_it {
            targeting.push(policy);
        }
    }
    let mut holders: HashMap<Section, &Arc<BackendTlsPolicy>> = HashMap::new();
    for (section, mut claimants) in claims {
        claimants.sort_by_key(|p| order(p));
        holders.insert(section, claimants[0]);
    }
    let held = |policy: &BackendTlsPolicy| {
        holders
            .values()
            .any(|h| h.namespace() == policy.namespace() && h.name_any() == policy.name_any())
    };
    let mut judged: HashMap<(String, String), Judged> = HashMap::new();
    for policy in targeting {
        let key = (policy.namespace().unwrap_or_default(), policy.name_any());
        let verdict = judge(&policy.spec.validation, &configmap);
        let accepted = if let Some(why) = missing.get(&key).filter(|_| !held(policy)) {
            Cond::not("TargetNotFound", why.clone())
        } else if !held(policy) {
            Cond::not(
                "Conflicted",
                format!("an older BackendTLSPolicy targets Service {svc_name} the same way"),
            )
        } else {
            verdict.accepted.clone()
        };
        out.statuses.push(PolicyStatus {
            namespace: key.0.clone(),
            name: key.1.clone(),
            generation: policy.metadata.generation,
            accepted,
            resolved_refs: verdict.resolved_refs.clone(),
        });
        judged.insert(key, verdict);
    }
    for port in ports {
        let holder = holders
            .get(&Section::Port(port))
            .or_else(|| holders.get(&Section::Whole));
        if let Some(holder) = holder {
            let key = (holder.namespace().unwrap_or_default(), holder.name_any());
            if let Some(verdict) = judged.get(&key) {
                out.by_port.insert(port, verdict.decision.clone());
            }
        }
    }
    out.statuses
        .sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    out
}

/// A policy on its own: its conditions, and what it does where it applies.
struct Judged {
    accepted: Cond,
    resolved_refs: Cond,
    decision: Decision,
}

fn judge(validation: &BackendTlsPolicyValidation, configmap: &impl Fn(&str) -> CaRef) -> Judged {
    let resolved = Cond::ok("ResolvedRefs", "All references resolved");
    let invalid = |message: String| Judged {
        accepted: Cond::not("Invalid", message),
        resolved_refs: resolved.clone(),
        decision: Decision::Refuse,
    };
    let refs = validation
        .ca_certificate_refs
        .as_deref()
        .unwrap_or_default();
    let well_known = validation
        .well_known_ca_certificates
        .as_deref()
        .filter(|w| !w.is_empty());
    let ca = match (refs.is_empty(), well_known) {
        (false, Some(_)) => {
            return invalid(
                "only one of caCertificateRefs and wellKnownCACertificates may be set".into(),
            )
        }
        (true, None) => {
            return invalid("one of caCertificateRefs or wellKnownCACertificates is needed".into())
        }
        (true, Some(SYSTEM)) => Ca::System,
        (true, Some(other)) => {
            return invalid(format!(
                "wellKnownCACertificates {other:?} is not supported"
            ))
        }
        (false, None) => {
            let mut bundle = String::new();
            let mut first_invalid: Option<Cond> = None;
            for r in refs {
                match ca_bundle(&r.group, &r.kind, &r.name, configmap) {
                    Ok(pem) => {
                        bundle.push_str(&pem);
                        if !pem.ends_with('\n') {
                            bundle.push('\n');
                        }
                    }
                    Err(why) => {
                        first_invalid.get_or_insert(why);
                    }
                }
            }
            match (bundle.is_empty(), first_invalid) {
                (true, Some(why)) => {
                    return Judged {
                        accepted: Cond::not(
                            "NoValidCACertificate",
                            format!("no caCertificateRef is valid: {}", why.message),
                        ),
                        resolved_refs: why,
                        decision: Decision::Refuse,
                    }
                }
                (_, why) => {
                    let decision = Decision::Verify(Verify {
                        hostname: validation.hostname.clone(),
                        subject_alt_names: sans(validation),
                        ca: Ca::Bundle(bundle),
                    });
                    return Judged {
                        accepted: Cond::ok("Accepted", "Policy accepted"),
                        resolved_refs: why.unwrap_or(resolved),
                        decision,
                    };
                }
            }
        }
    };
    Judged {
        accepted: Cond::ok("Accepted", "Policy accepted"),
        resolved_refs: resolved,
        decision: Decision::Verify(Verify {
            hostname: validation.hostname.clone(),
            subject_alt_names: sans(validation),
            ca,
        }),
    }
}

fn sans(validation: &BackendTlsPolicyValidation) -> Vec<San> {
    validation
        .subject_alt_names
        .iter()
        .flatten()
        .filter_map(|san| match san.r#type {
            BackendTlsPolicyValidationSubjectAltNamesType::Hostname => {
                san.hostname.clone().map(San::Hostname)
            }
            BackendTlsPolicyValidationSubjectAltNamesType::Uri => san.uri.clone().map(San::Uri),
        })
        .collect()
}

/// The PEM of one caCertificateRef, or why it is invalid. Each certificate
/// must be usable as a trust anchor: one the data plane would refuse is
/// refused here, where status can say so.
fn ca_bundle(
    group: &str,
    kind: &str,
    name: &str,
    configmap: &impl Fn(&str) -> CaRef,
) -> Result<String, Cond> {
    if !group.is_empty() || kind != "ConfigMap" {
        return Err(Cond::not(
            "InvalidKind",
            format!("caCertificateRef {name}: kind {kind:?} of group {group:?} is not supported, only ConfigMap"),
        ));
    }
    let invalid = |why: &str| {
        Cond::not(
            "InvalidCACertificateRef",
            format!("caCertificateRef ConfigMap {name}: {why}"),
        )
    };
    let pem = match configmap(name) {
        CaRef::Missing => return Err(invalid("not found")),
        CaRef::NoKey => return Err(invalid(&format!("no key {CA_KEY}"))),
        CaRef::Pem(pem) => pem,
    };
    let mut roots = rustls::RootCertStore::empty();
    let mut count = 0;
    for cert in CertificateDer::pem_slice_iter(pem.as_bytes()) {
        let Ok(cert) = cert else {
            return Err(invalid(&format!("{CA_KEY} is not valid PEM")));
        };
        if roots.add(cert).is_err() {
            return Err(invalid(&format!(
                "{CA_KEY} holds a certificate that is no CA"
            )));
        }
        count += 1;
    }
    if count == 0 {
        return Err(invalid(&format!("{CA_KEY} holds no certificate")));
    }
    Ok(pem)
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
    use serde_json::json;

    fn service() -> Service {
        serde_json::from_value(json!({
            "metadata": {"name": "web", "namespace": "apps"},
            "spec": {"ports": [{"name": "https-1", "port": 443}, {"name": "https-2", "port": 8443}]},
        }))
        .unwrap()
    }

    fn policy(
        name: &str,
        second: i64,
        section: Option<&str>,
        validation: serde_json::Value,
    ) -> Arc<BackendTlsPolicy> {
        let mut target = json!({"group": "", "kind": "Service", "name": "web"});
        if let Some(section) = section {
            target["sectionName"] = json!(section);
        }
        let mut p: BackendTlsPolicy = serde_json::from_value(json!({
            "metadata": {"name": name, "namespace": "apps", "generation": 1},
            "spec": {"targetRefs": [target], "validation": validation},
        }))
        .unwrap();
        p.metadata.creation_timestamp = Some(Time(
            k8s_openapi::jiff::Timestamp::from_second(second).unwrap(),
        ));
        Arc::new(p)
    }

    fn system(hostname: &str) -> serde_json::Value {
        json!({"hostname": hostname, "wellKnownCACertificates": "System"})
    }

    fn with_ca(cm: &str) -> serde_json::Value {
        json!({"hostname": "abc.example.com",
               "caCertificateRefs": [{"group": "", "kind": "ConfigMap", "name": cm}]})
    }

    /// A self-signed CA, as PEM.
    fn ca_pem() -> String {
        let params = rcgen::CertificateParams::new(vec!["ca.test".into()]).unwrap();
        let mut params = params;
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let key = rcgen::KeyPair::generate().unwrap();
        params.self_signed(&key).unwrap().pem()
    }

    fn status<'a>(out: &'a ServicePolicies, name: &str) -> &'a PolicyStatus {
        out.statuses.iter().find(|s| s.name == name).unwrap()
    }

    /// Of two policies on one Service and section, the older holds it and
    /// the other is Conflicted; a port's own policy and the Service's both
    /// hold, each where it applies.
    #[test]
    fn the_oldest_policy_holds_and_a_port_beats_the_service() {
        let policies = [
            policy("whole-2", 100, None, system("two.test")),
            policy("whole-1", 100, None, system("one.test")),
            policy("port", 200, Some("https-1"), system("port.test")),
        ];
        let out = for_service(&policies, &service(), [443, 8443], |_| CaRef::Missing);
        assert_eq!(status(&out, "whole-1").accepted.reason, "Accepted");
        assert_eq!(status(&out, "whole-2").accepted.reason, "Conflicted");
        assert!(!status(&out, "whole-2").accepted.ok);
        assert_eq!(status(&out, "port").accepted.reason, "Accepted");
        let hostname = |port| match &out.by_port[&port] {
            Decision::Verify(v) => v.hostname.clone(),
            Decision::Refuse => panic!("refused"),
        };
        assert_eq!(hostname(443), "port.test");
        assert_eq!(hostname(8443), "one.test");
    }

    /// A caCertificateRef that resolves to no CA refuses the backend and
    /// says why; one of another kind is InvalidKind.
    #[test]
    fn an_invalid_ca_refuses_the_backend() {
        for (name, validation, resolved) in [
            ("missing", with_ca("absent"), "InvalidCACertificateRef"),
            ("keyless", with_ca("keyless"), "InvalidCACertificateRef"),
            (
                "secret",
                json!({"hostname": "abc.example.com",
                       "caCertificateRefs": [{"group": "", "kind": "Secret", "name": "x"}]}),
                "InvalidKind",
            ),
        ] {
            let out = for_service(
                &[policy(name, 1, None, validation)],
                &service(),
                [443],
                |cm| match cm {
                    "keyless" => CaRef::NoKey,
                    _ => CaRef::Missing,
                },
            );
            let s = status(&out, name);
            assert_eq!(
                (s.accepted.ok, s.accepted.reason),
                (false, "NoValidCACertificate"),
                "{name}"
            );
            assert_eq!(
                (s.resolved_refs.ok, s.resolved_refs.reason),
                (false, resolved),
                "{name}"
            );
            assert_eq!(out.by_port[&443], Decision::Refuse, "{name}");
        }
    }

    /// A valid CA bundle verifies, by hostname, or by the SANs when set.
    #[test]
    fn a_valid_ca_verifies_by_hostname_or_sans() {
        let pem = ca_pem();
        let mut validation = with_ca("ca");
        validation["subjectAltNames"] = json!([
            {"type": "Hostname", "hostname": "san.example.com"},
            {"type": "URI", "uri": "spiffe://cluster/ns/apps/sa/web"},
        ]);
        let out = for_service(
            &[policy("p", 1, None, validation)],
            &service(),
            [443],
            |cm| match cm {
                "ca" => CaRef::Pem(pem.clone()),
                _ => CaRef::Missing,
            },
        );
        let s = status(&out, "p");
        assert!(s.accepted.ok && s.resolved_refs.ok, "{s:?}");
        let Decision::Verify(v) = &out.by_port[&443] else {
            panic!("refused");
        };
        assert_eq!(v.hostname, "abc.example.com");
        assert_eq!(
            v.subject_alt_names,
            [
                San::Hostname("san.example.com".into()),
                San::Uri("spiffe://cluster/ns/apps/sa/web".into())
            ]
        );
        assert_eq!(v.ca, Ca::Bundle(pem));
    }

    /// An unknown wellKnownCACertificates, or both CA sources at once, is
    /// Accepted=False Invalid and refuses the backend.
    #[test]
    fn a_bad_ca_source_is_invalid() {
        for validation in [
            json!({"hostname": "a.test", "wellKnownCACertificates": "example.com/mine"}),
            json!({"hostname": "a.test", "wellKnownCACertificates": "System",
                   "caCertificateRefs": [{"group": "", "kind": "ConfigMap", "name": "ca"}]}),
        ] {
            let out = for_service(
                &[policy("p", 1, None, validation)],
                &service(),
                [443],
                |_| CaRef::Missing,
            );
            assert_eq!(status(&out, "p").accepted.reason, "Invalid");
            assert_eq!(out.by_port[&443], Decision::Refuse);
        }
    }

    /// A sectionName the Service has no port for attaches to nothing.
    #[test]
    fn a_missing_section_is_target_not_found() {
        let out = for_service(
            &[policy("p", 1, Some("nope"), system("a.test"))],
            &service(),
            [443],
            |_| CaRef::Missing,
        );
        assert_eq!(status(&out, "p").accepted.reason, "TargetNotFound");
        assert!(!out.by_port.contains_key(&443), "nothing applies");
    }

    /// A port no policy names keeps today's behaviour.
    #[test]
    fn a_port_no_policy_names_is_left_alone() {
        let out = for_service(
            &[policy("p", 1, Some("https-1"), system("a.test"))],
            &service(),
            [443, 8443],
            |_| CaRef::Missing,
        );
        assert!(out.by_port.contains_key(&443));
        assert!(!out.by_port.contains_key(&8443));
    }
}
