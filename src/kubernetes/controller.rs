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

/// Custom address type for binding a Gateway to a named network interface.
/// Used in `spec.addresses[].type` to auto-discover UDN/Multus IPs.
pub(crate) const NETWORK_ADDRESS_TYPE: &str = "portail.epheo.eu/Network";

/// Addresses available to this gateway instance for binding / reporting.
/// `status.addresses` (LB VIP / cloud-assigned IPs) is owned by the operator,
/// so portail only needs locally-bindable IPs.
pub(crate) struct UsableAddresses {
    /// IPs from local network interfaces (includes secondary/UDN interfaces).
    pub interface_ips: Vec<String>,
    /// IPs resolved from `portail.epheo.eu/Network` address type (network-name → IP).
    pub network_ips: HashMap<String, String>,
}

impl UsableAddresses {
    /// All known addresses as a set (for membership checks).
    pub fn all(&self) -> HashSet<&str> {
        self.interface_ips
            .iter()
            .chain(self.network_ips.values())
            .map(|s| s.as_str())
            .collect()
    }

    /// Best locally-bindable address — first interface IP, else `0.0.0.0`.
    pub fn preferred_ip(&self) -> &str {
        self.interface_ips
            .first()
            .map(|s| s.as_str())
            .unwrap_or("0.0.0.0")
    }

    pub fn is_empty(&self) -> bool {
        self.interface_ips.is_empty() && self.network_ips.is_empty()
    }
}

/// Resolve a network name to this pod's local IP on that network.
///
/// Reads the NetworkAttachmentDefinition to get the subnet CIDR, then matches
/// it against local interface IPs discovered via `getifaddrs()`.
async fn resolve_network_to_local_ip(
    client: &Client,
    network_name: &str,
    namespace: &str,
    local_ips: &[String],
) -> Option<String> {
    // Read the NAD in the Gateway's namespace
    let nad_api: Api<kube::api::DynamicObject> = Api::namespaced_with(
        client.clone(),
        namespace,
        &kube::discovery::ApiResource {
            group: "k8s.cni.cncf.io".into(),
            version: "v1".into(),
            api_version: "k8s.cni.cncf.io/v1".into(),
            kind: "NetworkAttachmentDefinition".into(),
            plural: "network-attachment-definitions".into(),
        },
    );
    let nad = match nad_api.get(network_name).await {
        Ok(n) => n,
        Err(e) => {
            warn!("Failed to read NetworkAttachmentDefinition {namespace}/{network_name}: {e}");
            return None;
        }
    };

    // Extract subnet from spec.config JSON
    let config_str = nad.data["spec"]["config"].as_str()?;
    let config: serde_json::Value = serde_json::from_str(config_str).ok()?;
    let subnets_str = config["subnets"].as_str()?;

    // Parse CIDR and match against local IPs
    let (net_addr, prefix_len) = parse_cidr(subnets_str)?;
    for ip_str in local_ips {
        if let Ok(ip) = ip_str.parse::<std::net::Ipv4Addr>() {
            if ip_in_subnet(ip, net_addr, prefix_len) {
                return Some(ip_str.clone());
            }
        }
    }
    warn!(
        "No local interface IP matches subnet {subnets_str} for network {namespace}/{network_name}"
    );
    None
}

fn parse_cidr(cidr: &str) -> Option<(std::net::Ipv4Addr, u32)> {
    let (addr_str, len_str) = cidr.split_once('/')?;
    let addr: std::net::Ipv4Addr = addr_str.parse().ok()?;
    let len: u32 = len_str.parse().ok()?;
    Some((addr, len))
}

fn ip_in_subnet(ip: std::net::Ipv4Addr, network: std::net::Ipv4Addr, prefix_len: u32) -> bool {
    if prefix_len > 32 {
        return false;
    }
    let mask = if prefix_len == 0 {
        0u32
    } else {
        !0u32 << (32 - prefix_len)
    };
    u32::from(ip) & mask == u32::from(network) & mask
}

