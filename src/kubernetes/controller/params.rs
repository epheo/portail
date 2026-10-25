//! A Gateway's GatewayParameters: built-in defaults, then its
//! GatewayClass's parametersRef, then its own infrastructure parametersRef,
//! field by field; then validated.

use gateway_api::gatewayclasses::GatewayClass;
use gateway_api::gateways::Gateway;
use k8s_openapi::api::core::v1::ResourceRequirements;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::runtime::reflector::{ObjectRef, Store};
use kube::ResourceExt;

use super::ControllerCtx;
use crate::kubernetes::addresses::NETWORK_ADDRESS_TYPE;
use crate::kubernetes::crds::{
    GatewayParameters, GatewayParametersSpec, NetworkSelection, Provisioning, ServiceParameters,
    ServiceType,
};

const GROUP: &str = "portail.epheo.eu";
const KIND: &str = "GatewayParameters";

/// The cap main.rs puts on its own default worker threads.
const MAX_WORKER_THREADS: u32 = 4;

/// A Gateway's parameters with every default applied.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Params {
    pub provisioning: Provisioning,
    pub replicas: i32,
    pub image: String,
    pub resources: ResourceRequirements,
    /// `PORTAIL_WORKER_THREADS`: the cpu limit rounded up, capped as
    /// main.rs caps its default. None without a cpu limit.
    pub worker_threads: Option<u32>,
    pub service_type: ServiceType,
    pub networks: Vec<NetworkSelection>,
}

/// `Err` says why the parameters cannot be used: the Gateway is
/// Accepted=False InvalidParameters, and its last snapshot and objects
/// stay as they are.
pub(crate) fn resolve(ctx: &ControllerCtx, gateway: &Gateway) -> Result<Params, String> {
    let store = &ctx.cache.gateway_parameters;
    let mut layers = Vec::new();
    if let Some(class) = ctx
        .cache
        .gateway_classes
        .get(&ObjectRef::new(&gateway.spec.gateway_class_name))
    {
        layers.extend(class_layer(store, &class)?);
    }
    layers.extend(gateway_layer(store, gateway)?);

    let mut network_address = false;
    for addr in gateway.spec.addresses.iter().flatten() {
        if addr.r#type.as_deref() != Some(NETWORK_ADDRESS_TYPE) {
            continue;
        }
        let value = addr.value.as_deref().unwrap_or("");
        if !is_dns1123_subdomain(value) {
            return Err(format!(
                "spec.addresses: Network {value:?} is not a NetworkAttachmentDefinition name \
                 (a DNS-1123 subdomain)"
            ));
        }
        network_address = true;
    }
    concrete(
        merge(builtin(&ctx.dataplane_image), &layers),
        network_address,
    )
}

/// Whether `class`'s parametersRef can be used, as its Accepted condition
/// says: `Err` is Accepted=False InvalidParameters, for the class and its
/// Gateways alike.
#[allow(dead_code)] // For gatewayclass.rs (P4): class Accepted.
pub(crate) fn check_class(ctx: &ControllerCtx, class: &GatewayClass) -> Result<(), String> {
    let layers: Vec<_> = class_layer(&ctx.cache.gateway_parameters, class)?
        .into_iter()
        .collect();
    concrete(merge(builtin(&ctx.dataplane_image), &layers), false).map(|_| ())
}

/// The class's GatewayParameters, if it names one.
fn class_layer(
    store: &Store<GatewayParameters>,
    class: &GatewayClass,
) -> Result<Option<GatewayParametersSpec>, String> {
    let Some(r) = &class.spec.parameters_ref else {
        return Ok(None);
    };
    let who = format!("GatewayClass {}", class.name_any());
    check_kind(&who, &r.group, &r.kind)?;
    let Some(namespace) = r.namespace.as_deref().filter(|ns| !ns.is_empty()) else {
        return Err(format!(
            "{who}: parametersRef to {KIND} {} needs a namespace",
            r.name
        ));
    };
    lookup(store, namespace, &r.name).map(Some)
}

