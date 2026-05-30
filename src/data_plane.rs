//! Tokio-based data plane — one TcpListener (or UdpSocket) per (address, port),
//! one accept loop per listener. Accept work is distributed across the Tokio
//! runtime's worker threads, not via per-core SO_REUSEPORT fan-out (which adds
//! task count without throughput on a single shared runtime).
//!
//! On shutdown, the data plane stops accepting new connections and waits up to
//! `DRAIN_TIMEOUT` for in-flight connections to finish.

use anyhow::Result;
use arc_swap::ArcSwap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, UdpSocket};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::{CertificateRef, ListenerConfig, PerformanceConfig, Protocol, TlsMode};
use crate::health::HealthRegistry;
use crate::logging::{info, warn};
use crate::routing::{BackendSelector, RouteTable};
use crate::tls::{self, DynamicTlsAcceptor};
use crate::udp_worker;
use crate::worker;

struct TcpListenerEntry {
    port: u16,
    listener: TcpListener,
    tls_acceptor: Option<Arc<DynamicTlsAcceptor>>,
    tls_passthrough: bool,
}

struct UdpListenerEntry {
    port: u16,
    socket: UdpSocket,
}

pub struct DataPlane {
    tcp_listeners: Vec<TcpListenerEntry>,
    udp_listeners: Vec<UdpListenerEntry>,
    task_handles: Vec<JoinHandle<()>>,
    max_idle_per_backend: usize,
    connect_timeout: Duration,
    health: Arc<HealthRegistry>,
    udp_session_timeout: Duration,
    shutdown: CancellationToken,
    active_connections: Arc<AtomicUsize>,
    bound_endpoints: std::collections::HashSet<(Option<String>, u16)>,
    /// Per-port dynamic TLS acceptors for hot-reload support.
    tls_configs: std::collections::HashMap<u16, Arc<DynamicTlsAcceptor>>,
    /// Per-port cert fingerprint to skip no-op TLS reloads.
    tls_cert_hashes: std::collections::HashMap<u16, u64>,
    /// Shared backend selector across all listeners for correct weighted distribution.
    selector: Arc<BackendSelector>,
}

/// Maximum time to wait for in-flight connections on shutdown.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Merge TLS cert refs across all Terminate listeners on the same port.
///
/// One TCP listener per port serves traffic for every (port, hostname) scope,
/// so its `ServerConfig` must hold every hostname's cert. Building per-listener
/// would let the last write clobber earlier listeners and strand SNIs — e.g.
/// `example.org` via a catch-all listener gets dropped when a sibling
/// `https-with-hostname` listener is processed last, hanging its TLS handshake
/// until the client times out.
fn merge_cert_refs_by_port(
    listener_configs: &[ListenerConfig],
) -> std::collections::HashMap<u16, Vec<CertificateRef>> {
    let mut merged: std::collections::HashMap<u16, Vec<CertificateRef>> =
        std::collections::HashMap::new();
    for listener_cfg in listener_configs {
        let Some(tls_cfg) = &listener_cfg.tls else {
            continue;
        };
        if tls_cfg.mode != TlsMode::Terminate {
            continue;
        }
        if !matches!(listener_cfg.protocol, Protocol::HTTPS | Protocol::TLS) {
            continue;
        }
        merged
            .entry(listener_cfg.port)
            .or_default()
            .extend(tls_cfg.certificate_refs.iter().cloned());
    }
    merged
}

/// Compute a fingerprint of all cert PEM bytes for change detection.
/// Uses FNV-1a for speed — collision resistance isn't critical here,
/// a false positive just triggers one extra ServerConfig rebuild.
fn compute_cert_fingerprint(refs: &[CertificateRef]) -> u64 {
    use std::hash::Hasher;
    let mut hasher = fnv::FnvHasher::default();
    for r in refs {
        if let Some(pem) = &r.cert_pem {
            hasher.write(pem);
        }
        if let Some(pem) = &r.key_pem {
            hasher.write(pem);
        }
        hasher.write(r.name.as_bytes());
        if let Some(h) = &r.hostname {
            hasher.write(h.as_bytes());
        }
    }
    hasher.finish()
}

