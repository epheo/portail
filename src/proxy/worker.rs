//! Tokio async TCP worker — one task per accepted connection.
//!
//! HTTP request/response proxying with keepalive and backend connection pooling,
//! raw TCP bidirectional forwarding, TLS termination, and TLS passthrough.

use anyhow::Result;
use arc_swap::ArcSwap;
use std::borrow::Cow;
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
    absolute_form_target, apply_request_header_modifications, apply_response_header_mods,
    assemble_forward_headers, assemble_forward_headers_slow, dispatch_mirrors, extract_header_mods,
    extract_url_rewrite, header_lines, sanitize_response_headers, to_origin_form,
    HeaderModifications, MIRROR_BODY_MAX,
};
use crate::proxy::http_parser::{find_header_end, is_token, parse_digits, trim_ows};
use crate::proxy::method::{is_method_byte, MAX_METHOD_LEN};
use crate::proxy::request_processor::{
    analyze_request, is_http_request, LocalReply, RequestMeta, RoutingResult,
};
use crate::proxy::tls::{self, Connection, DynamicTlsAcceptor};
use crate::routing::{BackendSelector, HttpFilter, HttpRouteRule, RouteTable};

/// Continue reading from `client` into `buf` starting at offset `already_read`
/// until the full HTTP headers (\r\n\r\n) are found or the buffer fills.
/// Returns 0 when the client closes first.
async fn read_remaining_headers(
    client: &mut Connection,
    buf: &mut [u8],
    already_read: usize,
) -> Result<usize> {
    let mut total = already_read;
    loop {
        if total >= buf.len() {
            return Err(anyhow::anyhow!("Request headers exceed buffer size"));
        }
        let n = client.read(&mut buf[total..]).await?;
        if n == 0 {
            // Never parse a truncated head: the proxy's own headers would be
            // appended to its unterminated last line, which could hide them
            // (e.g. inside a client header's value).
            return Ok(0);
        }
        total += n;
        if find_header_end(&buf[..total]).is_some() {
            return Ok(total);
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
    /// Advertise h2 via ALPN on TLS-terminate listeners and bridge h2
    /// streams through this engine (see `h2_bridge`). Off by default.
    pub http2: bool,
}

impl WorkerConfig {
    pub fn from_perf(perf: &crate::config::PerformanceConfig, max_idle_per_backend: usize) -> Self {
        Self {
            max_idle_per_backend,
            connect_timeout: perf.backend_timeout,
            client_header_timeout: perf.client_header_timeout,
            tcp_keepalive_time: perf.tcp_keepalive_time,
            idle_body_timeout: perf.idle_body_timeout,
            http2: perf.http2,
        }
    }
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
        stats.accepted.inc();
        stats.active.inc();
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
        self.stats.active.dec();
    }
}

pub(crate) struct ConnectionState {
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
    /// Second reusable header buffer for filter passes that rewrite the
    /// scratch (mem::swap partner). Empty until a filtered route is hit, so
    /// the no-filter path never allocates it.
    filter_buf: Vec<u8>,
    /// Per-step body-streaming progress budget; None = disabled.
    idle_body_timeout: Option<Duration>,
    /// Per-listener stats slot (shared with the accept loop) for per-port
    /// request/error accounting.
    listener_stats: Arc<ListenerStats>,
    /// Status of the current request's response as written to the client,
    /// recorded where each response is emitted (fail helpers included) so the
    /// keepalive loop can access-log it without threading a value through
    /// every early return. 0 = no response written yet.
    last_status: u16,
}

impl ConnectionState {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        server_port: u16,
        routes: Arc<ArcSwap<RouteTable>>,
        pool: PoolHandle,
        selector: Arc<BackendSelector>,
        health: Arc<HealthRegistry>,
        is_tls: bool,
        peer_ip: std::net::IpAddr,
        cfg: &WorkerConfig,
        listener_stats: Arc<ListenerStats>,
    ) -> Self {
        Self {
            server_port,
            routes,
            pool,
            selector,
            health,
            is_tls,
            peer_ip,
            client_header_timeout: cfg.client_header_timeout,
            connect_timeout: cfg.connect_timeout,
            header_buf: Vec::with_capacity(1024),
            scratch_buf: Vec::with_capacity(1024),
            filter_buf: Vec::new(),
            idle_body_timeout: idle_opt(cfg.idle_body_timeout),
            listener_stats,
            last_status: 0,
        }
    }
}

/// Pool handle for one client connection: the process-wide shared pool when
/// `performance.backendPoolScope: process` is set, else a fresh
/// per-connection pool (the default).
pub(crate) fn make_pool(shared: &Option<Arc<SharedBackendPool>>, cfg: &WorkerConfig) -> PoolHandle {
    match shared {
        Some(p) => PoolHandle::Shared(p.clone()),
        None => PoolHandle::PerConn(BackendPool::new(
            cfg.max_idle_per_backend,
            cfg.connect_timeout,
        )),
    }
}

/// Everything an accept loop shares across its connections, bundled so the
/// data plane's two spawn sites thread one value instead of eleven arguments.
pub struct ListenerCtx {
    pub server_port: u16,
    pub routes: Arc<ArcSwap<RouteTable>>,
    pub cfg: WorkerConfig,
    pub tls_acceptor: Option<Arc<DynamicTlsAcceptor>>,
    pub tls_passthrough: bool,
    pub health: Arc<HealthRegistry>,
    pub selector: Arc<BackendSelector>,
    pub active_connections: Arc<AtomicUsize>,
    pub shared_pool: Option<Arc<SharedBackendPool>>,
}

