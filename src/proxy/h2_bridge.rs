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

/// The client's stream as h2 drives it. h2 ends a connection by shutting
/// the stream down and dropping it, while a client closing in step still
/// sends its own GOAWAY and close_notify: closed on those unread, the kernel
/// resets and the client's close fails. So the shutdown reads off what
/// still comes until the client closes too, within `budget`, as nginx does
/// for HTTP/2.
struct LingeringClose<S> {
    inner: S,
    budget: Duration,
    /// Armed once the stream is shut down.
    deadline: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<S: AsyncRead + Unpin> AsyncRead for LingeringClose<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for LingeringClose<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        if this.deadline.is_none() {
            ready!(Pin::new(&mut this.inner).poll_shutdown(cx))?;
            this.deadline = Some(Box::pin(tokio::time::sleep(this.budget)));
        }
        // What still comes is dropped: the size only sets reads per wake.
        let mut discard = [0u8; 4096];
        loop {
            let mut buf = ReadBuf::new(&mut discard);
            match Pin::new(&mut this.inner).poll_read(cx, &mut buf) {
                Poll::Ready(Ok(())) if !buf.filled().is_empty() => {}
                // EOF or an error: the client is done.
                Poll::Ready(_) => return Poll::Ready(Ok(())),
                Poll::Pending => {
                    return match &mut this.deadline {
                        Some(deadline) => deadline.as_mut().poll(cx).map(Ok),
                        None => Poll::Ready(Ok(())),
                    };
                }
            }
        }
    }
}

/// Serve one ALPN-negotiated h2 client connection until it closes.
pub(crate) async fn serve(tls_stream: ServerTlsStream<TcpStream>, ctx: H2Ctx) {
    let io = LingeringClose {
        inner: tls_stream,
        budget: ctx.cfg.client_header_timeout,
        deadline: None,
    };
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
}