impl DataPlane {
    /// Create one TcpListener / UdpSocket per (address, port) listener config.
    /// The Tokio runtime multiplexes accepts across its worker threads; no
    /// per-core SO_REUSEPORT fan-out is needed for a single-process data plane.
    pub fn new(
        listeners: &[ListenerConfig],
        performance_config: &PerformanceConfig,
        cert_dir: &std::path::Path,
    ) -> Result<Self> {
        let health = Arc::new(HealthRegistry::new());

        let mut tcp_listeners = Vec::new();
        let mut udp_listeners = Vec::new();

        // Pre-build TLS acceptors (one per listener config that needs TLS)
        let mut tls_acceptors: Vec<Option<Arc<DynamicTlsAcceptor>>> =
            Vec::with_capacity(listeners.len());
        let mut tls_passthrough_flags: Vec<bool> = Vec::with_capacity(listeners.len());

        for listener_cfg in listeners {
            match (&listener_cfg.protocol, &listener_cfg.tls) {
                (Protocol::HTTPS, Some(tls_cfg)) if tls_cfg.mode == TlsMode::Terminate => {
                    let config = tls::build_server_config(&tls_cfg.certificate_refs, cert_dir)?;
                    tls_acceptors.push(Some(Arc::new(DynamicTlsAcceptor::new(config))));
                    tls_passthrough_flags.push(false);
                }
                (Protocol::TLS, Some(tls_cfg)) if tls_cfg.mode == TlsMode::Passthrough => {
                    tls_acceptors.push(None);
                    tls_passthrough_flags.push(true);
                }
                _ => {
                    tls_acceptors.push(None);
                    tls_passthrough_flags.push(false);
                }
            }
        }

        for (i, listener_cfg) in listeners.iter().enumerate() {
            // `port` is the published identity (routing/TLS key); `bound_port` is
            // the socket we actually bind — the Service's targetPort when set.
            let port = listener_cfg.port;
            let bound_port = listener_cfg.target_port.unwrap_or(port);
            match listener_cfg.protocol {
                Protocol::HTTP | Protocol::HTTPS | Protocol::TCP | Protocol::TLS => {
                    let std_listener = create_tcp_listener(
                        bound_port,
                        listener_cfg.address.as_deref(),
                        listener_cfg.interface.as_deref(),
                    )?;
                    let tokio_listener = TcpListener::from_std(std_listener)?;

                    tcp_listeners.push(TcpListenerEntry {
                        port,
                        listener: tokio_listener,
                        tls_acceptor: tls_acceptors[i].clone(),
                        tls_passthrough: tls_passthrough_flags[i],
                    });
                }
                Protocol::UDP => {
                    let std_socket = create_udp_socket(
                        bound_port,
                        listener_cfg.address.as_deref(),
                        listener_cfg.interface.as_deref(),
                    )?;
                    let tokio_socket = UdpSocket::from_std(std_socket)?;

                    udp_listeners.push(UdpListenerEntry {
                        port,
                        socket: tokio_socket,
                    });
                }
            }
        }

        // Readiness tracks the bound socket, so key endpoints on the bound port.
        let bound_endpoints: std::collections::HashSet<(Option<String>, u16)> = listeners
            .iter()
            .map(|l| (l.address.clone(), l.target_port.unwrap_or(l.port)))
            .collect();

        Ok(Self {
            tcp_listeners,
            udp_listeners,
            task_handles: Vec::new(),
            max_idle_per_backend: 64,
            connect_timeout: performance_config.backend_timeout,
            health,
            udp_session_timeout: performance_config.udp_session_timeout,
            shutdown: CancellationToken::new(),
            active_connections: Arc::new(AtomicUsize::new(0)),
            bound_endpoints,
            tls_configs: std::collections::HashMap::new(),
            tls_cert_hashes: std::collections::HashMap::new(),
            selector: Arc::new(BackendSelector::new()),
        })
    }