/// Run an accept loop on a single listener, spawning a task per connection.
pub async fn run_worker(listener: TcpListener, ctx: ListenerCtx, shutdown: CancellationToken) {
    let ListenerCtx {
        server_port,
        routes,
        cfg,
        tls_acceptor,
        tls_passthrough,
        health,
        selector,
        active_connections,
        shared_pool,
    } = ctx;
    // Per-port scrape identity: lets an operator answer "is anything accepting
    // on this port" from /metrics alone, without exec'ing into the pod. The
    // guard flips `portail_listener_up` to 0 when this loop exits.
    let (listener_stats, _up_guard) = crate::metrics::register_listener("tcp", server_port);

    // Selector is shared across all listeners so weighted round-robin counters
    // produce the correct distribution globally.

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
                            let selector = selector.clone();
                            let shared_pool = shared_pool.clone();
                            let listener_stats = listener_stats.clone();
                            tokio::spawn(async move {
                                let _guard = conn_guard;
                                serve_tls_connection(
                                    tcp_stream,
                                    peer,
                                    acceptor,
                                    server_port,
                                    routes,
                                    cfg,
                                    health,
                                    selector,
                                    shared_pool,
                                    listener_stats,
                                )
                                .await;
                            });
                        } else {
                            let state = ConnectionState::new(
                                server_port,
                                routes,
                                make_pool(&shared_pool, &cfg),
                                selector.clone(),
                                health,
                                false,
                                peer.ip(),
                                &cfg,
                                listener_stats.clone(),
                            );
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

/// One accepted connection on a TLS-terminate listener.
///
/// First check whether this SNI should be passed through instead of
/// terminated — that handles HTTPS/Terminate and TLS/Passthrough listeners
/// sharing one port. Otherwise handshake, then dispatch by ALPN: h2 goes to
/// the bridge, all else stays on the h1 path.
#[allow(clippy::too_many_arguments)]
async fn serve_tls_connection(
    tcp_stream: TcpStream,
    peer: SocketAddr,
    acceptor: Arc<DynamicTlsAcceptor>,
    server_port: u16,
    routes: Arc<ArcSwap<RouteTable>>,
    cfg: WorkerConfig,
    health: Arc<HealthRegistry>,
    selector: Arc<BackendSelector>,
    shared_pool: Option<Arc<SharedBackendPool>>,
    listener_stats: Arc<ListenerStats>,
) {
    // Peek ClientHello for SNI to decide dispatch mode, waiting out
    // fragmented hellos.
    let should_passthrough =
        match crate::proxy::tls::peek_sni(&tcp_stream, cfg.client_header_timeout).await {
            Some(hostname) => {
                let rt = routes.load();
                rt.has_tls_passthrough_route(&hostname, server_port)
            }
            None => false,
        };

    if should_passthrough {
        let _ = handle_tls_passthrough(
            tcp_stream,
            server_port,
            routes,
            health,
            selector,
            cfg.client_header_timeout,
            cfg.connect_timeout,
        )
        .await;
        return;
    }

    // Bound the handshake: a client that stalls mid-handshake would
    // otherwise pin this task (and its buffers) forever.
    let tls_stream = match tokio::time::timeout(
        cfg.client_header_timeout,
        acceptor.acceptor().accept(tcp_stream),
    )
    .await
    {
        Ok(Ok(s)) => s,
        outcome => {
            METRICS.tls_handshake_failures_total.inc();
            listener_stats.tls_handshake_failures.inc();
            match outcome {
                Ok(Err(_e)) => debug!("TLS handshake failed from {}: {}", peer, _e),
                _ => debug!("TLS handshake from {} timed out", peer),
            }
            return;
        }
    };

    // ALPN decided the protocol during the handshake.
    if cfg.http2 && tls_stream.get_ref().1.alpn_protocol() == Some(b"h2".as_ref()) {
        METRICS.http2_connections_total.inc();
        // One pool per h2 connection: the process pool when set, else a
        // connection-scoped pool its streams share (h1 keepalive semantics).
        let pool = shared_pool.unwrap_or_else(|| {
            Arc::new(SharedBackendPool::new(
                cfg.max_idle_per_backend,
                cfg.connect_timeout,
            ))
        });
        crate::proxy::h2_bridge::serve(
            tls_stream,
            crate::proxy::h2_bridge::H2Ctx {
                server_port,
                routes,
                selector,
                health,
                pool,
                cfg,
                listener_stats,
                peer,
            },
        )
        .await;
        return;
    }
    let conn = Connection::Tls { inner: tls_stream };
    let state = ConnectionState::new(
        server_port,
        routes,
        make_pool(&shared_pool, &cfg),
        selector,
        health,
        true,
        peer.ip(),
        &cfg,
        listener_stats,
    );
    let _ = handle_connection(conn, peer, state).await;
}

pub(crate) async fn handle_connection(
    mut client: Connection,
    _peer: SocketAddr,
    mut state: ConnectionState,
) -> Result<()> {
    client.set_nodelay(true)?;

    let mut buf = vec![0u8; 65536];

    let mut n = client.read(&mut buf).await?;

    // If data looks like HTTP but headers aren't complete, accumulate more reads.
    // TCP/binary data (no \r\n\r\n expected) passes through immediately.
    if is_http_request(&buf[..n]) && find_header_end(&buf[..n]).is_none() {
        n = complete_headers(&mut client, &mut buf, n, state.client_header_timeout).await?;
    }
    if n == 0 {
        return Ok(());
    }

    loop {
        // Loop top = this request's bytes are in buf (initial read or the
        // keepalive read at the bottom), so this stamps headers-complete.
        let request_started = std::time::Instant::now();
        // Copied out now because the response phase reuses buf as scratch;
        // None whenever access logging is off.
        let access = crate::access_log::capture(
            &buf[..n],
            request_started,
            state.peer_ip,
            state.server_port,
            state.is_tls,
        );
        let route_table = state.routes.load();
        let result = analyze_request(
            &route_table,
            &state.selector,
            &buf[..n],
            state.server_port,
            &state.health,
            state.is_tls,
        )?;

        // Bytes read past this request (a pipelined next request) are carried
        // over and replayed as the next iteration's input instead of being
        // sent to the backend or lost.
        let pending: Option<Vec<u8>> = match result {
            RoutingResult::TcpForward { backend_addr } => {
                return handle_tcp_connection(
                    client,
                    backend_addr,
                    &buf[..n],
                    Arc::clone(&state.health),
                    state.connect_timeout,
                    "TCP",
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
                state.listener_stats.http_requests.inc();
                let limited = rule
                    .limiter
                    .as_ref()
                    .is_some_and(|l| !l.check(state.peer_ip));
                if limited {
                    crate::metrics::METRICS.rate_limited_total.inc();
                    state.listener_stats.rate_limited.inc();
                    state.last_status = 429;
                    let reply = LocalReply {
                        status: 429,
                        location: None,
                        reusable: meta.local_reply_reusable,
                        head: meta.is_head,
                    };
                    if !send_local_reply(&mut client, &reply, access).await? {
                        return Ok(());
                    }
                    pipelined_tail(&buf[..n])
                } else {
                    // proxy_http_request owns all timing for the request lifecycle —
                    // backend_request_timeout (connect + TTFB) and request_timeout
                    // (total deadline) are applied phase-aware inside.
                    let backend_addr = backend.socket_addr;
                    let (ka, leftover) = proxy_http_request(
                        &mut client,
                        &mut buf,
                        n,
                        backend,
                        rule,
                        &meta,
                        &mut state,
                    )
                    .await?;
                    if !meta.is_upgrade {
                        crate::metrics::REQUEST_DURATION.observe(request_started.elapsed());
                    }
                    crate::access_log::emit(access, state.last_status, Some(backend_addr));
                    if !ka || !meta.keepalive {
                        return Ok(());
                    }
                    leftover
                }
            }
            RoutingResult::LocalReply(reply) => {
                if !send_local_reply(&mut client, &reply, access).await? {
                    return Ok(());
                }
                pipelined_tail(&buf[..n])
            }
            RoutingResult::UdpForward { .. } | RoutingResult::CloseConnection => {
                return Ok(());
            }
        };

        // Explicitly drop the route table guard before reading the next request.
        // This allows old route tables to be freed between keepalive iterations.
        drop(route_table);

        if let Some(bytes) = pending {
            // Replay pipelined bytes as the start of the next request. They
            // came out of `buf` via a single read, so they always fit.
            buf[..bytes.len()].copy_from_slice(&bytes);
            n = bytes.len();
        } else {
            n = client.read(&mut buf).await?;
        }
        // RFC 9112 2.2: empty lines before a request-line are ignored. An
        // old client ends a POST body with a CRLF its length leaves out.
        loop {
            let empty_lines = buf[..n]
                .as_chunks::<2>()
                .0
                .iter()
                .take_while(|pair| *pair == b"\r\n")
                .count();
            if empty_lines == 0 {
                break;
            }
            buf.copy_within(empty_lines * 2..n, 0);
            n -= empty_lines * 2;
            if n == 0 {
                n = client.read(&mut buf).await?;
            }
        }
        // The connection is HTTP by now: bytes that cannot open a request
        // (frames held back from a refused upgrade) close it. They must not
        // reach TCP routing, nor wait out the header budget for a 408.
        if n == 0 || !may_open_request(&buf[..n]) {
            return Ok(());
        }
        if find_header_end(&buf[..n]).is_none() {
            n = complete_headers(&mut client, &mut buf, n, state.client_header_timeout).await?;
        }
        if n == 0 || !is_http_request(&buf[..n]) {
            return Ok(());
        }
    }
}

/// Whether `data` can still be the start of a request: a method and its SP,
/// or a read that ended inside the method token.
fn may_open_request(data: &[u8]) -> bool {
    is_http_request(data)
        || (data.len() <= MAX_METHOD_LEN
            && data.first().is_some_and(u8::is_ascii_uppercase)
            && data.iter().all(|&b| is_method_byte(b)))
}

/// Complete a partially-read request header block within the slow-loris
/// budget. The budget starts once a request's first bytes have arrived — the
/// idle wait between keepalive requests is intentionally unbounded. Sends
/// `431` when the block outgrows the buffer and `408` when the client is too
/// slow, then errors out to close the connection. Returns 0 when the client
/// closed before the head ended: nothing is forwarded or answered.
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
///   already on the wire); we reset the client connection, since a `504` would
///   corrupt framing and a synthetic terminator would fake a complete body.
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
    // Cleared per request so an Err exit can never surface the previous
    // keepalive response's status to the access log.
    state.last_status = 0;

    let req_header_end = find_header_end(&buf[..request_bytes]).unwrap_or(request_bytes);

    // Determine where THIS request ends within the bytes already read, from
    // its own framing, upgrade or not. Bytes past that boundary must never
    // reach the backend as body (a desync/smuggling vector: the backend
    // would parse them as a second request and answer it, poisoning the
    // pooled connection). They are held back as `leftover`: the next
    // pipelined request for the keepalive loop, or early tunnel bytes once
    // the backend accepts an upgrade. Settled before connecting, so a
    // malformed body costs no backend connection.
    let tail_len = request_bytes - req_header_end;
    let (request_end, req_chunked) = if let Some(cl) = meta.content_length {
        (req_header_end + tail_len.min(cl), None)
    } else if meta.is_chunked {
        let mut stream = ChunkedStream::new();
        let consumed = stream.observe(&buf[req_header_end..request_bytes]);
        if stream.is_broken() {
            state.last_status = 400;
            send_error_response(client, 400).await?;
            return Ok((false, None));
        }
        (req_header_end + consumed, Some(stream))
    } else {
        // No declared body: the entire tail is the next pipelined request.
        (req_header_end, None)
    };
    // Capture the held tail now: the response phase reuses `buf` as scratch
    // for backend reads and would overwrite it.
    let mut leftover: Option<Vec<u8>> = if request_end < request_bytes {
        Some(buf[request_end..request_bytes].to_vec())
    } else {
        None
    };

    // Connect (SAFE: no client bytes yet, helper sends 502/504 on failure).
    let (backend, from_pool) =
        match connect_to_backend(client, state, backend_ref, backend_budget, total_deadline).await?
        {
            Some(b) => b,
            None => return Ok((false, None)),
        };

    let has_mirrors = rule
        .filters
        .iter()
        .any(|f| matches!(f, HttpFilter::RequestMirror { .. }));

    // Assemble the outgoing header block once — hop-by-hop stripped,
    // X-Forwarded-* appended, filter modifications applied. The pool-retry
    // resend and the mirror capture reuse it verbatim.
    build_outgoing_headers(buf, req_header_end, rule, backend_ref, meta, state);
    let headers_len = state.scratch_buf.len();
    let mirror_header_data: Option<Vec<u8>> = has_mirrors.then(|| state.scratch_buf.clone());

    let mut backend = match send_request_with_retry(
        client,
        state,
        backend_ref,
        backend,
        from_pool,
        headers_len,
        &buf[req_header_end..request_end],
        backend_budget,
        backend_phase_start,
        total_deadline,
    )
    .await?
    {
        Some(b) => b,
        None => return Ok((false, None)),
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
    let relay = match total_deadline {
        Some(deadline) => {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, relay_fut).await {
                Ok(r) => r?,
                Err(_) => {
                    warn!(
                        "Request body relay to backend {} exceeded request timeout",
                        backend_addr
                    );
                    state.last_status = 408;
                    send_error_response(client, 408).await?;
                    return Ok((false, None));
                }
            }
        }
        None => relay_fut.await?,
    };
    // An incomplete body drops `backend` on return, never pooling it. No
    // final response has gone out yet (at most the interim 100), so a 400
    // is still well-formed.
    let (mirror_body, relay_leftover) = match relay {
        RequestBody::Complete { mirror, leftover } => (mirror, leftover),
        incomplete => {
            if let RequestBody::Broken = incomplete {
                warn!(
                    "Rejecting request with malformed chunked body to backend {}",
                    backend_addr
                );
            } else {
                debug!("Client closed mid request body to backend {}", backend_addr);
            }
            state.last_status = 400;
            // The client may already be gone (EOF case); the close follows
            // either way.
            let _ = send_error_response(client, 400).await;
            return Ok((false, None));
        }
    };
    if relay_leftover.is_some() {
        leftover = relay_leftover;
    }

    // Both are Some iff has_mirrors: pairing them here keeps any fallback to
    // the raw (unsanitized) client headers structurally impossible.
    if let (Some(body), Some(header_bytes)) = (&mirror_body, &mirror_header_data) {
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
        &mut state.header_buf,
        &mut state.scratch_buf,
        &mut state.filter_buf,
        resp_mods.as_ref(),
        header_budget,
        meta.is_head,
        meta.is_upgrade,
        !meta.keepalive,
    )
    .await?
    {
        HeaderPhase::Proxied(f) => f,
        HeaderPhase::ErrorSent(code) => {
            state.last_status = code;
            return Ok((false, None));
        }
    };
    crate::metrics::UPSTREAM_TTFB.observe(backend_phase_start.elapsed());
    state.last_status = framing.status;

    // WebSocket / protocol upgrade: raw bidi only once the backend ACCEPTED
    // (101). A refused upgrade (e.g. 403 with a body) stays on normal HTTP
    // framing — tunneling it would let the client's next requests bypass
    // routing, filters, and timeouts entirely. Only now do the held bytes
    // become tunnel payload, ahead of anything the tunnel copies; on a
    // refusal they are returned below as the next request instead.
    if meta.is_upgrade && framing.status == 101 {
        if let Some(held) = leftover.take() {
            if backend.write_all(&held).await.is_err() || backend.flush().await.is_err() {
                return Ok((false, None));
            }
        }
        client.flush().await?;
        let _ = tokio::io::copy_bidirectional(client, &mut backend).await;
        return Ok((false, None));
    }

    // Stream response body (UNSAFE: headers on wire). Expiry, errors, and
    // truncation reset the client: no synthetic terminator or 504 may make a
    // cut-short body look complete.
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
                    warn!(
                        "Response body from backend {} exceeded request timeout; resetting client connection",
                        backend_addr
                    );
                    reset_on_drop(client);
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

    match body_result {
        Ok(BodyEnd::Complete) => {}
        Ok(BodyEnd::Closed) => {
            // Close-delimited: our clean close is how the client learns the
            // body ended. Over TLS that takes close_notify, not just a FIN:
            // strict clients read a bare FIN as a truncation attack.
            idle_guarded(state.idle_body_timeout, client.shutdown()).await?;
            return Ok((false, None));
        }
        Ok(BodyEnd::Broken) => {
            warn!(
                "Backend {} sent malformed chunked framing; resetting client connection",
                backend_addr
            );
            reset_on_drop(client);
            return Ok((false, None));
        }
        Ok(BodyEnd::Truncated) => {
            debug!("Backend {} closed mid response body", backend_addr);
            reset_on_drop(client);
            return Ok((false, None));
        }
        Err(_e) => {
            debug!("Response body from backend {} failed: {}", backend_addr, _e);
            reset_on_drop(client);
            return Ok((false, None));
        }
    }

    client.flush().await?;

    // Overshoot (bytes past the framing end) poisons the conn: the excess was
    // dropped here but whatever else follows would desync the next response.
    if framing.overshoot {
        warn!(
            "Backend {} sent bytes past the response framing end; excess dropped, not pooling connection",
            backend_addr
        );
    }
    let reusable = !framing.overshoot;

    // Both bodies completed per their framing: every other outcome returned
    // above, dropping `backend`. The client got a clean response, but the
    // backend conn returns to the pool only when `backend_conn_poolable`
    // allows (notably never after HEAD or a refused upgrade) and never when
    // the backend announced Connection: close.
    if backend_conn_poolable(reusable, meta.is_head, meta.is_upgrade) && !framing.backend_close {
        state.pool.release(
            backend_addr,
            backend_ref.use_tls,
            &backend_ref.server_name,
            backend,
        );
    } else if reusable {
        debug!(
            "Backend {} conn not pooled (HEAD, refused upgrade, or backend Connection: close)",
            backend_addr
        );
    }

    Ok((reusable, leftover))
}

/// Whether a backend connection may be returned to the idle pool after a
/// complete exchange. Callers never ask after an incomplete request or
/// response body: that conn holds stale state and is always dropped.
///
/// Requires clean response framing, and additionally
/// **never after a HEAD request**. A non-compliant backend may send a response
/// body (RFC 7230 §3.3 forbids it) that we cannot reliably detect at header time
/// without blocking: a *compliant* HEAD response legitimately carries
/// `Content-Length` with no body, so the declared framing is no signal. Reusing
/// such a conn would bleed leftover body bytes into the next request's response;
/// HEAD is rare and bodyless, so dropping the conn instead costs almost nothing.
/// A refused upgrade is likewise never pooled: a backend may have detached
/// the socket from its HTTP parser on seeing the upgrade request (Node's
/// 'upgrade' event), so a refusal does not prove it still reads HTTP.
fn backend_conn_poolable(framing_clean: bool, is_head: bool, was_upgrade: bool) -> bool {
    framing_clean && !is_head && !was_upgrade
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
/// the connect and send-retry paths would otherwise repeat per error arm; log
/// wording stays caller-side.
async fn fail_backend(
    state: &mut ConnectionState,
    backend_label: &str,
    client: &mut Connection,
    addr: std::net::SocketAddr,
    code: u16,
) -> Result<(bool, Option<Vec<u8>>)> {
    match code {
        504 => {
            crate::metrics::record_backend_connect_timeout(backend_label);
            state.listener_stats.upstream_connect_timeouts.inc();
        }
        _ => {
            crate::metrics::record_backend_connect_error(backend_label);
            state.listener_stats.upstream_connect_errors.inc();
        }
    }
    if state.health.record_failure(addr) {
        HealthRegistry::spawn_probe(Arc::clone(&state.health), addr);
    }
    state.last_status = code;
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
            warn!("Backend {} connect failed: {}", backend_addr, e);
            fail_backend(
                state,
                &backend_metric_label(backend_ref),
                client,
                backend_addr,
                502,
            )
            .await?;
            Ok(None)
        }
        Err(_) => {
            warn!("Backend {} connect timeout", backend_addr);
            fail_backend(
                state,
                &backend_metric_label(backend_ref),
                client,
                backend_addr,
                504,
            )
            .await?;
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

    // The backend is an origin server, sent origin-form only (RFC 9112
    // 3.2.1). Before filters, so a URLRewrite or set Host meets the same
    // head as for an origin-form request.
    if let Some((target, authority)) = absolute_form_target(raw, meta.path_span) {
        to_origin_form(&state.scratch_buf, target, authority, &mut state.filter_buf);
        std::mem::swap(&mut state.scratch_buf, &mut state.filter_buf);
    }

    // has_filters already covers every backend's filters (see routing build).
    if rule.has_filters {
        let rule_mods = extract_header_mods(&rule.filters, false);
        let request_path = meta
            .path_span
            .and_then(|(s, e)| raw.get(s as usize..e as usize))
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("/");
        let url_rewrite = extract_url_rewrite(&rule.filters, &rule.path, request_path);
        let backend_mods = extract_header_mods(&backend_ref.filters, false);
        if rule_mods.is_some() || url_rewrite.is_some() || backend_mods.is_some() {
            apply_request_header_modifications(
                &state.scratch_buf,
                rule_mods.as_ref(),
                url_rewrite.as_ref(),
                &mut state.filter_buf,
            );
            std::mem::swap(&mut state.scratch_buf, &mut state.filter_buf);
            if let Some(bm) = backend_mods {
                apply_request_header_modifications(
                    &state.scratch_buf,
                    Some(&bm),
                    None,
                    &mut state.filter_buf,
                );
                std::mem::swap(&mut state.scratch_buf, &mut state.filter_buf);
            }
        }
    }
}

/// Tails at or below this ride in the same write as the scratch prefix.
const COMBINE_MAX: usize = 16 * 1024;

/// Write `scratch[..keep_len]` plus `tail`. Small tails are appended to the
/// scratch so the common case stays one write; the scratch is truncated back
/// to `keep_len` afterwards — on error too — so callers can reuse the kept
/// prefix (pool-retry resend, response scratch).
async fn write_with_tail(
    conn: &mut Connection,
    scratch: &mut Vec<u8>,
    keep_len: usize,
    tail: &[u8],
) -> Result<()> {
    if tail.is_empty() {
        conn.write_all(&scratch[..keep_len]).await?;
    } else if tail.len() <= COMBINE_MAX {
        scratch.truncate(keep_len);
        scratch.extend_from_slice(tail);
        let res = conn.write_all(scratch).await;
        scratch.truncate(keep_len);
        res?;
    } else {
        conn.write_all(&scratch[..keep_len]).await?;
        conn.write_all(tail).await?;
    }
    Ok(())
}

/// Send the assembled request: headers in `state.scratch_buf[..headers_len]`
/// plus the same-read `body` tail (up to the request's framing boundary —
/// bytes beyond it belong to a pipelined next request and never reach the
/// backend).
///
/// Retry contract: if the conn came from the pool AND the send errors, we
/// retry once on a fresh conn. SAFE — `relay_request_body` hasn't been
/// called, so no client body bytes have been consumed yet, and no client
/// write has happened either. The scratch and `body` still hold exactly
/// what must be sent; the original send's bytes went into a dead pipe, so
/// no backend observed them. The retry is unconditionally safe regardless
/// of HTTP method.
///
/// `Ok(Some(conn))` hands back the connection that accepted the request;
/// `Ok(None)` means the failure was already answered to the client.
#[allow(clippy::too_many_arguments)]
async fn send_request_with_retry(
    client: &mut Connection,
    state: &mut ConnectionState,
    backend_ref: &crate::routing::Backend,
    mut backend: Connection,
    from_pool: bool,
    headers_len: usize,
    body: &[u8],
    backend_budget: Duration,
    backend_phase_start: std::time::Instant,
    total_deadline: Option<std::time::Instant>,
) -> Result<Option<Connection>> {
    let backend_addr = backend_ref.socket_addr;
    match write_with_tail(&mut backend, &mut state.scratch_buf, headers_len, body).await {
        Ok(()) => return Ok(Some(backend)),
        Err(_e) if from_pool => {
            // Pool entry died between probe and write. Reconnect once and
            // replay. Do NOT record_failure here: a stale pool entry is
            // expected churn, not a backend problem.
            METRICS.pool_stale_retries_total.inc();
            debug!(
                "Backend {} pool conn died on send ({}); retrying with fresh conn",
                backend_addr, _e
            );
        }
        Err(e) => return Err(e),
    }
    drop(backend);
    let fresh_budget = clamp_to_deadline(
        backend_budget.saturating_sub(backend_phase_start.elapsed()),
        total_deadline,
    );
    let mut backend = match tokio::time::timeout(
        fresh_budget,
        state
            .pool
            .acquire_fresh(backend_addr, backend_ref.use_tls, &backend_ref.server_name),
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
            fail_backend(
                state,
                &backend_metric_label(backend_ref),
                client,
                backend_addr,
                502,
            )
            .await?;
            return Ok(None);
        }
        Err(_) => {
            warn!(
                "Backend {} fresh-conn acquire timeout after pool retry",
                backend_addr
            );
            fail_backend(
                state,
                &backend_metric_label(backend_ref),
                client,
                backend_addr,
                504,
            )
            .await?;
            return Ok(None);
        }
    };
    if let Err(e2) = write_with_tail(&mut backend, &mut state.scratch_buf, headers_len, body).await
    {
        warn!(
            "Backend {} send failed on fresh conn after pool retry: {}",
            backend_addr, e2
        );
        fail_backend(
            state,
            &backend_metric_label(backend_ref),
            client,
            backend_addr,
            502,
        )
        .await?;
        return Ok(None);
    }
    Ok(Some(backend))
}

/// Outcome of the response-header phase: the framing to stream the body
/// under, or the error status this helper already wrote to the client.
enum HeaderPhase {
    Proxied(ResponseFraming),
    ErrorSent(u16),
}

/// Read response headers from the backend within `header_budget` and forward
/// them to the client. This is the SAFE→UNSAFE transition: on entry no client
/// bytes have been written and timeout → `504`; on success the client has the
/// headers, and any later failure must reset the client connection instead.
///
/// Returns `ErrorSent` when the caller should bail out (`return Ok(false)`).
#[allow(clippy::too_many_arguments)]
async fn forward_response_headers(
    client: &mut Connection,
    backend: &mut Connection,
    header_buf: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
    filter_buf: &mut Vec<u8>,
    resp_mods: Option<&HeaderModifications<'_>>,
    header_budget: Duration,
    is_head_request: bool,
    expect_upgrade: bool,
    client_close: bool,
) -> Result<HeaderPhase> {
    let resp_header_end =
        match tokio::time::timeout(header_budget, read_response_head(backend, header_buf)).await {
            Ok(Ok(end)) => end,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                send_error_response(client, 504).await?;
                return Ok(HeaderPhase::ErrorSent(504));
            }
        };

    let Some(framing) =
        frame_response(header_buf, resp_header_end, is_head_request, expect_upgrade)
    else {
        warn!("Backend sent a response head with ambiguous framing; rejecting");
        send_error_response(client, 502).await?;
        return Ok(HeaderPhase::ErrorSent(502));
    };

    // A 101 nobody asked for would turn the client connection into an
    // unframed tunnel; refuse it while the client wire is still clean.
    if framing.status == 101 && !expect_upgrade {
        warn!("Backend sent 101 without an upgrade request; rejecting");
        send_error_response(client, 502).await?;
        return Ok(HeaderPhase::ErrorSent(502));
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
        return Ok(HeaderPhase::Proxied(framing));
    }

    // Hop-by-hop response headers never reach the client (RFC 7230 6.1);
    // the proxy owns the client-side Connection header.
    sanitize_response_headers(
        &header_buf[..resp_header_end],
        framing.transfer_encoded,
        client_close || framing.close_delimited(),
        scratch,
    );
    let headers: &mut Vec<u8> = if let Some(mods) = resp_mods {
        apply_response_header_mods(scratch, mods, filter_buf);
        filter_buf
    } else {
        scratch
    };
    // Hot path: sanitized headers + in-framing tail in a single write when
    // the tail is small enough to append to the scratch.
    let keep_len = headers.len();
    let tail = &header_buf[resp_header_end..tail_end];
    write_with_tail(client, headers, keep_len, tail).await?;
    Ok(HeaderPhase::Proxied(framing))
}

