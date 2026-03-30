use arc_swap::ArcSwap;
use futures::StreamExt;
use k8s_openapi::api::core::v1::{Namespace, Secret, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::api::Api;
use kube::runtime::controller::Action;
use kube::runtime::reflector::{ObjectRef, Store};
use kube::runtime::watcher;
use kube::runtime::{predicates, reflector, Controller, WatchStreamExt};
use kube::Client;
use kube::ResourceExt;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use gateway_api::experimental::tcproutes::TCPRoute;
use gateway_api::experimental::tlsroutes::TLSRoute;
use gateway_api::experimental::udproutes::UDPRoute;
use gateway_api::gatewayclasses::GatewayClass;
use gateway_api::gateways::Gateway;
use gateway_api::httproutes::HTTPRoute;
use gateway_api::referencegrants::ReferenceGrant;

use crate::logging::{debug, error, info, warn};
use crate::routing::RouteTable;

use super::parent_ref::ParentRefAccess;
use super::reconciler::{reconcile_to_config, ClusterSnapshot, ServiceState};
use super::reference_grants::is_reference_allowed;
use super::status;

#[derive(Debug)]
#[allow(dead_code)]
enum ReconcileError {
    Kube(kube::Error),
    Reconcile(String),
}

impl std::fmt::Display for ReconcileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Kube(e) => write!(f, "Kubernetes API error: {}", e),
            Self::Reconcile(msg) => write!(f, "Reconciliation error: {}", msg),
        }
    }
}

impl std::error::Error for ReconcileError {}

impl From<kube::Error> for ReconcileError {
    fn from(e: kube::Error) -> Self {
        Self::Kube(e)
    }
}

/// Semantic grouping of reflector stores — cached local copies of cluster resources.
#[derive(Clone)]
struct ResourceCache {
    gateways: Store<Gateway>,
    gateway_classes: Store<GatewayClass>,
    http_routes: Store<HTTPRoute>,
    tcp_routes: Store<TCPRoute>,
    tls_routes: Store<TLSRoute>,
    udp_routes: Store<UDPRoute>,
    namespaces: Store<Namespace>,
    services: Store<Service>,
    endpoint_slices: Store<EndpointSlice>,
    secrets: Store<Secret>,
    reference_grants: Store<ReferenceGrant>,
}

/// Read all objects from a reflector store, cloning each.
fn snapshot<K>(store: &Store<K>) -> Vec<K>
where
    K: kube::Resource + Clone,
    K::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
    store.state().iter().map(|arc| (**arc).clone()).collect()
}

/// Addresses routable to this gateway instance, split by source for priority.
pub(crate) struct UsableAddresses {
    /// LoadBalancer ingress IPs (externally reachable)
    pub lb_ips: Vec<String>,
    /// NODE_IP / POD_IP from env vars (DaemonSet + hostNetwork)
    pub env_ips: Vec<String>,
}

impl UsableAddresses {
    /// All known addresses as a set (for membership checks).
    pub fn all(&self) -> HashSet<&str> {
        self.lb_ips
            .iter()
            .chain(&self.env_ips)
            .map(|s| s.as_str())
            .collect()
    }

    /// Best externally-reachable address: LB VIP first, then NODE_IP/POD_IP.
    pub fn preferred_ip(&self) -> &str {
        self.lb_ips
            .first()
            .or(self.env_ips.first())
            .map(|s| s.as_str())
            .unwrap_or("0.0.0.0")
    }

    pub fn is_empty(&self) -> bool {
        self.lb_ips.is_empty() && self.env_ips.is_empty()
    }
}

/// Discover addresses routable to this pod via EndpointSlice back-reference.
///
/// 1. Find EndpointSlices whose endpoints contain our POD_IP
/// 2. Resolve the owning Service via `kubernetes.io/service-name` label
/// 3. Extract LoadBalancer ingress IPs from those Services
/// 4. Fall back to NODE_IP / POD_IP env vars (DaemonSet + hostNetwork)
pub(crate) fn discover_usable_addresses(
    services: &Store<Service>,
    endpoint_slices: &Store<EndpointSlice>,
) -> UsableAddresses {
    let pod_ip = std::env::var("POD_IP").unwrap_or_default();

    let my_ns = std::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "default".to_string());

    // Find Services that route to this pod via EndpointSlice back-reference
    let my_service_names: HashSet<String> = if !pod_ip.is_empty() {
        endpoint_slices
            .state()
            .iter()
            .filter(|eps| eps.metadata.namespace.as_deref() == Some(&my_ns))
            .filter(|eps| {
                eps.endpoints
                    .iter()
                    .any(|ep| ep.addresses.iter().any(|addr| addr == &pod_ip))
            })
            .filter_map(|eps| {
                eps.metadata
                    .labels
                    .as_ref()?
                    .get("kubernetes.io/service-name")
                    .cloned()
            })
            .collect()
    } else {
        HashSet::new()
    };

    // Get LB ingress IPs from those Services
    let lb_ips: Vec<String> = services
        .state()
        .iter()
        .filter(|svc| svc.metadata.namespace.as_deref() == Some(&my_ns))
        .filter(|svc| {
            svc.metadata
                .name
                .as_deref()
                .is_some_and(|name| my_service_names.contains(name))
        })
        .filter_map(|svc| {
            svc.status
                .as_ref()?
                .load_balancer
                .as_ref()?
                .ingress
                .as_ref()
        })
        .flatten()
        .filter_map(|ingress| ingress.ip.clone())
        .collect();

    // Env var fallback (DaemonSet + hostNetwork)
    let mut env_ips = Vec::new();
    if let Ok(ip) = std::env::var("NODE_IP") {
        env_ips.push(ip);
    }
    if let Ok(ip) = std::env::var("POD_IP") {
        env_ips.push(ip);
    }

    UsableAddresses { lb_ips, env_ips }
}

#[derive(Clone)]
struct ControllerCtx {
    client: Client,
    controller_name: String,
    cache: ResourceCache,
    /// Per-gateway config cache — avoids re-reconciling other gateways.
    /// Populated during reconcile, pruned against the gateway store.
    config_cache: Arc<std::sync::Mutex<HashMap<(String, String), crate::config::PortailConfig>>>,
    routes: Arc<ArcSwap<RouteTable>>,
    data_plane: Arc<std::sync::Mutex<crate::data_plane::DataPlane>>,
    performance_config: crate::config::PerformanceConfig,
}

