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
            let port = listener_cfg.port;
            match listener_cfg.protocol {
                Protocol::HTTP | Protocol::HTTPS | Protocol::TCP | Protocol::TLS => {
                    let std_listener = create_tcp_listener(
                        port,
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
                        port,
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

        let bound_endpoints: std::collections::HashSet<(Option<String>, u16)> = listeners
            .iter()
            .map(|l| (l.address.clone(), l.port))
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

        // Merge TLS cert refs across all Terminate listeners on the same port —
        // one TCP listener per port serves traffic for every (port, hostname)
        // scope, so its ServerConfig must hold every hostname's cert. Building
        // per-listener would let the last write clobber earlier listeners and
        // strand SNIs (e.g. `example.org` via catch-all listener gets dropped
        // when `https-with-hostname` is processed last, hanging its TLS).
        let mut merged_cert_refs: std::collections::HashMap<u16, Vec<CertificateRef>> =
            std::collections::HashMap::new();
        for listener_cfg in listener_configs {
            if let Some(tls_cfg) = &listener_cfg.tls {
                if tls_cfg.mode == TlsMode::Terminate
                    && matches!(listener_cfg.protocol, Protocol::HTTPS | Protocol::TLS)
                {
                    merged_cert_refs
                        .entry(listener_cfg.port)
                        .or_default()
                        .extend(tls_cfg.certificate_refs.iter().cloned());
                }
            }
        }

        for listener_cfg in listener_configs {
            let port = listener_cfg.port;

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
                let endpoint = (effective_addr.map(String::from), port);
                if self.bound_endpoints.contains(&endpoint) {
                    continue; // Endpoint already bound
                }

                let mut port_ok = true;
                if listener_cfg.protocol == Protocol::UDP {
                    match create_udp_socket(port, effective_addr, listener_cfg.interface.as_deref())
                    {
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
                        port,
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

    /// Update TLS configs for all listeners from the latest reconciled config.
    /// Called on every reconcile to pick up Secret changes (cert-manager rotation, etc.).
    /// Returns the number of ports updated and any errors.
    pub fn update_tls_configs(
        &mut self,
        listener_configs: &[ListenerConfig],
    ) -> (usize, Vec<(u16, String)>) {
        let mut updated = 0;
        let mut errors = Vec::new();
        for listener_cfg in listener_configs {
            let port = listener_cfg.port;
            match (&listener_cfg.protocol, &listener_cfg.tls) {
                (Protocol::HTTPS, Some(tls_cfg)) | (Protocol::TLS, Some(tls_cfg))
                    if tls_cfg.mode == TlsMode::Terminate =>
                {
                    if let Some(existing) = self.tls_configs.get(&port) {
                        let fingerprint = compute_cert_fingerprint(&tls_cfg.certificate_refs);
                        if self.tls_cert_hashes.get(&port) == Some(&fingerprint) {
                            continue; // Certs unchanged — skip hot-reload
                        }
                        match tls::build_server_config(
                            &tls_cfg.certificate_refs,
                            std::path::Path::new("/unused"),
                        ) {
                            Ok(config) => {
                                existing.update(config);
                                self.tls_cert_hashes.insert(port, fingerprint);
                                updated += 1;
                            }
                            Err(e) => {
                                errors.push((port, format!("TLS config rebuild failed: {}", e)));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        (updated, errors)
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
