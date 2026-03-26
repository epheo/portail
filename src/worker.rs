//! Tokio async TCP worker — one task per accepted connection.
//!
//! HTTP request/response proxying with keepalive and backend connection pooling,
//! raw TCP bidirectional forwarding, TLS termination, and TLS passthrough.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use arc_swap::ArcSwap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use anyhow::Result;

use crate::backend_pool::BackendPool;
use crate::health::HealthRegistry;
use crate::http_filters::{apply_request_header_modifications, apply_response_header_mods, dispatch_mirrors};
use crate::http_parser::find_header_end;
use crate::logging::{warn, info, debug};
use crate::request_processor::{self, HeaderModifications, HttpFilterData, ProcessingDecision};
use crate::routing::{BackendSelector, RouteTable};
use crate::tls::{self, Connection, DynamicTlsAcceptor};

/// Quick check if data starts with a known HTTP method.
/// Used to decide whether to accumulate headers or pass through for raw TCP.
#[inline]
fn looks_like_http(data: &[u8]) -> bool {
    data.starts_with(b"GET ")
        || data.starts_with(b"POST ")
        || data.starts_with(b"PUT ")
        || data.starts_with(b"DELETE ")
        || data.starts_with(b"HEAD ")
        || data.starts_with(b"OPTIONS ")
        || data.starts_with(b"PATCH ")
        || data.starts_with(b"CONNECT ")
        || data.starts_with(b"TRACE ")
}

/// Continue reading from `client` into `buf` starting at offset `already_read`
/// until the full HTTP headers (\r\n\r\n) are found or the buffer fills.
async fn read_remaining_headers(client: &mut Connection, buf: &mut [u8], already_read: usize) -> Result<usize> {
    let mut total = already_read;
    loop {
        let n = client.read(&mut buf[total..]).await?;
        if n == 0 {
            return Ok(total); // Client closed mid-headers — use what we have
        }
        total += n;
        if find_header_end(&buf[..total]).is_some() {
            return Ok(total);
        }
        if total >= buf.len() {
            return Err(anyhow::anyhow!("Request headers exceed buffer size"));
        }
    }
}

struct ConnectionState {
    server_port: u16,
    routes: Arc<ArcSwap<RouteTable>>,
    pool: BackendPool,
    selector: Arc<BackendSelector>,
    health: Arc<HealthRegistry>,
    /// Whether this connection was accepted via TLS (used for redirect scheme).
    is_tls: bool,
    /// Reused across keepalive responses to avoid per-response heap allocation.
    header_buf: Vec<u8>,
}

