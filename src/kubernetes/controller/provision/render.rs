//! A Managed Gateway's objects as they are applied: its ServiceAccount,
//! Deployment, Service and PodDisruptionBudget.

use std::collections::BTreeMap;

use gateway_api::gateways::Gateway;
use k8s_openapi::api::apps::v1::{
    Deployment, DeploymentSpec, DeploymentStrategy, RollingUpdateDeployment,
};
use k8s_openapi::api::core::v1::{
    Capabilities, ConfigMapProjection, Container, EnvVar, EnvVarSource, HTTPGetAction, KeyToPath,
    ObjectFieldSelector, PodSecurityContext, PodSpec, PodTemplateSpec, Probe,
    ProjectedVolumeSource, SeccompProfile, SecurityContext, Service, ServiceAccount,
    ServiceAccountTokenProjection, ServicePort, ServiceSpec, TopologySpreadConstraint, Volume,
    VolumeMount, VolumeProjection,
};
use k8s_openapi::api::policy::v1::{PodDisruptionBudget, PodDisruptionBudgetSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::ResourceExt;
use serde::Serialize;

use super::ports::{
    bound_ports, derive_ports, protocol, DerivedPort, ADMIN_PORT, PRIVILEGED_PORT_MAX,
};
use crate::cli::{ControllerArgs, CONTROLLER_CA_ENV};
use crate::kubernetes::addresses::NETWORK_ADDRESS_TYPE;
use crate::kubernetes::controller::ca::ControllerCa;
use crate::kubernetes::controller::params::Params;
use crate::kubernetes::crds::{ControllerTransport, NetworkSelection, ServiceType};
use crate::kubernetes::naming::{
    controller_audience, label_value, resource_name, LABEL_GATEWAY, LABEL_GATEWAY_NAME,
    LABEL_MANAGED_BY, LABEL_NAME, MANAGED_BY, NAME,
};
use crate::sync::Transport;

/// The controller as its Managed data planes see it: where to reach it,
/// how to trust it, and the install identity every object it provisions
/// carries.
#[derive(Clone, Debug)]
pub(crate) struct ControllerEndpoint {
    /// The controller Service's sync URL.
    pub address: String,
    /// The audience of the token the data plane presents.
    pub audience: String,
    /// The controller CA, which the data plane verifies the controller
    /// against.
    pub ca_pem: String,
    /// [`LABEL_CONTROLLER`]'s value for this install.
    pub install: String,
}

impl ControllerEndpoint {
    pub(crate) fn new(args: &ControllerArgs, ca: &ControllerCa) -> Self {
        Self {
            address: format!(
                "https://{}.{}.svc:{}",
                args.service_name, args.namespace, args.sync_port
            ),
            audience: controller_audience(&args.service_name, &args.namespace),
            ca_pem: ca.ca_pem().to_owned(),
            install: install_label(&args.controller_name),
        }
    }

    /// The default controller Service in `portail-system`, a dummy CA.
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self {
            address: "https://portail-controller.portail-system.svc:9443".into(),
            audience: "portail-controller.portail-system.svc".into(),
            ca_pem: "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n".into(),
            install: install_label(super::super::testkit::CONTROLLER),
        }
    }
}

/// The install an object belongs to, valued by [`install_label`]. Every
/// portail install writes managed-by portail-controller and owns its
/// objects by Gateway uid; only this tells two installs (two
/// controllerNames) apart, so one never deletes or takes over the other's
/// objects when a Gateway moves between their classes.
pub(crate) const LABEL_CONTROLLER: &str = "portail.epheo.eu/controller-name";

/// Hex digits of SHA-256 a shortened value ends with, as naming's.
const HASH_HEX_DIGITS: usize = 8;

