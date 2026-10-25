//! One Gateway's snapshot onto the data plane: bind what it asks for,
//! close what it no longer does, build and swap the route table when the
//! config moved, and report what is bound.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use arc_swap::{ArcSwap, ArcSwapOption};
use tokio_util::sync::CancellationToken;

use super::addresses::{bind_addresses_resolved, local_interface_ips, resolve_networks};
use super::dns_refresh::{spawn_dns_refresh, Published};
use crate::config::{ListenerConfig, PerformanceConfig, PortailConfig};
use crate::logging::{error, info, warn};
use crate::proxy::data_plane::{BindError, DataPlane, Endpoint};
use crate::routing::RouteTable;
use crate::sync::{EndpointReport, InstanceReport, ListenerReport, Snapshot, Transport};

/// Applies snapshots to a data plane, one at a time.
pub struct Applier {
    data_plane: Arc<Mutex<DataPlane>>,
    routes: Arc<ArcSwap<RouteTable>>,
    perf: PerformanceConfig,
    /// /readyz: the snapshot's endpoints have live accept loops.
    ready: Arc<AtomicBool>,
    /// The last built config and the table built from it, for the DNS
    /// refresh to re-resolve. `None` until a table builds.
    published: Arc<ArcSwapOption<Published>>,
    /// A snapshot with the same fingerprint and live listeners skips the
    /// bind and the rebuild: no table is built, so nothing is resolved or
    /// compiled for one.
    last: Option<Applied>,
    dns_epoch: tokio::sync::watch::Sender<u64>,
    /// Cold-start latency context for the "Data plane ready" log line.
    started: Instant,
}

/// The data plane's last apply.
#[derive(Clone)]
struct Applied {
    fingerprint: u64,
    build_error: Option<String>,
}

/// How an apply left the data plane.
struct DataPlaneState {
    rebuilt: bool,
    build_error: Option<String>,
    /// From this apply's binds.
    bind_errors: Vec<BindError>,
}

fn lock(data_plane: &Mutex<DataPlane>) -> MutexGuard<'_, DataPlane> {
    data_plane.lock().unwrap_or_else(|p| p.into_inner())
}

impl Applier {
    pub fn new(
        data_plane: Arc<Mutex<DataPlane>>,
        routes: Arc<ArcSwap<RouteTable>>,
        perf: PerformanceConfig,
        ready: Arc<AtomicBool>,
    ) -> Self {
        // Let the data plane flip /readyz false itself the instant an accept
        // loop dies, instead of waiting for the next apply to notice.
        lock(&data_plane).set_ready_flag(ready.clone());
        Self {
            data_plane,
            routes,
            perf,
            ready,
            published: Arc::new(ArcSwapOption::empty()),
            last: None,
            dns_epoch: tokio::sync::watch::Sender::new(0),
            started: Instant::now(),
        }
    }

    /// Bring the data plane in line with `snapshot` and report the result,
    /// under the snapshot's version as sent.
    pub async fn apply(&mut self, snapshot: &Snapshot) -> InstanceReport {
        let config = snapshot.to_config();
        let local = local_interface_ips();
        let networks = resolve_networks(&config.gateway.addresses, &snapshot.networks, &local);
        let served = Served {
            bind: bind_addresses_resolved(&config.gateway.addresses, &networks, &local),
            config: Arc::new(config),
        };
        let state = self.bring_up(&served).await;
        let gateway = snapshot.gateway.key();
        let config = &served.config;
        match &state.build_error {
            Some(e) if state.rebuilt => {
                error!("Failed to build route table for Gateway {}: {}", gateway, e)
            }
            None if state.rebuilt => info!(
                "Gateway {} applied ({}): {} HTTP, {} TCP, {} TLS, {} UDP routes",
                gateway,
                snapshot.version,
                config.http_routes.len(),
                config.tcp_routes.len(),
                config.tls_routes.len(),
                config.udp_routes.len(),
            ),
            _ => {}
        }
        InstanceReport {
            applied: snapshot.version.clone(),
            listeners: self.listener_reports(&served, &state.bind_errors),
            apply_error: state.build_error,
            interface_ips: local,
            networks,
            dns_epoch: self.dns_epoch(),
            leaving: false,
        }
    }

