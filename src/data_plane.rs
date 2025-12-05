//! Tokio-based data plane — creates SO_REUSEPORT listeners and spawns async workers.
//!
//! Each worker is a Tokio task sharing the runtime's thread pool. eBPF programs
//! attach to listener sockets the same way for both TCP and UDP (SO_REUSEPORT
//! uses the same dest-port offset in the transport header).
//!
//! On shutdown, the data plane stops accepting new connections and waits up to
//! `DRAIN_TIMEOUT` for in-flight connections to finish.


use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use arc_swap::ArcSwap;
use tokio::net::{TcpListener, UdpSocket};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use anyhow::Result;

use crate::config::{ListenerConfig, PerformanceConfig, Protocol, TlsMode};
use crate::health::HealthRegistry;
use crate::logging::{info, warn};
use crate::routing::RouteTable;
use crate::tls;
use crate::worker;
use crate::udp_worker;

struct TcpListenerEntry {
    worker_id: usize,
    port: u16,
    listener: TcpListener,
    tls_acceptor: Option<Arc<TlsAcceptor>>,
    tls_passthrough: bool,
}

struct UdpListenerEntry {
    worker_id: usize,
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
    bound_ports: std::collections::HashSet<u16>,
}

/// Maximum time to wait for in-flight connections on shutdown.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

