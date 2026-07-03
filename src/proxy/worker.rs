//! Tokio async TCP worker — one task per accepted connection.
//!
//! HTTP request/response proxying with keepalive and backend connection pooling,
//! raw TCP bidirectional forwarding, TLS termination, and TLS passthrough.

use anyhow::Result;
use arc_swap::ArcSwap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use crate::logging::{debug, info, warn};
use crate::metrics::METRICS;
use crate::proxy::backend_pool::{BackendPool, PoolHandle, SharedBackendPool};
use crate::proxy::chunked::ChunkedStream;
use crate::proxy::health::HealthRegistry;
use crate::proxy::http_filters::{
    apply_request_header_modifications, apply_response_header_mods, dispatch_mirrors,
    extract_header_mods, extract_url_rewrite, HeaderModifications, MIRROR_BODY_MAX,
};
use crate::proxy::http_parser::find_header_end;
use crate::proxy::request_processor::{
    analyze_request, is_http_request, RequestMeta, RoutingResult,
};
use crate::proxy::tls::{self, Connection, DynamicTlsAcceptor};
use crate::routing::{BackendSelector, HttpFilter, HttpRouteRule, RouteTable};

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

/// Per-listener worker tunables, threaded from `PerformanceConfig` by the
/// data plane.
#[derive(Clone, Copy)]
pub struct WorkerConfig {
    /// Idle backend connections retained per backend in each connection's pool.
    pub max_idle_per_backend: usize,
    /// Default backend connect budget (rule-level timeouts override per request).
    pub connect_timeout: Duration,
    /// Client TLS-handshake / request-header-completion budget (slow-loris
    /// guard). Does not bound the idle wait between keepalive requests.
    pub client_header_timeout: Duration,
}

/// RAII in-flight-connection counter. One guard lives inside each spawned
/// per-connection task; `DataPlane::shutdown` polls the shared count and
/// drains until it reaches zero (or the drain timeout expires). Drop-based so
/// the count stays correct on every exit path, including panics and aborts.
struct ConnGuard(Arc<AtomicUsize>);

impl ConnGuard {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        METRICS.connections_accepted_total.inc();
        METRICS.active_connections.inc();
        Self(counter)
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        // Release pairs with the Acquire load in the shutdown drain loop.
        self.0.fetch_sub(1, Ordering::Release);
        METRICS.active_connections.dec();
    }
}

struct ConnectionState {
    server_port: u16,
    routes: Arc<ArcSwap<RouteTable>>,
    pool: PoolHandle,
    selector: Arc<BackendSelector>,
    health: Arc<HealthRegistry>,
    /// Whether this connection was accepted via TLS (used for redirect scheme).
    is_tls: bool,
    /// Budget for completing a request's header block once bytes arrive.
    client_header_timeout: Duration,
    /// Backend connect budget for raw TCP forwarding (HTTP connects go
    /// through the pool, which carries its own copy).
    connect_timeout: Duration,
    /// Reused across keepalive responses to avoid per-response heap allocation.
    header_buf: Vec<u8>,
}

