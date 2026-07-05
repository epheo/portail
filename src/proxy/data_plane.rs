//! Tokio-based data plane — one TcpListener (or UdpSocket) per (address, port),
//! one accept loop per listener. Accept work is distributed across the Tokio
//! runtime's worker threads, not via per-core SO_REUSEPORT fan-out (which adds
//! task count without throughput on a single shared runtime).
//!
//! On shutdown, the data plane stops accepting new connections and waits up to
//! `DRAIN_TIMEOUT` for in-flight connections to finish.

use anyhow::Result;
use arc_swap::ArcSwap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, UdpSocket};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::{CertificateRef, ListenerConfig, PerformanceConfig, Protocol, TlsMode};
use crate::logging::{info, warn};
use crate::proxy::health::HealthRegistry;
use crate::proxy::tls::{self, DynamicTlsAcceptor};
use crate::proxy::udp_worker;
use crate::proxy::worker;
use crate::routing::{BackendSelector, RouteTable};

struct TcpListenerEntry {
    port: u16,
    /// Socket actually bound (the Service targetPort when decoupled from `port`).
    bound_port: u16,
    address: Option<String>,
    listener: TcpListener,
    tls_acceptor: Option<Arc<DynamicTlsAcceptor>>,
    tls_passthrough: bool,
}

struct UdpListenerEntry {
    port: u16,
    bound_port: u16,
    address: Option<String>,
    socket: UdpSocket,
}

/// State of one bound (address, port) endpoint.
enum ListenerSlot {
    /// Socket bound at construction, accept loop not yet spawned — the
    /// standalone-mode window between `new()` and `start()`. Counts as
    /// bound: the kernel is already queueing connections on the socket.
    Pending,
    Running(JoinHandle<()>),
}

impl ListenerSlot {
    fn alive(&self) -> bool {
        match self {
            ListenerSlot::Pending => true,
            ListenerSlot::Running(handle) => !handle.is_finished(),
        }
    }
}