/// Extract response header modifications from rule + backend filters (zero-alloc borrows).
fn extract_response_mods<'a>(
    rule: &'a HttpRouteRule,
    backend: &'a crate::routing::Backend,
) -> Option<HeaderModifications<'a>> {
    extract_header_mods(&rule.filters, true).or_else(|| extract_header_mods(&backend.filters, true))
}

/// How the request-body relay ended. Either incomplete outcome leaves a
/// partial request on the backend conn, which must then be dropped: pooled,
/// the next request's bytes would complete this body.
enum RequestBody {
    /// Forwarded in full per its framing (or there was none). `mirror` is
    /// `Some` if mirrors are active and the body fit within
    /// `MIRROR_BODY_MAX`; `leftover` holds bytes read past the body's end (a
    /// pipelined next request), which go back to the keepalive loop, never
    /// to the backend.
    Complete {
        mirror: Option<Vec<u8>>,
        leftover: Option<Vec<u8>>,
    },
    /// Client EOF before the framing completed.
    Truncated,
    /// Malformed chunked framing; nothing from the offending byte on was
    /// forwarded.
    Broken,
}

/// Relay the request body from client to backend, optionally teeing into a mirror buffer.
///
/// `chunked` is the observer pre-seeded with the initial-read body tail (built
/// by the caller when the request is chunk-framed); it may already be Broken.
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
) -> Result<RequestBody> {
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
                return Ok(RequestBody::Truncated);
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
        while !stream.is_terminated() && !stream.is_broken() {
            let n = idle_guarded(idle, client.read(&mut buf[..])).await?;
            if n == 0 {
                return Ok(RequestBody::Truncated);
            }
            let consumed = stream.observe(&buf[..n]);
            // The exchange is abandoned: even the valid prefix would only
            // feed a backend conn about to be dropped.
            if stream.is_broken() {
                break;
            }
            idle_guarded(idle, backend.write_all(&buf[..consumed])).await?;
            tee_mirror_chunk(&mut mirror_body, &mut overflow, &buf[..consumed]);
            if consumed < n {
                leftover = Some(buf[consumed..n].to_vec());
            }
        }
        if stream.is_broken() {
            return Ok(RequestBody::Broken);
        }
    }

    Ok(RequestBody::Complete {
        mirror: if overflow { None } else { mirror_body },
        leftover,
    })
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
/// when to stop reading from the backend, and whether its EOF ends the body or
/// truncates it.
struct ResponseFraming {
    /// Response status code (0 when the status line is unparseable).
    status: u16,
    /// The backend announced it will close this connection (Connection:
    /// close, or an HTTP/1.0 response without keep-alive). Such a conn must
    /// not return to the pool: the next write would fail and burn the retry.
    backend_close: bool,
    /// The head carries Transfer-Encoding, which overrides Content-Length:
    /// the sanitizer then drops every Content-Length line.
    transfer_encoded: bool,
    /// `Some` iff the body is framed by Content-Length.
    content_length: Option<usize>,
    no_body: bool,
    /// Body bytes (within the declared framing) that arrived in the same
    /// backend read that completed the response headers. The header-forwarding
    /// phase writes exactly this many tail bytes to the client, and it seeds
    /// the body-phase byte counter.
    body_bytes_already_forwarded: usize,
    /// `Some` iff the final transfer coding is chunked. Pre-seeded
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

impl ResponseFraming {
    /// Only the backend's close ends this body, so the client connection
    /// closes after it.
    fn close_delimited(&self) -> bool {
        !self.no_body && self.content_length.is_none() && self.chunked.is_none()
    }
}

/// Cap of bytes accumulated while waiting for backend response headers. Protects
/// against a backend that never finishes its header block. pub(crate) so the
/// h2 bridge sizes its own response-head accumulation from the same limit
/// instead of a copy that could drift below it.
pub(crate) const RESPONSE_HEADER_CAP: usize = 65536;

/// Interim (1xx) responses tolerated per exchange before the backend is
/// declared broken. Generous: a compliant exchange has at most a couple
/// (100, 103), the cap only stops an infinite interim stream.
const MAX_INTERIM_RESPONSES: u32 = 8;

/// Phase 1: read response headers from the backend into `header_buf`. Performs
/// NO writes to the client; aborting/timeout/error before this returns leaves the
/// client wire untouched, so the caller can still emit a clean `504`.
/// Returns where the final (non-interim) head ends: `header_buf` then holds
/// `[headers..header_end][same-read body tail...]`.
async fn read_response_head(backend: &mut Connection, header_buf: &mut Vec<u8>) -> Result<usize> {
    header_buf.clear();
    let mut interim_seen = 0u32;

    loop {
        // Check before reading: a swallowed interim response leaves the next
        // response's bytes already buffered.
        if let Some(header_end) = find_header_end(header_buf) {
            // A malformed status line is returned as final, for the caller
            // to refuse.
            let status = head_status(&header_buf[..header_end]).unwrap_or(0);

            // Interim responses are proxy-consumed: the proxy answers
            // 100-continue itself (Expect never reaches the backend), and
            // relaying other 1xx mid-exchange would confuse clients that
            // already got a 100. 1xx has no body, so everything past the
            // block belongs to the next response. 101 is not interim here:
            // the caller dispatches it (upgrade accept or reject).
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
            return Ok(header_end);
        }

        if header_buf.len() > RESPONSE_HEADER_CAP {
            return Err(anyhow::anyhow!("response headers exceed buffer size"));
        }

        // read_buf with little spare capacity degrades to tiny reads; keep a
        // full read's worth reserved (and spare > 0 makes Ok(0) mean EOF).
        header_buf.reserve(16 * 1024);
        let n = backend.read_buf(header_buf).await?;
        if n == 0 {
            return Err(anyhow::anyhow!(
                "backend closed before sending response headers"
            ));
        }
    }
}

/// Framing of the final head in `header_buf[..header_end]`, including how
/// much of the same-read tail belongs to its body. `None` when the head is
/// malformed (see `parse_response_head`): none of it may reach the client.
fn frame_response(
    header_buf: &[u8],
    header_end: usize,
    is_head_request: bool,
    expect_upgrade: bool,
) -> Option<ResponseFraming> {
    let parsed = parse_response_head(&header_buf[..header_end])?;
    let status = parsed.status;
    // RFC 7230 3.3: responses to HEAD have no message body regardless
    // of the response's Content-Length / Transfer-Encoding. Forcing
    // no_body here keeps `forward_response_body` from blocking on
    // bytes the backend correctly never sends.
    let no_body = response_has_no_body(status, is_head_request);
    let mut framing = ResponseFraming {
        status,
        backend_close: parsed.backend_close,
        transfer_encoded: parsed.transfer_encoded,
        content_length: None,
        no_body,
        body_bytes_already_forwarded: 0,
        chunked: None,
        overshoot: false,
    };

    // Decide how many of the tail bytes (same read as the headers)
    // belong to THIS response's body. Anything beyond the declared
    // framing is overshoot: dropped, and the conn is not poolable.
    let tail = &header_buf[header_end..];
    if is_head_request {
        // HEAD: any body bytes a non-compliant backend sent are
        // dropped (never forwarded per RFC 7230 3.3); the conn is
        // already never pooled after HEAD.
    } else if status == 101 && expect_upgrade {
        // Accepted upgrade: bytes after the 101 header block are
        // tunnel payload that arrived early: they must reach the
        // client, not be dropped as no-body overshoot.
        framing.body_bytes_already_forwarded = tail.len();
    } else if no_body {
        // 204/304/1xx: no body allowed; any tail is overshoot.
        framing.overshoot = !tail.is_empty();
    } else {
        match parsed.body {
            BodyFraming::Chunked => {
                let mut stream = ChunkedStream::new();
                let consumed = if tail.is_empty() {
                    0
                } else {
                    stream.observe(tail)
                };
                // A Broken stream also stops short of the tail; that is no
                // overshoot, and the body phase resets the client on it.
                framing.overshoot = stream.is_terminated() && consumed < tail.len();
                framing.body_bytes_already_forwarded = consumed;
                framing.chunked = Some(stream);
            }
            BodyFraming::Length(cl) => {
                framing.body_bytes_already_forwarded = tail.len().min(cl);
                framing.overshoot = tail.len() > cl;
                framing.content_length = Some(cl);
            }
            BodyFraming::Close => framing.body_bytes_already_forwarded = tail.len(),
        }
    }
    Some(framing)
}

/// How the response body ended.
enum BodyEnd {
    /// The declared framing completed (or there was no body).
    Complete,
    /// Close-delimited body ended by the backend's EOF.
    Closed,
    /// Backend EOF before the declared length or chunked terminator.
    Truncated,
    /// Malformed chunked framing; nothing from the offending byte on was
    /// forwarded.
    Broken,
}

/// Phase 2 — stream response body to the client. Hot path: read → write → check
/// framing terminator. Only `Complete` leaves both connections reusable.
async fn forward_response_body(
    backend: &mut Connection,
    client: &mut Connection,
    buf: &mut [u8],
    framing: &mut ResponseFraming,
    idle: Option<Duration>,
) -> Result<BodyEnd> {
    if framing.no_body {
        return Ok(BodyEnd::Complete);
    }
    if let Some(cl) = framing.content_length {
        if framing.body_bytes_already_forwarded >= cl {
            return Ok(BodyEnd::Complete);
        }
    }
    if let Some(stream) = &framing.chunked {
        if stream.is_terminated() {
            return Ok(BodyEnd::Complete);
        }
        // Broken by the bytes that arrived with the headers.
        if stream.is_broken() {
            return Ok(BodyEnd::Broken);
        }
    }

    let mut body_bytes_forwarded = framing.body_bytes_already_forwarded;
    loop {
        let n = idle_guarded(idle, backend.read(buf)).await?;
        if n == 0 {
            return Ok(if framing.close_delimited() {
                BodyEnd::Closed
            } else {
                BodyEnd::Truncated
            });
        }

        // Forward only bytes within the declared framing; anything past the
        // boundary is backend overshoot — dropped, and flagged so the caller
        // never pools the connection.
        if let Some(stream) = &mut framing.chunked {
            let consumed = stream.observe(&buf[..n]);
            // The client is about to be reset: even the valid prefix could
            // not reach it as anything but a broken response.
            if stream.is_broken() {
                return Ok(BodyEnd::Broken);
            }
            idle_guarded(idle, client.write_all(&buf[..consumed])).await?;
            if stream.is_terminated() {
                framing.overshoot |= consumed < n;
                return Ok(BodyEnd::Complete);
            }
        } else if let Some(cl) = framing.content_length {
            let to_write = n.min(cl - body_bytes_forwarded);
            idle_guarded(idle, client.write_all(&buf[..to_write])).await?;
            body_bytes_forwarded += to_write;
            if body_bytes_forwarded >= cl {
                framing.overshoot |= to_write < n;
                return Ok(BodyEnd::Complete);
            }
        } else {
            // EOF-framed: everything until close is body.
            idle_guarded(idle, client.write_all(&buf[..n])).await?;
        }
    }
}

/// Make dropping `client` send RST instead of FIN, for a response cut short
/// after its headers went out. A FIN would pass a close-delimited body off as
/// complete; a reset is an error for every framing. The h2 bridge's in-memory
/// pipe has no reset: it relies on the CL or chunked framing to spot the cut.
fn reset_on_drop(client: &Connection) {
    let tcp = match client {
        Connection::Plain { inner } => inner,
        Connection::Tls { inner } => inner.get_ref().0,
        Connection::ClientTls { .. } | Connection::Mem { .. } => return,
    };
    // Zero linger: close() discards unsent data and resets without blocking.
    let _ = socket2::SockRef::from(tcp).set_linger(Some(Duration::ZERO));
}

/// Zero means disabled in the config; the streaming loops branch on Option.
#[inline]
pub(crate) fn idle_opt(d: Duration) -> Option<Duration> {
    (!d.is_zero()).then_some(d)
}

/// Whether a response of this status carries no body (RFC 7230 3.3): HEAD
/// requests, 204, 304, and all 1xx. Shared with the h2 bridge so both
/// framing paths agree on which responses end at their headers.
#[inline]
pub(crate) fn response_has_no_body(status: u16, is_head: bool) -> bool {
    is_head || status == 204 || status == 304 || (100..200).contains(&status)
}

/// Await one streaming step under the optional per-step progress budget
/// (None = unbounded). `context` names the stalled party in the error.
/// Shared with the h2 bridge, whose steps are not io::Result-shaped.
#[inline]
pub(crate) async fn progress<T, F>(
    context: &'static str,
    idle: Option<Duration>,
    fut: F,
) -> Result<T>
where
    F: std::future::Future<Output = T>,
{
    match idle {
        Some(d) => tokio::time::timeout(d, fut)
            .await
            .map_err(|_| anyhow::anyhow!("{} stalled: no progress within {:?}", context, d)),
        None => Ok(fut.await),
    }
}

/// Await one body-streaming I/O step under the optional progress budget.
/// Only wraps the multi-read/write streaming loops — the common single-read
/// response never reaches these, so no timer is created on that path.
#[inline]
async fn idle_guarded<T, F>(idle: Option<Duration>, fut: F) -> Result<T>
where
    F: std::future::Future<Output = std::io::Result<T>>,
{
    Ok(progress("body stream", idle, fut).await??)
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
    client: TcpStream,
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

    // peek_sni consumed nothing, so the ClientHello is still on the socket:
    // the generic L4 forward replays it to the backend via the normal reads.
    client.set_nodelay(true)?;
    handle_tcp_connection(
        Connection::Plain { inner: client },
        backend_addr,
        &[],
        health,
        connect_timeout,
        "TLS passthrough",
    )
    .await
}

async fn handle_tcp_connection(
    mut client: Connection,
    backend_addr: SocketAddr,
    initial_data: &[u8],
    health: Arc<HealthRegistry>,
    connect_timeout: Duration,
    ctx: &'static str,
) -> Result<()> {
    let connect_result =
        tokio::time::timeout(connect_timeout, TcpStream::connect(backend_addr)).await;

    match connect_result {
        Ok(Ok(mut backend)) => {
            health.record_success(backend_addr);
            backend.set_nodelay(true)?;
            if !initial_data.is_empty() {
                backend.write_all(initial_data).await?;
            }
            // Errors from copy_bidirectional are expected when either side closes;
            // don't propagate — the data transfer is best-effort once started.
            let _ = tokio::io::copy_bidirectional(&mut client, &mut backend).await;
        }
        Ok(Err(e)) => {
            if health.record_failure(backend_addr) {
                HealthRegistry::spawn_probe(health, backend_addr);
            }
            return Err(anyhow::anyhow!(
                "{} connect to {} failed: {}",
                ctx,
                backend_addr,
                e
            ));
        }
        Err(_) => {
            if health.record_failure(backend_addr) {
                HealthRegistry::spawn_probe(health, backend_addr);
            }
            return Err(anyhow::anyhow!(
                "{} connect to {} timed out",
                ctx,
                backend_addr
            ));
        }
    }
    Ok(())
}

/// Answer a request the proxy handles itself (redirect, 404, 429, 500, 503)
/// and access-log it. Returns whether the connection stays open; the reply
/// announces `Connection: close` exactly when it does not. Staying open
/// matters most for 429: a throttled client's next request would otherwise
/// cost a fresh TLS handshake, more of our CPU than the refused request.
async fn send_local_reply(
    client: &mut Connection,
    reply: &LocalReply,
    access: Option<Box<crate::access_log::AccessCapture>>,
) -> Result<bool> {
    let location = reply.location.as_deref();
    let bytes = local_reply_bytes(reply.status, location, !reply.reusable, reply.head);
    client.write_all(&bytes).await?;
    client.flush().await?;
    crate::access_log::emit(access, reply.status, None);
    Ok(reply.reusable)
}

/// Bytes past a bodyless request's head: the next pipelined request, which
/// the keepalive loop replays after a local reply.
fn pipelined_tail(request: &[u8]) -> Option<Vec<u8>> {
    let head_end = find_header_end(request)?;
    (head_end < request.len()).then(|| request[head_end..].to_vec())
}

/// Error reply after which the connection always closes (400, 408, 431,
/// 502, 504): what remains of the request on the wire is unknown or unread.
async fn send_error_response(client: &mut Connection, error_code: u16) -> Result<()> {
    client
        .write_all(&local_reply_bytes(error_code, None, true, false))
        .await?;
    Ok(())
}

/// A response the proxy writes itself. Only redirects, which carry their
/// Location, are built per call: 429 under a crawl and 502/504 storms
/// answer every request, so their bytes are statics.
fn local_reply_bytes(
    status: u16,
    location: Option<&str>,
    close: bool,
    head: bool,
) -> Cow<'static, [u8]> {
    let fixed = match location {
        None => static_reply(status),
        Some(_) => None,
    };
    match fixed {
        Some(reply) => Cow::Borrowed(reply.bytes(close, head)),
        None => Cow::Owned(build_local_reply(status, location, close, head)),
    }
}