/// Run an accept loop on a single listener, spawning a task per connection.
pub async fn run_worker(
    listener: TcpListener,
    server_port: u16,
    routes: Arc<ArcSwap<RouteTable>>,
    cfg: WorkerConfig,
    shutdown: CancellationToken,
    tls_acceptor: Option<Arc<DynamicTlsAcceptor>>,
    tls_passthrough: bool,
    health: Arc<HealthRegistry>,
    selector: Arc<BackendSelector>,
    active_connections: Arc<AtomicUsize>,
    shared_pool: Option<Arc<SharedBackendPool>>,
) {
    // Selector is shared across all listeners so weighted round-robin counters
    // produce the correct distribution globally.

    // Per-connection pool handle: the process-wide shared pool when
    // `performance.backendPoolScope: process` is set, else a fresh
    // per-connection pool (the default).
    let make_pool = move |shared: &Option<Arc<SharedBackendPool>>| match shared {
        Some(p) => PoolHandle::Shared(p.clone()),
        None => PoolHandle::PerConn(BackendPool::new(
            cfg.max_idle_per_backend,
            cfg.connect_timeout,
        )),
    };

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!("Listener on :{} shutting down", server_port);
                break;
            }
            result = listener.accept() => {
                match result {
                    Ok((tcp_stream, peer)) => {
                        let routes = routes.clone();
                        let acceptor = tls_acceptor.clone();
                        let health = health.clone();
                        let conn_guard = ConnGuard::new(active_connections.clone());

                        if tls_passthrough && acceptor.is_none() {
                            // Pure passthrough mode (no TLS termination cert available)
                            tokio::spawn(async move {
                                let _guard = conn_guard;
                                let _ = handle_tls_passthrough(tcp_stream, server_port, routes, health, cfg.client_header_timeout, cfg.connect_timeout).await;
                            });
                        } else if let Some(acceptor) = acceptor {
                            // TLS termination mode — but first check if this SNI
                            // should be passed through instead of terminated.
                            // This handles the case where both HTTPS/Terminate and
                            // TLS/Passthrough listeners share the same port.
                            let selector = selector.clone();
                            let pool = make_pool(&shared_pool);
                            tokio::spawn(async move {
                                let _guard = conn_guard;
                                // Peek ClientHello for SNI to decide dispatch mode
                                let should_passthrough = {
                                    let mut peek_buf = [0u8; 16384];
                                    match tokio::time::timeout(
                                        cfg.client_header_timeout,
                                        tcp_stream.peek(&mut peek_buf),
                                    ).await {
                                        Ok(Ok(n)) if n > 0 => {
                                            let sni = crate::proxy::tls::extract_sni(&peek_buf[..n]);
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
                                    let _ = handle_tls_passthrough(tcp_stream, server_port, routes, health, cfg.client_header_timeout, cfg.connect_timeout).await;
                                } else {
                                    // Bound the handshake: a client that stalls
                                    // mid-handshake would otherwise pin this task
                                    // (and its buffers) forever.
                                    match tokio::time::timeout(
                                        cfg.client_header_timeout,
                                        acceptor.acceptor().accept(tcp_stream),
                                    )
                                    .await
                                    {
                                        Ok(Ok(tls_stream)) => {
                                            let conn = Connection::Tls { inner: tls_stream };
                                            let state = ConnectionState {
                                                server_port,
                                                routes,
                                                pool,
                                                selector,
                                                health,
                                                is_tls: true,
                                                client_header_timeout: cfg.client_header_timeout,
                                                connect_timeout: cfg.connect_timeout,
                                                header_buf: Vec::with_capacity(1024),
                                            };
                                            let _ = handle_connection(conn, peer, state).await;
                                        }
                                        Ok(Err(_e)) => {
                                            METRICS.tls_handshake_failures_total.inc();
                                            debug!("TLS handshake failed from {}: {}", peer, _e);
                                        }
                                        Err(_) => {
                                            METRICS.tls_handshake_failures_total.inc();
                                            debug!("TLS handshake from {} timed out", peer);
                                        }
                                    }
                                }
                            });
                        } else {
                            let selector = selector.clone();
                            let state = ConnectionState {
                                server_port,
                                routes,
                                pool: make_pool(&shared_pool),
                                selector,
                                health,
                                is_tls: false,
                                client_header_timeout: cfg.client_header_timeout,
                                connect_timeout: cfg.connect_timeout,
                                header_buf: Vec::with_capacity(1024),
                            };
                            tokio::spawn(async move {
                                let _guard = conn_guard;
                                let conn = Connection::Plain { inner: tcp_stream };
                                let _ = handle_connection(conn, peer, state).await;
                            });
                        }
                    }
                    Err(e) => {
                        warn!("Listener on :{} accept error: {}", server_port, e);
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
        n = complete_headers(&mut client, &mut buf, n, state.client_header_timeout).await?;
    }

    // Bytes read past a request's body (a pipelined next request) are carried
    // over here and replayed as the next iteration's input instead of being
    // sent to the backend or lost.
    let mut pending: Option<Vec<u8>> = None;

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
                    state.connect_timeout,
                )
                .await;
            }
            RoutingResult::HttpForward {
                backend,
                rule,
                meta,
            } => {
                METRICS.http_requests_total.inc();
                // proxy_http_request owns all timing for the request lifecycle —
                // backend_request_timeout (connect + TTFB) and request_timeout
                // (total deadline) are applied phase-aware inside.
                let (ka, leftover) =
                    proxy_http_request(&mut client, &mut buf, n, backend, rule, &meta, &mut state)
                        .await?;
                if !ka || !meta.keepalive {
                    return Ok(());
                }
                pending = leftover;
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

        if let Some(bytes) = pending.take() {
            // Replay pipelined bytes as the start of the next request. They
            // came out of `buf` via a single read, so they always fit.
            buf[..bytes.len()].copy_from_slice(&bytes);
            n = bytes.len();
        } else {
            n = client.read(&mut buf).await?;
        }
        if n > 0 && find_header_end(&buf[..n]).is_none() {
            n = complete_headers(&mut client, &mut buf, n, state.client_header_timeout).await?;
        }
        if n == 0 {
            return Ok(());
        }
    }
}

/// Complete a partially-read request header block within the slow-loris
/// budget. The budget starts once a request's first bytes have arrived — the
/// idle wait between keepalive requests is intentionally unbounded. Sends
/// `431` when the block outgrows the buffer and `408` when the client is too
/// slow, then errors out to close the connection.
async fn complete_headers(
    client: &mut Connection,
    buf: &mut [u8],
    already_read: usize,
    budget: Duration,
) -> Result<usize> {
    match tokio::time::timeout(budget, read_remaining_headers(client, buf, already_read)).await {
        Ok(Ok(total)) => Ok(total),
        Ok(Err(e)) => {
            let _ = send_error_response(client, 431).await;
            Err(e)
        }
        Err(_) => {
            let _ = send_error_response(client, 408).await;
            Err(anyhow::anyhow!("client header read timed out"))
        }
    }
}

/// Proxy a single HTTP request to a backend and forward the response.
/// Returns `(reusable, leftover)`: `reusable` is true if the response was
/// fully received (connection reusable for keepalive); `leftover` holds any
/// bytes read past this request's body — a pipelined next request — which the
/// keepalive loop replays instead of reading from the socket again.
///
/// Owns all timing for the request lifecycle. Two timeouts come in from the rule:
///
/// - `backend_request_timeout` — single shared budget covering pool acquire,
///   TCP/TLS connect, request send, and response time-to-first-byte. On expiry
///   we are still in the SAFE zone (no client bytes written), so a `504` is
///   well-formed.
/// - `request_timeout` — total request lifetime, applied as an absolute
///   `Instant` deadline. Clamps the SAFE-zone phases too, then bounds the body
///   stream. On body-phase expiry we are in the UNSAFE zone (response headers
///   already on the wire); we emit an encoding-aware terminator instead of a
///   bogus `504` that would corrupt framing.
///
/// WebSocket upgrades skip the total deadline (long-lived stream).
async fn proxy_http_request(
    client: &mut Connection,
    buf: &mut [u8],
    request_bytes: usize,
    backend_ref: &crate::routing::Backend,
    rule: &HttpRouteRule,
    meta: &RequestMeta,
    state: &mut ConnectionState,
) -> Result<(bool, Option<Vec<u8>>)> {
    use std::time::Instant;

    let backend_addr = backend_ref.socket_addr;
    let backend_budget = rule
        .backend_request_timeout
        .filter(|d| !d.is_zero())
        .unwrap_or(Duration::from_secs(30));
    let total_deadline = if meta.is_upgrade {
        None
    } else {
        rule.request_timeout
            .filter(|d| !d.is_zero())
            .map(|d| Instant::now() + d)
    };
    let backend_phase_start = Instant::now();

    // Connect (SAFE: no client bytes yet, helper sends 502/504 on failure).
    let (mut backend, from_pool) =
        match connect_to_backend(client, state, backend_ref, backend_budget, total_deadline).await?
        {
            Some(b) => b,
            None => return Ok((false, None)),
        };

    let req_header_end = find_header_end(&buf[..request_bytes]).unwrap_or(request_bytes);

    // Determine where THIS request ends within the bytes already read. Bytes
    // past that boundary are a pipelined next request: they must never reach
    // the backend as body (a desync/smuggling vector — the backend would
    // parse them as a second request and answer it, poisoning the pooled
    // connection), and are instead handed back to the keepalive loop.
    // Upgrade requests are exempt: after the 101 the connection is a raw
    // tunnel and the tail belongs to it.
    let tail_len = request_bytes - req_header_end;
    let (request_end, req_chunked) = if meta.is_upgrade {
        (request_bytes, None)
    } else if let Some(cl) = meta.content_length {
        (req_header_end + tail_len.min(cl), None)
    } else if meta.is_chunked {
        let mut stream = ChunkedStream::new();
        let consumed = stream.observe(&buf[req_header_end..request_bytes]);
        (req_header_end + consumed, Some(stream))
    } else {
        // No declared body: the entire tail is the next pipelined request.
        (req_header_end, None)
    };
    // Capture the pipelined tail now — the response phase reuses `buf` as
    // scratch for backend reads and would overwrite it.
    let mut leftover: Option<Vec<u8>> = if request_end < request_bytes {
        Some(buf[request_end..request_bytes].to_vec())
    } else {
        None
    };

    // Send request to backend, optionally applying filter modifications.
    //
    // Retry contract: if the conn came from the pool AND the send errors, we
    // retry once on a fresh conn. SAFE — `relay_request_body` hasn't been
    // called, so no client body bytes have been consumed yet, and no client
    // write has happened either. `buf[..request_end]` still holds exactly
    // what arrived; the original send's bytes went into a dead pipe, so no
    // backend observed them. The retry is unconditionally safe regardless of
    // HTTP method.
    let has_mirrors = (rule.has_filters || !backend_ref.filters.is_empty())
        && rule
            .filters
            .iter()
            .any(|f| matches!(f, HttpFilter::RequestMirror { .. }));
    let mirror_header_data = match send_request_to_backend(
        &mut backend,
        buf,
        req_header_end,
        request_end,
        rule,
        backend_ref,
        has_mirrors,
    )
    .await
    {
        Ok(m) => m,
        Err(_e) if from_pool => {
            // Pool entry died between probe and write. Reconnect once and
            // replay. Do NOT record_failure here: a stale pool entry is
            // expected churn, not a backend problem.
            METRICS.pool_stale_retries_total.inc();
            debug!(
                "Backend {} pool conn died on send ({}); retrying with fresh conn",
                backend_addr, _e
            );
            drop(backend);
            let fresh_budget = clamp_to_deadline(
                backend_budget.saturating_sub(backend_phase_start.elapsed()),
                total_deadline,
            );
            backend = match tokio::time::timeout(
                fresh_budget,
                state.pool.acquire_fresh(
                    backend_addr,
                    backend_ref.use_tls,
                    &backend_ref.server_name,
                ),
            )
            .await
            {
                Ok(Ok(c)) => {
                    state.health.record_success(backend_addr);
                    c
                }
                Ok(Err(e2)) => {
                    warn!(
                        "Backend {} fresh-conn acquire failed after pool retry: {}",
                        backend_addr, e2
                    );
                    return fail_backend(&state.health, client, backend_addr, 502).await;
                }
                Err(_) => {
                    warn!(
                        "Backend {} fresh-conn acquire timeout after pool retry",
                        backend_addr
                    );
                    return fail_backend(&state.health, client, backend_addr, 504).await;
                }
            };
            match send_request_to_backend(
                &mut backend,
                buf,
                req_header_end,
                request_end,
                rule,
                backend_ref,
                has_mirrors,
            )
            .await
            {
                Ok(m) => m,
                Err(e2) => {
                    warn!(
                        "Backend {} send failed on fresh conn after pool retry: {}",
                        backend_addr, e2
                    );
                    return fail_backend(&state.health, client, backend_addr, 502).await;
                }
            }
        }
        Err(e) => return Err(e),
    };

    // Relay remaining request body + optional mirror tee, clamped to the
    // rule's total deadline — `request_timeout` covers the request-body phase
    // too, so a slow client body cannot hold the backend conn open past it.
    // Expiry is SAFE for the client wire (no response bytes yet) → 408.
    // Any bytes read past the body end (pipelined next request) come back as
    // leftover; mutually exclusive with the initial-read leftover above.
    let relay_fut = relay_request_body(
        client,
        &mut backend,
        buf,
        req_header_end,
        request_end,
        meta.content_length,
        req_chunked,
        has_mirrors,
    );
    let (mirror_body, relay_leftover) = match total_deadline {
        Some(deadline) => {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, relay_fut).await {
                Ok(r) => r?,
                Err(_) => {
                    warn!(
                        "Request body relay to backend {} exceeded request timeout",
                        backend_addr
                    );
                    send_error_response(client, 408).await?;
                    return Ok((false, None));
                }
            }
        }
        None => relay_fut.await?,
    };
    if relay_leftover.is_some() {
        leftover = relay_leftover;
    }

    if let Some(ref body) = mirror_body {
        let header_bytes = mirror_header_data
            .as_deref()
            .unwrap_or(&buf[..req_header_end]);
        let mut full_request = Vec::with_capacity(header_bytes.len() + body.len());
        full_request.extend_from_slice(header_bytes);
        full_request.extend_from_slice(body);
        dispatch_mirrors(&rule.filters, &full_request);
    }

    // Forward response headers (SAFE→UNSAFE transition lives inside this call;
    // on timeout the helper emits a 504 — no client bytes written yet).
    let header_budget = clamp_to_deadline(
        backend_budget.saturating_sub(backend_phase_start.elapsed()),
        total_deadline,
    );
    let resp_mods = extract_response_mods(rule, backend_ref);
    let mut framing = match forward_response_headers(
        client,
        &mut backend,
        buf,
        &mut state.header_buf,
        resp_mods.as_ref(),
        header_budget,
        meta.is_head,
    )
    .await?
    {
        Some(f) => f,
        None => return Ok((false, None)),
    };

    // WebSocket / protocol upgrade: switch to raw bidi after headers.
    if meta.is_upgrade {
        client.flush().await?;
        let _ = tokio::io::copy_bidirectional(client, &mut backend).await;
        return Ok((false, None));
    }

    // Stream response body (UNSAFE: headers on wire; expiry → encoding-aware
    // termination, never a synthetic 504 that would corrupt framing).
    let body_result = match total_deadline {
        Some(deadline) => {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(
                remaining,
                forward_response_body(&mut backend, client, buf, &mut framing),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => {
                    terminate_stream_gracefully(client, &framing).await;
                    return Ok((false, None));
                }
            }
        }
        None => forward_response_body(&mut backend, client, buf, &mut framing).await,
    };

    let response_complete = match body_result {
        Ok(c) => c,
        Err(_) => {
            terminate_stream_gracefully(client, &framing).await;
            return Ok((false, None));
        }
    };

    client.flush().await?;

    // Pool only if framing terminated cleanly. The chunked observer signals
    // termination at a true chunk boundary; content-length matches are exact.
    // A Broken state means stale bytes could remain on the wire — don't reuse.
    // Overshoot (bytes past the framing end) likewise poisons the conn: the
    // excess was dropped here but whatever else follows would desync the next
    // response.
    if framing.overshoot {
        warn!(
            "Backend {} sent bytes past the response framing end; excess dropped, not pooling connection",
            backend_addr
        );
    }
    let framing_clean =
        framing.chunked.as_ref().is_none_or(|s| !s.is_broken()) && !framing.overshoot;
    if !framing_clean && !framing.overshoot {
        warn!(
            "Backend {} response framing broken; not pooling connection",
            backend_addr
        );
    }
    let reusable = response_complete && framing_clean;

    // The client got a clean response, but the backend conn returns to the pool
    // only when `backend_conn_poolable` allows — notably never after HEAD.
    if backend_conn_poolable(response_complete, framing_clean, meta.is_head) {
        state.pool.release(
            backend_addr,
            backend_ref.use_tls,
            &backend_ref.server_name,
            backend,
        );
    } else if reusable {
        debug!(
            "Backend {} conn not pooled after HEAD (avoids body-desync on reuse)",
            backend_addr
        );
    }

    Ok((reusable, leftover))
}