/// Create a reflector for a resource type, returning the store and a stream.
/// The stream is consumed by `Controller::watches_stream()` which both drives
/// the reflector (keeping the store up to date) and triggers reconciliation
/// of mapped Gateway(s) when resources change.
fn create_reflector<K>(
    api: Api<K>,
) -> (
    Store<K>,
    impl futures::Stream<Item = Result<K, watcher::Error>> + Send + 'static,
)
where
    K: kube::Resource
        + Clone
        + std::fmt::Debug
        + serde::de::DeserializeOwned
        + Send
        + Sync
        + 'static,
    K::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
    let writer = reflector::store::Writer::default();
    let reader = writer.as_reader();
    let stream = reflector::reflector(writer, watcher::watcher(api, watcher::Config::default()))
        .default_backoff()
        .touched_objects(); // touched_objects includes deletes; applied_objects drops them
    (reader, stream)
}

// ---------------------------------------------------------------------------
// Mapper functions: map secondary resource events to Gateway ObjectRef(s)
// ---------------------------------------------------------------------------

/// Map a route to the Gateway(s) it targets via parentRefs.
fn map_route_to_gateways<R: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope>>(
    route: &R,
    parent_refs: &Option<Vec<impl ParentRefAccess>>,
    store: &Store<Gateway>,
) -> Vec<ObjectRef<Gateway>> {
    let route_ns = route.namespace().unwrap_or_default();
    let mut refs = Vec::new();
    if let Some(prs) = parent_refs {
        for pr in prs {
            if pr
                .ref_group()
                .is_some_and(|g| g != "gateway.networking.k8s.io")
            {
                continue;
            }
            if pr.ref_kind().is_some_and(|k| k != "Gateway") {
                continue;
            }
            let gw_ns = pr.ref_namespace().unwrap_or(&route_ns);
            let obj_ref = ObjectRef::<Gateway>::new(pr.ref_name()).within(gw_ns);
            if store.get(&obj_ref).is_some() {
                refs.push(obj_ref);
            }
        }
    }
    refs
}

/// Map any resource to ALL managed gateways (used for infrequently-changing
/// resources like Service, Namespace, ReferenceGrant where targeted mapping
/// adds complexity for negligible CPU gain).
fn all_gateway_refs(store: &Store<Gateway>) -> Vec<ObjectRef<Gateway>> {
    store
        .state()
        .into_iter()
        .map(|gw| ObjectRef::from_obj(&*gw))
        .collect()
}