/// The Gateway's own GatewayParameters, in its namespace, if it names one.
fn gateway_layer(
    store: &Store<GatewayParameters>,
    gateway: &Gateway,
) -> Result<Option<GatewayParametersSpec>, String> {
    let Some(r) = gateway
        .spec
        .infrastructure
        .as_ref()
        .and_then(|i| i.parameters_ref.as_ref())
    else {
        return Ok(None);
    };
    check_kind("Gateway infrastructure", &r.group, &r.kind)?;
    let namespace = gateway.namespace().unwrap_or_else(|| "default".into());
    lookup(store, &namespace, &r.name).map(Some)
}

fn check_kind(who: &str, group: &str, kind: &str) -> Result<(), String> {
    if group == GROUP && kind == KIND {
        return Ok(());
    }
    Err(format!(
        "{who}: parametersRef must name a {KIND} of {GROUP}, not a {kind} of {group:?}"
    ))
}

/// The referent, its fields checked one by one.
fn lookup(
    store: &Store<GatewayParameters>,
    namespace: &str,
    name: &str,
) -> Result<GatewayParametersSpec, String> {
    let Some(params) = store.get(&ObjectRef::new(name).within(namespace)) else {
        return Err(format!("{KIND} {namespace}/{name} not found"));
    };
    check_fields(&params.spec).map_err(|e| format!("{KIND} {namespace}/{name}: {e}"))?;
    Ok(params.spec.clone())
}

/// What the CRD schema cannot say, and its bounds again: a CRD installed
/// from an older manifest may lack them.
fn check_fields(spec: &GatewayParametersSpec) -> Result<(), String> {
    if let Some(replicas) = spec.replicas.filter(|r| *r < 1) {
        return Err(format!("replicas must be at least 1, not {replicas}"));
    }
    for (i, n) in spec.networks.iter().flatten().enumerate() {
        if !is_dns1123_subdomain(&n.name) {
            return Err(format!(
                "networks[{i}].name {:?} is not a NetworkAttachmentDefinition name \
                 (a DNS-1123 subdomain)",
                n.name
            ));
        }
        if let Some(ns) = n.namespace.as_deref() {
            if !crate::sync::is_dns1123_label(ns) {
                return Err(format!(
                    "networks[{i}].namespace {ns:?} is no namespace name"
                ));
            }
        }
        if let Some(ip) = n.ips.iter().flatten().find(|ip| !is_cidr(ip)) {
            return Err(format!(
                "networks[{i}].ips: {ip:?} is not an address in CIDR form"
            ));
        }
        if let Some(gw) = n
            .gateway
            .iter()
            .flatten()
            .find(|gw| gw.parse::<std::net::IpAddr>().is_err())
        {
            return Err(format!(
                "networks[{i}].gateway: {gw:?} is not an IP address"
            ));
        }
    }
    if let Some(cpu) = cpu_limit(spec.resources.as_ref()) {
        match cpu_cores(&cpu.0) {
            Some(cores) if cores > 0.0 => {}
            _ => {
                return Err(format!(
                    "resources.limits.cpu {:?} is no positive quantity",
                    cpu.0
                ))
            }
        }
    }
    Ok(())
}

/// Built-in defaults (operator parity: BuildDeployment's resources).
fn builtin(image: &str) -> GatewayParametersSpec {
    let quantities = |cpu: &str, memory: &str| {
        Some(
            [("cpu", cpu), ("memory", memory)]
                .into_iter()
                .map(|(k, v)| (k.to_string(), Quantity(v.to_string())))
                .collect(),
        )
    };
    GatewayParametersSpec {
        provisioning: Some(Provisioning::Managed),
        replicas: Some(1),
        image: Some(image.to_string()),
        resources: Some(ResourceRequirements {
            requests: quantities("100m", "64Mi"),
            limits: quantities("2", "256Mi"),
            ..Default::default()
        }),
        service: None,
        networks: None,
    }
}