    /// Dynamically add TCP/UDP listeners for new ports. Called by the K8s
    /// controller when new Gateway ports are discovered. One listener per
    /// (address, port); accept work is distributed by the Tokio runtime.
    /// Builds TLS acceptors from in-memory cert data when listeners use
    /// HTTPS/TLS. For already-bound ports, updates the TLS config via
    /// `DynamicTlsAcceptor` hot-reload. Returns `(ports_opened, errors)`.
    pub fn add_tcp_listeners(
        &mut self,
        listener_configs: &[ListenerConfig],
        addresses: &[String],
        routes: Arc<ArcSwap<RouteTable>>,
        performance_config: &crate::config::PerformanceConfig,
    ) -> (usize, Vec<(u16, String)>) {
        let mut opened = 0usize;
        let mut errors = Vec::new();

        // If addresses are specified, bind each listener to each address.
        // Otherwise, bind to the listener's own address (or 0.0.0.0).
        let bind_addrs: Vec<Option<&str>> = if addresses.is_empty() {
            vec![None]
        } else {
            addresses.iter().map(|a| Some(a.as_str())).collect()
        };

        // Merge TLS cert refs across all Terminate listeners on the same port
        // (see `merge_cert_refs_by_port`).
        let merged_cert_refs = merge_cert_refs_by_port(listener_configs);

        for listener_cfg in listener_configs {
            // `port` is the published identity (routing/TLS key); `bound_port` is
            // the socket we actually bind — the Service's targetPort when set.
            let port = listener_cfg.port;
            let bound_port = listener_cfg.target_port.unwrap_or(port);

            // Build DynamicTlsAcceptor if this listener needs TLS termination.
            // Uses the per-port merged cert refs so multi-listener ports get
            // every hostname's cert in one ServerConfig (see merge above).
            let (tls_acceptor, tls_passthrough) = match (&listener_cfg.protocol, &listener_cfg.tls)
            {
                (Protocol::HTTPS, Some(tls_cfg)) | (Protocol::TLS, Some(tls_cfg))
                    if tls_cfg.mode == TlsMode::Terminate =>
                {
                    let port_refs = merged_cert_refs
                        .get(&port)
                        .map(|v| v.as_slice())
                        .unwrap_or(&tls_cfg.certificate_refs);
                    match tls::build_server_config(port_refs, std::path::Path::new("/unused")) {
                        Ok(config) => {
                            // If port is already bound, hot-reload the TLS config only if certs changed
                            if let Some(existing) = self.tls_configs.get(&port) {
                                let fingerprint = compute_cert_fingerprint(port_refs);
                                if self.tls_cert_hashes.get(&port) == Some(&fingerprint) {
                                    continue; // Certs unchanged — skip hot-reload
                                }
                                existing.update(config);
                                self.tls_cert_hashes.insert(port, fingerprint);
                                info!("Hot-reloaded TLS config for port {}", port);
                                continue; // Port already has a listener, just update TLS
                            }
                            self.tls_cert_hashes
                                .insert(port, compute_cert_fingerprint(port_refs));
                            let dyn_acceptor = Arc::new(DynamicTlsAcceptor::new(config));
                            self.tls_configs.insert(port, dyn_acceptor.clone());
                            (Some(dyn_acceptor), false)
                        }
                        Err(e) => {
                            errors.push((port, format!("TLS config build failed: {}", e)));
                            continue;
                        }
                    }
                }
                (Protocol::TLS, Some(tls_cfg)) if tls_cfg.mode == TlsMode::Passthrough => {
                    (None, true)
                }
                _ => (None, false),
            };

            for &bind_addr in &bind_addrs {
                let effective_addr = bind_addr.or(listener_cfg.address.as_deref());
                let endpoint = (effective_addr.map(String::from), bound_port);
                if self.bound_endpoints.contains(&endpoint) {
                    continue; // Endpoint already bound
                }

                let mut port_ok = true;
                if listener_cfg.protocol == Protocol::UDP {
                    match create_udp_socket(
                        bound_port,
                        effective_addr,
                        listener_cfg.interface.as_deref(),
                    ) {
                        Ok(std_socket) => match UdpSocket::from_std(std_socket) {
                            Ok(tokio_socket) => {
                                let routes = routes.clone();
                                let health = self.health.clone();
                                let shutdown = self.shutdown.clone();
                                let session_timeout = self.udp_session_timeout;
                                let socket = Arc::new(tokio_socket);

                                let handle = tokio::spawn(async move {
                                    udp_worker::run_udp_worker(
                                        socket,
                                        port,
                                        routes,
                                        session_timeout,
                                        shutdown,
                                        health,
                                    )
                                    .await;
                                });
                                self.task_handles.push(handle);
                            }
                            Err(e) => {
                                errors.push((port, format!("UDP from_std: {}", e)));
                                port_ok = false;
                            }
                        },
                        Err(e) => {
                            errors.push((port, format!("UDP bind: {}", e)));
                            port_ok = false;
                        }
                    }
                } else {
                    match create_tcp_listener(
                        bound_port,
                        effective_addr,
                        listener_cfg.interface.as_deref(),
                    ) {
                        Ok(std_listener) => match TcpListener::from_std(std_listener) {
                            Ok(tokio_listener) => {
                                let routes = routes.clone();
                                let health = self.health.clone();
                                let shutdown = self.shutdown.clone();
                                let max_idle = self.max_idle_per_backend;
                                let connect_timeout = performance_config.backend_timeout;
                                let acceptor = tls_acceptor.clone();
                                let passthrough = tls_passthrough;

                                let selector = self.selector.clone();
                                let handle = tokio::spawn(async move {
                                    worker::run_worker(
                                        tokio_listener,
                                        port,
                                        routes,
                                        max_idle,
                                        connect_timeout,
                                        shutdown,
                                        acceptor,
                                        passthrough,
                                        health,
                                        selector,
                                    )
                                    .await;
                                });
                                self.task_handles.push(handle);
                            }
                            Err(e) => {
                                errors.push((port, format!("TCP from_std: {}", e)));
                                port_ok = false;
                            }
                        },
                        Err(e) => {
                            errors.push((port, format!("TCP bind: {}", e)));
                            port_ok = false;
                        }
                    }
                }
                if port_ok {
                    self.bound_endpoints.insert(endpoint);
                    opened += 1;
                }
            } // end for bind_addr
        }
        (opened, errors)
    }