pub async fn run_controller(
    routes: Arc<ArcSwap<RouteTable>>,
    controller_name: String,
    shutdown: CancellationToken,
    data_plane: Arc<std::sync::Mutex<crate::data_plane::DataPlane>>,
    performance_config: crate::config::PerformanceConfig,
) -> anyhow::Result<()> {
    let client = Client::try_default().await?;
    info!("Kubernetes client connected, starting Gateway API controller");

    // --- Primary resource: Gateway ---
    // predicate_filter(predicates::generation) filters status-only changes,
    // breaking the reconcile → status patch → watch event feedback loop.
    let gw_writer = reflector::store::Writer::default();
    let store_gateways = gw_writer.as_reader();
    let gw_stream = reflector::reflector(
        gw_writer,
        watcher::watcher(
            Api::<Gateway>::all(client.clone()),
            watcher::Config::default(),
        ),
    )
    .default_backoff()
    .applied_objects()
    .predicate_filter(predicates::generation);

    // --- Secondary resources ---
    // Each resource gets a reflector (store + stream). The stream feeds into
    // Controller::watches_stream() with a mapper that targets affected Gateway(s).
    // This is the standard kube-rs/controller-runtime pattern: secondary resource
    // events go through a separate path unaffected by the primary predicate_filter.

    // CRD reflectors — generation-filtered to ignore status-only changes
    let (store_http_routes, http_route_stream) =
        create_reflector(Api::<HTTPRoute>::all(client.clone()));
    let (store_tcp_routes, tcp_route_stream) =
        create_reflector(Api::<TCPRoute>::all(client.clone()));
    let (store_tls_routes, tls_route_stream) =
        create_reflector(Api::<TLSRoute>::all(client.clone()));
    let (store_udp_routes, udp_route_stream) =
        create_reflector(Api::<UDPRoute>::all(client.clone()));
    // GatewayClass: managed by a separate Controller (below).
    // We create that controller early to get its store for ControllerCtx lookups.
    let gc_api = Api::<GatewayClass>::all(client.clone());
    let gc_controller = Controller::new(gc_api, watcher::Config::default());
    let store_gateway_classes = gc_controller.store();

    // Core resource reflectors — no generation filter (core types don't reliably set it)
    let (store_namespaces, namespace_stream) =
        create_reflector(Api::<Namespace>::all(client.clone()));
    let (store_services, service_stream) = create_reflector(Api::<Service>::all(client.clone()));
    let (store_endpoint_slices, endpoint_slice_stream) =
        create_reflector(Api::<EndpointSlice>::all(client.clone()));
    let (store_reference_grants, reference_grant_stream) =
        create_reflector(Api::<ReferenceGrant>::all(client.clone()));

    // TLS secrets — field-filtered to avoid watching all secrets
    let (store_secrets, secret_stream) = {
        let writer = reflector::store::Writer::default();
        let reader = writer.as_reader();
        let stream = reflector::reflector(
            writer,
            watcher::watcher(
                Api::<Secret>::all(client.clone()),
                watcher::Config::default().fields("type=kubernetes.io/tls"),
            ),
        )
        .default_backoff()
        .touched_objects();
        (reader, stream)
    };

    let store_gateways_for_controller = store_gateways.clone();

    let ctx = Arc::new(ControllerCtx {
        client: client.clone(),
        controller_name: controller_name.clone(),
        config_cache: Arc::new(std::sync::Mutex::new(HashMap::new())),
        cache: ResourceCache {
            gateways: store_gateways,
            gateway_classes: store_gateway_classes.clone(),
            http_routes: store_http_routes,
            tcp_routes: store_tcp_routes,
            tls_routes: store_tls_routes,
            udp_routes: store_udp_routes,
            namespaces: store_namespaces,
            services: store_services,
            endpoint_slices: store_endpoint_slices,
            secrets: store_secrets,
            reference_grants: store_reference_grants,
        },
        routes,
        data_plane,
        performance_config,
    });

    // --- Gateway controller ---
    // Primary: Gateway stream with generation predicate (breaks feedback loop).
    // Secondary: watches_stream() per resource type with targeted mappers.
    //
    // Routes → map to parent Gateway(s) via parentRefs (targeted)
    // Secret → map to Gateways referencing it in TLS config (targeted)
    // EndpointSlice → map via Service→Route→Gateway (targeted)
    // Service, Namespace, ReferenceGrant → all Gateways (broad, infrequent changes)

    // Each watches_stream closure captures an owned gateway store clone via move.
    let [gw1, gw2, gw3, gw4, gw5, gw6, gw7, gw8] =
        std::array::from_fn::<_, 8, _>(|_| store_gateways_for_controller.clone());

    let gw_controller = Controller::for_stream(gw_stream, store_gateways_for_controller)
        // Routes: targeted — only reconcile the Gateway(s) referenced in parentRefs.
        // No predicate_filter here: it caches generation per object key and never
        // clears on delete, so delete+recreate with the same name (common in
        // conformance tests) would be suppressed. The status feedback loop is
        // already broken by the primary Gateway stream's predicate filter.
        .watches_stream(http_route_stream, move |route: HTTPRoute| {
            map_route_to_gateways(&route, &route.spec.parent_refs, &gw1)
        })
        .watches_stream(tcp_route_stream, move |route: TCPRoute| {
            map_route_to_gateways(&route, &route.spec.parent_refs, &gw2)
        })
        .watches_stream(tls_route_stream, move |route: TLSRoute| {
            map_route_to_gateways(&route, &route.spec.parent_refs, &gw3)
        })
        .watches_stream(udp_route_stream, move |route: UDPRoute| {
            map_route_to_gateways(&route, &route.spec.parent_refs, &gw4)
        })
        // Secret: targeted — only reconcile Gateways referencing this secret in TLS
        .watches_stream(
            secret_stream,
            move |secret: Secret| -> Vec<ObjectRef<Gateway>> {
                let secret_name = secret.metadata.name.as_deref().unwrap_or("");
                let secret_ns = secret.metadata.namespace.as_deref().unwrap_or("default");
                gw5.state()
                    .into_iter()
                    .filter(|g| {
                        let gw_ns = g.metadata.namespace.as_deref().unwrap_or("default");
                        g.spec.listeners.iter().any(|l| {
                            l.tls.as_ref().is_some_and(|tls| {
                                tls.certificate_refs.as_ref().is_some_and(|refs| {
                                    refs.iter().any(|cr| {
                                        cr.name == secret_name
                                            && cr.namespace.as_deref().unwrap_or(gw_ns) == secret_ns
                                    })
                                })
                            })
                        })
                    })
                    .map(|g| ObjectRef::from_obj(&*g))
                    .collect()
            },
        )
        // EndpointSlice, Service, Namespace, ReferenceGrant: broad — reconcile all
        // Gateways. These have complex multi-hop mappings and the 1s debounce
        // coalesces frequent EndpointSlice events from pod scaling/readiness.
        .watches_stream(endpoint_slice_stream, move |_: EndpointSlice| {
            all_gateway_refs(&gw6)
        })
        .watches_stream(service_stream, move |_: Service| all_gateway_refs(&gw7))
        .watches_stream(namespace_stream, move |_: Namespace| all_gateway_refs(&gw8))
        .watches_stream(reference_grant_stream, {
            let gw = ctx.cache.gateways.clone();
            move |_: ReferenceGrant| all_gateway_refs(&gw)
        })
        .with_config(kube::runtime::controller::Config::default().debounce(Duration::from_secs(1)))
        .shutdown_on_signal()
        .run(reconcile, error_policy, ctx);

    // --- GatewayClass controller ---
    // Separate controller: accepts/rejects GatewayClasses independently of Gateways.
    // Controller::new() manages its own reflector and store, guaranteeing the store
    // is populated (after InitDone) before any reconcile fires — no timing bugs.
    // The same store is shared with ControllerCtx for Gateway reconcile lookups,
    // eliminating the need for a separate GatewayClass reflector.
    let gc_ctx = Arc::new(GatewayClassCtx {
        client,
        controller_name,
        store: store_gateway_classes,
    });
    let gc_controller = gc_controller
        .with_config(kube::runtime::controller::Config::default().debounce(Duration::from_secs(1)))
        .shutdown_on_signal()
        .run(reconcile_gateway_class, gc_error_policy, gc_ctx);

    info!("Gateway API controller started, watching for resource changes");

    tokio::select! {
        _ = gw_controller.for_each(|result| async move {
            match result {
                Ok((_obj_ref, _action)) => {}
                Err(e) => warn!("Gateway reconciliation error: {}", e),
            }
        }) => {
            info!("Gateway controller stream ended");
        }
        _ = gc_controller.for_each(|result| async move {
            match result {
                Ok((_obj_ref, _action)) => {}
                Err(e) => warn!("GatewayClass reconciliation error: {}", e),
            }
        }) => {
            info!("GatewayClass controller stream ended");
        }
        _ = shutdown.cancelled() => {
            info!("Controller received shutdown signal");
        }
    }

    Ok(())
}

