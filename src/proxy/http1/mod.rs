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
    analyze_request, is_http_request, LocalReply, RoutingResult,
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
    /// Its port has a TCP route beside its HTTP routes, and it is not
    /// terminated TLS: bytes that open no request go to that route. Read
    /// from the table once, by whoever accepted the connection, in the
    /// same look that sends a port with no HTTP route straight to the
    /// relay: the engine never sees one.
    tcp_route: bool,
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
        tcp_route: bool,
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
            tcp_route,
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
    let tcp_route = state.tcp_route;
    let mut buf = Vec::new();
    let served = serve_http(&mut client, &mut buf, &mut state, tcp_route).await;
    if let Ok(Handoff::Tcp {
        backend_addr,
        proxied,
    }) = served
    {
        let header = state.proxy_header(proxied);
        // The tunnel keeps nothing of the engine: its state, pool and
        // buffers would otherwise live as long as it does.
        let ConnectionState {
            health,
            connect_timeout,
            backend_keepalive,
            idle_body_timeout,
            listener_stats,
            ..
        } = state;
        handle_tcp_connection(
            client,
            backend_addr,
            header,
            buf,
            health,
            connect_timeout,
            &backend_keepalive,
            idle_body_timeout,
            "TCP",
            &listener_stats,
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
mod tests {
    use super::*;
    use crate::proxy::testkit::{
        assert_never_connected, backend, conn_state, conn_state_header, delivered, exchange,
        gateway_backend, idle_backend, read_one_reply, read_until_closed, received,
        refusing_backend, response, routes, routes_with_tcp, serve, serve_state, tcp_client,
        tcp_route, tls_client, tls_read_to_end, GET_A, OPAQUE, REFUSED, UPGRADE,
    };
    use crate::routing::{Backend, HttpRouteRule, PathMatchType};
    use tokio::io::DuplexStream;
    use tokio::net::{TcpListener, TcpStream};

    /// `serve`, with the connection draining once `drain` is cancelled.
    fn serve_draining(
        rt: RouteTable,
        drain: CancellationToken,
    ) -> (DuplexStream, tokio::task::JoinHandle<Result<()>>) {
        let mut state = conn_state(rt, false);
        state.drain = drain;
        serve_state(state)
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

    /// Raw TCP to a Gateway backend after a sniff: the header goes before
    /// the bytes the engine read to tell them from a request.
    #[tokio::test]
    async fn a_gateway_backend_hears_who_connected_before_the_sniffed_bytes() {
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
                    proto: crate::proxy::backend_pool::BackendProto::Plain,
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
}
