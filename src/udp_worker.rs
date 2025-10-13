//! UDP worker — per-listener recv_from loop with per-session connected backend sockets.
//! Sessions are identified by source address and expire after a configurable timeout.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use arc_swap::ArcSwap;
use dashmap::DashMap;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::logging::{warn, info, debug};
use crate::request_processor::{self, ProcessingDecision};
use crate::routing::{BackendSelector, RouteTable};

struct UdpSession {
    backend_socket: Arc<UdpSocket>,
    _backend_task: JoinHandle<()>,
    last_active: Arc<std::sync::atomic::AtomicU64>,
}

/// Run a UDP recv_from loop on a single socket, dispatching datagrams to per-session backends.
pub async fn run_udp_worker(
    worker_id: usize,
    socket: Arc<UdpSocket>,
    server_port: u16,
    routes: Arc<ArcSwap<RouteTable>>,
    session_timeout: Duration,
    shutdown: CancellationToken,
) {
    info!("UDP worker {} listening on port {}", worker_id, server_port);

    let sessions: Arc<DashMap<SocketAddr, UdpSession>> = Arc::new(DashMap::new());
    let mut selector = BackendSelector::new();
    let mut buf = vec![0u8; 65536];

    // Reaper task: periodically remove expired sessions
    let reaper_sessions = sessions.clone();
    let reaper_shutdown = shutdown.clone();
    let _reaper = tokio::spawn(async move {
        let mut interval = tokio::time::interval(session_timeout / 2);
        loop {
            tokio::select! {
                biased;
                _ = reaper_shutdown.cancelled() => break,
                _ = interval.tick() => {
                    let now = Instant::now().elapsed().as_secs();
                    let timeout_secs = session_timeout.as_secs();
                    reaper_sessions.retain(|_addr, session: &mut UdpSession| {
                        let last = session.last_active.load(std::sync::atomic::Ordering::Relaxed);
                        if now.saturating_sub(last) > timeout_secs {
                            session._backend_task.abort();
                            false
                        } else {
                            true
                        }
                    });
                }
            }
        }
    });

    // Use a monotonic epoch for last_active tracking
    let epoch = Instant::now();

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!("UDP worker {} shutting down", worker_id);
                break;
            }
            result = socket.recv_from(&mut buf) => {
                match result {
                    Ok((n, client_addr)) => {
                        let now_secs = epoch.elapsed().as_secs();

                        // Update last_active or create new session
                        if let Some(session) = sessions.get(&client_addr) {
                            session.last_active.store(now_secs, std::sync::atomic::Ordering::Relaxed);
                            if let Err(e) = session.backend_socket.send(&buf[..n]).await {
                                debug!("UDP send to backend failed for {}: {}", client_addr, e);
                                drop(session);
                                sessions.remove(&client_addr);
                            }
                            continue;
                        }

                        // New session — route lookup
                        let decision = {
                            let route_table = routes.load();
                            match request_processor::analyze_udp_request(&route_table, &mut selector, server_port) {
                                Ok(d) => d,
                                Err(e) => {
                                    warn!("UDP routing error for port {}: {}", server_port, e);
                                    continue;
                                }
                            }
                        };

                        let backend_addr = match decision {
                            ProcessingDecision::UdpForward { backend_addr } => backend_addr,
                            _ => continue,
                        };

                        // Create connected backend socket
                        let backend_socket = match UdpSocket::bind("0.0.0.0:0").await {
                            Ok(s) => s,
                            Err(e) => {
                                warn!("UDP backend socket bind failed: {}", e);
                                continue;
                            }
                        };
                        if let Err(e) = backend_socket.connect(backend_addr).await {
                            warn!("UDP backend connect to {} failed: {}", backend_addr, e);
                            continue;
                        }

                        let backend_socket = Arc::new(backend_socket);

                        // Forward the first datagram
                        if let Err(e) = backend_socket.send(&buf[..n]).await {
                            warn!("UDP initial send to {} failed: {}", backend_addr, e);
                            continue;
                        }

                        let last_active = Arc::new(std::sync::atomic::AtomicU64::new(now_secs));

                        // Spawn backend→client forwarding task
                        let listener_socket = socket.clone();
                        let backend_rx = backend_socket.clone();
                        let task_last_active = last_active.clone();
                        let task_timeout = session_timeout;
                        let backend_task = tokio::spawn(async move {
                            let mut reply_buf = vec![0u8; 65536];
                            while let Ok(Ok(n)) = tokio::time::timeout(task_timeout, backend_rx.recv(&mut reply_buf)).await {
                                task_last_active.store(
                                    epoch.elapsed().as_secs(),
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                                if listener_socket.send_to(&reply_buf[..n], client_addr).await.is_err() {
                                    break;
                                }
                            }
                        });

                        sessions.insert(client_addr, UdpSession {
                            backend_socket,
                            _backend_task: backend_task,
                            last_active,
                        });

                        debug!("UDP session created: {} -> {} (port {})", client_addr, backend_addr, server_port);
                    }
                    Err(e) => {
                        warn!("UDP worker {} recv_from error: {}", worker_id, e);
                    }
                }
            }
        }
    }

    // Cleanup: abort all session tasks
    for entry in sessions.iter() {
        entry.value()._backend_task.abort();
    }
}