/// A controllerName as a label value: as it is when it is one, else with
/// each character a label value refuses (its '/' at least) turned to '-',
/// cut to fit, and a hash of the whole name appended, so two names never
/// share a value.
pub(crate) fn install_label(controller_name: &str) -> String {
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    let edge = |c: char| c.is_ascii_alphanumeric();
    let max = crate::sync::MAX_DNS1123_LABEL;
    if controller_name.len() <= max
        && controller_name.chars().all(allowed)
        && controller_name.starts_with(edge)
        && controller_name.ends_with(edge)
    {
        return controller_name.to_string();
    }
    let cleaned: String = controller_name
        .chars()
        .map(|c| if allowed(c) { c } else { '-' })
        .collect();
    let keep = max - 1 - HASH_HEX_DIGITS;
    let cut = cleaned
        .get(..keep)
        .unwrap_or(&cleaned)
        .trim_matches(|c: char| !edge(c));
    let hash = &crate::sync::sha256_hex(controller_name.as_bytes())[..HASH_HEX_DIGITS];
    if cut.is_empty() {
        hash.to_string()
    } else {
        format!("{cut}-{hash}")
    }
}

/// The data plane's `--token-file` default (cli.rs), where the projected
/// token is mounted.
const TOKEN_DIR: &str = "/var/run/secrets/portail.epheo.eu/controller";
const TOKEN_FILE: &str = "token";

/// The projected token lifetime: the projected-token default; kubelet
/// refreshes it at 80%.
const TOKEN_EXPIRATION_SECS: i64 = 3600;

/// APIServerProxy's second projected volume, where the data plane's
/// `--apiserver-token-file` and `--apiserver-ca-file` defaults (cli.rs)
/// point: an API-audience token and the cluster CA.
const APISERVER_DIR: &str = "/var/run/secrets/portail.epheo.eu/apiserver";
const CA_FILE: &str = "ca.crt";
/// Published in every namespace by the controller manager: the CA the
/// apiserver's serving certificate chains to, under key `ca.crt`.
const ROOT_CA_CONFIGMAP: &str = "kube-root-ca.crt";

/// `--shutdown-delay`'s default 5s, plus the data plane's DRAIN_TIMEOUT 30s
/// (src/proxy/data_plane.rs), plus 5s for the leave report and the exit.
const TERMINATION_GRACE_SECS: i64 = 40;

/// The objects of one Managed Gateway.
#[derive(Clone, Debug, Default)]
pub(crate) struct Objects {
    pub service_account: ServiceAccount,
    pub deployment: Deployment,
    /// None when the Service type is None.
    pub service: Option<Service>,
    /// Only above one replica: with one, a PDB could only block a drain or
    /// allow everything.
    pub pdb: Option<PodDisruptionBudget>,
}

/// `gateway`'s objects under `params`, and the port its data plane binds
/// per (transport, published port) where a Service maps one onto the other.
/// `existing` is the owned Service's assignments, kept for listeners that
/// stay.
pub(crate) fn desired(
    gateway: &Gateway,
    params: &Params,
    existing: &BTreeMap<(Transport, u16), u16>,
    controller: &ControllerEndpoint,
) -> (Objects, BTreeMap<(Transport, u16), u16>) {
    let ports = derive_ports(&gateway.spec.listeners);
    let bound = if params.service_type == ServiceType::None {
        BTreeMap::new()
    } else {
        bound_ports(&ports, existing)
    };
    let objects = render(gateway, params, &ports, &bound, controller);
    (objects, bound)
}

