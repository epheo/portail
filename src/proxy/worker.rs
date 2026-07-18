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
use crate::metrics::{ListenerStats, METRICS};
use crate::proxy::backend_pool::{BackendPool, PoolHandle, SharedBackendPool};
use crate::proxy::chunked::ChunkedStream;
use crate::proxy::health::HealthRegistry;
use crate::proxy::http_filters::{
    apply_request_header_modifications, apply_response_header_mods, assemble_forward_headers,
    assemble_forward_headers_slow, dispatch_mirrors, extract_header_mods, extract_url_rewrite,
    sanitize_response_headers, HeaderModifications, MIRROR_BODY_MAX,
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
    /// Idle time before the kernel's first TCP keepalive probe on accepted
    /// sockets (see [`crate::config::PerformanceConfig::tcp_keepalive_time`]).
    pub tcp_keepalive_time: Duration,
    /// Per-step progress budget on body streaming (see
    /// [`crate::config::PerformanceConfig::idle_body_timeout`]); zero disables.
    pub idle_body_timeout: Duration,
}

/// Probe cadence once `tcp_keepalive_time` idle has elapsed: a peer that
/// answers none of `TCP_KEEPALIVE_RETRIES` probes spaced
/// `TCP_KEEPALIVE_INTERVAL` apart is reaped by the kernel (the parked read
/// returns ETIMEDOUT, dropping the task, its pooled backend connection, and
/// both fds). Fixed rather than configurable: reap latency is dominated by
/// the configurable idle threshold, and 10s x 5 keeps the tail short (~50s)
/// without being trigger-happy over one dropped ACK.
pub const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
pub const TCP_KEEPALIVE_RETRIES: u32 = 5;

/// RAII in-flight-connection counter. One guard lives inside each spawned
/// per-connection task; `DataPlane::shutdown` polls the shared count and
/// drains until it reaches zero (or the drain timeout expires). Drop-based so
/// the count stays correct on every exit path, including panics and aborts.
struct ConnGuard {
    active: Arc<AtomicUsize>,
    stats: Arc<ListenerStats>,
}

