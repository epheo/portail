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
use crate::request_processor::{self, HeaderModifications, HttpFilterData, ProcessingDecision, URLRewrite, RewrittenPath};
use crate::routing::{BackendSelector, RouteTable};

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
        ProcessingDecision::HttpForward { backend_addr, keepalive, filters } => {
            handle_http_connection(
                client, &mut buf, n, backend_addr, keepalive, filters,
                server_port, routes, pool, &mut selector,
            ).await
        }
        ProcessingDecision::HttpRedirect { status_code, location, keepalive } => {
            send_redirect_response(&mut client, status_code, &location).await?;
            if keepalive {
                handle_keepalive_after_response(client, &mut buf, server_port, routes, pool, &mut selector).await
            } else {
                Ok(())
            }
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
    initial_filters: Option<Box<HttpFilterData>>,
    server_port: u16,
    routes: Arc<ArcSwap<RouteTable>>,
    pool: Arc<BackendPool>,
    selector: &mut BackendSelector,
) -> Result<()> {
    let mut backend_addr = initial_backend_addr;
    let mut keepalive = initial_keepalive;
    let mut request_bytes = initial_bytes;
    let mut filter_data = initial_filters;

    loop {
        let mut backend = pool.acquire(backend_addr).await?;

        if let Some(ref fd) = filter_data {
            let has_mods = fd.request_header_mods.is_some() || fd.url_rewrite.is_some();
            if has_mods {
                let modified = apply_request_modifications(
                    &buf[..request_bytes],
                    fd.request_header_mods.as_ref(),
                    fd.url_rewrite.as_ref(),
                );
                if !fd.mirror_addrs.is_empty() { dispatch_mirrors(&fd.mirror_addrs, &modified); }
                backend.write_all(&modified).await?;
            } else {
                if !fd.mirror_addrs.is_empty() { dispatch_mirrors(&fd.mirror_addrs, &buf[..request_bytes]); }
                backend.write_all(&buf[..request_bytes]).await?;
            }
        } else {
            backend.write_all(&buf[..request_bytes]).await?;
        }

        let resp_mods = filter_data.as_ref().and_then(|fd| fd.response_header_mods.as_ref());
        let response_complete = forward_http_response(&mut backend, &mut client, buf, resp_mods).await?;

        if response_complete && keepalive {
            pool.release(backend_addr, backend);
        }

        if !keepalive {
            return Ok(());
        }

        request_bytes = client.read(buf).await?;
        if request_bytes == 0 {
            return Ok(());
        }

        let decision = {
            let route_table = routes.load();
            request_processor::analyze_request(&route_table, selector, &buf[..request_bytes], server_port)?
        };

        match decision {
            ProcessingDecision::HttpForward { backend_addr: addr, keepalive: ka, filters } => {
                backend_addr = addr;
                keepalive = ka;
                filter_data = filters;
            }
            ProcessingDecision::HttpRedirect { status_code, location, keepalive: ka } => {
                send_redirect_response(&mut client, status_code, &location).await?;
                if !ka {
                    return Ok(());
                }
                continue;
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

/// After sending a redirect on a keepalive connection, continue reading requests.
async fn handle_keepalive_after_response(
    mut client: TcpStream,
    buf: &mut Vec<u8>,
    server_port: u16,
    routes: Arc<ArcSwap<RouteTable>>,
    pool: Arc<BackendPool>,
    selector: &mut BackendSelector,
) -> Result<()> {
    loop {
        let n = client.read(buf).await?;
        if n == 0 {
            return Ok(());
        }

        let decision = {
            let route_table = routes.load();
            request_processor::analyze_request(&route_table, selector, &buf[..n], server_port)?
        };

        match decision {
            ProcessingDecision::HttpForward { backend_addr, keepalive, filters } => {
                return handle_http_connection(
                    client, buf, n, backend_addr, keepalive, filters,
                    server_port, routes, pool, selector,
                ).await;
            }
            ProcessingDecision::HttpRedirect { status_code, location, keepalive } => {
                send_redirect_response(&mut client, status_code, &location).await?;
                if !keepalive {
                    return Ok(());
                }
                continue;
            }
            ProcessingDecision::SendHttpError { error_code, close_connection } => {
                send_error_response(&mut client, error_code).await?;
                if close_connection {
                    return Ok(());
                }
                continue;
            }
            _ => return Ok(()),
        }
    }
}

/// Forward the backend's HTTP response to the client.
/// Returns true if the response was fully received (backend connection reusable).
async fn forward_http_response(
    backend: &mut TcpStream,
    client: &mut TcpStream,
    buf: &mut [u8],
    response_mods: Option<&HeaderModifications>,
) -> Result<bool> {
    if response_mods.is_some() {
        forward_http_response_with_mods(backend, client, buf, response_mods.unwrap()).await
    } else {
        forward_http_response_passthrough(backend, client, buf).await
    }
}

/// Fast path: stream response bytes to client immediately, track headers
/// only for completion detection (connection reuse). Zero buffering delay.
async fn forward_http_response_passthrough(
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

        // Write to client immediately — don't wait for header parsing
        client.write_all(&buf[..n]).await?;

        if !headers_parsed {
            header_buf.extend_from_slice(&buf[..n]);

            if let Some(header_end) = find_header_end(&header_buf) {
                headers_parsed = true;
                let headers = &header_buf[..header_end];
                content_length = parse_content_length(headers);
                chunked = is_chunked_transfer(headers);
                body_bytes_forwarded = header_buf.len() - header_end;

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

/// Slow path: buffer headers to apply modifications before forwarding.
async fn forward_http_response_with_mods(
    backend: &mut TcpStream,
    client: &mut TcpStream,
    buf: &mut [u8],
    mods: &HeaderModifications,
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

        if !headers_parsed {
            header_buf.extend_from_slice(&buf[..n]);

            if let Some(header_end) = find_header_end(&header_buf) {
                headers_parsed = true;

                let modified_headers = apply_response_header_mods(&header_buf[..header_end], mods);
                client.write_all(&modified_headers).await?;
                if header_buf.len() > header_end {
                    client.write_all(&header_buf[header_end..]).await?;
                }

                let headers = &header_buf[..header_end];
                content_length = parse_content_length(headers);
                chunked = is_chunked_transfer(headers);
                body_bytes_forwarded = header_buf.len() - header_end;

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
            client.write_all(&buf[..n]).await?;

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

async fn send_redirect_response(client: &mut TcpStream, status_code: u16, location: &str) -> Result<()> {
    let status_text = match status_code {
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        _ => "Found",
    };

    let body = format!("{} {}", status_code, status_text);
    let response = format!(
        "HTTP/1.1 {} {}\r\nLocation: {}\r\nContent-Length: {}\r\n\r\n{}",
        status_code, status_text, location, body.len(), body
    );

    client.write_all(response.as_bytes()).await?;
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

// --- Request modification ---

/// Fire-and-forget mirror dispatch. Response is discarded per Gateway API spec.
fn dispatch_mirrors(mirror_addrs: &[SocketAddr], data: &[u8]) {
    for &addr in mirror_addrs {
        let data = data.to_vec();
        tokio::spawn(async move {
            if let Ok(mut conn) = TcpStream::connect(addr).await {
                let _ = conn.write_all(&data).await;
            }
        });
    }
}

/// Apply request modifications: header mods and/or URL rewrite.
/// Operates on raw bytes — no intermediate String allocations.
fn apply_request_modifications(
    original: &[u8],
    header_mods: Option<&HeaderModifications>,
    url_rewrite: Option<&URLRewrite>,
) -> Vec<u8> {
    let header_end = find_header_end(original).unwrap_or(original.len());
    let header_region = &original[..header_end];
    let body = &original[header_end..];

    let mut out = Vec::with_capacity(original.len() + 256);
    let rewrite_hostname = url_rewrite.and_then(|r| r.hostname.as_deref());

    // Process line by line on raw bytes
    let mut pos = 0;
    let mut first_line = true;
    while pos < header_region.len() {
        let line_start = pos;
        // Find \r\n boundary
        while pos < header_region.len() && header_region[pos] != b'\r' && header_region[pos] != b'\n' {
            pos += 1;
        }
        let line = &header_region[line_start..pos];

        // Skip past CRLF
        if pos < header_region.len() && header_region[pos] == b'\r' { pos += 1; }
        if pos < header_region.len() && header_region[pos] == b'\n' { pos += 1; }

        if line.is_empty() {
            continue;
        }

        if first_line {
            first_line = false;
            // Request line: rewrite path if needed
            if let Some(rewrite) = url_rewrite {
                if let Some(ref rewritten) = rewrite.path {
                    let new_path = match rewritten {
                        RewrittenPath::Full(p) => p.as_bytes(),
                        RewrittenPath::PrefixReplaced(p) => p.as_bytes(),
                    };
                    // Find METHOD and VERSION by locating the two spaces
                    if let Some(sp1) = line.iter().position(|&b| b == b' ') {
                        if let Some(sp2) = line[sp1 + 1..].iter().position(|&b| b == b' ') {
                            out.extend_from_slice(&line[..sp1 + 1]);
                            out.extend_from_slice(new_path);
                            out.extend_from_slice(&line[sp1 + 1 + sp2..]);
                            out.extend_from_slice(b"\r\n");
                            continue;
                        }
                    }
                }
            }
            out.extend_from_slice(line);
            out.extend_from_slice(b"\r\n");
            continue;
        }

        // Header line: find colon to extract name
        let colon_pos = match line.iter().position(|&b| b == b':') {
            Some(p) => p,
            None => {
                out.extend_from_slice(line);
                out.extend_from_slice(b"\r\n");
                continue;
            }
        };
        let name = &line[..colon_pos];

        // Replace Host header if hostname rewrite is active
        if let Some(new_host) = rewrite_hostname {
            if name.eq_ignore_ascii_case(b"host") {
                out.extend_from_slice(b"Host: ");
                out.extend_from_slice(new_host.as_bytes());
                out.extend_from_slice(b"\r\n");
                continue;
            }
        }

        if let Some(mods) = header_mods {
            // SAFETY: header names are ASCII per RFC 7230
            let name_str = unsafe { std::str::from_utf8_unchecked(name) };

            if mods.remove.iter().any(|r| r.eq_ignore_ascii_case(name_str)) {
                continue;
            }
            if let Some(set_header) = mods.set.iter().find(|h| h.name.eq_ignore_ascii_case(name_str)) {
                out.extend_from_slice(set_header.name.as_bytes());
                out.extend_from_slice(b": ");
                out.extend_from_slice(set_header.value.as_bytes());
                out.extend_from_slice(b"\r\n");
                continue;
            }
        }

        out.extend_from_slice(line);
        out.extend_from_slice(b"\r\n");
    }

    // Add headers
    if let Some(mods) = header_mods {
        for h in &mods.add {
            out.extend_from_slice(h.name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(h.value.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
    }

    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

/// Apply response header modifications to buffered response headers.
/// Returns the modified header region (including trailing \r\n\r\n).
fn apply_response_header_mods(headers: &[u8], mods: &HeaderModifications) -> Vec<u8> {
    let header_str = match std::str::from_utf8(headers) {
        Ok(s) => s,
        Err(_) => return headers.to_vec(),
    };

    let lines: Vec<&str> = header_str.lines().collect();
    if lines.is_empty() {
        return headers.to_vec();
    }

    let status_line = lines[0];
    let header_lines = &lines[1..];

    let mut result_headers: Vec<String> = Vec::with_capacity(header_lines.len() + mods.add.len() + mods.set.len());

    for line in header_lines {
        if line.is_empty() {
            continue;
        }
        let colon_pos = match line.find(':') {
            Some(p) => p,
            None => continue,
        };
        let name = &line[..colon_pos];

        if mods.remove.iter().any(|r| r.eq_ignore_ascii_case(name)) {
            continue;
        }

        if let Some(set_header) = mods.set.iter().find(|h| h.name.eq_ignore_ascii_case(name)) {
            result_headers.push(format!("{}: {}", set_header.name, set_header.value));
            continue;
        }

        result_headers.push(line.to_string());
    }

    for h in &mods.add {
        result_headers.push(format!("{}: {}", h.name, h.value));
    }

    let mut out = Vec::with_capacity(headers.len() + 256);
    out.extend_from_slice(status_line.as_bytes());
    out.extend_from_slice(b"\r\n");
    for h in &result_headers {
        out.extend_from_slice(h.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");

    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request_processor::{URLRewrite, RewrittenPath, HeaderModifications};
    use crate::routing::HttpHeader;

    #[test]
    fn test_apply_modifications_path_rewrite() {
        let request = b"GET /old/path HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let rewrite = URLRewrite {
            hostname: None,
            path: Some(RewrittenPath::Full("/new/path".to_string())),
        };
        let result = apply_request_modifications(request, None, Some(&rewrite));
        let result_str = std::str::from_utf8(&result).unwrap();

        assert!(result_str.starts_with("GET /new/path HTTP/1.1\r\n"));
        assert!(result_str.contains("Host: example.com"));
    }

    #[test]
    fn test_apply_modifications_host_rewrite() {
        let request = b"GET / HTTP/1.1\r\nHost: original.example.com\r\n\r\n";
        let rewrite = URLRewrite {
            hostname: Some("rewritten.example.com".to_string()),
            path: None,
        };
        let result = apply_request_modifications(request, None, Some(&rewrite));
        let result_str = std::str::from_utf8(&result).unwrap();

        assert!(result_str.contains("Host: rewritten.example.com"));
        assert!(!result_str.contains("original.example.com"));
    }

    #[test]
    fn test_apply_modifications_combined() {
        let request = b"GET /old HTTP/1.1\r\nHost: old.com\r\nUser-Agent: test\r\n\r\n";
        let rewrite = URLRewrite {
            hostname: Some("new.com".to_string()),
            path: Some(RewrittenPath::Full("/new".to_string())),
        };
        let mods = HeaderModifications {
            add: vec![HttpHeader { name: "X-Added".to_string(), value: "yes".to_string() }],
            set: vec![],
            remove: vec!["User-Agent".to_string()],
        };
        let result = apply_request_modifications(request, Some(&mods), Some(&rewrite));
        let result_str = std::str::from_utf8(&result).unwrap();

        assert!(result_str.starts_with("GET /new HTTP/1.1\r\n"));
        assert!(result_str.contains("Host: new.com"));
        assert!(result_str.contains("X-Added: yes"));
        assert!(!result_str.contains("User-Agent"));
    }

    #[test]
    fn test_apply_modifications_no_mods_passthrough() {
        let request = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let result = apply_request_modifications(request, None, None);
        // Should produce equivalent output (headers may be reformatted but content same)
        let result_str = std::str::from_utf8(&result).unwrap();
        assert!(result_str.starts_with("GET / HTTP/1.1\r\n"));
        assert!(result_str.contains("Host: example.com"));
    }

    #[test]
    fn test_apply_response_header_mods_add() {
        let headers = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n";
        let mods = HeaderModifications {
            add: vec![HttpHeader { name: "X-Custom".to_string(), value: "added".to_string() }],
            set: vec![],
            remove: vec![],
        };
        let result = apply_response_header_mods(headers, &mods);
        let result_str = std::str::from_utf8(&result).unwrap();
        assert!(result_str.contains("X-Custom: added"));
    }

    #[test]
    fn test_apply_response_header_mods_remove() {
        let headers = b"HTTP/1.1 200 OK\r\nX-Internal: secret\r\nContent-Length: 5\r\n\r\n";
        let mods = HeaderModifications {
            add: vec![],
            set: vec![],
            remove: vec!["X-Internal".to_string()],
        };
        let result = apply_response_header_mods(headers, &mods);
        let result_str = std::str::from_utf8(&result).unwrap();
        assert!(!result_str.contains("X-Internal"));
        assert!(result_str.contains("Content-Length: 5"));
    }

    #[test]
    fn test_apply_response_header_mods_set() {
        let headers = b"HTTP/1.1 200 OK\r\nServer: old-server\r\n\r\n";
        let mods = HeaderModifications {
            add: vec![],
            set: vec![HttpHeader { name: "Server".to_string(), value: "uringress".to_string() }],
            remove: vec![],
        };
        let result = apply_response_header_mods(headers, &mods);
        let result_str = std::str::from_utf8(&result).unwrap();
        assert!(result_str.contains("Server: uringress"));
        assert!(!result_str.contains("old-server"));
    }
}