    /// The Gateway is gone (404): close every listener, drop the table and
    /// its certs, turn NotReady. The next apply starts afresh.
    pub async fn clear(&mut self) {
        let closing = lock(&self.data_plane).close_unwanted(&[]);
        for accept_loop in closing {
            let _ = accept_loop.await;
        }
        // An empty pass drops the closed ports' TLS plans: no cert outlives
        // its listener.
        lock(&self.data_plane).add_tcp_listeners(&[], self.routes.clone(), &self.perf);
        let empty = Arc::new(RouteTable::new());
        self.routes.store(empty.clone());
        lock(&self.data_plane).health().prune(&empty);
        self.published.store(None);
        self.last = None;
        if self.ready.swap(false, Ordering::AcqRel) {
            info!("Data plane cleared: no listener bound");
        }
    }

    /// DNS refresh swaps so far.
    pub fn dns_epoch(&self) -> u64 {
        *self.dns_epoch.borrow()
    }

    /// Wakes on each DNS refresh swap: the report's `dnsEpoch` moved.
    pub fn dns_epochs(&self) -> tokio::sync::watch::Receiver<u64> {
        self.dns_epoch.subscribe()
    }

    /// Re-resolve the applied config's backends on the performance config's
    /// interval, swapping the table when the resolved set changes.
    pub fn spawn_dns_refresh(&self, shutdown: CancellationToken) {
        let health = lock(&self.data_plane).health().clone();
        let epoch = self.dns_epoch.clone();
        spawn_dns_refresh(
            self.published.clone(),
            self.routes.clone(),
            health,
            self.perf.dns_refresh_interval,
            shutdown,
            move || epoch.send_modify(|e| *e += 1),
        );
    }

    /// The gate is three-way:
    /// - unchanged fingerprint, every endpoint live: nothing to do. The
    ///   fingerprint covers the cert bytes (no hot-reload can be pending),
    ///   and readiness is read from live accept loops, so a dead listener
    ///   takes the bind path.
    /// - unchanged, not live: the one thing still owed is a bind retry.
    /// - changed: bind, rebuild the table, publish it.
    async fn bring_up(&mut self, served: &Served) -> DataPlaneState {
        let binds = served.binds();
        let fingerprint = data_plane_fingerprint(&served.config, &binds);
        let unchanged = self
            .last
            .clone()
            .filter(|applied| applied.fingerprint == fingerprint);
        // Which listeners terminate nothing is the pass's to say: asked
        // after it.
        let is_ready = |dp: &Mutex<DataPlane>| {
            let dp = lock(dp);
            dp.is_ready_for_endpoints(&served.required(&dp))
        };

        if is_ready(&self.data_plane) {
            self.ready.store(true, Ordering::Release);
            if let Some(applied) = &unchanged {
                return DataPlaneState {
                    rebuilt: false,
                    build_error: applied.build_error.clone(),
                    bind_errors: Vec::new(),
                };
            }
        }

        let bind_errors = self.ensure_listeners(&binds).await;
        let ready = is_ready(&self.data_plane);
        // Store the truth in BOTH directions: readiness must also drop when a
        // required endpoint has no live accept loop and the rebind above failed,
        // not only rise on success.
        let was_ready = self.ready.swap(ready, Ordering::AcqRel);
        if ready && !was_ready {
            info!(
                "Data plane ready: {} listener endpoint(s) live, {:.1}s after start",
                served.required(&lock(&self.data_plane)).len(),
                self.started.elapsed().as_secs_f32()
            );
        }
        if let Some(applied) = unchanged {
            return DataPlaneState {
                rebuilt: false,
                build_error: applied.build_error,
                bind_errors,
            };
        }

        let build_error = match build_route_table(served.config.clone()).await {
            Ok(route_table) => {
                let table = Arc::new(route_table);
                self.routes.store(table.clone());
                lock(&self.data_plane).health().prune(&table);
                // Publish only after the table is live: the DNS-refresh task
                // re-resolves from this pair and must never pick up a config
                // whose build failed. Its CAS against the pair's table drops a
                // tick that read the previous pair.
                self.published.store(Some(Arc::new(Published {
                    config: served.config.clone(),
                    table,
                })));
                None
            }
            Err(e) => Some(e.to_string()),
        };
        self.last = Some(Applied {
            fingerprint,
            build_error: build_error.clone(),
        });
        DataPlaneState {
            rebuilt: true,
            build_error,
            bind_errors,
        }
    }