    /// Start async listener tasks (one accept loop per TCP listener / UDP socket).
    pub fn start(&mut self, routes: Arc<ArcSwap<RouteTable>>) {
        let tcp_count = self.tcp_listeners.len();
        let udp_count = self.udp_listeners.len();

        for entry in self.tcp_listeners.drain(..) {
            let routes = routes.clone();
            let health = self.health.clone();
            let shutdown = self.shutdown.clone();
            let max_idle = self.max_idle_per_backend;
            let connect_timeout = self.connect_timeout;
            let selector = self.selector.clone();

            let handle = tokio::spawn(async move {
                worker::run_worker(
                    entry.listener,
                    entry.port,
                    routes,
                    max_idle,
                    connect_timeout,
                    shutdown,
                    entry.tls_acceptor,
                    entry.tls_passthrough,
                    health,
                    selector,
                )
                .await;
            });

            self.task_handles.push(handle);
        }

        for entry in self.udp_listeners.drain(..) {
            let socket = Arc::new(entry.socket);
            let routes = routes.clone();
            let health = self.health.clone();
            let shutdown = self.shutdown.clone();
            let session_timeout = self.udp_session_timeout;

            let handle = tokio::spawn(async move {
                udp_worker::run_udp_worker(
                    socket,
                    entry.port,
                    routes,
                    session_timeout,
                    shutdown,
                    health,
                )
                .await;
            });

            self.task_handles.push(handle);
        }

        info!(
            "Data plane started: {} TCP + {} UDP listener tasks",
            tcp_count, udp_count
        );
    }