/// Resolve TLS certificates for a Gateway from the reflector Secret store.
/// Iterates all listeners, checks cross-namespace ReferenceGrant permissions,
/// looks up Secrets, and validates PEM format.
fn resolve_gateway_certs(
    gateway: &Gateway,
    gw_ns: &str,
    reference_grants: &[ReferenceGrant],
    store_secrets: &Store<Secret>,
) -> HashMap<(String, String), (Vec<u8>, Vec<u8>)> {
    let mut cert_data: HashMap<(String, String), (Vec<u8>, Vec<u8>)> = HashMap::new();
    for listener in &gateway.spec.listeners {
        let Some(tls) = &listener.tls else { continue };
        let Some(cert_refs) = &tls.certificate_refs else {
            continue;
        };
        for cert_ref in cert_refs {
            let secret_ns = cert_ref.namespace.as_deref().unwrap_or(gw_ns);

            // Cross-namespace cert ref requires a ReferenceGrant
            if secret_ns != gw_ns
                && !is_reference_allowed(
                    reference_grants,
                    "gateway.networking.k8s.io",
                    "Gateway",
                    gw_ns,
                    "",
                    "Secret",
                    secret_ns,
                    &cert_ref.name,
                )
            {
                warn!(
                    "Cross-namespace certificate ref {}/{} not allowed by ReferenceGrant",
                    secret_ns, cert_ref.name
                );
                continue;
            }

            // Look up secret from reflector cache
            let secret_ref = ObjectRef::<Secret>::new(&cert_ref.name).within(secret_ns);
            match store_secrets.get(&secret_ref) {
                Some(secret) => {
                    if let Some(data) = &secret.data {
                        let cert_pem = data.get("tls.crt").map(|b| b.0.clone());
                        let key_pem = data.get("tls.key").map(|b| b.0.clone());
                        if let (Some(cert), Some(key)) = (cert_pem, key_pem) {
                            let cert_str = String::from_utf8_lossy(&cert);
                            let key_str = String::from_utf8_lossy(&key);
                            if cert_str.contains("BEGIN CERTIFICATE")
                                && (key_str.contains("BEGIN PRIVATE KEY")
                                    || key_str.contains("BEGIN RSA PRIVATE KEY")
                                    || key_str.contains("BEGIN EC PRIVATE KEY"))
                            {
                                cert_data.insert(
                                    (cert_ref.name.clone(), secret_ns.to_string()),
                                    (cert, key),
                                );
                            } else {
                                warn!(
                                    "Secret {}/{} has malformed PEM data",
                                    secret_ns, cert_ref.name
                                );
                            }
                        } else {
                            warn!(
                                "Secret {}/{} missing tls.crt or tls.key",
                                secret_ns, cert_ref.name
                            );
                        }
                    }
                }
                None => {
                    debug!("Secret {}/{} not found in cache", secret_ns, cert_ref.name);
                }
            }
        }
    }
    cert_data
}

/// Resolve service metadata: known services set, headless endpoint overrides,
/// ExternalName overrides, and appProtocol overrides.
fn resolve_services(
    all_services: &[Service],
    store_endpoint_slices: &Store<EndpointSlice>,
) -> ServiceState {
    let known_services: HashSet<(String, String)> = all_services
        .iter()
        .filter_map(|svc| {
            let name = svc.metadata.name.as_ref()?;
            let ns = svc.metadata.namespace.as_deref().unwrap_or("default");
            Some((name.clone(), ns.to_string()))
        })
        .collect();

    // Detect headless services and fetch their EndpointSlices for pod IP + targetPort resolution.
    let mut endpoint_overrides: HashMap<(String, u16), Vec<(String, u16)>> = HashMap::new();

    let headless_services: Vec<(&str, &str)> = all_services
        .iter()
        .filter_map(|svc| {
            let spec = svc.spec.as_ref()?;
            let cluster_ip = spec.cluster_ip.as_deref().unwrap_or("");
            if cluster_ip == "None" || cluster_ip.is_empty() {
                let name = svc.metadata.name.as_deref()?;
                let ns = svc.metadata.namespace.as_deref().unwrap_or("default");
                Some((name, ns))
            } else {
                None
            }
        })
        .collect();

    if !headless_services.is_empty() {
        let all_endpoint_slices: Vec<EndpointSlice> = store_endpoint_slices
            .state()
            .iter()
            .map(|arc| (**arc).clone())
            .collect();

        for (svc_name, svc_ns) in &headless_services {
            let matching_slices: Vec<&EndpointSlice> = all_endpoint_slices
                .iter()
                .filter(|eps| {
                    let eps_ns = eps.metadata.namespace.as_deref().unwrap_or("default");
                    if eps_ns != *svc_ns {
                        return false;
                    }
                    eps.metadata
                        .labels
                        .as_ref()
                        .and_then(|l| l.get("kubernetes.io/service-name"))
                        .is_some_and(|v| v == *svc_name)
                })
                .collect();

            let svc_fqdn = format!("{}.{}.svc", svc_name, svc_ns);

            let svc_spec = all_services
                .iter()
                .find(|s| {
                    s.metadata.name.as_deref() == Some(svc_name)
                        && s.metadata.namespace.as_deref().unwrap_or("default") == *svc_ns
                })
                .and_then(|s| s.spec.as_ref());

            for eps in &matching_slices {
                if eps.address_type != "IPv4" {
                    continue;
                }
                let endpoints = &eps.endpoints;
                let eps_ports = eps.ports.as_deref().unwrap_or_default();

                if let Some(svc_ports) = svc_spec.and_then(|s| s.ports.as_ref()) {
                    for sp in svc_ports {
                        let service_port = sp.port as u16;
                        let port_name = sp.name.as_deref().unwrap_or("");
                        let target_port = eps_ports
                            .iter()
                            .find(|ep| ep.name.as_deref().unwrap_or("") == port_name)
                            .and_then(|ep| ep.port)
                            .unwrap_or(service_port as i32)
                            as u16;

                        let key = (svc_fqdn.clone(), service_port);
                        let entry = endpoint_overrides.entry(key).or_default();
                        for endpoint in endpoints {
                            let is_ready = endpoint
                                .conditions
                                .as_ref()
                                .and_then(|c| c.ready)
                                .unwrap_or(true);
                            if is_ready {
                                for addr in &endpoint.addresses {
                                    entry.push((addr.clone(), target_port));
                                }
                            }
                        }
                    }
                }
            }
        }

        if !endpoint_overrides.is_empty() {
            info!(
                "Found {} headless endpoint overrides for {} services",
                endpoint_overrides.values().map(|v| v.len()).sum::<usize>(),
                endpoint_overrides.len(),
            );
        }
    }

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

    ServiceState {
        known_services,
        endpoint_overrides,
        app_protocol_overrides,
    }
}