    /// Bind the data plane to `binds` (see [`Served::binds`]) and close what
    /// they no longer hold. Returns the bind errors.
    async fn ensure_listeners(&self, binds: &[ListenerConfig]) -> Vec<BindError> {
        info!(
            "Ensuring data plane listeners for ports: {:?}",
            binds.iter().map(|l| l.port).collect::<Vec<_>>()
        );
        let closing = lock(&self.data_plane).close_unwanted(binds);
        // A listener moving between the wildcard and a specific address binds
        // once its old socket is gone, in this apply: the port is dark until
        // then.
        for accept_loop in closing {
            let _ = accept_loop.await;
        }
        let (opened, errors) =
            lock(&self.data_plane).add_tcp_listeners(binds, self.routes.clone(), &self.perf);
        if opened > 0 {
            info!("Opened {} new port(s)", opened);
        }
        for e in &errors {
            warn!("Failed to bind {}: {}", e.endpoint, e.error);
        }
        errors
    }

    /// Each listener's endpoints, bound or not, the unbound ones with their
    /// bind error.
    fn listener_reports(&self, served: &Served, bind_errors: &[BindError]) -> Vec<ListenerReport> {
        let dp = lock(&self.data_plane);
        served
            .config
            .gateway
            .listeners
            .iter()
            .map(|l| ListenerReport {
                name: l.name.clone(),
                endpoints: match served.bind.as_deref() {
                    None => Vec::new(),
                    Some(bind) => required_endpoints(std::slice::from_ref(l), bind)
                        .into_iter()
                        .map(|endpoint| {
                            // Its socket may be a sibling's: it serves none
                            // of it.
                            let refusal = dp.refusal(l);
                            let bound = refusal.is_none()
                                && dp.is_ready_for_endpoints(std::slice::from_ref(&endpoint));
                            let error = refusal.or_else(|| {
                                bind_errors
                                    .iter()
                                    .find(|e| !bound && e.endpoint == endpoint)
                                    .map(|e| e.error.clone())
                            });
                            EndpointReport {
                                error,
                                address: endpoint.address,
                                port: endpoint.port,
                                transport: endpoint.transport,
                                bound,
                            }
                        })
                        .collect(),
                },
            })
            .collect()
    }
}

/// The Gateway as the data plane serves it.
struct Served {
    config: Arc<PortailConfig>,
    /// Where its listeners bind (see
    /// [`bind_addresses`](super::addresses::bind_addresses)).
    bind: Option<Vec<String>>,
}

impl Served {
    /// Its listeners as the data plane binds them: one per bind address,
    /// with `address` set to it; as they are on every interface; none when
    /// nothing may bind.
    fn binds(&self) -> Vec<ListenerConfig> {
        let listeners = &self.config.gateway.listeners;
        match self.bind.as_deref() {
            None => Vec::new(),
            Some([]) => listeners.clone(),
            Some(addresses) => listeners
                .iter()
                .flat_map(|l| {
                    addresses.iter().map(|a| ListenerConfig {
                        address: Some(a.clone()),
                        ..l.clone()
                    })
                })
                .collect(),
        }
    }

    /// The endpoints its listeners must have live to be ready. Unbindable
    /// listeners still count, so readiness stays down. An HTTPS listener
    /// that terminates nothing does not (no certificateRef left, or its
    /// certs failed to build): it can never serve, and counting it took
    /// every other listener of the pod out with it.
    fn required(&self, dp: &DataPlane) -> Vec<Endpoint> {
        let bind = self.bind.as_deref().unwrap_or_default();
        self.config
            .gateway
            .listeners
            .iter()
            .filter(|l| dp.refusal(l).is_none())
            .flat_map(|l| required_endpoints(std::slice::from_ref(l), bind))
            .collect()
    }
}