/// `base` with each layer's set fields over it, in order; `networks` and
/// `resources` replace whole.
fn merge(
    mut base: GatewayParametersSpec,
    layers: &[GatewayParametersSpec],
) -> GatewayParametersSpec {
    for layer in layers {
        let GatewayParametersSpec {
            provisioning,
            replicas,
            image,
            resources,
            service,
            networks,
        } = layer.clone();
        base.provisioning = provisioning.or(base.provisioning);
        base.replicas = replicas.or(base.replicas);
        base.image = image.or(base.image);
        base.resources = resources.or(base.resources);
        base.networks = networks.or(base.networks);
        if let Some(type_) = service.and_then(|s| s.type_) {
            base.service = Some(ServiceParameters { type_: Some(type_) });
        }
    }
    base
}

/// The merged parameters with every default filled in, checked across
/// fields.
fn concrete(spec: GatewayParametersSpec, network_address: bool) -> Result<Params, String> {
    let replicas = spec.replicas.unwrap_or(1);
    let networks = spec.networks.unwrap_or_default();
    if replicas > 1
        && networks
            .iter()
            .any(|n| n.ips.iter().flatten().next().is_some())
    {
        return Err(format!(
            "networks with static ips need replicas 1, not {replicas}: one static IP cannot \
             be on two pods"
        ));
    }
    let resources = spec.resources.unwrap_or_default();
    let worker_threads = cpu_limit(Some(&resources))
        .and_then(|cpu| cpu_cores(&cpu.0))
        .map(|cores| (cores.ceil() as u32).clamp(1, MAX_WORKER_THREADS));
    // Operator parity: a Gateway on Multus networks binds on them, and
    // nothing fronts it.
    let service_type =
        spec.service
            .and_then(|s| s.type_)
            .unwrap_or(if network_address || !networks.is_empty() {
                ServiceType::None
            } else {
                ServiceType::LoadBalancer
            });
    Ok(Params {
        provisioning: spec.provisioning.unwrap_or(Provisioning::Managed),
        replicas,
        image: spec.image.unwrap_or_default(),
        resources,
        worker_threads,
        service_type,
        networks,
    })
}

fn cpu_limit(resources: Option<&ResourceRequirements>) -> Option<&Quantity> {
    resources?.limits.as_ref()?.get("cpu")
}

/// A Kubernetes quantity in cores: a decimal number with an optional SI,
/// binary or exponent suffix (resource.Quantity's grammar).
fn cpu_cores(quantity: &str) -> Option<f64> {
    let split = quantity
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '+' || c == '-'))
        .unwrap_or(quantity.len());
    let (number, suffix) = quantity.split_at(split);
    let number: f64 = number.parse().ok()?;
    let scale = match suffix {
        "" => 1.0,
        "n" => 1e-9,
        "u" => 1e-6,
        "m" => 1e-3,
        "k" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        "P" => 1e15,
        "E" => 1e18,
        "Ki" => 1024f64,
        "Mi" => 1024f64.powi(2),
        "Gi" => 1024f64.powi(3),
        "Ti" => 1024f64.powi(4),
        "Pi" => 1024f64.powi(5),
        "Ei" => 1024f64.powi(6),
        e => 10f64.powi(e.strip_prefix(['e', 'E'])?.parse().ok()?),
    };
    Some(number * scale).filter(|cores| cores.is_finite())
}

/// RFC 1123 subdomain, as Kubernetes names most objects: at most 253
/// characters, dot-separated runs of lowercase alphanumerics and '-', each
/// alphanumeric at both ends.
fn is_dns1123_subdomain(s: &str) -> bool {
    let alnum = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    s.len() <= 253
        && s.split('.').all(|label| {
            let b = label.as_bytes();
            !b.is_empty()
                && b.iter().all(|&c| alnum(c) || c == b'-')
                && alnum(b[0])
                && alnum(b[b.len() - 1])
        })
}

fn is_cidr(s: &str) -> bool {
    let Some((addr, len)) = s.split_once('/') else {
        return false;
    };
    let max = match addr.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(_)) => 32,
        Ok(std::net::IpAddr::V6(_)) => 128,
        Err(_) => return false,
    };
    len.parse::<u8>().is_ok_and(|len| len <= max)
}