/// Whether a backend connection may be returned to the idle pool after a response.
///
/// Requires a fully-received response with clean framing, and — additionally —
/// **never after a HEAD request**. A non-compliant backend may send a response
/// body (RFC 7230 §3.3 forbids it) that we cannot reliably detect at header time
/// without blocking: a *compliant* HEAD response legitimately carries
/// `Content-Length` with no body, so the declared framing is no signal. Reusing
/// such a conn would bleed leftover body bytes into the next request's response;
/// HEAD is rare and bodyless, so dropping the conn instead costs almost nothing.
fn backend_conn_poolable(response_complete: bool, framing_clean: bool, is_head: bool) -> bool {
    response_complete && framing_clean && !is_head
}

/// Record a backend failure (spawning a health probe if this newly trips the
/// backend unhealthy) and send `code` to the client. Returns `Ok((false, None))`
/// — the client connection is not reusable. Centralizes the failure boilerplate
/// the backend-send retry path would otherwise repeat per error arm.
async fn fail_backend(
    health: &Arc<HealthRegistry>,
    client: &mut Connection,
    addr: std::net::SocketAddr,
    code: u16,
) -> Result<(bool, Option<Vec<u8>>)> {
    match code {
        504 => METRICS.upstream_connect_timeouts_total.inc(),
        _ => METRICS.upstream_connect_errors_total.inc(),
    }
    if health.record_failure(addr) {
        HealthRegistry::spawn_probe(Arc::clone(health), addr);
    }
    send_error_response(client, code).await?;
    Ok((false, None))
}

