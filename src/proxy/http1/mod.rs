//! The HTTP/1.1 side a client speaks to: one connection's requests,
//! proxied with keepalive and backend connection pooling, until it closes
//! or turns out to be opaque TCP.

mod backend;
mod body;
mod exchange;
pub(crate) mod reply;

use anyhow::Result;
use arc_swap::ArcSwap;
use bytes::BufMut;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

use crate::metrics::{ListenerStats, METRICS};
use crate::proxy::backend_pool::PoolHandle;
use crate::proxy::deadline::idle_opt;
use crate::proxy::health::HealthRegistry;
use crate::proxy::http_filters::ForwardedLines;
use crate::proxy::http_parser::{find_header_end, has_header_end};
use crate::proxy::listener::{tcp_keepalive, ServeConfig};
use crate::proxy::method::{is_method_byte, MAX_METHOD_LEN};
use crate::proxy::proxy_protocol;
use crate::proxy::relay::handle_tcp_connection;
use crate::proxy::request_processor::{
    analyze_request, analyze_tcp_request, is_http_request, LocalReply, RoutingResult,
};
use crate::proxy::tls::Connection;
use crate::routing::{BackendSelector, RouteTable};
use exchange::{proxy_http_request, Framing};
use reply::{send_error_response, send_local_reply};

/// Read what the client has sent into `buf`'s spare room, as one read with
/// room up to `CLIENT_BUF` would: a read that fills the room grows the
/// buffer and goes on with what has already arrived, without waiting.
/// Returns the bytes read, 0 at EOF. `buf` must be below its cap.
async fn read_head(client: &mut Connection, buf: &mut Vec<u8>) -> std::io::Result<usize> {
    use futures::FutureExt;
    let start = buf.len();
    let mut room = head_room(buf);
    let mut read = client.read_buf(&mut (&mut *buf).limit(room)).await?;
    while read == room && buf.len() < CLIENT_BUF {
        room = head_room(buf);
        match client.read_buf(&mut (&mut *buf).limit(room)).now_or_never() {
            Some(Ok(n)) => read = n,
            // Nothing more yet. After an error, the bytes already read
            // still count: the next read ends the connection.
            None | Some(Err(_)) => break,
        }
    }
    Ok(buf.len() - start)
}

/// Spare room for the next head read, doubling the buffer when it is full,
/// never past `CLIENT_BUF`.
fn head_room(buf: &mut Vec<u8>) -> usize {
    if buf.len() == buf.capacity() {
        let grown = (2 * buf.capacity()).clamp(CLIENT_BUF_INITIAL, CLIENT_BUF);
        buf.reserve_exact(grown - buf.len());
    }
    buf.capacity().min(CLIENT_BUF) - buf.len()
}

