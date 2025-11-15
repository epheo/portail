//! Tokio-based data plane — creates SO_REUSEPORT listeners and spawns async workers.
//!
//! Each worker is a Tokio task sharing the runtime's thread pool. eBPF programs
//! attach to listener sockets the same way for both TCP and UDP (SO_REUSEPORT
//! uses the same dest-port offset in the transport header).

use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use arc_swap::ArcSwap;
use tokio::net::{TcpListener, UdpSocket};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use anyhow::Result;

use crate::backend_pool::BackendPool;
use crate::config::{ListenerConfig, PerformanceConfig, Protocol, TlsMode};
use crate::health::HealthRegistry;
use crate::logging::info;
use crate::routing::RouteTable;
use crate::tls;
use crate::worker;
use crate::udp_worker;

struct TcpListenerEntry {
    worker_id: usize,
    port: u16,
    raw_fd: RawFd,
    listener: TcpListener,
    tls_acceptor: Option<Arc<TlsAcceptor>>,
    tls_passthrough: bool,
}

struct UdpListenerEntry {
    worker_id: usize,
    port: u16,
    raw_fd: RawFd,
    socket: UdpSocket,
}

pub struct DataPlane {
    tcp_listeners: Vec<TcpListenerEntry>,
    udp_listeners: Vec<UdpListenerEntry>,
    task_handles: Vec<JoinHandle<()>>,
    pool: Arc<BackendPool>,
    health: Arc<HealthRegistry>,
    udp_session_timeout: Duration,
    shutdown: CancellationToken,
}

impl DataPlane {
    /// Create listeners for all (worker, listener) combinations.
    /// Sockets are created with SO_REUSEPORT so multiple workers can share the same port.
    pub fn new(
        worker_count: usize,
        listeners: &[ListenerConfig],
        performance_config: &PerformanceConfig,
        cert_dir: &PathBuf,
    ) -> Result<Self> {
        let pool = Arc::new(BackendPool::new(
            64,
            performance_config.backend_timeout,
        ));
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
                        let std_listener = create_reuseport_tcp_listener(port)?;
                        let raw_fd = std_listener.as_raw_fd();
                        let tokio_listener = TcpListener::from_std(std_listener)?;

                        tcp_listeners.push(TcpListenerEntry {
                            worker_id,
                            port,
                            raw_fd,
                            listener: tokio_listener,
                            tls_acceptor: tls_acceptors[i].clone(),
                            tls_passthrough: tls_passthrough_flags[i],
                        });

                        info!("Worker {} TCP bound to port {} (fd={}) with SO_REUSEPORT", worker_id, port, raw_fd);
                    }
                    Protocol::UDP => {
                        let std_socket = create_reuseport_udp_socket(port)?;
                        let raw_fd = std_socket.as_raw_fd();
                        let tokio_socket = UdpSocket::from_std(std_socket)?;

                        udp_listeners.push(UdpListenerEntry {
                            worker_id,
                            port,
                            raw_fd,
                            socket: tokio_socket,
                        });

                        info!("Worker {} UDP bound to port {} (fd={}) with SO_REUSEPORT", worker_id, port, raw_fd);
                    }
                }
            }
        }

        Ok(Self {
            tcp_listeners,
            udp_listeners,
            task_handles: Vec::new(),
            pool,
            health,
            udp_session_timeout: performance_config.udp_session_timeout,
            shutdown: CancellationToken::new(),
        })
    }

    /// Returns (worker_id, port, raw_fd) tuples for eBPF attachment.
    pub fn get_listener_fds(&self) -> Vec<(usize, u16, RawFd)> {
        let tcp_fds = self.tcp_listeners.iter()
            .map(|e| (e.worker_id, e.port, e.raw_fd));
        let udp_fds = self.udp_listeners.iter()
            .map(|e| (e.worker_id, e.port, e.raw_fd));
        tcp_fds.chain(udp_fds).collect()
    }

    /// Start async worker tasks — call after eBPF programs are attached.
    pub fn start(&mut self, routes: Arc<ArcSwap<RouteTable>>) {
        let tcp_count = self.tcp_listeners.len();
        let udp_count = self.udp_listeners.len();

        for entry in self.tcp_listeners.drain(..) {
            let routes = routes.clone();
            let pool = self.pool.clone();
            let health = self.health.clone();
            let shutdown = self.shutdown.clone();

            let handle = tokio::spawn(async move {
                worker::run_worker(
                    entry.worker_id,
                    entry.listener,
                    entry.port,
                    routes,
                    pool,
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

    /// Signal shutdown and wait for all workers to finish.
    pub async fn shutdown(self) {
        info!("Data plane shutting down");
        self.shutdown.cancel();

        for handle in self.task_handles {
            let _ = handle.await;
        }

        info!("Data plane shutdown complete");
    }
}

fn create_reuseport_tcp_listener(port: u16) -> Result<std::net::TcpListener> {
    use socket2::{Socket, Domain, Type, Protocol};

    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    socket.bind(&addr.into())?;
    socket.listen(1024)?;

    Ok(socket.into())
}

fn create_reuseport_udp_socket(port: u16) -> Result<std::net::UdpSocket> {
    use socket2::{Socket, Domain, Type, Protocol};

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    socket.bind(&addr.into())?;

    Ok(socket.into())
}