/// One fixed status in both connection modes. The body ends the bytes, so
/// the HEAD form is the same slice minus `body_len`.
struct FixedReply {
    keep: &'static [u8],
    close: &'static [u8],
    body_len: usize,
}

impl FixedReply {
    fn bytes(&self, close: bool, head: bool) -> &'static [u8] {
        let full = if close { self.close } else { self.keep };
        if head {
            &full[..full.len() - self.body_len]
        } else {
            full
        }
    }
}

/// `$len` is the body's length, counted by hand: the test
/// `fixed_replies_match_the_builder` holds every expansion to
/// `build_local_reply`, byte for byte.
macro_rules! fixed_reply {
    ($status:literal, $reason:literal, $len:literal $(, $extra:literal)?) => {
        FixedReply {
            keep: concat!(
                "HTTP/1.1 ", $status, " ", $reason, "\r\n", $($extra,)?
                "Content-Length: ", $len, "\r\n\r\n", $status, " ", $reason
            )
            .as_bytes(),
            close: concat!(
                "HTTP/1.1 ", $status, " ", $reason, "\r\n", $($extra,)?
                "Content-Length: ", $len, "\r\nConnection: close\r\n\r\n", $status, " ", $reason
            )
            .as_bytes(),
            body_len: $len,
        }
    };
}