impl ConnGuard {
    fn new(counter: Arc<AtomicUsize>, stats: Arc<ListenerStats>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        METRICS.connections_accepted_total.inc();
        METRICS.active_connections.inc();
        stats.accepted.fetch_add(1, Ordering::Relaxed);
        stats.active.fetch_add(1, Ordering::Relaxed);
        Self {
            active: counter,
            stats,
        }
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        // Release pairs with the Acquire load in the shutdown drain loop.
        self.active.fetch_sub(1, Ordering::Release);
        METRICS.active_connections.dec();
        self.stats.active.fetch_sub(1, Ordering::Relaxed);
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
    /// Client address for X-Forwarded-For.
    peer_ip: std::net::IpAddr,
    /// Budget for completing a request's header block once bytes arrive.
    client_header_timeout: Duration,
    /// Backend connect budget for raw TCP forwarding (HTTP connects go
    /// through the pool, which carries its own copy).
    connect_timeout: Duration,
    /// Reused across keepalive responses to avoid per-response heap allocation.
    header_buf: Vec<u8>,
    /// Reused header-assembly scratch: outgoing request block during the send
    /// phase, sanitized response block during the response phase.
    scratch_buf: Vec<u8>,
    /// Per-step body-streaming progress budget; None = disabled.
    idle_body_timeout: Option<Duration>,
    /// Per-listener stats slot (shared with the accept loop) for per-port
    /// request/error accounting.
    listener_stats: Arc<ListenerStats>,
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
    // Per-port scrape identity: lets an operator answer "is anything accepting
    // on this port" from /metrics alone, without exec'ing into the pod. The
    // guard flips `portail_listener_up` to 0 when this loop exits.
    let (listener_stats, _up_guard) = crate::metrics::register_listener("tcp", server_port);

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

    // Dead-peer detection: a client that vanishes without FIN/RST would
    // otherwise park its connection task forever, since the idle wait
    // between keepalive requests is intentionally unbounded.
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(cfg.tcp_keepalive_time)
        .with_interval(TCP_KEEPALIVE_INTERVAL)
        .with_retries(TCP_KEEPALIVE_RETRIES);

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
                        // _-prefixed: debug! compiles out in release builds.
                        if let Err(_e) =
                            socket2::SockRef::from(&tcp_stream).set_tcp_keepalive(&keepalive)
                        {
                            debug!("TCP keepalive setup failed for {}: {}", peer, _e);
                        }
                        let routes = routes.clone();
                        let acceptor = tls_acceptor.clone();
                        let health = health.clone();
                        let conn_guard =
                            ConnGuard::new(active_connections.clone(), listener_stats.clone());

                        if tls_passthrough && acceptor.is_none() {
                            // Pure passthrough mode (no TLS termination cert available)
                            let selector = selector.clone();
                            tokio::spawn(async move {
                                let _guard = conn_guard;
                                let _ = handle_tls_passthrough(tcp_stream, server_port, routes, health, selector, cfg.client_header_timeout, cfg.connect_timeout).await;
                            });
                        } else if let Some(acceptor) = acceptor {
                            // TLS termination mode — but first check if this SNI
                            // should be passed through instead of terminated.
                            // This handles the case where both HTTPS/Terminate and
                            // TLS/Passthrough listeners share the same port.
                            let selector = selector.clone();
                            let pool = make_pool(&shared_pool);
                            let listener_stats = listener_stats.clone();
                            tokio::spawn(async move {
                                let _guard = conn_guard;
                                // Peek ClientHello for SNI to decide dispatch
                                // mode, waiting out fragmented hellos.
                                let should_passthrough = match crate::proxy::tls::peek_sni(
                                    &tcp_stream,
                                    cfg.client_header_timeout,
                                )
                                .await
                                {
                                    Some(hostname) => {
                                        let rt = routes.load();
                                        rt.has_tls_passthrough_route(&hostname, server_port)
                                    }
                                    None => false,
                                };

                                if should_passthrough {
                                    let _ = handle_tls_passthrough(tcp_stream, server_port, routes, health, selector, cfg.client_header_timeout, cfg.connect_timeout).await;
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
                                                peer_ip: peer.ip(),
                                                client_header_timeout: cfg.client_header_timeout,
                                                connect_timeout: cfg.connect_timeout,
                                                header_buf: Vec::with_capacity(1024),
                                                scratch_buf: Vec::with_capacity(1024),
                                                idle_body_timeout: idle_opt(
                                                    cfg.idle_body_timeout,
                                                ),
                                                listener_stats: listener_stats.clone(),
                                            };
                                            let _ = handle_connection(conn, peer, state).await;
                                        }
                                        Ok(Err(_e)) => {
                                            METRICS.tls_handshake_failures_total.inc();
                                            listener_stats
                                                .tls_handshake_failures
                                                .fetch_add(1, Ordering::Relaxed);
                                            debug!("TLS handshake failed from {}: {}", peer, _e);
                                        }
                                        Err(_) => {
                                            METRICS.tls_handshake_failures_total.inc();
                                            listener_stats
                                                .tls_handshake_failures
                                                .fetch_add(1, Ordering::Relaxed);
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
                                peer_ip: peer.ip(),
                                client_header_timeout: cfg.client_header_timeout,
                                connect_timeout: cfg.connect_timeout,
                                header_buf: Vec::with_capacity(1024),
                                scratch_buf: Vec::with_capacity(1024),
                                idle_body_timeout: idle_opt(cfg.idle_body_timeout),
                                listener_stats: listener_stats.clone(),
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
        // Loop top = this request's bytes are in buf (initial read or the
        // keepalive read at the bottom), so this stamps headers-complete.
        let request_started = std::time::Instant::now();
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
                // Per-vhost sample (rule carries its route hostname's registry
                // slot) + the per-listener aggregate.
                rule.requests.fetch_add(1, Ordering::Relaxed);
                state
                    .listener_stats
                    .http_requests
                    .fetch_add(1, Ordering::Relaxed);
                // proxy_http_request owns all timing for the request lifecycle —
                // backend_request_timeout (connect + TTFB) and request_timeout
                // (total deadline) are applied phase-aware inside.
                let (ka, leftover) =
                    proxy_http_request(&mut client, &mut buf, n, backend, rule, &meta, &mut state)
                        .await?;
                if !meta.is_upgrade {
                    crate::metrics::REQUEST_DURATION.observe(request_started.elapsed());
                }
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

    // Assemble the outgoing header block once — hop-by-hop stripped,
    // X-Forwarded-* appended, filter modifications applied. The pool-retry
    // resend and the mirror capture reuse it verbatim.
    build_outgoing_headers(buf, req_header_end, rule, backend_ref, meta, state);
    let headers_len = state.scratch_buf.len();
    let mirror_header_data: Option<Vec<u8>> = has_mirrors.then(|| state.scratch_buf.clone());

    let send_result = send_assembled_request(
        &mut backend,
        &mut state.scratch_buf,
        headers_len,
        &buf[req_header_end..request_end],
    )
    .await;
    match send_result {
        Ok(()) => {}
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
                    return fail_backend(
                        &state.health,
                        &state.listener_stats,
                        &backend_metric_label(backend_ref),
                        client,
                        backend_addr,
                        502,
                    )
                    .await;
                }
                Err(_) => {
                    warn!(
                        "Backend {} fresh-conn acquire timeout after pool retry",
                        backend_addr
                    );
                    return fail_backend(
                        &state.health,
                        &state.listener_stats,
                        &backend_metric_label(backend_ref),
                        client,
                        backend_addr,
                        504,
                    )
                    .await;
                }
            };
            if let Err(e2) = send_assembled_request(
                &mut backend,
                &mut state.scratch_buf,
                headers_len,
                &buf[req_header_end..request_end],
            )
            .await
            {
                warn!(
                    "Backend {} send failed on fresh conn after pool retry: {}",
                    backend_addr, e2
                );
                return fail_backend(
                    &state.health,
                    &state.listener_stats,
                    &backend_metric_label(backend_ref),
                    client,
                    backend_addr,
                    502,
                )
                .await;
            }
        }
        Err(e) => return Err(e),
    };

    // Expect: 100-continue — the proxy answers on the backend's behalf. The
    // Expect header was stripped from the forwarded request (a compliant
    // backend then never sends its own 100, and a stray one is swallowed by
    // the response reader), so the client's wait for the interim response
    // must be satisfied here or uploads deadlock: the client waits for 100,
    // the proxy waits for body, the backend waits for the request.
    if meta.expect_continue {
        client.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await?;
        client.flush().await?;
    }

    // Relay remaining request body + optional mirror tee, clamped to the
    // rule's total deadline — `request_timeout` covers the request-body phase
    // too, so a slow client body cannot hold the backend conn open past it.
    // Expiry is SAFE for the client wire (no response bytes yet) → 408; the
    // interim 100 above does not spoil that (interim-then-final is legal).
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
        state.idle_body_timeout,
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
        &mut state.scratch_buf,
        resp_mods.as_ref(),
        header_budget,
        meta.is_head,
        meta.is_upgrade,
        !meta.keepalive,
    )
    .await?
    {
        Some(f) => f,
        None => return Ok((false, None)),
    };
    crate::metrics::UPSTREAM_TTFB.observe(backend_phase_start.elapsed());