/// Run an accept loop on a single listener, spawning a task per connection.
pub async fn run_worker(
    worker_id: usize,
    listener: TcpListener,
    server_port: u16,
    routes: Arc<ArcSwap<RouteTable>>,
    max_idle_per_backend: usize,
    connect_timeout: Duration,
    shutdown: CancellationToken,
    tls_acceptor: Option<Arc<DynamicTlsAcceptor>>,
    tls_passthrough: bool,
    health: Arc<HealthRegistry>,
) {
    info!("Worker {} accepting on port {}", worker_id, server_port);

    // Shared selector across all connections in this worker so weighted routing
    // counters increment properly across separate TCP connections.
    let shared_selector = Arc::new(BackendSelector::new());

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!("Worker {} shutting down", worker_id);
                break;
            }
            result = listener.accept() => {
                match result {
                    Ok((tcp_stream, peer)) => {
                        let routes = routes.clone();
                        let acceptor = tls_acceptor.clone();
                        let health = health.clone();

                        if tls_passthrough && acceptor.is_none() {
                            // Pure passthrough mode (no TLS termination cert available)
                            tokio::spawn(async move {
                                if let Err(_e) = handle_tls_passthrough(tcp_stream, server_port, routes, health).await {
                                    debug!("TLS passthrough from {} closed: {}", peer, _e);
                                }
                            });
                        } else if let Some(acceptor) = acceptor {
                            // TLS termination mode — but first check if this SNI
                            // should be passed through instead of terminated.
                            // This handles the case where both HTTPS/Terminate and
                            // TLS/Passthrough listeners share the same port.
                            let selector = shared_selector.clone();
                            tokio::spawn(async move {
                                // Peek ClientHello for SNI to decide dispatch mode
                                let should_passthrough = {
                                    let mut peek_buf = [0u8; 16384];
                                    match tokio::time::timeout(
                                        Duration::from_secs(5),
                                        tcp_stream.peek(&mut peek_buf),
                                    ).await {
                                        Ok(Ok(n)) if n > 0 => {
                                            let sni = crate::tls::extract_sni(&peek_buf[..n]);
                                            if let Some(ref hostname) = sni {
                                                let rt = routes.load();
                                                rt.has_tls_passthrough_route(hostname, server_port)
                                            } else {
                                                false
                                            }
                                        }
                                        _ => false,
                                    }
                                };

                                if should_passthrough {
                                    if let Err(_e) = handle_tls_passthrough(tcp_stream, server_port, routes, health).await {
                                        debug!("TLS passthrough from {} closed: {}", peer, _e);
                                    }
                                } else {
                                    match acceptor.acceptor().accept(tcp_stream).await {
                                        Ok(tls_stream) => {
                                            let conn = Connection::Tls { inner: tls_stream };
                                            let state = ConnectionState {
                                                server_port,
                                                routes,
                                                pool: BackendPool::new(max_idle_per_backend, connect_timeout),
                                                selector,
                                                health,
                                                is_tls: true,
                                                header_buf: Vec::with_capacity(1024),
                                            };
                                            if let Err(_e) = handle_connection(conn, peer, state).await {
                                                debug!("TLS connection from {} closed: {}", peer, _e);
                                            }
                                        }
                                        Err(_e) => {
                                            debug!("TLS handshake failed from {}: {}", peer, _e);
                                        }
                                    }
                                }
                            });
                        } else {
                            let selector = shared_selector.clone();
                            let state = ConnectionState {
                                server_port,
                                routes,
                                pool: BackendPool::new(max_idle_per_backend, connect_timeout),
                                selector,
                                health,
                                is_tls: false,
                                header_buf: Vec::with_capacity(1024),
                            };
                            tokio::spawn(async move {
                                let start = std::time::Instant::now();
                                let conn = Connection::Plain { inner: tcp_stream };
                                let result = handle_connection(conn, peer, state).await;
                                let elapsed = start.elapsed();
                                match result {
                                    Ok(()) => info!("{} port={} duration={:?}", peer, server_port, elapsed),
                                    Err(_e) => {
                                        debug!("Connection from {} closed: {}", peer, _e);
                                    }
                                }
                            });
                        }
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
    mut client: Connection,
    _peer: SocketAddr,
    mut state: ConnectionState,
) -> Result<()> {
    let _conn_start = std::time::Instant::now();
    client.set_nodelay(true)?;

    let mut buf = vec![0u8; 65536];

    let mut n = client.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }

    // If data looks like HTTP but headers aren't complete, accumulate more reads.
    // TCP/binary data (no \r\n\r\n expected) passes through immediately.
    if looks_like_http(&buf[..n]) && find_header_end(&buf[..n]).is_none() {
        match read_remaining_headers(&mut client, &mut buf, n).await {
            Ok(total) => n = total,
            Err(e) => {
                let _ = send_error_response(&mut client, 431).await;
                return Err(e);
            }
        }
    }

    loop {
        let decision = {
            let route_table = state.routes.load();
            request_processor::analyze_request(&route_table, &state.selector, &buf[..n], state.server_port, &state.health, state.is_tls)?
        };

        match decision {
            ProcessingDecision::TcpForward { backend_addr } => {
                return handle_tcp_connection(client, backend_addr, &buf[..n], Arc::clone(&state.health)).await;
            }
            ProcessingDecision::HttpForward { backend_addr, keepalive, filters, backend_timeout, request_timeout, content_length, is_chunked, is_upgrade, backend_use_tls, backend_server_name } => {
                let forward_fut = handle_http_forward(
                    &mut client, &mut buf, n, backend_addr, keepalive, filters, backend_timeout, content_length, is_chunked, is_upgrade, backend_use_tls, backend_server_name, &mut state,
                );
                let ka = if !is_upgrade {
                    if let Some(req_timeout) = request_timeout.filter(|d| !d.is_zero()) {
                        match tokio::time::timeout(req_timeout, forward_fut).await {
                            Ok(result) => result?,
                            Err(_) => {
                                let _ = send_error_response(&mut client, 504).await;
                                return Ok(());
                            }
                        }
                    } else {
                        forward_fut.await?
                    }
                } else {
                    // Upgrades (WebSocket) must not be wrapped in request_timeout —
                    // the bidirectional stream is long-lived.
                    forward_fut.await?
                };
                if !ka {
                    return Ok(());
                }
            }
            ProcessingDecision::HttpRedirect { status_code, location, keepalive } => {
                send_redirect_response(&mut client, status_code, &location).await?;
                if !keepalive {
                    return Ok(());
                }
            }
            ProcessingDecision::SendHttpError { error_code, close_connection } => {
                send_error_response(&mut client, error_code).await?;
                if close_connection {
                    return Ok(());
                }
            }
            ProcessingDecision::UdpForward { .. } | ProcessingDecision::CloseConnection => {
                return Ok(());
            }
        }

        n = client.read(&mut buf).await?;
        if n > 0 && find_header_end(&buf[..n]).is_none() {
            match read_remaining_headers(&mut client, &mut buf, n).await {
                Ok(total) => n = total,
                Err(e) => {
                    let _ = send_error_response(&mut client, 431).await;
                    return Err(e);
                }
            }
        }
        if n == 0 {
            return Ok(());
        }
    }
}

/// Handle HTTP forward requests in a keepalive loop.
/// Returns true if the connection should continue (keepalive), false to close.
async fn handle_http_forward(
    client: &mut Connection,
    buf: &mut [u8],
    initial_bytes: usize,
    initial_backend_addr: SocketAddr,
    initial_keepalive: bool,
    initial_filters: Option<Box<HttpFilterData>>,
    initial_backend_timeout: Option<std::time::Duration>,
    initial_content_length: Option<usize>,
    initial_is_chunked: bool,
    initial_is_upgrade: bool,
    initial_backend_use_tls: bool,
    initial_backend_server_name: String,
    state: &mut ConnectionState,
) -> Result<bool> {
    let mut backend_addr = initial_backend_addr;
    let mut keepalive = initial_keepalive;
    let mut request_bytes = initial_bytes;
    let mut filter_data = initial_filters;
    let mut per_rule_timeout = initial_backend_timeout;
    let mut req_content_length = initial_content_length;
    let mut req_is_chunked = initial_is_chunked;
    let mut req_is_upgrade = initial_is_upgrade;
    let mut use_tls = initial_backend_use_tls;
    let mut server_name = initial_backend_server_name;

    loop {
        let timeout_dur = per_rule_timeout
            .filter(|d| !d.is_zero())
            .unwrap_or(std::time::Duration::from_secs(30));
        let mut backend = match tokio::time::timeout(timeout_dur, state.pool.acquire(backend_addr, use_tls, &server_name)).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(e)) => {
                warn!("Backend {} connect failed: {}", backend_addr, e);
                if state.health.record_failure(backend_addr) {
                    HealthRegistry::spawn_probe(Arc::clone(&state.health), backend_addr);
                }
                send_error_response(client, 502).await?;
                return Ok(false);
            }
            Err(_) => {
                warn!("Backend {} connect timeout", backend_addr);
                if state.health.record_failure(backend_addr) {
                    HealthRegistry::spawn_probe(Arc::clone(&state.health), backend_addr);
                }
                send_error_response(client, 504).await?;
                return Ok(false);
            }
        };

        // Determine header boundary for body separation
        let header_end = find_header_end(&buf[..request_bytes]).unwrap_or(request_bytes);

        if let Some(ref fd) = filter_data {
            let has_mods = fd.request_header_mods.is_some() || fd.url_rewrite.is_some();
            if has_mods {
                let modified_headers = apply_request_header_modifications(
                    &buf[..header_end],
                    fd.request_header_mods.as_ref(),
                    fd.url_rewrite.as_ref(),
                );
                // Mirror needs full request (headers + body)
                if !fd.mirror_addrs.is_empty() {
                    let mut mirror_data = modified_headers.clone();
                    mirror_data.extend_from_slice(&buf[header_end..request_bytes]);
                    dispatch_mirrors(&fd.mirror_addrs, &mirror_data);
                }
                // Send modified headers, then original body — zero-copy for body
                backend.write_all(&modified_headers).await?;
                let body = &buf[header_end..request_bytes];
                if !body.is_empty() {
                    backend.write_all(body).await?;
                }
            } else {
                if !fd.mirror_addrs.is_empty() { dispatch_mirrors(&fd.mirror_addrs, &buf[..request_bytes]); }
                backend.write_all(&buf[..request_bytes]).await?;
            }
        } else {
            backend.write_all(&buf[..request_bytes]).await?;
        }

        // --- Relay remaining request body bytes (the core POST fix) ---
        let body_in_initial = request_bytes.saturating_sub(header_end);
        if let Some(cl) = req_content_length {
            let mut remaining = cl.saturating_sub(body_in_initial);
            while remaining > 0 {
                let n = client.read(&mut buf[..]).await?;
                if n == 0 { break; }
                let to_send = n.min(remaining);
                backend.write_all(&buf[..to_send]).await?;
                remaining -= to_send;
            }
        } else if req_is_chunked {
            // Relay chunked body until 0\r\n\r\n terminator
            let mut chunked_tail = [0u8; 5];
            let mut chunked_tail_len: usize = 0;
            // Check if terminator was already in the initial buffer body
            if body_in_initial > 0 {
                append_chunked_tail(&mut chunked_tail, &mut chunked_tail_len, &buf[header_end..request_bytes]);
            }
            while !is_chunked_terminator(&chunked_tail, chunked_tail_len) {
                let n = client.read(&mut buf[..]).await?;
                if n == 0 { break; }
                backend.write_all(&buf[..n]).await?;
                append_chunked_tail(&mut chunked_tail, &mut chunked_tail_len, &buf[..n]);
            }
        }

        // --- WebSocket / protocol upgrade handling ---
        if req_is_upgrade {
            // For upgrades, forward the backend response headers then switch
            // to bidirectional streaming for the remainder of the connection.
            let resp_mods = filter_data.as_ref().and_then(|fd| fd.response_header_mods.as_ref());
            let _response_complete = match tokio::time::timeout(
                timeout_dur,
                forward_http_response(&mut backend, client, buf, resp_mods, &mut state.header_buf),
            ).await {
                Ok(result) => result?,
                Err(_) => {
                    send_error_response(client, 504).await?;
                    return Ok(false);
                }
            };
            // Flush the TLS stream so the browser receives the 101 before
            // we switch to bidirectional streaming. Without this, the encrypted
            // 101 response may remain buffered and the browser never upgrades.
            client.flush().await?;
            // After sending the 101 response, switch to raw bidirectional streaming.
            // Errors are expected when either side closes — best-effort.
            let _ = tokio::io::copy_bidirectional(client, &mut backend).await;
            return Ok(false);
        }

        let resp_mods = filter_data.as_ref().and_then(|fd| fd.response_header_mods.as_ref());
        let response_complete = match tokio::time::timeout(
            timeout_dur,
            forward_http_response(&mut backend, client, buf, resp_mods, &mut state.header_buf),
        ).await {
            Ok(result) => result?,
            Err(_) => {
                send_error_response(client, 504).await?;
                return Ok(false);
            }
        };
        // Flush the TLS stream so the client receives all response bytes.
        // Without this, the tail of large responses may remain buffered in
        // rustls and stall the client (plain TCP is unaffected — no-op flush).
        client.flush().await?;

        if response_complete && keepalive {
            state.pool.release(backend_addr, backend);
        }

        if !keepalive {
            return Ok(false);
        }

        request_bytes = client.read(buf).await?;
        if request_bytes > 0 && find_header_end(&buf[..request_bytes]).is_none() {
            match read_remaining_headers(client, buf, request_bytes).await {
                Ok(total) => request_bytes = total,
                Err(e) => {
                    let _ = send_error_response(client, 431).await;
                    return Err(e);
                }
            }
        }
        if request_bytes == 0 {
            return Ok(false);
        }

        let decision = {
            let route_table = state.routes.load();
            request_processor::analyze_request(&route_table, &state.selector, &buf[..request_bytes], state.server_port, &state.health, state.is_tls)?
        };

        match decision {
            ProcessingDecision::HttpForward { backend_addr: addr, keepalive: ka, filters, backend_timeout: bt, request_timeout: _, content_length: cl, is_chunked: ch, is_upgrade: up, backend_use_tls: tls, backend_server_name: sn } => {
                backend_addr = addr;
                keepalive = ka;
                filter_data = filters;
                per_rule_timeout = bt;
                req_content_length = cl;
                req_is_chunked = ch;
                req_is_upgrade = up;
                use_tls = tls;
                server_name = sn;
            }
            ProcessingDecision::HttpRedirect { status_code, location, keepalive: ka } => {
                send_redirect_response(client, status_code, &location).await?;
                if !ka {
                    return Ok(false);
                }
                return Ok(true);
            }
            ProcessingDecision::SendHttpError { error_code, close_connection } => {
                send_error_response(client, error_code).await?;
                if close_connection {
                    return Ok(false);
                }
                return Ok(true);
            }
            ProcessingDecision::TcpForward { .. } | ProcessingDecision::UdpForward { .. } | ProcessingDecision::CloseConnection => {
                return Ok(false);
            }
        }
    }
}