/// The endpoints the data plane must be bound to for these listeners to
/// be ready. Keyed on each listener's bound port (`target_port` when the
/// controller decoupled it from the published port).
fn required_endpoints(listeners: &[ListenerConfig], bind_addresses: &[String]) -> Vec<Endpoint> {
    let on = |address: Option<&String>, l: &ListenerConfig| Endpoint {
        address: address.cloned(),
        port: l.target_port.unwrap_or(l.port),
        transport: Transport::of(&l.protocol),
    };
    if bind_addresses.is_empty() {
        listeners.iter().map(|l| on(None, l)).collect()
    } else {
        bind_addresses
            .iter()
            .flat_map(|addr| listeners.iter().map(move |l| on(Some(addr), l)))
            .collect()
    }
}

/// Build the RouteTable on a blocking thread (`to_route_table` compiles
/// regexes and resolves backend DNS, both sync).
async fn build_route_table(config: Arc<PortailConfig>) -> anyhow::Result<RouteTable> {
    tokio::task::spawn_blocking(move || config.to_route_table())
        .await
        .map_err(|e| anyhow::anyhow!("route table build panicked: {}", e))?
}

/// Fingerprint of what the data plane runs: the config (its JSON plus the
/// cert/key bytes and override maps that serde skips) and the bind plan.
/// Hash inputs with non-deterministic iteration order (HashMaps) are
/// canonicalized by sorting.
fn data_plane_fingerprint(config: &PortailConfig, binds: &[ListenerConfig]) -> u64 {
    use std::hash::Hasher;

    fn write_sorted(h: &mut fnv::FnvHasher, mut items: Vec<String>) {
        items.sort_unstable();
        for item in &items {
            h.write(item.as_bytes());
            h.write(&[0]);
        }
    }

    let mut h = fnv::FnvHasher::default();
    if let Ok(bytes) = serde_json::to_vec(config) {
        h.write(&bytes);
    }
    // Fields the JSON serialization skips but an apply still applies:
    // cert/key PEM bytes and the override maps.
    for l in &config.gateway.listeners {
        if let Some(tls) = &l.tls {
            for cr in &tls.certificate_refs {
                h.write(cr.name.as_bytes());
                if let Some(pem) = &cr.cert_pem {
                    h.write(pem);
                }
                if let Some(pem) = &cr.key_pem {
                    h.write(pem);
                }
            }
        }
    }
    write_sorted(
        &mut h,
        config
            .endpoint_overrides
            .iter()
            .map(|(k, v)| format!("ep{:?}={:?}", k, v))
            .chain(
                config
                    .app_protocol_overrides
                    .iter()
                    .map(|(k, v)| format!("ap{:?}={}", k, v)),
            )
            .chain(
                config
                    .headless_target_ports
                    .iter()
                    .map(|(k, v)| format!("ht{:?}={}", k, v)),
            )
            .collect(),
    );
    for b in binds {
        h.write(format!("{:?}:{}", b.address, b.port).as_bytes());
        h.write(&[0]);
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Protocol;
    use crate::proxy::data_plane::NO_CERTIFICATE;
    use crate::sync::GatewayRef;

    fn lc(port: u16, target_port: Option<u16>) -> ListenerConfig {
        ListenerConfig {
            name: "l".to_string(),
            protocol: Protocol::HTTP,
            port,
            target_port,
            hostname: None,
            address: None,
            interface: None,
            tls: None,
            rate_limit: None,
        }
    }

    fn tcp(address: Option<&str>, port: u16) -> Endpoint {
        Endpoint {
            address: address.map(str::to_string),
            port,
            transport: Transport::Tcp,
        }
    }

    #[test]
    fn required_endpoints_keys_on_bound_port() {
        let listeners = [lc(80, Some(8000)), lc(8080, None)];
        let eps = required_endpoints(&listeners, &[]);
        assert!(eps.contains(&tcp(None, 8000)));
        assert!(eps.contains(&tcp(None, 8080)));
        assert!(
            !eps.contains(&tcp(None, 80)),
            "published port must not be the readiness key"
        );
    }

    #[test]
    fn required_endpoints_products_bind_addresses() {
        let listeners = [lc(443, Some(8001))];
        let eps = required_endpoints(
            &listeners,
            &["10.0.0.1".to_string(), "10.0.0.2".to_string()],
        );
        assert_eq!(eps.len(), 2);
        assert!(eps.contains(&tcp(Some("10.0.0.1"), 8001)));
        assert!(eps.contains(&tcp(Some("10.0.0.2"), 8001)));
    }

    /// A UDP listener needs a UDP socket: a TCP one on its port is not it.
    #[test]
    fn required_endpoints_carry_the_transport() {
        let udp = ListenerConfig {
            protocol: Protocol::UDP,
            ..lc(53, None)
        };
        let eps = required_endpoints(&[lc(53, None), udp], &[]);
        assert_eq!(
            eps.iter().map(|e| e.transport).collect::<Vec<_>>(),
            [Transport::Tcp, Transport::Udp]
        );
    }

    /// The skip gate is only sound if the fingerprint is deterministic and
    /// moves with everything the data plane applies, serde-skipped fields
    /// and the bind plan included.
    #[test]
    fn fingerprint_deterministic_and_sensitive() {
        let config = PortailConfig::default();
        let bind = vec!["10.0.0.1".to_string()];
        let fp = |c: &PortailConfig, bind: Option<&[String]>| {
            let served = Served {
                config: Arc::new(c.clone()),
                bind: bind.map(<[String]>::to_vec),
            };
            data_plane_fingerprint(c, &served.binds())
        };

        assert_eq!(fp(&config, Some(&bind)), fp(&config, Some(&bind)));

        // Wildcard, specific address and binding nothing are three plans.
        assert_ne!(fp(&config, Some(&bind)), fp(&config, Some(&[])));
        assert_ne!(fp(&config, Some(&[])), fp(&config, None));

        // Override maps are serde-skipped but change what the table resolves.
        let mut headless = config.clone();
        headless
            .headless_target_ports
            .insert(("db.prod.svc".into(), 5432), 15432);
        assert_ne!(fp(&config, None), fp(&headless, None));

        // So are the cert bytes a hot-reload must pick up.
        let with_cert = |pem: &[u8]| {
            let mut c = config.clone();
            c.gateway.listeners[0].tls = Some(crate::config::TlsConfig {
                mode: crate::config::TlsMode::Terminate,
                certificate_refs: vec![crate::config::CertificateRef {
                    name: "cert".into(),
                    hostname: None,
                    cert_pem: Some(pem.to_vec()),
                    key_pem: Some(b"K".to_vec()),
                    namespace: None,
                }],
            });
            c
        };
        assert_ne!(fp(&with_cert(b"A"), None), fp(&with_cert(b"B"), None));
    }

    fn applier() -> (Applier, Arc<AtomicBool>) {
        let perf = PerformanceConfig::default();
        let data_plane = DataPlane::new(&[], &perf, std::path::Path::new("/unused")).unwrap();
        let ready = Arc::new(AtomicBool::new(false));
        let applier = Applier::new(
            Arc::new(Mutex::new(data_plane)),
            Arc::new(ArcSwap::from_pointee(RouteTable::new())),
            perf,
            ready.clone(),
        );
        (applier, ready)
    }

    /// One HTTP and one TCP listener on free ports, wildcard-bound.
    fn snapshot(http: u16, tcp: u16, generation: i64) -> Snapshot {
        let config: PortailConfig = serde_json::from_value(serde_json::json!({
            "gateway": {"name": "gw", "listeners": [
                {"name": "http", "protocol": "HTTP", "port": http},
                {"name": "tcp", "protocol": "TCP", "port": tcp},
            ]},
            "tcpRoutes": [{
                "parentRefs": [{"name": "gw", "sectionName": "tcp"}],
                "rules": [{"backendRefs": [{"name": "127.0.0.1", "port": 9}]}],
            }],
        }))
        .unwrap();
        let gateway = GatewayRef {
            namespace: "default".into(),
            name: "gw".into(),
            uid: "gw-uid".into(),
            generation,
        };
        Snapshot::from_config(gateway, config, vec![])
    }

    fn free_ports() -> (u16, u16) {
        let http = crate::proxy::data_plane::test_ports::wildcard();
        let tcp = std::iter::repeat_with(crate::proxy::data_plane::test_ports::wildcard)
            .find(|&port| port != http)
            .unwrap();
        (http, tcp)
    }

    fn bindable(port: u16) -> bool {
        std::net::TcpListener::bind(("0.0.0.0", port)).is_ok()
    }

    /// The report names every listener's endpoints as bound, at the
    /// snapshot's version; /readyz follows.
    #[test]
    fn an_apply_binds_and_reports() {
        alone("an_apply_binds_and_reports", async {
            let (mut applier, ready) = applier();
            let (http, tcp) = free_ports();
            let snapshot = snapshot(http, tcp, 1);

            let report = applier.apply(&snapshot).await;
            assert_eq!(report.applied, snapshot.version);
            assert_eq!(report.apply_error, None);
            let endpoints: Vec<(&str, Option<&str>, u16, Transport, bool)> = report
                .listeners
                .iter()
                .flat_map(|l| {
                    l.endpoints.iter().map(move |e| {
                        (
                            l.name.as_str(),
                            e.address.as_deref(),
                            e.port,
                            e.transport,
                            e.bound,
                        )
                    })
                })
                .collect();
            assert_eq!(
                endpoints,
                [
                    ("http", None, http, Transport::Tcp, true),
                    ("tcp", None, tcp, Transport::Tcp, true),
                ]
            );
            assert!(ready.load(Ordering::Acquire));
            assert!(!bindable(http) && !bindable(tcp));
        });
    }

    /// A newer controller's snapshot, with a field this data plane does not
    /// know hashed into its version, applies and reports that version.
    #[tokio::test]
    async fn a_newer_controllers_snapshot_applies() {
        let (mut applier, _) = applier();
        let config: PortailConfig = serde_json::from_value(serde_json::json!({
            "gateway": {"name": "gw", "listeners": [
                {"name": "http", "protocol": "HTTP", "port": crate::proxy::data_plane::test_ports::wildcard()},
            ]},
        }))
        .unwrap();
        let gateway = GatewayRef {
            namespace: "default".into(),
            name: "gw".into(),
            uid: "gw-uid".into(),
            generation: 1,
        };
        let mut json =
            serde_json::to_value(Snapshot::from_config(gateway, config, vec![])).unwrap();
        json["runtime"] = serde_json::json!({"workers": 4});
        json["version"] = serde_json::json!("");
        let version =
            crate::sync::sha256_hex(&serde_json::to_vec(&json).unwrap())[..16].to_string();
        json["version"] = serde_json::json!(version);
        let received: Snapshot = serde_json::from_value(json).unwrap();

        let report = applier.apply(&received).await;
        assert_eq!(report.applied, version);
        assert_eq!(report.apply_error, None);
    }

    /// A generation bump with the same config rebuilds nothing.
    #[tokio::test]
    async fn an_unchanged_config_is_not_rebuilt() {
        let (mut applier, _) = applier();
        let (http, tcp) = free_ports();
        applier.apply(&snapshot(http, tcp, 1)).await;
        let table = applier.routes.load_full();

        let bumped = snapshot(http, tcp, 2);
        let report = applier.apply(&bumped).await;
        assert_eq!(report.applied, bumped.version);
        assert!(Arc::ptr_eq(&table, &applier.routes.load_full()));
    }

    /// A port held elsewhere is reported unbound, with the bind error.
    #[tokio::test]
    async fn a_held_port_is_reported_unbound() {
        let (mut applier, ready) = applier();
        // Held before the apply: another test probing the same port cannot
        // take it in between.
        let (tcp, _held) = std::iter::repeat_with(crate::proxy::data_plane::test_ports::wildcard)
            .find_map(|port| Some((port, std::net::TcpListener::bind(("0.0.0.0", port)).ok()?)))
            .unwrap();
        let http = crate::proxy::data_plane::test_ports::wildcard();

        let report = applier.apply(&snapshot(http, tcp, 1)).await;
        let tcp_endpoint = &report.listeners[1].endpoints[0];
        assert!(!tcp_endpoint.bound);
        assert!(
            tcp_endpoint
                .error
                .as_deref()
                .is_some_and(|e| e.contains("bind")),
            "{tcp_endpoint:?}"
        );
        assert!(!ready.load(Ordering::Acquire));
    }

    /// A TCP and a UDP listener on one port both bind, and the report says
    /// so per transport.
    #[tokio::test]
    async fn tcp_and_udp_on_one_port_both_report_bound() {
        let (mut applier, ready) = applier();
        let port = std::iter::repeat_with(crate::proxy::data_plane::test_ports::wildcard)
            .find(|&port| std::net::UdpSocket::bind(("0.0.0.0", port)).is_ok())
            .unwrap();
        let config: PortailConfig = serde_json::from_value(serde_json::json!({
            "gateway": {"name": "gw", "listeners": [
                {"name": "tcp", "protocol": "TCP", "port": port},
                {"name": "udp", "protocol": "UDP", "port": port},
            ]},
        }))
        .unwrap();
        let gateway = GatewayRef {
            namespace: "default".into(),
            name: "gw".into(),
            uid: "gw-uid".into(),
            generation: 1,
        };

        let report = applier
            .apply(&Snapshot::from_config(gateway, config, vec![]))
            .await;
        let endpoints: Vec<(&str, Transport, bool)> = report
            .listeners
            .iter()
            .flat_map(|l| {
                l.endpoints
                    .iter()
                    .map(move |e| (l.name.as_str(), e.transport, e.bound))
            })
            .collect();
        assert_eq!(
            endpoints,
            [("tcp", Transport::Tcp, true), ("udp", Transport::Udp, true)]
        );
        assert!(ready.load(Ordering::Acquire));
    }

    /// An HTTPS listener with `refs` (Secret name, cert PEM, key PEM) and
    /// an HTTP one, on `(https, http)`.
    fn https_and_http((https, http): (u16, u16), refs: &[(&str, &[u8], &[u8])]) -> Snapshot {
        let names: Vec<_> = refs
            .iter()
            .map(|(name, ..)| serde_json::json!({"name": name}))
            .collect();
        let mut config: PortailConfig = serde_json::from_value(serde_json::json!({
            "gateway": {"name": "gw", "listeners": [
                {"name": "https", "protocol": "HTTPS", "port": https,
                 "tls": {"mode": "Terminate", "certificateRefs": names}},
                {"name": "http", "protocol": "HTTP", "port": http},
            ]},
        }))
        .unwrap();
        let tls = config.gateway.listeners[0].tls.as_mut().unwrap();
        for (r, (_, cert, key)) in tls.certificate_refs.iter_mut().zip(refs) {
            r.cert_pem = Some(cert.to_vec());
            r.key_pem = Some(key.to_vec());
        }
        let gateway = GatewayRef {
            namespace: "default".into(),
            name: "gw".into(),
            uid: "gw-uid".into(),
            generation: 1,
        };
        Snapshot::from_config(gateway, config, vec![])
    }

    /// An HTTPS listener whose certs fail to build terminates nothing, as
    /// a revoked one: out of readiness, reported unbound with the build
    /// error, its port closed on a running data plane, and the pod's other
    /// listeners serve on. It counted for readiness, and the whole pod went
    /// NotReady.
    #[tokio::test]
    async fn a_listener_whose_certs_fail_keeps_the_pod_ready() {
        let bad: (&str, &[u8], &[u8]) = ("bad", b"not a cert", b"not a key");
        let (mut fresh, fresh_ready) = applier();
        let report = apply_bound(
            &mut fresh,
            &fresh_ready,
            &https_and_http(free_ports(), &[bad]),
        )
        .await;
        let https = &report.listeners[0].endpoints[0];
        assert!(!https.bound);
        assert!(
            https
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with("TLS config build failed")),
            "{https:?}"
        );
        assert!(report.listeners[1].endpoints[0].bound);

        let Some((cert, key)) = crate::proxy::tls::test_util::generate_test_cert("app.test") else {
            return; // openssl unavailable
        };
        let (mut running, running_ready) = applier();
        let ports = free_ports();
        let good = https_and_http(ports, &[("good", &cert, &key)]);
        apply_bound(&mut running, &running_ready, &good).await;
        let report = running.apply(&https_and_http(ports, &[bad])).await;
        assert!(running_ready.load(Ordering::Acquire), "{report:?}");
        assert!(!report.listeners[0].endpoints[0].bound);
        assert!(report.listeners[1].endpoints[0].bound);
    }

    /// A revoked listener (no certificateRef left) is out of readiness and
    /// reported unbound with why, whether the data plane starts with it or
    /// served its certs before: the binds and the report end the same.
    #[tokio::test]
    async fn a_revoked_listener_ends_the_same_fresh_or_running() {
        let Some((cert, key)) = crate::proxy::tls::test_util::generate_test_cert("app.test") else {
            return; // openssl unavailable
        };
        let snapshot = |ports, with_cert: bool| match with_cert {
            true => https_and_http(ports, &[("cert", &cert, &key)]),
            false => https_and_http(ports, &[]),
        };
        // Ports differ between the two: neither binds one the other just
        // closed.
        let shape = |report: &InstanceReport| -> Vec<(String, bool, Option<String>)> {
            report
                .listeners
                .iter()
                .flat_map(|l| {
                    l.endpoints
                        .iter()
                        .map(|e| (l.name.clone(), e.bound, e.error.clone()))
                })
                .collect()
        };
        let https_endpoint = |port| Endpoint {
            address: None,
            port,
            transport: Transport::Tcp,
        };

        let (mut fresh, fresh_ready) = applier();
        let ports = free_ports();
        let fresh_report = apply_bound(&mut fresh, &fresh_ready, &snapshot(ports, false)).await;
        let fresh_bound =
            lock(&fresh.data_plane).is_ready_for_endpoints(&[https_endpoint(ports.0)]);
        assert!(!fresh_bound);

        let (mut running, running_ready) = applier();
        let ports = free_ports();
        apply_bound(&mut running, &running_ready, &snapshot(ports, true)).await;
        assert!(lock(&running.data_plane).is_ready_for_endpoints(&[https_endpoint(ports.0)]));
        let running_report = running.apply(&snapshot(ports, false)).await;
        assert!(running_ready.load(Ordering::Acquire));
        assert_eq!(shape(&running_report), shape(&fresh_report));
        assert_eq!(
            shape(&running_report),
            [
                ("https".to_string(), false, Some(NO_CERTIFICATE.to_string())),
                ("http".to_string(), true, None),
            ]
        );
        assert!(!lock(&running.data_plane).is_ready_for_endpoints(&[https_endpoint(ports.0)]));
        let closing = lock(&running.data_plane).close_unwanted(&[]);
        for accept_loop in closing {
            accept_loop.await.unwrap();
        }
        // A child another test forks holds this process's sockets until
        // its exec.
        for _ in 0..200 {
            if bindable(ports.0) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("the running data plane kept the revoked port");
    }

    /// Apply until every endpoint binds: a probe socket of this test's can
    /// sit in a child another test forks until its exec.
    async fn apply_bound(
        applier: &mut Applier,
        ready: &AtomicBool,
        snapshot: &Snapshot,
    ) -> InstanceReport {
        for _ in 0..200 {
            let report = applier.apply(snapshot).await;
            if ready.load(Ordering::Acquire) {
                return report;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("{} never bound", snapshot.version);
    }

    /// Runs `test` in a process of its own, on a current-thread runtime:
    /// it binds a port it just saw freed, and a child another test spawns
    /// holds this process's sockets until its exec.
    fn alone(name: &str, test: impl std::future::Future<Output = ()>) {
        if crate::proxy::test_util::in_own_process(module_path!(), name) {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(test);
        }
    }

    /// A 404 closes every listener and turns NotReady; the next snapshot
    /// binds again.
    #[test]
    fn clear_closes_every_listener() {
        alone("clear_closes_every_listener", async {
            let (mut applier, ready) = applier();
            let (http, tcp) = free_ports();
            let snapshot = snapshot(http, tcp, 1);
            applier.apply(&snapshot).await;

            applier.clear().await;
            assert!(!ready.load(Ordering::Acquire));
            assert!(applier.published.load().is_none());
            assert!(
                bindable(http) && bindable(tcp),
                "the cleared ports are released"
            );

            applier.apply(&snapshot).await;
            assert!(ready.load(Ordering::Acquire), "bound again after a clear");
        });
    }
}