#[cfg(test)]
mod tests {
    use super::super::testkit::{self, apply, MockApi, TestCluster, CONTROLLER};
    use super::*;
    use gateway_api::gatewayclasses::GatewayClassParametersRef;
    use gateway_api::gateways::{GatewayInfrastructure, GatewayInfrastructureParametersRef};

    fn parameters(namespace: &str, name: &str, yaml: &str) -> GatewayParameters {
        let mut p = GatewayParameters::new(name, serde_yaml::from_str(yaml).unwrap());
        p.metadata.namespace = Some(namespace.into());
        p
    }

    fn class_ref(namespace: Option<&str>, name: &str) -> GatewayClassParametersRef {
        GatewayClassParametersRef {
            group: GROUP.into(),
            kind: KIND.into(),
            name: name.into(),
            namespace: namespace.map(Into::into),
        }
    }

    /// A cluster with class `portail`, its parametersRef `class_params`.
    fn cluster(class_params: Option<GatewayClassParametersRef>) -> TestCluster {
        let mut cluster = TestCluster::default();
        let mut class = testkit::gateway_class("portail", CONTROLLER);
        class.spec.parameters_ref = class_params;
        apply(&mut cluster.gateway_classes, class);
        cluster
    }

    fn gateway_with_ref(name: &str) -> Gateway {
        let mut gw = testkit::gateway("gw", 1);
        gw.spec.infrastructure = Some(GatewayInfrastructure {
            parameters_ref: Some(GatewayInfrastructureParametersRef {
                group: GROUP.into(),
                kind: KIND.into(),
                name: name.into(),
            }),
            ..Default::default()
        });
        gw
    }

    fn resolve_in(cluster: &TestCluster, gw: &Gateway) -> Result<Params, String> {
        resolve(&cluster.ctx(MockApi::default().client()), gw)
    }

