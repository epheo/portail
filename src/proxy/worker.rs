//! Tokio async TCP worker — one task per accepted connection.
//!
//! HTTP request/response proxying with keepalive and backend connection pooling,
//! raw TCP bidirectional forwarding, TLS termination, and TLS passthrough.

use anyhow::Result;
use arc_swap::ArcSwap;
use std::borrow::Cow;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use crate::logging::{debug, info, warn};
use crate::metrics::{ListenerStats, METRICS};
use crate::proxy::backend_pool::{
    connect_tcp, log_local_exhaustion, BackendPool, ConnectError, PoolHandle, SharedBackendPool,
};
use crate::proxy::chunked::ChunkedStream;
use crate::proxy::health::HealthRegistry;
use crate::proxy::http_filters::{
    absolute_form_target, apply_request_header_modifications, apply_response_header_mods,
    assemble_forward_headers, assemble_forward_headers_slow, dispatch_mirrors, extract_header_mods,
    extract_url_rewrite, header_lines, sanitize_response_headers, to_origin_form,
    HeaderModifications, MIRROR_BODY_MAX,
};
use crate::proxy::http_parser::{
    find_header_end, is_field_value_byte, is_token, parse_digits, trim_ows,
};
use crate::proxy::method::{is_method_byte, MAX_METHOD_LEN};
use crate::proxy::request_processor::{
    analyze_request, analyze_tcp_request, client_done, is_http_request, ClientSent, LocalReply,
    RequestMeta, RoutingResult,
};
use crate::proxy::tls::{self, Connection, DynamicTlsAcceptor};
use crate::routing::{BackendSelector, HttpFilter, HttpRouteRule, RouteTable};

/// Continue reading from `client` into `buf` starting at offset `already_read`
/// until the full HTTP headers (\r\n\r\n) are found or the buffer fills.
/// Returns `Some(0)` when the client closes first, `None` when the head
/// outgrows `buf`.
async fn read_remaining_headers(
    client: &mut Connection,
    buf: &mut [u8],
    already_read: usize,
) -> std::io::Result<Option<usize>> {
    let mut total = already_read;
    loop {
        if total >= buf.len() {
            return Ok(None);
        }
        let n = client.read(&mut buf[total..]).await?;
        if n == 0 {
            // Never parse a truncated head: the proxy's own headers would be
            // appended to its unterminated last line, which could hide them
            // (e.g. inside a client header's value).
            return Ok(Some(0));
        }
        total += n;
        if find_header_end(&buf[..total]).is_some() {
            return Ok(Some(total));
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
    /// guard), and the bound on a connection's close. Does not bound the
    /// idle wait between keepalive requests.
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

/// The keepalive of accepted and backend sockets alike: a dead backend pins
/// an idle client's task and both fds as surely as a dead client.
pub(crate) fn tcp_keepalive(time: Duration) -> socket2::TcpKeepalive {
    socket2::TcpKeepalive::new()
        .with_time(time)
        .with_interval(TCP_KEEPALIVE_INTERVAL)
        .with_retries(TCP_KEEPALIVE_RETRIES)
}

/// Pause before retrying a failed accept(). EMFILE, ENFILE, ENOBUFS and
/// ENOMEM leave the listener readable, so an immediate retry fails the same
/// way: the loop spins a core and logs every turn until resources free up.
#[derive(Default)]
pub(crate) struct AcceptBackoff {
    delay: Option<Duration>,
}

impl AcceptBackoff {
    // Go net/http's schedule: a blip costs a few ms, a sustained shortage
    // one retry (and one log line) a second.
    const FIRST: Duration = Duration::from_millis(5);
    const MAX: Duration = Duration::from_secs(1);

    /// The pause owed after `error`; `None` when only the connection being
    /// accepted failed, which leaves the listener healthy: aborted while
    /// queued, or a network error Linux accept(2) passes on from it, which
    /// its man page says to retry at once like EAGAIN.
    pub(crate) fn after(&mut self, error: &std::io::Error) -> Option<Duration> {
        use std::io::ErrorKind::{ConnectionAborted, ConnectionReset};
        if matches!(error.kind(), ConnectionAborted | ConnectionReset)
            || matches!(
                error.raw_os_error(),
                Some(
                    libc::ENETDOWN
                        | libc::EPROTO
                        | libc::ENOPROTOOPT
                        | libc::EHOSTDOWN
                        | libc::ENONET
                        | libc::EHOSTUNREACH
                        | libc::EOPNOTSUPP
                        | libc::ENETUNREACH
                )
            )
        {
            return None;
        }
        let delay = self.delay.map_or(Self::FIRST, |d| (d * 2).min(Self::MAX));
        self.delay = Some(delay);
        Some(delay)
    }

    pub(crate) fn reset(&mut self) {
        self.delay = None;
    }
}

/// RAII in-flight-connection counter. One guard lives inside each spawned
/// per-connection task; `data_plane::Drain::run` polls the shared count and
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
    /// Set on every backend socket this connection opens.
    backend_keepalive: socket2::TcpKeepalive,
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
    /// Cancelled when the data plane drains: the idle wait between
    /// requests ends, and the exchange in flight is the last one. This
    /// connection's own child of the listener token, so the checks on
    /// every request lock nothing shared.
    drain: CancellationToken,
    /// The connection closes after a reply while the client may still be
    /// sending: the close reads off what comes instead of resetting on it.
    /// Set by `close_after_reply` only, so every reply follows one rule.
    linger: bool,
    /// This request's client sends nothing more (see [`client_done`]), so a
    /// closing reply has nothing to linger for. Reset as each request
    /// starts: nothing is known yet of its client.
    client_done: bool,
    /// `reset_on_drop` armed the client: the close must not undo it. Kept
    /// here rather than read back off the socket, a syscall every close
    /// would pay.
    reset: bool,
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
        drain: CancellationToken,
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
            backend_keepalive: tcp_keepalive(cfg.tcp_keepalive_time),
            header_buf: Vec::with_capacity(1024),
            scratch_buf: Vec::with_capacity(1024),
            filter_buf: Vec::new(),
            idle_body_timeout: idle_opt(cfg.idle_body_timeout),
            listener_stats,
            last_status: 0,
            drain,
            linger: false,
            client_done: false,
            reset: false,
        }
    }

    /// The connection closes after a reply: a client still sending, a
    /// pipelined request included, would get a reset that can destroy the
    /// reply it has not read yet.
    fn close_after_reply(&mut self) {
        self.linger = !self.client_done;
    }

    /// Record an error reply the proxy wrote itself, which always closes:
    /// logged as `status`, and closing as any reply does.
    fn refused(&mut self, status: u16) {
        self.last_status = status;
        self.close_after_reply();
    }
}

/// Whether a connection may take another request after this one.
fn keep_open(drain: &CancellationToken, reusable: bool) -> bool {
    reusable && !drain.is_cancelled()
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
    let keepalive = tcp_keepalive(cfg.tcp_keepalive_time);

    let mut backoff = AcceptBackoff::default();
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
                        backoff.reset();
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
                                let _ = handle_tls_passthrough(tcp_stream, server_port, routes, health, selector, &cfg).await;
                            });
                        } else if let Some(acceptor) = acceptor {
                            let selector = selector.clone();
                            let shared_pool = shared_pool.clone();
                            let listener_stats = listener_stats.clone();
                            let drain = shutdown.child_token();
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
                                    drain,
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
                                shutdown.child_token(),
                            );
                            tokio::spawn(async move {
                                let _guard = conn_guard;
                                let conn = Connection::Plain { inner: tcp_stream };
                                let _ = handle_connection(conn, peer, state).await;
                            });
                        }
                    }
                    Err(e) => {
                        if let Some(pause) = backoff.after(&e) {
                            warn!(
                                "Listener on :{} accept error: {}; retrying in {:?}",
                                server_port, e, pause
                            );
                            // Cut short by a shutdown, which the loop then sees.
                            let _ = tokio::time::timeout(pause, shutdown.cancelled()).await;
                        } else {
                            debug!("Listener on :{} accept error: {}", server_port, e);
                        }
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
    drain: CancellationToken,
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
        let _ =
            handle_tls_passthrough(tcp_stream, server_port, routes, health, selector, &cfg).await;
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
                drain,
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
        drain,
    );
    let _ = handle_connection(conn, peer, state).await;
}

/// A client connection's buffer: the largest request head, and the most
/// a lingering close reads off. The h2 bridge's close reads off no more.
pub(crate) const CLIENT_BUF: usize = 64 * 1024;

pub(crate) async fn handle_connection(
    mut client: Connection,
    _peer: SocketAddr,
    mut state: ConnectionState,
) -> Result<()> {
    client.set_nodelay(true)?;

    let (tcp_route, http_routes) = {
        let rt = state.routes.load();
        let port = state.server_port;
        (rt.has_tcp_route(port), rt.has_http_routes(port))
    };
    // Server-speaks-first protocols (MySQL, SMTP, FTP, IMAP, POP3) send
    // nothing before the backend's banner. With no HTTP route to tell
    // apart there is nothing to sniff, and waiting for the client's first
    // bytes would deadlock them.
    if tcp_route && !http_routes {
        return forward_to_tcp_route(client, &state).await;
    }

    let mut buf = vec![0u8; CLIENT_BUF];
    let served = serve_http(&mut client, &mut buf, &mut state, tcp_route).await;
    if let Ok(Handoff::Tcp { backend_addr, len }) = served {
        return handle_tcp_connection(
            client,
            backend_addr,
            &buf[..len],
            Arc::clone(&state.health),
            state.connect_timeout,
            &state.backend_keepalive,
            state.idle_body_timeout,
            "TCP",
        )
        .await;
    }
    close_connection(&mut client, &mut buf, &state).await;
    served.map(drop)
}

/// Where [`serve_http`] leaves the connection.
enum Handoff {
    /// The HTTP exchanges are over: the connection closes.
    Close,
    /// Not HTTP: the connection goes to a TCP route, `buf[..len]` first.
    Tcp {
        backend_addr: SocketAddr,
        len: usize,
    },
}

/// Serve requests until the connection must close, or turns out to be
/// opaque TCP. Every exit comes back to `handle_connection`, which owns the
/// one close.
async fn serve_http(
    client: &mut Connection,
    buf: &mut [u8],
    state: &mut ConnectionState,
    tcp_route: bool,
) -> Result<Handoff> {
    let mut n = idle_read(client, buf, &state.drain).await?;
    n = sniff_first_request(client, buf, n, state.client_header_timeout, !tcp_route).await;

    // If data looks like HTTP but headers aren't complete, accumulate more reads.
    // TCP/binary data (no \r\n\r\n expected) passes through immediately.
    if is_http_request(&buf[..n]) && find_header_end(&buf[..n]).is_none() {
        n = complete_headers(client, buf, n, state).await?;
    }
    if n == 0 {
        return Ok(Handoff::Close);
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
                return Ok(Handoff::Tcp {
                    backend_addr,
                    len: n,
                });
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
                        reusable: keep_open(&state.drain, meta.local_reply_reusable),
                        client_done: meta.client_done,
                        head: meta.is_head,
                    };
                    if !send_local_reply(client, state, &reply, access).await? {
                        return Ok(Handoff::Close);
                    }
                    pipelined_tail(&buf[..n])
                } else {
                    // proxy_http_request owns all timing for the request lifecycle —
                    // backend_request_timeout (connect + TTFB) and request_timeout
                    // (total deadline) are applied phase-aware inside.
                    let backend_addr = backend.socket_addr;
                    let (keep, leftover) =
                        proxy_http_request(client, buf, n, backend, rule, &meta, state).await?;
                    if !meta.is_upgrade {
                        crate::metrics::REQUEST_DURATION.observe(request_started.elapsed());
                    }
                    crate::access_log::emit(access, state.last_status, Some(backend_addr));
                    // No drain check: the head already told the client. A
                    // drain since then ends the idle wait, or the next reply.
                    if !keep {
                        return Ok(Handoff::Close);
                    }
                    leftover
                }
            }
            RoutingResult::LocalReply(mut reply) => {
                reply.reusable = keep_open(&state.drain, reply.reusable);
                if !send_local_reply(client, state, &reply, access).await? {
                    return Ok(Handoff::Close);
                }
                pipelined_tail(&buf[..n])
            }
            RoutingResult::UdpForward { .. } | RoutingResult::CloseConnection => {
                return Ok(Handoff::Close);
            }
        };

        // Explicitly drop the route table guard before reading the next request.
        // This allows old route tables to be freed between keepalive iterations.
        drop(route_table);
        state.client_done = false;

        if let Some(bytes) = pending {
            // Replay pipelined bytes as the start of the next request. They
            // came out of `buf` via a single read, so they always fit.
            buf[..bytes.len()].copy_from_slice(&bytes);
            n = bytes.len();
        } else {
            n = idle_read(client, buf, &state.drain).await?;
        }
        // RFC 9112 2.2: empty lines before a request-line are ignored. An
        // old client ends a POST body with a CRLF its length leaves out.
        loop {
            let skip = empty_lines_len(&buf[..n]);
            if skip > 0 {
                buf.copy_within(skip..n, 0);
                n -= skip;
            }
            match &buf[..n] {
                // Between requests: as unbounded as the idle wait.
                [] if skip > 0 => n = idle_read(client, buf, &state.drain).await?,
                // Inside an empty line: as bounded as a request head.
                [b'\r'] => {
                    let rest = client.read(&mut buf[1..]);
                    match tokio::time::timeout(state.client_header_timeout, rest).await {
                        Ok(Ok(read)) if read > 0 => n += read,
                        _ => return Ok(Handoff::Close),
                    }
                }
                _ => break,
            }
        }
        // The connection is HTTP by now: bytes that cannot open a request
        // (frames held back from a refused upgrade) close it. They must not
        // reach TCP routing, nor wait out the header budget for a 408.
        if n == 0 || !may_open_request(&buf[..n]) {
            return Ok(Handoff::Close);
        }
        if find_header_end(&buf[..n]).is_none() {
            n = complete_headers(client, buf, n, state).await?;
        }
        if n == 0 || !is_http_request(&buf[..n]) {
            return Ok(Handoff::Close);
        }
    }
}

