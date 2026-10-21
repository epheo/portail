//! HTTP/2 front end: each h2 stream is replayed as one HTTP/1.1 exchange
//! over an in-memory duplex into the same engine that serves h1 clients
//! ([`worker::handle_connection`]), so routing, filters, backend pooling,
//! timeouts, and observability behave identically across both protocols.
//! Only ALPN-negotiated h2 connections pay the replay's in-memory copy;
//! the h1 path is untouched.
//!
//! Deliberate v1 limits: trailers do not survive the h1 replay (gRPC needs
//! a native h2 path), CONNECT/:protocol streams are refused, and the
//! engine's 100-continue handling is bypassed by stripping `expect`.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::{pin, Pin};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use std::time::Duration;

use anyhow::{anyhow, Result};
use arc_swap::ArcSwap;
use bytes::{Buf, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio_rustls::server::TlsStream as ServerTlsStream;
use tokio_util::sync::CancellationToken;

use crate::logging::debug;
use crate::metrics::ListenerStats;
use crate::proxy::backend_pool::SharedBackendPool;
use crate::proxy::chunked::ChunkedStream;
use crate::proxy::health::HealthRegistry;
use crate::proxy::http_filters::{
    header_lines, is_hop_by_hop_request_header, is_hop_by_hop_response_header,
};
use crate::proxy::http_parser::{find_header_end, trim_ows};
use crate::proxy::tls::Connection;
use crate::proxy::worker::{self, progress, BodyFraming, WorkerConfig};
use crate::routing::{BackendSelector, RouteTable};

/// Streams per connection: enough for browser multiplexing, small enough
/// that one connection cannot spawn unbounded engine exchanges.
const MAX_CONCURRENT_STREAMS: u32 = 128;

/// Decoded header-list cap, kept under the engine's 64 KiB request buffer
/// so a synthesized head always fits in one engine read and never trips
/// its 431 path.
const MAX_HEADER_LIST_SIZE: u32 = 48 * 1024;

/// Per-direction duplex buffer; matches the engine's read buffer so a
/// typical exchange crosses in one hop, and a body that comes whole with its
/// head stays replayable on a stale pooled conn. Grown on demand, it costs
/// only the bytes in flight.
const DUPLEX_BUF: usize = 64 * 1024;

/// Upload window per stream: h2's 64 KiB default caps an upload at 64 KiB
/// per round trip; 2 MiB carries 200 Mbit/s at 80 ms.
const STREAM_WINDOW: u32 = 2 * 1024 * 1024;

/// Upload window per connection: two streams' worth, so one stalled upload
/// leaves a full window to the connection's other streams.
/// Two stalled uploads hold all of it: h2 shares one window per connection.
const CONNECTION_WINDOW: u32 = 2 * STREAM_WINDOW;

/// Size of each response read from the duplex.
const RESP_READ: usize = 16 * 1024;

/// Ceiling on response-head bytes accumulated before the terminator is found.
/// Tied to the engine's own header cap plus one read of body overshoot, so an
/// engine-legal head is never rejected only on the h2 path.
const RESPONSE_HEAD_CAP: usize = worker::RESPONSE_HEADER_CAP + RESP_READ;

/// Everything a bridged connection shares with its listener's accept loop.
/// `pool` is resolved once per h2 connection (the process-wide pool under
/// `backendPoolScope: process`, else one shared pool for this connection's
/// streams) so streams reuse backend connections like h1 keepalive requests
/// instead of each building a fresh pool.
pub(crate) struct H2Ctx {
    pub(crate) server_port: u16,
    pub(crate) routes: Arc<ArcSwap<RouteTable>>,
    pub(crate) selector: Arc<BackendSelector>,
    pub(crate) health: Arc<HealthRegistry>,
    pub(crate) pool: Arc<SharedBackendPool>,
    pub(crate) cfg: WorkerConfig,
    pub(crate) listener_stats: Arc<ListenerStats>,
    pub(crate) peer: SocketAddr,
    /// Cancelled when the data plane drains: the connection sends GOAWAY.
    pub(crate) drain: CancellationToken,
}

/// The client's stream as h2 drives it, with the two bounds h2 lacks.
///
/// Writes: h2 has no send timeout. A client that stops reading parks its
/// writes forever: while its streams live, after they were reset, and in
/// the flush h2 runs before it shuts the stream down. Writes pending for
/// `write_idle` with no flush completing fail, and h2 drops the connection,
/// as nginx's send_timeout does. Reads stay unbounded: an idle connection
/// is legitimate.
///
/// Close: h2 ends a connection by shutting the stream down and dropping it,
/// while a client closing in step still sends its own GOAWAY and
/// close_notify: closed on those unread, the kernel resets and the client's
/// close fails. So the shutdown reads off what still comes until the client
/// closes too or falls silent for `LINGER_IDLE`, up to h1's `CLIENT_BUF` of
/// it, within `close_budget`, as nginx does for HTTP/2.
struct ClientStream<S> {
    inner: S,
    /// `None` lets writes wait forever.
    write_idle: Option<Duration>,
    /// One timer for all of the connection's write stalls, reset as each
    /// begins: a stall must not allocate.
    stall: Option<Pin<Box<tokio::time::Sleep>>>,
    /// A write or flush was pending since `stall` was last reset, and no
    /// flush has completed since.
    stalled: bool,
    close_budget: Duration,
    /// Armed by the first shutdown poll, before the inner shutdown: a client
    /// that stops reading holds the close_notify write as long as it likes.
    /// The linger then moves it to `LINGER_IDLE` past the client's last
    /// bytes, never past the budget.
    close_deadline: Option<Pin<Box<tokio::time::Sleep>>>,
    /// The budget's end, once the inner shutdown is done and only the
    /// linger is left.
    linger_until: Option<tokio::time::Instant>,
    /// What the linger read off so far.
    discarded: usize,
}

impl<S> ClientStream<S> {
    fn new(inner: S, write_idle: Option<Duration>, close_budget: Duration) -> Self {
        Self {
            inner,
            write_idle,
            stall: None,
            stalled: false,
            close_budget,
            close_deadline: None,
            linger_until: None,
            discarded: 0,
        }
    }

    /// A write or flush is pending: start the stall clock unless it runs,
    /// and fail once it ran out.
    fn poll_stall<T>(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<T>> {
        let Some(idle) = self.write_idle else {
            return Poll::Pending;
        };
        let stall = self
            .stall
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(idle)));
        if !self.stalled {
            self.stalled = true;
            stall.as_mut().reset(tokio::time::Instant::now() + idle);
        }
        ready!(stall.as_mut().poll(cx));
        Poll::Ready(Err(io::ErrorKind::TimedOut.into()))
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for ClientStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for ClientStream<S> {
    // A write taken is no progress: TLS buffers what the client never
    // reads, so counting it let every timer wakeup buy another budget.
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_write(cx, buf) {
            Poll::Pending => self.poll_stall(cx),
            taken => taken,
        }
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_write_vectored(cx, bufs) {
            Poll::Pending => self.poll_stall(cx),
            taken => taken,
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.inner).poll_flush(cx) {
            Poll::Pending => self.poll_stall(cx),
            flushed => {
                self.stalled = false;
                flushed
            }
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        let budget = this.close_budget;
        let deadline = this
            .close_deadline
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(budget)));
        // The silence that ends the linger counts from its start, then
        // from the client's last bytes.
        let mut restart = false;
        let close_by = match this.linger_until {
            Some(close_by) => close_by,
            None => {
                if Pin::new(&mut this.inner).poll_shutdown(cx)?.is_pending() {
                    return deadline.as_mut().poll(cx).map(Ok);
                }
                restart = true;
                *this.linger_until.insert(deadline.deadline())
            }
        };
        // What still comes is dropped: the size only sets reads per wake.
        let mut discard = [0u8; 4096];
        loop {
            let left = worker::CLIENT_BUF - this.discarded;
            if left == 0 {
                return Poll::Ready(Ok(()));
            }
            let room = left.min(discard.len());
            let mut buf = ReadBuf::new(&mut discard[..room]);
            match Pin::new(&mut this.inner).poll_read(cx, &mut buf) {
                Poll::Ready(Ok(())) if !buf.filled().is_empty() => {
                    this.discarded += buf.filled().len();
                    restart = true;
                }
                // EOF or an error: the client is done.
                Poll::Ready(_) => return Poll::Ready(Ok(())),
                Poll::Pending => break,
            }
        }
        if restart {
            let idle = tokio::time::Instant::now() + worker::LINGER_IDLE;
            deadline.as_mut().reset(idle.min(close_by));
        }
        deadline.as_mut().poll(cx).map(Ok)
    }
}