/// Acquire a backend connection within `backend_budget` (clamped by `total_deadline`).
/// On any failure or timeout, records health failure and sends `502`/`504` to the
/// client. Returns `Ok(None)` when the caller should bail out (`return Ok(false)`),
/// which signals the connection is not reusable.
///
/// `Ok(Some((conn, from_pool)))`: `from_pool == true` means the conn came from
/// the idle pool and the caller may safely retry once with `acquire_fresh` if a
/// subsequent SAFE-zone write to it fails (the probe is best-effort).
async fn connect_to_backend(
    client: &mut Connection,
    state: &mut ConnectionState,
    backend_ref: &crate::routing::Backend,
    backend_budget: Duration,
    total_deadline: Option<std::time::Instant>,
) -> Result<Option<(Connection, bool)>> {
    let backend_addr = backend_ref.socket_addr;
    let connect_budget = clamp_to_deadline(backend_budget, total_deadline);
    match tokio::time::timeout(
        connect_budget,
        state
            .pool
            .acquire(backend_addr, backend_ref.use_tls, &backend_ref.server_name),
    )
    .await
    {
        Ok(Ok(result)) => {
            // Reset passive-failure state: without this, transient failures
            // accumulate over the process lifetime and eventually trip the
            // "consecutive"-failure threshold (the L4 paths already record
            // success on connect).
            state.health.record_success(backend_addr);
            Ok(Some(result))
        }
        Ok(Err(e)) => {
            METRICS.upstream_connect_errors_total.inc();
            warn!("Backend {} connect failed: {}", backend_addr, e);
            if state.health.record_failure(backend_addr) {
                HealthRegistry::spawn_probe(Arc::clone(&state.health), backend_addr);
            }
            send_error_response(client, 502).await?;
            Ok(None)
        }
        Err(_) => {
            METRICS.upstream_connect_timeouts_total.inc();
            warn!("Backend {} connect timeout", backend_addr);
            if state.health.record_failure(backend_addr) {
                HealthRegistry::spawn_probe(Arc::clone(&state.health), backend_addr);
            }
            send_error_response(client, 504).await?;
            Ok(None)
        }
    }
}