/// End a connection with one graceful shutdown: close_notify then FIN over
/// TLS, where strict clients (OpenSSL 3, rustls) read a bare FIN as a
/// truncation attack. Plain TCP that does not linger needs none: dropping
/// the socket sends the same FIN. A connection `reset_on_drop` armed keeps
/// its reset, which a clean close would turn into a complete-looking
/// response.
///
/// `state.linger` follows a reply after which the connection closes, unless
/// the client is done (see [`client_done`]). Closing on bytes still
/// arriving makes the kernel reset: an upload then fails its send
/// instead of reading the reply, and a pipelined request can destroy the
/// reply not read yet. As nginx's lingering_close does, what still arrives
/// is read and dropped until the client closes or falls silent for
/// `LINGER_IDLE`, up to one `buf` of it. A drain's close after a reply
/// lingers too, so a client keeping its socket open holds the drain that
/// long. An idle connection, no reply just written, closes at once: there
/// is no reply for a reset to destroy.
///
/// All of it within the client header budget: a client that stops reading,
/// or never stops sending, cannot hold the close.
async fn close_connection(client: &mut Connection, buf: &mut [u8], state: &ConnectionState) {
    let tls = match client {
        Connection::Plain { .. } => false,
        Connection::Tls { .. } => true,
        // The h2 bridge's pipe has no reset to avoid, and its stream must
        // see the end at once: dropping the pipe is the close.
        Connection::Mem { .. } | Connection::ClientTls { .. } => return,
    };
    if state.reset || !(tls || state.linger) {
        return;
    }
    let deadline = tokio::time::Instant::now() + state.client_header_timeout;
    let shutdown = tokio::time::timeout_at(deadline, client.shutdown()).await;
    if state.linger && matches!(shutdown, Ok(Ok(()))) {
        read_off(client, buf, deadline).await;
    }
}

/// How long a lingering close waits for the client's next bytes: a client
/// silent this long is done. nginx's lingering_timeout default.
pub(crate) const LINGER_IDLE: Duration = Duration::from_secs(5);

/// Read and drop what the client still sends, up to one `buf` of it, until
/// it closes, falls silent for `LINGER_IDLE`, or `deadline` passes.
async fn read_off(client: &mut Connection, buf: &mut [u8], deadline: tokio::time::Instant) {
    let mut discarded = 0;
    while discarded < buf.len() {
        let idle = deadline.min(tokio::time::Instant::now() + LINGER_IDLE);
        match tokio::time::timeout_at(idle, client.read(&mut buf[discarded..])).await {
            Ok(Ok(read)) if read > 0 => discarded += read,
            _ => return,
        }
    }
}

/// Relay a connection on a TCP-only port to its route, before the client
/// has sent anything.
async fn forward_to_tcp_route(client: Connection, state: &ConnectionState) -> Result<()> {
    let routed = analyze_tcp_request(
        &state.routes.load(),
        &state.selector,
        state.server_port,
        &state.health,
    )?;
    let RoutingResult::TcpForward { backend_addr } = routed else {
        return Ok(()); // the router logged why
    };
    handle_tcp_connection(
        client,
        backend_addr,
        &[],
        Arc::clone(&state.health),
        state.connect_timeout,
        &state.backend_keepalive,
        state.idle_body_timeout,
        "TCP",
    )
    .await
}

/// The wait for a connection's next request, its first included: unbounded,
/// as long idle is legitimate there, until a drain ends it like an EOF
/// would (0).
async fn idle_read(
    client: &mut Connection,
    buf: &mut [u8],
    drain: &CancellationToken,
) -> std::io::Result<usize> {
    tokio::select! {
        // A request already sent is served: its reply announces the close.
        biased;
        read = client.read(buf) => read,
        () = drain.cancelled() => Ok(0),
    }
}

/// Byte length of the empty lines (CRLF pairs) opening `data`.
fn empty_lines_len(data: &[u8]) -> usize {
    let pairs = data.as_chunks::<2>().0;
    pairs.iter().take_while(|pair| *pair == b"\r\n").count() * 2
}