/// Validate Gateway listeners and compute gateway-level acceptance.
/// Returns (listener_statuses, gateway_accepted, reason, message).
fn validate_gateway(
    gateway: &Gateway,
    gw_ns: &str,
    cert_data: &HashMap<(String, String), (Vec<u8>, Vec<u8>)>,
    reference_grants: &[ReferenceGrant],
) -> (
    HashMap<String, status::ListenerStatus>,
    bool,
    String,
    String,
) {
    let mut listener_statuses: HashMap<String, status::ListenerStatus> = HashMap::new();

    // Detect protocol conflicts: same port, different protocols
    let mut port_protocols: HashMap<i32, String> = HashMap::new();
    let mut conflicted_ports: HashSet<i32> = HashSet::new();
    for l in &gateway.spec.listeners {
        match port_protocols.get(&l.port) {
            Some(existing_proto) if *existing_proto != l.protocol => {
                conflicted_ports.insert(l.port);
            }
            None => {
                port_protocols.insert(l.port, l.protocol.clone());
            }
            _ => {}
        }
    }

    for listener in &gateway.spec.listeners {
        let mut ls = status::ListenerStatus::default();
        let mut refs_failed = false;

        if conflicted_ports.contains(&listener.port) {
            ls.accepted = false;
            ls.accepted_reason = "ProtocolConflict".into();
            ls.accepted_message =
                "Listener port conflicts with another listener using a different protocol".into();
            ls.conflicted = true;
            ls.conflicted_reason = "ProtocolConflict".into();
            ls.conflicted_message =
                "Listener port shared with another listener using different protocol".into();
        } else {
            ls.accepted = true;
            ls.accepted_reason = "Accepted".into();
            ls.accepted_message = "Listener accepted".into();
        }

        // Validate TLS certificateRefs for HTTPS/TLS listeners
        if let Some(tls) = &listener.tls {
            if let Some(cert_refs) = &tls.certificate_refs {
                for cert_ref in cert_refs {
                    let group = cert_ref.group.as_deref().unwrap_or("");
                    let kind_str = cert_ref.kind.as_deref().unwrap_or("Secret");
                    if !group.is_empty() || kind_str != "Secret" {
                        refs_failed = true;
                        ls.resolved_refs_reason = "InvalidCertificateRef".into();
                        ls.resolved_refs_message = format!(
                            "Unsupported certificateRef group/kind: {}/{}",
                            group, kind_str
                        );
                        continue;
                    }

                    let secret_ns = cert_ref.namespace.as_deref().unwrap_or(gw_ns);

                    // Cross-namespace cert ref requires ReferenceGrant
                    if secret_ns != gw_ns
                        && !is_reference_allowed(
                            reference_grants,
                            "gateway.networking.k8s.io",
                            "Gateway",
                            gw_ns,
                            "",
                            "Secret",
                            secret_ns,
                            &cert_ref.name,
                        )
                    {
                        refs_failed = true;
                        ls.resolved_refs_reason = "RefNotPermitted".into();
                        ls.resolved_refs_message = format!(
                            "Cross-namespace certificate ref {}/{} not allowed by ReferenceGrant",
                            secret_ns, cert_ref.name
                        );
                        continue;
                    }

                    if !cert_data.contains_key(&(cert_ref.name.clone(), secret_ns.to_string())) {
                        refs_failed = true;
                        ls.resolved_refs_reason = "InvalidCertificateRef".into();
                        ls.resolved_refs_message = format!(
                            "Secret {}/{} not found or missing tls.crt/tls.key",
                            secret_ns, cert_ref.name
                        );
                    }
                }
            }
        } else if matches!(listener.protocol.as_str(), "HTTPS" | "TLS") {
            refs_failed = true;
            ls.resolved_refs_reason = "InvalidCertificateRef".into();
            ls.resolved_refs_message = "HTTPS/TLS listener requires TLS configuration".into();
        }

        // Check allowedRoutes.kinds validity and compute supportedKinds
        if let Some(allowed_routes) = &listener.allowed_routes {
            if let Some(kinds) = &allowed_routes.kinds {
                let valid_kinds = ["HTTPRoute", "TCPRoute", "TLSRoute", "UDPRoute", "GRPCRoute"];
                let mut supported = Vec::new();
                let mut has_invalid = false;
                for k in kinds {
                    let group = k.group.as_deref().unwrap_or("gateway.networking.k8s.io");
                    let kind_str = &k.kind;
                    if group == "gateway.networking.k8s.io"
                        && valid_kinds.contains(&kind_str.as_str())
                    {
                        supported.push(serde_json::json!({
                            "group": "gateway.networking.k8s.io",
                            "kind": kind_str,
                        }));
                    } else {
                        has_invalid = true;
                    }
                }
                ls.supported_kinds = supported;
                if has_invalid {
                    refs_failed = true;
                    ls.resolved_refs_reason = "InvalidRouteKinds".into();
                    ls.resolved_refs_message =
                        "One or more route kinds in allowedRoutes are not supported".into();
                }
            } else {
                ls.supported_kinds = status::supported_kinds_for_protocol(&listener.protocol);
            }
        } else {
            ls.supported_kinds = status::supported_kinds_for_protocol(&listener.protocol);
        }

        if !refs_failed {
            ls.resolved_refs = true;
            ls.resolved_refs_reason = "ResolvedRefs".into();
            ls.resolved_refs_message = "All references resolved".into();
        }

        if ls.accepted && ls.resolved_refs {
            ls.programmed = true;
            ls.programmed_reason = "Programmed".into();
            ls.programmed_message = "Programmed".into();
        } else {
            ls.programmed = false;
            ls.programmed_reason = "Invalid".into();
            ls.programmed_message = if !ls.accepted {
                "Listener not programmed due to acceptance failure".into()
            } else {
                "Listener has unresolved references".into()
            };
        }

        listener_statuses.insert(listener.name.clone(), ls);
    }

    // Gateway-level acceptance
    let supported_protocols = ["HTTP", "HTTPS", "TLS", "TCP", "UDP"];
    let mut accepted = true;
    let mut reason = "Accepted".to_string();
    let mut message = "Gateway accepted by portail controller".to_string();

    for listener in &gateway.spec.listeners {
        if !supported_protocols.contains(&listener.protocol.as_str()) {
            accepted = false;
            reason = "InvalidParameters".to_string();
            message = format!(
                "Listener '{}' uses unsupported protocol '{}'",
                listener.name, listener.protocol
            );
            break;
        }
    }

    if accepted {
        if let Some(addresses) = &gateway.spec.addresses {
            for addr in addresses {
                let addr_type = addr.r#type.as_deref().unwrap_or("IPAddress");
                if addr_type != "IPAddress" && addr_type != "Hostname" {
                    accepted = false;
                    reason = "UnsupportedAddress".to_string();
                    message = format!("Unsupported address type '{}' in spec.addresses", addr_type);
                    break;
                }
            }
        }
    }

    if accepted && !listener_statuses.is_empty() {
        let all_rejected = listener_statuses.values().all(|ls| !ls.accepted);
        if all_rejected {
            accepted = false;
            reason = "InvalidListeners".to_string();
            message = "All listeners are rejected due to conflicts or errors".to_string();
        }
    }

    (listener_statuses, accepted, reason, message)
}