/// Send request headers (with optional filter modifications) and the same-read
/// body tail (up to `request_end`, the request's framing boundary — bytes
/// beyond it belong to a pipelined next request and never reach the backend)
/// to the backend. Returns the buffer captured for mirroring, if any.
///
/// Fast path (no filters/mods): one `write_all` covering headers + tail.
async fn send_request_to_backend(
    backend: &mut Connection,
    buf: &[u8],
    req_header_end: usize,
    request_end: usize,
    rule: &HttpRouteRule,
    backend_ref: &crate::routing::Backend,
    has_mirrors: bool,
) -> Result<Option<Vec<u8>>> {
    let has_filters = rule.has_filters || !backend_ref.filters.is_empty();
    if !has_filters {
        backend.write_all(&buf[..request_end]).await?;
        return Ok(None);
    }

    let rule_mods = extract_header_mods(&rule.filters, false);
    let request_path = extract_request_path(&buf[..req_header_end]);
    let url_rewrite = extract_url_rewrite(&rule.filters, &rule.path, request_path.unwrap_or("/"));
    let backend_mods = extract_header_mods(&backend_ref.filters, false);
    let has_mods = rule_mods.is_some() || url_rewrite.is_some() || backend_mods.is_some();

    if !has_mods {
        let mirror = has_mirrors.then(|| buf[..req_header_end].to_vec());
        backend.write_all(&buf[..request_end]).await?;
        return Ok(mirror);
    }

    let modified_headers = apply_request_header_modifications(
        &buf[..req_header_end],
        rule_mods.as_ref(),
        url_rewrite.as_ref(),
    );
    let final_headers = if let Some(bm) = backend_mods {
        apply_request_header_modifications(&modified_headers, Some(&bm), None)
    } else {
        modified_headers
    };
    let mirror = has_mirrors.then(|| final_headers.clone());
    backend.write_all(&final_headers).await?;
    let body = &buf[req_header_end..request_end];
    if !body.is_empty() {
        backend.write_all(body).await?;
    }
    Ok(mirror)
}

