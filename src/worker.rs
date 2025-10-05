//! Tokio async worker — one task per accepted connection (TCP) or per listener (UDP).
//!
//! TCP: HTTP request/response proxying with keepalive and backend connection pooling,
//! and raw TCP bidirectional forwarding via copy_bidirectional.
//!
//! UDP: Per-listener recv_from loop with per-session connected backend sockets.
//! Sessions are identified by source address and expire after a configurable timeout.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use arc_swap::ArcSwap;
use dashmap::DashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use anyhow::Result;

use crate::backend_pool::BackendPool;
use crate::logging::{warn, info, debug};
use crate::request_processor::{self, BackendSelector, ProcessingDecision};
use crate::routing::RouteTable;

/// Run an accept loop on a single listener, spawning a task per connection.
pub async fn run_worker(
    worker_id: usize,
    listener: TcpListener,
    server_port: u16,
    routes: Arc<ArcSwap<RouteTable>>,
    pool: Arc<BackendPool>,
    shutdown: CancellationToken,
) {
    info!("Worker {} accepting on port {}", worker_id, server_port);

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!("Worker {} shutting down", worker_id);
                break;
            }
            result = listener.accept() => {
                match result {
                    Ok((client, peer)) => {
                        let routes = routes.clone();
                        let pool = pool.clone();
                        tokio::spawn(async move {
                            if let Err(_e) = handle_connection(client, peer, server_port, routes, pool).await {
                                debug!("Connection from {} closed: {}", peer, _e);
                            }
                        });
                    }
                    Err(e) => {
                        warn!("Worker {} accept error: {}", worker_id, e);
                    }
                }
            }
        }
    }
}

async fn handle_connection(
    mut client: TcpStream,
    _peer: SocketAddr,
    server_port: u16,
    routes: Arc<ArcSwap<RouteTable>>,
    pool: Arc<BackendPool>,
) -> Result<()> {
    client.set_nodelay(true)?;

    let mut buf = vec![0u8; 65536];
    let mut selector = BackendSelector::new();

    // First read to determine protocol
    let n = client.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }

    let decision = {
        let route_table = routes.load();
        request_processor::analyze_request(&route_table, &mut selector, &buf[..n], server_port)?
    };

    match decision {
        ProcessingDecision::TcpForward { backend_addr } => {
            handle_tcp_connection(client, backend_addr, &buf[..n]).await
        }
        ProcessingDecision::HttpForward { backend_addr, keepalive } => {
            handle_http_connection(client, &mut buf, n, backend_addr, keepalive, server_port, routes, pool, &mut selector).await
        }
        ProcessingDecision::SendHttpError { error_code, .. } => {
            send_error_response(&mut client, error_code).await
        }
        ProcessingDecision::UdpForward { .. } | ProcessingDecision::CloseConnection => Ok(()),
    }
}

async fn handle_http_connection(
    mut client: TcpStream,
    buf: &mut Vec<u8>,
    initial_bytes: usize,
    initial_backend_addr: SocketAddr,
    initial_keepalive: bool,
    server_port: u16,
    routes: Arc<ArcSwap<RouteTable>>,
    pool: Arc<BackendPool>,
    selector: &mut BackendSelector,
) -> Result<()> {
    let mut backend_addr = initial_backend_addr;
    let mut keepalive = initial_keepalive;
    let mut request_bytes = initial_bytes;

    loop {
        let mut backend = pool.acquire(backend_addr).await?;

        backend.write_all(&buf[..request_bytes]).await?;

        let response_complete = forward_http_response(&mut backend, &mut client, buf).await?;

        if response_complete && keepalive {
            pool.release(backend_addr, backend);
        }

        if !keepalive {
            return Ok(());
        }

        // Wait for next request on this client connection (keepalive)
        request_bytes = client.read(buf).await?;
        if request_bytes == 0 {
            return Ok(());
        }

        // Re-analyze for next request (host/path may differ)
        let decision = {
            let route_table = routes.load();
            request_processor::analyze_request(&route_table, selector, &buf[..request_bytes], server_port)?
        };

        match decision {
            ProcessingDecision::HttpForward { backend_addr: addr, keepalive: ka } => {
                backend_addr = addr;
                keepalive = ka;
            }
            ProcessingDecision::SendHttpError { error_code, close_connection } => {
                send_error_response(&mut client, error_code).await?;
                if close_connection {
                    return Ok(());
                }
                continue;
            }
            ProcessingDecision::TcpForward { .. } | ProcessingDecision::UdpForward { .. } | ProcessingDecision::CloseConnection => {
                return Ok(());
            }
        }
    }
}