async fn reconcile(
    gateway: Arc<Gateway>,
    ctx: Arc<ControllerCtx>,
) -> Result<Action, ReconcileError> {
    let gw_name = gateway.name_any();
    let gw_ns = gateway.namespace().unwrap_or_else(|| "default".to_string());
    debug!("Reconciling Gateway {}/{}", gw_ns, gw_name);

    // Verify this Gateway's class is managed by us — read from store, no API call
    let gateway_class_name = &gateway.spec.gateway_class_name;
    let gc_ref = ObjectRef::<GatewayClass>::new(gateway_class_name);
    let gc = match ctx.cache.gateway_classes.get(&gc_ref) {
        Some(gc) => gc,
        None => {
            debug!(
                "GatewayClass '{}' not in store yet, requeueing",
                gateway_class_name
            );
            return Ok(Action::requeue(Duration::from_secs(1)));
        }
    };

    if gc.spec.controller_name != ctx.controller_name {
        debug!(
            "Gateway {}/{} references GatewayClass '{}' with controller '{}', not ours ('{}')",
            gw_ns, gw_name, gateway_class_name, gc.spec.controller_name, ctx.controller_name
        );
        return Ok(Action::await_change());
    }

    // Read all resources from reflector caches — no API server calls.
    let snapshot = ClusterSnapshot {
        http_routes: snapshot(&ctx.cache.http_routes),
        tcp_routes: snapshot(&ctx.cache.tcp_routes),
        tls_routes: snapshot(&ctx.cache.tls_routes),
        udp_routes: snapshot(&ctx.cache.udp_routes),
        namespace_labels: ctx
            .cache
            .namespaces
            .state()
            .iter()
            .filter_map(|ns| {
                let name = ns.metadata.name.clone()?;
                let labels = ns.metadata.labels.clone().unwrap_or_default();
                Some((name, labels))
            })
            .collect(),
        reference_grants: snapshot(&ctx.cache.reference_grants),
        services: snapshot(&ctx.cache.services),
    };

    let cert_data = resolve_gateway_certs(
        &gateway,
        &gw_ns,
        &snapshot.reference_grants,
        &ctx.cache.secrets,
    );
    let services = resolve_services(&snapshot.services, &ctx.cache.endpoint_slices);

    let (listener_statuses, gateway_accepted, gateway_accepted_reason, gateway_accepted_message) =
        validate_gateway(&gateway, &gw_ns, &cert_data, &snapshot.reference_grants);

    let usable = discover_usable_addresses(&ctx.cache.services, &ctx.cache.endpoint_slices);

    // Build config for the triggered Gateway
    let result = match reconcile_to_config(&gateway, &snapshot, &cert_data, &services) {
        Ok(r) => r,
        Err(e) => {
            error!("Failed to reconcile Gateway {}/{}: {}", gw_ns, gw_name, e);
            status::update_gateway_status(
                &ctx.client,
                &gateway,
                gateway_accepted,
                &gateway_accepted_reason,
                &gateway_accepted_message,
                false,
                "Invalid",
                &format!("Reconciliation failed: {}", e),
                &HashMap::new(),
                &listener_statuses,
                &usable,
            )
            .await;
            return Err(ReconcileError::Reconcile(e.to_string()));
        }
    };

    // Determine bind addresses: only use spec.addresses that are local to this pod.
    // LB VIPs are external routing identities handled by kube-proxy/MetalLB —
    // the pod binds to 0.0.0.0 for those ports and traffic arrives via
    // Service routing. DaemonSet+hostNetwork addresses ARE local and should
    // be bound specifically (interface isolation for multi-network setups).
    let bind_addresses: Vec<String> = result
        .config
        .gateway
        .addresses
        .iter()
        .filter(|addr| usable.env_ips.iter().any(|ip| ip == *addr))
        .cloned()
        .collect();

    // Dynamically open TCP listeners for ports defined in this Gateway's spec.
    // Done after reconciliation so ListenerConfig has TLS cert data populated.
    {
        let listener_configs = &result.config.gateway.listeners;
        info!(
            "Ensuring data plane listeners for ports: {:?}",
            listener_configs.iter().map(|l| l.port).collect::<Vec<_>>()
        );

        match ctx.data_plane.lock() {
            Ok(mut dp) => {
                let (opened, errors) = dp.add_tcp_listeners(
                    listener_configs,
                    &bind_addresses,
                    ctx.routes.clone(),
                    &ctx.performance_config,
                );
                if opened > 0 {
                    info!("Opened {} new port(s)", opened);
                }
                for (port, err) in &errors {
                    warn!("Failed to bind port {}: {}", port, err);
                }
            }
            Err(e) => {
                warn!("Failed to lock data plane: {}", e);
            }
        }
    }

    // Cache this gateway's config and build the global route table from all cached configs.
    // This avoids re-reconciling other gateways on every change (O(n) instead of O(n²)).
    let all_configs = {
        let mut cache = ctx.config_cache.lock().unwrap();
        cache.insert((gw_ns.clone(), gw_name.clone()), result.config.clone());
        // Prune configs for gateways no longer in the store or managed by a different controller
        let live: HashSet<_> = ctx
            .cache
            .gateways
            .state()
            .iter()
            .filter(|g| g.spec.gateway_class_name == *gateway_class_name)
            .map(|g| {
                (
                    g.metadata
                        .namespace
                        .as_deref()
                        .unwrap_or("default")
                        .to_string(),
                    g.metadata.name.as_deref().unwrap_or("").to_string(),
                )
            })
            .collect();
        cache.retain(|k, _| live.contains(k));
        // Apply fresh endpoint/appProtocol overrides to all cached configs.
        // These are global state (derived from Services/EndpointSlices), not per-gateway,
        // so cached configs must use the current values, not stale ones from their last reconcile.
        let mut configs: Vec<_> = cache.values().cloned().collect();
        for config in &mut configs {
            config.endpoint_overrides = services.endpoint_overrides.clone();
            config.app_protocol_overrides = services.app_protocol_overrides.clone();
        }
        configs
    };

    // Merge TLS cert refs from ALL gateways per port, then hot-reload.
    // This ensures the SNI resolver has certs from all gateways sharing a port
    // (e.g. public *.desku.be + private *.mmt + conformance test example.org on port 443).
    {
        use crate::config::{
            CertificateRef, ListenerConfig as LC, Protocol as P, TlsConfig, TlsMode,
        };
        let mut merged_certs_by_port: HashMap<u16, Vec<CertificateRef>> = HashMap::new();
        let mut port_protocol: HashMap<u16, P> = HashMap::new();
        for config in &all_configs {
            for listener in &config.gateway.listeners {
                if let Some(tls_cfg) = &listener.tls {
                    if tls_cfg.mode == TlsMode::Terminate && !tls_cfg.certificate_refs.is_empty() {
                        merged_certs_by_port
                            .entry(listener.port)
                            .or_default()
                            .extend(tls_cfg.certificate_refs.clone());
                        port_protocol
                            .entry(listener.port)
                            .or_insert_with(|| listener.protocol.clone());
                    }
                }
            }
        }
        if !merged_certs_by_port.is_empty() {
            let merged_listeners: Vec<LC> = merged_certs_by_port
                .into_iter()
                .map(|(port, cert_refs)| LC {
                    name: format!("merged-tls-{}", port),
                    protocol: port_protocol.remove(&port).unwrap_or(P::HTTPS),
                    port,
                    hostname: None,
                    address: None,
                    interface: None,
                    tls: Some(TlsConfig {
                        mode: TlsMode::Terminate,
                        certificate_refs: cert_refs,
                    }),
                })
                .collect();
            match ctx.data_plane.lock() {
                Ok(mut dp) => {
                    let (tls_updated, tls_errors) = dp.update_tls_configs(&merged_listeners);
                    if tls_updated > 0 {
                        debug!("Refreshed merged TLS config for {} port(s)", tls_updated);
                    }
                    for (port, err) in &tls_errors {
                        warn!("Merged TLS config update failed for port {}: {}", port, err);
                    }
                }
                Err(e) => {
                    warn!("Failed to lock data plane for TLS update: {}", e);
                }
            }
        }
    }

    // Build a single route table from all configs.
    // Each config's to_route_table() adds routes scoped by listener (port, hostname).

    // Compute per-listener attached route counts from accepted routes (for this gateway only).
    // Use the listener_names field which tracks exactly which listeners each route was accepted by.
    let mut listener_route_counts: HashMap<String, i32> = HashMap::new();
    for listener in &gateway.spec.listeners {
        listener_route_counts.insert(listener.name.clone(), 0);
    }
    for ra in &result.route_status {
        if ra.accepted {
            for listener_name in &ra.listener_names {
                *listener_route_counts
                    .entry(listener_name.clone())
                    .or_insert(0) += 1;
            }
        }
    }

    let route_table = tokio::task::spawn_blocking(move || {
        let mut combined = crate::routing::RouteTable::new();
        for config in &all_configs {
            let rt = config.to_route_table()?;
            // Merge listener scopes from each gateway's route table
            for (port, scopes) in rt.listener_scopes {
                combined
                    .listener_scopes
                    .entry(port)
                    .or_default()
                    .extend(scopes);
            }
            combined.tcp_routes.extend(rt.tcp_routes);
            combined.udp_routes.extend(rt.udp_routes);
            combined.tls_routes.extend(rt.tls_routes);
            combined.wildcard_tls_routes.extend(rt.wildcard_tls_routes);
        }
        Ok::<_, anyhow::Error>(combined)
    })
    .await
    .map_err(|e| ReconcileError::Reconcile(e.to_string()))?;

    let route_table_ok = match route_table {
        Ok(route_table) => {
            ctx.routes.store(Arc::new(route_table));
            info!(
                "Gateway {}/{} reconciled: {} HTTP, {} TCP, {} TLS, {} UDP routes",
                gw_ns,
                gw_name,
                result.config.http_routes.len(),
                result.config.tcp_routes.len(),
                result.config.tls_routes.len(),
                result.config.udp_routes.len(),
            );

            // Verify data plane has bound all required (address, port) endpoints.
            // Must match what we passed to add_tcp_listeners above.
            let required_endpoints: Vec<(Option<String>, u16)> = if bind_addresses.is_empty() {
                gateway
                    .spec
                    .listeners
                    .iter()
                    .map(|l| (None, l.port as u16))
                    .collect()
            } else {
                bind_addresses
                    .iter()
                    .flat_map(|addr| {
                        gateway
                            .spec
                            .listeners
                            .iter()
                            .map(move |l| (Some(addr.clone()), l.port as u16))
                    })
                    .collect()
            };
            let dp_ready = match ctx.data_plane.lock() {
                Ok(dp) => dp.is_ready_for_endpoints(&required_endpoints),
                Err(_) => false,
            };
            // Check if static addresses are usable by this node
            let (addrs_usable, addrs_reason, addrs_msg) = if let Some(spec_addrs) =
                &gateway.spec.addresses
            {
                if spec_addrs.is_empty() {
                    (true, "Programmed", String::from("Programmed"))
                } else {
                    let all_usable = if usable.is_empty() {
                        true // No known addresses — can't determine, assume usable
                    } else {
                        let known = usable.all();
                        spec_addrs.iter().all(|a| {
                            match a.r#type.as_deref().unwrap_or("IPAddress") {
                                "IPAddress" => known.contains(a.value.as_deref().unwrap_or("")),
                                "Hostname" => true,
                                _ => false,
                            }
                        })
                    };
                    if all_usable {
                        (true, "Programmed", String::from("Programmed"))
                    } else {
                        (
                                false,
                                "AddressNotUsable",
                                String::from(
                                    "One or more addresses in spec.addresses are not usable by this gateway",
                                ),
                            )
                    }
                }
            } else {
                (true, "Programmed", String::from("Programmed"))
            };

            let has_static_addrs = gateway
                .spec
                .addresses
                .as_ref()
                .map(|a| !a.is_empty())
                .unwrap_or(false);
            let (programmed, programmed_reason, programmed_msg) = if !addrs_usable {
                (false, addrs_reason, addrs_msg)
            } else if !dp_ready && has_static_addrs {
                // Static addresses set but bind failed — address not usable on this pod
                (
                    false,
                    "AddressNotUsable",
                    String::from(
                        "One or more addresses in spec.addresses are not usable by this gateway",
                    ),
                )
            } else if !dp_ready {
                warn!("Data plane not ready: not all listener ports are bound");
                (
                    false,
                    "Invalid",
                    String::from("Data plane not ready: not all listener ports are bound"),
                )
            } else {
                (true, "Programmed", String::from("Programmed"))
            };

            status::update_gateway_status(
                &ctx.client,
                &gateway,
                gateway_accepted,
                &gateway_accepted_reason,
                &gateway_accepted_message,
                programmed,
                programmed_reason,
                &programmed_msg,
                &listener_route_counts,
                &listener_statuses,
                &usable,
            )
            .await;
            dp_ready && addrs_usable
        }
        Err(e) => {
            error!(
                "Failed to build route table for Gateway {}/{}: {}",
                gw_ns, gw_name, e
            );
            status::update_gateway_status(
                &ctx.client,
                &gateway,
                gateway_accepted,
                &gateway_accepted_reason,
                &gateway_accepted_message,
                false,
                "Invalid",
                &format!("Route table conversion failed: {}", e),
                &listener_route_counts,
                &listener_statuses,
                &usable,
            )
            .await;
            false
        }
    };

    // Update per-route status — group by (kind, name, namespace) to write all
    // parent statuses in a single patch (prevents SSA from overwriting previous entries
    // when a route references multiple listeners/gateways).
    let programmed = route_table_ok;
    {
        use std::collections::BTreeMap;
        // Use a per-gateway field manager so SSA doesn't overwrite parents from other gateways
        let field_manager = format!("portail-{}-{}", gw_ns, gw_name);
        // Key: (kind, namespace, name) → Vec of parent statuses
        let mut grouped: BTreeMap<(&str, &str, &str), Vec<status::RouteParentStatus>> =
            BTreeMap::new();
        for ra in &result.route_status {
            let route_programmed = ra.accepted && programmed;
            grouped
                .entry((ra.kind, &ra.namespace, &ra.name))
                .or_default()
                .push(status::RouteParentStatus {
                    controller_name: ctx.controller_name.clone(),
                    gateway_name: gw_name.clone(),
                    gateway_namespace: gw_ns.clone(),
                    section_name: ra.section_name.clone(),
                    port: None,
                    accepted: ra.accepted,
                    accepted_reason: ra.accepted_reason.clone(),
                    message: ra.message.clone(),
                    refs_resolved: ra.refs_resolved,
                    refs_reason: ra.refs_reason.clone(),
                    refs_message: ra.refs_message.clone(),
                    programmed: route_programmed,
                    generation: ra.generation,
                });
        }

        for ((kind, ns, name), parents) in &grouped {
            macro_rules! patch_route_status {
                ($store:expr, $ty:ty) => {{
                    let existing = get_existing_route_parents_from_store(&$store, ns, name);
                    status::update_route_status::<$ty>(
                        &ctx.client,
                        name,
                        ns,
                        parents,
                        &field_manager,
                        &existing,
                    )
                    .await;
                }};
            }
            match *kind {
                "HTTPRoute" => patch_route_status!(ctx.cache.http_routes, HTTPRoute),
                "TCPRoute" => patch_route_status!(ctx.cache.tcp_routes, TCPRoute),
                "TLSRoute" => patch_route_status!(ctx.cache.tls_routes, TLSRoute),
                "UDPRoute" => patch_route_status!(ctx.cache.udp_routes, UDPRoute),
                _ => {}
            }
        }
    }

    Ok(Action::await_change())
}