pub struct DataPlane {
    tcp_listeners: Vec<TcpListenerEntry>,
    udp_listeners: Vec<UdpListenerEntry>,
    /// One accept-loop slot per bound (address, bound-port) endpoint.
    /// Readiness reads LIVE task state from this map — a finished task no
    /// longer counts as bound, which keeps /readyz and Programmed truthful
    /// when an accept loop dies. (Its add-only predecessor had no way back:
    /// a dead listener stayed "bound" until pod restart, refusing every
    /// connection while readiness stayed green.)
    listener_tasks: std::collections::HashMap<(Option<String>, u16), ListenerSlot>,
    /// Pod readiness flag shared with the admin endpoint (`/readyz`), flipped
    /// false the moment an accept loop exits outside shutdown — no waiting
    /// for the next reconcile pass to notice. `None` in standalone mode.
    ready_flag: Option<Arc<AtomicBool>>,
    worker_config: worker::WorkerConfig,
    health: Arc<HealthRegistry>,
    udp_session_timeout: Duration,
    shutdown: CancellationToken,
    active_connections: Arc<AtomicUsize>,
    /// Per-port dynamic TLS acceptors for hot-reload support.
    tls_configs: std::collections::HashMap<u16, Arc<DynamicTlsAcceptor>>,
    /// Per-port cert fingerprint to skip no-op TLS reloads.
    tls_cert_hashes: std::collections::HashMap<u16, u64>,
    /// Shared backend selector across all listeners for correct weighted distribution.
    selector: Arc<BackendSelector>,
    /// Process-wide backend pool when `performance.backendPoolScope: process`;
    /// `None` keeps the default per-connection pools.
    shared_pool: Option<Arc<crate::proxy::backend_pool::SharedBackendPool>>,
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
pub(crate) fn merge_cert_refs_by_port(
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
                        bound_port,
                        address: listener_cfg.address.clone(),
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
                        bound_port,
                        address: listener_cfg.address.clone(),
                        socket: tokio_socket,
                    });
                }
            }
        }

        // Readiness tracks the bound socket, so key endpoints on the bound
        // port. Sockets exist from here on; `start()` attaches accept loops.
        let listener_tasks: std::collections::HashMap<(Option<String>, u16), ListenerSlot> =
            listeners
                .iter()
                .map(|l| {
                    (
                        (l.address.clone(), l.target_port.unwrap_or(l.port)),
                        ListenerSlot::Pending,
                    )
                })
                .collect();

        Ok(Self {
            tcp_listeners,
            udp_listeners,
            listener_tasks,
            ready_flag: None,
            worker_config: worker::WorkerConfig {
                max_idle_per_backend: 64,
                connect_timeout: performance_config.backend_timeout,
                client_header_timeout: performance_config.client_header_timeout,
                tcp_keepalive_time: performance_config.tcp_keepalive_time,
            },
            health,
            udp_session_timeout: performance_config.udp_session_timeout,
            shutdown: CancellationToken::new(),
            active_connections: Arc::new(AtomicUsize::new(0)),
            tls_configs: std::collections::HashMap::new(),
            tls_cert_hashes: std::collections::HashMap::new(),
            selector: Arc::new(BackendSelector::new()),
            shared_pool: match performance_config.backend_pool_scope {
                crate::config::PoolScope::Process => Some(Arc::new(
                    crate::proxy::backend_pool::SharedBackendPool::new(
                        64,
                        performance_config.backend_timeout,
                    ),
                )),
                crate::config::PoolScope::Connection => None,
            },
        })
    }

    /// Share the pod readiness flag so a dying accept loop can flip it
    /// immediately (the admin endpoint serves it as `/readyz`). Wired by the
    /// Kubernetes controller at startup; standalone mode has no probe to feed.
    pub fn set_ready_flag(&mut self, flag: Arc<AtomicBool>) {
        self.ready_flag = Some(flag);
    }

    fn endpoint_alive(&self, endpoint: &(Option<String>, u16)) -> bool {
        self.listener_tasks
            .get(endpoint)
            .is_some_and(ListenerSlot::alive)
    }

    /// Spawn an accept loop and record it under its endpoint. The wrapper is
    /// the death-watch: an accept loop returns exactly once, on shutdown, so
    /// any other exit means its socket is gone — flip readiness right away
    /// instead of letting `/readyz` vouch for a port nothing is bound to.
    /// The finished handle stays in the map until the next ensure pass reaps
    /// and rebinds it. (A panic bypasses the wrapper body: under
    /// `panic = "abort"` the whole process dies with it — its own honest
    /// signal — and `JoinHandle::is_finished` covers the unwind case.)
    fn spawn_listener_task(
        &mut self,
        endpoint: (Option<String>, u16),
        task: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        let shutdown = self.shutdown.clone();
        let ready_flag = self.ready_flag.clone();
        let desc = format!("{}:{}", endpoint.0.as_deref().unwrap_or("*"), endpoint.1);
        let handle = tokio::spawn(async move {
            task.await;
            if !shutdown.is_cancelled() {
                warn!("Accept loop for {} exited unexpectedly", desc);
                if let Some(flag) = ready_flag {
                    flag.store(false, Ordering::Release);
                }
            }
        });
        self.listener_tasks
            .insert(endpoint, ListenerSlot::Running(handle));
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

        // Reap finished accept loops before anything else: a finished task
        // means its socket is gone (the listener lives inside the task), so
        // the endpoint must stop counting as bound — the bind loop below
        // then re-opens it this same pass.
        self.listener_tasks.retain(|(addr, bound_port), slot| {
            if slot.alive() {
                true
            } else {
                warn!(
                    "Listener {}:{} accept loop is dead; rebinding this pass",
                    addr.as_deref().unwrap_or("*"),
                    bound_port
                );
                false
            }
        });

        // If addresses are specified, bind each listener to each address.
        // Otherwise, bind to the listener's own address (or 0.0.0.0).
        let bind_addrs: Vec<Option<&str>> = if addresses.is_empty() {
            vec![None]
        } else {
            addresses.iter().map(|a| Some(a.as_str())).collect()
        };

        // Reconcile per-port TLS state FIRST, gated by the cheap fingerprint:
        // build_server_config (full PEM parse + key load for every ref) runs
        // only when the port's merged cert bytes actually changed, and once
        // per port — not once per listener sharing it. The bind loop below
        // always runs, so a newly appeared bind address still gets bound even
        // when the certs are unchanged.
        let merged_cert_refs = merge_cert_refs_by_port(listener_configs);
        for (&port, port_refs) in &merged_cert_refs {
            let fingerprint = compute_cert_fingerprint(port_refs);
            if self.tls_cert_hashes.get(&port) == Some(&fingerprint) {
                continue; // Certs unchanged — the existing acceptor keeps serving
            }
            match tls::build_server_config(port_refs, std::path::Path::new("/unused")) {
                Ok(config) => {
                    if let Some(existing) = self.tls_configs.get(&port) {
                        existing.update(config);
                        info!("Hot-reloaded TLS config for port {}", port);
                    } else {
                        self.tls_configs
                            .insert(port, Arc::new(DynamicTlsAcceptor::new(config)));
                    }
                    self.tls_cert_hashes.insert(port, fingerprint);
                }
                // No fingerprint recorded on failure: the next pass retries.
                Err(e) => errors.push((port, format!("TLS config build failed: {}", e))),
            }
        }

        for listener_cfg in listener_configs {
            // `port` is the published identity (routing/TLS key); `bound_port` is
            // the socket we actually bind — the Service's targetPort when set.
            let port = listener_cfg.port;
            let bound_port = listener_cfg.target_port.unwrap_or(port);

            // TLS-terminate listeners take the per-port acceptor prepared above.
            let (tls_acceptor, tls_passthrough) = match (&listener_cfg.protocol, &listener_cfg.tls)
            {
                (Protocol::HTTPS, Some(tls_cfg)) | (Protocol::TLS, Some(tls_cfg))
                    if tls_cfg.mode == TlsMode::Terminate =>
                {
                    match self.tls_configs.get(&port) {
                        Some(acceptor) => (Some(acceptor.clone()), false),
                        // Config build failed (error already recorded): don't
                        // open a listener that could never handshake.
                        None => continue,
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
                if self.endpoint_alive(&endpoint) {
                    continue; // Endpoint already bound with a live accept loop
                }
                // A wildcard socket we already hold serves every address on
                // the port (routing is port-keyed); an additional
                // address-specific bind adds nothing and, depending on the
                // platform's overlap rules, can fail EADDRINUSE against our
                // own wildcard socket.
                if endpoint.0.is_some() && self.endpoint_alive(&(None, bound_port)) {
                    continue;
                }

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

                                self.spawn_listener_task(endpoint, async move {
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
                                opened += 1;
                            }
                            Err(e) => errors.push((port, format!("UDP from_std: {}", e))),
                        },
                        Err(e) => errors.push((port, format!("UDP bind: {}", e))),
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
                                // The controller's perf config takes precedence
                                // over the startup one for dynamic listeners.
                                let cfg = worker::WorkerConfig {
                                    max_idle_per_backend: self.worker_config.max_idle_per_backend,
                                    connect_timeout: performance_config.backend_timeout,
                                    client_header_timeout: performance_config.client_header_timeout,
                                    tcp_keepalive_time: performance_config.tcp_keepalive_time,
                                };
                                let acceptor = tls_acceptor.clone();
                                let passthrough = tls_passthrough;

                                let selector = self.selector.clone();
                                let active = self.active_connections.clone();
                                let shared_pool = self.shared_pool.clone();
                                self.spawn_listener_task(endpoint, async move {
                                    worker::run_worker(
                                        tokio_listener,
                                        port,
                                        routes,
                                        cfg,
                                        shutdown,
                                        acceptor,
                                        passthrough,
                                        health,
                                        selector,
                                        active,
                                        shared_pool,
                                    )
                                    .await;
                                });
                                opened += 1;
                            }
                            Err(e) => errors.push((port, format!("TCP from_std: {}", e))),
                        },
                        Err(e) => errors.push((port, format!("TCP bind: {}", e))),
                    }
                }
            } // end for bind_addr
        }
        (opened, errors)
    }

    /// Start async listener tasks (one accept loop per TCP listener / UDP socket).
    /// Upgrades each construction-time `Pending` slot to `Running`.
    pub fn start(&mut self, routes: Arc<ArcSwap<RouteTable>>) {
        let tcp_count = self.tcp_listeners.len();
        let udp_count = self.udp_listeners.len();

        let tcp_entries: Vec<_> = self.tcp_listeners.drain(..).collect();
        for entry in tcp_entries {
            let routes = routes.clone();
            let health = self.health.clone();
            let shutdown = self.shutdown.clone();
            let cfg = self.worker_config;
            let selector = self.selector.clone();
            let active = self.active_connections.clone();
            let shared_pool = self.shared_pool.clone();

            let TcpListenerEntry {
                port,
                bound_port,
                address,
                listener,
                tls_acceptor,
                tls_passthrough,
            } = entry;
            self.spawn_listener_task((address, bound_port), async move {
                worker::run_worker(
                    listener,
                    port,
                    routes,
                    cfg,
                    shutdown,
                    tls_acceptor,
                    tls_passthrough,
                    health,
                    selector,
                    active,
                    shared_pool,
                )
                .await;
            });
        }

        let udp_entries: Vec<_> = self.udp_listeners.drain(..).collect();
        for entry in udp_entries {
            let socket = Arc::new(entry.socket);
            let routes = routes.clone();
            let health = self.health.clone();
            let shutdown = self.shutdown.clone();
            let session_timeout = self.udp_session_timeout;
            let port = entry.port;

            self.spawn_listener_task((entry.address, entry.bound_port), async move {
                udp_worker::run_udp_worker(socket, port, routes, session_timeout, shutdown, health)
                    .await;
            });
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

        for (_endpoint, slot) in self.listener_tasks {
            if let ListenerSlot::Running(handle) = slot {
                let _ = handle.await;
            }
        }

        info!("Data plane shutdown complete");
    }

    /// Check if all the specified (address, port) endpoints are bound with a
    /// LIVE accept loop (or a construction-time socket awaiting `start()`).
    /// A dead accept loop does NOT count: readiness and Programmed must go
    /// false the moment a listener can no longer accept, so the reconcile
    /// safety net rebinds instead of trusting stale bookkeeping.
    pub fn is_ready_for_endpoints(&self, required: &[(Option<String>, u16)]) -> bool {
        required.iter().all(|ep| {
            self.endpoint_alive(ep)
                // A wildcard socket on the port already accepts traffic for
                // any specific address; routing is keyed on the port. The
                // reverse does NOT hold: a required wildcard bind is not
                // satisfied by an address-specific one.
                || (ep.0.is_some() && self.endpoint_alive(&(None, ep.1)))
        })
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
        let perf = test_perf();
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

    fn test_perf() -> crate::config::PerformanceConfig {
        crate::config::PerformanceConfig {
            backend_timeout: std::time::Duration::from_secs(1),
            udp_session_timeout: std::time::Duration::from_secs(1),
            dns_refresh_interval: std::time::Duration::from_secs(1),
            client_header_timeout: std::time::Duration::from_secs(1),
            ..Default::default()
        }
    }

    /// Regression: with unchanged certs, a reconcile that brings a NEW bind
    /// address must still bind it. The old flow `continue`d out of the whole
    /// listener iteration on a fingerprint match, silently skipping the
    /// bind loop for addresses that appeared after the first pass.
    #[tokio::test]
    async fn new_bind_address_with_unchanged_certs_still_binds() {
        let Some((cert, key)) = crate::proxy::tls::test_util::generate_test_cert("a.example.org")
        else {
            return; // openssl unavailable
        };
        let mut listener = https_listener("https", 23443, Some("a.example.org"), "cert");
        listener.tls.as_mut().unwrap().certificate_refs[0].cert_pem = Some(cert);
        listener.tls.as_mut().unwrap().certificate_refs[0].key_pem = Some(key);

        let perf = test_perf();
        let mut dp = DataPlane::new(&[], &perf, std::path::Path::new("/unused")).unwrap();
        let routes = Arc::new(ArcSwap::from_pointee(RouteTable::new()));

        let (opened, errors) = dp.add_tcp_listeners(
            std::slice::from_ref(&listener),
            &["127.0.0.1".to_string()],
            routes.clone(),
            &perf,
        );
        assert_eq!((opened, errors.len()), (1, 0));
        let hash_after_first = dp.tls_cert_hashes.get(&23443).copied();
        assert!(
            hash_after_first.is_some(),
            "TLS state prepared for the port"
        );

        // Same certs, one more address: the new endpoint must be bound and
        // the TLS state must not churn.
        let (opened, errors) = dp.add_tcp_listeners(
            std::slice::from_ref(&listener),
            &["127.0.0.1".to_string(), "127.0.0.2".to_string()],
            routes,
            &perf,
        );
        assert_eq!(errors, vec![]);
        assert_eq!(opened, 1, "the address that appeared later must be bound");
        assert_eq!(dp.tls_cert_hashes.get(&23443).copied(), hash_after_first);
    }

    /// Unparseable certs on a port shared by several Terminate listeners:
    /// one build attempt, one error, no listener bound on that port.
    #[tokio::test]
    async fn invalid_certs_error_once_per_port_and_skip_binding() {
        let listeners = vec![
            https_listener("a", 23444, None, "bad"),
            https_listener("b", 23444, Some("b.example.org"), "bad"),
        ];
        let perf = test_perf();
        let mut dp = DataPlane::new(&[], &perf, std::path::Path::new("/unused")).unwrap();
        let routes = Arc::new(ArcSwap::from_pointee(RouteTable::new()));

        let (opened, errors) = dp.add_tcp_listeners(&listeners, &[], routes, &perf);
        assert_eq!(opened, 0, "a port that cannot handshake must not bind");
        assert_eq!(errors.len(), 1, "one error per port, not per listener");
        assert!(
            !dp.tls_cert_hashes.contains_key(&23444),
            "failed build must retry next pass"
        );
    }

    /// A wildcard bind on a port covers every specific address: readiness
    /// treats (Some(addr), port) as served by (None, port), and the bind loop
    /// must not attempt an address-specific re-bind (which can EADDRINUSE
    /// against our own wildcard socket, keeping Programmed=False forever).
    #[tokio::test]
    async fn wildcard_bind_covers_specific_addresses() {
        let listeners = vec![ListenerConfig {
            name: "http".into(),
            protocol: P::HTTP,
            port: 23445,
            target_port: None,
            hostname: None,
            address: None, // wildcard
            interface: None,
            tls: None,
        }];
        let perf = test_perf();
        let mut dp = DataPlane::new(&listeners, &perf, std::path::Path::new("/unused")).unwrap();
        let routes = Arc::new(ArcSwap::from_pointee(RouteTable::new()));

        // Same port, now with a concrete bind address (e.g. a Gateway
        // spec.addresses entry): the wildcard already serves it.
        let (opened, errors) =
            dp.add_tcp_listeners(&listeners, &["127.0.0.1".to_string()], routes, &perf);
        assert_eq!((opened, errors.len()), (0, 0), "no re-bind, no error");

        assert!(dp.is_ready_for_endpoints(&[(Some("127.0.0.1".to_string()), 23445)]));
        assert!(dp.is_ready_for_endpoints(&[(None, 23445)]));
        // The reverse is not satisfied: specific never covers wildcard.
        assert!(!dp.is_ready_for_endpoints(&[(None, 23446)]));
    }

    /// A dead accept loop must drop out of readiness and be rebound by the
    /// next ensure pass. This is the ".21 outage" shape: the old add-only
    /// bookkeeping kept reporting bound (readiness green, Programmed=True)
    /// while the kernel refused every connection, until pod restart.
    #[tokio::test]
    async fn dead_accept_loop_unreadies_then_rebinds() {
        let listeners = vec![ListenerConfig {
            name: "http".into(),
            protocol: P::HTTP,
            port: 23447,
            target_port: None,
            hostname: None,
            address: None,
            interface: None,
            tls: None,
        }];
        let perf = test_perf();
        let mut dp = DataPlane::new(&[], &perf, std::path::Path::new("/unused")).unwrap();
        let routes = Arc::new(ArcSwap::from_pointee(RouteTable::new()));

        let (opened, errors) = dp.add_tcp_listeners(&listeners, &[], routes.clone(), &perf);
        assert_eq!((opened, errors.len()), (1, 0));
        assert!(dp.is_ready_for_endpoints(&[(None, 23447)]));

        // Kill the accept loop out-of-band — stands in for any task death.
        if let Some(ListenerSlot::Running(handle)) = dp.listener_tasks.get(&(None, 23447)) {
            handle.abort();
        } else {
            panic!("expected a running listener slot");
        }
        // Bounded wait for the abort to land (is_finished flips async).
        for _ in 0..200 {
            if !dp.endpoint_alive(&(None, 23447)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            !dp.is_ready_for_endpoints(&[(None, 23447)]),
            "a dead accept loop must not count as bound"
        );

        // The next ensure pass reaps the corpse and rebinds the endpoint.
        let (opened, errors) = dp.add_tcp_listeners(&listeners, &[], routes, &perf);
        assert_eq!((opened, errors.len()), (1, 0), "dead endpoint must rebind");
        assert!(dp.is_ready_for_endpoints(&[(None, 23447)]));
    }
}
