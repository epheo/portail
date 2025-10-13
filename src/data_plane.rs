//! Tokio-based data plane — creates SO_REUSEPORT listeners and spawns async workers.
//!
//! Each worker is a Tokio task sharing the runtime's thread pool. eBPF programs
//! attach to listener sockets the same way for both TCP and UDP (SO_REUSEPORT
//! uses the same dest-port offset in the transport header).

use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;
use std::time::Duration;
use arc_swap::ArcSwap;
use tokio::net::{TcpListener, UdpSocket};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use anyhow::Result;

use crate::backend_pool::BackendPool;
use crate::config::{PerformanceConfig, Protocol};
use crate::logging::info;
use crate::routing::RouteTable;
use crate::worker;
use crate::udp_worker;

struct TcpListenerEntry {
    worker_id: usize,
    port: u16,
    raw_fd: RawFd,
    listener: Option<TcpListener>,
}

struct UdpListenerEntry {
    worker_id: usize,
    port: u16,
    raw_fd: RawFd,
    socket: Option<UdpSocket>,
}

pub struct DataPlane {
    tcp_listeners: Vec<TcpListenerEntry>,
    udp_listeners: Vec<UdpListenerEntry>,
    task_handles: Vec<JoinHandle<()>>,
    pool: Arc<BackendPool>,
    udp_session_timeout: Duration,
    shutdown: CancellationToken,
}

impl DataPlane {
    /// Create listeners for all (worker, port, protocol) combinations.
    /// Sockets are created with SO_REUSEPORT so multiple workers can share the same port.
    pub fn new(
        worker_count: usize,
        listener_protocols: &[(u16, Protocol)],
        performance_config: &PerformanceConfig,
    ) -> Result<Self> {
        let pool = Arc::new(BackendPool::new(
            64,
            performance_config.backend_timeout,
        ));

        let mut tcp_listeners = Vec::new();
        let mut udp_listeners = Vec::new();

        for worker_id in 0..worker_count {
            for (port, protocol) in listener_protocols {
                match protocol {
                    Protocol::HTTP | Protocol::HTTPS | Protocol::TCP => {
                        let std_listener = create_reuseport_tcp_listener(*port)?;
                        let raw_fd = std_listener.as_raw_fd();
                        let tokio_listener = TcpListener::from_std(std_listener)?;

                        tcp_listeners.push(TcpListenerEntry {
                            worker_id,
                            port: *port,
                            raw_fd,
                            listener: Some(tokio_listener),
                        });

                        info!("Worker {} TCP bound to port {} (fd={}) with SO_REUSEPORT", worker_id, port, raw_fd);
                    }
                    Protocol::UDP => {
                        let std_socket = create_reuseport_udp_socket(*port)?;
                        let raw_fd = std_socket.as_raw_fd();
                        let tokio_socket = UdpSocket::from_std(std_socket)?;

                        udp_listeners.push(UdpListenerEntry {
                            worker_id,
                            port: *port,
                            raw_fd,
                            socket: Some(tokio_socket),
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
        for entry in &mut self.tcp_listeners {
            let listener = entry.listener.take().expect("listener already started");
            let routes = routes.clone();
            let pool = self.pool.clone();
            let shutdown = self.shutdown.clone();
            let worker_id = entry.worker_id;
            let port = entry.port;

            let handle = tokio::spawn(async move {
                worker::run_worker(worker_id, listener, port, routes, pool, shutdown).await;
            });

            self.task_handles.push(handle);
        }

        for entry in &mut self.udp_listeners {
            let socket = Arc::new(entry.socket.take().expect("socket already started"));
            let routes = routes.clone();
            let shutdown = self.shutdown.clone();
            let worker_id = entry.worker_id;
            let port = entry.port;
            let session_timeout = self.udp_session_timeout;

            let handle = tokio::spawn(async move {
                udp_worker::run_udp_worker(worker_id, socket, port, routes, session_timeout, shutdown).await;
            });

            self.task_handles.push(handle);
        }

        info!("Data plane started: {} TCP + {} UDP worker tasks",
            self.tcp_listeners.len(), self.udp_listeners.len());
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