/// Extend a first read until the sniff can tell HTTP from opaque bytes,
/// within `budget`. Empty lines before the first request-line are ignored
/// too (RFC 9112 2.2), but only when an HTTP request follows: for TCP
/// routing the bytes stay as sent. A read ending inside them waits for
/// more; with `await_method` so does one ending inside a method ("GET",
/// "PROPF"), for a port no TCP route serves, where sniffed as opaque it
/// would close unanswered. Returns the length of the data now in `buf`.
async fn sniff_first_request(
    client: &mut Connection,
    buf: &mut [u8],
    mut n: usize,
    budget: Duration,
    await_method: bool,
) -> usize {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let skip = empty_lines_len(&buf[..n]);
        let rest = &buf[skip..n];
        if is_http_request(rest) {
            if skip > 0 {
                buf.copy_within(skip..n, 0);
            }
            return n - skip;
        }
        let undecided = rest == b"\r"
            || (skip > 0 && rest.is_empty())
            || (await_method && may_open_request(rest));
        if !undecided || n == buf.len() {
            return n;
        }
        match tokio::time::timeout_at(deadline, client.read(&mut buf[n..])).await {
            Ok(Ok(read)) if read > 0 => n += read,
            _ => return n,
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
/// slow, then errors out to close the connection, as a client I/O error
/// does with no reply. Returns 0 when the client closed before the head
/// ended: nothing is forwarded or answered.
async fn complete_headers(
    client: &mut Connection,
    buf: &mut [u8],
    already_read: usize,
    state: &mut ConnectionState,
) -> Result<usize> {
    let budget = state.client_header_timeout;
    match tokio::time::timeout(budget, read_remaining_headers(client, buf, already_read)).await {
        Ok(Ok(Some(total))) => Ok(total),
        Ok(Ok(None)) => {
            // The rest of the head may still be on its way.
            state.refused(431);
            let _ = send_error_response(client, 431).await;
            Err(anyhow::anyhow!("Request headers exceed buffer size"))
        }
        // A reset, or a TLS record that does not decode: the connection is
        // gone or broken, so nothing could be answered.
        Ok(Err(e)) => Err(e.into()),
        Err(_) => {
            // No linger: this client already used its whole budget.
            let _ = send_error_response(client, 408).await;
            Err(anyhow::anyhow!("client header read timed out"))
        }
    }
}

/// Proxy a single HTTP request to a backend and forward the response.
/// Returns `(keep, leftover)`: `keep` when the response went out whole and
/// its head announced keep-alive, which the client may already act on;
/// `leftover` holds any bytes read past this request's body (a pipelined
/// next request), which the keepalive loop replays instead of reading from
/// the socket again.
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
            state.refused(400);
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
    // The whole body came with the head, so the relay below reads nothing
    // into `buf`: a replay can still send it from there.
    let body_held = match (meta.content_length, &req_chunked) {
        (Some(cl), _) => tail_len >= cl,
        (None, Some(stream)) => stream.is_terminated(),
        (None, None) => true,
    };
    // Settled again once the relay below is through.
    state.client_done = client_done(ClientSent {
        keepalive: meta.keepalive,
        request_in: body_held && !meta.is_upgrade,
        seen_past: leftover.is_some(),
    });

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

    let (mut backend, reused) = match send_request_with_retry(
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
    // A pooled conn the backend dropped after the probe, as this request
    // reached it, shows only as a close before any response byte. Sending
    // the request again is safe only when its method is idempotent (the
    // backend may have acted before dying) and the proxy still holds it.
    let mut replayable = reused && meta.idempotent && body_held;

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
                    state.refused(408);
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
    let (mirror_body, relay_leftover, answered_early) = match relay {
        RequestBody::Complete { mirror, leftover } => (mirror, leftover, false),
        // A backend may answer before it has the whole body (a 413, a 401)
        // and stop reading it; RFC 9112 9.5 has the client watch for that.
        // The rest of the client's body stays unread, so the client is not
        // done: the close reads it off.
        RequestBody::BackendGone => (None, None, true),
        incomplete => {
            if let RequestBody::Broken = incomplete {
                warn!(
                    "Rejecting request with malformed chunked body to backend {}",
                    backend_addr
                );
            } else {
                debug!("Client closed mid request body to backend {}", backend_addr);
            }
            state.refused(400);
            // The client may already be gone (EOF case); the close follows
            // either way.
            let _ = send_error_response(client, 400).await;
            return Ok((false, None));
        }
    };
    if relay_leftover.is_some() {
        leftover = relay_leftover;
    }
    if !answered_early {
        // All of the request is in, unless it switches protocols.
        state.client_done = client_done(ClientSent {
            keepalive: meta.keepalive,
            request_in: !meta.is_upgrade,
            seen_past: leftover.is_some(),
        });
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
    let resp_mods = extract_response_mods(rule, backend_ref);
    let (mut framing, keep) = loop {
        // An early answer is one already here: a backend that only stalled
        // gets no fresh budget to answer in.
        let header_budget = (!answered_early).then(|| {
            clamp_to_deadline(
                backend_budget.saturating_sub(backend_phase_start.elapsed()),
                total_deadline,
            )
        });
        // With the body cut short, the connection neither tunnels nor
        // stays open.
        match forward_response_headers(
            client,
            &mut backend,
            &mut state.header_buf,
            &mut state.scratch_buf,
            &mut state.filter_buf,
            resp_mods.as_ref(),
            header_budget,
            meta.is_head,
            meta.is_upgrade && !answered_early,
            meta.keepalive && !answered_early,
            &state.drain,
        )
        .await?
        {
            HeaderPhase::Proxied { framing, keep } => break (framing, keep),
            HeaderPhase::ErrorSent(code) => {
                state.refused(code);
                return Ok((false, None));
            }
            HeaderPhase::Unanswered if replayable => {
                replayable = false;
                METRICS.pool_stale_retries_total.inc();
                debug!(
                    "Backend {} pool conn closed unanswered; replaying on a fresh conn",
                    backend_addr
                );
                backend = match resend_on_fresh(
                    client,
                    state,
                    backend_ref,
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
            }
            HeaderPhase::Unanswered => {
                if answered_early {
                    warn!("Backend {} stopped taking the request body", backend_addr);
                } else {
                    warn!("Backend {} closed without answering", backend_addr);
                }
                state.refused(502);
                send_error_response(client, 502).await?;
                return Ok((false, None));
            }
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
                    reset_on_drop(client, state);
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
            // Close-delimited: the connection's graceful close, which
            // follows, is how the client learns the body ended.
            state.close_after_reply();
            return Ok((false, None));
        }
        Ok(BodyEnd::Broken) => {
            warn!(
                "Backend {} sent malformed chunked framing; resetting client connection",
                backend_addr
            );
            reset_on_drop(client, state);
            return Ok((false, None));
        }
        Ok(BodyEnd::Truncated) => {
            debug!("Backend {} closed mid response body", backend_addr);
            reset_on_drop(client, state);
            return Ok((false, None));
        }
        Err(_e) => {
            debug!("Response body from backend {} failed: {}", backend_addr, _e);
            reset_on_drop(client, state);
            return Ok((false, None));
        }
    }

    client.flush().await?;
    if answered_early {
        // Never pooled: its request was cut short.
        state.close_after_reply();
        return Ok((false, None));
    }

    // Overshoot (bytes past the framing end) poisons the conn: the excess was
    // dropped here but whatever else follows would desync the next response.
    // The client connection stays open: the excess never reached it. Closing
    // it would let a backend padding with a stray CRLF close every client.
    if framing.overshoot {
        warn!(
            "Backend {} sent bytes past the response framing end; excess dropped, not pooling connection",
            backend_addr
        );
    }
    let framing_clean = !framing.overshoot;

    // Both bodies completed per their framing: every other outcome returned
    // above, dropping `backend`. The client got a clean response, but the
    // backend conn returns to the pool only when `backend_conn_poolable`
    // allows (notably never after HEAD or a refused upgrade) and never when
    // the backend announced Connection: close.
    if backend_conn_poolable(framing_clean, meta.is_head, meta.is_upgrade) && !framing.backend_close
    {
        state.pool.release(
            backend_addr,
            backend_ref.use_tls,
            &backend_ref.server_name,
            backend,
        );
    } else if framing_clean {
        debug!(
            "Backend {} conn not pooled (HEAD, refused upgrade, or backend Connection: close)",
            backend_addr
        );
    }

    if !keep {
        state.close_after_reply();
    }
    Ok((keep, leftover))
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

/// A backend connect that failed (`502`) or timed out (`504`): counted as
/// one, then answered as [`fail_backend`] does.
async fn fail_connect(
    state: &mut ConnectionState,
    backend_ref: &crate::routing::Backend,
    client: &mut Connection,
    code: u16,
) -> Result<()> {
    let label = backend_metric_label(backend_ref);
    match code {
        504 => {
            crate::metrics::record_backend_connect_timeout(&label);
            state.listener_stats.upstream_connect_timeouts.inc();
        }
        _ => {
            crate::metrics::record_backend_connect_error(&label);
            state.listener_stats.upstream_connect_errors.inc();
        }
    }
    fail_backend(state, client, backend_ref.socket_addr, code).await
}

/// A request that failed to send on a freshly connected backend conn: a
/// send error, not a connect error, then answered `502` as [`fail_backend`]
/// does.
async fn fail_send(
    state: &mut ConnectionState,
    backend_ref: &crate::routing::Backend,
    client: &mut Connection,
) -> Result<()> {
    crate::metrics::record_backend_send_error(&backend_metric_label(backend_ref));
    state.listener_stats.upstream_send_errors.inc();
    fail_backend(state, client, backend_ref.socket_addr, 502).await
}

/// Record a backend failure (spawning a health probe if this newly trips the
/// backend unhealthy) and send `code` to the client, whose connection then
/// closes. Log wording stays caller-side.
async fn fail_backend(
    state: &mut ConnectionState,
    client: &mut Connection,
    addr: SocketAddr,
    code: u16,
) -> Result<()> {
    if state.health.record_failure(addr) {
        HealthRegistry::spawn_probe(Arc::clone(&state.health), addr);
    }
    state.refused(code);
    send_error_response(client, code).await
}

/// Answer a connect the proxy could not make for want of its own fds or
/// memory: a 503, and the backend's health left alone.
async fn refuse_locally(
    state: &mut ConnectionState,
    client: &mut Connection,
    addr: SocketAddr,
    e: &std::io::Error,
) -> Result<()> {
    log_local_exhaustion(addr, e);
    // Not refused(): a lingering close would hold this connection's fd
    // while the proxy is short of fds or memory.
    state.last_status = 503;
    send_error_response(client, 503).await
}

/// Acquire a backend connection within `backend_budget` (clamped by `total_deadline`).
/// On a backend failure or timeout, records health failure and sends `502`/`504`
/// to the client; on local exhaustion, a `503` (see [`ConnectError::Local`]).
/// Returns `Ok(None)` when the caller should bail out (`return Ok(false)`),
/// which signals the connection is not reusable.
///
/// `Ok(Some((conn, from_pool)))`: `from_pool == true` means the conn came from
/// the idle pool and the caller may retry once with `acquire_fresh` if the
/// backend turns out to have dropped it (the probe is best-effort).
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
        state.pool.acquire(
            backend_addr,
            backend_ref.use_tls,
            &backend_ref.server_name,
            &state.backend_keepalive,
        ),
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
        Ok(Err(ConnectError::Local(e))) => {
            refuse_locally(state, client, backend_addr, &e).await?;
            Ok(None)
        }
        Ok(Err(e)) => {
            warn!("Backend {} connect failed: {}", backend_addr, e);
            fail_connect(state, backend_ref, client, 502).await?;
            Ok(None)
        }
        Err(_) => {
            warn!("Backend {} connect timeout", backend_addr);
            fail_connect(state, backend_ref, client, 504).await?;
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
/// `Ok(Some((conn, reused)))` hands back the connection that accepted the
/// request, `reused` when it is still the pooled one; `Ok(None)` means the
/// failure was already answered to the client.
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
) -> Result<Option<(Connection, bool)>> {
    let backend_addr = backend_ref.socket_addr;
    match write_with_tail(&mut backend, &mut state.scratch_buf, headers_len, body).await {
        Ok(()) => return Ok(Some((backend, from_pool))),
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
        Err(e) => {
            warn!("Backend {} send failed: {}", backend_addr, e);
            fail_send(state, backend_ref, client).await?;
            return Ok(None);
        }
    }
    drop(backend);
    let fresh = resend_on_fresh(
        client,
        state,
        backend_ref,
        headers_len,
        body,
        backend_budget,
        backend_phase_start,
        total_deadline,
    )
    .await?;
    Ok(fresh.map(|backend| (backend, false)))
}

/// Send the assembled request again on a brand-new connection, in place of
/// a pooled one found dead. Unlike that stale entry, a failure here is the
/// backend's, unless the proxy is out of resources: answered to the client
/// either way (`Ok(None)`).
#[allow(clippy::too_many_arguments)]
async fn resend_on_fresh(
    client: &mut Connection,
    state: &mut ConnectionState,
    backend_ref: &crate::routing::Backend,
    headers_len: usize,
    body: &[u8],
    backend_budget: Duration,
    backend_phase_start: std::time::Instant,
    total_deadline: Option<std::time::Instant>,
) -> Result<Option<Connection>> {
    let backend_addr = backend_ref.socket_addr;
    let fresh_budget = clamp_to_deadline(
        backend_budget.saturating_sub(backend_phase_start.elapsed()),
        total_deadline,
    );
    let mut backend = match tokio::time::timeout(
        fresh_budget,
        state.pool.acquire_fresh(
            backend_addr,
            backend_ref.use_tls,
            &backend_ref.server_name,
            &state.backend_keepalive,
        ),
    )
    .await
    {
        Ok(Ok(c)) => {
            state.health.record_success(backend_addr);
            c
        }
        Ok(Err(ConnectError::Local(e))) => {
            refuse_locally(state, client, backend_addr, &e).await?;
            return Ok(None);
        }
        Ok(Err(e2)) => {
            warn!(
                "Backend {} fresh-conn acquire failed after pool retry: {}",
                backend_addr, e2
            );
            fail_connect(state, backend_ref, client, 502).await?;
            return Ok(None);
        }
        Err(_) => {
            warn!(
                "Backend {} fresh-conn acquire timeout after pool retry",
                backend_addr
            );
            fail_connect(state, backend_ref, client, 504).await?;
            return Ok(None);
        }
    };
    if let Err(e2) = write_with_tail(&mut backend, &mut state.scratch_buf, headers_len, body).await
    {
        warn!(
            "Backend {} send failed on fresh conn after pool retry: {}",
            backend_addr, e2
        );
        fail_send(state, backend_ref, client).await?;
        return Ok(None);
    }
    Ok(Some(backend))
}

/// Outcome of the response-header phase: the framing to stream the body
/// under, or the error status this helper already wrote to the client.
enum HeaderPhase {
    /// `keep`: the head told the client the connection stays open, which
    /// holds once the body is through, whatever drain starts meanwhile.
    Proxied {
        framing: ResponseFraming,
        keep: bool,
    },
    ErrorSent(u16),
    /// No head came, and nothing was written to the client: the backend
    /// closed or reset before its first byte, or held no complete head
    /// where only one already here counts. The caller replays or answers.
    Unanswered,
}

/// Read response headers from the backend within `header_budget` and forward
/// them to the client. This is the SAFE→UNSAFE transition: on entry no client
/// bytes have been written and timeout → `504`; on success the client has the
/// headers, and any later failure must reset the client connection instead.
/// A `None` budget takes only a head that has already arrived.
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
    header_budget: Option<Duration>,
    is_head_request: bool,
    expect_upgrade: bool,
    keepalive: bool,
    drain: &CancellationToken,
) -> Result<HeaderPhase> {
    let read = match header_budget {
        Some(budget) => tokio::time::timeout(budget, read_response_head(backend, header_buf)).await,
        None => Ok(arrived_head(backend, header_buf).await),
    };
    let read = match read {
        Ok(Ok(read)) => read,
        Ok(Err(e)) => {
            warn!("Backend response head failed: {}", e);
            send_error_response(client, 502).await?;
            return Ok(HeaderPhase::ErrorSent(502));
        }
        Err(_) => {
            send_error_response(client, 504).await?;
            return Ok(HeaderPhase::ErrorSent(504));
        }
    };
    let (resp_header_end, head) = match read {
        HeadRead::Head(end, head) => (end, head),
        HeadRead::Malformed => {
            warn!("Backend sent a response head with ambiguous framing; rejecting");
            send_error_response(client, 502).await?;
            return Ok(HeaderPhase::ErrorSent(502));
        }
        HeadRead::Unanswered => return Ok(HeaderPhase::Unanswered),
    };
    let framing = frame_response(
        head,
        header_buf,
        resp_header_end,
        is_head_request,
        expect_upgrade,
    );

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
        return Ok(HeaderPhase::Proxied {
            framing,
            keep: false,
        });
    }

    // Hop-by-hop response headers never reach the client (RFC 7230 6.1);
    // the proxy owns the client-side Connection header.
    // The head's own Content-Length, even where no body follows (HEAD, 304).
    let content_length = match head.body {
        BodyFraming::Length(n) => Some(n),
        BodyFraming::Chunked | BodyFraming::Close => None,
    };
    // Decided only now that the head is in: a drain that started while
    // the backend worked is still announced.
    let keep = keep_open(drain, keepalive) && !framing.close_delimited();
    sanitize_response_headers(
        &header_buf[..resp_header_end],
        content_length,
        !keep,
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
    Ok(HeaderPhase::Proxied { framing, keep })
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
    /// The backend stopped taking the body: closed, reset (a pooled conn it
    /// had dropped) or stalled past the progress budget.
    BackendGone,
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
            if idle_guarded(idle, backend.write_all(&buf[..to_send]))
                .await
                .is_err()
            {
                return Ok(RequestBody::BackendGone);
            }
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
            if idle_guarded(idle, backend.write_all(&buf[..consumed]))
                .await
                .is_err()
            {
                return Ok(RequestBody::BackendGone);
            }
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
    status: u16,
    /// The backend announced it will close this connection (Connection:
    /// close, or an HTTP/1.0 response without keep-alive). Such a conn must
    /// not return to the pool: the next write would fail and burn the retry.
    backend_close: bool,
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

/// How reading a response head ended, short of an error.
enum HeadRead {
    /// The final (non-interim) head and where it ends: `header_buf` holds
    /// `[headers..header_end][same-read body tail...]`.
    Head(usize, ResponseHead),
    /// A head, interim ones included, a recipient could frame differently
    /// (see `parse_response_head`).
    Malformed,
    /// Closed or reset before a single byte; from [`arrived_head`], also no
    /// complete head yet.
    Unanswered,
}

/// [`read_response_head`] over what the backend has already sent, with no
/// wait for more.
async fn arrived_head(backend: &mut Connection, header_buf: &mut Vec<u8>) -> Result<HeadRead> {
    use futures::FutureExt;
    // tokio resumes a yielded task only after polling the reactor, which
    // then knows of bytes that landed while this task kept busy.
    tokio::task::yield_now().await;
    read_response_head(backend, header_buf)
        .now_or_never()
        .unwrap_or(Ok(HeadRead::Unanswered))
}

/// Phase 1: read response headers from the backend into `header_buf`. Performs
/// NO writes to the client; aborting/timeout/error before this returns leaves the
/// client wire untouched, so the caller can still emit a clean `504`.
async fn read_response_head(
    backend: &mut Connection,
    header_buf: &mut Vec<u8>,
) -> Result<HeadRead> {
    header_buf.clear();
    let mut interim_seen = 0u32;
    let mut answered = false;

    loop {
        // Check before reading: a swallowed interim response leaves the next
        // response's bytes already buffered.
        if let Some(header_end) = find_header_end(header_buf) {
            let Some(head) = parse_response_head(&header_buf[..header_end]) else {
                return Ok(HeadRead::Malformed);
            };

            // Interim responses are proxy-consumed: the proxy answers
            // 100-continue itself (Expect never reaches the backend), and
            // relaying other 1xx mid-exchange would confuse clients that
            // already got a 100. 1xx has no body, so everything past the
            // block belongs to the next response. 101 is not interim here:
            // the caller dispatches it (upgrade accept or reject).
            if (100..200).contains(&head.status) && head.status != 101 {
                interim_seen += 1;
                if interim_seen > MAX_INTERIM_RESPONSES {
                    return Err(anyhow::anyhow!(
                        "backend sent more than {} interim 1xx responses",
                        MAX_INTERIM_RESPONSES
                    ));
                }
                debug!("Swallowing backend interim {} response", head.status);
                header_buf.drain(..header_end);
                continue;
            }
            return Ok(HeadRead::Head(header_end, head));
        }

        if header_buf.len() > RESPONSE_HEADER_CAP {
            return Err(anyhow::anyhow!("response headers exceed buffer size"));
        }

        // read_buf with little spare capacity degrades to tiny reads; keep a
        // full read's worth reserved (and spare > 0 makes Ok(0) mean EOF).
        header_buf.reserve(16 * 1024);
        match backend.read_buf(header_buf).await {
            Ok(1..) => answered = true,
            Ok(0) | Err(_) if !answered => return Ok(HeadRead::Unanswered),
            Ok(_) => return Err(anyhow::anyhow!("backend closed mid response head")),
            Err(e) => return Err(e.into()),
        }
    }
}

/// Framing of `parsed`, the final head in `header_buf[..header_end]`,
/// including how much of the same-read tail belongs to its body.
fn frame_response(
    parsed: ResponseHead,
    header_buf: &[u8],
    header_end: usize,
    is_head_request: bool,
    expect_upgrade: bool,
) -> ResponseFraming {
    let status = parsed.status;
    // RFC 7230 3.3: responses to HEAD have no message body regardless
    // of the response's Content-Length / Transfer-Encoding. Forcing
    // no_body here keeps `forward_response_body` from blocking on
    // bytes the backend correctly never sends.
    let no_body = response_has_no_body(status, is_head_request);
    let mut framing = ResponseFraming {
        status,
        backend_close: parsed.backend_close,
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
    framing
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
/// complete; a reset is an error for every framing. The h2 bridge's pipe has
/// no reset: it is told, and resets the stream. `state` records it for the
/// close, which must not undo it.
fn reset_on_drop(client: &Connection, state: &mut ConnectionState) {
    state.reset = true;
    if let Connection::Mem { aborted, .. } = client {
        aborted.store(true, Ordering::Release);
    }
    if let Some(tcp) = client_socket(client) {
        // Zero linger: close() discards unsent data and resets without blocking.
        let _ = socket2::SockRef::from(tcp).set_linger(Some(Duration::ZERO));
    }
}

/// The client's TCP socket; `None` for the h2 bridge's in-memory pipe.
fn client_socket(client: &Connection) -> Option<&TcpStream> {
    match client {
        Connection::Plain { inner } => Some(inner),
        Connection::Tls { inner } => Some(inner.get_ref().0),
        Connection::ClientTls { .. } | Connection::Mem { .. } => None,
    }
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
    cfg: &WorkerConfig,
) -> Result<()> {
    let peek_timeout = cfg.client_header_timeout;
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
        cfg.connect_timeout,
        &tcp_keepalive(cfg.tcp_keepalive_time),
        idle_opt(cfg.idle_body_timeout),
        "TLS passthrough",
    )
    .await
}

async fn handle_tcp_connection(
    client: Connection,
    backend_addr: SocketAddr,
    initial_data: &[u8],
    health: Arc<HealthRegistry>,
    connect_timeout: Duration,
    keepalive: &socket2::TcpKeepalive,
    idle: Option<Duration>,
    ctx: &'static str,
) -> Result<()> {
    let mut backend = match connect_tcp(backend_addr, connect_timeout, keepalive).await {
        Ok(backend) => backend,
        // Raw TCP has no status to answer with: the client is just closed.
        Err(ConnectError::Local(e)) => {
            log_local_exhaustion(backend_addr, &e);
            return Ok(());
        }
        Err(e) => {
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
    };
    health.record_success(backend_addr);
    if !initial_data.is_empty() {
        backend.write_all(initial_data).await?;
    }
    relay_l4(client, backend, idle).await;
    Ok(())
}

/// Per-direction copy buffer of the L4 relay: the 8 KiB each side got from
/// `tokio::io::copy_bidirectional`, which it replaces.
const L4_COPY_BUF: usize = 8 * 1024;

/// Copy both ways until both directions end. While both are open the relay
/// is unbounded, like any idle TCP session; once one side has finished, the
/// other must make progress within `idle` (None: unbounded), or a peer that
/// never closes would pin the task and both fds forever. Errors on either
/// side end both: the transfer is best-effort once started.
async fn relay_l4<A, B>(a: A, b: B, idle: Option<Duration>)
where
    A: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut a_rd, mut a_wr) = tokio::io::split(a);
    let (mut b_rd, mut b_wr) = tokio::io::split(b);
    let start = tokio::time::Instant::now();
    let progress = AtomicU64::new(0);
    let a_to_b = pump(&mut a_rd, &mut b_wr, start, &progress);
    let b_to_a = pump(&mut b_rd, &mut a_wr, start, &progress);
    tokio::pin!(a_to_b, b_to_a);
    let a_done = tokio::select! {
        done = &mut a_to_b => if done.is_ok() { true } else { return },
        done = &mut b_to_a => if done.is_ok() { false } else { return },
    };
    if a_done {
        finish_half(b_to_a, idle, start, &progress).await;
    } else {
        finish_half(a_to_b, idle, start, &progress).await;
    }
}

/// Drive the direction still open until it ends, or until `idle` passes
/// without progress.
async fn finish_half<F: std::future::Future>(
    mut half: std::pin::Pin<&mut F>,
    idle: Option<Duration>,
    start: tokio::time::Instant,
    progress: &AtomicU64,
) {
    let Some(idle) = idle else {
        half.await;
        return;
    };
    let stalled_at = || start + Duration::from_nanos(progress.load(Ordering::Relaxed)) + idle;
    loop {
        if tokio::time::timeout_at(stalled_at(), half.as_mut())
            .await
            .is_ok()
        {
            return;
        }
        // Progress made while waiting pushed the deadline out.
        if stalled_at() <= tokio::time::Instant::now() {
            return;
        }
    }
}

/// One direction of an L4 relay: copy until EOF, then pass the half-close
/// on. Every completed step stamps `progress` (nanos since `start`), the
/// clock `finish_half` reads.
async fn pump<R, W>(
    rd: &mut R,
    wr: &mut W,
    start: tokio::time::Instant,
    progress: &AtomicU64,
) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let stamp = || progress.store(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
    let mut buf = vec![0u8; L4_COPY_BUF];
    loop {
        let n = rd.read(&mut buf).await?;
        stamp();
        if n == 0 {
            return wr.shutdown().await;
        }
        wr.write_all(&buf[..n]).await?;
        // TLS holds written records until flushed.
        wr.flush().await?;
        stamp();
    }
}

/// Answer a request the proxy handles itself (redirect, 404, 429, 500, 503)
/// and access-log it. Returns whether the connection stays open; the reply
/// announces `Connection: close` exactly when it does not, and the close
/// then lingers unless the client is done. Staying open matters most for
/// 429: a throttled client's next request would otherwise cost a fresh TLS
/// handshake, more of our CPU than the refused request.
async fn send_local_reply(
    client: &mut Connection,
    state: &mut ConnectionState,
    reply: &LocalReply,
    access: Option<Box<crate::access_log::AccessCapture>>,
) -> Result<bool> {
    let location = reply.location.as_deref();
    let bytes = local_reply_bytes(reply.status, location, !reply.reusable, reply.head);
    state.client_done = reply.client_done;
    if !reply.reusable {
        state.close_after_reply();
    }
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
                    action: crate::routing::BackendAction::Forward,
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
            CancellationToken::new(),
        )
    }

    fn serve(rt: RouteTable) -> (DuplexStream, tokio::task::JoinHandle<Result<()>>) {
        serve_state(conn_state(rt, false))
    }

    /// `serve` for a state the test adjusted first.
    fn serve_state(state: ConnectionState) -> (DuplexStream, tokio::task::JoinHandle<Result<()>>) {
        let (client, engine) = tokio::io::duplex(64 * 1024);
        let peer = "127.0.0.1:40000".parse().unwrap();
        let task = tokio::spawn(handle_connection(
            Connection::Mem {
                inner: engine,
                aborted: Default::default(),
            },
            peer,
            state,
        ));
        (client, task)
    }

    /// `serve`, with the connection draining once `drain` is cancelled.
    fn serve_draining(
        rt: RouteTable,
        drain: CancellationToken,
    ) -> (DuplexStream, tokio::task::JoinHandle<Result<()>>) {
        let mut state = conn_state(rt, false);
        state.drain = drain;
        serve_state(state)
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
                    action: crate::routing::BackendAction::Forward,
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

    /// A backend address that refuses connections. The socket is bound but
    /// not listening; held, it keeps a parallel test from taking the port.
    fn refusing_backend() -> (tokio::net::TcpSocket, SocketAddr) {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = socket.local_addr().unwrap();
        (socket, addr)
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
                action: crate::routing::BackendAction::Forward,
            }],
        );
        rt
    }

    /// Keepalive time, interval and retries of the socket in this process
    /// whose local address is `local`: the proxy end of a backend connection,
    /// as the backend saw its peer. `None` when keepalive is off.
    fn keepalive_of(local: SocketAddr) -> Option<(Duration, Duration, u32)> {
        use std::os::fd::FromRawFd;
        for entry in std::fs::read_dir("/proc/self/fd").unwrap() {
            let Ok(fd) = entry.unwrap().file_name().to_string_lossy().parse() else {
                continue;
            };
            // A dup, so a test closing `fd` meanwhile cannot pull it away.
            let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
            if dup < 0 {
                continue;
            }
            let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(dup) };
            let sock = socket2::SockRef::from(&owned);
            if sock.local_addr().ok().and_then(|a| a.as_socket()) != Some(local) {
                continue;
            }
            return sock.keepalive().unwrap().then(|| {
                (
                    sock.tcp_keepalive_time().unwrap(),
                    sock.tcp_keepalive_interval().unwrap(),
                    sock.tcp_keepalive_retries().unwrap(),
                )
            });
        }
        panic!("no socket bound to {local}");
    }

    /// A backend that vanishes without FIN or RST would otherwise pin an
    /// idle client's task and both fds forever, as a dead client once did.
    #[tokio::test]
    async fn backend_sockets_get_the_client_keepalive() {
        let want = Some((
            Duration::from_secs(60),
            TCP_KEEPALIVE_INTERVAL,
            TCP_KEEPALIVE_RETRIES,
        ));

        // Checked once the first bytes arrive: the proxy sends them only
        // after setting the socket up, while accept returns before.
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (mut client, _task) = serve(routes(http.local_addr().unwrap()));
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n")
            .await
            .unwrap();
        let (mut pooled, proxy_end) = http.accept().await.unwrap();
        pooled.read_exact(&mut [0u8; 4]).await.unwrap();
        assert_eq!(keepalive_of(proxy_end), want, "pooled HTTP backend");

        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let unused = http.local_addr().unwrap();
        let (mut client, _task) = serve(routes_with_tcp(unused, tcp.local_addr().unwrap()));
        client.write_all(OPAQUE).await.unwrap();
        let (mut relayed, proxy_end) = tcp.accept().await.unwrap();
        relayed.read_exact(&mut [0u8; 4]).await.unwrap();
        assert_eq!(keepalive_of(proxy_end), want, "L4 backend");
    }

    /// What a scripted backend does with a request head.
    #[derive(Clone, Copy)]
    enum Turn {
        Answer,
        /// Answer, then close: an idle timeout firing after the response.
        AnswerThenClose,
        /// Close without a byte: an idle timeout firing as the request lands.
        Close,
        Reset,
    }

    /// A backend's script: the turn for request `head` on connection `conn`,
    /// both counted from 0.
    type Script = fn(usize, usize) -> Turn;

    /// Answers request 0 on each connection and drops the next unanswered.
    fn then_close(_conn: usize, head: usize) -> Turn {
        [Turn::Answer, Turn::Close][head.min(1)]
    }

    fn then_reset(_conn: usize, head: usize) -> Turn {
        [Turn::Answer, Turn::Reset][head.min(1)]
    }

    /// Plays `script` on backend connection `conn`; returns how it must end.
    async fn play<S>(sock: &mut S, script: Script, conn: usize, reply: &[u8]) -> Turn
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let (mut seen, mut chunk, mut turn) = (Vec::new(), [0u8; 4096], 0);
        loop {
            match sock.read(&mut chunk).await {
                Ok(n @ 1..) => seen.extend_from_slice(&chunk[..n]),
                _ => return Turn::Close,
            }
            while let Some(end) = find_header_end(&seen) {
                seen.drain(..end);
                let act = script(conn, turn);
                turn += 1;
                if let Turn::Answer | Turn::AnswerThenClose = act {
                    if sock.write_all(reply).await.is_err() {
                        return Turn::Close;
                    }
                }
                if !matches!(act, Turn::Answer) {
                    return act;
                }
            }
        }
    }

    const OK: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";

    /// Plain backend answering OK by `script`.
    async fn scripted_backend(script: Script) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for conn in 0.. {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    if let Turn::Reset = play(&mut sock, script, conn, OK.as_bytes()).await {
                        let _ = socket2::SockRef::from(&sock).set_linger(Some(Duration::ZERO));
                    }
                });
            }
        });
        addr
    }

    /// TLS backend answering by `script` with the SNI the connection was
    /// opened with as the body; closes with close_notify. `None` without
    /// openssl.
    async fn scripted_tls_backend(script: Script) -> Option<SocketAddr> {
        static CERT: std::sync::OnceLock<Option<(Vec<u8>, Vec<u8>)>> = std::sync::OnceLock::new();
        let (cert_pem, key_pem) = CERT
            .get_or_init(|| crate::proxy::tls::test_util::generate_test_cert("backend.test"))
            .clone()?;
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let cert = crate::config::CertificateRef {
            name: "backend".to_string(),
            cert_pem: Some(cert_pem),
            key_pem: Some(key_pem),
            ..Default::default()
        };
        let server_cfg =
            crate::proxy::tls::build_server_config(&[cert], std::path::Path::new("/unused"), false)
                .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_cfg));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for conn in 0.. {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let sni = tls.get_ref().1.server_name().unwrap_or("").to_owned();
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{sni}",
                        sni.len()
                    );
                    match play(&mut tls, script, conn, reply.as_bytes()).await {
                        Turn::Reset => {
                            let tcp = socket2::SockRef::from(tls.get_ref().0);
                            let _ = tcp.set_linger(Some(Duration::ZERO));
                        }
                        _ => {
                            let _ = tls.shutdown().await;
                        }
                    }
                });
            }
        });
        Some(addr)
    }

    /// Each (host, sni) proxies to the TLS backend at `addr` under that SNI.
    fn tls_routes(addr: SocketAddr, hosts: &[(&str, &str)]) -> RouteTable {
        let mut rt = RouteTable::new();
        for (host, sni) in hosts {
            rt.add_http_route(
                host,
                HttpRouteRule::new(
                    PathMatchType::Prefix,
                    "/".to_string(),
                    vec![],
                    vec![],
                    vec![],
                    vec![Backend {
                        socket_addr: addr,
                        weight: 1,
                        filters: vec![],
                        use_tls: true,
                        server_name: sni.to_string(),
                        action: crate::routing::BackendAction::Forward,
                    }],
                ),
            );
        }
        rt
    }

    /// The next response on a kept-open client connection, by its
    /// Content-Length; whatever arrived when the engine closes instead.
    async fn response(client: &mut DuplexStream) -> String {
        let (mut out, mut chunk) = (Vec::new(), [0u8; 4096]);
        loop {
            if let Some(end) = find_header_end(&out) {
                let head = String::from_utf8_lossy(&out[..end]).to_ascii_lowercase();
                let len: usize = head
                    .split("\r\ncontent-length: ")
                    .nth(1)
                    .and_then(|v| v.split("\r\n").next()?.parse().ok())
                    .unwrap_or(0);
                if out.len() >= end + len {
                    let resp = String::from_utf8_lossy(&out[..end + len]).into_owned();
                    out.drain(..end + len);
                    assert!(out.is_empty(), "unexpected bytes after {resp}");
                    return resp;
                }
            }
            let read = tokio::time::timeout(Duration::from_secs(5), client.read(&mut chunk));
            match read.await.expect("no response").unwrap() {
                0 => return String::from_utf8_lossy(&out).into_owned(),
                n => out.extend_from_slice(&chunk[..n]),
            }
        }
    }

    const GET_A: &[u8] = b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n";

    /// Node's https server closes an idle keep-alive conn after 5s with
    /// close_notify and FIN. A pooled TLS conn was never probed, so every
    /// request reusing one met EOF and the client was dropped unanswered.
    #[tokio::test]
    async fn pooled_tls_conn_the_backend_closed_is_not_reused() {
        let Some(addr) = scripted_tls_backend(|_, _| Turn::AnswerThenClose).await else {
            return; // openssl unavailable
        };
        let (mut client, _task) = serve(tls_routes(addr, &[("a.test", "a.test")]));
        for _ in 0..2 {
            client.write_all(GET_A).await.unwrap();
            let out = response(&mut client).await;
            assert!(out.ends_with("\r\n\r\na.test"), "{out}");
            // The backend's close lands before the next request.
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// A pooled conn can also die after the probe, as the request reaches
    /// the backend. Nothing was answered, so an idempotent request whose
    /// body the proxy still holds is replayed once on a fresh conn.
    #[tokio::test]
    async fn idempotent_request_is_replayed_once_when_a_pooled_conn_dies_unanswered() {
        let put = b"PUT / HTTP/1.1\r\nHost: a.test\r\nContent-Length: 2\r\n\r\nhi";
        for script in [then_close, then_reset] {
            for request in [GET_A, put] {
                let (mut client, _task) = serve(routes(scripted_backend(script).await));
                client.write_all(GET_A).await.unwrap();
                assert_eq!(response(&mut client).await, OK);
                client.write_all(request).await.unwrap();
                assert_eq!(response(&mut client).await, OK);
            }
        }

        let Some(addr) = scripted_tls_backend(then_close).await else {
            return; // openssl unavailable
        };
        let (mut client, _task) = serve(tls_routes(addr, &[("a.test", "a.test")]));
        for _ in 0..2 {
            client.write_all(GET_A).await.unwrap();
            let out = response(&mut client).await;
            assert!(out.ends_with("\r\n\r\na.test"), "{out}");
        }
    }

    /// The backend may have acted on a POST before dying, and a streamed
    /// body is gone: no replay, but a 502 rather than a silent close.
    #[tokio::test]
    async fn unreplayable_request_on_a_pooled_conn_that_dies_unanswered_gets_a_502() {
        const BAD_GATEWAY: &str =
            "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 15\r\nConnection: close\r\n\r\n502 Bad Gateway";
        let (mut client, _task) = serve(routes(scripted_backend(then_close).await));
        client.write_all(GET_A).await.unwrap();
        assert_eq!(response(&mut client).await, OK);
        let post = b"POST / HTTP/1.1\r\nHost: a.test\r\nContent-Length: 2\r\n\r\nhi";
        assert_eq!(exchange(&mut client, post).await, BAD_GATEWAY);

        let (mut client, _task) = serve(routes(scripted_backend(then_close).await));
        client.write_all(GET_A).await.unwrap();
        assert_eq!(response(&mut client).await, OK);
        client
            .write_all(b"PUT / HTTP/1.1\r\nHost: a.test\r\nContent-Length: 4\r\n\r\nhi")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(exchange(&mut client, b"hi").await, BAD_GATEWAY);

        // Replayed once only: the fresh conn dying too ends in a 502.
        static CONNS: AtomicUsize = AtomicUsize::new(0);
        let addr = scripted_backend(|conn, head| {
            CONNS.fetch_max(conn + 1, Ordering::Relaxed);
            [Turn::Answer, Turn::Close][usize::from(conn + head > 0)]
        })
        .await;
        let (mut client, _task) = serve(routes(addr));
        client.write_all(GET_A).await.unwrap();
        assert_eq!(response(&mut client).await, OK);
        assert_eq!(exchange(&mut client, GET_A).await, BAD_GATEWAY);
        assert_eq!(CONNS.load(Ordering::Relaxed), 2);
    }

    /// No response byte has gone out while the head is read: whatever
    /// breaks it, the client gets a 502, not a silent close.
    #[tokio::test]
    async fn backend_closing_mid_head_gets_a_502() {
        let addr = closing_backend(b"HTTP/1.1 200 OK\r\nContent-Le").await;
        let (mut client, _task) = serve(routes(addr));
        let out = exchange(&mut client, GET_A).await;
        assert!(out.starts_with("HTTP/1.1 502 Bad Gateway\r\n"), "{out}");
    }

    /// Two routes to one TLS backend under different SNIs: a conn pooled
    /// for one served the other, whose request then met the first one's
    /// certificate and vhost.
    #[tokio::test]
    async fn pooled_tls_conn_serves_only_its_own_sni() {
        let Some(addr) = scripted_tls_backend(|_, _| Turn::Answer).await else {
            return; // openssl unavailable
        };
        let rt = tls_routes(addr, &[("a.test", "a.sni"), ("b.test", "b.sni")]);
        let (mut client, _task) = serve(rt);
        for (host, sni) in [
            ("a.test", "a.sni"),
            ("b.test", "b.sni"),
            ("a.test", "a.sni"),
        ] {
            let request = format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n");
            client.write_all(request.as_bytes()).await.unwrap();
            let out = response(&mut client).await;
            assert!(out.ends_with(&format!("\r\n\r\n{sni}")), "{host}: {out}");
        }
    }

    /// Out of fds, the proxy never reaches the backend: the client gets a
    /// 503 and the backend's health is untouched. Counted as backend
    /// failures, one exhausted proxy ejected healthy backends route-wide.
    #[test]
    fn local_exhaustion_is_a_503_not_a_backend_failure() {
        use crate::proxy::test_util::{in_own_process, FdsExhausted};
        if !in_own_process(
            module_path!(),
            "local_exhaustion_is_a_503_not_a_backend_failure",
        ) {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (addr, _rx) = backend(REFUSED).await;
            let unused = idle_backend().await;
            let health = Arc::new(HealthRegistry::new());
            let serve_shared = |rt| {
                let mut state = conn_state(rt, false);
                state.health = Arc::clone(&health);
                serve_state(state)
            };

            let exhausted = FdsExhausted::new();
            for _ in 0..=crate::proxy::health::FAILURE_THRESHOLD {
                let (mut client, _task) = serve_shared(routes(addr));
                let out = exchange(&mut client, GET_A).await;
                assert!(
                    out.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
                    "{out}"
                );
                // Raw TCP has no status to send: the client is just closed.
                let l4 = routes_with_tcp(unused.local_addr().unwrap(), addr);
                let (mut client, _task) = serve_shared(l4);
                assert_eq!(exchange(&mut client, OPAQUE).await, "");
            }
            assert!(health.is_healthy(&addr));
            drop(exhausted);

            let (mut client, _task) = serve_shared(routes(addr));
            let out = exchange(&mut client, GET_A).await;
            assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "{out}");
        });
    }

    /// Accepts, reads nothing, and resets each connection `after` it.
    async fn resetting_backend(after: Duration) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    tokio::time::sleep(after).await;
                    let _ = socket2::SockRef::from(&sock).set_linger(Some(Duration::ZERO));
                });
            }
        });
        addr
    }

    /// The connect succeeded: a request the fresh conn then fails to send
    /// is a backend failure, a 502 and a send error, but no connect error.
    /// On a conn fresh from the pool's miss, and on the one replacing a
    /// dead pooled conn.
    #[tokio::test]
    async fn a_send_failure_on_a_fresh_conn_is_a_send_error() {
        // Late enough that the connect is done: a reset during it is a
        // connect failure.
        let addr = resetting_backend(Duration::from_millis(100)).await;
        // More than any socket buffer takes: still sending at the reset.
        let large = vec![b'x'; 16 << 20];
        for (from_pool, body) in [(false, &[][..]), (true, &large[..])] {
            let mut state = conn_state(routes(addr), false);
            let backend_ref = Backend {
                socket_addr: addr,
                weight: 1,
                filters: vec![],
                use_tls: false,
                server_name: String::new(),
                action: crate::routing::BackendAction::Forward,
            };
            // A conn whose peer is gone: the first write fails.
            let (dead, _) = tokio::io::duplex(1);
            let dead = Connection::Mem {
                inner: dead,
                aborted: Default::default(),
            };
            let (engine, mut client) = tokio::io::duplex(64 * 1024);
            let mut engine = Connection::Mem {
                inner: engine,
                aborted: Default::default(),
            };
            state.scratch_buf.extend_from_slice(GET_A);
            let headers_len = state.scratch_buf.len();
            let sent = send_request_with_retry(
                &mut engine,
                &mut state,
                &backend_ref,
                dead,
                from_pool,
                headers_len,
                body,
                Duration::from_secs(5),
                std::time::Instant::now(),
                None,
            )
            .await
            .unwrap();
            assert!(sent.is_none());
            drop(engine);
            let out = read_until_closed(&mut client).await;
            assert!(out.starts_with("HTTP/1.1 502 "), "{out}");
            let connect_errors = state.listener_stats.upstream_connect_errors.get();
            assert_eq!(connect_errors, 0, "from_pool={from_pool}");
            let send_errors = state.listener_stats.upstream_send_errors.get();
            assert_eq!(send_errors, 1, "from_pool={from_pool}");
            // One failure was recorded: the rest of the threshold trips it.
            for _ in 2..crate::proxy::health::FAILURE_THRESHOLD {
                assert!(!state.health.record_failure(addr));
            }
            assert!(state.health.record_failure(addr), "from_pool={from_pool}");
        }
        let scrape = crate::metrics::render(true);
        let series = format!("portail_upstream_send_errors_total{{backend=\"{addr}\"}} 2\n");
        assert!(scrape.contains(&series), "{scrape}");
    }

    /// Only a TCP route on the engine's port.
    fn tcp_route(backend: SocketAddr) -> RouteTable {
        let mut rt = RouteTable::new();
        rt.add_tcp_route(
            8080,
            vec![Backend {
                socket_addr: backend,
                weight: 1,
                filters: vec![],
                use_tls: false,
                server_name: String::new(),
                action: crate::routing::BackendAction::Forward,
            }],
        );
        rt
    }

    /// Sends `banner` on accept, as MySQL or SMTP do, then echoes until EOF.
    async fn banner_backend(banner: &'static [u8]) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let (mut rd, mut wr) = sock.split();
                    if wr.write_all(banner).await.is_ok() {
                        let _ = tokio::io::copy(&mut rd, &mut wr).await;
                    }
                });
            }
        });
        addr
    }

    /// MySQL, SMTP, FTP, IMAP and POP3 clients send nothing before the
    /// server's banner: the proxy waiting for their first bytes to sniff
    /// deadlocked them.
    #[tokio::test]
    async fn tcp_only_port_connects_before_the_client_speaks() {
        const BANNER: &[u8] = b"220 smtp.test ESMTP\r\n";
        let (mut client, _task) = serve(tcp_route(banner_backend(BANNER).await));
        let mut banner = [0u8; BANNER.len()];
        tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut banner))
            .await
            .expect("no banner")
            .unwrap();
        assert_eq!(banner, BANNER);
        assert_eq!(exchange(&mut client, b"QUIT\r\n").await, "QUIT\r\n");
    }

    /// A first segment too short for the sniff ("GET", "PROPF") went to TCP
    /// routing, which a port without a TCP route cannot serve: closed
    /// unanswered.
    #[tokio::test]
    async fn http_port_waits_for_a_method_cut_short() {
        let (addr, _rx) = backend(REFUSED).await;
        for (first, rest) in [
            ("GET", " / HTTP/1.1\r\nHost: a.test\r\n\r\n"),
            ("G", "ET / HTTP/1.1\r\nHost: a.test\r\n\r\n"),
            (
                "\r\nPROPF",
                "IND / HTTP/1.1\r\nHost: a.test\r\nContent-Length: 0\r\n\r\n",
            ),
        ] {
            let (mut client, _task) = serve(routes(addr));
            client.write_all(first.as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            let out = exchange(&mut client, rest.as_bytes()).await;
            assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "{first:?}: {out}");
        }
    }

    /// A TCP backend running `serve` on each connection.
    async fn l4_backend<F, Fut>(serve: F) -> SocketAddr
    where
        F: Fn(TcpStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(serve(sock));
            }
        });
        addr
    }

    /// The engine on a TCP-only port relaying to `backend`.
    fn serve_l4(
        backend: SocketAddr,
        idle: Option<Duration>,
    ) -> (DuplexStream, tokio::task::JoinHandle<Result<()>>) {
        let mut state = conn_state(tcp_route(backend), false);
        state.idle_body_timeout = idle;
        serve_state(state)
    }

    const IDLE: Duration = Duration::from_millis(200);

    /// Once one side has finished, a peer that never closes pinned the
    /// relay, its task and both fds forever.
    #[tokio::test]
    async fn half_closed_l4_relay_ends_after_the_idle_budget() {
        // The client is done; the backend never answers nor closes.
        let silent = l4_backend(|mut sock| async move {
            let _ = sock.read_to_end(&mut Vec::new()).await;
            std::future::pending::<()>().await;
        })
        .await;
        let (mut client, task) = serve_l4(silent, Some(IDLE));
        client.write_all(b"QUIT\r\n").await.unwrap();
        client.shutdown().await.unwrap();
        let ended = tokio::time::timeout(IDLE * 10, task).await;
        assert!(ended.is_ok(), "relay pinned after the client's FIN");

        // The backend is done; the client never closes.
        let done = l4_backend(|mut sock| async move {
            let _ = sock.write_all(b"bye").await;
            let _ = sock.shutdown().await;
            std::future::pending::<()>().await;
        })
        .await;
        let (mut client, task) = serve_l4(done, Some(IDLE));
        client.read_exact(&mut [0u8; 3]).await.unwrap();
        let ended = tokio::time::timeout(IDLE * 10, task).await;
        assert!(ended.is_ok(), "relay pinned after the backend's FIN");
    }

    /// The bound is on a stalled half, not on how long it runs; with both
    /// sides open, or the budget disabled, the relay stays unbounded.
    #[tokio::test]
    async fn l4_relay_bounds_only_a_stalled_half() {
        let trickle = l4_backend(|mut sock| async move {
            let _ = sock.read_to_end(&mut Vec::new()).await;
            for _ in 0..5 {
                tokio::time::sleep(IDLE / 2).await;
                if sock.write_all(b"x").await.is_err() {
                    return;
                }
            }
        })
        .await;
        let (mut client, _task) = serve_l4(trickle, Some(IDLE));
        assert_eq!(exchange(&mut client, b"GO").await, "xxxxx");

        let (mut client, task) = serve_l4(banner_backend(b"").await, Some(IDLE));
        tokio::time::sleep(IDLE * 3).await;
        assert!(!task.is_finished(), "relay cut with both sides open");
        assert_eq!(exchange(&mut client, b"ping").await, "ping");

        let silent = l4_backend(|mut sock| async move {
            let _ = sock.read_to_end(&mut Vec::new()).await;
            std::future::pending::<()>().await;
        })
        .await;
        let (mut client, task) = serve_l4(silent, None);
        client.shutdown().await.unwrap();
        tokio::time::sleep(IDLE * 3).await;
        assert!(!task.is_finished(), "relay cut with the budget disabled");
    }

    /// Bytes no method can open still close at once, not after the header
    /// budget.
    #[tokio::test]
    async fn http_port_closes_on_opaque_bytes_without_waiting() {
        let (addr, _rx) = backend(REFUSED).await;
        for opaque in [&b"\x16\x03\x01\x02\x00"[..], b"GET1", b"get / HTTP/1.1\r\n"] {
            let (mut client, task) = serve(routes(addr));
            client.write_all(opaque).await.unwrap();
            let started = tokio::time::Instant::now();
            assert_eq!(read_until_closed(&mut client).await, "");
            task.await.unwrap().unwrap();
            assert!(started.elapsed() < Duration::from_secs(1), "{opaque:?}");
        }
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

    /// Before the first request too, and across a read that ends inside an
    /// empty line.
    #[tokio::test]
    async fn empty_lines_before_the_first_request_are_skipped() {
        let (mut client, _task) = serve(RouteTable::new());
        let out = exchange(&mut client, &[b"\r\n\r\n", UNROUTED].concat()).await;
        assert!(out.starts_with("HTTP/1.1 404 Not Found\r\n"), "{out}");

        let (mut client, _task) = serve(RouteTable::new());
        client.write_all(b"\r").await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let out = exchange(&mut client, &[b"\n", UNROUTED].concat()).await;
        assert!(out.starts_with("HTTP/1.1 404 Not Found\r\n"), "{out}");
    }

    /// Not followed by HTTP, the bytes reach the TCP route as sent.
    #[tokio::test]
    async fn empty_lines_before_opaque_bytes_stay_for_tcp_routing() {
        let (tcp, mut rx) = backend(REFUSED).await;
        // HTTP routes too: a TCP-only port relays without sniffing at all.
        let http = idle_backend().await;
        let (mut client, _task) = serve(routes_with_tcp(http.local_addr().unwrap(), tcp));
        let input = [b"\r\n", OPAQUE].concat();
        let out = exchange(&mut client, &input).await;
        assert_eq!(out.as_bytes(), REFUSED);
        assert_eq!(delivered(&mut rx).await.as_bytes(), input);
    }

    #[tokio::test]
    async fn a_read_ending_inside_an_empty_line_waits_for_the_rest() {
        let (addr, _rx) = backend(REFUSED).await;
        let (mut client, _task) = serve(routes(addr));
        let post = &POST_THEN_CRLF[..POST_THEN_CRLF.len() - 1];
        client.write_all(post).await.unwrap();
        let mut head = vec![0u8; REFUSED.len()];
        client.read_exact(&mut head).await.unwrap();
        assert_eq!(head, REFUSED);
        let out = exchange(&mut client, &[b"\n", UNROUTED].concat()).await;
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
    async fn a_drain_closes_an_idle_keepalive_connection() {
        let (addr, _rx) = backend(REFUSED).await;
        let drain = CancellationToken::new();
        let (mut client, task) = serve_draining(routes(addr), drain.clone());
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n")
            .await
            .unwrap();
        let mut head = vec![0u8; REFUSED.len()];
        client.read_exact(&mut head).await.unwrap();
        assert_eq!(head, REFUSED);
        drain.cancel();
        assert_eq!(read_until_closed(&mut client).await, "");
        task.await.unwrap().unwrap();
    }

    /// Before its first request too: a connection that has sent nothing yet
    /// (a browser's preconnect) must not hold the drain.
    #[tokio::test]
    async fn a_drain_closes_a_connection_that_has_not_spoken() {
        let (addr, _rx) = backend(REFUSED).await;
        let drain = CancellationToken::new();
        let (mut client, task) = serve_draining(routes(addr), drain.clone());
        drain.cancel();
        assert_eq!(read_until_closed(&mut client).await, "");
        task.await.unwrap().unwrap();
    }

    /// The exchange in flight when the drain starts is the last one: its
    /// response announces the close, and a pipelined request is not read.
    #[tokio::test]
    async fn a_drain_lets_the_exchange_in_flight_finish_then_closes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let drain = CancellationToken::new();
        let (mut client, task) =
            serve_draining(routes(listener.local_addr().unwrap()), drain.clone());
        client
            .write_all(
                b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\nGET / HTTP/1.1\r\nHost: nope.test\r\n\r\n",
            )
            .await
            .unwrap();
        let (mut sock, _) = listener.accept().await.unwrap();
        let (mut seen, mut chunk) = (Vec::new(), [0u8; 4096]);
        while find_header_end(&seen).is_none() {
            let n = sock.read(&mut chunk).await.unwrap();
            seen.extend_from_slice(&chunk[..n]);
        }
        drain.cancel();
        sock.write_all(REFUSED).await.unwrap();
        let out = read_until_closed(&mut client).await;
        assert_eq!(
            out,
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nno"
        );
        task.await.unwrap().unwrap();
    }

    /// A head that went out announcing keep-alive decided: a drain that
    /// starts during its body does not close under the client, whose next
    /// request, pipelined or sent meanwhile, is answered with the close.
    /// Closed on it unread, the kernel's reset could also destroy the
    /// response the client was still reading.
    #[tokio::test]
    async fn a_drain_after_a_keepalive_head_answers_the_request_already_sent() {
        const HEAD: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n";
        const NEXT: &[u8] = b"GET / HTTP/1.1\r\nHost: nope.test\r\n\r\n";
        for pipelined in [true, false] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let drain = CancellationToken::new();
            let (mut client, task) =
                serve_draining(routes(listener.local_addr().unwrap()), drain.clone());
            let first = if pipelined {
                [GET_A, NEXT].concat()
            } else {
                GET_A.to_vec()
            };
            client.write_all(&first).await.unwrap();
            let (mut sock, _) = listener.accept().await.unwrap();
            let (mut seen, mut chunk) = (Vec::new(), [0u8; 4096]);
            while find_header_end(&seen).is_none() {
                let n = sock.read(&mut chunk).await.unwrap();
                seen.extend_from_slice(&chunk[..n]);
            }
            sock.write_all(HEAD).await.unwrap();
            let mut head = vec![0u8; HEAD.len()];
            client.read_exact(&mut head).await.unwrap();
            assert_eq!(head, HEAD);

            drain.cancel();
            if !pipelined {
                client.write_all(NEXT).await.unwrap();
            }
            sock.write_all(b"ok").await.unwrap();
            let out = read_until_closed(&mut client).await;
            assert_eq!(
                out,
                "okHTTP/1.1 404 Not Found\r\nContent-Length: 13\r\nConnection: close\r\n\r\n404 Not Found",
                "pipelined={pipelined}"
            );
            task.await.unwrap().unwrap();
        }
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

    /// The Content-Length beside chunked neither ends the body nor reaches
    /// the client.
    #[tokio::test]
    async fn chunked_frames_the_response_over_content_length() {
        let (addr, _rx) = backend(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked \r\nContent-Length: 3\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
        )
        .await;
        let (mut client, _task) = serve(routes(addr));
        let out = exchange(&mut client, b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n").await;
        assert_eq!(
            out,
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked \r\n\r\n5\r\nhello\r\n0\r\n\r\n"
        );
    }

    /// For h1 and h2 clients alike, and whether a body follows or not: the
    /// h2 bridge, which cannot carry a coding, relays this 502.
    #[tokio::test]
    async fn any_transfer_encoding_but_one_chunked_gets_a_502() {
        let get = &b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n"[..];
        let head = &b"HEAD / HTTP/1.1\r\nHost: a.test\r\n\r\n"[..];
        for (te, request) in [
            ("Transfer-Encoding: gzip, chunked\r\n", get),
            (
                "Transfer-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n",
                get,
            ),
            (
                "Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n",
                get,
            ),
            ("Transfer-Encoding: identity\r\n", get),
            ("Transfer-Encoding: ,chunked\r\n", get),
            ("Transfer-Encoding: \r\n", get),
            ("Transfer-Encoding: gzip\r\n", head),
        ] {
            let reply = format!("HTTP/1.1 200 OK\r\n{te}\r\n5\r\nhello\r\n0\r\n\r\n");
            let addr = closing_backend(reply.as_bytes()).await;
            let (mut client, _task) = serve(routes(addr));
            let out = exchange(&mut client, request).await;
            assert_eq!(
                out,
                "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 15\r\nConnection: close\r\n\r\n502 Bad Gateway",
                "{te}"
            );
        }
    }

    type TlsClient = tokio_rustls::client::TlsStream<TcpStream>;

    /// A TLS client connection to the engine serving `rt`, over a real
    /// socket. `None` when openssl is unavailable.
    async fn tls_client(
        rt: RouteTable,
    ) -> Option<(TlsClient, tokio::task::JoinHandle<Result<()>>)> {
        let (cert_pem, key_pem) = crate::proxy::tls::test_util::generate_test_cert("a.test")?;
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
        let front = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let front_addr = front.local_addr().unwrap();
        let engine = tokio::spawn(async move {
            let (tcp, peer) = front.accept().await.unwrap();
            let tls = acceptor.accept(tcp).await.unwrap();
            let state = conn_state(rt, true);
            handle_connection(Connection::Tls { inner: tls }, peer, state).await
        });

        let client_cfg = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(crate::proxy::backend_pool::NoVerifier))
            .with_no_client_auth();
        let tcp = TcpStream::connect(front_addr).await.unwrap();
        let sni = rustls::pki_types::ServerName::try_from("a.test").unwrap();
        let tls = tokio_rustls::TlsConnector::from(Arc::new(client_cfg))
            .connect(sni, tcp)
            .await
            .unwrap();
        Some((tls, engine))
    }

    /// Everything until the engine closes. The read fails when the close
    /// came without close_notify: a strict client (OpenSSL 3, rustls) reads
    /// a bare FIN as a truncation attack.
    async fn tls_read_to_end(tls: &mut TlsClient) -> (std::io::Result<usize>, String) {
        let mut out = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(5), tls.read_to_end(&mut out))
            .await
            .expect("engine never closed the connection");
        (read, String::from_utf8_lossy(&out).into_owned())
    }

    /// Over TLS only close_notify tells a strict client that a
    /// close-delimited body ended rather than was cut.
    #[tokio::test]
    async fn close_delimited_body_over_tls_ends_with_close_notify() {
        let addr = closing_backend(b"HTTP/1.1 200 OK\r\n\r\nall of it").await;
        let Some((mut tls, engine)) = tls_client(routes(addr)).await else {
            return;
        };
        tls.write_all(b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n")
            .await
            .unwrap();
        let (read, out) = tls_read_to_end(&mut tls).await;
        assert!(read.is_ok(), "{read:?} after {out:?}");
        assert!(out.ends_with("Connection: close\r\n\r\nall of it"), "{out}");
        // Done, as a client is once the close arrives: a lingering close
        // waits for this.
        drop(tls);
        engine.await.unwrap().unwrap();
    }

    /// Whatever ends a TLS connection, the close carries close_notify.
    #[tokio::test]
    async fn every_tls_close_sends_close_notify() {
        let (addr, _rx) = backend(REFUSED).await;
        let (_socket, refused) = refusing_backend();
        for (backend, request, status) in [
            (
                addr,
                "GET / HTTP/1.1\r\nHost: a.test\r\nConnection: close\r\n\r\n",
                "200",
            ),
            (addr, "GET / HTTP/1.1\r\n\r\n", "400"),
            (
                addr,
                "POST / HTTP/1.1\r\nHost: nope.test\r\nContent-Length: 2\r\n\r\nhi",
                "404",
            ),
            (refused, "GET / HTTP/1.1\r\nHost: a.test\r\n\r\n", "502"),
        ] {
            let Some((mut tls, engine)) = tls_client(routes(backend)).await else {
                return;
            };
            tls.write_all(request.as_bytes()).await.unwrap();
            let (read, out) = tls_read_to_end(&mut tls).await;
            assert!(read.is_ok(), "{request:?}: {read:?} after {out:?}");
            assert!(out.starts_with(&format!("HTTP/1.1 {status} ")), "{out}");
            // Done, as a client is once the close arrives: a lingering
            // close waits for this.
            drop(tls);
            let _ = engine.await.unwrap();
        }

        // The client ends a kept-open connection: its close_notify is answered.
        let Some((mut tls, engine)) = tls_client(routes(addr)).await else {
            return;
        };
        tls.write_all(b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n")
            .await
            .unwrap();
        let mut head = vec![0u8; REFUSED.len()];
        tls.read_exact(&mut head).await.unwrap();
        tls.shutdown().await.unwrap();
        let (read, out) = tls_read_to_end(&mut tls).await;
        assert!(read.is_ok(), "{read:?} after {out:?}");
        engine.await.unwrap().unwrap();
    }

    /// A plain TCP client connection to the engine serving with `state`:
    /// the kernel resets only real sockets.
    async fn tcp_client(
        state: ConnectionState,
    ) -> (TcpStream, tokio::task::JoinHandle<Result<()>>) {
        let front = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let front_addr = front.local_addr().unwrap();
        let engine = tokio::spawn(async move {
            let (tcp, peer) = front.accept().await.unwrap();
            handle_connection(Connection::Plain { inner: tcp }, peer, state).await
        });
        (TcpStream::connect(front_addr).await.unwrap(), engine)
    }

    /// One response off `client`, by its Content-Length.
    async fn read_one_reply<R: tokio::io::AsyncRead + Unpin>(client: &mut R) -> String {
        let (mut out, mut chunk) = (Vec::new(), [0u8; 4096]);
        loop {
            if let Some(end) = find_header_end(&out) {
                let head = String::from_utf8_lossy(&out[..end]).to_ascii_lowercase();
                let len = head
                    .split("content-length: ")
                    .nth(1)
                    .and_then(|v| v.split("\r\n").next()?.parse::<usize>().ok())
                    .unwrap();
                if out.len() >= end + len {
                    return String::from_utf8_lossy(&out).into_owned();
                }
            }
            let n = client.read(&mut chunk).await.unwrap();
            assert!(n > 0, "closed mid reply: {}", String::from_utf8_lossy(&out));
            out.extend_from_slice(&chunk[..n]);
        }
    }

    /// The proxy answers before the client is done sending (unrouted host,
    /// backend down). Closing on bytes still coming resets the connection:
    /// a client still uploading (curl) then fails its send and never reads
    /// the reply. The close reads them off instead.
    #[tokio::test]
    async fn a_closing_reply_lingers_while_the_client_still_sends() {
        let (_socket, refused) = refusing_backend();
        let body = [b'x'; 32 * 1024];
        for (host, status) in [("nope.test", "404"), ("a.test", "502")] {
            let (mut client, engine) = tcp_client(conn_state(routes(refused), false)).await;
            let head = format!(
                "POST / HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            client.write_all(head.as_bytes()).await.unwrap();
            client.write_all(&body[..1024]).await.unwrap();
            let reply = read_one_reply(&mut client).await;
            assert!(reply.starts_with(&format!("HTTP/1.1 {status} ")), "{reply}");
            assert!(reply.contains("\r\nConnection: close\r\n"), "{reply}");

            client
                .write_all(&body[1024..])
                .await
                .expect("the close reset the upload");
            client.shutdown().await.unwrap();
            engine.await.unwrap().unwrap();
            assert!(client.take_error().unwrap().is_none(), "{host}: reset");
            assert_eq!(client.read(&mut [0u8; 1]).await.unwrap(), 0);
        }
    }

    /// A client that asked to close, its request read whole, sends nothing
    /// more: whatever the reply, the close does not wait on it, so a client
    /// keeping its socket open cannot hold it.
    #[tokio::test]
    async fn a_closing_reply_to_a_client_done_does_not_linger() {
        let budget = Duration::from_secs(2);
        let (addr, _rx) = backend(REFUSED).await;
        let (_socket, refused) = refusing_backend();
        for (backend, request, status) in [
            (
                addr,
                "GET / HTTP/1.1\r\nHost: nope.test\r\nConnection: close\r\n\r\n",
                "404",
            ),
            (
                addr,
                "GET / HTTP/1.1\r\nHost: a.test\r\nConnection: close\r\n\r\n",
                "200",
            ),
            (addr, "GET / HTTP/1.0\r\nHost: a.test\r\n\r\n", "200"),
            (
                addr,
                "POST / HTTP/1.1\r\nHost: a.test\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi",
                "200",
            ),
            (
                refused,
                "GET / HTTP/1.1\r\nHost: a.test\r\nConnection: close\r\n\r\n",
                "502",
            ),
        ] {
            let mut state = conn_state(routes(backend), false);
            state.client_header_timeout = budget;
            let (mut client, engine) = tcp_client(state).await;
            client.write_all(request.as_bytes()).await.unwrap();
            let reply = read_one_reply(&mut client).await;
            assert!(reply.starts_with(&format!("HTTP/1.1 {status} ")), "{reply}");
            let ended = tokio::time::timeout(budget / 2, engine).await;
            let ended = ended.unwrap_or_else(|_| panic!("{status} to {request:?} lingered"));
            ended.unwrap().unwrap();
        }
    }

    /// A drain closes a keep-alive client after the reply the proxy writes
    /// itself, though its request was read whole: the requests it pipelined
    /// behind, more than one read takes, land unread. Closed on them, the
    /// kernel would reset, and a reset can destroy the reply not read yet.
    #[tokio::test]
    async fn a_drain_closing_a_pipelining_client_keeps_the_reply_intact() {
        let drain = CancellationToken::new();
        let mut state = conn_state(RouteTable::new(), false);
        state.drain = drain.clone();
        let (mut client, engine) = tcp_client(state).await;
        // The drain starts while the head arrives.
        client.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        drain.cancel();
        // Past the engine's 64 KiB buffer, and within one more of it.
        let pipelined = b"GET / HTTP/1.1\r\nHost: nope.test\r\n\r\n".repeat(2048);
        let rest = [&b"Host: nope.test\r\n\r\n"[..], &pipelined].concat();
        client.write_all(&rest).await.unwrap();

        let mut out = Vec::new();
        let read = client.read_to_end(&mut out).await;
        let out = String::from_utf8_lossy(&out);
        assert!(read.is_ok(), "{read:?} after {out}");
        assert_eq!(
            out,
            "HTTP/1.1 404 Not Found\r\nContent-Length: 13\r\nConnection: close\r\n\r\n404 Not Found"
        );
        assert!(client.take_error().unwrap().is_none(), "reset");
        drop(client);
        engine.await.unwrap().unwrap();
    }

    /// A client that asked to close yet sent past its request may still be
    /// sending: whatever answers it, the close reads off what it pipelined.
    /// Closed on that unread, the kernel would reset, and a reset can
    /// destroy the reply not read yet.
    #[tokio::test]
    async fn a_client_sending_past_its_close_is_not_done() {
        let (addr, _rx) = backend(REFUSED).await;
        // Past the engine's 64 KiB buffer, and within one more of it.
        let pipelined = b"GET / HTTP/1.1\r\nHost: nope.test\r\n\r\n".repeat(2048);
        for (host, reply) in [
            (
                "nope.test",
                "HTTP/1.1 404 Not Found\r\nContent-Length: 13\r\nConnection: close\r\n\r\n404 Not Found",
            ),
            (
                "a.test",
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nno",
            ),
        ] {
            let (mut client, engine) = tcp_client(conn_state(routes(addr), false)).await;
            let head = format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
            client
                .write_all(&[head.as_bytes(), &pipelined].concat())
                .await
                .unwrap();
            let mut out = Vec::new();
            let read = client.read_to_end(&mut out).await;
            let out = String::from_utf8_lossy(&out);
            assert!(read.is_ok(), "{host}: {read:?} after {out}");
            assert_eq!(out, reply);
            assert!(client.take_error().unwrap().is_none(), "{host}: reset");
            drop(client);
            engine.await.unwrap().unwrap();
        }
    }

    /// The proxy closes after a proxied response for its own reasons (a
    /// drain, a close-delimited body): the next request, pipelined while
    /// the backend worked, lands unread. A reset on it can destroy the
    /// response not read yet, and passes a close-delimited body off as cut.
    #[tokio::test]
    async fn a_proxied_response_that_closes_survives_a_pipelined_request() {
        const NEXT: &[u8] = b"GET /next HTTP/1.1\r\nHost: a.test\r\n\r\n";
        for (draining, reply, body) in [
            (
                true,
                REFUSED,
                "\r\nContent-Length: 2\r\nConnection: close\r\n\r\nno",
            ),
            (
                false,
                &b"HTTP/1.1 200 OK\r\n\r\nall of it"[..],
                "\r\nConnection: close\r\n\r\nall of it",
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let drain = CancellationToken::new();
            let mut state = conn_state(routes(listener.local_addr().unwrap()), false);
            state.drain = drain.clone();
            let (mut client, engine) = tcp_client(state).await;
            client.write_all(GET_A).await.unwrap();
            let (mut sock, _) = listener.accept().await.unwrap();
            let (mut seen, mut chunk) = (Vec::new(), [0u8; 4096]);
            while find_header_end(&seen).is_none() {
                let n = sock.read(&mut chunk).await.unwrap();
                seen.extend_from_slice(&chunk[..n]);
            }
            if draining {
                drain.cancel();
            }
            client.write_all(NEXT).await.unwrap();
            sock.write_all(reply).await.unwrap();
            drop(sock);

            let mut out = Vec::new();
            let read = client.read_to_end(&mut out).await;
            let out = String::from_utf8_lossy(&out);
            assert!(read.is_ok(), "draining={draining}: {read:?} after {out}");
            assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "{out}");
            assert!(out.ends_with(body), "{out}");
            assert!(client.take_error().unwrap().is_none(), "reset");
            drop(client);
            engine.await.unwrap().unwrap();
        }
    }

    /// Reads one request head off a backend connection.
    async fn read_request_head(sock: &mut TcpStream) -> Vec<u8> {
        let (mut seen, mut chunk) = (Vec::new(), [0u8; 4096]);
        while find_header_end(&seen).is_none() {
            let n = sock.read(&mut chunk).await.unwrap();
            assert!(n > 0, "closed mid head");
            seen.extend_from_slice(&chunk[..n]);
        }
        seen
    }

    /// Bytes a backend sends past its response's framing cost it its conn,
    /// never pooled, but not the client its own: the excess never reached
    /// it. Whether the excess came with the head or in the body.
    #[tokio::test]
    async fn backend_overshoot_drops_the_backend_conn_not_the_client() {
        const HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n";
        for with_head in [true, false] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (mut client, _task) = serve(routes(listener.local_addr().unwrap()));
            client.write_all(GET_A).await.unwrap();
            let (mut sock, _) = listener.accept().await.unwrap();
            read_request_head(&mut sock).await;
            let out = if with_head {
                sock.write_all(format!("{HEAD}okEXTRA").as_bytes())
                    .await
                    .unwrap();
                response(&mut client).await
            } else {
                // The head goes out alone: the excess comes in the body.
                sock.write_all(HEAD.as_bytes()).await.unwrap();
                let mut head = vec![0u8; HEAD.len()];
                client.read_exact(&mut head).await.unwrap();
                sock.write_all(b"okEXTRA").await.unwrap();
                let mut body = [0u8; 2];
                client.read_exact(&mut body).await.unwrap();
                String::from_utf8_lossy(&[&head[..], &body].concat()).into_owned()
            };
            assert_eq!(out, OK, "with_head={with_head}");

            let mut byte = [0u8; 1];
            let closed = tokio::time::timeout(Duration::from_secs(5), sock.read(&mut byte));
            let closed = closed.await.expect("the backend conn was kept");
            assert_eq!(closed.unwrap(), 0, "with_head={with_head}");

            client.write_all(GET_A).await.unwrap();
            let (mut sock, _) = listener.accept().await.unwrap();
            read_request_head(&mut sock).await;
            sock.write_all(OK.as_bytes()).await.unwrap();
            assert_eq!(response(&mut client).await, OK, "with_head={with_head}");
        }
    }

    /// A request pipelined while the backend overshoots its answer to the
    /// one before gets its own response, from a fresh backend conn, and
    /// the excess never reaches the client. Whether the request came with
    /// the one before or while the backend worked.
    #[tokio::test]
    async fn backend_overshoot_spares_a_pipelined_request() {
        const GET_B: &[u8] = b"GET /b HTTP/1.1\r\nHost: a.test\r\n\r\n";
        const B: &str = "HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nB";
        for together in [true, false] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (mut client, _task) = serve(routes(listener.local_addr().unwrap()));
            if together {
                client.write_all(&[GET_A, GET_B].concat()).await.unwrap();
            } else {
                client.write_all(GET_A).await.unwrap();
            }
            let (mut sock, _) = listener.accept().await.unwrap();
            assert!(read_request_head(&mut sock).await.starts_with(b"GET / "));
            if !together {
                client.write_all(GET_B).await.unwrap();
            }
            sock.write_all(format!("{OK}EXTRA").as_bytes())
                .await
                .unwrap();
            assert_eq!(response(&mut client).await, OK, "together={together}");

            let accept = tokio::time::timeout(Duration::from_secs(5), listener.accept());
            let (mut sock, _) = accept
                .await
                .expect("B went to the overshooting conn")
                .unwrap();
            assert!(read_request_head(&mut sock).await.starts_with(b"GET /b "));
            sock.write_all(B.as_bytes()).await.unwrap();
            assert_eq!(response(&mut client).await, B, "together={together}");
        }
    }

    /// Reads a request head, answers `reply` without reading the body, and
    /// closes, resetting at once when `reset`. `answered` fires then.
    async fn early_backend(
        reply: &'static [u8],
        reset: bool,
    ) -> (SocketAddr, tokio::sync::oneshot::Receiver<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (answered, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // A byte at a time: the body stays unread, as at a server
            // that refuses the request from its head.
            let mut head = Vec::new();
            while find_header_end(&head).is_none() {
                let mut byte = [0u8];
                if sock.read(&mut byte).await.unwrap() == 0 {
                    return;
                }
                head.push(byte[0]);
            }
            sock.write_all(reply).await.unwrap();
            if reset {
                let _ = socket2::SockRef::from(&sock).set_linger(Some(Duration::ZERO));
            }
            drop(sock);
            let _ = answered.send(());
        });
        (addr, rx)
    }

    /// A backend may answer before it has the whole body (413, 401) and
    /// stop reading it. Its answer reaches the client, still uploading, in
    /// place of a 502; the connection then closes.
    #[tokio::test]
    async fn an_early_answer_to_an_upload_reaches_the_client() {
        const TOO_LARGE: &[u8] =
            b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 8\r\n\r\ntoo much";
        for reset in [false, true] {
            let (addr, answered) = early_backend(TOO_LARGE, reset).await;
            let (mut client, engine) = tcp_client(conn_state(routes(addr), false)).await;
            client
                .write_all(b"POST / HTTP/1.1\r\nHost: a.test\r\nContent-Length: 65536\r\n\r\n")
                .await
                .unwrap();
            client.write_all(&[b'x'; 1024]).await.unwrap();
            answered.await.unwrap();
            // More body after the backend's close, in separate sends: after
            // a FIN, only the send after the one its kernel resets fails.
            for _ in 0..4 {
                client.write_all(&[b'x'; 8 * 1024]).await.unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let reply = read_one_reply(&mut client).await;
            assert_eq!(
                reply,
                "HTTP/1.1 413 Payload Too Large\r\nContent-Length: 8\r\nConnection: close\r\n\r\ntoo much",
                "reset={reset}"
            );
            // The unread rest of the upload is read off, not reset on.
            client.shutdown().await.unwrap();
            engine.await.unwrap().unwrap();
            assert!(client.take_error().unwrap().is_none(), "reset={reset}");
            assert_eq!(client.read(&mut [0u8; 1]).await.unwrap(), 0);
        }
    }

    /// Nothing answered when the backend stopped taking the body: a 502,
    /// with no fresh budget for an answer to come in.
    #[tokio::test]
    async fn a_backend_that_stops_reading_unanswered_gets_a_502_at_once() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut state = conn_state(routes(listener.local_addr().unwrap()), false);
        state.idle_body_timeout = Some(IDLE);
        let (client, _engine) = tcp_client(state).await;
        let (mut rd, mut wr) = client.into_split();
        // More than the sockets between proxy and backend hold.
        let uploader = tokio::spawn(async move {
            let head = b"POST / HTTP/1.1\r\nHost: a.test\r\nContent-Length: 67108864\r\n\r\n";
            let chunk = [b'x'; 64 * 1024];
            if wr.write_all(head).await.is_ok() {
                while wr.write_all(&chunk).await.is_ok() {}
            }
        });
        let (_stalled, _) = listener.accept().await.unwrap();
        let started = std::time::Instant::now();
        let reply = tokio::time::timeout(Duration::from_secs(2), read_one_reply(&mut rd))
            .await
            .expect("waited on the backend for an answer");
        assert!(reply.starts_with("HTTP/1.1 502 "), "{reply}");
        assert!(started.elapsed() >= IDLE, "{:?}", started.elapsed());
        uploader.abort();
    }

    /// Lingering is bounded: by the header budget, here below LINGER_IDLE,
    /// for a client that sends nothing more and never closes, and by one
    /// buffer's worth of bytes for a client that keeps sending.
    #[tokio::test]
    async fn a_lingering_close_is_bounded() {
        let head = b"POST / HTTP/1.1\r\nHost: nope.test\r\nContent-Length: 999999999\r\n\r\n";
        let budget = Duration::from_millis(500);
        let state = || {
            let mut state = conn_state(RouteTable::new(), false);
            state.client_header_timeout = budget;
            state
        };

        let (mut client, engine) = tcp_client(state()).await;
        client.write_all(head).await.unwrap();
        let started = std::time::Instant::now();
        engine.await.unwrap().unwrap();
        let elapsed = started.elapsed();
        assert!(elapsed >= budget && elapsed < budget * 3, "{elapsed:?}");

        let (mut client, engine) = tcp_client(state()).await;
        client.write_all(head).await.unwrap();
        let sender = tokio::spawn(async move {
            let chunk = [b'x'; 16 * 1024];
            while client.write_all(&chunk).await.is_ok() {}
        });
        let started = std::time::Instant::now();
        engine.await.unwrap().unwrap();
        assert!(started.elapsed() < budget / 2, "{:?}", started.elapsed());
        sender.await.unwrap();
    }

    /// A client told to close that keeps its socket open, as urllib3 and
    /// HttpURLConnection pools do, held its fd and task, and a drain, for
    /// the whole header budget. Silent for LINGER_IDLE, it is done.
    /// Real time: a paused clock jumps to the next timer whenever the
    /// runtime parks on loopback I/O, and could expire the header budget.
    #[tokio::test]
    async fn a_lingering_close_ends_once_the_client_falls_silent() {
        let budget = LINGER_IDLE * 6;
        let mut state = conn_state(RouteTable::new(), false);
        state.client_header_timeout = budget;
        let (mut client, engine) = tcp_client(state).await;
        let started = std::time::Instant::now();
        client
            .write_all(b"POST / HTTP/1.1\r\nHost: nope.test\r\nContent-Length: 9\r\n\r\n")
            .await
            .unwrap();
        engine.await.unwrap().unwrap();
        let elapsed = started.elapsed();
        assert!(elapsed >= LINGER_IDLE && elapsed < budget, "{elapsed:?}");
        let mut out = Vec::new();
        client.read_to_end(&mut out).await.unwrap();
        let out = String::from_utf8_lossy(&out);
        assert!(out.starts_with("HTTP/1.1 404 "), "{out}");
    }

    /// While the client keeps sending, the close reads on: until it falls
    /// silent for LINGER_IDLE, or the close budget runs out.
    #[tokio::test(start_paused = true)]
    async fn a_lingering_close_reads_while_the_client_sends() {
        let budget = LINGER_IDLE * 6;
        let every = LINGER_IDLE / 2;
        for (sends, lingered) in [
            (0, LINGER_IDLE),
            (4, every * 4 + LINGER_IDLE),
            (usize::MAX, budget),
        ] {
            let (engine, mut client) = tokio::io::duplex(64 * 1024);
            let mut engine = Connection::Mem {
                inner: engine,
                aborted: Default::default(),
            };
            let sender = tokio::spawn(async move {
                for _ in 0..sends {
                    tokio::time::sleep(every).await;
                    let _ = client.write_all(b"x").await;
                }
                // Never closes.
                std::future::pending::<()>().await;
            });
            let started = tokio::time::Instant::now();
            read_off(&mut engine, &mut [0u8; 64 * 1024], started + budget).await;
            assert_eq!(started.elapsed(), lingered, "{sends} sends");
            sender.abort();
        }
    }

    /// A client that used its whole header budget gets its 408 and the close
    /// at once: a linger would hold a slow loris's fd and task for another
    /// budget.
    #[tokio::test]
    async fn a_408_closes_without_lingering() {
        let budget = Duration::from_millis(500);
        let mut state = conn_state(RouteTable::new(), false);
        state.client_header_timeout = budget;
        let (mut client, engine) = tcp_client(state).await;
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: a.test\r\n")
            .await
            .unwrap();
        let started = std::time::Instant::now();
        assert!(engine.await.unwrap().is_err());
        let elapsed = started.elapsed();
        assert!(elapsed < budget + budget / 2, "{elapsed:?}");
        let reply = read_one_reply(&mut client).await;
        assert!(reply.starts_with("HTTP/1.1 408 "), "{reply}");
    }

    /// A head that outgrows the buffer may still be on its way: its 431
    /// closes as any closing reply does, reading off what comes.
    #[tokio::test]
    async fn a_431_lingers_while_the_client_still_sends() {
        let (mut client, engine) = tcp_client(conn_state(RouteTable::new(), false)).await;
        let pad = format!("X-Pad: {}\r\n", "p".repeat(1022));
        let head = format!("GET / HTTP/1.1\r\nHost: a.test\r\n{}", pad.repeat(80));
        client.write_all(head.as_bytes()).await.unwrap();
        let reply = read_one_reply(&mut client).await;
        assert!(reply.starts_with("HTTP/1.1 431 "), "{reply}");
        client.shutdown().await.unwrap();
        assert!(engine.await.unwrap().is_err());
        assert!(client.take_error().unwrap().is_none(), "reset");
        assert_eq!(client.read(&mut [0u8; 1]).await.unwrap(), 0);
    }

    /// A client whose connection fails mid head (a reset, a TLS record that
    /// does not decode) outgrew nothing: no 431 is written, logged, or
    /// lingered on.
    #[tokio::test]
    async fn a_client_error_mid_head_is_no_431() {
        let front = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(front.local_addr().unwrap())
            .await
            .unwrap();
        let (engine_end, _) = front.accept().await.unwrap();
        socket2::SockRef::from(&client)
            .set_linger(Some(Duration::ZERO))
            .unwrap();
        drop(client);

        let head = b"GET / HTTP/1.1\r\nHost: a.test\r\n";
        let mut buf = vec![0u8; 65536];
        buf[..head.len()].copy_from_slice(head);
        let mut conn = Connection::Plain { inner: engine_end };
        let mut state = conn_state(RouteTable::new(), false);
        let read = complete_headers(&mut conn, &mut buf, head.len(), &mut state).await;
        let err = read.unwrap_err();
        let kind = err.downcast_ref::<std::io::Error>().map(|e| e.kind());
        assert_eq!(kind, Some(std::io::ErrorKind::ConnectionReset), "{err}");
        assert_eq!(state.last_status, 0);
        assert!(!state.linger);
    }

    /// A body cut short after its head went out must still reset: a clean
    /// close_notify would pass it off as complete.
    #[tokio::test]
    async fn a_truncated_tls_response_still_resets() {
        let addr = closing_backend(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npartial").await;
        let Some((mut tls, engine)) = tls_client(routes(addr)).await else {
            return;
        };
        tls.write_all(b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n")
            .await
            .unwrap();
        let (read, out) = tls_read_to_end(&mut tls).await;
        let kind = read.as_ref().map_err(std::io::Error::kind);
        assert_eq!(kind, Err(std::io::ErrorKind::ConnectionReset), "{out}");
        engine.await.unwrap().unwrap();
    }

    /// Over plain TCP too, a body cut short resets, while one that ends at
    /// the backend's close ends with the client's FIN.
    #[tokio::test]
    async fn a_truncated_plain_response_resets_and_a_closed_one_ends() {
        for (reply, ends) in [
            (
                &b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npartial"[..],
                false,
            ),
            (b"HTTP/1.1 200 OK\r\n\r\nall of it", true),
        ] {
            let addr = closing_backend(reply).await;
            let (mut client, engine) = tcp_client(conn_state(routes(addr), false)).await;
            client.write_all(GET_A).await.unwrap();
            let mut out = Vec::new();
            let read = client.read_to_end(&mut out).await;
            let out = String::from_utf8_lossy(&out);
            match read {
                Ok(_) => assert!(ends && out.ends_with("all of it"), "{out}"),
                Err(e) => {
                    assert!(!ends, "{e}: {out}");
                    assert_eq!(e.kind(), std::io::ErrorKind::ConnectionReset);
                }
            }
            drop(client);
            engine.await.unwrap().unwrap();
        }
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
        // client as one canonical line: some clients (llhttp) refuse any
        // repeat.
        let addr = closing_backend(
            b"HTTP/1.1 200 OK\r\ncontent-length: 05\r\nX-A: 1\r\nContent-Length: 5\r\n\r\nhello",
        )
        .await;
        let (mut client, _task) = serve(routes(addr));
        let out = exchange(&mut client, b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n").await;
        assert_eq!(
            out,
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-A: 1\r\n\r\nhello"
        );
    }

    /// An interim head is never forwarded, but a lenient reader could still
    /// split it differently and take its tail for the final response.
    #[tokio::test]
    async fn malformed_interim_heads_get_a_502() {
        const FINAL: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
        for interim in [
            "HTTP/1.1 100 Continue\r\nX-Bad\r\n\r\n",
            "HTTP/1.1 103 Early Hints\r\nLink : </a>\r\n\r\n",
            "HTTP/1.1 100 Continue\r\nX-A: 1\rX-B: 2\r\n\r\n",
        ] {
            let addr = closing_backend(format!("{interim}{FINAL}").as_bytes()).await;
            let (mut client, _task) = serve(routes(addr));
            let out = exchange(&mut client, b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n").await;
            assert!(
                out.starts_with("HTTP/1.1 502 Bad Gateway\r\n"),
                "{interim:?}: {out}"
            );
        }
        let addr = closing_backend(
            format!("HTTP/1.1 103 Early Hints\r\nLink: </a>\r\n\r\n{FINAL}").as_bytes(),
        )
        .await;
        let (mut client, _task) = serve(routes(addr));
        let out = exchange(&mut client, b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n").await;
        assert_eq!(out, FINAL);
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
    /// `Transfer-Encoding: chunked`, the one coding a response may carry.
    Chunked,
    /// Ends at the backend's close: neither Transfer-Encoding nor
    /// Content-Length.
    Close,
}

/// The one reading of a response head that the body relay, the sanitizer
/// and the h2 bridge all follow: two readings could disagree on where the
/// body ends, and the client would take the rest as the next response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResponseHead {
    pub(crate) status: u16,
    /// Chunked overrides a Content-Length, which must then not be
    /// forwarded either.
    pub(crate) body: BodyFraming,
    /// An explicit Connection: close, or HTTP/1.0 without keep-alive.
    pub(crate) backend_close: bool,
}

/// `None` when a recipient could frame the head differently than this
/// reading: a status line other than `HTTP/1.x SP 3DIGIT [SP reason]`, a
/// Content-Length other than one `1*DIGIT` value (repeated lines must
/// agree), a Transfer-Encoding other than one `chunked` line or on HTTP/1.0,
/// or a line a client could split or name otherwise (bare CR, no colon, a
/// field name that is no token, which covers obs-fold). Lines are split
/// exactly as the sanitizer splits them.
pub(crate) fn parse_response_head(head: &[u8]) -> Option<ResponseHead> {
    let mut lines = header_lines(head);
    let (status, is_http10) = parse_status_line(lines.next()?)?;
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
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
            // The request parser's rule: a coding, list or repeat is where
            // readers disagree on the body's end, and h2 carries none.
            if chunked || !trim_ows(value).eq_ignore_ascii_case(b"chunked") {
                return None;
            }
            chunked = true;
        } else if name.eq_ignore_ascii_case(b"connection") {
            for token in value.split(|&b| b == b',').map(trim_ows) {
                close |= token.eq_ignore_ascii_case(b"close");
                keep_alive |= token.eq_ignore_ascii_case(b"keep-alive");
            }
        }
    }

    // RFC 9112 6.1: HTTP/1.0 has no transfer codings, so its framing is
    // faulty whatever the rest of the head says.
    if chunked && is_http10 {
        return None;
    }
    let body = match (chunked, content_length) {
        (true, _) => BodyFraming::Chunked,
        (false, Some(n)) => BodyFraming::Length(n),
        (false, None) => BodyFraming::Close,
    };
    Some(ResponseHead {
        status,
        body,
        backend_close: close || (is_http10 && !keep_alive),
    })
}

/// The status code and whether the version is HTTP/1.0, from a status line
/// without its CRLF. `HTTP/1.x SP 3DIGIT`, then nothing or SP and a reason
/// of field-value bytes: a CTL there is where a client may end the line.
/// No code below 100: a client may read one as interim, so bodiless.
fn parse_status_line(line: &[u8]) -> Option<(u16, bool)> {
    let [b'H', b'T', b'T', b'P', b'/', b'1', b'.', minor, b' ', d0 @ b'1'..=b'9', d1, d2, reason @ ..] =
        line
    else {
        return None;
    };
    if !(minor.is_ascii_digit() && d1.is_ascii_digit() && d2.is_ascii_digit())
        || !matches!(reason, [] | [b' ', ..])
        || !reason.iter().all(|&b| is_field_value_byte(b))
    {
        return None;
    }
    let status = u16::from(d0 - b'0') * 100 + u16::from(d1 - b'0') * 10 + u16::from(d2 - b'0');
    Some((status, *minor == b'0'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_backoff_doubles_to_a_cap_and_resets_on_success() {
        let emfile = std::io::Error::from_raw_os_error(libc::EMFILE);
        let mut backoff = AcceptBackoff::default();
        let pauses: Vec<u128> = (0..10)
            .map(|_| backoff.after(&emfile).unwrap().as_millis())
            .collect();
        assert_eq!(pauses, [5, 10, 20, 40, 80, 160, 320, 640, 1000, 1000]);
        backoff.reset();
        assert_eq!(backoff.after(&emfile), Some(Duration::from_millis(5)));
        // Only that connection failed: retried at once, schedule kept.
        let aborted = std::io::Error::from(std::io::ErrorKind::ConnectionAborted);
        assert_eq!(backoff.after(&aborted), None);
        assert_eq!(backoff.after(&emfile), Some(Duration::from_millis(10)));
    }

    /// Linux accept(2) returns the queued connection's pending network
    /// error; its man page says to retry those at once, like EAGAIN.
    #[test]
    fn accept_retries_a_queued_connections_network_error_at_once() {
        let emfile = std::io::Error::from_raw_os_error(libc::EMFILE);
        let mut backoff = AcceptBackoff::default();
        assert_eq!(backoff.after(&emfile), Some(Duration::from_millis(5)));
        for errno in [
            libc::ENETDOWN,
            libc::EPROTO,
            libc::ENOPROTOOPT,
            libc::EHOSTDOWN,
            libc::ENONET,
            libc::EHOSTUNREACH,
            libc::EOPNOTSUPP,
            libc::ENETUNREACH,
        ] {
            let error = std::io::Error::from_raw_os_error(errno);
            assert_eq!(backoff.after(&error), None, "{error}");
        }
        assert_eq!(backoff.after(&emfile), Some(Duration::from_millis(10)));
    }

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
        (
            Connection::Mem {
                inner: engine,
                aborted: Default::default(),
            },
            peer,
        )
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
        let parsed = parse_response_head(head).unwrap();
        let framing = frame_response(parsed, head, head.len(), false, false);
        assert!(framing.chunked.is_some());
        assert_eq!(framing.content_length, None);
    }

    fn head_framing(fields: &str) -> Option<BodyFraming> {
        parse_response_head(format!("HTTP/1.1 200 OK\r\n{fields}\r\n").as_bytes()).map(|h| h.body)
    }

    #[test]
    fn response_framing_is_chunked_a_length_or_the_close() {
        use BodyFraming::{Chunked, Close, Length};
        for (fields, want) in [
            ("Content-Length: 5\r\n", Length(5)),
            ("Content-Length:5 \r\nContent-Length: 5\r\n", Length(5)),
            ("Content-Length: 007\r\n", Length(7)),
            ("", Close),
            ("Transfer-Encoding: chunked\r\n", Chunked),
            ("Transfer-Encoding: \tChunked \r\n", Chunked),
            (
                "Transfer-Encoding: CHUNKED\r\nContent-Length: 9\r\n",
                Chunked,
            ),
        ] {
            assert_eq!(head_framing(fields), Some(want), "{fields:?}");
        }
    }

    /// The request parser's rule: one line, its value `chunked`. Codings,
    /// lists and repeats are where readers disagree on where a body ends.
    #[test]
    fn transfer_encoding_is_one_chunked_or_malformed() {
        for fields in [
            "Transfer-Encoding: gzip, chunked\r\n",
            "Transfer-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n",
            "Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n",
            "Transfer-Encoding: chunked, chunked\r\n",
            "Transfer-Encoding: chunked, gzip\r\n",
            "Transfer-Encoding: identity\r\nContent-Length: 5\r\n",
            "Transfer-Encoding: chunked;x=1\r\n",
            "Transfer-Encoding: ,chunked\r\n",
            "Transfer-Encoding: chunked, \r\n",
            "Transfer-Encoding: \r\nContent-Length: 5\r\n",
        ] {
            assert_eq!(head_framing(fields), None, "{fields:?}");
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
            (b"HTTP/1.1 200 O\tK", Some((200, false))),
            (b"HTTP/1.1 200 \xc3\xa9t\xc3\xa9", Some((200, false))),
            (b"HTTP/1.1 200 O\x00K", None),
            (b"HTTP/1.1 200 O\x0bK", None),
            (b"HTTP/1.1 200 O\x7fK", None),
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
        let head = b"HTTP/1.1 304 Not Modified\r\nETag: x\r\n\r\n";
        assert_eq!(parse_response_head(head).unwrap().status, 304);
    }
}