/// Read response headers from the backend within `header_budget` and forward
/// them to the client. This is the SAFE→UNSAFE transition: on entry no client
/// bytes have been written and timeout → `504`; on success the client has the
/// headers and the caller must use encoding-aware termination on later expiry.
///
/// Returns `Ok(None)` when the caller should bail out (`return Ok(false)`).
async fn forward_response_headers(
    client: &mut Connection,
    backend: &mut Connection,
    buf: &mut [u8],
    header_buf: &mut Vec<u8>,
    resp_mods: Option<&HeaderModifications<'_>>,
    header_budget: Duration,
    is_head_request: bool,
) -> Result<Option<ResponseFraming>> {
    let (resp_header_end, framing) = match tokio::time::timeout(
        header_budget,
        read_response_headers(backend, buf, header_buf, is_head_request),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            send_error_response(client, 504).await?;
            return Ok(None);
        }
    };

    // SAFE → UNSAFE: first client write happens here.
    //
    // Only tail bytes within the declared framing are forwarded
    // (`body_bytes_already_forwarded`): for HEAD and no-body statuses that is
    // zero — body bytes a non-compliant backend sent are dropped rather than
    // leaking to the client — and for CL/chunked responses it stops at the
    // framing boundary so backend overshoot never reaches the client.
    let tail_end = resp_header_end + framing.body_bytes_already_forwarded;
    if let Some(mods) = resp_mods {
        let modified = apply_response_header_mods(&header_buf[..resp_header_end], mods);
        client.write_all(&modified).await?;
        if tail_end > resp_header_end {
            client
                .write_all(&header_buf[resp_header_end..tail_end])
                .await?;
        }
    } else {
        // Hot path: headers + in-framing tail in a single write.
        client.write_all(&header_buf[..tail_end]).await?;
    }
    Ok(Some(framing))
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
///
/// `chunked` is the observer pre-seeded with the initial-read body tail (built
/// by the caller when the request is chunk-framed).
///
/// Returns `(mirror_body, leftover)`: `mirror_body` is `Some(bytes)` if
/// mirrors are active and the body fit within `MIRROR_BODY_MAX`; `leftover`
/// holds any bytes read past the body's framing end — a pipelined next
/// request — which must go back to the keepalive loop, never to the backend.
async fn relay_request_body(
    client: &mut Connection,
    backend: &mut Connection,
    buf: &mut [u8],
    header_end: usize,
    request_end: usize,
    content_length: Option<usize>,
    chunked: Option<ChunkedStream>,
    has_mirrors: bool,
) -> Result<(Option<Vec<u8>>, Option<Vec<u8>>)> {
    let body_in_initial = request_end.saturating_sub(header_end);
    let initial_body = &buf[header_end..request_end];

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
    let mut leftover: Option<Vec<u8>> = None;

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
            if to_send < n {
                // The read ran past the body end: pipelined next request.
                leftover = Some(buf[to_send..n].to_vec());
            }
        }
    } else if let Some(mut stream) = chunked {
        // Broken framing exits too: there is no boundary left to find, and
        // the backend cannot parse the body either — the exchange is doomed
        // and the response phase will surface the failure.
        while !stream.is_terminated() && !stream.is_broken() {
            let n = client.read(&mut buf[..]).await?;
            if n == 0 {
                break;
            }
            let consumed = stream.observe(&buf[..n]);
            backend.write_all(&buf[..consumed]).await?;
            tee_mirror_chunk(&mut mirror_body, &mut overflow, &buf[..consumed]);
            if consumed < n {
                leftover = Some(buf[consumed..n].to_vec());
            }
        }
    }

    Ok((if overflow { None } else { mirror_body }, leftover))
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