/// Serve one ALPN-negotiated h2 client connection until it closes.
pub(crate) async fn serve(tls_stream: ServerTlsStream<TcpStream>, ctx: H2Ctx) {
    let io = ClientStream::new(
        tls_stream,
        worker::idle_opt(ctx.cfg.idle_body_timeout),
        ctx.cfg.client_header_timeout,
    );
    let handshake = h2::server::Builder::new()
        .max_concurrent_streams(MAX_CONCURRENT_STREAMS)
        .max_header_list_size(MAX_HEADER_LIST_SIZE)
        .initial_window_size(STREAM_WINDOW)
        .initial_connection_window_size(CONNECTION_WINDOW)
        .handshake(io);
    // The h2 preface + SETTINGS is this protocol's header-read phase; it
    // gets the same slow-loris budget as an h1 header block.
    let mut conn = match tokio::time::timeout(ctx.cfg.client_header_timeout, handshake).await {
        Ok(Ok(c)) => c,
        Ok(Err(_e)) => {
            debug!("h2 handshake failed from {}: {}", ctx.peer, _e);
            return;
        }
        Err(_) => {
            debug!("h2 handshake from {} timed out", ctx.peer);
            return;
        }
    };

    let ctx = Arc::new(ctx);
    // h2 frees a stream's slot the moment the client resets it, while its
    // engine may still wait on the backend: these bound the engines.
    let engines = Arc::new(Semaphore::new(MAX_CONCURRENT_STREAMS as usize));
    let mut draining = false;
    loop {
        let stream = tokio::select! {
            stream = conn.accept() => stream,
            // GOAWAY: the client opens no new streams, the ones in flight
            // finish, and accept() ends once the last of them closes.
            () = ctx.drain.cancelled(), if !draining => {
                draining = true;
                conn.graceful_shutdown();
                continue;
            }
        };
        let Some(Ok((req, respond))) = stream else {
            // Closed, or a connection-level error; h2 already sent GOAWAY
            // where due.
            return;
        };
        let ctx = ctx.clone();
        let engines = engines.clone();
        tokio::spawn(async move {
            let _ = serve_stream(req, respond, &ctx, &engines).await;
        });
    }
}