/// Forward the backend's HTTP response to the client.
/// Returns true if the response was fully received (backend connection reusable).
async fn forward_http_response(
    backend: &mut TcpStream,
    client: &mut TcpStream,
    buf: &mut [u8],
) -> Result<bool> {
    let mut headers_parsed = false;
    let mut content_length: Option<usize> = None;
    let mut body_bytes_forwarded: usize = 0;
    let mut chunked = false;
    let mut header_buf = Vec::new();

    loop {
        let n = backend.read(buf).await?;
        if n == 0 {
            return Ok(false);
        }

        client.write_all(&buf[..n]).await?;

        if !headers_parsed {
            header_buf.extend_from_slice(&buf[..n]);

            if let Some(header_end) = find_header_end(&header_buf) {
                headers_parsed = true;
                let headers = &header_buf[..header_end];

                content_length = parse_content_length(headers);
                chunked = is_chunked_transfer(headers);

                let body_in_this_chunk = header_buf.len() - header_end;
                body_bytes_forwarded = body_in_this_chunk;

                if let Some(cl) = content_length {
                    if body_bytes_forwarded >= cl {
                        return Ok(true);
                    }
                }
                if chunked && ends_with_chunked_terminator(&header_buf[header_end..]) {
                    return Ok(true);
                }
                if is_no_body_status(headers) {
                    return Ok(true);
                }
            }
        } else {
            body_bytes_forwarded += n;

            if let Some(cl) = content_length {
                if body_bytes_forwarded >= cl {
                    return Ok(true);
                }
            }
            if chunked && buf[..n].ends_with(b"0\r\n\r\n") {
                return Ok(true);
            }
        }
    }
}

// --- UDP worker ---

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
                            loop {
                                match tokio::time::timeout(task_timeout, backend_rx.recv(&mut reply_buf)).await {
                                    Ok(Ok(n)) => {
                                        task_last_active.store(
                                            epoch.elapsed().as_secs(),
                                            std::sync::atomic::Ordering::Relaxed,
                                        );
                                        if let Err(_e) = listener_socket.send_to(&reply_buf[..n], client_addr).await {
                                            break;
                                        }
                                    }
                                    // Timeout or error — session expired
                                    _ => break,
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

async fn handle_tcp_connection(
    mut client: TcpStream,
    backend_addr: SocketAddr,
    initial_data: &[u8],
) -> Result<()> {
    let mut backend = TcpStream::connect(backend_addr).await?;
    backend.set_nodelay(true)?;

    backend.write_all(initial_data).await?;

    tokio::io::copy_bidirectional(&mut client, &mut backend).await?;
    Ok(())
}

async fn send_error_response(client: &mut TcpStream, error_code: u16) -> Result<()> {
    let (status_text, body) = match error_code {
        400 => ("Bad Request", "400 Bad Request"),
        404 => ("Not Found", "404 Not Found"),
        502 => ("Bad Gateway", "502 Bad Gateway"),
        503 => ("Service Unavailable", "503 Service Unavailable"),
        504 => ("Gateway Timeout", "504 Gateway Timeout"),
        _ => ("Internal Server Error", "500 Internal Server Error"),
    };

    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        error_code, status_text, body.len(), body
    );

    client.write_all(response.as_bytes()).await?;
    Ok(())
}

// --- HTTP response parsing helpers ---

fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|pos| pos + 4)
}

fn parse_content_length(headers: &[u8]) -> Option<usize> {
    let headers_str = std::str::from_utf8(headers).ok()?;
    for line in headers_str.lines() {
        if line.len() > 16 && line[..16].eq_ignore_ascii_case("content-length: ") {
            return line[16..].trim().parse().ok();
        }
        if line.len() > 15 && line[..15].eq_ignore_ascii_case("content-length:") {
            return line[15..].trim().parse().ok();
        }
    }
    None
}

fn is_chunked_transfer(headers: &[u8]) -> bool {
    let headers_str = match std::str::from_utf8(headers) {
        Ok(s) => s,
        Err(_) => return false,
    };
    for line in headers_str.lines() {
        if line.len() > 19 && line[..19].eq_ignore_ascii_case("transfer-encoding: ") {
            return line[19..].trim().eq_ignore_ascii_case("chunked");
        }
        if line.len() > 18 && line[..18].eq_ignore_ascii_case("transfer-encoding:") {
            return line[18..].trim().eq_ignore_ascii_case("chunked");
        }
    }
    false
}

fn is_no_body_status(headers: &[u8]) -> bool {
    if headers.len() < 12 { return false; }
    let status = &headers[9..12];
    status == b"204" || status == b"304" || status[0] == b'1'
}

fn ends_with_chunked_terminator(data: &[u8]) -> bool {
    data.ends_with(b"0\r\n\r\n")
}