static BAD_REQUEST: FixedReply = fixed_reply!(400, "Bad Request", 15);
static NOT_FOUND: FixedReply = fixed_reply!(404, "Not Found", 13);
static REQUEST_TIMEOUT: FixedReply = fixed_reply!(408, "Request Timeout", 19);
static TOO_MANY_REQUESTS: FixedReply =
    fixed_reply!(429, "Too Many Requests", 21, "Retry-After: 1\r\n");
static HEADERS_TOO_LARGE: FixedReply = fixed_reply!(431, "Request Header Fields Too Large", 35);
static INTERNAL_SERVER_ERROR: FixedReply = fixed_reply!(500, "Internal Server Error", 25);
static BAD_GATEWAY: FixedReply = fixed_reply!(502, "Bad Gateway", 15);
static SERVICE_UNAVAILABLE: FixedReply = fixed_reply!(503, "Service Unavailable", 23);
static GATEWAY_TIMEOUT: FixedReply = fixed_reply!(504, "Gateway Timeout", 19);

/// The static for a reply without Location; `None` for a redirect status,
/// which the builder keeps. Other statuses answer 500, as the builder does.
fn static_reply(status: u16) -> Option<&'static FixedReply> {
    Some(match status {
        301 | 302 | 303 | 307 | 308 => return None,
        400 => &BAD_REQUEST,
        404 => &NOT_FOUND,
        408 => &REQUEST_TIMEOUT,
        429 => &TOO_MANY_REQUESTS,
        431 => &HEADERS_TOO_LARGE,
        502 => &BAD_GATEWAY,
        503 => &SERVICE_UNAVAILABLE,
        504 => &GATEWAY_TIMEOUT,
        _ => &INTERNAL_SERVER_ERROR,
    })
}

/// Serialize a response the proxy writes itself; the body repeats the
/// status line ("404 Not Found") and is left out for HEAD.
fn build_local_reply(status: u16, location: Option<&str>, close: bool, head: bool) -> Vec<u8> {
    use std::io::Write;

    let (status, reason) = match status {
        301 => (301, "Moved Permanently"),
        302 => (302, "Found"),
        303 => (303, "See Other"),
        307 => (307, "Temporary Redirect"),
        308 => (308, "Permanent Redirect"),
        400 => (400, "Bad Request"),
        404 => (404, "Not Found"),
        408 => (408, "Request Timeout"),
        429 => (429, "Too Many Requests"),
        431 => (431, "Request Header Fields Too Large"),
        502 => (502, "Bad Gateway"),
        503 => (503, "Service Unavailable"),
        504 => (504, "Gateway Timeout"),
        other if location.is_some() => (other, "Found"),
        _ => (500, "Internal Server Error"),
    };
    let body = format!("{} {}", status, reason);
    let mut resp = Vec::with_capacity(128 + location.map_or(0, str::len));
    let _ = write!(resp, "HTTP/1.1 {} {}\r\n", status, reason);
    if let Some(location) = location {
        let _ = write!(resp, "Location: {}\r\n", location);
    }
    if status == 429 {
        resp.extend_from_slice(b"Retry-After: 1\r\n");
    }
    let _ = write!(resp, "Content-Length: {}\r\n", body.len());
    if close {
        resp.extend_from_slice(b"Connection: close\r\n");
    }
    resp.extend_from_slice(b"\r\n");
    if !head {
        resp.extend_from_slice(body.as_bytes());
    }
    resp
}

/// One client connection driven through `handle_connection` over an
/// in-memory duplex, against real loopback backends.
#[cfg(test)]
mod connection_tests {
    use super::*;
    use crate::routing::{Backend, HttpHeader, PathMatchType, URLRewritePath};
    use tokio::io::DuplexStream;
    use tokio::sync::mpsc::UnboundedReceiver;

    const REFUSED: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nno";
    const SWITCHED: &[u8] =
        b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n";
    const UPGRADE: &[u8] =
        b"GET /ws HTTP/1.1\r\nHost: a.test\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n";