/// Replay one h2 stream as an h1 exchange through the engine. The engine
/// runs under one of `engines`, and is dropped with its backend exchange
/// the moment the client resets the stream.
async fn serve_stream(
    req: http::Request<h2::RecvStream>,
    mut respond: h2::server::SendResponse<Bytes>,
    ctx: &H2Ctx,
    engines: &Semaphore,
) -> Result<()> {
    let (parts, body) = req.into_parts();
    let is_head = parts.method == http::Method::HEAD;
    let idle = worker::idle_opt(ctx.cfg.idle_body_timeout);

    let Some((head, chunk_body)) = synth_request_head(&parts, body.is_end_stream()) else {
        // CONNECT or no authority: not expressible as an origin-form h1
        // exchange through the engine. Count it like an engine-answered
        // error so refusals are not invisible to /metrics.
        ctx.listener_stats.http_requests.inc();
        let resp = http::Response::builder().status(400).body(()).unwrap();
        let _ = respond.send_response(resp, true);
        return Ok(());
    };

    // Held until the engine is done or dropped, not until h2 frees the slot.
    let _engine_slot = unless_reset(engines.acquire(), |cx| respond.poll_reset(cx)).await?;

    let (io, engine_io) = tokio::io::duplex(DUPLEX_BUF);
    let aborted = Arc::new(AtomicBool::new(false));
    let state = worker::ConnectionState::new(
        ctx.server_port,
        ctx.routes.clone(),
        crate::proxy::backend_pool::PoolHandle::Shared(ctx.pool.clone()),
        ctx.selector.clone(),
        ctx.health.clone(),
        true,
        ctx.peer.ip(),
        &ctx.cfg,
        ctx.listener_stats.clone(),
        // The connection drains by GOAWAY, which lets every accepted stream
        // finish. Its token would end the engine's first read, polled on a
        // still empty pipe, before the request is in.
        CancellationToken::new(),
    );
    let engine_conn = Connection::Mem {
        inner: engine_io,
        aborted: aborted.clone(),
    };
    let engine = worker::handle_connection(engine_conn, ctx.peer, state);

    // The bridge owns its duplex half and drops it on completion, so an
    // engine still reading the duplex sees EOF instead of parking on it.
    let bridge = async move {
        let mut io = io;
        let mut respond = respond;
        let result = async {
            // Waiting for the head, the engine may sit on the backend for
            // its whole budget: only the reset watch sees the client leave.
            let to_head = async {
                // A duplex write failing means the engine already answered
                // and dropped its half (e.g. a routing 404); fall through to
                // read that buffered response rather than aborting the stream.
                if io.write_all(&head).await.is_ok() {
                    // Always pump: for a chunked body this must emit the
                    // terminator even when the body is empty, and for a
                    // content-length/no-body head it returns as soon as the
                    // h2 stream ends.
                    pump_request_body(&mut io, body, chunk_body, idle).await?;
                }
                read_response_head(&mut io).await
            };
            let resp_head = unless_reset(to_head, |cx| respond.poll_reset(cx)).await?;
            forward_response(&mut io, &mut respond, resp_head, is_head, idle, &aborted).await
        }
        .await;
        drop(io);
        if result.is_err() {
            respond.send_reset(h2::Reason::INTERNAL_ERROR);
        }
        result
    };

    // Engine errors (client-equivalent close, backend failures it already
    // answered) are its own business; the stream outcome is the bridge's.
    let mut engine = pin!(engine);
    let mut bridge = pin!(bridge);
    let mut engine_done = false;
    let result = std::future::poll_fn(|cx| {
        if !engine_done {
            engine_done = engine.as_mut().poll(cx).is_ready();
        }
        bridge.as_mut().poll(cx)
    })
    .await;
    // A delivered response lets the engine wind down, as it may still pool
    // its backend conn; a reset or failed stream drops the exchange.
    if result.is_ok() && !engine_done {
        let _ = engine.await;
    }
    result
}