    /// Signal shutdown and wait for in-flight connections to drain (up to 30s).
    pub async fn shutdown(self) {
        info!("Data plane shutting down — draining in-flight connections");
        self.shutdown.cancel();

        // Wait for active connections to drain, with timeout
        let active = self.active_connections.clone();
        let drain_result = tokio::time::timeout(DRAIN_TIMEOUT, async {
            loop {
                let count = active.load(Ordering::Acquire);
                if count == 0 {
                    break;
                }
                info!("Waiting for {} in-flight connection(s) to drain", count);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        })
        .await;

        match drain_result {
            Ok(()) => info!("All connections drained"),
            Err(_) => {
                let remaining = active.load(Ordering::Acquire);
                warn!(
                    "Drain timeout after {:?}: {} connections still active",
                    DRAIN_TIMEOUT, remaining
                );
            }
        }

        for handle in self.task_handles {
            let _ = handle.await;
        }

        info!("Data plane shutdown complete");
    }

    /// Check if all the specified (address, port) endpoints are bound and ready to serve traffic.
    pub fn is_ready_for_endpoints(&self, required: &[(Option<String>, u16)]) -> bool {
        required.iter().all(|ep| self.bound_endpoints.contains(ep))
    }

    /// Get a reference to the shared health registry.
    pub fn health(&self) -> &Arc<HealthRegistry> {
        &self.health
    }
}

fn create_tcp_listener(
    port: u16,
    bind_addr: Option<&str>,
    interface: Option<&str>,
) -> Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};

    let ip: std::net::IpAddr = bind_addr.unwrap_or("0.0.0.0").parse().map_err(|e| {
        anyhow::anyhow!("Invalid bind address '{}': {}", bind_addr.unwrap_or(""), e)
    })?;
    let domain = if ip.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_nonblocking(true)?;
    // SO_REUSEADDR lets a restart re-bind a port still held in TIME-WAIT by an
    // older connection. Standard for any long-running proxy / server; unrelated
    // to SO_REUSEPORT (per-core fan-out, removed in c010a19). Without this, a
    // graceful restart waits up to net.ipv4.tcp_fin_timeout (60s by default)
    // per port before it can accept again.
    socket.set_reuse_address(true)?;

    if let Some(iface) = interface {
        socket.bind_device(Some(iface.as_bytes()))?;
    }

    let addr = std::net::SocketAddr::new(ip, port);
    socket.bind(&addr.into())?;
    socket.listen(1024)?;

    Ok(socket.into())
}