/// Continue reading from `client` into `buf` until the full HTTP headers
/// (\r\n\r\n) are found or the buffer reaches its cap. Returns
/// `Some(false)` when the client closes first, `None` when the head
/// outgrows `CLIENT_BUF`.
async fn read_remaining_headers(
    client: &mut Connection,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<bool>> {
    loop {
        if buf.len() >= CLIENT_BUF {
            return Ok(None);
        }
        if read_head(client, buf).await? == 0 {
            // Never parse a truncated head: the proxy's own headers would be
            // appended to its unterminated last line, which could hide them
            // (e.g. inside a client header's value).
            return Ok(Some(false));
        }
        if has_header_end(buf) {
            return Ok(Some(true));
        }
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
    /// The client: X-Forwarded-For, access logs, rate limits, and the
    /// source of a PROXY v2 header to a backend.
    peer: SocketAddr,
    /// Where the client reached this data plane: the header's destination.
    local: SocketAddr,
    /// Forwarding lines appended to every request this connection sends,
    /// formatted at the first: many connections forward none.
    forwarded: Option<ForwardedLines>,
    /// Budget for completing a request's header block once bytes arrive.
    client_header_timeout: Duration,
    /// Backend connect budget for raw TCP forwarding (HTTP connects go
    /// through the pool, which carries its own copy).
    connect_timeout: Duration,
    /// Set on every backend socket this connection opens.
    backend_keepalive: socket2::TcpKeepalive,
    /// Reused across keepalive responses to avoid per-response heap allocation.
    /// Allocated by the first response read, which sizes it.
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
    /// How the requests observe `drain`: see `DrainWatch`.
    drain_watch: Arc<DrainWatch>,
    /// The connection closes after a reply while the client may still be
    /// sending: the close reads off what comes instead of resetting on it.
    /// Set by `close_after_reply` only, so every reply follows one rule.
    linger: bool,
    /// This request's client sends nothing more (see [`client_done`]), so a
    /// closing reply has nothing to linger for. Reset as each request
    /// starts: nothing is known yet of its client.
    pub(crate) client_done: bool,
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
        peer: SocketAddr,
        local: SocketAddr,
        cfg: &ServeConfig,
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
            peer,
            local,
            forwarded: None,
            client_header_timeout: cfg.client_header_timeout,
            connect_timeout: cfg.connect_timeout,
            backend_keepalive: tcp_keepalive(cfg.tcp_keepalive_time),
            header_buf: Vec::new(),
            scratch_buf: Vec::with_capacity(1024),
            filter_buf: Vec::new(),
            idle_body_timeout: idle_opt(cfg.idle_body_timeout),
            listener_stats,
            last_status: 0,
            drain,
            drain_watch: Arc::default(),
            linger: false,
            client_done: false,
            reset: false,
        }
    }

    /// The PROXY v2 header a proxied backend's connection opens with.
    fn proxy_header(&self, proxied: bool) -> Option<Vec<u8>> {
        proxied.then(|| proxy_protocol::header(self.peer, self.local))
    }

    /// The connection closes after a reply: a client still sending, a
    /// pipelined request included, would get a reset that can destroy the
    /// reply it has not read yet.
    pub(crate) fn close_after_reply(&mut self) {
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
fn keep_open(drain: &DrainWatch, reusable: bool) -> bool {
    reusable && !drain.started()
}

/// A connection's view of its drain token, read without the token's lock.
/// tokio-util's `is_cancelled` locks the token, as do registering and
/// dropping a wait on it, and every keepalive request did all three. The
/// token is waited on once per connection instead (`arm`), by a waker
/// that raises this flag.
#[derive(Default)]
struct DrainWatch {
    started: AtomicBool,
    task: futures::task::AtomicWaker,
}

impl DrainWatch {
    fn started(&self) -> bool {
        self.started.load(Ordering::Acquire)
    }

    /// Resolves once the drain has started.
    async fn wait(&self) {
        std::future::poll_fn(|cx| {
            if self.started() {
                return std::task::Poll::Ready(());
            }
            self.task.register(cx.waker());
            // A drain since the check may have woken no one.
            if self.started() {
                std::task::Poll::Ready(())
            } else {
                std::task::Poll::Pending
            }
        })
        .await
    }

    /// Have `drained`, a wait on the drain token, raise `watch`. Polled
    /// this once, it stays registered for as long as it lives.
    fn arm(watch: &Arc<DrainWatch>, drained: Pin<&mut WaitForCancellationFutureOwned>) {
        use std::future::Future;
        let waker = std::task::Waker::from(Arc::clone(watch));
        if drained
            .poll(&mut std::task::Context::from_waker(&waker))
            .is_ready()
        {
            waker.wake();
        }
    }
}

impl std::task::Wake for DrainWatch {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.started.store(true, Ordering::Release);
        self.task.wake();
    }
}

/// The most a client connection's buffer holds: the largest request head,
/// one body read, and the most a lingering close reads off. The h2
/// bridge's close reads off no more.
pub(crate) const CLIENT_BUF: usize = 64 * 1024;

/// A client connection's buffer to start with: room for a typical request
/// head. Allocated at the first read and never zeroed; it grows on need.
const CLIENT_BUF_INITIAL: usize = 4 * 1024;

pub(crate) async fn serve(
    mut client: Connection,
    _peer: SocketAddr,
    mut state: ConnectionState,
) -> Result<()> {
    let (tcp_route, http_routes) = {
        let rt = state.routes.load();
        let port = state.server_port;
        // Never for a terminated connection: see `analyze_request`.
        let tcp_route = !state.is_tls && rt.has_tcp_route(port);
        (tcp_route, rt.has_http_routes(port))
    };
    // Server-speaks-first protocols (MySQL, SMTP, FTP, IMAP, POP3) send
    // nothing before the backend's banner. With no HTTP route to tell
    // apart there is nothing to sniff, and waiting for the client's first
    // bytes would deadlock them.
    if tcp_route && !http_routes {
        return forward_to_tcp_route(client, &state).await;
    }

    let mut buf = Vec::new();
    let served = serve_http(&mut client, &mut buf, &mut state, tcp_route).await;
    if let Ok(Handoff::Tcp {
        backend_addr,
        proxied,
    }) = served
    {
        handle_tcp_connection(
            client,
            backend_addr,
            state.proxy_header(proxied),
            &buf,
            Arc::clone(&state.health),
            state.connect_timeout,
            &state.backend_keepalive,
            state.idle_body_timeout,
            "TCP",
            &state.listener_stats,
        )
        .await;
        return Ok(());
    }
    close_connection(&mut client, &mut buf, &state).await;
    served.map(drop)
}

/// Where [`serve_http`] leaves the connection.
enum Handoff {
    /// The HTTP exchanges are over: the connection closes.
    Close,
    /// Not HTTP: the connection goes to a TCP route, `buf` first.
    Tcp {
        backend_addr: SocketAddr,
        proxied: bool,
    },
}

/// Serve requests until the connection must close, or turns out to be
/// opaque TCP. Every exit comes back to [`serve`], which owns the
/// one close.
async fn serve_http(
    client: &mut Connection,
    buf: &mut Vec<u8>,
    state: &mut ConnectionState,
    tcp_route: bool,
) -> Result<Handoff> {
    // Lives as long as the requests: dropping it ends the wait on the token.
    let drained = std::pin::pin!(state.drain.clone().cancelled_owned());
    DrainWatch::arm(&state.drain_watch, drained);
    idle_read(client, buf, &state.drain_watch).await?;
    sniff_first_request(client, buf, state.client_header_timeout, !tcp_route).await;

    // If data looks like HTTP but headers aren't complete, accumulate more reads.
    // TCP/binary data (no \r\n\r\n expected) passes through immediately.
    if is_http_request(buf) && !has_header_end(buf) && !complete_headers(client, buf, state).await?
    {
        return Ok(Handoff::Close);
    }
    if buf.is_empty() {
        return Ok(Handoff::Close);
    }
    // The backend budget's timer, kept across this connection's requests:
    // see `within`. A Sleep never reset or polled is not registered.
    let mut budget_timer = std::pin::pin!(tokio::time::sleep_until(tokio::time::Instant::now()));

    loop {
        // Loop top = this request's bytes are in buf (initial read or the
        // keepalive read at the bottom), so this stamps headers-complete.
        let request_started = std::time::Instant::now();
        // Copied out now because the response phase reuses buf as scratch;
        // None whenever access logging is off.
        let access = crate::access_log::capture(
            buf,
            request_started,
            state.peer.ip(),
            state.server_port,
            state.is_tls,
        );
        let route_table = state.routes.load();
        let result = analyze_request(
            &route_table,
            &state.selector,
            buf,
            state.server_port,
            &state.health,
            state.is_tls,
        )?;

        // Bytes read past this request (a pipelined next request) are carried
        // over and replayed as the next iteration's input instead of being
        // sent to the backend or lost.
        let pending: Option<Vec<u8>> = match result {
            RoutingResult::TcpForward {
                backend_addr,
                proxied,
            } => {
                return Ok(Handoff::Tcp {
                    backend_addr,
                    proxied,
                });
            }
            RoutingResult::HttpForward {
                backend,
                rule,
                meta,
            } => {
                let limited = rule
                    .limiter
                    .as_ref()
                    .is_some_and(|l| !l.check(state.peer.ip()));
                if limited {
                    crate::metrics::METRICS.rate_limited_total.inc();
                    state.listener_stats.rate_limited.inc();
                    state.last_status = 429;
                    let reply = LocalReply {
                        status: 429,
                        location: None,
                        reusable: keep_open(&state.drain_watch, meta.local_reply_reusable),
                        client_done: meta.client_done,
                        head: meta.is_head,
                    };
                    if !send_local_reply(client, state, &reply, access).await? {
                        return Ok(Handoff::Close);
                    }
                    pipelined_tail(buf)
                } else if let Some(framing) = Framing::of(buf, &meta) {
                    // After the limiter and the framing: a 429 or a malformed
                    // body is no dispatch. Per-vhost sample (the rule carries
                    // its route hostname's registry slot) + the per-listener
                    // aggregate.
                    rule.requests.fetch_add(1, Ordering::Relaxed);
                    state.listener_stats.http_requests.inc();
                    // proxy_http_request owns all timing for the request lifecycle —
                    // backend_request_timeout (connect + TTFB) and request_timeout
                    // (total deadline) are applied phase-aware inside.
                    let backend_addr = backend.socket_addr;
                    let timer = budget_timer.as_mut();
                    let (keep, leftover) = proxy_http_request(
                        client, buf, framing, backend, rule, &meta, state, timer,
                    )
                    .await?;
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
                } else {
                    // Refused as a malformed head is: where the body ends is
                    // unknown, so the connection closes.
                    METRICS.http_requests_rejected_total.inc();
                    let reply = LocalReply {
                        status: 400,
                        location: None,
                        reusable: false,
                        client_done: false,
                        head: meta.is_head,
                    };
                    send_local_reply(client, state, &reply, access).await?;
                    return Ok(Handoff::Close);
                }
            }
            RoutingResult::LocalReply(mut reply) => {
                reply.reusable = keep_open(&state.drain_watch, reply.reusable);
                if !send_local_reply(client, state, &reply, access).await? {
                    return Ok(Handoff::Close);
                }
                pipelined_tail(buf)
            }
            RoutingResult::UdpForward { .. } | RoutingResult::CloseConnection => {
                return Ok(Handoff::Close);
            }
        };

        // Explicitly drop the route table guard before reading the next request.
        // This allows old route tables to be freed between keepalive iterations.
        drop(route_table);
        state.client_done = false;

        buf.clear();
        if let Some(bytes) = pending {
            // Replay pipelined bytes as the start of the next request. They
            // came out of `buf`, so they fit its cap.
            buf.extend_from_slice(&bytes);
        } else {
            idle_read(client, buf, &state.drain_watch).await?;
        }
        // RFC 9112 2.2: empty lines before a request-line are ignored. An
        // old client ends a POST body with a CRLF its length leaves out.
        loop {
            let skip = empty_lines_len(buf);
            if skip > 0 {
                buf.drain(..skip);
            }
            match &buf[..] {
                // Between requests: as unbounded as the idle wait.
                [] if skip > 0 => {
                    idle_read(client, buf, &state.drain_watch).await?;
                }
                // Inside an empty line: as bounded as a request head.
                [b'\r'] => {
                    let rest = read_head(client, buf);
                    match tokio::time::timeout(state.client_header_timeout, rest).await {
                        Ok(Ok(read)) if read > 0 => {}
                        _ => return Ok(Handoff::Close),
                    }
                }
                _ => break,
            }
        }
        // The connection is HTTP by now: bytes that cannot open a request
        // (frames held back from a refused upgrade) close it. They must not
        // reach TCP routing, nor wait out the header budget for a 408.
        if buf.is_empty() || !may_open_request(buf) {
            return Ok(Handoff::Close);
        }
        if !has_header_end(buf) && !complete_headers(client, buf, state).await? {
            return Ok(Handoff::Close);
        }
        if !is_http_request(buf) {
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
/// `LINGER_IDLE`, up to `CLIENT_BUF` of it. A drain's close after a reply
/// lingers too, so a client keeping its socket open holds the drain that
/// long. An idle connection, no reply just written, closes at once: there
/// is no reply for a reset to destroy.
///
/// All of it within the client header budget: a client that stops reading,
/// or never stops sending, cannot hold the close.
async fn close_connection(client: &mut Connection, buf: &mut Vec<u8>, state: &ConnectionState) {
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

/// Read and drop what the client still sends, up to `CLIENT_BUF` of it,
/// until it closes, falls silent for `LINGER_IDLE`, or `deadline` passes.
/// `buf` is not grown for it: the connection is going.
async fn read_off(client: &mut Connection, buf: &mut Vec<u8>, deadline: tokio::time::Instant) {
    let mut discarded = 0;
    while discarded < CLIENT_BUF {
        buf.clear();
        let room = buf
            .capacity()
            .max(CLIENT_BUF_INITIAL)
            .min(CLIENT_BUF - discarded);
        let idle = deadline.min(tokio::time::Instant::now() + LINGER_IDLE);
        let mut spare = (&mut *buf).limit(room);
        match tokio::time::timeout_at(idle, client.read_buf(&mut spare)).await {
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
    let RoutingResult::TcpForward {
        backend_addr,
        proxied,
    } = routed
    else {
        return Ok(()); // the router logged why
    };
    handle_tcp_connection(
        client,
        backend_addr,
        state.proxy_header(proxied),
        &[],
        Arc::clone(&state.health),
        state.connect_timeout,
        &state.backend_keepalive,
        state.idle_body_timeout,
        "TCP",
        &state.listener_stats,
    )
    .await;
    Ok(())
}

/// The wait for a connection's next request, its first included: unbounded,
/// as long idle is legitimate there, until a drain ends it like an EOF
/// would (0).
async fn idle_read(
    client: &mut Connection,
    buf: &mut Vec<u8>,
    drain: &DrainWatch,
) -> std::io::Result<usize> {
    tokio::select! {
        // A request already sent is served: its reply announces the close.
        biased;
        read = read_head(client, buf) => read,
        () = drain.wait() => Ok(0),
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
/// would close unanswered.
async fn sniff_first_request(
    client: &mut Connection,
    buf: &mut Vec<u8>,
    budget: Duration,
    await_method: bool,
) {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let skip = empty_lines_len(buf);
        let rest = &buf[skip..];
        if is_http_request(rest) {
            if skip > 0 {
                buf.drain(..skip);
            }
            return;
        }
        let undecided = rest == b"\r"
            || (skip > 0 && rest.is_empty())
            || (await_method && may_open_request(rest));
        if !undecided || buf.len() >= CLIENT_BUF {
            return;
        }
        match tokio::time::timeout_at(deadline, read_head(client, buf)).await {
            Ok(Ok(read)) if read > 0 => {}
            _ => return,
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
/// does with no reply. Returns false when the client closed before the
/// head ended: nothing is forwarded or answered.
async fn complete_headers(
    client: &mut Connection,
    buf: &mut Vec<u8>,
    state: &mut ConnectionState,
) -> Result<bool> {
    let budget = state.client_header_timeout;
    match tokio::time::timeout(budget, read_remaining_headers(client, buf)).await {
        Ok(Ok(Some(complete))) => Ok(complete),
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

/// Bytes past a bodyless request's head: the next pipelined request, which
/// the keepalive loop replays after a local reply.
fn pipelined_tail(request: &[u8]) -> Option<Vec<u8>> {
    let head_end = find_header_end(request)?;
    (head_end < request.len()).then(|| request[head_end..].to_vec())
}

/// One client connection driven through [`serve`] over an
/// in-memory duplex, against real loopback backends.
#[cfg(test)]
mod connection_tests {
    use super::exchange::send_request_with_retry;
    use super::*;
    use crate::proxy::listener::{TCP_KEEPALIVE_INTERVAL, TCP_KEEPALIVE_RETRIES};
    use crate::proxy::relay::handle_tls_passthrough;
    use crate::proxy::testkit::{
        assert_never_connected, backend, closing_backend, conn_state, conn_state_header, delivered,
        exchange, gateway_backend, idle_backend, keepalive_where, l4_backend, read_one_reply,
        read_until_closed, received, refusing_backend, response, routes, routes_with_tcp, serve,
        serve_state, tcp_client, tls_client, tls_read_to_end, GET_A, IDLE, OK, OPAQUE, REFUSED,
        UPGRADE,
    };
    use crate::routing::{
        Backend, HttpFilter, HttpHeader, HttpRouteRule, PathMatchType, URLRewritePath,
    };
    use std::sync::atomic::AtomicUsize;
    use tokio::io::DuplexStream;
    use tokio::net::{TcpListener, TcpStream};

    const SWITCHED: &[u8] =
        b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n";

    /// `serve`, with the connection draining once `drain` is cancelled.
    fn serve_draining(
        rt: RouteTable,
        drain: CancellationToken,
    ) -> (DuplexStream, tokio::task::JoinHandle<Result<()>>) {
        let mut state = conn_state(rt, false);
        state.drain = drain;
        serve_state(state)
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
            path: Some(URLRewritePath::ReplacePrefixMatch("/v2".into())),
        }];
        let seen = forwarded_head(
            |addr| filtered_route("p.test", "/v1", rewrite, addr),
            "GET http://p.test:8080/v1/users HTTP/1.1\r\nHost: p.test\r\n\r\n",
        )
        .await;
        assert!(seen.starts_with("GET /v2/users HTTP/1.1\r\n"), "{seen}");
        assert!(seen.contains("\r\nHost: p.test:8080\r\n"), "{seen}");
    }

    /// Routing matches the path a backend resolves dot-segments to, and the
    /// backend gets that very path: the rule on `/admin` cannot be walked
    /// around, whatever form the request comes in.
    #[tokio::test]
    async fn dot_segments_reach_the_rule_and_backend_of_their_path() {
        let (_held, elsewhere) = refusing_backend();
        let guarded = move |admin| {
            let mut rt = filtered_route("a.test", "/admin", vec![], admin);
            rt.add_http_route(
                "a.test",
                HttpRouteRule::new(
                    PathMatchType::Prefix,
                    "/".to_string(),
                    vec![],
                    vec![],
                    vec![],
                    vec![Backend {
                        socket_addr: elsewhere,
                        weight: 1,
                        filters: vec![],
                        use_tls: false,
                        server_name: String::new(),
                        action: crate::routing::BackendAction::Forward,
                    }],
                ),
            );
            rt
        };
        for (request, line) in [
            (
                "GET /public/../admin?x=1 HTTP/1.1\r\nHost: a.test\r\n\r\n",
                "GET /admin?x=1 HTTP/1.1\r\n",
            ),
            (
                "GET /public/%2E%2e/admin/./users HTTP/1.1\r\nHost: a.test\r\n\r\n",
                "GET /admin/users HTTP/1.1\r\n",
            ),
            (
                "GET http://a.test/x/../admin?x HTTP/1.1\r\nHost: a.test\r\n\r\n",
                "GET /admin?x HTTP/1.1\r\n",
            ),
            // A Connection-nominated header takes the slow assembly path.
            (
                "GET /x/../admin HTTP/1.1\r\nHost: a.test\r\nConnection: X-Foo\r\nX-Foo: 1\r\n\r\n",
                "GET /admin HTTP/1.1\r\n",
            ),
        ] {
            let seen = forwarded_head(guarded, request).await;
            assert!(seen.starts_with(line), "{request:?}: {seen}");
        }
    }

    /// A prefix rewrite replaces the prefix of the path routing matched.
    #[tokio::test]
    async fn a_path_rewrite_applies_to_the_normalized_path() {
        let rewrite = vec![HttpFilter::URLRewrite {
            hostname: None,
            path: Some(URLRewritePath::ReplacePrefixMatch("/v2".into())),
        }];
        let seen = forwarded_head(
            |addr| filtered_route("p.test", "/v1", rewrite, addr),
            "GET /v1/a/../b?q=1 HTTP/1.1\r\nHost: p.test\r\n\r\n",
        )
        .await;
        assert!(seen.starts_with("GET /v2/b?q=1 HTTP/1.1\r\n"), "{seen}");
    }

    #[tokio::test]
    async fn a_path_rewrite_keeps_the_query() {
        for request in [
            "GET /v1/users?id=7&x HTTP/1.1\r\nHost: p.test\r\n\r\n",
            "GET http://p.test/v1/users?id=7&x HTTP/1.1\r\nHost: p.test\r\n\r\n",
        ] {
            let rewrite = vec![HttpFilter::URLRewrite {
                hostname: None,
                path: Some(URLRewritePath::ReplacePrefixMatch("/v2".into())),
            }];
            let seen = forwarded_head(
                |addr| filtered_route("p.test", "/v1", rewrite, addr),
                request,
            )
            .await;
            assert!(
                seen.starts_with("GET /v2/users?id=7&x HTTP/1.1\r\n"),
                "{request:?}: {seen}"
            );
        }
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

    /// An invalid ref's share answers 500; a Gateway backend's before any
    /// of its instances reported answers 503: one is wrong, the other not
    /// up yet.
    #[tokio::test]
    async fn an_unreported_gateway_backend_answers_503_an_invalid_ref_500() {
        for (backend, status) in [
            (Backend::rejecting(1), "500"),
            (Backend::unavailable(1), "503"),
        ] {
            let mut rt = RouteTable::new();
            rt.add_http_route(
                "a.test",
                HttpRouteRule::new(
                    PathMatchType::Prefix,
                    "/".to_string(),
                    vec![],
                    vec![],
                    vec![],
                    vec![backend],
                ),
            );
            let (mut client, _task) = serve(rt);
            let out = exchange(&mut client, b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n").await;
            assert!(out.starts_with(&format!("HTTP/1.1 {status} ")), "{out}");
        }
    }

    /// Raw TCP has no status to answer with. A backend that refuses the
    /// connect is counted on its listener and by address, as an HTTP
    /// backend is: for a TCP route, and for TLS passthrough.
    #[tokio::test]
    async fn an_l4_backend_that_refuses_is_counted() {
        let (_held, refused) = refusing_backend();
        let by_address = || {
            let metrics = crate::metrics::render(true);
            let line = format!("backend=\"{refused}\"");
            metrics
                .lines()
                .filter(|l| {
                    l.starts_with("portail_upstream_connect_errors_total") && l.contains(&line)
                })
                .count()
        };
        assert_eq!(by_address(), 0);

        let state = conn_state(tcp_route(refused), false);
        let stats = Arc::clone(&state.listener_stats);
        let (mut client, task) = serve_state(state);
        assert_eq!(read_until_closed(&mut client).await, "");
        task.await.unwrap().unwrap();
        assert_eq!(stats.upstream_connect_errors.get(), 1);
        assert_eq!(by_address(), 1);

        let mut rt = RouteTable::new();
        rt.add_tls_route(0, "down.test", vec![gateway_backend(refused)]);
        let routes = Arc::new(ArcSwap::from_pointee(rt));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, peer) = listener.accept().await.unwrap();
        let local = server.local_addr().unwrap();
        let stats = ListenerStats::default();
        handle_tls_passthrough(
            server,
            (peer, local),
            Some("down.test"),
            None,
            8443,
            routes,
            Arc::default(),
            Arc::default(),
            &ServeConfig::from_perf(&Default::default(), 4),
            &stats,
        )
        .await;
        assert_eq!(client.read(&mut [0u8; 8]).await.unwrap(), 0);
        assert_eq!(stats.upstream_connect_errors.get(), 1);
    }

    /// A masked WebSocket text frame ("hello"), as a client streams it.
    const WS_FRAME: &[u8] = b"\x81\x85\x37\xfa\x21\x3d\x7f\x9f\x4d\x51\x58";

    /// Keepalive time, interval and retries of the socket in this process
    /// whose local address is `local`: the proxy end of a backend connection,
    /// as the backend saw its peer. `None` when keepalive is off.
    fn keepalive_of(local: SocketAddr) -> Option<(Duration, Duration, u32)> {
        keepalive_where(|sock| sock.local_addr().ok().and_then(|a| a.as_socket()) == Some(local))
    }

    /// Raw TCP to a Gateway backend: the header goes before any byte of
    /// the client's, on a TCP-only port and after a sniff alike.
    #[tokio::test]
    async fn a_gateway_backend_hears_who_connected_before_the_bytes() {
        let (tcp, mut rx) = backend(REFUSED).await;
        let mut rt = RouteTable::new();
        rt.add_tcp_route(8080, vec![gateway_backend(tcp)]);
        let (mut client, _task) = serve(rt);
        client.write_all(b"hello").await.unwrap();
        client.shutdown().await.unwrap();
        assert_eq!(
            received(&mut rx).await,
            [conn_state_header(), b"hello".to_vec()].concat()
        );

        let (http, _) = backend(REFUSED).await;
        let (tcp, mut rx) = backend(REFUSED).await;
        let mut rt = routes(http);
        rt.add_tcp_route(8080, vec![gateway_backend(tcp)]);
        let (mut client, _task) = serve(rt);
        client.write_all(OPAQUE).await.unwrap();
        client.shutdown().await.unwrap();
        assert_eq!(
            received(&mut rx).await,
            [conn_state_header(), OPAQUE.to_vec()].concat()
        );
    }

    /// A Gateway backend's connection carries one client's header, so it
    /// serves one request: never from the pool, never back to it.
    #[tokio::test]
    async fn each_request_to_a_gateway_backend_opens_its_own_connection() {
        let (addr, mut rx) = backend(REFUSED).await;
        let mut rt = RouteTable::new();
        rt.add_http_route(
            "a.test",
            HttpRouteRule::new(
                PathMatchType::Prefix,
                "/".to_string(),
                vec![],
                vec![],
                vec![],
                vec![gateway_backend(addr)],
            ),
        );
        let (mut client, _task) = serve(rt);
        for _ in 0..2 {
            client
                .write_all(b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n")
                .await
                .unwrap();
            let reply = read_one_reply(&mut client).await;
            assert!(reply.starts_with("HTTP/1.1 200 OK\r\n"), "{reply}");
        }
        client.shutdown().await.unwrap();
        for _ in 0..2 {
            let seen = received(&mut rx).await;
            let header = conn_state_header();
            assert_eq!(seen[..header.len()], header[..]);
            let request = String::from_utf8_lossy(&seen[header.len()..]).into_owned();
            assert!(request.starts_with("GET / HTTP/1.1\r\n"), "{request}");
            assert_eq!(request.matches("GET ").count(), 1, "{request}");
        }
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

    /// The backend budget times each request from its own start, whatever
    /// the connection's last request left on its timer: a deadline still
    /// pending, a later one, or one that passed while the connection idled.
    #[tokio::test]
    async fn the_backend_budget_runs_anew_for_each_request() {
        const BUDGET: Duration = Duration::from_millis(300);
        // Answers the first request of each connection, holds the rest.
        let (addr, _delivered) = backend(OK.as_bytes()).await;
        let routes = || {
            let mut rt = RouteTable::new();
            for (host, budget) in [("a.test", BUDGET), ("l.test", BUDGET * 20)] {
                let backend = Backend {
                    socket_addr: addr,
                    weight: 1,
                    filters: vec![],
                    use_tls: false,
                    server_name: String::new(),
                    action: crate::routing::BackendAction::Forward,
                };
                let path = "/".to_string();
                let mut rule = HttpRouteRule::new(
                    PathMatchType::Prefix,
                    path,
                    vec![],
                    vec![],
                    vec![],
                    vec![backend],
                );
                rule.backend_request_timeout = Some(budget);
                rt.add_http_route(host, rule);
            }
            rt
        };
        let get_l: &[u8] = b"GET / HTTP/1.1\r\nHost: l.test\r\n\r\n";
        for (first, idle) in [
            (GET_A, Duration::ZERO),
            (GET_A, BUDGET * 2 / 3),
            (GET_A, BUDGET * 2),
            (get_l, Duration::ZERO),
        ] {
            let (mut client, _task) = serve(routes());
            client.write_all(first).await.unwrap();
            assert_eq!(response(&mut client).await, OK);
            tokio::time::sleep(idle).await;
            let started = std::time::Instant::now();
            client.write_all(GET_A).await.unwrap();
            let out = response(&mut client).await;
            let waited = started.elapsed();
            assert!(out.starts_with("HTTP/1.1 504 "), "{out}");
            let first = String::from_utf8_lossy(first);
            assert!(
                waited >= BUDGET && waited < BUDGET * 5,
                "504 after {waited:?}, {idle:?} after {first}"
            );
        }
    }

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
    /// body the proxy still holds is replayed once on a fresh conn. A body
    /// sent whole with its head is held, even past the buffer's first read.
    #[tokio::test]
    async fn idempotent_request_is_replayed_once_when_a_pooled_conn_dies_unanswered() {
        let put = b"PUT / HTTP/1.1\r\nHost: a.test\r\nContent-Length: 2\r\n\r\nhi";
        let body = "x".repeat(2 * CLIENT_BUF_INITIAL);
        let big_put = format!(
            "PUT / HTTP/1.1\r\nHost: a.test\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        for script in [then_close, then_reset] {
            for request in [GET_A, put, big_put.as_bytes()] {
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

    /// The buffer starts small but the head cap stays put: a head ending
    /// exactly at it is read whole, one byte more is a 431.
    #[tokio::test]
    async fn a_head_may_fill_the_read_buffer_but_not_outgrow_it() {
        for (past_cap, status) in [(0, "404"), (1, "431")] {
            let (mut client, _task) = serve(RouteTable::new());
            let start = "GET / HTTP/1.1\r\nHost: a.test\r\nX-Pad: ";
            let pad = "p".repeat(CLIENT_BUF + past_cap - start.len() - 4);
            // The engine stops reading at a 431: the last byte may not land.
            let _ = client
                .write_all(format!("{start}{pad}\r\n\r\n").as_bytes())
                .await;
            let reply = response(&mut client).await;
            assert!(reply.starts_with(&format!("HTTP/1.1 {status} ")), "{reply}");
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

    /// The engine on a TCP-only port relaying to `backend`.
    fn serve_l4(
        backend: SocketAddr,
        idle: Option<Duration>,
    ) -> (DuplexStream, tokio::task::JoinHandle<Result<()>>) {
        let mut state = conn_state(tcp_route(backend), false);
        state.idle_body_timeout = idle;
        serve_state(state)
    }

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

    /// An upgraded tunnel is the same relay: once the client has finished,
    /// a backend that never closes ends with the idle budget.
    #[tokio::test]
    async fn half_closed_upgrade_tunnel_ends_after_the_idle_budget() {
        let silent = l4_backend(|mut sock| async move {
            let _ = sock.read(&mut [0u8; 4096]).await;
            let _ = sock.write_all(SWITCHED).await;
            let _ = sock.read_to_end(&mut Vec::new()).await;
            std::future::pending::<()>().await;
        })
        .await;
        let mut state = conn_state(routes(silent), false);
        state.idle_body_timeout = Some(IDLE);
        let (mut client, task) = serve_state(state);
        client.write_all(UPGRADE).await.unwrap();
        client
            .read_exact(&mut vec![0u8; SWITCHED.len()])
            .await
            .unwrap();
        client.shutdown().await.unwrap();
        let ended = tokio::time::timeout(IDLE * 10, task).await;
        assert!(ended.is_ok(), "tunnel pinned after the client's FIN");
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

    /// A terminated connection is an HTTPS listener's: its bytes never
    /// reach a TCP route in plaintext, whatever else took the port.
    #[tokio::test]
    async fn terminated_bytes_never_reach_a_tcp_route() {
        // Beside HTTP routes: opaque bytes close, relayed nowhere.
        let (http, tcp) = (idle_backend().await, idle_backend().await);
        let rt = routes_with_tcp(http.local_addr().unwrap(), tcp.local_addr().unwrap());
        let (mut client, task) = serve_state(conn_state(rt, true));
        client.write_all(OPAQUE).await.unwrap();
        assert_eq!(read_until_closed(&mut client).await, "");
        task.await.unwrap().unwrap();
        assert_never_connected(&tcp).await;

        // A TCP route alone relays before the client speaks: not here,
        // where the connection is served as HTTP and finds no route.
        let tcp = idle_backend().await;
        let rt = tcp_route(tcp.local_addr().unwrap());
        let (mut client, task) = serve_state(conn_state(rt, true));
        let out = exchange(&mut client, UNROUTED).await;
        assert!(out.starts_with("HTTP/1.1 404 Not Found\r\n"), "{out}");
        task.await.unwrap().unwrap();
        assert_never_connected(&tcp).await;
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

    /// The request counters count dispatches to a backend: a 429 counts
    /// only as limited.
    #[tokio::test]
    async fn a_limited_request_counts_only_as_limited() {
        let (addr, _rx) = backend(REFUSED).await;
        let rule = |limiter| {
            let mut rule = HttpRouteRule::new(
                PathMatchType::Prefix,
                "/".to_string(),
                vec![],
                vec![],
                vec![],
                vec![Backend {
                    socket_addr: addr,
                    weight: 1,
                    filters: vec![],
                    use_tls: false,
                    server_name: String::new(),
                    action: crate::routing::BackendAction::Forward,
                }],
            );
            rule.limiter = limiter;
            rule
        };
        let mut rt = RouteTable::new();
        // An empty bucket that never refills: every request is limited.
        let empty = crate::rate_limit::limiter_for(0, "limited-count-test", 0, 0);
        rt.add_http_route("limited.count.test", rule(Some(empty)));
        rt.add_http_route("free.count.test", rule(None));
        let state = conn_state(rt, false);
        let stats = Arc::clone(&state.listener_stats);
        let (mut client, _task) = serve_state(state);
        let out = exchange(
            &mut client,
            b"GET / HTTP/1.1\r\nHost: limited.count.test\r\n\r\nGET / HTTP/1.1\r\nHost: free.count.test\r\n\r\n",
        )
        .await;
        assert!(
            out.starts_with("HTTP/1.1 429 Too Many Requests\r\n"),
            "{out}"
        );
        assert!(out.ends_with("no"), "{out}");
        assert_eq!(stats.rate_limited.get(), 1);
        assert_eq!(stats.http_requests.get(), 1);
        let host_count = |host| crate::metrics::host_requests_counter(host).load(Ordering::Relaxed);
        assert_eq!(host_count("limited.count.test"), 0);
        assert_eq!(host_count("free.count.test"), 1);
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

    /// No backend sees a body whose buffered chunked framing is broken:
    /// its 400 counts as rejected, as a malformed head's does.
    #[tokio::test]
    async fn a_malformed_buffered_chunked_body_is_no_dispatch() {
        let idle = idle_backend().await;
        let state = conn_state(routes(idle.local_addr().unwrap()), false);
        let stats = Arc::clone(&state.listener_stats);
        let (mut client, task) = serve_state(state);
        let rejected = || METRICS.http_requests_rejected_total.get();
        let before = rejected();
        let out = exchange(
            &mut client,
            b"POST / HTTP/1.1\r\nHost: a.test\r\nTransfer-Encoding: chunked\r\n\r\nZ\r\nzz\r\n0\r\n\r\n",
        )
        .await;
        assert!(out.starts_with("HTTP/1.1 400 Bad Request\r\n"), "{out}");
        task.await.unwrap().unwrap();
        assert_never_connected(&idle).await;
        assert_eq!(stats.http_requests.get(), 0);
        assert!(rejected() > before);
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
            read_off(&mut engine, &mut Vec::new(), started + budget).await;
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
        let mut buf = head.to_vec();
        let mut conn = Connection::Plain { inner: engine_end };
        let mut state = conn_state(RouteTable::new(), false);
        let read = complete_headers(&mut conn, &mut buf, &mut state).await;
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
}

#[cfg(test)]
mod tests {
    use super::body::{forward_response_body, relay_request_body, BodyEnd, RequestBody};
    use super::exchange::backend_conn_poolable;
    use super::*;
    use crate::proxy::chunked::ChunkedStream;
    use crate::proxy::upstream::response::ResponseFraming;

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
        let mut buf = Vec::new();
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
        let mut buf = Vec::new();
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
}