impl DataPlane {
    /// Create listeners for all (worker, listener) combinations.
    /// Sockets are created with SO_REUSEPORT so multiple workers can share the same port.
    pub fn new(
        worker_count: usize,
        listeners: &[ListenerConfig],
        performance_config: &PerformanceConfig,
        cert_dir: &PathBuf,
    ) -> Result<Self> {
        let health = Arc::new(HealthRegistry::new());

        let mut tcp_listeners = Vec::new();
        let mut udp_listeners = Vec::new();

        // Pre-build TLS acceptors (shared across workers for the same listener)
        let mut tls_acceptors: Vec<Option<Arc<TlsAcceptor>>> = Vec::with_capacity(listeners.len());
        let mut tls_passthrough_flags: Vec<bool> = Vec::with_capacity(listeners.len());

        for listener_cfg in listeners {
            match (&listener_cfg.protocol, &listener_cfg.tls) {
                (Protocol::HTTPS, Some(tls_cfg)) if tls_cfg.mode == TlsMode::Terminate => {
                    let acceptor = tls::build_tls_acceptor(&tls_cfg.certificate_refs, cert_dir)?;
                    tls_acceptors.push(Some(Arc::new(acceptor)));
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

        for worker_id in 0..worker_count {
            for (i, listener_cfg) in listeners.iter().enumerate() {
                let port = listener_cfg.port;
                match listener_cfg.protocol {
                    Protocol::HTTP | Protocol::HTTPS | Protocol::TCP | Protocol::TLS => {
                        let std_listener = create_reuseport_tcp_listener(
                            port,
                            listener_cfg.address.as_deref(),
                            listener_cfg.interface.as_deref(),
                        )?;
                        let tokio_listener = TcpListener::from_std(std_listener)?;

                        tcp_listeners.push(TcpListenerEntry {
                            worker_id,
                            port,
                            listener: tokio_listener,
                            tls_acceptor: tls_acceptors[i].clone(),
                            tls_passthrough: tls_passthrough_flags[i],
                        });

                        info!("Worker {} TCP bound to port {} with SO_REUSEPORT", worker_id, port);
                    }
                    Protocol::UDP => {
                        let std_socket = create_reuseport_udp_socket(
                            port,
                            listener_cfg.address.as_deref(),
                            listener_cfg.interface.as_deref(),
                        )?;
                        let tokio_socket = UdpSocket::from_std(std_socket)?;

                        udp_listeners.push(UdpListenerEntry {
                            worker_id,
                            port,
                            socket: tokio_socket,
                        });

                        info!("Worker {} UDP bound to port {} with SO_REUSEPORT", worker_id, port);
                    }
                }
            }
        }

        let bound_ports: std::collections::HashSet<u16> = tcp_listeners.iter().map(|e| e.port)
            .chain(udp_listeners.iter().map(|e| e.port))
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
            bound_ports,
        })
    }

    /// Returns the set of TCP/UDP ports currently being listened on.
    pub fn active_ports(&self) -> &std::collections::HashSet<u16> {
        &self.bound_ports
    }

    /// Dynamically add TCP listeners for new ports. Creates SO_REUSEPORT sockets
    /// and spawns worker tasks. Called by K8s controller when new Gateway ports are discovered.
    /// Returns (ports_opened, errors) for caller-side logging.
    pub fn add_tcp_listeners(
        &mut self,
        ports: &[u16],
        worker_count: usize,
        routes: Arc<ArcSwap<RouteTable>>,
        performance_config: &crate::config::PerformanceConfig,
    ) -> (usize, Vec<(u16, String)>) {
        let mut opened = 0usize;
        let mut errors = Vec::new();
        for &port in ports {
            if self.bound_ports.contains(&port) {
                continue;
            }
            let mut port_ok = true;
            for worker_id in 0..worker_count {
                match create_reuseport_tcp_listener(port, None, None) {
                    Ok(std_listener) => {
                        match TcpListener::from_std(std_listener) {
                            Ok(tokio_listener) => {
                                let routes = routes.clone();
                                let health = self.health.clone();
                                let shutdown = self.shutdown.clone();
                                let max_idle = self.max_idle_per_backend;
                                let connect_timeout = performance_config.backend_timeout;

                                let handle = tokio::spawn(async move {
                                    worker::run_worker(
                                        worker_id,
                                        tokio_listener,
                                        port,
                                        routes,
                                        max_idle,
                                        connect_timeout,
                                        shutdown,
                                        None,
                                        false,
                                        health,
                                    ).await;
                                });
                                self.task_handles.push(handle);
                            }
                            Err(e) => {
                                errors.push((port, format!("from_std: {}", e)));
                                port_ok = false;
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        errors.push((port, format!("bind worker {}: {}", worker_id, e)));
                        port_ok = false;
                        break;
                    }
                }
            }
            if port_ok {
                self.bound_ports.insert(port);
                opened += 1;
            }
        }
        (opened, errors)
    }

    /// Start async worker tasks — call after eBPF programs are attached.
    pub fn start(&mut self, routes: Arc<ArcSwap<RouteTable>>) {
        let tcp_count = self.tcp_listeners.len();
        let udp_count = self.udp_listeners.len();

        for entry in self.tcp_listeners.drain(..) {
            let routes = routes.clone();
            let health = self.health.clone();
            let shutdown = self.shutdown.clone();
            let max_idle = self.max_idle_per_backend;
            let connect_timeout = self.connect_timeout;

            let handle = tokio::spawn(async move {
                worker::run_worker(
                    entry.worker_id,
                    entry.listener,
                    entry.port,
                    routes,
                    max_idle,
                    connect_timeout,
                    shutdown,
                    entry.tls_acceptor,
                    entry.tls_passthrough,
                    health,
                ).await;
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
                udp_worker::run_udp_worker(entry.worker_id, socket, entry.port, routes, session_timeout, shutdown, health).await;
            });

            self.task_handles.push(handle);
        }

        info!("Data plane started: {} TCP + {} UDP worker tasks", tcp_count, udp_count);
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
        }).await;

        match drain_result {
            Ok(()) => info!("All connections drained"),
            Err(_) => {
                let remaining = active.load(Ordering::Acquire);
                warn!("Drain timeout after {:?}: {} connections still active", DRAIN_TIMEOUT, remaining);
            }
        }

        for handle in self.task_handles {
            let _ = handle.await;
        }

        info!("Data plane shutdown complete");
    }

    /// Get a reference to the shared health registry.
    pub fn health(&self) -> &Arc<HealthRegistry> {
        &self.health
    }
}

fn create_reuseport_tcp_listener(
    port: u16,
    bind_addr: Option<&str>,
    interface: Option<&str>,
) -> Result<std::net::TcpListener> {
    use socket2::{Socket, Domain, Type, Protocol};

    let ip: std::net::IpAddr = bind_addr.unwrap_or("0.0.0.0").parse()
        .map_err(|e| anyhow::anyhow!("Invalid bind address '{}': {}", bind_addr.unwrap_or(""), e))?;
    let domain = if ip.is_ipv6() { Domain::IPV6 } else { Domain::IPV4 };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;

    if let Some(iface) = interface {
        socket.bind_device(Some(iface.as_bytes()))?;
    }

    let addr = std::net::SocketAddr::new(ip, port);
    socket.bind(&addr.into())?;
    socket.listen(1024)?;

    Ok(socket.into())
}

fn create_reuseport_udp_socket(
    port: u16,
    bind_addr: Option<&str>,
    interface: Option<&str>,
) -> Result<std::net::UdpSocket> {
    use socket2::{Socket, Domain, Type, Protocol};

    let ip: std::net::IpAddr = bind_addr.unwrap_or("0.0.0.0").parse()
        .map_err(|e| anyhow::anyhow!("Invalid bind address '{}': {}", bind_addr.unwrap_or(""), e))?;
    let domain = if ip.is_ipv6() { Domain::IPV6 } else { Domain::IPV4 };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;

    if let Some(iface) = interface {
        socket.bind_device(Some(iface.as_bytes()))?;
    }

    let addr = std::net::SocketAddr::new(ip, port);
    socket.bind(&addr.into())?;

    Ok(socket.into())
}