    fn quantities(pairs: &[(&str, &str)]) -> Option<std::collections::BTreeMap<String, Quantity>> {
        Some(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), Quantity(v.to_string())))
                .collect(),
        )
    }

    #[tokio::test]
    async fn no_parameters_gives_the_defaults() {
        let params = resolve_in(&cluster(None), &testkit::gateway("gw", 1)).unwrap();
        assert_eq!(
            params,
            Params {
                provisioning: Provisioning::Managed,
                replicas: 1,
                image: "ghcr.io/epheo/portail:test".into(),
                resources: ResourceRequirements {
                    requests: quantities(&[("cpu", "100m"), ("memory", "64Mi")]),
                    limits: quantities(&[("cpu", "2"), ("memory", "256Mi")]),
                    ..Default::default()
                },
                worker_threads: Some(2),
                service_type: ServiceType::LoadBalancer,
                networks: vec![],
            }
        );
    }

    /// Set fields win field by field, the Gateway's over the class's;
    /// `networks` and `resources` replace whole.
    #[tokio::test]
    async fn gateway_parameters_override_the_class_field_by_field() {
        let mut cluster = cluster(Some(class_ref(Some("portail-system"), "defaults")));
        apply(
            &mut cluster.parameters,
            parameters(
                "portail-system",
                "defaults",
                "replicas: 2
image: registry.local/portail:1
service: {type: ClusterIP}
resources: {limits: {cpu: \"3\", memory: 1Gi}, requests: {cpu: 500m}}
networks: [{name: a}, {name: b}]
",
            ),
        );
        apply(
            &mut cluster.parameters,
            parameters(
                "default",
                "mine",
                "image: registry.local/portail:2
resources: {limits: {memory: 512Mi}}
networks: [{name: c}]
",
            ),
        );
        let params = resolve_in(&cluster, &gateway_with_ref("mine")).unwrap();
        assert_eq!(params.replicas, 2, "from the class");
        assert_eq!(params.image, "registry.local/portail:2", "the Gateway's");
        assert_eq!(
            params.service_type,
            ServiceType::ClusterIP,
            "from the class"
        );
        assert_eq!(
            params.resources,
            ResourceRequirements {
                limits: quantities(&[("memory", "512Mi")]),
                ..Default::default()
            },
            "replaced whole: no cpu limit, no requests"
        );
        assert_eq!(params.worker_threads, None, "no cpu limit");
        let names: Vec<&str> = params.networks.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["c"], "replaced whole");
    }

    #[tokio::test]
    async fn unmanaged_from_the_class() {
        let mut cluster = cluster(Some(class_ref(Some("portail-system"), "hand-written")));
        apply(
            &mut cluster.parameters,
            parameters("portail-system", "hand-written", "provisioning: Unmanaged"),
        );
        let params = resolve_in(&cluster, &testkit::gateway("gw", 1)).unwrap();
        assert_eq!(params.provisioning, Provisioning::Unmanaged);
    }

    /// A missing referent, a wrong group or kind, or a class reference
    /// without a namespace: InvalidParameters, with why.
    #[tokio::test]
    async fn unusable_references_are_invalid_parameters() {
        let err = resolve_in(&cluster(None), &gateway_with_ref("absent")).unwrap_err();
        assert_eq!(err, "GatewayParameters default/absent not found");

        let err = resolve_in(
            &cluster(Some(class_ref(Some("portail-system"), "absent"))),
            &testkit::gateway("gw", 1),
        )
        .unwrap_err();
        assert_eq!(err, "GatewayParameters portail-system/absent not found");

        let err = resolve_in(
            &cluster(Some(class_ref(None, "defaults"))),
            &testkit::gateway("gw", 1),
        )
        .unwrap_err();
        assert!(err.contains("needs a namespace"), "{err}");

        let mut wrong = class_ref(Some("default"), "config");
        wrong.group = String::new();
        wrong.kind = "ConfigMap".into();
        let err = resolve_in(&cluster(Some(wrong)), &testkit::gateway("gw", 1)).unwrap_err();
        assert_eq!(
            err,
            "GatewayClass portail: parametersRef must name a GatewayParameters of \
             portail.epheo.eu, not a ConfigMap of \"\""
        );

        let mut gw = gateway_with_ref("mine");
        let r = gw.spec.infrastructure.as_mut().unwrap();
        r.parameters_ref.as_mut().unwrap().kind = "Other".into();
        let err = resolve_in(&cluster(None), &gw).unwrap_err();
        assert!(
            err.starts_with("Gateway infrastructure: parametersRef"),
            "{err}"
        );
    }

    /// A static IP sits on one pod.
    #[tokio::test]
    async fn static_ips_with_two_replicas_are_rejected() {
        let mut cluster = cluster(None);
        apply(
            &mut cluster.parameters,
            parameters(
                "default",
                "mine",
                "replicas: 2
networks: [{name: br-ex-network, namespace: default, ips: [172.16.87.21/24]}]
",
            ),
        );
        let err = resolve_in(&cluster, &gateway_with_ref("mine")).unwrap_err();
        assert!(err.contains("static ips need replicas 1, not 2"), "{err}");
    }

    /// The class's replicas meet the Gateway's static ips: the merged
    /// result is what is checked.
    #[tokio::test]
    async fn the_merge_is_checked() {
        let mut cluster = cluster(Some(class_ref(Some("portail-system"), "defaults")));
        apply(
            &mut cluster.parameters,
            parameters("portail-system", "defaults", "replicas: 2"),
        );
        apply(
            &mut cluster.parameters,
            parameters(
                "default",
                "mine",
                "networks: [{name: n, ips: [10.0.0.5/24]}]",
            ),
        );
        assert!(resolve_in(&cluster, &gateway_with_ref("mine")).is_err());
    }

    #[tokio::test]
    async fn malformed_fields_are_invalid_parameters() {
        for (yaml, says) in [
            ("replicas: 0", "replicas must be at least 1"),
            ("networks: [{name: Bad_Name}]", "networks[0].name"),
            (
                "networks: [{name: n, namespace: a.b}]",
                "networks[0].namespace",
            ),
            (
                "networks: [{name: n, ips: [172.16.87.21]}]",
                "networks[0].ips",
            ),
            (
                "networks: [{name: n, ips: [10.0.0.1/33]}]",
                "networks[0].ips",
            ),
            (
                "networks: [{name: n, gateway: [nope]}]",
                "networks[0].gateway",
            ),
            ("resources: {limits: {cpu: lots}}", "resources.limits.cpu"),
            ("resources: {limits: {cpu: \"0\"}}", "resources.limits.cpu"),
        ] {
            let mut cluster = cluster(None);
            apply(&mut cluster.parameters, parameters("default", "mine", yaml));
            let err = resolve_in(&cluster, &gateway_with_ref("mine")).unwrap_err();
            assert!(
                err.starts_with("GatewayParameters default/mine: ") && err.contains(says),
                "{yaml}: {err}"
            );
        }
    }

    /// Operator parity: ExtractNetworkNames refuses a Network address that
    /// names no NAD.
    #[tokio::test]
    async fn a_network_address_must_be_a_nad_name() {
        for bad in ["", "net-a,net-b", "net a", "Net-Frontend", "-frontend"] {
            let mut gw = testkit::gateway("gw", 1);
            gw.spec.addresses = Some(vec![serde_json::from_value(
                serde_json::json!({"type": NETWORK_ADDRESS_TYPE, "value": bad}),
            )
            .unwrap()]);
            let err = resolve_in(&cluster(None), &gw).unwrap_err();
            assert!(err.starts_with("spec.addresses: Network"), "{bad:?}: {err}");
        }
    }

    /// Worker threads follow the cpu limit, rounded up, capped at 4.
    #[tokio::test]
    async fn worker_threads_from_the_cpu_limit() {
        for (cpu, threads) in [
            ("250m", 1),
            ("1", 1),
            ("1500m", 2),
            ("2", 2),
            ("3.5", 4),
            ("16", 4),
            ("2e0", 2),
        ] {
            let mut cluster = cluster(None);
            let yaml = format!("resources: {{limits: {{cpu: \"{cpu}\"}}}}");
            apply(
                &mut cluster.parameters,
                parameters("default", "mine", &yaml),
            );
            let params = resolve_in(&cluster, &gateway_with_ref("mine")).unwrap();
            assert_eq!(params.worker_threads, Some(threads), "{cpu}");
        }
    }

    /// Unset, the Service is None when the Gateway binds on networks, else
    /// LoadBalancer; a set type wins.
    #[tokio::test]
    async fn the_service_type_default_follows_the_networks() {
        let mut gw = testkit::gateway("gw", 1);
        gw.spec.addresses = Some(vec![serde_json::from_value(
            serde_json::json!({"type": NETWORK_ADDRESS_TYPE, "value": "udn-a"}),
        )
        .unwrap()]);
        let params = resolve_in(&cluster(None), &gw).unwrap();
        assert_eq!(params.service_type, ServiceType::None);

        let mut cluster = cluster(None);
        apply(
            &mut cluster.parameters,
            parameters("default", "mine", "networks: [{name: br-ex}]"),
        );
        let params = resolve_in(&cluster, &gateway_with_ref("mine")).unwrap();
        assert_eq!(params.service_type, ServiceType::None);

        apply(
            &mut cluster.parameters,
            parameters(
                "default",
                "mine",
                "networks: [{name: br-ex}]\nservice: {type: NodePort}",
            ),
        );
        let params = resolve_in(&cluster, &gateway_with_ref("mine")).unwrap();
        assert_eq!(params.service_type, ServiceType::NodePort);
    }

    #[tokio::test]
    async fn check_class_says_why() {
        let cluster = cluster(Some(class_ref(Some("portail-system"), "absent")));
        let ctx = cluster.ctx(MockApi::default().client());
        let class = ctx
            .cache
            .gateway_classes
            .get(&ObjectRef::new("portail"))
            .unwrap();
        assert_eq!(
            check_class(&ctx, &class).unwrap_err(),
            "GatewayParameters portail-system/absent not found"
        );
        assert!(check_class(&ctx, &testkit::gateway_class("plain", CONTROLLER)).is_ok());
    }
}