fn render(
    gateway: &Gateway,
    params: &Params,
    ports: &[DerivedPort],
    bound: &BTreeMap<(Transport, u16), u16>,
    controller: &ControllerEndpoint,
) -> Objects {
    let gw_name = gateway.name_any();
    let gw_ns = gateway.namespace().unwrap_or_else(|| "default".into());
    let name = resource_name(&gw_name);
    let value = label_value(&gw_name);
    let labels: BTreeMap<String, String> = [
        (LABEL_NAME, NAME),
        (LABEL_MANAGED_BY, MANAGED_BY),
        (LABEL_GATEWAY, value.as_str()),
        (LABEL_GATEWAY_NAME, value.as_str()),
        (LABEL_CONTROLLER, controller.install.as_str()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let selector = LabelSelector {
        match_labels: Some([(LABEL_GATEWAY.to_string(), value.clone())].into()),
        ..Default::default()
    };
    let metadata = ObjectMeta {
        name: Some(name.clone()),
        namespace: Some(gw_ns.clone()),
        labels: Some(labels.clone()),
        // No blockOwnerDeletion: it needs a gateways/finalizers grant, and
        // garbage collection deletes the children without it.
        owner_references: Some(vec![OwnerReference {
            api_version: "gateway.networking.k8s.io/v1".into(),
            kind: "Gateway".into(),
            name: gw_name.clone(),
            uid: gateway.metadata.uid.clone().unwrap_or_default(),
            controller: Some(true),
            block_owner_deletion: None,
        }]),
        ..Default::default()
    };

    let service_account = ServiceAccount {
        metadata: metadata.clone(),
        automount_service_account_token: Some(false),
        ..Default::default()
    };

    let service = (params.service_type != ServiceType::None).then(|| Service {
        metadata: metadata.clone(),
        spec: Some(ServiceSpec {
            type_: Some(service_type_name(params.service_type).into()),
            selector: selector.match_labels.clone(),
            ports: Some(
                ports
                    .iter()
                    .map(|p| ServicePort {
                        name: Some(p.name.clone()),
                        port: p.port.into(),
                        protocol: Some(protocol(p.transport).into()),
                        target_port: Some(IntOrString::Int(
                            bound
                                .get(&(p.transport, p.port))
                                .copied()
                                .unwrap_or(p.port)
                                .into(),
                        )),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        }),
        status: None,
    });

    let pdb = (params.replicas > 1).then(|| PodDisruptionBudget {
        metadata: metadata.clone(),
        spec: Some(PodDisruptionBudgetSpec {
            max_unavailable: Some(IntOrString::Int(1)),
            selector: Some(selector.clone()),
            ..Default::default()
        }),
        status: None,
    });

    // A static IP cannot be on two pods: the old one goes first, and no
    // other endpoint takes over while it drains, so its delay is pure gap.
    let recreate = params
        .networks
        .iter()
        .any(|n| n.ips.as_ref().is_some_and(|ips| !ips.is_empty()));
    let strategy = if recreate {
        DeploymentStrategy {
            type_: Some("Recreate".into()),
            rolling_update: None,
        }
    } else {
        DeploymentStrategy {
            type_: Some("RollingUpdate".into()),
            rolling_update: Some(RollingUpdateDeployment {
                max_surge: Some(IntOrString::Int(1)),
                max_unavailable: Some(IntOrString::Int(0)),
            }),
        }
    };

    let mut args = vec![
        "dataplane".to_string(),
        "--gateway".into(),
        format!("{gw_ns}/{gw_name}"),
        "--controller-address".into(),
        controller.address.clone(),
        // Keeps /metrics off Multus and VIP interfaces.
        "--admin-address".into(),
        "$(POD_IP)".into(),
    ];
    if recreate {
        args.push("--shutdown-delay=0s".into());
    }
    let proxied = params.controller_transport == ControllerTransport::ApiServerProxy;
    if proxied {
        args.extend(["--transport".into(), "apiserver-proxy".into()]);
    }

    let mut env = vec![
        EnvVar {
            name: "POD_IP".into(),
            value_from: Some(EnvVarSource {
                field_ref: Some(ObjectFieldSelector {
                    field_path: "status.podIP".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
        EnvVar {
            name: CONTROLLER_CA_ENV.into(),
            value: Some(controller.ca_pem.clone()),
            ..Default::default()
        },
    ];
    if let Some(threads) = params.worker_threads {
        env.push(EnvVar {
            name: "PORTAIL_WORKER_THREADS".into(),
            value: Some(threads.to_string()),
            ..Default::default()
        });
    }

    // Pairs with the image's cap_net_bind_service=+p; without it the pod
    // passes restricted Pod Security.
    let binds_privileged = ports.iter().any(|p| {
        bound.get(&(p.transport, p.port)).copied().unwrap_or(p.port) <= PRIVILEGED_PORT_MAX
    });

    let container = Container {
        name: "portail".into(),
        image: Some(params.image.clone()),
        // Operator parity: images loaded into kind or an air-gapped node
        // are used as they are.
        image_pull_policy: Some("IfNotPresent".into()),
        args: Some(args),
        env: Some(env),
        resources: Some(params.resources.clone()),
        // Periods and thresholds: operator parity, liveness at the kubelet
        // defaults. Startup waits on /livez, not /readyz: with no
        // last-known-good, a pod started while the controller is down is
        // NotReady until it returns, and a startup failure would kill it
        // in a crash loop whose backoff outlasts the outage.
        readiness_probe: Some(probe("/readyz", 5, 6)),
        startup_probe: Some(probe("/livez", 1, 30)),
        liveness_probe: Some(probe("/livez", 10, 3)),
        security_context: Some(SecurityContext {
            allow_privilege_escalation: Some(false),
            read_only_root_filesystem: Some(true),
            capabilities: Some(Capabilities {
                drop: Some(vec!["ALL".into()]),
                add: binds_privileged.then(|| vec!["NET_BIND_SERVICE".into()]),
            }),
            ..Default::default()
        }),
        volume_mounts: Some(
            std::iter::once(("controller-token", TOKEN_DIR))
                .chain(proxied.then_some(("apiserver-token", APISERVER_DIR)))
                .map(|(name, path)| VolumeMount {
                    name: name.into(),
                    mount_path: path.into(),
                    read_only: Some(true),
                    ..Default::default()
                })
                .collect(),
        ),
        // No containerPorts: they are informational, and listener changes
        // would roll the pods (operator parity).
        ..Default::default()
    };

    let networks = multus_networks(&params.networks, gateway);
    let pod_spec = PodSpec {
        service_account_name: Some(name.clone()),
        automount_service_account_token: Some(false),
        termination_grace_period_seconds: Some(TERMINATION_GRACE_SECS),
        security_context: Some(PodSecurityContext {
            run_as_non_root: Some(true),
            seccomp_profile: Some(SeccompProfile {
                type_: "RuntimeDefault".into(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        topology_spread_constraints: (params.replicas > 1).then(|| {
            vec![TopologySpreadConstraint {
                max_skew: 1,
                topology_key: "kubernetes.io/hostname".into(),
                when_unsatisfiable: "ScheduleAnyway".into(),
                label_selector: Some(selector.clone()),
                ..Default::default()
            }]
        }),
        containers: vec![container],
        volumes: Some(
            std::iter::once(projected(
                "controller-token",
                vec![token(Some(controller.audience.clone()))],
            ))
            .chain(proxied.then(apiserver_volume))
            .collect(),
        ),
        ..Default::default()
    };

    let deployment = Deployment {
        metadata,
        spec: Some(DeploymentSpec {
            replicas: Some(params.replicas),
            selector,
            strategy: Some(strategy),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    annotations: networks.map(|n| [(MULTUS_NETWORKS.to_string(), n)].into()),
                    ..Default::default()
                }),
                spec: Some(pod_spec),
            },
            ..Default::default()
        }),
        status: None,
    };

    Objects {
        service_account,
        deployment,
        service,
        pdb,
    }
}

fn projected(name: &str, sources: Vec<VolumeProjection>) -> Volume {
    Volume {
        name: name.into(),
        projected: Some(ProjectedVolumeSource {
            sources: Some(sources),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// A pod-bound ServiceAccount token for `audience`, the apiserver's when
/// `None`.
fn token(audience: Option<String>) -> VolumeProjection {
    VolumeProjection {
        service_account_token: Some(ServiceAccountTokenProjection {
            audience,
            expiration_seconds: Some(TOKEN_EXPIRATION_SECS),
            path: TOKEN_FILE.into(),
        }),
        ..Default::default()
    }
}

/// APIServerProxy: what the apiserver hop needs, and nothing more than a
/// default mount would give, while automountServiceAccountToken stays
/// false. The token is good for any API call the ServiceAccount may make:
/// the apiserver-proxy Role allows one.
fn apiserver_volume() -> Volume {
    projected(
        "apiserver-token",
        vec![
            token(None),
            VolumeProjection {
                config_map: Some(ConfigMapProjection {
                    name: ROOT_CA_CONFIGMAP.into(),
                    items: Some(vec![KeyToPath {
                        key: CA_FILE.into(),
                        path: CA_FILE.into(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ],
    )
}

fn probe(path: &str, period: i32, failures: i32) -> Probe {
    Probe {
        http_get: Some(HTTPGetAction {
            path: Some(path.into()),
            port: IntOrString::Int(ADMIN_PORT.into()),
            ..Default::default()
        }),
        period_seconds: Some(period),
        failure_threshold: Some(failures),
        ..Default::default()
    }
}

fn service_type_name(t: ServiceType) -> &'static str {
    match t {
        ServiceType::LoadBalancer => "LoadBalancer",
        ServiceType::ClusterIP => "ClusterIP",
        ServiceType::NodePort => "NodePort",
        ServiceType::None => "None",
    }
}

const MULTUS_NETWORKS: &str = "k8s.v1.cni.cncf.io/networks";

/// A Multus NetworkSelectionElement, its keys as Multus spells them.
#[derive(Serialize)]
struct Selection<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    namespace: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ips: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mac: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    interface: Option<&'a str>,
    #[serde(rename = "default-route", skip_serializing_if = "Option::is_none")]
    default_route: Option<&'a [String]>,
}

/// The pod's Multus annotation: `networks`, then each Network address not
/// already among them; None when there is none.
fn multus_networks(networks: &[NetworkSelection], gateway: &Gateway) -> Option<String> {
    let gw_ns = gateway.namespace().unwrap_or_else(|| "default".into());
    let mut selections: Vec<Selection> = networks
        .iter()
        .map(|n| Selection {
            name: &n.name,
            namespace: n.namespace.as_deref(),
            ips: n.ips.as_deref(),
            mac: n.mac.as_deref(),
            interface: n.interface.as_deref(),
            default_route: n.gateway.as_deref(),
        })
        .collect();
    for addr in gateway.spec.addresses.iter().flatten() {
        if addr.r#type.as_deref() != Some(NETWORK_ADDRESS_TYPE) {
            continue;
        }
        let Some(value) = addr.value.as_deref() else {
            continue;
        };
        let listed = selections
            .iter()
            .any(|s| s.name == value && s.namespace.is_none_or(|ns| ns == gw_ns));
        if !listed {
            selections.push(Selection {
                name: value,
                namespace: None,
                ips: None,
                mac: None,
                interface: None,
                default_route: None,
            });
        }
    }
    (!selections.is_empty()).then(|| serde_json::to_string(&selections).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::super::super::params;
    use super::super::super::testkit::{self, apply, MockApi, TestCluster, CONTROLLER};
    use super::*;
    use crate::kubernetes::crds::GatewayParameters;
    use gateway_api::gatewayclasses::GatewayClassParametersRef;
    use gateway_api::gateways::{GatewayInfrastructure, GatewayInfrastructureParametersRef};

    /// A Gateway `name` in `edge` with these listeners and addresses.
    fn gateway(name: &str, listeners: serde_json::Value, addresses: serde_json::Value) -> Gateway {
        let mut gw = testkit::gateway(name, 1);
        gw.metadata.namespace = Some("edge".into());
        gw.spec.listeners = serde_json::from_value(listeners).unwrap();
        gw.spec.addresses = serde_json::from_value(addresses).unwrap();
        gw
    }

    /// The objects of `gw` under class parameters `class` and Gateway
    /// parameters `own` (YAML specs), with no Service live yet.
    async fn objects_of(
        gw: &Gateway,
        class: Option<&str>,
        own: Option<&str>,
    ) -> Vec<serde_json::Value> {
        objects_for(gw, class, own, &ControllerEndpoint::for_tests()).await
    }

    async fn objects_for(
        gw: &Gateway,
        class: Option<&str>,
        own: Option<&str>,
        controller: &ControllerEndpoint,
    ) -> Vec<serde_json::Value> {
        let mut cluster = TestCluster::default();
        let mut gc = testkit::gateway_class("portail", CONTROLLER);
        let mut gw = gw.clone();
        let mut add = |namespace: &str, name: &str, yaml: &str| {
            let mut p = GatewayParameters::new(name, serde_yaml::from_str(yaml).unwrap());
            p.metadata.namespace = Some(namespace.into());
            apply(&mut cluster.parameters, p);
        };
        if let Some(yaml) = class {
            add("portail-system", "class", yaml);
            gc.spec.parameters_ref = Some(GatewayClassParametersRef {
                group: "portail.epheo.eu".into(),
                kind: "GatewayParameters".into(),
                name: "class".into(),
                namespace: Some("portail-system".into()),
            });
        }
        if let Some(yaml) = own {
            add("edge", "own", yaml);
            gw.spec.infrastructure = Some(GatewayInfrastructure {
                parameters_ref: Some(GatewayInfrastructureParametersRef {
                    group: "portail.epheo.eu".into(),
                    kind: "GatewayParameters".into(),
                    name: "own".into(),
                }),
                ..Default::default()
            });
        }
        apply(&mut cluster.gateway_classes, gc);
        let ctx = cluster.ctx(MockApi::default().client());
        let params = params::resolve(&ctx, &gw).unwrap();
        let (objects, _) = desired(&gw, &params, &BTreeMap::new(), controller);
        let mut out = vec![
            serde_json::to_value(&objects.service_account).unwrap(),
            serde_json::to_value(&objects.deployment).unwrap(),
        ];
        out.extend(
            objects
                .service
                .iter()
                .map(|s| serde_json::to_value(s).unwrap()),
        );
        out.extend(objects.pdb.iter().map(|p| serde_json::to_value(p).unwrap()));
        out
    }

    /// Compare with goldens/`case`.yaml; PORTAIL_UPDATE_GOLDENS=1 rewrites it.
    fn golden(case: &str, objects: &[serde_json::Value]) {
        let path = format!(
            "{}/src/kubernetes/controller/provision/goldens/{case}.yaml",
            env!("CARGO_MANIFEST_DIR")
        );
        let rendered = objects
            .iter()
            .map(|o| serde_yaml::to_string(o).unwrap())
            .collect::<Vec<_>>()
            .join("---\n");
        if std::env::var_os("PORTAIL_UPDATE_GOLDENS").is_some() {
            std::fs::write(&path, &rendered).unwrap();
            return;
        }
        let expected = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            expected == rendered,
            "{path} differs; PORTAIL_UPDATE_GOLDENS=1 rewrites it.\nrendered:\n{rendered}"
        );
    }

    fn http_https() -> serde_json::Value {
        serde_json::json!([
            {"name": "http", "port": 80, "protocol": "HTTP"},
            {"name": "https", "port": 443, "protocol": "HTTPS",
             "tls": {"certificateRefs": [{"name": "cert"}]}},
        ])
    }

    /// No parameters: one replica behind a LoadBalancer on unprivileged
    /// targets, no PDB, no capability.
    #[tokio::test]
    async fn golden_loadbalancer_one_replica() {
        let gw = gateway("public", http_https(), serde_json::Value::Null);
        golden("loadbalancer-1", &objects_of(&gw, None, None).await);
    }

    /// Two replicas: a PDB and a soft spread across nodes.
    #[tokio::test]
    async fn golden_loadbalancer_two_replicas() {
        let gw = gateway("public", http_https(), serde_json::Value::Null);
        golden(
            "loadbalancer-2",
            &objects_of(&gw, None, Some("replicas: 2")).await,
        );
    }

    /// A ClusterIP class with a smaller cpu limit and an image.
    #[tokio::test]
    async fn golden_clusterip() {
        let gw = gateway("internal", http_https(), serde_json::Value::Null);
        golden(
            "clusterip",
            &objects_of(
                &gw,
                Some(
                    "service: {type: ClusterIP}
image: registry.local/portail:0.2.0
resources: {limits: {cpu: 500m, memory: 128Mi}, requests: {cpu: 50m, memory: 32Mi}}
",
                ),
                None,
            )
            .await,
        );
    }

    /// konstruct-shaped: a static IP on a localnet: Recreate, no shutdown
    /// delay, no Service, no PDB, NET_BIND_SERVICE for 80 and 443.
    #[tokio::test]
    async fn golden_static_ip() {
        let gw = gateway(
            "public",
            serde_json::json!([
                {"name": "https", "port": 443, "protocol": "HTTPS",
                 "tls": {"certificateRefs": [{"name": "cert"}]}},
                {"name": "http", "port": 80, "protocol": "HTTP"},
                {"name": "ssh", "port": 2223, "protocol": "TCP"},
            ]),
            serde_json::json!([{"type": "IPAddress", "value": "172.16.87.21"}]),
        );
        golden(
            "static-ip",
            &objects_of(
                &gw,
                None,
                Some(
                    "networks:
- {name: br-ex-network, namespace: default, ips: [172.16.87.21/24], gateway: [172.16.87.1]}
",
                ),
            )
            .await,
        );
    }

    /// A Gateway on a UDN by Network address: Multus annotation, no
    /// Service, no capability on 8080.
    #[tokio::test]
    async fn golden_udn_network_address() {
        let gw = gateway(
            "portail",
            serde_json::json!([{"name": "http", "port": 8080, "protocol": "HTTP"}]),
            serde_json::json!([{"type": NETWORK_ADDRESS_TYPE, "value": "udn-a"}]),
        );
        golden("udn", &objects_of(&gw, None, None).await);
    }

    /// APIServerProxy: the transport flag, an API-audience token and the
    /// cluster CA in a second projected volume, still no automounted
    /// token.
    #[tokio::test]
    async fn golden_apiserver_proxy() {
        let gw = gateway(
            "svc-gw",
            serde_json::json!([{"name": "http", "port": 8080, "protocol": "HTTP"}]),
            serde_json::Value::Null,
        );
        golden(
            "apiserver-proxy",
            &objects_of(
                &gw,
                None,
                Some("service: {type: ClusterIP}\ncontrollerTransport: APIServerProxy"),
            )
            .await,
        );
    }

    /// The mounts are where the data plane's defaults read its tokens and
    /// the apiserver's CA.
    #[test]
    fn the_mounts_match_the_data_plane_defaults() {
        use clap::Parser;
        let args = crate::cli::Args::try_parse_from([
            "portail",
            "dataplane",
            "--gateway=ns/gw",
            "--controller-address=https://ctl.ns.svc",
        ])
        .unwrap();
        let Some(crate::cli::Command::Dataplane(args)) = args.command else {
            panic!("data plane args");
        };
        let under = |dir: &str, file: &str| std::path::Path::new(dir).join(file);
        assert_eq!(args.token_file, under(TOKEN_DIR, TOKEN_FILE));
        assert_eq!(args.apiserver_token_file, under(APISERVER_DIR, TOKEN_FILE));
        assert_eq!(args.apiserver_ca_file, under(APISERVER_DIR, CA_FILE));
    }

    fn deployment_of(objects: &[serde_json::Value]) -> &serde_json::Value {
        objects.iter().find(|o| o["kind"] == "Deployment").unwrap()
    }

    fn added_capabilities(objects: &[serde_json::Value]) -> serde_json::Value {
        deployment_of(objects)["spec"]["template"]["spec"]["containers"][0]["securityContext"]
            ["capabilities"]["add"]
            .clone()
    }

    /// NET_BIND_SERVICE iff a bound port is below 1024: behind a Service
    /// none is; without one the published port is bound.
    #[tokio::test]
    async fn net_bind_service_iff_a_bound_port_is_privileged() {
        let gw = gateway("gw", http_https(), serde_json::Value::Null);
        assert!(added_capabilities(&objects_of(&gw, None, None).await).is_null());
        let none = objects_of(&gw, None, Some("service: {type: None}")).await;
        assert_eq!(
            added_capabilities(&none),
            serde_json::json!(["NET_BIND_SERVICE"])
        );

        let high = gateway(
            "gw",
            serde_json::json!([{"name": "http", "port": 8080, "protocol": "HTTP"}]),
            serde_json::Value::Null,
        );
        let none = objects_of(&high, None, Some("service: {type: None}")).await;
        assert!(added_capabilities(&none).is_null());
    }

    /// A pod started while the controller is down stays NotReady instead
    /// of being killed in a loop: startup waits on liveness, readiness on
    /// the snapshot.
    #[tokio::test]
    async fn startup_waits_on_liveness_not_readiness() {
        let gw = gateway("gw", http_https(), serde_json::Value::Null);
        let objects = objects_of(&gw, None, None).await;
        let container = &deployment_of(&objects)["spec"]["template"]["spec"]["containers"][0];
        assert_eq!(container["startupProbe"]["httpGet"]["path"], "/livez");
        assert_eq!(container["startupProbe"]["periodSeconds"], 1);
        assert_eq!(container["startupProbe"]["failureThreshold"], 30);
        assert_eq!(container["readinessProbe"]["httpGet"]["path"], "/readyz");
    }

    /// The live Service's targets stay with their listeners.
    #[tokio::test]
    async fn existing_targets_are_kept() {
        let gw = gateway("gw", http_https(), serde_json::Value::Null);
        let cluster = TestCluster::default();
        let ctx = cluster.ctx(MockApi::default().client());
        let params = params::resolve(&ctx, &gw).unwrap();
        let existing = [((Transport::Tcp, 443), 8000)].into_iter().collect();
        let (objects, bound) = desired(&gw, &params, &existing, &ControllerEndpoint::for_tests());
        assert_eq!(bound[&(Transport::Tcp, 443)], 8000);
        assert_eq!(bound[&(Transport::Tcp, 80)], 8001);
        let ports = objects.service.unwrap().spec.unwrap().ports.unwrap();
        assert_eq!(ports[0].target_port, Some(IntOrString::Int(8001)));
    }

    /// A Network address already among `networks` is not listed twice;
    /// one in another namespace is another network.
    #[test]
    fn multus_lists_each_network_once() {
        let mut gw = gateway(
            "gw",
            serde_json::json!([]),
            serde_json::json!([
                {"type": NETWORK_ADDRESS_TYPE, "value": "udn-a"},
                {"type": NETWORK_ADDRESS_TYPE, "value": "udn-b"},
            ]),
        );
        gw.metadata.namespace = Some("edge".into());
        let networks = vec![
            NetworkSelection {
                name: "udn-a".into(),
                namespace: Some("edge".into()),
                ..Default::default()
            },
            NetworkSelection {
                name: "udn-b".into(),
                namespace: Some("other".into()),
                ..Default::default()
            },
        ];
        assert_eq!(
            multus_networks(&networks, &gw).unwrap(),
            r#"[{"name":"udn-a","namespace":"edge"},{"name":"udn-b","namespace":"other"},{"name":"udn-b"}]"#
        );
        assert_eq!(
            multus_networks(
                &[],
                &gateway("gw", serde_json::json!([]), serde_json::Value::Null)
            ),
            None
        );
    }

    /// Any controllerName gets a valid label value; two names never share
    /// one.
    #[test]
    fn an_install_label_for_every_controller_name() {
        assert_eq!(install_label("portail-a"), "portail-a");
        let default = install_label("portail.epheo.eu/gateway-controller");
        assert!(
            default.starts_with("portail.epheo.eu-gateway-controller-"),
            "{default}"
        );
        let long = format!("example.com/{}", "x".repeat(80));
        for name in ["a/b", "a-b", "example.com/~weird%path!", long.as_str(), "/"] {
            let value = install_label(name);
            assert!(value.len() <= 63, "{value}");
            assert!(
                value
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
                    && value.starts_with(|c: char| c.is_ascii_alphanumeric())
                    && value.ends_with(|c: char| c.is_ascii_alphanumeric()),
                "{name}: {value}"
            );
        }
        assert_ne!(install_label("a/b"), install_label("a-b"));
    }

    fn controller_args() -> ControllerArgs {
        use clap::Parser;
        let args = crate::cli::Args::try_parse_from([
            "portail",
            "controller",
            "--namespace",
            "portail-system",
        ])
        .unwrap();
        let Some(crate::cli::Command::Controller(args)) = args.command else {
            panic!("controller args");
        };
        args
    }

    #[test]
    fn the_endpoint_is_the_controller_service() {
        let ca = ControllerCa::generated("portail-system", "portail-controller");
        let endpoint = ControllerEndpoint::new(&controller_args(), &ca);
        assert_eq!(
            endpoint.address,
            "https://portail-controller.portail-system.svc:9443"
        );
        assert_eq!(endpoint.audience, "portail-controller.portail-system.svc");
    }

    /// A Managed data plane verifies the controller against this PEM: an
    /// empty one fails every handshake.
    #[tokio::test]
    async fn a_managed_deployment_carries_the_controller_ca() {
        let ca = ControllerCa::generated("portail-system", "portail-controller");
        let endpoint = ControllerEndpoint::new(&controller_args(), &ca);
        let gw = gateway("public", http_https(), serde_json::Value::Null);
        let objects = objects_for(&gw, None, None, &endpoint).await;
        let env = deployment_of(&objects)["spec"]["template"]["spec"]["containers"][0]["env"]
            .as_array()
            .unwrap()
            .clone();
        let pem = env
            .iter()
            .find(|e| e["name"] == CONTROLLER_CA_ENV)
            .and_then(|e| e["value"].as_str())
            .unwrap();
        assert!(pem.starts_with("-----BEGIN CERTIFICATE-----"), "{pem}");
        assert_eq!(pem, ca.ca_pem());
    }
}