/// Response framing derived from raw backend headers. Drives the body-phase loop:
/// when to stop reading from the backend, and how to terminate the client stream
/// gracefully on a mid-body timeout.
struct ResponseFraming {
    content_length: Option<usize>,
    no_body: bool,
    /// Body bytes (within the declared framing) that arrived in the same
    /// backend read that completed the response headers. The header-forwarding
    /// phase writes exactly this many tail bytes to the client, and it seeds
    /// the body-phase byte counter.
    body_bytes_already_forwarded: usize,
    /// `Some` iff the response uses `Transfer-Encoding: chunked`. Pre-seeded
    /// with any chunked body bytes that arrived in the same read as the
    /// response headers, so the observer is already at the correct framing
    /// position when `forward_response_body` starts looping.
    chunked: Option<ChunkedStream>,
    /// The backend sent bytes past the end of the declared framing
    /// (Content-Length reached, chunked terminator passed, or a body on a
    /// no-body status). The excess is dropped — never forwarded to the
    /// client — and the connection must not be pooled: whatever follows the
    /// boundary would desync the next response.
    overshoot: bool,
}

/// Cap of bytes accumulated while waiting for backend response headers. Protects
/// against a backend that never finishes its header block.
const RESPONSE_HEADER_CAP: usize = 65536;

/// Phase 1 — read response headers from the backend into `header_buf`. Performs
/// NO writes to the client; aborting/timeout/error before this returns leaves the
/// client wire untouched, so the caller can still emit a clean `504`.
/// On return, `header_buf` holds `[headers..header_end][same-read body tail...]`.
async fn read_response_headers(
    backend: &mut Connection,
    buf: &mut [u8],
    header_buf: &mut Vec<u8>,
    is_head_request: bool,
) -> Result<(usize, ResponseFraming)> {
    header_buf.clear();

    loop {
        let n = backend.read(buf).await?;
        if n == 0 {
            return Err(anyhow::anyhow!(
                "backend closed before sending response headers"
            ));
        }
        header_buf.extend_from_slice(&buf[..n]);

        if let Some(header_end) = find_header_end(header_buf) {
            let headers = &header_buf[..header_end];
            let content_length = parse_content_length(headers);
            let is_chunked = is_chunked_transfer(headers);
            // RFC 7230 §3.3: responses to HEAD have no message body regardless
            // of the response's Content-Length / Transfer-Encoding. Forcing
            // no_body here keeps `forward_response_body` from blocking on
            // bytes the backend correctly never sends.
            let no_body = is_head_request || is_no_body_status(headers);

            // Decide how many of the tail bytes (same read as the headers)
            // belong to THIS response's body. Anything beyond the declared
            // framing is overshoot: dropped, and the conn is not poolable.
            let tail_len = header_buf.len() - header_end;
            let (tail_forward, chunked, overshoot) = if is_head_request {
                // HEAD: any body bytes a non-compliant backend sent are
                // dropped (never forwarded per RFC 7230 §3.3); the conn is
                // already never pooled after HEAD.
                (0, None, false)
            } else if no_body {
                // 204/304/1xx: no body allowed; any tail is overshoot.
                (0, None, tail_len > 0)
            } else if is_chunked {
                let mut stream = ChunkedStream::new();
                let consumed = if tail_len > 0 {
                    stream.observe(&header_buf[header_end..])
                } else {
                    0
                };
                (consumed, Some(stream), consumed < tail_len)
            } else if let Some(cl) = content_length {
                (tail_len.min(cl), None, tail_len > cl)
            } else {
                // EOF-framed: everything until close is body.
                (tail_len, None, false)
            };

            return Ok((
                header_end,
                ResponseFraming {
                    content_length: if is_head_request {
                        None
                    } else {
                        content_length
                    },
                    no_body,
                    body_bytes_already_forwarded: tail_forward,
                    chunked,
                    overshoot,
                },
            ));
        }

        if header_buf.len() > RESPONSE_HEADER_CAP {
            return Err(anyhow::anyhow!("response headers exceed buffer size"));
        }
    }
}

/// Phase 2 — stream response body to the client. Hot path: read → write → check
/// framing terminator. Returns `Ok(true)` when the response framing completes
/// cleanly (connection reusable for keepalive); `Ok(false)` on backend EOF
/// without a terminator.
async fn forward_response_body(
    backend: &mut Connection,
    client: &mut Connection,
    buf: &mut [u8],
    framing: &mut ResponseFraming,
) -> Result<bool> {
    if framing.no_body {
        return Ok(true);
    }
    if let Some(cl) = framing.content_length {
        if framing.body_bytes_already_forwarded >= cl {
            return Ok(true);
        }
    }
    if let Some(stream) = &framing.chunked {
        if stream.is_terminated() {
            return Ok(true);
        }
    }

    let mut body_bytes_forwarded = framing.body_bytes_already_forwarded;
    loop {
        let n = backend.read(buf).await?;
        if n == 0 {
            return Ok(false);
        }

        // Forward only bytes within the declared framing; anything past the
        // boundary is backend overshoot — dropped, and flagged so the caller
        // never pools the connection.
        if let Some(stream) = &mut framing.chunked {
            let consumed = stream.observe(&buf[..n]);
            client.write_all(&buf[..consumed]).await?;
            if stream.is_terminated() {
                framing.overshoot |= consumed < n;
                return Ok(true);
            }
        } else if let Some(cl) = framing.content_length {
            let to_write = n.min(cl - body_bytes_forwarded);
            client.write_all(&buf[..to_write]).await?;
            body_bytes_forwarded += to_write;
            if body_bytes_forwarded >= cl {
                framing.overshoot |= to_write < n;
                return Ok(true);
            }
        } else {
            // EOF-framed: everything until close is body.
            client.write_all(&buf[..n]).await?;
        }
    }
}