/// Forward the backend's HTTP response to the client.
/// Returns true if the response was fully received (backend connection reusable).
///
/// Without mods (passthrough): writes each chunk to client immediately,
/// buffers headers only for completion tracking. Zero buffering delay.
/// With mods: buffers until headers complete, applies modifications, then writes.
async fn forward_http_response(
    backend: &mut Connection,
    client: &mut Connection,
    buf: &mut [u8],
    response_mods: Option<&HeaderModifications>,
    header_buf: &mut Vec<u8>,
) -> Result<bool> {
    let mut headers_parsed = false;
    let mut content_length: Option<usize> = None;
    let mut body_bytes_forwarded: usize = 0;
    let mut chunked = false;
    header_buf.clear();
    // Rolling tail buffer for detecting chunked terminator across read boundaries.
    // Tracks the last 5 bytes (len of b"0\r\n\r\n") seen so far in the body.
    let mut chunked_tail = [0u8; 5];
    let mut chunked_tail_len: usize = 0;

    loop {
        let n = backend.read(buf).await?;
        if n == 0 {
            return Ok(false);
        }

        if !headers_parsed {
            // Passthrough: write to client before buffering for header parsing
            if response_mods.is_none() {
                client.write_all(&buf[..n]).await?;
            }

            header_buf.extend_from_slice(&buf[..n]);

            if let Some(header_end) = find_header_end(header_buf) {
                headers_parsed = true;

                // With mods: apply modifications and write buffered data now
                if let Some(mods) = response_mods {
                    let modified = apply_response_header_mods(&header_buf[..header_end], mods);
                    client.write_all(&modified).await?;
                    if header_buf.len() > header_end {
                        client.write_all(&header_buf[header_end..]).await?;
                    }
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
                if chunked {
                    let body_so_far = &header_buf[header_end..];
                    append_chunked_tail(&mut chunked_tail, &mut chunked_tail_len, body_so_far);
                    if is_chunked_terminator(&chunked_tail, chunked_tail_len) {
                        return Ok(true);
                    }
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
            if chunked {
                append_chunked_tail(&mut chunked_tail, &mut chunked_tail_len, &buf[..n]);
                if is_chunked_terminator(&chunked_tail, chunked_tail_len) {
                    return Ok(true);
                }
            }
        }
    }
}

/// L4 TLS passthrough: peek ClientHello for SNI, then forward raw TCP to backend.
async fn handle_tls_passthrough(
    mut client: TcpStream,
    server_port: u16,
    routes: Arc<ArcSwap<RouteTable>>,
    health: Arc<HealthRegistry>,
) -> Result<()> {
    let mut peek_buf = [0u8; 16384];
    let n = match tokio::time::timeout(Duration::from_secs(5), client.peek(&mut peek_buf)).await {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => return Err(anyhow::anyhow!("TLS passthrough: client sent no data within 5s")),
    };
    if n == 0 {
        return Ok(());
    }

    let sni = tls::extract_sni(&peek_buf[..n]);
    let hostname = match sni.as_deref() {
        Some(h) => h,
        None => return Err(anyhow::anyhow!("TLS passthrough: no SNI in ClientHello, cannot route")),
    };

    let route_table = routes.load();
    let backend_addr = route_table.resolve_tls_passthrough(hostname, server_port)
        .ok_or_else(|| anyhow::anyhow!("No TLS passthrough route for SNI '{}'", hostname))?;

    match TcpStream::connect(backend_addr).await {
        Ok(mut backend) => {
            health.record_success(backend_addr);
            backend.set_nodelay(true)?;
            client.set_nodelay(true)?;
            tokio::io::copy_bidirectional(&mut client, &mut backend).await?;
        }
        Err(e) => {
            if health.record_failure(backend_addr) {
                HealthRegistry::spawn_probe(health, backend_addr);
            }
            return Err(anyhow::anyhow!("TLS passthrough connect to {} failed: {}", backend_addr, e));
        }
    }

    Ok(())
}

async fn handle_tcp_connection(
    mut client: Connection,
    backend_addr: SocketAddr,
    initial_data: &[u8],
    health: Arc<HealthRegistry>,
) -> Result<()> {
    let connect_result = tokio::time::timeout(
        Duration::from_secs(5),
        TcpStream::connect(backend_addr),
    ).await;

    match connect_result {
        Ok(Ok(mut backend)) => {
            health.record_success(backend_addr);
            backend.set_nodelay(true)?;
            backend.write_all(initial_data).await?;
            // Errors from copy_bidirectional are expected when either side closes;
            // don't propagate — the data transfer is best-effort once started.
            let _ = tokio::io::copy_bidirectional(&mut client, &mut backend).await;
        }
        Ok(Err(e)) => {
            if health.record_failure(backend_addr) {
                HealthRegistry::spawn_probe(health, backend_addr);
            }
            return Err(anyhow::anyhow!("TCP connect to {} failed: {}", backend_addr, e));
        }
        Err(_) => {
            if health.record_failure(backend_addr) {
                HealthRegistry::spawn_probe(health, backend_addr);
            }
            return Err(anyhow::anyhow!("TCP connect to {} timed out", backend_addr));
        }
    }
    Ok(())
}

async fn send_redirect_response(client: &mut Connection, status_code: u16, location: &str) -> Result<()> {
    use std::io::Write;

    let status_text = match status_code {
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        _ => "Found",
    };

    // Pre-compute body once, then build the entire response in one write! call
    // into a stack-estimated Vec. Avoids two separate format! heap allocations.
    let mut resp = Vec::with_capacity(128 + location.len());
    let _ = write!(
        resp,
        "HTTP/1.1 {} {}\r\nLocation: {}\r\nContent-Length: {}\r\n\r\n{} {}",
        status_code, status_text, location,
        // body length: digits of status_code (always 3) + 1 space + status_text length
        3 + 1 + status_text.len(),
        status_code, status_text,
    );

    client.write_all(&resp).await?;
    Ok(())
}

async fn send_error_response(client: &mut Connection, error_code: u16) -> Result<()> {
    static RESP_400: &[u8] = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 15\r\nConnection: close\r\n\r\n400 Bad Request";
    static RESP_404: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Length: 13\r\nConnection: close\r\n\r\n404 Not Found";
    static RESP_502: &[u8] = b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 15\r\nConnection: close\r\n\r\n502 Bad Gateway";
    static RESP_503: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 23\r\nConnection: close\r\n\r\n503 Service Unavailable";
    static RESP_504: &[u8] = b"HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 19\r\nConnection: close\r\n\r\n504 Gateway Timeout";
    static RESP_500: &[u8] = b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 25\r\nConnection: close\r\n\r\n500 Internal Server Error";

    let response = match error_code {
        400 => RESP_400,
        404 => RESP_404,
        502 => RESP_502,
        503 => RESP_503,
        504 => RESP_504,
        _ => RESP_500,
    };

    client.write_all(response).await?;
    Ok(())
}

// --- HTTP response parsing helpers ---

fn parse_content_length(headers: &[u8]) -> Option<usize> {
    crate::routing::find_header_value(headers, "content-length")?.parse().ok()
}

fn is_chunked_transfer(headers: &[u8]) -> bool {
    crate::routing::find_header_value(headers, "transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
}

fn is_no_body_status(headers: &[u8]) -> bool {
    if headers.len() < 12 { return false; }
    let status = &headers[9..12];
    status == b"204" || status == b"304" || status[0] == b'1'
}

/// Append new data to the rolling 5-byte tail buffer used for chunked terminator detection.
#[inline]
fn append_chunked_tail(tail: &mut [u8; 5], tail_len: &mut usize, data: &[u8]) {
    if data.len() >= 5 {
        // Fast path: new data is large enough to fill the entire tail
        tail.copy_from_slice(&data[data.len() - 5..]);
        *tail_len = 5;
    } else if !data.is_empty() {
        // Shift existing tail left and append new bytes
        let keep = 5usize.saturating_sub(data.len()).min(*tail_len);
        if keep > 0 {
            tail.copy_within((*tail_len - keep)..*tail_len, 0);
        }
        let start = keep;
        tail[start..start + data.len()].copy_from_slice(data);
        *tail_len = keep + data.len();
    }
}

/// Check if the tail buffer contains the chunked terminator `0\r\n\r\n`.
#[inline]
fn is_chunked_terminator(tail: &[u8; 5], tail_len: usize) -> bool {
    tail_len == 5 && tail == b"0\r\n\r\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunked_terminator_single_read() {
        let mut tail = [0u8; 5];
        let mut len = 0;
        append_chunked_tail(&mut tail, &mut len, b"some body data0\r\n\r\n");
        assert!(is_chunked_terminator(&tail, len));
    }

    #[test]
    fn test_chunked_terminator_split_across_two_reads() {
        let mut tail = [0u8; 5];
        let mut len = 0;
        // First read ends with "0\r\n"
        append_chunked_tail(&mut tail, &mut len, b"chunk data0\r\n");
        assert!(!is_chunked_terminator(&tail, len));
        // Second read starts with "\r\n"
        append_chunked_tail(&mut tail, &mut len, b"\r\n");
        assert!(is_chunked_terminator(&tail, len));
    }

    #[test]
    fn test_chunked_terminator_split_byte_by_byte() {
        let mut tail = [0u8; 5];
        let mut len = 0;
        for byte in b"0\r\n\r\n" {
            append_chunked_tail(&mut tail, &mut len, std::slice::from_ref(byte));
        }
        assert!(is_chunked_terminator(&tail, len));
    }

    #[test]
    fn test_chunked_terminator_false_positive_resistance() {
        let mut tail = [0u8; 5];
        let mut len = 0;
        // Contains the bytes but not at the end
        append_chunked_tail(&mut tail, &mut len, b"0\r\n\r\nmore data");
        assert!(!is_chunked_terminator(&tail, len));
    }

    #[test]
    fn test_chunked_terminator_not_enough_data() {
        let mut tail = [0u8; 5];
        let mut len = 0;
        append_chunked_tail(&mut tail, &mut len, b"\r\n");
        assert!(!is_chunked_terminator(&tail, len));
    }

    #[test]
    fn test_chunked_terminator_exact_five_bytes() {
        let mut tail = [0u8; 5];
        let mut len = 0;
        append_chunked_tail(&mut tail, &mut len, b"0\r\n\r\n");
        assert!(is_chunked_terminator(&tail, len));
    }
}