/// Await `fut` unless the client resets the stream, or its connection dies,
/// first: h2 frees a reset stream's slot at once, so nothing the exchange
/// waits on may outlive the reset.
async fn unless_reset<T, E: Into<anyhow::Error>>(
    fut: impl Future<Output = Result<T, E>>,
    mut poll_reset: impl FnMut(&mut Context<'_>) -> Poll<Result<h2::Reason, h2::Error>>,
) -> Result<T> {
    let mut fut = pin!(fut);
    std::future::poll_fn(|cx| {
        if let Poll::Ready(out) = fut.as_mut().poll(cx) {
            return Poll::Ready(out.map_err(Into::into));
        }
        poll_reset(cx).map(|reset| match reset {
            Ok(reason) => Err(anyhow!("client reset the stream: {:?}", reason)),
            Err(e) => Err(anyhow!("h2 stream lost: {}", e)),
        })
    })
    .await
}

/// Serialize an h2 request head as the equivalent h1 head. Returns the head
/// plus whether DATA frames must be re-framed as chunked (no content-length
/// to preserve). `None` when the request cannot ride the h1 replay.
fn synth_request_head(parts: &http::request::Parts, no_body: bool) -> Option<(Vec<u8>, bool)> {
    if parts.method == http::Method::CONNECT {
        return None;
    }
    let authority = parts.uri.authority().map(|a| a.as_str()).or_else(|| {
        parts
            .headers
            .get(http::header::HOST)
            .and_then(|v| v.to_str().ok())
    })?;
    let path = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let has_cl = parts.headers.contains_key(http::header::CONTENT_LENGTH);
    let chunk_body = !no_body && !has_cl;

    let mut head = Vec::with_capacity(256);
    head.extend_from_slice(parts.method.as_str().as_bytes());
    head.push(b' ');
    head.extend_from_slice(path.as_bytes());
    head.extend_from_slice(b" HTTP/1.1\r\nhost: ");
    head.extend_from_slice(authority.as_bytes());
    head.extend_from_slice(b"\r\n");

    // h2 clients may split the cookie header for HPACK efficiency; h1
    // servers expect one line (RFC 9113 8.2.3).
    let mut cookies: Vec<&[u8]> = Vec::new();
    for (name, value) in &parts.headers {
        if name == http::header::COOKIE {
            cookies.push(value.as_bytes());
            continue;
        }
        if skip_request_header(name, no_body) {
            continue;
        }
        head.extend_from_slice(name.as_str().as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(value.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    if !cookies.is_empty() {
        head.extend_from_slice(b"cookie: ");
        for (i, c) in cookies.iter().enumerate() {
            if i > 0 {
                head.extend_from_slice(b"; ");
            }
            head.extend_from_slice(c);
        }
        head.extend_from_slice(b"\r\n");
    }
    // The replay is strictly one exchange per engine invocation: the engine
    // exits after responding and the duplex EOF ends the stream.
    head.extend_from_slice(b"connection: close\r\n");
    if no_body {
        // Explicit zero: a bodyless POST/PUT without any length header gets
        // 411 from some backends.
        head.extend_from_slice(b"content-length: 0\r\n");
    } else if chunk_body {
        head.extend_from_slice(b"transfer-encoding: chunked\r\n");
    }
    head.extend_from_slice(b"\r\n");
    Some((head, chunk_body))
}

/// Connection-scoped headers do not survive protocol translation (RFC 9113
/// 8.2.2). Reuses the engine's request hop-by-hop set (connection, keep-alive,
/// proxy-connection, te, trailer, expect -- expect so the engine's own
/// 100-continue never lands inside a stream) and adds the bridge-specific
/// drops: `host` (re-derived from :authority), `upgrade`, `transfer-encoding`
/// (we choose the request framing), and `content-length` on a bodyless request
/// (replaced by an explicit zero).
fn skip_request_header(name: &http::HeaderName, no_body: bool) -> bool {
    use http::header;
    is_hop_by_hop_request_header(name.as_str().as_bytes())
        || name == header::HOST
        || name == header::UPGRADE
        || name == header::TRANSFER_ENCODING
        || (no_body && name == header::CONTENT_LENGTH)
}

/// Forward the h2 request body into the duplex, re-framed as chunked when
/// the request declared no content-length. A duplex write failure means the
/// engine already answered; the buffered response is still readable, so it
/// is not an error here. An h2-side body error is: the client is gone.
async fn pump_request_body(
    io: &mut DuplexStream,
    mut body: h2::RecvStream,
    chunked: bool,
    idle: Option<Duration>,
) -> Result<()> {
    let mut framed = Vec::new();
    // Bounded like every other bridge await: a client that opens a stream and
    // then withholds DATA must not park this task past the idle budget
    // (client_header_timeout only covers the h2 handshake).
    while let Some(data) = progress("h2 bridge", idle, body.data()).await? {
        let data = data.map_err(|e| anyhow!("h2 request body: {}", e))?;
        let n = data.len();
        // A zero-length interior DATA frame (legal, END_STREAM unset) carries
        // no body bytes; re-framed as a chunk it would be "0\r\n\r\n" -- the
        // chunked terminator -- truncating the body mid-stream. Skip it; the
        // real terminator is written once below.
        if n == 0 {
            continue;
        }
        let write_ok = if chunked {
            use std::io::Write;
            framed.clear();
            framed.reserve(n + 16);
            let _ = write!(framed, "{:x}\r\n", n);
            framed.extend_from_slice(&data);
            framed.extend_from_slice(b"\r\n");
            io.write_all(&framed).await.is_ok()
        } else {
            io.write_all(&data).await.is_ok()
        };
        // Window goes back only once the engine's pipe took the bytes, so h2
        // buffers at most one window past what the engine drained.
        let _ = body.flow_control().release_capacity(n);
        // A closed engine half ends the exchange; its response is buffered.
        if !write_ok {
            return Ok(());
        }
    }
    // Request trailers (body.trailers()) are dropped: they cannot be
    // represented mid-replay without chunked trailer support end-to-end.
    if chunked {
        let _ = io.write_all(b"0\r\n\r\n").await;
    }
    Ok(())
}

/// Read the engine's h1 response head from the duplex. Returns the bytes
/// read, which may run past the head into the body, and where the head ends.
async fn read_response_head(io: &mut DuplexStream) -> Result<(BytesMut, usize)> {
    let mut acc = BytesMut::with_capacity(RESP_READ);
    loop {
        if let Some(end) = find_header_end(&acc) {
            return Ok((acc, end));
        }
        if acc.len() > RESPONSE_HEAD_CAP {
            return Err(anyhow!("engine response headers exceed cap"));
        }
        // The engine's own budgets (backend TTFB, request deadline, idle
        // body) bound how long this read can park; no extra timer needed.
        acc.reserve(RESP_READ);
        if io.read_buf(&mut acc).await? == 0 {
            return Err(anyhow!("engine closed before response headers"));
        }
    }
}

/// Translate the engine's h1 response, its head read by
/// [`read_response_head`], onto the h2 stream: headers mapped (hop-by-hop
/// dropped), body de-framed into DATA. The engine sets `aborted` where an h1
/// client would get a reset.
async fn forward_response(
    io: &mut DuplexStream,
    respond: &mut h2::server::SendResponse<Bytes>,
    (mut acc, header_end): (BytesMut, usize),
    is_head: bool,
    idle: Option<Duration>,
    aborted: &AtomicBool,
) -> Result<()> {
    // The engine framed its output by this same reading, which admits no
    // transfer coding but the chunked framing removed here: h2 carries none.
    let head = worker::parse_response_head(&acc[..header_end])
        .ok_or_else(|| anyhow!("engine sent a malformed response head"))?;
    let status = head.status;
    let (chunked, content_length) = match head.body {
        BodyFraming::Chunked => (true, None),
        BodyFraming::Length(n) => (false, Some(n)),
        BodyFraming::Close => (false, None),
    };
    let no_body = worker::response_has_no_body(status, is_head) || content_length == Some(0);
    // We reframe a chunked body into DATA, so the h2 response must not also
    // carry the backend's content-length: the DATA total need not equal it and
    // a mismatch is malformed per RFC 9113 8.1.1.
    let resp = build_h2_response(&acc[..header_end], status, chunked)?;

    let mut send = respond
        .send_response(resp, no_body)
        .map_err(|e| anyhow!("h2 send_response: {}", e))?;
    if no_body {
        return Ok(());
    }

    // Body bytes that arrived in the same reads as the head.
    acc.advance(header_end);

    if chunked {
        let mut dec = ChunkedStream::new();
        let mut scratch = Vec::with_capacity(16 * 1024);
        loop {
            if feed_chunked(&mut dec, &acc, &mut scratch, &mut send, idle).await? {
                return Ok(());
            }
            acc.clear();
            if read_body(io, &mut acc, &mut send, idle).await? == 0 {
                return Err(anyhow!("truncated chunked response"));
            }
        }
    } else if let Some(cl) = content_length {
        let mut remaining = cl;
        loop {
            let take = acc.len().min(remaining);
            if take > 0 {
                let eos = take == remaining;
                send_data(&mut send, acc.split_to(take).freeze(), eos, idle).await?;
                remaining -= take;
            }
            if remaining == 0 {
                return Ok(());
            }
            if read_body(io, &mut acc, &mut send, idle).await? == 0 {
                return Err(anyhow!("truncated response body"));
            }
        }
    } else {
        // EOF-framed: the engine's `connection: close` reply - everything
        // until duplex EOF is body.
        loop {
            if !acc.is_empty() {
                send_data(&mut send, acc.split().freeze(), false, idle).await?;
            }
            if read_body(io, &mut acc, &mut send, idle).await? == 0 {
                // EOF alone cannot tell this body's end from its cut.
                if aborted.load(Ordering::Acquire) {
                    return Err(anyhow!("engine cut the response short"));
                }
                send.send_data(Bytes::new(), true)
                    .map_err(|e| anyhow!("h2 send: {}", e))?;
                return Ok(());
            }
        }
    }
}

/// Read more of the engine's response body into `acc`. Parked here the
/// bridge polls nothing else on the stream, so the client's reset is
/// watched alongside.
async fn read_body(
    io: &mut DuplexStream,
    acc: &mut BytesMut,
    send: &mut h2::SendStream<Bytes>,
    idle: Option<Duration>,
) -> Result<usize> {
    // After a freeze detaches the accumulator's capacity, an unreserved
    // read_buf degrades to 64-byte reads; reserve keeps them at RESP_READ.
    acc.reserve(RESP_READ);
    let read = unless_reset(io.read_buf(acc), |cx| send.poll_reset(cx));
    progress("h2 bridge", idle, read).await?
}

/// Decode one read's worth of chunked body and forward the payload.
/// Returns `true` once the terminator has passed (stream ended). Malformed
/// framing resets the stream and errors.
async fn feed_chunked(
    dec: &mut ChunkedStream,
    bytes: &[u8],
    scratch: &mut Vec<u8>,
    send: &mut h2::SendStream<Bytes>,
    idle: Option<Duration>,
) -> Result<bool> {
    scratch.clear();
    dec.decode_into(bytes, scratch);
    if dec.is_broken() {
        // Reset here rather than trust every caller to: only a reset, never
        // END_STREAM, tells the client its body is incomplete.
        send.send_reset(h2::Reason::INTERNAL_ERROR);
        return Err(anyhow!("broken chunked framing from engine"));
    }
    let done = dec.is_terminated();
    if !scratch.is_empty() || done {
        send_data(send, Bytes::copy_from_slice(scratch), done, idle).await?;
    }
    Ok(done)
}

/// Map the engine's h1 response head onto an h2 response. `drop_content_length`
/// strips the backend's content-length (set when the body is chunk-decoded, so
/// the DATA total may differ). A header the http crate rejects is dropped, not
/// fatal: the h1 client gets it verbatim, so the h2 client must not lose the
/// whole response to one odd line.
fn build_h2_response(
    head: &[u8],
    status: u16,
    drop_content_length: bool,
) -> Result<http::Response<()>> {
    use http::header::{HeaderName, HeaderValue};
    let mut builder = http::Response::builder()
        .status(http::StatusCode::from_u16(status).map_err(|_| anyhow!("bad engine status"))?);
    let headers = builder
        .headers_mut()
        .ok_or_else(|| anyhow!("h2 response builder init failed"))?;
    for line in header_lines(head) {
        // Obs-fold continuation (leading OWS, RFC 7230 3.2.4, deprecated) is
        // not a header on its own; skip it rather than fabricate a bad name.
        // The status line (no colon) is skipped by the colon check below.
        if matches!(line.first(), Some(b' ' | b'\t')) {
            continue;
        }
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        let name = &line[..colon];
        if skip_response_header(name)
            || (drop_content_length && name.eq_ignore_ascii_case(b"content-length"))
        {
            continue;
        }
        // Trim leading AND trailing OWS: a trailing space in an h2 field value
        // is malformed (RFC 9113 8.2.1) and strict clients reject it.
        let value = trim_ows(&line[colon + 1..]);
        // Drop a single header the http crate rejects; never fail the whole
        // response into an INTERNAL_ERROR reset over one bad byte.
        let (Ok(hn), Ok(hv)) = (HeaderName::from_bytes(name), HeaderValue::from_bytes(value))
        else {
            continue;
        };
        headers.append(hn, hv);
    }
    builder
        .body(())
        .map_err(|e| anyhow!("h2 response build: {}", e))
}

/// Response headers that must not cross to the h2 side: the shared hop-by-hop
/// set plus transfer-encoding (the bridge de-frames the body into DATA).
fn skip_response_header(name: &[u8]) -> bool {
    is_hop_by_hop_response_header(name) || name.eq_ignore_ascii_case(b"transfer-encoding")
}

/// Send one payload under h2 flow control, splitting to granted window.
async fn send_data(
    send: &mut h2::SendStream<Bytes>,
    mut data: Bytes,
    eos: bool,
    idle: Option<Duration>,
) -> Result<()> {
    if data.is_empty() {
        send.send_data(data, eos)
            .map_err(|e| anyhow!("h2 send: {}", e))?;
        return Ok(());
    }
    while !data.is_empty() {
        send.reserve_capacity(data.len());
        // A client that stops granting window would park this forever; the
        // same per-step budget that bounds h1 body writes applies.
        let granted = progress(
            "h2 bridge",
            idle,
            std::future::poll_fn(|cx| send.poll_capacity(cx)),
        )
        .await?
        .ok_or_else(|| anyhow!("h2 stream closed"))?
        .map_err(|e| anyhow!("h2 capacity: {}", e))?;
        if granted == 0 {
            continue;
        }
        let chunk = data.split_to(granted.min(data.len()));
        let last = eos && data.is_empty();
        send.send_data(chunk, last)
            .map_err(|e| anyhow!("h2 send: {}", e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(req: http::request::Builder) -> http::request::Parts {
        req.body(()).unwrap().into_parts().0
    }

    fn head_str(head: &[u8]) -> &str {
        std::str::from_utf8(head).unwrap()
    }

    #[test]
    fn synth_get_is_origin_form_single_exchange() {
        let p = parts(
            http::Request::builder()
                .method("GET")
                .uri("https://example.com/a/b?x=1")
                .header("user-agent", "t"),
        );
        let (head, chunk) = synth_request_head(&p, true).unwrap();
        let s = head_str(&head);
        assert!(s.starts_with("GET /a/b?x=1 HTTP/1.1\r\nhost: example.com\r\n"));
        assert!(s.contains("user-agent: t\r\n"));
        assert!(s.contains("connection: close\r\n"));
        assert!(s.contains("content-length: 0\r\n"));
        assert!(s.ends_with("\r\n\r\n"));
        assert!(!chunk);
    }

    #[test]
    fn synth_strips_connection_scoped_headers() {
        let p = parts(
            http::Request::builder()
                .method("GET")
                .uri("https://h.test/")
                .header("te", "trailers")
                .header("upgrade", "websocket")
                .header("keep-alive", "300")
                .header("proxy-connection", "keep-alive")
                .header("trailer", "x-t")
                .header("expect", "100-continue"),
        );
        let (head, _) = synth_request_head(&p, true).unwrap();
        let s = head_str(&head).to_ascii_lowercase();
        for h in [
            "te:",
            "upgrade:",
            "keep-alive:",
            "proxy-connection:",
            "trailer:",
            "expect:",
        ] {
            assert!(!s.contains(h), "{} must be stripped, got:\n{}", h, s);
        }
    }

    #[test]
    fn synth_body_framing_follows_content_length() {
        // Declared length: copied through, no re-framing.
        let p = parts(
            http::Request::builder()
                .method("POST")
                .uri("https://h.test/u")
                .header("content-length", "5"),
        );
        let (head, chunk) = synth_request_head(&p, false).unwrap();
        assert!(head_str(&head).contains("content-length: 5\r\n"));
        assert!(!chunk);

        // No length: DATA frames become a chunked h1 body.
        let p = parts(
            http::Request::builder()
                .method("POST")
                .uri("https://h.test/u"),
        );
        let (head, chunk) = synth_request_head(&p, false).unwrap();
        assert!(head_str(&head).contains("transfer-encoding: chunked\r\n"));
        assert!(chunk);
    }

    #[test]
    fn synth_joins_split_cookies() {
        let p = parts(
            http::Request::builder()
                .method("GET")
                .uri("https://h.test/")
                .header("cookie", "a=1")
                .header("cookie", "b=2"),
        );
        let (head, _) = synth_request_head(&p, true).unwrap();
        assert!(head_str(&head).contains("cookie: a=1; b=2\r\n"));
    }

    #[test]
    fn synth_refuses_connect_and_missing_authority() {
        let p = parts(http::Request::builder().method("CONNECT").uri("h.test:443"));
        assert!(synth_request_head(&p, true).is_none());

        let p = parts(http::Request::builder().method("GET").uri("/no-authority"));
        assert!(synth_request_head(&p, true).is_none());
    }

    #[test]
    fn response_mapping_strips_hop_by_hop_keeps_the_rest() {
        // Content-Length body (not chunked): the length is preserved.
        let head = b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nconnection: close\r\ncontent-length: 3\r\nx-custom: v\r\n\r\n";
        let status = worker::parse_response_head(head).unwrap().status;
        let resp = build_h2_response(head, status, false).unwrap();
        assert_eq!(resp.status(), 200);
        let h = resp.headers();
        assert_eq!(h.get("content-type").unwrap(), "text/plain");
        assert_eq!(h.get("x-custom").unwrap(), "v");
        assert_eq!(h.get("content-length").unwrap(), "3");
        assert!(h.get("connection").is_none());
    }

    #[test]
    fn response_mapping_drops_content_length_when_chunked() {
        // A chunked body is de-framed into DATA; keeping the backend's
        // content-length would let it contradict the DATA total.
        let head = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\ncontent-length: 100\r\nx-k: v\r\n\r\n";
        let resp = build_h2_response(head, 200, true).unwrap();
        let h = resp.headers();
        assert!(h.get("content-length").is_none());
        assert!(h.get("transfer-encoding").is_none());
        assert_eq!(h.get("x-k").unwrap(), "v");
    }

    #[test]
    fn response_mapping_trims_trailing_ows_and_skips_obs_fold() {
        // Trailing whitespace is illegal in an h2 field value; obs-fold
        // continuation lines are skipped rather than fabricating a bad name.
        let head = b"HTTP/1.1 200 OK\r\nx-token: abc \r\n more-of-x-token\r\nx-b: y\r\n\r\n";
        let resp = build_h2_response(head, 200, false).unwrap();
        let h = resp.headers();
        assert_eq!(h.get("x-token").unwrap(), "abc");
        assert_eq!(h.get("x-b").unwrap(), "y");
    }

    #[test]
    fn response_mapping_drops_one_bad_header_not_whole_response() {
        // A header the http crate rejects must not fail the entire response.
        let head = b"HTTP/1.1 200 OK\r\nx-good: 1\r\nx-b\x01ad: 2\r\nx-also-good: 3\r\n\r\n";
        let resp = build_h2_response(head, 200, false).unwrap();
        let h = resp.headers();
        assert_eq!(h.get("x-good").unwrap(), "1");
        assert_eq!(h.get("x-also-good").unwrap(), "3");
    }

    #[test]
    fn response_mapping_rejects_unparseable_status() {
        assert!(build_h2_response(b"garbage\r\n\r\n", 0, false).is_err());
    }

    /// A server TLS stream and its client, past a handshake that picked h2;
    /// `None` without openssl.
    async fn tls_pair() -> Option<(
        ServerTlsStream<TcpStream>,
        tokio_rustls::client::TlsStream<TcpStream>,
    )> {
        let (cert_pem, key_pem) = crate::proxy::tls::test_util::generate_test_cert("h2.test")?;
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let cert = crate::config::CertificateRef {
            name: "h2".to_string(),
            cert_pem: Some(cert_pem),
            key_pem: Some(key_pem),
            ..Default::default()
        };
        let server_cfg =
            crate::proxy::tls::build_server_config(&[cert], std::path::Path::new("/unused"), true)
                .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_cfg));
        let mut client_cfg = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(crate::proxy::backend_pool::NoVerifier))
            .with_no_client_auth();
        client_cfg.alpn_protocols = vec![b"h2".to_vec()];
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_cfg));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let sni = rustls::pki_types::ServerName::try_from("h2.test").unwrap();
        let (server, client) = tokio::join!(
            async { acceptor.accept(listener.accept().await.unwrap().0).await },
            async {
                connector
                    .connect(sni, TcpStream::connect(addr).await.unwrap())
                    .await
            },
        );
        Some((server.unwrap(), client.unwrap()))
    }

    /// A client that stops reading leaves close_notify no room to go out:
    /// the close budget bounds that write, as it bounds the linger after.
    #[tokio::test]
    async fn a_close_notify_the_client_never_reads_is_bounded() {
        let Some((server, _client)) = tls_pair().await else {
            return; // openssl unavailable
        };
        let budget = Duration::from_millis(300);
        let mut io = ClientStream::new(server, None, budget);
        let chunk = [0u8; 64 * 1024];
        while tokio::time::timeout(budget, io.write(&chunk)).await.is_ok() {}

        let started = std::time::Instant::now();
        let closed = tokio::time::timeout(budget * 10, io.shutdown()).await;
        assert!(
            closed.is_ok(),
            "close_notify held the close past its budget"
        );
        assert!(started.elapsed() >= budget, "{:?}", started.elapsed());
    }

    /// A client that keeps its socket open after the close held it, its
    /// task and a drain for the whole close budget. Silent for LINGER_IDLE,
    /// it is done; one still sending is read until it stops, or the budget
    /// runs out.
    #[tokio::test(start_paused = true)]
    async fn a_closing_h2_connection_lingers_while_the_client_sends() {
        let budget = worker::LINGER_IDLE * 6;
        let every = worker::LINGER_IDLE / 2;
        for (sends, lingered) in [
            (0, worker::LINGER_IDLE),
            (4, every * 4 + worker::LINGER_IDLE),
            (usize::MAX, budget),
        ] {
            let (server, mut client) = tokio::io::duplex(64 * 1024);
            let sender = tokio::spawn(async move {
                for _ in 0..sends {
                    tokio::time::sleep(every).await;
                    let _ = client.write_all(b"x").await;
                }
                // Never closes.
                std::future::pending::<()>().await;
            });
            let mut io = ClientStream::new(server, None, budget);
            let started = tokio::time::Instant::now();
            io.shutdown().await.unwrap();
            assert_eq!(started.elapsed(), lingered, "{sends} sends");
            sender.abort();
        }
    }

    /// A client that never stops sending held the close for its whole
    /// budget: the linger read off all of it. It stops after one h1
    /// buffer's worth, as h1's close does.
    #[tokio::test(start_paused = true)]
    async fn a_closing_h2_connection_reads_off_one_buffer_at_most() {
        let budget = worker::LINGER_IDLE * 6;
        let every = worker::LINGER_IDLE / 2;
        let (server, mut client) = tokio::io::duplex(worker::CLIENT_BUF);
        let sender = tokio::spawn(async move {
            let chunk = [b'x'; worker::CLIENT_BUF / 4];
            loop {
                tokio::time::sleep(every).await;
                let _ = client.write_all(&chunk).await;
            }
        });
        let mut io = ClientStream::new(server, None, budget);
        let started = tokio::time::Instant::now();
        io.shutdown().await.unwrap();
        assert_eq!(started.elapsed(), every * 4);
        sender.abort();
    }

    /// A client stream taking every write at once but completing a flush
    /// only `every` after the last one: a client reading at that pace.
    struct SlowReader {
        every: Duration,
        flushed: Pin<Box<tokio::time::Sleep>>,
    }

    impl SlowReader {
        fn new(every: Duration) -> Self {
            Self {
                every,
                flushed: Box::pin(tokio::time::sleep(every)),
            }
        }
    }

    impl AsyncRead for SlowReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for SlowReader {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            ready!(self.flushed.as_mut().poll(cx));
            let next = tokio::time::Instant::now() + self.every;
            self.flushed.as_mut().reset(next);
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Only a completed flush shows the client reading: each restarts the
    /// write clock, while a write the stream merely took does not.
    #[tokio::test(start_paused = true)]
    async fn only_a_completed_flush_restarts_the_write_clock() {
        let budget = Duration::from_secs(10);
        let client = |every, idle| ClientStream::new(SlowReader::new(every), idle, budget);

        let mut io = client(budget * 3 / 4, Some(budget));
        for _ in 0..8 {
            io.write_all(b"x").await.unwrap();
            io.flush().await.expect("a client keeping pace was dropped");
        }

        let mut io = client(budget * 2, Some(budget));
        io.write_all(b"x").await.unwrap();
        let first = tokio::time::timeout(budget / 2, io.flush()).await;
        assert!(first.is_err(), "flushed early");
        io.write_all(b"x").await.unwrap();
        let started = tokio::time::Instant::now();
        let stalled = io.flush().await.unwrap_err();
        assert_eq!(stalled.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < budget,
            "a taken write restarted the clock"
        );

        let mut io = client(budget * 2, None);
        io.write_all(b"x").await.unwrap();
        io.flush()
            .await
            .expect("idleBodyTimeout 0s timed a write out");
    }

    /// Backend answering each request head with a `body` of `len` bytes,
    /// sent as fast as the proxy takes it.
    async fn backend(len: usize, body: &'static [u8]) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let (mut seen, mut chunk) = (Vec::new(), [0u8; 4096]);
                    while let Ok(n @ 1..) = sock.read(&mut chunk).await {
                        seen.extend_from_slice(&chunk[..n]);
                        if find_header_end(&seen).is_none() {
                            continue;
                        }
                        seen.clear();
                        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {len}\r\n\r\n");
                        if sock.write_all(head.as_bytes()).await.is_err() {
                            return;
                        }
                        for _ in 0..len / body.len() {
                            if sock.write_all(body).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });
        addr
    }

    /// The bridge's context for a.test routed to `backend`, with `budget`
    /// as idleBodyTimeout and clientHeaderTimeout.
    fn ctx(backend: SocketAddr, budget: Duration) -> H2Ctx {
        let mut routes = RouteTable::new();
        routes.add_http_route(
            "a.test",
            crate::routing::HttpRouteRule::new(
                crate::routing::PathMatchType::Prefix,
                "/".to_string(),
                vec![],
                vec![],
                vec![],
                vec![crate::routing::Backend {
                    socket_addr: backend,
                    weight: 1,
                    filters: vec![],
                    use_tls: false,
                    server_name: String::new(),
                }],
            ),
        );
        let cfg = WorkerConfig {
            max_idle_per_backend: 4,
            connect_timeout: Duration::from_secs(2),
            client_header_timeout: budget,
            tcp_keepalive_time: Duration::from_secs(60),
            idle_body_timeout: budget,
            http2: true,
        };
        H2Ctx {
            server_port: 8080,
            routes: Arc::new(ArcSwap::from_pointee(routes)),
            selector: Arc::new(BackendSelector::new()),
            health: Arc::new(HealthRegistry::new()),
            pool: Arc::new(SharedBackendPool::new(4, cfg.connect_timeout)),
            cfg,
            listener_stats: Arc::new(ListenerStats::default()),
            peer: "127.0.0.1:40000".parse().unwrap(),
            drain: CancellationToken::new(),
        }
    }

    /// A client stream that stops reading once `reading` is cleared.
    struct Unread<S> {
        inner: S,
        reading: Arc<AtomicBool>,
    }

    impl<S: AsyncRead + Unpin> AsyncRead for Unread<S> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if !self.reading.load(Ordering::Relaxed) {
                return Poll::Pending;
            }
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }

    impl<S: AsyncWrite + Unpin> AsyncWrite for Unread<S> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.inner).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    /// Windows no response here outgrows, so only the sockets push back.
    const CLIENT_WINDOW: u32 = 1 << 30;

    /// What a client that stopped reading mid-response does next.
    #[derive(Debug, Clone, Copy)]
    enum Then {
        Waits,
        ResetsItsStream,
        /// h2 flushes before it shuts the stream down.
        Closes,
    }

    /// h2 has no send timeout: a client that stopped reading held its
    /// connection forever, its streams reset or its close stuck in h2's
    /// last flush. A write pending past idleBodyTimeout now drops it.
    #[tokio::test]
    async fn a_client_that_stops_reading_loses_its_h2_connection() {
        use std::os::fd::AsFd;
        let budget = Duration::from_millis(500);
        let addr = backend(1 << 40, &[b'x'; 64 * 1024]).await;
        for then in [Then::Waits, Then::ResetsItsStream, Then::Closes] {
            let Some((server, client)) = tls_pair().await else {
                return; // openssl unavailable
            };
            let mut served = tokio::spawn(serve(server, ctx(addr, budget)));
            // Closed on unread bytes once h2 drops the client stream, the
            // socket would reset: the proxy's next write would fail anyway.
            let _socket = client.get_ref().0.as_fd().try_clone_to_owned().unwrap();
            let reading = Arc::new(AtomicBool::new(true));
            let io = Unread {
                inner: client,
                reading: Arc::clone(&reading),
            };
            let (send, conn) = h2::client::Builder::new()
                .initial_window_size(CLIENT_WINDOW)
                .initial_connection_window_size(CLIENT_WINDOW)
                .handshake::<_, Bytes>(io)
                .await
                .unwrap();
            tokio::spawn(conn);
            let mut send = send.ready().await.unwrap();
            let req = http::Request::get("https://a.test/").body(()).unwrap();
            let (resp, mut stream) = send.send_request(req, true).unwrap();
            let resp = resp.await.unwrap();
            reading.store(false, Ordering::Relaxed);
            // Every buffer on the way fills well within this.
            tokio::time::sleep(budget / 2).await;
            match then {
                Then::Waits => {}
                Then::ResetsItsStream => stream.send_reset(h2::Reason::CANCEL),
                Then::Closes => drop((send, resp, stream)),
            }
            let ended = tokio::time::timeout(budget * 4, &mut served).await;
            assert!(ended.is_ok(), "{then:?}: connection held past its budget");
        }
    }

    /// GET a.test over `send`: the status and body.
    async fn get(send: &h2::client::SendRequest<Bytes>) -> (u16, Vec<u8>) {
        let mut send = send.clone().ready().await.unwrap();
        let req = http::Request::get("https://a.test/").body(()).unwrap();
        let (resp, _) = send.send_request(req, true).unwrap();
        let resp = resp.await.unwrap();
        let status = resp.status().as_u16();
        let mut body = resp.into_body();
        let mut out = Vec::new();
        while let Some(chunk) = body.data().await {
            let chunk = chunk.unwrap();
            out.extend_from_slice(&chunk);
            let _ = body.flow_control().release_capacity(chunk.len());
        }
        (status, out)
    }

    /// Reads stay unbounded: with nothing to write, an h2 connection idles
    /// past every budget, as an h1 keepalive connection does.
    #[tokio::test]
    async fn an_idle_h2_connection_outlives_its_budgets() {
        let budget = Duration::from_millis(200);
        let addr = backend(2, b"ok").await;
        let Some((server, client)) = tls_pair().await else {
            return; // openssl unavailable
        };
        let served = tokio::spawn(serve(server, ctx(addr, budget)));
        let (send, conn) = h2::client::handshake(client).await.unwrap();
        tokio::spawn(conn);
        for _ in 0..2 {
            assert_eq!(get(&send).await, (200, b"ok".to_vec()));
            tokio::time::sleep(budget * 3).await;
            assert!(!served.is_finished(), "an idle connection was dropped");
        }
    }
}