fn error_policy(_obj: Arc<Gateway>, _error: &ReconcileError, _ctx: Arc<ControllerCtx>) -> Action {
    warn!("Gateway reconciliation error, requeueing in 30s");
    Action::requeue(Duration::from_secs(30))
}

// ---------------------------------------------------------------------------
// GatewayClass controller — runs alongside the Gateway controller
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct GatewayClassCtx {
    client: Client,
    controller_name: String,
    store: Store<GatewayClass>,
}

async fn reconcile_gateway_class(
    gc: Arc<GatewayClass>,
    ctx: Arc<GatewayClassCtx>,
) -> Result<Action, ReconcileError> {
    if gc.spec.controller_name != ctx.controller_name {
        return Ok(Action::await_change());
    }

    // Accept the oldest GatewayClass for our controller (per Gateway API spec).
    // The store is guaranteed populated — Controller::new() waits for InitDone.
    let accepted = ctx
        .store
        .state()
        .iter()
        .filter(|g| g.spec.controller_name == ctx.controller_name)
        .min_by_key(|g| g.metadata.creation_timestamp.clone())
        .map(|g| g.name_any());

    let is_accepted = accepted.as_deref() == Some(gc.name_any().as_str());
    if is_accepted {
        status::update_gateway_class_status(&ctx.client, &gc, true, "Accepted by portail").await;
    } else {
        status::update_gateway_class_status(
            &ctx.client,
            &gc,
            false,
            &format!(
                "Another GatewayClass '{}' is already accepted by this controller",
                accepted.unwrap_or_default()
            ),
        )
        .await;
    }

    Ok(Action::await_change())
}

fn gc_error_policy(
    _obj: Arc<GatewayClass>,
    _error: &ReconcileError,
    _ctx: Arc<GatewayClassCtx>,
) -> Action {
    warn!("GatewayClass reconciliation error, requeueing in 30s");
    Action::requeue(Duration::from_secs(30))
}

/// Read existing parent status conditions from a route's reflector store entry.
fn get_existing_route_parents_from_store<K>(
    store: &Store<K>,
    ns: &str,
    name: &str,
) -> Vec<serde_json::Value>
where
    K: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope>
        + serde::Serialize
        + Clone
        + std::fmt::Debug,
    <K as kube::Resource>::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
    let obj_ref = ObjectRef::<K>::new(name).within(ns);
    match store.get(&obj_ref) {
        Some(route) => {
            let val = serde_json::to_value(route.as_ref()).unwrap_or_default();
            val.pointer("/status/parents")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
        }
        None => vec![],
    }
}