/// Enumerate IPv4/IPv6 addresses from all local network interfaces.
/// Discovers all routable IPs for EndpointSlice matching, bind-address filtering, and status reporting.
fn discover_local_interface_ips() -> Vec<String> {
    let mut ips = Vec::new();
    unsafe {
        let mut ifaddrs: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifaddrs) != 0 {
            return ips;
        }
        let mut ifa = ifaddrs;
        while !ifa.is_null() {
            let addr = (*ifa).ifa_addr;
            if !addr.is_null() {
                let family = (*addr).sa_family as i32;
                if family == libc::AF_INET {
                    let sa = addr as *const libc::sockaddr_in;
                    let ip = std::net::Ipv4Addr::from(u32::from_be((*sa).sin_addr.s_addr));
                    if !ip.is_loopback() {
                        ips.push(ip.to_string());
                    }
                } else if family == libc::AF_INET6 {
                    let sa = addr as *const libc::sockaddr_in6;
                    let ip = std::net::Ipv6Addr::from((*sa).sin6_addr.s6_addr);
                    let is_link_local = (ip.segments()[0] & 0xffc0) == 0xfe80;
                    if !ip.is_loopback() && !is_link_local {
                        ips.push(ip.to_string());
                    }
                }
            }
            ifa = (*ifa).ifa_next;
        }
        libc::freeifaddrs(ifaddrs);
    }
    ips
}

/// Enumerate IPs available on this pod's local interfaces (pod IP, host IPs
/// in hostNetwork mode, secondary network IPs from Multus, etc.). The operator
/// owns Gateway `status.addresses`, so portail only needs locally-bindable IPs.
pub(crate) fn discover_usable_addresses() -> UsableAddresses {
    UsableAddresses {
        interface_ips: discover_local_interface_ips(),
        network_ips: HashMap::new(),
    }
}