    // WebSocket / protocol upgrade: raw bidi only once the backend ACCEPTED
    // (101). A refused upgrade (e.g. 403 with a body) stays on normal HTTP
    // framing — tunneling it would let the client's next requests bypass
    // routing, filters, and timeouts entirely.
    if meta.is_upgrade && framing.status == 101 {
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
                forward_response_body(
                    &mut backend,
                    client,
                    buf,
                    &mut framing,
                    state.idle_body_timeout,
                ),
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
        None => {
            forward_response_body(
                &mut backend,
                client,
                buf,
                &mut framing,
                state.idle_body_timeout,
            )
            .await
        }
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
    // only when `backend_conn_poolable` allows — notably never after HEAD or a
    // refused upgrade — and never when the backend announced Connection: close.
    if backend_conn_poolable(
        response_complete,
        framing_clean,
        meta.is_head,
        meta.is_upgrade,
    ) && !framing.backend_close
    {
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
/// A refused upgrade is likewise never pooled: the client may have sent
/// speculative tunnel bytes with the request (forwarded as-is, exempt from
/// body framing), which the backend would parse as a next request.
fn backend_conn_poolable(
    response_complete: bool,
    framing_clean: bool,
    is_head: bool,
    was_upgrade: bool,
) -> bool {
    response_complete && framing_clean && !is_head && !was_upgrade
}

/// Metrics label for a backend: the CONFIGURED name and target port
/// ("jellyfin.jellyfin:8096") — stable while STRICT_DNS re-resolution churns
/// the pod IPs behind it. Falls back to the socket address when a backend
/// was built without a name (tests, manual construction).
fn backend_metric_label(backend: &crate::routing::Backend) -> String {
    if backend.server_name.is_empty() {
        backend.socket_addr.to_string()
    } else {
        format!("{}:{}", backend.server_name, backend.socket_addr.port())
    }
}

/// Record a backend failure (spawning a health probe if this newly trips the
/// backend unhealthy) and send `code` to the client. Returns `Ok((false, None))`
/// — the client connection is not reusable. Centralizes the failure boilerplate
/// the backend-send retry path would otherwise repeat per error arm.
async fn fail_backend(
    health: &Arc<HealthRegistry>,
    stats: &ListenerStats,
    backend_label: &str,
    client: &mut Connection,
    addr: std::net::SocketAddr,
    code: u16,
) -> Result<(bool, Option<Vec<u8>>)> {
    match code {
        504 => {
            crate::metrics::record_backend_connect_timeout(backend_label);
            stats
                .upstream_connect_timeouts
                .fetch_add(1, Ordering::Relaxed);
        }
        _ => {
            crate::metrics::record_backend_connect_error(backend_label);
            stats
                .upstream_connect_errors
                .fetch_add(1, Ordering::Relaxed);
        }
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
            crate::metrics::record_backend_connect_error(&backend_metric_label(backend_ref));
            state
                .listener_stats
                .upstream_connect_errors
                .fetch_add(1, Ordering::Relaxed);
            warn!("Backend {} connect failed: {}", backend_addr, e);
            if state.health.record_failure(backend_addr) {
                HealthRegistry::spawn_probe(Arc::clone(&state.health), backend_addr);
            }
            send_error_response(client, 502).await?;
            Ok(None)
        }
        Err(_) => {
            crate::metrics::record_backend_connect_timeout(&backend_metric_label(backend_ref));
            state
                .listener_stats
                .upstream_connect_timeouts
                .fetch_add(1, Ordering::Relaxed);
            warn!("Backend {} connect timeout", backend_addr);
            if state.health.record_failure(backend_addr) {
                HealthRegistry::spawn_probe(Arc::clone(&state.health), backend_addr);
            }
            send_error_response(client, 504).await?;
            Ok(None)
        }
    }
}

/// Assemble the outgoing request header block into `state.scratch_buf`:
/// hop-by-hop stripping + X-Forwarded-* (always, via parser-recorded spans),
/// then filter header modifications (when configured).
fn build_outgoing_headers(
    buf: &[u8],
    req_header_end: usize,
    rule: &HttpRouteRule,
    backend_ref: &crate::routing::Backend,
    meta: &RequestMeta,
    state: &mut ConnectionState,
) {
    let raw = &buf[..req_header_end];
    if meta.strip_spans.needs_slow_path {
        assemble_forward_headers_slow(
            raw,
            meta.is_upgrade,
            state.peer_ip,
            state.is_tls,
            &mut state.scratch_buf,
        );
    } else {
        assemble_forward_headers(
            raw,
            &meta.strip_spans,
            meta.upgrade_span,
            meta.is_upgrade,
            state.peer_ip,
            state.is_tls,
            &mut state.scratch_buf,
        );
    }

    if rule.has_filters || !backend_ref.filters.is_empty() {
        let rule_mods = extract_header_mods(&rule.filters, false);
        let request_path = extract_request_path(raw);
        let url_rewrite =
            extract_url_rewrite(&rule.filters, &rule.path, request_path.unwrap_or("/"));
        let backend_mods = extract_header_mods(&backend_ref.filters, false);
        if rule_mods.is_some() || url_rewrite.is_some() || backend_mods.is_some() {
            let modified = apply_request_header_modifications(
                &state.scratch_buf,
                rule_mods.as_ref(),
                url_rewrite.as_ref(),
            );
            let final_headers = if let Some(bm) = backend_mods {
                apply_request_header_modifications(&modified, Some(&bm), None)
            } else {
                modified
            };
            state.scratch_buf.clear();
            state.scratch_buf.extend_from_slice(&final_headers);
        }
    }
}

/// Send the assembled headers plus the same-read body tail (up to the
/// request's framing boundary — bytes beyond it belong to a pipelined next
/// request and never reach the backend). Small tails are appended to the
/// scratch so the common case stays one write; the scratch is truncated back
/// to the headers afterwards, keeping the pool-retry resend valid.
async fn send_assembled_request(
    backend: &mut Connection,
    scratch: &mut Vec<u8>,
    headers_len: usize,
    body: &[u8],
) -> Result<()> {
    const COMBINE_MAX: usize = 16 * 1024;
    if body.is_empty() {
        backend.write_all(&scratch[..headers_len]).await?;
    } else if body.len() <= COMBINE_MAX {
        scratch.truncate(headers_len);
        scratch.extend_from_slice(body);
        let res = backend.write_all(scratch).await;
        scratch.truncate(headers_len);
        res?;
    } else {
        backend.write_all(&scratch[..headers_len]).await?;
        backend.write_all(body).await?;
    }
    Ok(())
}

/// Read response headers from the backend within `header_budget` and forward
/// them to the client. This is the SAFE→UNSAFE transition: on entry no client
/// bytes have been written and timeout → `504`; on success the client has the
/// headers and the caller must use encoding-aware termination on later expiry.
///
/// Returns `Ok(None)` when the caller should bail out (`return Ok(false)`).
#[allow(clippy::too_many_arguments)]
async fn forward_response_headers(
    client: &mut Connection,
    backend: &mut Connection,
    buf: &mut [u8],
    header_buf: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
    resp_mods: Option<&HeaderModifications<'_>>,
    header_budget: Duration,
    is_head_request: bool,
    expect_upgrade: bool,
    client_close: bool,
) -> Result<Option<ResponseFraming>> {
    let (resp_header_end, framing) = match tokio::time::timeout(
        header_budget,
        read_response_headers(backend, buf, header_buf, is_head_request, expect_upgrade),
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

    // A 101 nobody asked for would turn the client connection into an
    // unframed tunnel; refuse it while the client wire is still clean.
    if framing.status == 101 && !expect_upgrade {
        warn!("Backend sent 101 without an upgrade request; rejecting");
        send_error_response(client, 502).await?;
        return Ok(None);
    }

    // SAFE → UNSAFE: first client write happens here.
    //
    // Only tail bytes within the declared framing are forwarded
    // (`body_bytes_already_forwarded`): for HEAD and no-body statuses that is
    // zero — body bytes a non-compliant backend sent are dropped rather than
    // leaking to the client — and for CL/chunked responses it stops at the
    // framing boundary so backend overshoot never reaches the client.
    let tail_end = resp_header_end + framing.body_bytes_already_forwarded;

    // Accepted upgrade: forwarded verbatim — the client needs the backend's
    // Connection/Upgrade headers intact, and the connection leaves HTTP.
    if framing.status == 101 {
        client.write_all(&header_buf[..tail_end]).await?;
        return Ok(Some(framing));
    }

    // Hop-by-hop response headers never reach the client (RFC 7230 §6.1);
    // the proxy owns the client-side Connection header.
    sanitize_response_headers(&header_buf[..resp_header_end], client_close, scratch);
    if let Some(mods) = resp_mods {
        let modified = apply_response_header_mods(scratch, mods);
        client.write_all(&modified).await?;
        if tail_end > resp_header_end {
            client
                .write_all(&header_buf[resp_header_end..tail_end])
                .await?;
        }
    } else if tail_end > resp_header_end {
        let tail = &header_buf[resp_header_end..tail_end];
        // Hot path: sanitized headers + in-framing tail in a single write
        // when the tail is small enough to append to the scratch.
        if tail.len() <= 16 * 1024 {
            let sanitized_len = scratch.len();
            scratch.extend_from_slice(tail);
            let res = client.write_all(scratch).await;
            scratch.truncate(sanitized_len);
            res?;
        } else {
            client.write_all(scratch).await?;
            client.write_all(tail).await?;
        }
    } else {
        client.write_all(scratch).await?;
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
#[allow(clippy::too_many_arguments)]
async fn relay_request_body(
    client: &mut Connection,
    backend: &mut Connection,
    buf: &mut [u8],
    header_end: usize,
    request_end: usize,
    content_length: Option<usize>,
    chunked: Option<ChunkedStream>,
    has_mirrors: bool,
    idle: Option<Duration>,
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
            let n = idle_guarded(idle, client.read(&mut buf[..])).await?;
            if n == 0 {
                break;
            }
            let to_send = n.min(remaining);
            idle_guarded(idle, backend.write_all(&buf[..to_send])).await?;
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
            let n = idle_guarded(idle, client.read(&mut buf[..])).await?;
            if n == 0 {
                break;
            }
            let consumed = stream.observe(&buf[..n]);
            idle_guarded(idle, backend.write_all(&buf[..consumed])).await?;
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
    /// Response status code (0 when the status line is unparseable).
    status: u16,
    /// The backend announced it will close this connection (Connection:
    /// close, or an HTTP/1.0 response without keep-alive). Such a conn must
    /// not return to the pool: the next write would fail and burn the retry.
    backend_close: bool,
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
/// Interim (1xx) responses tolerated per exchange before the backend is
/// declared broken. Generous: a compliant exchange has at most a couple
/// (100, 103), the cap only stops an infinite interim stream.
const MAX_INTERIM_RESPONSES: u32 = 8;

async fn read_response_headers(
    backend: &mut Connection,
    buf: &mut [u8],
    header_buf: &mut Vec<u8>,
    is_head_request: bool,
    expect_upgrade: bool,
) -> Result<(usize, ResponseFraming)> {
    header_buf.clear();
    let mut interim_seen = 0u32;

    loop {
        // Check before reading: a swallowed interim response leaves the next
        // response's bytes already buffered.
        if let Some(header_end) = find_header_end(header_buf) {
            let headers = &header_buf[..header_end];
            let status = parse_response_status(headers);

            // Interim responses are proxy-consumed: the proxy answers
            // 100-continue itself (Expect never reaches the backend), and
            // relaying other 1xx mid-exchange would confuse clients that
            // already got a 100. 1xx has no body, so everything past the
            // block belongs to the next response. 101 is not interim here —
            // it is dispatched below (upgrade accept or reject).
            if (100..200).contains(&status) && status != 101 {
                interim_seen += 1;
                if interim_seen > MAX_INTERIM_RESPONSES {
                    return Err(anyhow::anyhow!(
                        "backend sent more than {} interim 1xx responses",
                        MAX_INTERIM_RESPONSES
                    ));
                }
                debug!("Swallowing backend interim {} response", status);
                header_buf.drain(..header_end);
                continue;
            }
            let backend_close = response_wants_close(headers);
            let content_length = parse_content_length(headers);
            let is_chunked = is_chunked_transfer(headers);
            // RFC 7230 §3.3: responses to HEAD have no message body regardless
            // of the response's Content-Length / Transfer-Encoding. Forcing
            // no_body here keeps `forward_response_body` from blocking on
            // bytes the backend correctly never sends.
            let no_body =
                is_head_request || status == 204 || status == 304 || (100..200).contains(&status);

            // Decide how many of the tail bytes (same read as the headers)
            // belong to THIS response's body. Anything beyond the declared
            // framing is overshoot: dropped, and the conn is not poolable.
            let tail_len = header_buf.len() - header_end;
            let (tail_forward, chunked, overshoot) = if is_head_request {
                // HEAD: any body bytes a non-compliant backend sent are
                // dropped (never forwarded per RFC 7230 §3.3); the conn is
                // already never pooled after HEAD.
                (0, None, false)
            } else if status == 101 && expect_upgrade {
                // Accepted upgrade: bytes after the 101 header block are
                // tunnel payload that arrived early — they must reach the
                // client, not be dropped as no-body overshoot.
                (tail_len, None, false)
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
                    status,
                    backend_close,
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

        let n = backend.read(buf).await?;
        if n == 0 {
            return Err(anyhow::anyhow!(
                "backend closed before sending response headers"
            ));
        }
        header_buf.extend_from_slice(&buf[..n]);
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
    idle: Option<Duration>,
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
        let n = idle_guarded(idle, backend.read(buf)).await?;
        if n == 0 {
            return Ok(false);
        }

        // Forward only bytes within the declared framing; anything past the
        // boundary is backend overshoot — dropped, and flagged so the caller
        // never pools the connection.
        if let Some(stream) = &mut framing.chunked {
            let consumed = stream.observe(&buf[..n]);
            idle_guarded(idle, client.write_all(&buf[..consumed])).await?;
            if stream.is_terminated() {
                framing.overshoot |= consumed < n;
                return Ok(true);
            }
        } else if let Some(cl) = framing.content_length {
            let to_write = n.min(cl - body_bytes_forwarded);
            idle_guarded(idle, client.write_all(&buf[..to_write])).await?;
            body_bytes_forwarded += to_write;
            if body_bytes_forwarded >= cl {
                framing.overshoot |= to_write < n;
                return Ok(true);
            }
        } else {
            // EOF-framed: everything until close is body.
            idle_guarded(idle, client.write_all(&buf[..n])).await?;
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

/// Zero means disabled in the config; the streaming loops branch on Option.
#[inline]
fn idle_opt(d: Duration) -> Option<Duration> {
    (!d.is_zero()).then_some(d)
}

/// Await one body-streaming I/O step under the optional progress budget.
/// Only wraps the multi-read/write streaming loops — the common single-read
/// response never reaches these, so no timer is created on that path.
#[inline]
async fn idle_guarded<T, F>(idle: Option<Duration>, fut: F) -> Result<T>
where
    F: std::future::Future<Output = std::io::Result<T>>,
{
    match idle {
        Some(d) => match tokio::time::timeout(d, fut).await {
            Ok(r) => Ok(r?),
            Err(_) => Err(anyhow::anyhow!(
                "body stream stalled: no progress within {:?}",
                d
            )),
        },
        None => Ok(fut.await?),
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
    selector: Arc<BackendSelector>,
    peek_timeout: Duration,
    connect_timeout: Duration,
) -> Result<()> {
    let hostname = match tls::peek_sni(&client, peek_timeout).await {
        Some(h) => h,
        None => {
            return Err(anyhow::anyhow!(
                "TLS passthrough: no complete ClientHello with SNI within {:?}, cannot route",
                peek_timeout
            ))
        }
    };

    let route_table = routes.load();
    let backend_addr = route_table
        .resolve_tls_passthrough(&hostname, server_port, &selector, &health)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "TLS passthrough: no route or no healthy backend for SNI '{}'",
                hostname
            )
        })?;

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

/// Whether the backend announced closing this connection: an explicit
/// Connection: close token, or an HTTP/1.0 response without keep-alive.
fn response_wants_close(headers: &[u8]) -> bool {
    if let Some(v) = crate::routing::find_header_value(headers, "connection") {
        for token in v.split(',') {
            let t = token.trim();
            if t.eq_ignore_ascii_case("close") {
                return true;
            }
            if t.eq_ignore_ascii_case("keep-alive") {
                return false;
            }
        }
    }
    headers.starts_with(b"HTTP/1.0")
}

/// Parse the 3-digit status code from a response status line
/// ("HTTP/1.1 200 ..."). Returns 0 when unparseable.
fn parse_response_status(headers: &[u8]) -> u16 {
    if headers.len() < 12 {
        return 0;
    }
    let s = &headers[9..12];
    if s.iter().all(|b| b.is_ascii_digit()) {
        (s[0] - b'0') as u16 * 100 + (s[1] - b'0') as u16 * 10 + (s[2] - b'0') as u16
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_conn_pooling_policy() {
        // A complete, cleanly-framed non-HEAD response is poolable.
        assert!(backend_conn_poolable(true, true, false, false));
        // HEAD is never pooled: a non-compliant backend body would desync the
        // conn, and we can't tell that apart from a compliant Content-Length at
        // header time without blocking.
        assert!(!backend_conn_poolable(true, true, true, false));
        // A refused upgrade is never pooled: speculative tunnel bytes may have
        // been forwarded outside body framing.
        assert!(!backend_conn_poolable(true, true, false, true));
        // Incomplete or broken-framing responses are never pooled, HEAD or not.
        assert!(!backend_conn_poolable(false, true, false, false));
        assert!(!backend_conn_poolable(true, false, false, false));
        assert!(!backend_conn_poolable(false, false, true, false));
    }

    #[test]
    fn response_status_parsing() {
        assert_eq!(parse_response_status(b"HTTP/1.1 200 OK\r\n\r\n"), 200);
        assert_eq!(
            parse_response_status(b"HTTP/1.1 101 Switching Protocols\r\n\r\n"),
            101
        );
        assert_eq!(parse_response_status(b"HTTP/1.0 304 Not Modified\r\n"), 304);
        assert_eq!(parse_response_status(b"HTTP/1.1 abc\r\n"), 0);
        assert_eq!(parse_response_status(b"short"), 0);
    }
}