/// Best-effort clean shutdown after response headers were already forwarded.
/// Called when an expiry or backend error strikes during the body phase — we
/// are past the point where injecting a new HTTP response would be well-formed.
///
/// For chunked responses we emit `0\r\n\r\n` so a compliant client treats the
/// transfer as complete (truncated body, no framing error). The proxy doesn't
/// track inter-chunk boundaries — only watches a 5-byte rolling tail — so if
/// the backend stalled mid-chunk-header the terminator is malformed, but no
/// worse than a bare TCP close.
///
/// For Content-Length responses we cannot synthesize the missing bytes; the
/// caller closes the connection and the client sees a well-defined premature
/// EOF rather than malformed framing.
async fn terminate_stream_gracefully(client: &mut Connection, framing: &ResponseFraming) {
    if framing.chunked.is_some() {
        let _ = client.write_all(b"0\r\n\r\n").await;
        let _ = client.flush().await;
    }
}

/// Clamp a phase budget by an optional absolute deadline.
/// Returns `min(budget, deadline - now)` if a deadline exists, else the budget.
#[inline]
fn clamp_to_deadline(budget: Duration, deadline: Option<std::time::Instant>) -> Duration {
    match deadline {
        Some(d) => budget.min(d.saturating_duration_since(std::time::Instant::now())),
        None => budget,
    }
}

/// L4 TLS passthrough: peek ClientHello for SNI, then forward raw TCP to backend.
async fn handle_tls_passthrough(
    mut client: TcpStream,
    server_port: u16,
    routes: Arc<ArcSwap<RouteTable>>,
    health: Arc<HealthRegistry>,
    peek_timeout: Duration,
    connect_timeout: Duration,
) -> Result<()> {
    let mut peek_buf = [0u8; 16384];
    let n = match tokio::time::timeout(peek_timeout, client.peek(&mut peek_buf)).await {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => {
            return Err(anyhow::anyhow!(
                "TLS passthrough: client sent no ClientHello within {:?}",
                peek_timeout
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

    match tokio::time::timeout(connect_timeout, TcpStream::connect(backend_addr)).await {
        Err(_) => {
            if health.record_failure(backend_addr) {
                HealthRegistry::spawn_probe(health, backend_addr);
            }
            return Err(anyhow::anyhow!(
                "TLS passthrough connect to {} timed out",
                backend_addr
            ));
        }
        Ok(Ok(mut backend)) => {
            health.record_success(backend_addr);
            backend.set_nodelay(true)?;
            client.set_nodelay(true)?;
            tokio::io::copy_bidirectional(&mut client, &mut backend).await?;
        }
        Ok(Err(e)) => {
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
    connect_timeout: Duration,
) -> Result<()> {
    let connect_result =
        tokio::time::timeout(connect_timeout, TcpStream::connect(backend_addr)).await;

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
    static RESP_408: &[u8] = b"HTTP/1.1 408 Request Timeout\r\nContent-Length: 19\r\nConnection: close\r\n\r\n408 Request Timeout";
    static RESP_431: &[u8] = b"HTTP/1.1 431 Request Header Fields Too Large\r\nContent-Length: 35\r\nConnection: close\r\n\r\n431 Request Header Fields Too Large";
    static RESP_502: &[u8] = b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 15\r\nConnection: close\r\n\r\n502 Bad Gateway";
    static RESP_503: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 23\r\nConnection: close\r\n\r\n503 Service Unavailable";
    static RESP_504: &[u8] = b"HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 19\r\nConnection: close\r\n\r\n504 Gateway Timeout";
    static RESP_500: &[u8] = b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 25\r\nConnection: close\r\n\r\n500 Internal Server Error";

    let response = match error_code {
        400 => RESP_400,
        404 => RESP_404,
        408 => RESP_408,
        431 => RESP_431,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_conn_pooling_policy() {
        // A complete, cleanly-framed non-HEAD response is poolable.
        assert!(backend_conn_poolable(true, true, false));
        // HEAD is never pooled: a non-compliant backend body would desync the
        // conn, and we can't tell that apart from a compliant Content-Length at
        // header time without blocking.
        assert!(!backend_conn_poolable(true, true, true));
        // Incomplete or broken-framing responses are never pooled, HEAD or not.
        assert!(!backend_conn_poolable(false, true, false));
        assert!(!backend_conn_poolable(true, false, false));
        assert!(!backend_conn_poolable(false, false, true));
    }
}