#[derive(Clone)]
struct ControllerCtx {
    client: Client,
    controller_name: String,
    cache: ResourceCache,
    routes: Arc<ArcSwap<RouteTable>>,
    data_plane: Arc<std::sync::Mutex<crate::data_plane::DataPlane>>,
    performance_config: crate::config::PerformanceConfig,
    /// When false, the operator owns Gateway/GatewayClass lifecycle status
    /// (Accepted/Programmed/addresses); portail reports only listener + route status.
    manage_gateway_status: bool,
    /// Flipped to true once the data plane has bound this instance's listener
    /// ports. Surfaced via the readiness endpoint for the pod's readinessProbe.
    ready: Arc<std::sync::atomic::AtomicBool>,
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

/// Like `create_reflector`, but first probes the API endpoint with a lightweight
/// list request. If the CRD is not installed (404), returns an empty store and
/// a stream that never yields, so the controller can still start without it.
async fn create_optional_reflector<K>(
    api: Api<K>,
) -> (
    Store<K>,
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<K, watcher::Error>> + Send + 'static>>,
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
    // Probe with limit=0 — just checks the endpoint exists
    let probe = api.list(&kube::api::ListParams::default().limit(1)).await;
    match probe {
        Err(kube::Error::Api(ref status)) if status.code == 404 => {
            let kind = std::any::type_name::<K>()
                .rsplit("::")
                .next()
                .unwrap_or("Unknown");
            warn!("CRD not installed, skipping watcher: {kind}");
            let writer = reflector::store::Writer::default();
            let reader = writer.as_reader();
            (reader, Box::pin(futures::stream::pending()))
        }
        _ => {
            let (store, stream) = create_reflector(api);
            (store, Box::pin(stream))
        }
    }
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
    manage_gateway_status: bool,
    ready: Arc<std::sync::atomic::AtomicBool>,
    gateway_scope: Option<(String, String)>,
) -> anyhow::Result<()> {
    let client = Client::try_default().await?;
    info!("Kubernetes client connected, starting Gateway API controller");

    // --- Primary resource: Gateway ---
    // predicate_filter(predicates::generation) filters status-only changes,
    // breaking the reconcile → status patch → watch event feedback loop.
    //
    // When `gateway_scope` is set (operator-managed Deployments), narrow the
    // watch to that single Gateway via namespace + `metadata.name` field
    // selector. The reflector store then holds exactly one object, and every
    // downstream mapper that already filters by `route_targets_gateway` /
    // `all_gateway_refs` naturally collapses to "this Gateway or nothing."
    // Unset = legacy unscoped mode (watch all Gateways cluster-wide).
    if let Some((ns, name)) = &gateway_scope {
        info!("Controller scoped to Gateway {}/{}", ns, name);
    }
    let gw_writer = reflector::store::Writer::default();
    let store_gateways = gw_writer.as_reader();
    let (gw_api, gw_watcher_config) = match &gateway_scope {
        Some((ns, name)) => (
            Api::<Gateway>::namespaced(client.clone(), ns),
            watcher::Config::default().fields(&format!("metadata.name={}", name)),
        ),
        None => (
            Api::<Gateway>::all(client.clone()),
            watcher::Config::default(),
        ),
    };
    let gw_stream = reflector::reflector(gw_writer, watcher::watcher(gw_api, gw_watcher_config))
        .default_backoff()
        .applied_objects()
        .predicate_filter(predicates::generation);

    // --- Secondary resources ---
    // Each resource gets a reflector (store + stream). The stream feeds into
    // Controller::watches_stream() with a mapper that targets affected Gateway(s).
    // This is the standard kube-rs/controller-runtime pattern: secondary resource
    // events go through a separate path unaffected by the primary predicate_filter.

    // CRD reflectors — generation-filtered to ignore status-only changes
    // HTTPRoute is required (GA); experimental CRDs are optional.
    let (store_http_routes, http_route_stream) =
        create_reflector(Api::<HTTPRoute>::all(client.clone()));
    let (store_tcp_routes, tcp_route_stream) =
        create_optional_reflector(Api::<TCPRoute>::all(client.clone())).await;
    let (store_tls_routes, tls_route_stream) =
        create_optional_reflector(Api::<TLSRoute>::all(client.clone())).await;
    let (store_udp_routes, udp_route_stream) =
        create_optional_reflector(Api::<UDPRoute>::all(client.clone())).await;
    // GatewayClass status is owned by portail-operator now; portail no longer
    // runs its own GatewayClass controller.

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
        cache: ResourceCache {
            gateways: store_gateways,
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
        manage_gateway_status,
        ready,
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
fn validate_gateway(
    gateway: &Gateway,
    gw_ns: &str,
    cert_data: &HashMap<(String, String), (Vec<u8>, Vec<u8>)>,
    reference_grants: &[ReferenceGrant],
) -> (
    HashMap<String, status::ListenerStatus>,
    status::GatewayCondition,
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
                match addr_type {
                    "IPAddress" | "Hostname" => {}
                    t if t == NETWORK_ADDRESS_TYPE => {}
                    _ => {
                        accepted = false;
                        reason = "UnsupportedAddress".to_string();
                        message =
                            format!("Unsupported address type '{}' in spec.addresses", addr_type);
                        break;
                    }
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

    (
        listener_statuses,
        status::GatewayCondition {
            ok: accepted,
            reason,
            message,
        },
    )
}

/// Resolve `portail.epheo.eu/Network` entries in `spec.addresses` to local IPs.
/// Reads each named NetworkAttachmentDefinition for its subnet and matches it
/// against the addresses we already discovered via `getifaddrs()`.
async fn resolve_network_addresses(
    client: &Client,
    gateway: &Gateway,
    gw_ns: &str,
    usable: &mut UsableAddresses,
) {
    let Some(spec_addrs) = &gateway.spec.addresses else {
        return;
    };
    let gw_name = gateway.name_any();
    for addr in spec_addrs {
        if addr.r#type.as_deref() != Some(NETWORK_ADDRESS_TYPE) {
            continue;
        }
        let network_name = addr.value.as_deref().unwrap_or("");
        match resolve_network_to_local_ip(client, network_name, gw_ns, &usable.interface_ips).await
        {
            Some(ip) => {
                info!("Gateway {gw_ns}/{gw_name}: resolved network '{network_name}' to {ip}");
                usable.network_ips.insert(network_name.to_string(), ip);
            }
            None => warn!(
                "Gateway {gw_ns}/{gw_name}: could not resolve network '{network_name}' to a local IP"
            ),
        }
    }
}

/// Pick which spec.addresses are bind-targets on this pod. LB VIPs are external
/// routing identities handled by kube-proxy/MetalLB and bind to 0.0.0.0; only
/// pod-local interface IPs and resolved Network-type IPs get bound specifically.
fn compute_bind_addresses(addresses: &[String], usable: &UsableAddresses) -> Vec<String> {
    let network_ip_set: HashSet<&str> = usable.network_ips.values().map(|s| s.as_str()).collect();
    addresses
        .iter()
        .filter(|addr| {
            usable.interface_ips.iter().any(|ip| ip == *addr)
                || network_ip_set.contains(addr.as_str())
        })
        .cloned()
        .collect()
}

/// Open data-plane TCP listeners for this Gateway's ports. Called after the
/// per-Gateway config is built so ListenerConfig has resolved TLS cert data.
fn ensure_data_plane_listeners(
    ctx: &ControllerCtx,
    listeners: &[crate::config::ListenerConfig],
    bind_addresses: &[String],
) {
    info!(
        "Ensuring data plane listeners for ports: {:?}",
        listeners.iter().map(|l| l.port).collect::<Vec<_>>()
    );
    match ctx.data_plane.lock() {
        Ok(mut dp) => {
            let (opened, errors) = dp.add_tcp_listeners(
                listeners,
                bind_addresses,
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
        Err(e) => warn!("Failed to lock data plane: {}", e),
    }
}

/// (address, port) pairs the data plane must be bound to for this Gateway to
/// be considered Programmed. Mirrors what we just passed to `add_tcp_listeners`.
fn required_endpoints(gateway: &Gateway, bind_addresses: &[String]) -> Vec<(Option<String>, u16)> {
    if bind_addresses.is_empty() {
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
    }
}

/// Compute the `Programmed` condition from data-plane readiness. The operator
/// owns address-usability semantics for status.addresses; here we just report
/// whether the data plane has bound this Gateway's listener ports.
fn compute_programmed_condition(dp_ready: bool) -> status::GatewayCondition {
    if dp_ready {
        status::GatewayCondition {
            ok: true,
            reason: "Programmed".into(),
            message: "Programmed".into(),
        }
    } else {
        warn!("Data plane not ready: not all listener ports are bound");
        status::GatewayCondition {
            ok: false,
            reason: "Invalid".into(),
            message: "Data plane not ready: not all listener ports are bound".into(),
        }
    }
}

/// Build the RouteTable from this Gateway's PortailConfig on a blocking thread
/// (`to_route_table` compiles regexes and resolves backend DNS — both sync).
async fn build_route_table(config: crate::config::PortailConfig) -> anyhow::Result<RouteTable> {
    tokio::task::spawn_blocking(move || config.to_route_table())
        .await
        .map_err(|e| anyhow::anyhow!("route table build panicked: {}", e))?
}

async fn reconcile(
    gateway: Arc<Gateway>,
    ctx: Arc<ControllerCtx>,
) -> Result<Action, ReconcileError> {
    let gw_name = gateway.name_any();
    let gw_ns = gateway.namespace().unwrap_or_else(|| "default".to_string());
    debug!("Reconciling Gateway {}/{}", gw_ns, gw_name);

    // GatewayClass acceptance is owned by portail-operator; it only provisions a
    // portail Deployment for Gateways whose class references this controller, so
    // we can trust the scope and skip an in-process class check here.

    // Snapshot reflector caches in one shot — no API server calls.
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

    let (listener_statuses, accepted_cond) =
        validate_gateway(&gateway, &gw_ns, &cert_data, &snapshot.reference_grants);

    let mut usable = discover_usable_addresses();
    resolve_network_addresses(&ctx.client, &gateway, &gw_ns, &mut usable).await;

    // Build per-Gateway config; bail out with a failing Programmed condition on error.
    let mut result = match reconcile_to_config(&gateway, &snapshot, &cert_data, &services) {
        Ok(r) => r,
        Err(e) => {
            error!("Failed to reconcile Gateway {}/{}: {}", gw_ns, gw_name, e);
            let failure = status::GatewayCondition {
                ok: false,
                reason: "Invalid".into(),
                message: format!("Reconciliation failed: {}", e),
            };
            status::update_gateway_status(
                &ctx.client,
                &gateway,
                &accepted_cond,
                &failure,
                &HashMap::new(),
                &listener_statuses,
                &usable,
                ctx.manage_gateway_status,
            )
            .await;
            return Err(ReconcileError::Reconcile(e.to_string()));
        }
    };

    // Substitute resolved IPs for portail.epheo.eu/Network entries (reconcile_to_config
    // copies spec.addresses values as-is).
    if !usable.network_ips.is_empty() {
        result.config.gateway.addresses = result
            .config
            .gateway
            .addresses
            .iter()
            .map(|addr| {
                usable
                    .network_ips
                    .get(addr)
                    .cloned()
                    .unwrap_or_else(|| addr.clone())
            })
            .collect();
    }

    let bind_addresses = compute_bind_addresses(&result.config.gateway.addresses, &usable);
    ensure_data_plane_listeners(&ctx, &result.config.gateway.listeners, &bind_addresses);

    // Per-listener attached-route counts, restricted to this Gateway's routes.
    let mut listener_route_counts: HashMap<String, i32> = gateway
        .spec
        .listeners
        .iter()
        .map(|l| (l.name.clone(), 0))
        .collect();
    for ra in &result.route_status {
        if ra.accepted {
            for listener_name in &ra.listener_names {
                *listener_route_counts
                    .entry(listener_name.clone())
                    .or_insert(0) += 1;
            }
        }
    }

    // Build + swap the RouteTable; on failure produce a failing Programmed
    // condition (route table build is the last step that can fail). Counts
    // are captured before the config is moved into the blocking build.
    let http_count = result.config.http_routes.len();
    let tcp_count = result.config.tcp_routes.len();
    let tls_count = result.config.tls_routes.len();
    let udp_count = result.config.udp_routes.len();
    let programmed_cond = match build_route_table(result.config).await {
        Ok(route_table) => {
            ctx.routes.store(Arc::new(route_table));
            info!(
                "Gateway {}/{} reconciled: {} HTTP, {} TCP, {} TLS, {} UDP routes",
                gw_ns, gw_name, http_count, tcp_count, tls_count, udp_count,
            );

            let endpoints = required_endpoints(&gateway, &bind_addresses);
            let dp_ready = ctx
                .data_plane
                .lock()
                .map(|dp| dp.is_ready_for_endpoints(&endpoints))
                .unwrap_or(false);
            // Once the data plane has bound this gateway's listener ports, the
            // pod is serving — latch readiness on (never flips back).
            if dp_ready {
                ctx.ready.store(true, std::sync::atomic::Ordering::Release);
            }
            compute_programmed_condition(dp_ready)
        }
        Err(e) => {
            error!(
                "Failed to build route table for Gateway {}/{}: {}",
                gw_ns, gw_name, e
            );
            status::GatewayCondition {
                ok: false,
                reason: "Invalid".into(),
                message: format!("Route table conversion failed: {}", e),
            }
        }
    };
    let programmed = programmed_cond.ok;

    status::update_gateway_status(
        &ctx.client,
        &gateway,
        &accepted_cond,
        &programmed_cond,
        &listener_route_counts,
        &listener_statuses,
        &usable,
        ctx.manage_gateway_status,
    )
    .await;

    // Update per-route status — group by (kind, ns, name) so SSA writes all
    // parent entries together (otherwise multi-listener routes lose previous
    // entries on each patch).
    {
        use std::collections::BTreeMap;
        let field_manager = format!("portail-{}-{}", gw_ns, gw_name);
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