fn create_udp_socket(
    port: u16,
    bind_addr: Option<&str>,
    interface: Option<&str>,
) -> Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};

    let ip: std::net::IpAddr = bind_addr.unwrap_or("0.0.0.0").parse().map_err(|e| {
        anyhow::anyhow!("Invalid bind address '{}': {}", bind_addr.unwrap_or(""), e)
    })?;
    let domain = if ip.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_nonblocking(true)?;

    if let Some(iface) = interface {
        socket.bind_device(Some(iface.as_bytes()))?;
    }

    let addr = std::net::SocketAddr::new(ip, port);
    socket.bind(&addr.into())?;

    Ok(socket.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CertificateRef, ListenerConfig, Protocol as P, TlsConfig, TlsMode as TM};

    fn https_listener(
        name: &str,
        port: u16,
        hostname: Option<&str>,
        cert_name: &str,
    ) -> ListenerConfig {
        ListenerConfig {
            name: name.into(),
            protocol: P::HTTPS,
            port,
            target_port: None,
            hostname: hostname.map(str::to_string),
            address: None,
            interface: None,
            tls: Some(TlsConfig {
                mode: TM::Terminate,
                certificate_refs: vec![CertificateRef {
                    name: cert_name.into(),
                    hostname: hostname.map(str::to_string),
                    cert_pem: Some(b"PEM-CERT".to_vec()),
                    key_pem: Some(b"PEM-KEY".to_vec()),
                }],
            }),
        }
    }

    /// Two TLS-Terminate listeners on the same port — one catch-all, one
    /// hostname-scoped — must produce one merged cert-ref list under that
    /// port, in listener order. Regression for the bug where per-listener
    /// `existing.update(config)` clobbered sibling SNI bindings and hung
    /// the catch-all's TLS (Gateway API conformance HTTPRouteHTTPSListener).
    #[test]
    fn merges_same_port_listeners_into_one_cert_list() {
        let listeners = vec![
            https_listener("https", 443, None, "default-cert"),
            https_listener(
                "https-with-hostname",
                443,
                Some("second.example.org"),
                "default-cert",
            ),
        ];
        let merged = merge_cert_refs_by_port(&listeners);

        assert_eq!(merged.len(), 1, "one port expected");
        let refs = &merged[&443];
        assert_eq!(refs.len(), 2, "both listeners' refs must appear");
        assert_eq!(
            refs[0].hostname, None,
            "catch-all listener's ref comes first"
        );
        assert_eq!(
            refs[1].hostname.as_deref(),
            Some("second.example.org"),
            "hostname-scoped listener's ref preserves its hostname"
        );
    }

    /// Listeners on different ports stay isolated; non-TLS / Passthrough
    /// listeners contribute nothing.
    #[test]
    fn merge_respects_port_and_mode() {
        let mut passthrough = https_listener("tlsp", 9443, Some("passthrough.example.org"), "p");
        passthrough.protocol = P::TLS;
        passthrough.tls.as_mut().unwrap().mode = TM::Passthrough;

        let listeners = vec![
            https_listener("a", 443, None, "ca"),
            https_listener("b", 8443, Some("b.example.org"), "cb"),
            passthrough,
            ListenerConfig {
                name: "http-only".into(),
                protocol: P::HTTP,
                port: 80,
                target_port: None,
                hostname: None,
                address: None,
                interface: None,
                tls: None,
            },
        ];
        let merged = merge_cert_refs_by_port(&listeners);

        assert_eq!(merged.keys().copied().collect::<Vec<_>>().len(), 2);
        assert!(merged.contains_key(&443));
        assert!(merged.contains_key(&8443));
        assert!(
            !merged.contains_key(&9443),
            "Passthrough must not contribute"
        );
        assert!(!merged.contains_key(&80), "HTTP must not contribute");
    }

    /// Fingerprint over a port's merged refs must be stable across calls —
    /// if it weren't, every reconcile would flip the cache between siblings
    /// and re-trigger hot-reloads forever (the symptom that originally
    /// surfaced the bug in `Hot-reloaded TLS config for port 443` log spam).
    #[test]
    fn merged_fingerprint_is_stable_across_calls() {
        let listeners = vec![
            https_listener("https", 443, None, "c"),
            https_listener("https-with-hostname", 443, Some("h.example.org"), "c"),
        ];
        let m1 = merge_cert_refs_by_port(&listeners);
        let m2 = merge_cert_refs_by_port(&listeners);
        assert_eq!(
            compute_cert_fingerprint(&m1[&443]),
            compute_cert_fingerprint(&m2[&443]),
            "deterministic input must hash deterministically",
        );
    }

    /// A listener with a `target_port` binds that port (the Service's
    /// targetPort), not the published `port`, and readiness keys on it.
    #[tokio::test]
    async fn binds_target_port_not_published() {
        let listeners = vec![ListenerConfig {
            name: "http".into(),
            protocol: P::HTTP,
            port: 21080,
            target_port: Some(31080),
            hostname: None,
            address: None,
            interface: None,
            tls: None,
        }];
        let perf = crate::config::PerformanceConfig {
            backend_timeout: std::time::Duration::from_secs(1),
            udp_session_timeout: std::time::Duration::from_secs(1),
        };
        let dp = DataPlane::new(&listeners, &perf, std::path::Path::new("/unused")).unwrap();

        // Readiness keys on the bound (target) port, not the published port.
        assert!(dp.is_ready_for_endpoints(&[(None, 31080)]));
        assert!(!dp.is_ready_for_endpoints(&[(None, 21080)]));

        // The socket really landed on the target port (held by the data plane)...
        assert!(
            std::net::TcpListener::bind(("0.0.0.0", 31080u16)).is_err(),
            "target port must be bound by the data plane"
        );
        // ...and the published port was never bound.
        assert!(
            std::net::TcpListener::bind(("0.0.0.0", 21080u16)).is_ok(),
            "published port must not be bound"
        );

        drop(dp);
    }
}
