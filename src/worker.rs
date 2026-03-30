//! Tokio async TCP worker — one task per accepted connection.
//!
//! HTTP request/response proxying with keepalive and backend connection pooling,
//! raw TCP bidirectional forwarding, TLS termination, and TLS passthrough.

use anyhow::Result;
use arc_swap::ArcSwap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use crate::backend_pool::BackendPool;
use crate::health::HealthRegistry;
use crate::http_filters::{
    apply_request_header_modifications, apply_response_header_mods, dispatch_mirrors,
    extract_header_mods, extract_url_rewrite, HeaderModifications, MIRROR_BODY_MAX,
};
use crate::http_parser::find_header_end;
use crate::logging::{debug, info, warn};
use crate::request_processor::{analyze_request, is_http_request, RequestMeta, RoutingResult};
use crate::routing::{BackendSelector, HttpFilter, HttpRouteRule, RouteTable};
use crate::tls::{self, Connection, DynamicTlsAcceptor};

/// Continue reading from `client` into `buf` starting at offset `already_read`
/// until the full HTTP headers (\r\n\r\n) are found or the buffer fills.
async fn read_remaining_headers(
    client: &mut Connection,
    buf: &mut [u8],
    already_read: usize,
) -> Result<usize> {
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
    selector: Arc<BackendSelector>,
) {
    // Selector is shared across ALL workers (not just this one) so weighted
    // round-robin counters produce the correct distribution globally.

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
                                let _ = handle_tls_passthrough(tcp_stream, server_port, routes, health).await;
                            });
                        } else if let Some(acceptor) = acceptor {
                            // TLS termination mode — but first check if this SNI
                            // should be passed through instead of terminated.
                            // This handles the case where both HTTPS/Terminate and
                            // TLS/Passthrough listeners share the same port.
                            let selector = selector.clone();
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
                                    let _ = handle_tls_passthrough(tcp_stream, server_port, routes, health).await;
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
                                            let _ = handle_connection(conn, peer, state).await;
                                        }
                                        Err(_e) => {
                                            debug!("TLS handshake failed from {}: {}", peer, _e);
                                        }
                                    }
                                }
                            });
                        } else {
                            let selector = selector.clone();
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
                                let conn = Connection::Plain { inner: tcp_stream };
                                let _ = handle_connection(conn, peer, state).await;
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
    if is_http_request(&buf[..n]) && find_header_end(&buf[..n]).is_none() {
        match read_remaining_headers(&mut client, &mut buf, n).await {
            Ok(total) => n = total,
            Err(e) => {
                let _ = send_error_response(&mut client, 431).await;
                return Err(e);
            }
        }
    }

    loop {
        let route_table = state.routes.load();
        let result = analyze_request(
            &route_table,
            &state.selector,
            &buf[..n],
            state.server_port,
            &state.health,
            state.is_tls,
        )?;

        match result {
            RoutingResult::TcpForward { backend_addr } => {
                return handle_tcp_connection(
                    client,
                    backend_addr,
                    &buf[..n],
                    Arc::clone(&state.health),
                )
                .await;
            }
            RoutingResult::HttpForward {
                backend,
                rule,
                meta,
            } => {
                let forward_fut =
                    proxy_http_request(&mut client, &mut buf, n, backend, rule, &meta, &mut state);
                let ka = if !meta.is_upgrade {
                    if let Some(req_timeout) = rule.request_timeout.filter(|d| !d.is_zero()) {
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
                if !ka || !meta.keepalive {
                    return Ok(());
                }
            }
            RoutingResult::HttpRedirect {
                status_code,
                location,
                keepalive,
            } => {
                send_redirect_response(&mut client, status_code, &location).await?;
                if !keepalive {
                    return Ok(());
                }
            }
            RoutingResult::SendHttpError {
                error_code,
                close_connection,
            } => {
                send_error_response(&mut client, error_code).await?;
                if close_connection {
                    return Ok(());
                }
            }
            RoutingResult::UdpForward { .. } | RoutingResult::CloseConnection => {
                return Ok(());
            }
        }

        // Explicitly drop the route table guard before reading the next request.
        // This allows old route tables to be freed between keepalive iterations.
        drop(route_table);

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

/// Proxy a single HTTP request to a backend and forward the response.
/// Returns true if the response was fully received (connection reusable for keepalive).
async fn proxy_http_request(
    client: &mut Connection,
    buf: &mut [u8],
    request_bytes: usize,
    backend_ref: &crate::routing::Backend,
    rule: &HttpRouteRule,
    meta: &RequestMeta,
    state: &mut ConnectionState,
) -> Result<bool> {
    let backend_addr = backend_ref.socket_addr;
    let timeout_dur = rule
        .backend_request_timeout
        .filter(|d| !d.is_zero())
        .unwrap_or(std::time::Duration::from_secs(30));

    // --- Connect to backend ---
    let mut backend = match tokio::time::timeout(
        timeout_dur,
        state
            .pool
            .acquire(backend_addr, backend_ref.use_tls, &backend_ref.server_name),
    )
    .await
    {
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

    // --- Send request headers to backend (with optional modifications) ---
    let header_end = find_header_end(&buf[..request_bytes]).unwrap_or(request_bytes);
    let has_filters = rule.has_filters || !backend_ref.filters.is_empty();
    let has_mirrors = has_filters
        && rule
            .filters
            .iter()
            .any(|f| matches!(f, HttpFilter::RequestMirror { .. }));
    let mut mirror_header_data: Option<Vec<u8>> = None;

    if has_filters {
        // Extract request header mods from rule + backend filter lists (zero-alloc borrows)
        let rule_mods = extract_header_mods(&rule.filters, false);
        let request_path = extract_request_path(&buf[..header_end]);
        let url_rewrite =
            extract_url_rewrite(&rule.filters, &rule.path, request_path.unwrap_or("/"));

        let has_mods = rule_mods.is_some()
            || url_rewrite.is_some()
            || extract_header_mods(&backend_ref.filters, false).is_some();

        if has_mods {
            // Apply rule-level modifications
            let modified_headers = apply_request_header_modifications(
                &buf[..header_end],
                rule_mods.as_ref(),
                url_rewrite.as_ref(),
            );

            // Apply backend-level modifications on top if present
            let final_headers =
                if let Some(backend_mods) = extract_header_mods(&backend_ref.filters, false) {
                    apply_request_header_modifications(&modified_headers, Some(&backend_mods), None)
                } else {
                    modified_headers
                };

            if has_mirrors {
                mirror_header_data = Some(final_headers.clone());
            }
            backend.write_all(&final_headers).await?;
            let body = &buf[header_end..request_bytes];
            if !body.is_empty() {
                backend.write_all(body).await?;
            }
        } else {
            if has_mirrors {
                mirror_header_data = Some(buf[..header_end].to_vec());
            }
            backend.write_all(&buf[..request_bytes]).await?;
        }
    } else {
        backend.write_all(&buf[..request_bytes]).await?;
    }

    // --- Relay remaining request body + optional mirror tee ---
    let mirror_body = relay_request_body(
        client,
        &mut backend,
        buf,
        header_end,
        request_bytes,
        meta.content_length,
        meta.is_chunked,
        has_mirrors,
    )
    .await?;

    // --- Dispatch mirrors with complete request data ---
    if let Some(ref body) = mirror_body {
        let header_bytes = mirror_header_data.as_deref().unwrap_or(&buf[..header_end]);
        let mut full_request = Vec::with_capacity(header_bytes.len() + body.len());
        full_request.extend_from_slice(header_bytes);
        full_request.extend_from_slice(body);
        dispatch_mirrors(&rule.filters, &full_request);
    }

    // --- WebSocket / protocol upgrade handling ---
    if meta.is_upgrade {
        let resp_mods = extract_response_mods(rule, backend_ref);
        let _response_complete = match tokio::time::timeout(
            timeout_dur,
            forward_http_response(
                &mut backend,
                client,
                buf,
                resp_mods.as_ref(),
                &mut state.header_buf,
            ),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                send_error_response(client, 504).await?;
                return Ok(false);
            }
        };
        client.flush().await?;
        let _ = tokio::io::copy_bidirectional(client, &mut backend).await;
        return Ok(false);
    }

    // --- Forward response ---
    let resp_mods = extract_response_mods(rule, backend_ref);
    let response_complete = match tokio::time::timeout(
        timeout_dur,
        forward_http_response(
            &mut backend,
            client,
            buf,
            resp_mods.as_ref(),
            &mut state.header_buf,
        ),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            send_error_response(client, 504).await?;
            return Ok(false);
        }
    };
    client.flush().await?;

    if response_complete {
        state.pool.release(backend_addr, backend);
    }

    Ok(response_complete)
}

/// Extract response header modifications from rule + backend filters (zero-alloc borrows).
fn extract_response_mods<'a>(
    rule: &'a HttpRouteRule,
    backend: &'a crate::routing::Backend,
) -> Option<HeaderModifications<'a>> {
    extract_header_mods(&rule.filters, true).or_else(|| extract_header_mods(&backend.filters, true))
}

/// Extract the request path from the first line of raw HTTP headers.
/// Zero-allocation: borrows directly from the buffer.
fn extract_request_path(header_bytes: &[u8]) -> Option<&str> {
    let first_line_end = header_bytes.iter().position(|&b| b == b'\r')?;
    let first_line = std::str::from_utf8(&header_bytes[..first_line_end]).ok()?;
    let path = first_line.split_whitespace().nth(1)?;
    Some(path.split('?').next().unwrap_or(path))
}

/// Relay the request body from client to backend, optionally teeing into a mirror buffer.
/// Returns `Some(body_bytes)` if mirrors are active and body fits within MIRROR_BODY_MAX,
/// `None` if no mirrors or body exceeded the cap.
async fn relay_request_body(
    client: &mut Connection,
    backend: &mut Connection,
    buf: &mut [u8],
    header_end: usize,
    request_bytes: usize,
    content_length: Option<usize>,
    is_chunked: bool,
    has_mirrors: bool,
) -> Result<Option<Vec<u8>>> {
    let body_in_initial = request_bytes.saturating_sub(header_end);
    let initial_body = &buf[header_end..request_bytes];

    // Mirror body buffer: only allocated when mirrors need body data.
    let mut mirror_body: Option<Vec<u8>> = if has_mirrors {
        let cap = body_in_initial.min(MIRROR_BODY_MAX);
        let mut mb = Vec::with_capacity(cap);
        mb.extend_from_slice(&initial_body[..cap]);
        Some(mb)
    } else {
        None
    };
    let mut overflow = has_mirrors && body_in_initial > MIRROR_BODY_MAX;

    if let Some(cl) = content_length {
        let mut remaining = cl.saturating_sub(body_in_initial);
        while remaining > 0 {
            let n = client.read(&mut buf[..]).await?;
            if n == 0 {
                break;
            }
            let to_send = n.min(remaining);
            backend.write_all(&buf[..to_send]).await?;
            tee_mirror_chunk(&mut mirror_body, &mut overflow, &buf[..to_send]);
            remaining -= to_send;
        }
    } else if is_chunked {
        let mut chunked_tail = [0u8; 5];
        let mut chunked_tail_len: usize = 0;
        if body_in_initial > 0 {
            append_chunked_tail(
                &mut chunked_tail,
                &mut chunked_tail_len,
                &buf[header_end..request_bytes],
            );
        }
        while !is_chunked_terminator(&chunked_tail, chunked_tail_len) {
            let n = client.read(&mut buf[..]).await?;
            if n == 0 {
                break;
            }
            backend.write_all(&buf[..n]).await?;
            append_chunked_tail(&mut chunked_tail, &mut chunked_tail_len, &buf[..n]);
            tee_mirror_chunk(&mut mirror_body, &mut overflow, &buf[..n]);
        }
    }

    if overflow {
        Ok(None)
    } else {
        Ok(mirror_body)
    }
}

/// Tee a chunk into the mirror body buffer, tracking overflow.
#[inline]
fn tee_mirror_chunk(mirror_body: &mut Option<Vec<u8>>, overflow: &mut bool, chunk: &[u8]) {
    if let Some(ref mut mb) = mirror_body {
        if !*overflow {
            let space = MIRROR_BODY_MAX.saturating_sub(mb.len());
            if chunk.len() <= space {
                mb.extend_from_slice(chunk);
            } else {
                *overflow = true;
                warn!(
                    "mirror body exceeds {}B cap, skipping mirror for this request",
                    MIRROR_BODY_MAX
                );
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
    response_mods: Option<&HeaderModifications<'_>>,
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
        Err(_) => {
            return Err(anyhow::anyhow!(
                "TLS passthrough: client sent no data within 5s"
            ))
        }
    };
    if n == 0 {
        return Ok(());
    }

    let sni = tls::extract_sni(&peek_buf[..n]);
    let hostname = match sni.as_deref() {
        Some(h) => h,
        None => {
            return Err(anyhow::anyhow!(
                "TLS passthrough: no SNI in ClientHello, cannot route"
            ))
        }
    };

    let route_table = routes.load();
    let backend_addr = route_table
        .resolve_tls_passthrough(hostname, server_port)
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
            return Err(anyhow::anyhow!(
                "TLS passthrough connect to {} failed: {}",
                backend_addr,
                e
            ));
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
    let connect_result =
        tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(backend_addr)).await;

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
            return Err(anyhow::anyhow!(
                "TCP connect to {} failed: {}",
                backend_addr,
                e
            ));
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

async fn send_redirect_response(
    client: &mut Connection,
    status_code: u16,
    location: &str,
) -> Result<()> {
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
        status_code,
        status_text,
        location,
        // body length: digits of status_code (always 3) + 1 space + status_text length
        3 + 1 + status_text.len(),
        status_code,
        status_text,
    );

    client.write_all(&resp).await?;
    Ok(())
}

async fn send_error_response(client: &mut Connection, error_code: u16) -> Result<()> {
    static RESP_400: &[u8] = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 15\r\nConnection: close\r\n\r\n400 Bad Request";
    static RESP_404: &[u8] =
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 13\r\nConnection: close\r\n\r\n404 Not Found";
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
    crate::routing::find_header_value(headers, "content-length")?
        .parse()
        .ok()
}

fn is_chunked_transfer(headers: &[u8]) -> bool {
    crate::routing::find_header_value(headers, "transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
}

fn is_no_body_status(headers: &[u8]) -> bool {
    if headers.len() < 12 {
        return false;
    }
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