    /// Backend answering the first request head of each connection with
    /// `reply`; yields all bytes a connection delivered once it closes.
    async fn backend(reply: &[u8]) -> (SocketAddr, UnboundedReceiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let reply = reply.to_vec();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let (tx, reply) = (tx.clone(), reply.clone());
                tokio::spawn(async move {
                    let (mut seen, mut chunk, mut replied) = (Vec::new(), [0u8; 4096], false);
                    while let Ok(n @ 1..) = sock.read(&mut chunk).await {
                        seen.extend_from_slice(&chunk[..n]);
                        if !replied && find_header_end(&seen).is_some() {
                            replied = true;
                            if sock.write_all(&reply).await.is_err() {
                                break;
                            }
                        }
                    }
                    let _ = tx.send(seen);
                });
            }
        });
        (addr, rx)
    }

    /// Backend answering the first request head of each connection with
    /// `reply`, then closing: the end of a close-delimited body.
    async fn closing_backend(reply: &[u8]) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let reply = reply.to_vec();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let reply = reply.clone();
                tokio::spawn(async move {
                    let (mut seen, mut chunk) = (Vec::new(), [0u8; 4096]);
                    while let Ok(n @ 1..) = sock.read(&mut chunk).await {
                        seen.extend_from_slice(&chunk[..n]);
                        if find_header_end(&seen).is_some() {
                            let _ = sock.write_all(&reply).await;
                            return;
                        }
                    }
                });
            }
        });
        addr
    }

    /// a.test proxies to `backend`; r.test redirects to https.
    fn routes(backend: SocketAddr) -> RouteTable {
        let rule = |filters, backends| {
            HttpRouteRule::new(
                PathMatchType::Prefix,
                "/".to_string(),
                vec![],
                vec![],
                filters,
                backends,
            )
        };
        let mut rt = RouteTable::new();
        rt.add_http_route(
            "a.test",
            rule(
                vec![],
                vec![Backend {
                    socket_addr: backend,
                    weight: 1,
                    filters: vec![],
                    use_tls: false,
                    server_name: String::new(),
                }],
            ),
        );
        rt.add_http_route(
            "r.test",
            rule(
                vec![HttpFilter::RequestRedirect {
                    scheme: Some("https".to_string()),
                    hostname: None,
                    port: None,
                    path: None,
                    status_code: 301,
                }],
                vec![],
            ),
        );
        rt
    }

    fn conn_state(rt: RouteTable, is_tls: bool) -> ConnectionState {
        // The pool's TLS client config needs one, even for plain backends.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let cfg = WorkerConfig {
            max_idle_per_backend: 4,
            connect_timeout: Duration::from_secs(2),
            client_header_timeout: Duration::from_secs(2),
            tcp_keepalive_time: Duration::from_secs(60),
            idle_body_timeout: Duration::from_secs(5),
            http2: false,
        };
        ConnectionState::new(
            8080,
            Arc::new(ArcSwap::from_pointee(rt)),
            make_pool(&None, &cfg),
            Arc::new(BackendSelector::new()),
            Arc::new(HealthRegistry::new()),
            is_tls,
            "127.0.0.1".parse().unwrap(),
            &cfg,
            Arc::new(ListenerStats::default()),
        )
    }

    fn serve(rt: RouteTable) -> (DuplexStream, tokio::task::JoinHandle<Result<()>>) {
        let (client, engine) = tokio::io::duplex(64 * 1024);
        let peer = "127.0.0.1:40000".parse().unwrap();
        let task = tokio::spawn(handle_connection(
            Connection::Mem { inner: engine },
            peer,
            conn_state(rt, false),
        ));
        (client, task)
    }

    /// Send `input`, half-close, and collect everything until the engine
    /// closes the connection.
    async fn exchange(client: &mut DuplexStream, input: &[u8]) -> String {
        client.write_all(input).await.unwrap();
        client.shutdown().await.unwrap();
        let mut out = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut out))
            .await
            .expect("engine never closed the connection")
            .unwrap();
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Everything until the engine closes, without half-closing first: the
    /// close must be the engine's own decision.
    async fn read_until_closed(client: &mut DuplexStream) -> String {
        let mut out = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut out))
            .await
            .expect("engine never closed the connection")
            .unwrap();
        String::from_utf8_lossy(&out).into_owned()
    }

    async fn delivered(rx: &mut UnboundedReceiver<Vec<u8>>) -> String {
        let seen = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("backend connection never closed")
            .unwrap();
        String::from_utf8_lossy(&seen).into_owned()
    }

    /// One prefix rule on `host`, with `filters`, proxying to `backend`.
    fn filtered_route(
        host: &str,
        prefix: &str,
        filters: Vec<HttpFilter>,
        backend: SocketAddr,
    ) -> RouteTable {
        let mut rt = RouteTable::new();
        rt.add_http_route(
            host,
            HttpRouteRule::new(
                PathMatchType::Prefix,
                prefix.to_string(),
                vec![],
                vec![],
                filters,
                vec![Backend {
                    socket_addr: backend,
                    weight: 1,
                    filters: vec![],
                    use_tls: false,
                    server_name: String::new(),
                }],
            ),
        );
        rt
    }

    /// Sends `request` through `rt` (backend answering REFUSED) and returns
    /// the head the backend received, after checking it has one Host line.
    async fn forwarded_head(rt: impl FnOnce(SocketAddr) -> RouteTable, request: &str) -> String {
        let (addr, mut rx) = backend(REFUSED).await;
        let (mut client, _task) = serve(rt(addr));
        let out = exchange(&mut client, request.as_bytes()).await;
        assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "{request:?}: {out}");
        let seen = delivered(&mut rx).await;
        let hosts = seen.to_ascii_lowercase().matches("\r\nhost:").count();
        assert_eq!(hosts, 1, "{seen}");
        seen
    }

    /// RFC 9112 3.2.1: the backend is an origin server, which is sent
    /// origin-form; 3.2.2: the target's authority goes to Host.
    #[tokio::test]
    async fn absolute_form_reaches_the_backend_as_origin_form() {
        for (request, line, host) in [
            (
                "GET http://a.test:8080/p?q=1 HTTP/1.1\r\nHost: a.test:8080\r\n\r\n",
                "GET /p?q=1 HTTP/1.1\r\n",
                "a.test:8080",
            ),
            (
                "GET HTTP://a.test/p HTTP/1.1\r\nHost: a.test\r\n\r\n",
                "GET /p HTTP/1.1\r\n",
                "a.test",
            ),
            (
                "GET http://a.test HTTP/1.1\r\nHost: a.test\r\n\r\n",
                "GET / HTTP/1.1\r\n",
                "a.test",
            ),
            // The target's port wins over a Host that leaves it out.
            (
                "GET http://a.test:8080/p HTTP/1.1\r\nHost: a.test\r\n\r\n",
                "GET /p HTTP/1.1\r\n",
                "a.test:8080",
            ),
            (
                "GET http://a.test/p?q HTTP/1.0\r\n\r\n",
                "GET /p?q HTTP/1.0\r\n",
                "a.test",
            ),
        ] {
            let seen = forwarded_head(routes, request).await;
            assert!(seen.starts_with(line), "{request:?}: {seen}");
            assert!(seen.contains(&format!("\r\nHost: {host}\r\n")), "{seen}");
        }
    }

    #[tokio::test]
    async fn absolute_form_takes_a_hostname_rewrite() {
        let rewrite = vec![HttpFilter::URLRewrite {
            hostname: Some("internal.example".to_string()),
            path: None,
        }];
        let seen = forwarded_head(
            |addr| filtered_route("w.test", "/", rewrite, addr),
            "GET http://w.test/x?y HTTP/1.1\r\nHost: w.test\r\n\r\n",
        )
        .await;
        assert!(seen.starts_with("GET /x?y HTTP/1.1\r\n"), "{seen}");
        assert!(seen.contains("\r\nHost: internal.example\r\n"), "{seen}");
    }

    #[tokio::test]
    async fn absolute_form_takes_a_path_prefix_rewrite() {
        let rewrite = vec![HttpFilter::URLRewrite {
            hostname: None,
            path: Some(URLRewritePath::ReplacePrefixMatch("/v2".to_string())),
        }];
        let seen = forwarded_head(
            |addr| filtered_route("p.test", "/v1", rewrite, addr),
            "GET http://p.test:8080/v1/users HTTP/1.1\r\nHost: p.test\r\n\r\n",
        )
        .await;
        assert!(seen.starts_with("GET /v2/users HTTP/1.1\r\n"), "{seen}");
        assert!(seen.contains("\r\nHost: p.test:8080\r\n"), "{seen}");
    }

    /// The authority's Host is replaced by a `set Host`, never doubled.
    #[tokio::test]
    async fn absolute_form_takes_a_set_host() {
        let set = vec![HttpFilter::RequestHeaderModifier {
            add: Arc::new(vec![]),
            set: Arc::new(vec![HttpHeader {
                name: "Host".to_string(),
                value: "set.example".to_string(),
            }]),
            remove: Arc::new(vec![]),
        }];
        let seen = forwarded_head(
            |addr| filtered_route("s.test", "/", set, addr),
            "GET http://s.test/x HTTP/1.1\r\nHost: s.test\r\n\r\n",
        )
        .await;
        assert!(seen.starts_with("GET /x HTTP/1.1\r\n"), "{seen}");
        assert!(seen.contains("\r\nHost: set.example\r\n"), "{seen}");
    }

    #[tokio::test]
    async fn refused_upgrade_routes_the_held_tail_as_a_new_request() {
        let (addr, mut rx) = backend(REFUSED).await;
        let (mut client, _task) = serve(routes(addr));
        let mut input = UPGRADE.to_vec();
        input.extend_from_slice(
            b"POST /admin/delete HTTP/1.1\r\nHost: internal.only\r\nContent-Length: 0\r\n\r\n",
        );
        let out = exchange(&mut client, &input).await;
        assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "{out}");
        assert!(out.contains("HTTP/1.1 404 Not Found\r\n"), "{out}");
        let seen = delivered(&mut rx).await;
        assert!(seen.starts_with("GET /ws HTTP/1.1\r\n"), "{seen}");
        assert!(
            !seen.contains("admin"),
            "held tail reached the backend: {seen}"
        );
    }

    #[tokio::test]
    async fn accepted_upgrade_sends_held_bytes_before_tunnel_data() {
        let (addr, mut rx) = backend(SWITCHED).await;
        let (mut client, _task) = serve(routes(addr));
        let mut input = UPGRADE.to_vec();
        input.extend_from_slice(b"HELD");
        client.write_all(&input).await.unwrap();
        let mut head = vec![0u8; SWITCHED.len()];
        client.read_exact(&mut head).await.unwrap();
        assert_eq!(head, SWITCHED);
        let out = exchange(&mut client, b"TUNNEL").await;
        assert!(out.is_empty(), "{out}");
        let seen = delivered(&mut rx).await;
        assert!(seen.starts_with("GET /ws HTTP/1.1\r\n"), "{seen}");
        assert!(seen.ends_with("\r\n\r\nHELDTUNNEL"), "{seen}");
    }

    /// A backend nothing should reach: the test holds the listener.
    async fn idle_backend() -> TcpListener {
        TcpListener::bind("127.0.0.1:0").await.unwrap()
    }

    /// Call once the engine task has finished: any connect it made is
    /// already in the accept queue.
    async fn assert_never_connected(listener: &TcpListener) {
        let accepted = tokio::time::timeout(Duration::from_millis(100), listener.accept()).await;
        assert!(accepted.is_err(), "the proxy connected to the backend");
    }

    /// A masked WebSocket text frame ("hello"), as a client streams it.
    const WS_FRAME: &[u8] = b"\x81\x85\x37\xfa\x21\x3d\x7f\x9f\x4d\x51\x58";
    /// Not HTTP, but holding the blank line that ends a head.
    const OPAQUE: &[u8] = b"\x00\x01opaque\r\n\r\n";

    /// `routes(http)` plus a TCP route on the engine's port.
    fn routes_with_tcp(http: SocketAddr, tcp: SocketAddr) -> RouteTable {
        let mut rt = routes(http);
        rt.add_tcp_route(
            8080,
            vec![Backend {
                socket_addr: tcp,
                weight: 1,
                filters: vec![],
                use_tls: false,
                server_name: String::new(),
            }],
        );
        rt
    }

    #[tokio::test]
    async fn refused_upgrade_closes_on_held_tunnel_frames() {
        let (addr, _rx) = backend(REFUSED).await;
        let (mut client, task) = serve(routes(addr));
        let mut input = UPGRADE.to_vec();
        input.extend_from_slice(WS_FRAME);
        client.write_all(&input).await.unwrap();
        // Not a 408 after the header budget: the connection is HTTP, and
        // these bytes can never become a request.
        let out = read_until_closed(&mut client).await;
        assert_eq!(out, std::str::from_utf8(REFUSED).unwrap());
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn bytes_after_the_first_request_never_reach_tcp_routing() {
        for first in [UPGRADE, b"GET / HTTP/1.1\r\nHost: nope.test\r\n\r\n"] {
            let (addr, _rx) = backend(REFUSED).await;
            let tcp = idle_backend().await;
            let (mut client, task) = serve(routes_with_tcp(addr, tcp.local_addr().unwrap()));
            let mut input = first.to_vec();
            input.extend_from_slice(OPAQUE);
            client.write_all(&input).await.unwrap();
            let out = read_until_closed(&mut client).await;
            assert!(out.starts_with("HTTP/1.1 "), "{out}");
            task.await.unwrap().unwrap();
            assert_never_connected(&tcp).await;
        }
    }

    const POST_THEN_CRLF: &[u8] =
        b"POST / HTTP/1.1\r\nHost: a.test\r\nContent-Length: 2\r\n\r\nhi\r\n";
    const UNROUTED: &[u8] = b"GET / HTTP/1.1\r\nHost: nope.test\r\n\r\n";

    /// RFC 9112 2.2: an empty line before a request-line is ignored. Old
    /// clients end a POST body with a CRLF its Content-Length leaves out.
    #[tokio::test]
    async fn empty_lines_before_a_request_line_are_skipped() {
        for crlfs in ["", "\r\n\r\n"] {
            let (addr, _rx) = backend(REFUSED).await;
            let (mut client, _task) = serve(routes(addr));
            let input = [POST_THEN_CRLF, crlfs.as_bytes(), UNROUTED].concat();
            let out = exchange(&mut client, &input).await;
            assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "{out}");
            assert!(
                out.contains("HTTP/1.1 404 Not Found\r\n"),
                "{crlfs:?}: {out}"
            );
        }

        // The CRLF alone ends a read: the next request still gets an answer.
        let (addr, _rx) = backend(REFUSED).await;
        let (mut client, _task) = serve(routes(addr));
        client.write_all(POST_THEN_CRLF).await.unwrap();
        let mut head = vec![0u8; REFUSED.len()];
        client.read_exact(&mut head).await.unwrap();
        assert_eq!(head, REFUSED);
        let out = exchange(&mut client, UNROUTED).await;
        assert!(out.starts_with("HTTP/1.1 404 Not Found\r\n"), "{out}");
    }

    /// Only CRLF is an empty line here, never a bare LF.
    #[tokio::test]
    async fn a_bare_lf_before_a_request_line_closes() {
        let (addr, _rx) = backend(REFUSED).await;
        let (mut client, _task) = serve(routes(addr));
        let input = [
            &b"POST / HTTP/1.1\r\nHost: a.test\r\nContent-Length: 2\r\n\r\nhi\n"[..],
            UNROUTED,
        ]
        .concat();
        client.write_all(&input).await.unwrap();
        let out = read_until_closed(&mut client).await;
        assert_eq!(out, std::str::from_utf8(REFUSED).unwrap());
    }

    /// A read can end inside the method: that is still a request.
    #[tokio::test]
    async fn held_bytes_split_inside_the_method_still_route() {
        let (addr, _rx) = backend(REFUSED).await;
        let (mut client, _task) = serve(routes(addr));
        let mut input = UPGRADE.to_vec();
        input.extend_from_slice(b"GE");
        client.write_all(&input).await.unwrap();
        let mut head = vec![0u8; REFUSED.len()];
        client.read_exact(&mut head).await.unwrap();
        assert_eq!(head, REFUSED);
        let out = exchange(&mut client, b"T / HTTP/1.1\r\nHost: nope.test\r\n\r\n").await;
        assert!(out.starts_with("HTTP/1.1 404 Not Found\r\n"), "{out}");
    }

    #[tokio::test]
    async fn redirect_with_a_body_closes_without_reading_it() {
        let idle = idle_backend().await;
        let (mut client, task) = serve(routes(idle.local_addr().unwrap()));
        client
            .write_all(b"POST /x HTTP/1.1\r\nHost: r.test\r\nContent-Length: 40\r\n\r\n")
            .await
            .unwrap();
        let mut out = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut out))
            .await
            .expect("connection left open after the redirect")
            .unwrap();
        let out = String::from_utf8_lossy(&out);
        assert!(
            out.starts_with("HTTP/1.1 301 Moved Permanently\r\n"),
            "{out}"
        );
        assert!(out.contains("Connection: close\r\n"), "{out}");
        // The body, sent after the reply like a slow client's second
        // segment, finds the connection gone instead of being parsed.
        let body = b"GET /fromBody HTTP/1.1\r\nHost: a.test\r\n\r\n";
        assert!(client.write_all(body).await.is_err());
        task.await.unwrap().unwrap();
        assert_never_connected(&idle).await;
    }

    #[tokio::test]
    async fn pipelined_request_after_a_404_is_answered() {
        let (addr, mut rx) = backend(REFUSED).await;
        let (mut client, _task) = serve(routes(addr));
        let out = exchange(
            &mut client,
            b"GET / HTTP/1.1\r\nHost: nope.test\r\n\r\nGET /second HTTP/1.1\r\nHost: a.test\r\n\r\n",
        )
        .await;
        let (first, second) = out.split_at(out.find("HTTP/1.1 200").expect(&out));
        assert!(first.starts_with("HTTP/1.1 404 Not Found\r\n"), "{out}");
        assert!(!first.contains("Connection: close"), "{out}");
        assert!(second.ends_with("no"), "{out}");
        assert!(delivered(&mut rx).await.starts_with("GET /second "));
    }

    #[tokio::test]
    async fn head_cut_short_by_eof_is_dropped() {
        let idle = idle_backend().await;
        let (mut client, task) = serve(routes(idle.local_addr().unwrap()));
        let out = exchange(
            &mut client,
            b"GET /t HTTP/1.1\r\nHost: a.test\r\nX-Forwarded-For: 6.6.6.6\r\nX-Ignore: zz",
        )
        .await;
        assert!(out.is_empty(), "{out}");
        task.await.unwrap().unwrap();
        assert_never_connected(&idle).await;
    }

    #[tokio::test]
    async fn missing_host_gets_a_400_on_the_wire() {
        let (mut client, _task) = serve(RouteTable::new());
        let out = exchange(&mut client, b"GET / HTTP/1.1\r\n\r\n").await;
        assert!(out.starts_with("HTTP/1.1 400 Bad Request\r\n"), "{out}");
        assert!(out.contains("Connection: close\r\n"), "{out}");
    }

    #[tokio::test]
    async fn malformed_buffered_chunked_body_gets_a_400_without_a_backend() {
        let idle = idle_backend().await;
        let (mut client, task) = serve(routes(idle.local_addr().unwrap()));
        let out = exchange(
            &mut client,
            b"POST / HTTP/1.1\r\nHost: a.test\r\nTransfer-Encoding: chunked\r\n\r\nZ\r\nzz\r\n0\r\n\r\n",
        )
        .await;
        assert!(out.starts_with("HTTP/1.1 400 Bad Request\r\n"), "{out}");
        task.await.unwrap().unwrap();
        assert_never_connected(&idle).await;
    }

    #[tokio::test]
    async fn local_reply_to_head_leaves_no_body_on_a_kept_connection() {
        let (addr, _rx) = backend(REFUSED).await;
        let (mut client, _task) = serve(routes(addr));
        let out = exchange(
            &mut client,
            b"HEAD / HTTP/1.1\r\nHost: nope.test\r\n\r\nGET / HTTP/1.1\r\nHost: a.test\r\n\r\n",
        )
        .await;
        assert_eq!(
            out,
            "HTTP/1.1 404 Not Found\r\nContent-Length: 13\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nno"
        );
    }

    /// Codings before the final chunked do not change the framing, and the
    /// Content-Length beside them neither ends the body nor reaches the client.
    #[tokio::test]
    async fn final_chunked_coding_frames_the_response() {
        for te in [
            "Transfer-Encoding: gzip, chunked\r\n",
            "Transfer-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n",
        ] {
            let reply =
                format!("HTTP/1.1 200 OK\r\n{te}Content-Length: 3\r\n\r\n5\r\nhello\r\n0\r\n\r\n");
            let (addr, _rx) = backend(reply.as_bytes()).await;
            let (mut client, _task) = serve(routes(addr));
            let out = exchange(&mut client, b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n").await;
            assert_eq!(
                out,
                format!("HTTP/1.1 200 OK\r\n{te}\r\n5\r\nhello\r\n0\r\n\r\n"),
                "{te}"
            );
        }
    }

    /// No final chunked: only the backend's close ends the body, so the
    /// client connection closes after it and says so.
    #[tokio::test]
    async fn other_transfer_codings_are_read_to_the_backend_close() {
        for te in [
            "Transfer-Encoding: identity\r\n",
            "Transfer-Encoding: chunked, gzip\r\n",
            "Transfer-Encoding: chunked\r\nTransfer-Encoding: gzip\r\n",
        ] {
            let reply = format!("HTTP/1.1 200 OK\r\n{te}Content-Length: 2\r\n\r\nhello");
            let addr = closing_backend(reply.as_bytes()).await;
            let (mut client, task) = serve(routes(addr));
            client
                .write_all(b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\nGET /2 HTTP/1.1\r\nHost: a.test\r\n\r\n")
                .await
                .unwrap();
            let out = read_until_closed(&mut client).await;
            assert_eq!(
                out,
                format!("HTTP/1.1 200 OK\r\n{te}Connection: close\r\n\r\nhello"),
                "{te}"
            );
            task.await.unwrap().unwrap();
        }
    }

    /// Over TLS only close_notify tells a strict client (OpenSSL 3, rustls)
    /// that a close-delimited body ended rather than was cut.
    #[tokio::test]
    async fn close_delimited_body_over_tls_ends_with_close_notify() {
        let Some((cert_pem, key_pem)) = crate::proxy::tls::test_util::generate_test_cert("a.test")
        else {
            return; // openssl unavailable
        };
        let cert = crate::config::CertificateRef {
            name: "a".to_string(),
            cert_pem: Some(cert_pem),
            key_pem: Some(key_pem),
            ..Default::default()
        };
        let server_cfg =
            crate::proxy::tls::build_server_config(&[cert], std::path::Path::new("/unused"), false)
                .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_cfg));
        let addr = closing_backend(b"HTTP/1.1 200 OK\r\n\r\nall of it").await;
        let front = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let front_addr = front.local_addr().unwrap();
        let engine = tokio::spawn(async move {
            let (tcp, peer) = front.accept().await.unwrap();
            let tls = acceptor.accept(tcp).await.unwrap();
            let state = conn_state(routes(addr), true);
            handle_connection(Connection::Tls { inner: tls }, peer, state).await
        });

        let client_cfg = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(crate::proxy::backend_pool::NoVerifier))
            .with_no_client_auth();
        let tcp = TcpStream::connect(front_addr).await.unwrap();
        let sni = rustls::pki_types::ServerName::try_from("a.test").unwrap();
        let mut tls = tokio_rustls::TlsConnector::from(Arc::new(client_cfg))
            .connect(sni, tcp)
            .await
            .unwrap();
        tls.write_all(b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n")
            .await
            .unwrap();
        let mut out = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(5), tls.read_to_end(&mut out))
            .await
            .expect("engine never closed the connection");
        let out = String::from_utf8_lossy(&out);
        assert!(read.is_ok(), "{read:?} after {out:?}");
        assert!(out.ends_with("Connection: close\r\n\r\nall of it"), "{out}");
        engine.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn malformed_backend_content_length_gets_a_502() {
        for cl in [
            "Content-Length: 5, 5\r\n",
            "Content-Length: +5\r\n",
            "Content-Length: 5\r\nContent-Length: 6\r\n",
            "Content-Length: \r\n",
            "Content-Length: 0x5\r\n",
            "Content-Length: 99999999999999999999999\r\n",
        ] {
            let reply = format!(
                "HTTP/1.1 200 OK\r\n{cl}\r\nhelloHTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nfake"
            );
            let addr = closing_backend(reply.as_bytes()).await;
            let (mut client, _task) = serve(routes(addr));
            let out = exchange(&mut client, b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n").await;
            assert_eq!(
                out,
                "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 15\r\nConnection: close\r\n\r\n502 Bad Gateway",
                "{cl}"
            );
        }

        // Repeated lines agreeing on one value frame by it, and reach the
        // client as one: some clients (llhttp) refuse any repeat.
        let addr = closing_backend(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-A: 1\r\ncontent-length: 05\r\n\r\nhello",
        )
        .await;
        let (mut client, _task) = serve(routes(addr));
        let out = exchange(&mut client, b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n").await;
        assert_eq!(
            out,
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-A: 1\r\n\r\nhello"
        );
    }

    /// Framing hangs on the status: read as 204, `2040` dropped the body
    /// a lenient client then waited for.
    #[tokio::test]
    async fn malformed_backend_status_line_gets_a_502() {
        for status_line in [
            "HTTP/1.1 2040 OK",
            "HTTP/1.1 20 OK",
            "HTTP/1.1  200 OK",
            "HTTP/1.1 200\tOK",
            "http/1.1 200 OK",
            "HTTP/2 200 OK",
            "HTTP/1.1 abc OK",
            "ICY 200 OK",
        ] {
            let reply = format!("{status_line}\r\nContent-Length: 5\r\n\r\nhello");
            let addr = closing_backend(reply.as_bytes()).await;
            let (mut client, _task) = serve(routes(addr));
            let out = exchange(&mut client, b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n").await;
            assert_eq!(
                out,
                "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 15\r\nConnection: close\r\n\r\n502 Bad Gateway",
                "{status_line}"
            );
        }
        // A header line without a field name fails the same way.
        let addr = closing_backend(b"HTTP/1.1 200 OK\r\nContent-Length 5\r\n\r\nhello").await;
        let (mut client, _task) = serve(routes(addr));
        let out = exchange(&mut client, b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n").await;
        assert!(out.starts_with("HTTP/1.1 502 Bad Gateway\r\n"), "{out}");

        // A reason phrase is optional, its SP too.
        for status_line in ["HTTP/1.1 200", "HTTP/1.1 200 ", "HTTP/1.0 200 OK"] {
            let reply = format!("{status_line}\r\nContent-Length: 5\r\n\r\nhello");
            let addr = closing_backend(reply.as_bytes()).await;
            let (mut client, _task) = serve(routes(addr));
            let out = exchange(&mut client, b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n").await;
            assert!(out.starts_with(&format!("{status_line}\r\n")), "{out}");
            assert!(out.ends_with("\r\n\r\nhello"), "{out}");
        }
    }

    #[test]
    fn local_reply_announces_close_exactly_when_closing() {
        let bytes = |status, location, close, head| {
            String::from_utf8(local_reply_bytes(status, location, close, head).into_owned())
                .unwrap()
        };
        let keep = bytes(429, None, false, false);
        assert!(keep.starts_with("HTTP/1.1 429 Too Many Requests\r\n"));
        assert!(keep.contains("Retry-After: 1\r\n"));
        assert!(!keep.contains("Connection:"));
        assert!(keep.ends_with("\r\nContent-Length: 21\r\n\r\n429 Too Many Requests"));

        let close = bytes(404, None, true, false);
        assert!(close.ends_with("Connection: close\r\n\r\n404 Not Found"));

        let redirect = bytes(308, Some("https://a/"), false, false);
        assert!(redirect.contains("Location: https://a/\r\n"));
        assert!(redirect.ends_with("\r\n\r\n308 Permanent Redirect"));

        let head = bytes(301, Some("https://a/"), false, true);
        assert!(head.ends_with("Content-Length: 21\r\n\r\n"), "{head}");

        let unknown = bytes(418, None, true, false);
        assert!(unknown.starts_with("HTTP/1.1 500 Internal Server Error\r\n"));
    }

    #[test]
    fn fixed_replies_match_the_builder() {
        for status in [400, 404, 408, 429, 431, 500, 502, 503, 504, 418] {
            for (close, head) in [(false, false), (false, true), (true, false), (true, true)] {
                let reply = local_reply_bytes(status, None, close, head);
                assert!(matches!(reply, Cow::Borrowed(_)), "{status} allocated");
                assert_eq!(
                    String::from_utf8_lossy(&reply),
                    String::from_utf8_lossy(&build_local_reply(status, None, close, head)),
                    "{status} close={close} head={head}"
                );
            }
        }
        for (status, location) in [(301, Some("https://a/")), (302, None)] {
            let reply = local_reply_bytes(status, location, false, false);
            assert!(matches!(reply, Cow::Owned(_)));
            assert_eq!(*reply, build_local_reply(status, location, false, false));
        }
    }
}

// --- HTTP response parsing helpers ---

/// How a response body is delimited (RFC 9112 6.3), absent a status or
/// request method that rules out a body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BodyFraming {
    Length(usize),
    /// The final transfer coding is chunked; codings before it do not
    /// change where the body ends.
    Chunked,
    /// Ends at the backend's close: neither header, or a Transfer-Encoding
    /// whose final coding is not chunked.
    Close,
}

/// The one reading of a response head that the body relay, the sanitizer
/// and the h2 bridge all follow: two readings could disagree on where the
/// body ends, and the client would take the rest as the next response.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ResponseHead {
    pub(crate) status: u16,
    pub(crate) body: BodyFraming,
    /// Any Transfer-Encoding overrides Content-Length, which must then not
    /// be forwarded either.
    pub(crate) transfer_encoded: bool,
    /// A transfer coding besides one lone chunked: the body stays coded
    /// once de-chunked, which only HTTP/1.1 can declare.
    pub(crate) other_codings: bool,
    /// An explicit Connection: close, or HTTP/1.0 without keep-alive.
    pub(crate) backend_close: bool,
}

/// `None` when a recipient could frame the head differently than this
/// reading: a status line other than `HTTP/1.x SP 3DIGIT [SP reason]`, a
/// Content-Length other than one `1*DIGIT` value (repeated lines must
/// agree), Transfer-Encoding on HTTP/1.0, or a line a client could split or
/// name otherwise (bare CR, no colon, a field name that is no token, which
/// covers obs-fold). Lines are split exactly as the sanitizer splits them.
pub(crate) fn parse_response_head(head: &[u8]) -> Option<ResponseHead> {
    let mut lines = header_lines(head);
    let (status, is_http10) = parse_status_line(lines.next()?)?;
    let mut content_length: Option<usize> = None;
    let (mut transfer_encoded, mut chunked_last, mut codings) = (false, false, 0u32);
    let (mut close, mut keep_alive) = (false, false);

    for line in lines {
        if memchr::memchr(b'\r', line).is_some() {
            return None;
        }
        let colon = memchr::memchr(b':', line)?;
        let (name, value) = (&line[..colon], &line[colon + 1..]);
        if !is_token(name) {
            return None;
        }
        if name.eq_ignore_ascii_case(b"content-length") {
            let n = parse_digits(trim_ows(value))?;
            if content_length.is_some_and(|seen| seen != n) {
                return None;
            }
            content_length = Some(n);
        } else if name.eq_ignore_ascii_case(b"transfer-encoding") {
            transfer_encoded = true;
            for coding in value.split(|&b| b == b',').map(trim_ows) {
                if !coding.is_empty() {
                    codings += 1;
                    chunked_last = coding.eq_ignore_ascii_case(b"chunked");
                }
            }
        } else if name.eq_ignore_ascii_case(b"connection") {
            for token in value.split(|&b| b == b',').map(trim_ows) {
                close |= token.eq_ignore_ascii_case(b"close");
                keep_alive |= token.eq_ignore_ascii_case(b"keep-alive");
            }
        }
    }

    // RFC 9112 6.1: HTTP/1.0 has no transfer codings, so its framing is
    // faulty whatever the rest of the head says.
    if transfer_encoded && is_http10 {
        return None;
    }
    let body = match (transfer_encoded, content_length) {
        (true, _) if chunked_last => BodyFraming::Chunked,
        (true, _) | (false, None) => BodyFraming::Close,
        (false, Some(n)) => BodyFraming::Length(n),
    };
    Some(ResponseHead {
        status,
        body,
        transfer_encoded,
        other_codings: transfer_encoded && !(codings == 1 && chunked_last),
        backend_close: close || (is_http10 && !keep_alive),
    })
}

/// The status code and whether the version is HTTP/1.0, from a status line
/// without its CRLF. `HTTP/1.x SP 3DIGIT`, then nothing or SP and a reason.
/// No code below 100: a client may read one as interim, so bodiless.
fn parse_status_line(line: &[u8]) -> Option<(u16, bool)> {
    let [b'H', b'T', b'T', b'P', b'/', b'1', b'.', minor, b' ', d0 @ b'1'..=b'9', d1, d2, reason @ ..] =
        line
    else {
        return None;
    };
    if !(minor.is_ascii_digit() && d1.is_ascii_digit() && d2.is_ascii_digit())
        || !matches!(reason, [] | [b' ', ..])
        || memchr::memchr(b'\r', reason).is_some()
    {
        return None;
    }
    let status = u16::from(d0 - b'0') * 100 + u16::from(d1 - b'0') * 10 + u16::from(d2 - b'0');
    Some((status, *minor == b'0'))
}

fn head_status(head: &[u8]) -> Option<u16> {
    parse_status_line(header_lines(head).next()?).map(|(status, _)| status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_conn_pooling_policy() {
        // A complete, cleanly-framed non-HEAD response is poolable.
        assert!(backend_conn_poolable(true, false, false));
        // HEAD is never pooled: a non-compliant backend body would desync the
        // conn, and we can't tell that apart from a compliant Content-Length at
        // header time without blocking.
        assert!(!backend_conn_poolable(true, true, false));
        // A refused upgrade is never pooled: the backend may have left HTTP.
        assert!(!backend_conn_poolable(true, false, true));
        // Overshoot is never pooled, HEAD or not.
        assert!(!backend_conn_poolable(false, false, false));
        assert!(!backend_conn_poolable(false, true, false));
    }

    /// An engine-side connection plus the peer end a test drives.
    fn mem_pair() -> (Connection, tokio::io::DuplexStream) {
        let (engine, peer) = tokio::io::duplex(64 * 1024);
        (Connection::Mem { inner: engine }, peer)
    }

    async fn relay(
        client: &mut Connection,
        backend: &mut Connection,
        content_length: Option<usize>,
        chunked: Option<ChunkedStream>,
    ) -> RequestBody {
        let mut buf = vec![0u8; 4096];
        relay_request_body(
            client,
            backend,
            &mut buf,
            0,
            0,
            content_length,
            chunked,
            false,
            None,
        )
        .await
        .unwrap()
    }

    /// Pooled, a conn holding a partial body would take the next request's
    /// bytes as the rest of it.
    #[tokio::test]
    async fn request_body_client_eof_is_truncated() {
        let (mut client, mut client_peer) = mem_pair();
        let (mut backend, _backend_peer) = mem_pair();
        client_peer.write_all(b"hel").await.unwrap();
        drop(client_peer);
        let out = relay(&mut client, &mut backend, Some(10), None).await;
        assert!(matches!(out, RequestBody::Truncated));

        let (mut client, mut client_peer) = mem_pair();
        client_peer.write_all(b"5\r\nhello\r\n").await.unwrap();
        drop(client_peer);
        let out = relay(&mut client, &mut backend, None, Some(ChunkedStream::new())).await;
        assert!(matches!(out, RequestBody::Truncated));
    }

    #[tokio::test]
    async fn request_body_broken_chunked_forwards_nothing_past_it() {
        let (mut client, mut client_peer) = mem_pair();
        let (mut backend, mut backend_peer) = mem_pair();
        client_peer
            .write_all(b"5\r\nhello\r\n0\r\n\nGET /smuggled HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let out = relay(&mut client, &mut backend, None, Some(ChunkedStream::new())).await;
        assert!(matches!(out, RequestBody::Broken));
        drop(backend);
        let mut got = Vec::new();
        backend_peer.read_to_end(&mut got).await.unwrap();
        assert!(got.is_empty(), "forwarded {:?}", got);
    }

    /// Broken in the initial read: reported without reading the client
    /// again (its EOF would otherwise surface as Truncated).
    #[tokio::test]
    async fn request_body_already_broken_skips_client_read() {
        let (mut client, client_peer) = mem_pair();
        let (mut backend, _backend_peer) = mem_pair();
        drop(client_peer);
        let mut stream = ChunkedStream::new();
        stream.observe(b"5 \r\n");
        assert!(stream.is_broken());
        let out = relay(&mut client, &mut backend, None, Some(stream)).await;
        assert!(matches!(out, RequestBody::Broken));
    }

    fn framing(content_length: Option<usize>, chunked: bool) -> ResponseFraming {
        ResponseFraming {
            status: 200,
            backend_close: false,
            transfer_encoded: chunked,
            content_length,
            no_body: false,
            body_bytes_already_forwarded: 0,
            chunked: chunked.then(ChunkedStream::new),
            overshoot: false,
        }
    }

    /// Feeds `wire` then backend EOF; returns the outcome and what the
    /// client received.
    async fn forward(mut framing: ResponseFraming, wire: &[u8]) -> (BodyEnd, Vec<u8>) {
        let (mut backend, mut backend_peer) = mem_pair();
        let (mut client, mut client_peer) = mem_pair();
        backend_peer.write_all(wire).await.unwrap();
        drop(backend_peer);
        let mut buf = vec![0u8; 4096];
        let end = forward_response_body(&mut backend, &mut client, &mut buf, &mut framing, None)
            .await
            .unwrap();
        drop(client);
        let mut got = Vec::new();
        client_peer.read_to_end(&mut got).await.unwrap();
        (end, got)
    }

    #[tokio::test]
    async fn response_body_outcomes() {
        let (end, got) = forward(framing(None, true), b"5\r\nhello\r\n0\r\n\r\n").await;
        assert!(matches!(end, BodyEnd::Complete));
        assert_eq!(got, b"5\r\nhello\r\n0\r\n\r\n");

        // Only a close-delimited body may end at backend EOF.
        let (end, _) = forward(framing(None, false), b"all of it").await;
        assert!(matches!(end, BodyEnd::Closed));
        let (end, _) = forward(framing(Some(100), false), b"partial").await;
        assert!(matches!(end, BodyEnd::Truncated));
        let (end, got) = forward(framing(None, true), b"5\r\nhello\r\n").await;
        assert!(matches!(end, BodyEnd::Truncated));
        assert!(!got.ends_with(b"0\r\n\r\n"));

        let wire = b"5\r\nhello\r\n0\r\n\nHTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nSMUGGLED";
        let (end, got) = forward(framing(None, true), wire).await;
        assert!(matches!(end, BodyEnd::Broken));
        assert!(got.is_empty(), "forwarded {:?}", got);
    }

    /// A stray small Content-Length beside chunked must not end the body
    /// before the terminator.
    #[test]
    fn chunked_response_ignores_content_length() {
        let head = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 0\r\n\r\n";
        let framing = frame_response(head, head.len(), false, false).unwrap();
        assert!(framing.chunked.is_some());
        assert_eq!(framing.content_length, None);
        assert!(framing.transfer_encoded);
    }

    fn head_framing(fields: &str) -> Option<BodyFraming> {
        parse_response_head(format!("HTTP/1.1 200 OK\r\n{fields}\r\n").as_bytes()).map(|h| h.body)
    }

    #[test]
    fn response_framing_follows_the_final_transfer_coding() {
        use BodyFraming::{Chunked, Close, Length};
        for (fields, want) in [
            ("Content-Length: 5\r\n", Length(5)),
            ("Content-Length:5 \r\nContent-Length: 5\r\n", Length(5)),
            ("Content-Length: 007\r\n", Length(7)),
            ("", Close),
            ("Transfer-Encoding: chunked\r\n", Chunked),
            (
                "Transfer-Encoding: CHUNKED\r\nContent-Length: 9\r\n",
                Chunked,
            ),
            ("Transfer-Encoding: gzip, chunked\r\n", Chunked),
            (
                "Transfer-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n",
                Chunked,
            ),
            ("Transfer-Encoding: chunked, \r\n", Chunked),
            ("Transfer-Encoding: chunked, gzip\r\n", Close),
            (
                "Transfer-Encoding: chunked\r\nTransfer-Encoding: gzip\r\n",
                Close,
            ),
            (
                "Transfer-Encoding: identity\r\nContent-Length: 5\r\n",
                Close,
            ),
            ("Transfer-Encoding: chunked;x=1\r\n", Close),
            ("Transfer-Encoding: \r\nContent-Length: 5\r\n", Close),
        ] {
            assert_eq!(head_framing(fields), Some(want), "{fields:?}");
        }
    }

    #[test]
    fn response_heads_a_client_could_frame_differently_are_malformed() {
        for fields in [
            "Content-Length: 5, 5\r\n",
            "Content-Length: +5\r\n",
            "Content-Length: -1\r\n",
            "Content-Length: 5 5\r\n",
            "Content-Length: 5\r\nContent-Length: 6\r\n",
            "Content-Length: \r\n",
            "Content-Length: 18446744073709551616\r\n",
            // Malformed even where Transfer-Encoding overrides it.
            "Transfer-Encoding: chunked\r\nContent-Length: x\r\n",
            "Content-Length : 5\r\n",
            "Transfer-Encoding\t: chunked\r\n",
            ": x\r\n",
            "X-A: 1\rContent-Length: 5\r\n",
            "Content-Length: 5\r\r\n",
            "Transfer-Encoding: gzip,\r\n chunked\r\n",
            "X-A: 1\r\n\tContent-Length: 5\r\n",
            // A line with no field name a client agrees on: forwarded, it
            // could be read as anything, including a framing header.
            "Content-Length 5\r\n",
            "X-A\r\n",
            "X(A): 1\r\n",
            "X@A: 1\r\n",
            "\"X\": 1\r\n",
            "X\u{e9}: 1\r\n",
        ] {
            assert_eq!(head_framing(fields), None, "{fields:?}");
        }
        let head = b"HTTP/1.0 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert_eq!(parse_response_head(head), None);
        assert_eq!(parse_response_head(b"HTTP/1.1 200 OK\rX\r\n\r\n"), None);
    }

    /// The h2 bridge refuses what this flags: it can only remove chunked.
    #[test]
    fn only_a_lone_chunked_leaves_the_body_uncoded() {
        let coded = |fields: &str| {
            parse_response_head(format!("HTTP/1.1 200 OK\r\n{fields}\r\n").as_bytes())
                .unwrap()
                .other_codings
        };
        for fields in [
            "Content-Length: 5\r\n",
            "Transfer-Encoding: chunked\r\n",
            "Transfer-Encoding: Chunked, \r\n",
        ] {
            assert!(!coded(fields), "{fields:?}");
        }
        for fields in [
            "Transfer-Encoding: gzip, chunked\r\n",
            "Transfer-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n",
            "Transfer-Encoding: identity\r\n",
            "Transfer-Encoding: chunked, gzip\r\n",
            "Transfer-Encoding: chunked, chunked\r\n",
            "Transfer-Encoding: \r\n",
        ] {
            assert!(coded(fields), "{fields:?}");
        }
    }

    #[test]
    fn backend_close_is_any_close_token_or_a_bare_http10() {
        let close = |head: &[u8]| parse_response_head(head).unwrap().backend_close;
        assert!(!close(b"HTTP/1.1 200 OK\r\n\r\n"));
        assert!(close(
            b"HTTP/1.1 200 OK\r\nConnection: keep-alive, close\r\n\r\n"
        ));
        assert!(close(
            b"HTTP/1.1 200 OK\r\nX: 1\r\nConnection: close\r\n\r\n"
        ));
        assert!(close(b"HTTP/1.0 200 OK\r\n\r\n"));
        assert!(!close(b"HTTP/1.0 200 OK\r\nConnection: Keep-Alive\r\n\r\n"));
    }

    #[test]
    fn status_line_is_version_code_then_an_optional_reason() {
        for (line, want) in [
            (&b"HTTP/1.1 200 OK"[..], Some((200, false))),
            (b"HTTP/1.1 101 Switching Protocols", Some((101, false))),
            (b"HTTP/1.0 304 Not Modified", Some((304, true))),
            (b"HTTP/1.1 200", Some((200, false))),
            (b"HTTP/1.1 200 ", Some((200, false))),
            (b"HTTP/1.1 999 Request denied", Some((999, false))),
            (b"HTTP/1.1 2040 OK", None),
            (b"HTTP/1.1 20 OK", None),
            (b"HTTP/1.1 099 Early", None),
            (b"HTTP/1.1  200 OK", None),
            (b"HTTP/1.1 200\tOK", None),
            (b"HTTP/1.1 200 O\rK", None),
            (b"http/1.1 200 OK", None),
            (b"HTTP/2 200 OK", None),
            (b"HTTP/1.x 200 OK", None),
            (b"HTTP/1.1 abc", None),
            (b"short", None),
        ] {
            assert_eq!(
                parse_status_line(line),
                want,
                "{}",
                String::from_utf8_lossy(line)
            );
        }
        // The head's own status, and the one interim detection reads.
        let head = b"HTTP/1.1 304 Not Modified\r\nETag: x\r\n\r\n";
        assert_eq!(parse_response_head(head).unwrap().status, 304);
        assert_eq!(head_status(head), Some(304));
        assert_eq!(head_status(b"HTTP/1.1 1000 X\r\n\r\n"), None);
    }
}
