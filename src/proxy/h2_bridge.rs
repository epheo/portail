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

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use arc_swap::ArcSwap;
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream as ServerTlsStream;

use crate::logging::debug;
use crate::metrics::ListenerStats;
use crate::proxy::backend_pool::SharedBackendPool;
use crate::proxy::chunked::ChunkedStream;
use crate::proxy::health::HealthRegistry;
use crate::proxy::http_filters::{
    header_lines, is_hop_by_hop_request_header, is_hop_by_hop_response_header,
};
use crate::proxy::http_parser::find_header_end;
use crate::proxy::tls::Connection;
use crate::proxy::worker::{self, WorkerConfig};
use crate::routing::{BackendSelector, RouteTable};

/// Streams per connection: enough for browser multiplexing, small enough
/// that one connection cannot spawn unbounded engine exchanges.
const MAX_CONCURRENT_STREAMS: u32 = 128;

/// Decoded header-list cap, kept under the engine's 64 KiB request buffer
/// so a synthesized head always fits in one engine read and never trips
/// its 431 path.
const MAX_HEADER_LIST_SIZE: u32 = 48 * 1024;

/// Per-direction duplex buffer; matches the engine's read buffer so a
/// typical exchange crosses in one hop.
const DUPLEX_BUF: usize = 64 * 1024;

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
}

/// Serve one ALPN-negotiated h2 client connection until it closes.
pub(crate) async fn serve(tls_stream: ServerTlsStream<TcpStream>, ctx: H2Ctx) {
    let handshake = h2::server::Builder::new()
        .max_concurrent_streams(MAX_CONCURRENT_STREAMS)
        .max_header_list_size(MAX_HEADER_LIST_SIZE)
        .handshake(tls_stream);
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
    while let Some(stream) = conn.accept().await {
        let Ok((req, respond)) = stream else {
            // Connection-level error; h2 already sent GOAWAY where due.
            return;
        };
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let _ = serve_stream(req, respond, &ctx).await;
        });
    }
}

/// Replay one h2 stream as an h1 exchange through the engine.
async fn serve_stream(
    req: http::Request<h2::RecvStream>,
    respond: h2::server::SendResponse<Bytes>,
    ctx: &H2Ctx,
) -> Result<()> {
    let (parts, body) = req.into_parts();
    let is_head = parts.method == http::Method::HEAD;
    let idle = worker::idle_opt(ctx.cfg.idle_body_timeout);

    let Some((head, chunk_body)) = synth_request_head(&parts, body.is_end_stream()) else {
        // CONNECT or no authority: not expressible as an origin-form h1
        // exchange through the engine. Count it like an engine-answered
        // error so refusals are not invisible to /metrics.
        ctx.listener_stats
            .http_requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut respond = respond;
        let resp = http::Response::builder().status(400).body(()).unwrap();
        let _ = respond.send_response(resp, true);
        return Ok(());
    };

    let (io, engine_io) = tokio::io::duplex(DUPLEX_BUF);
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
    );
    let engine = worker::handle_connection(Connection::Mem { inner: engine_io }, ctx.peer, state);

    // The bridge owns its duplex half and drops it on completion, so the
    // engine sees EOF and unblocks even when the client aborts the stream;
    // otherwise the engine would park on the duplex until idle_body_timeout,
    // pinning a backend connection long after the client gave up.
    let bridge = async move {
        let mut io = io;
        let mut respond = respond;
        let result = async {
            // A duplex write failing means the engine already answered and
            // dropped its half (e.g. a routing 404); fall through to read
            // that buffered response rather than aborting the stream.
            if io.write_all(&head).await.is_ok() {
                // Always pump: for a chunked body this must emit the
                // terminator even when the body is empty, and for a
                // content-length/no-body head it returns as soon as the h2
                // stream ends.
                pump_request_body(&mut io, body, chunk_body, idle).await?;
            }
            forward_response(&mut io, &mut respond, is_head, idle).await
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
    let (_engine, result) = tokio::join!(engine, bridge);
    result
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
    while let Some(data) = progress(idle, body.data()).await? {
        let data = data.map_err(|e| anyhow!("h2 request body: {}", e))?;
        let n = data.len();
        // Release the flow-control window for every received byte, empty
        // frames included, before deciding whether to forward.
        let _ = body.flow_control().release_capacity(n);
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

/// Read the engine's h1 response from the duplex and translate it onto the
/// h2 stream: headers mapped (hop-by-hop dropped), body de-framed into DATA.
async fn forward_response(
    io: &mut DuplexStream,
    respond: &mut h2::server::SendResponse<Bytes>,
    is_head: bool,
    idle: Option<Duration>,
) -> Result<()> {
    let mut head: Vec<u8> = Vec::with_capacity(1024);
    let mut buf = vec![0u8; RESP_READ];
    let header_end = loop {
        if let Some(end) = find_header_end(&head) {
            break end;
        }
        if head.len() > RESPONSE_HEAD_CAP {
            return Err(anyhow!("engine response headers exceed cap"));
        }
        // The engine's own budgets (backend TTFB, request deadline, idle
        // body) bound how long this read can park; no extra timer needed.
        let n = io.read(&mut buf).await?;
        if n == 0 {
            return Err(anyhow!("engine closed before response headers"));
        }
        head.extend_from_slice(&buf[..n]);
    };

    let status = worker::parse_response_status(&head[..header_end]);
    let content_length = worker::parse_content_length(&head[..header_end]);
    let chunked = worker::is_chunked_transfer(&head[..header_end]);
    // Chunked framing wins over content-length (RFC 7230 3.3.3), matching the
    // engine, so a chunked body is never cut short by a stray Content-Length: 0.
    let no_body =
        worker::response_has_no_body(status, is_head) || (!chunked && content_length == Some(0));
    // We reframe a chunked body into DATA, so the h2 response must not also
    // carry the backend's content-length: the DATA total need not equal it and
    // a mismatch is malformed per RFC 9113 8.1.1.
    let resp = build_h2_response(&head[..header_end], status, chunked)?;

    let mut send = respond
        .send_response(resp, no_body)
        .map_err(|e| anyhow!("h2 send_response: {}", e))?;
    if no_body {
        return Ok(());
    }

    // Body bytes that arrived in the same reads as the head.
    let tail = head.split_off(header_end);

    if chunked {
        let mut dec = ChunkedStream::new();
        let mut scratch = Vec::with_capacity(16 * 1024);
        if feed_chunked(&mut dec, &tail, &mut scratch, &mut send, idle).await? {
            return Ok(());
        }
        loop {
            let n = progress(idle, io.read(&mut buf)).await??;
            if n == 0 {
                return Err(anyhow!("truncated chunked response"));
            }
            if feed_chunked(&mut dec, &buf[..n], &mut scratch, &mut send, idle).await? {
                return Ok(());
            }
        }
    } else if let Some(cl) = content_length {
        let mut remaining = cl;
        let take = tail.len().min(remaining);
        if take > 0 {
            send_data(
                &mut send,
                Bytes::copy_from_slice(&tail[..take]),
                take == remaining,
                idle,
            )
            .await?;
            remaining -= take;
        }
        while remaining > 0 {
            let n = progress(idle, io.read(&mut buf)).await??;
            if n == 0 {
                return Err(anyhow!("truncated response body"));
            }
            let take = n.min(remaining);
            send_data(
                &mut send,
                Bytes::copy_from_slice(&buf[..take]),
                take == remaining,
                idle,
            )
            .await?;
            remaining -= take;
        }
        Ok(())
    } else {
        // EOF-framed: the engine's `connection: close` reply - everything
        // until duplex EOF is body.
        if !tail.is_empty() {
            send_data(&mut send, Bytes::copy_from_slice(&tail), false, idle).await?;
        }
        loop {
            let n = progress(idle, io.read(&mut buf)).await??;
            if n == 0 {
                send.send_data(Bytes::new(), true)
                    .map_err(|e| anyhow!("h2 send: {}", e))?;
                return Ok(());
            }
            send_data(&mut send, Bytes::copy_from_slice(&buf[..n]), false, idle).await?;
        }
    }
}

/// Decode one read's worth of chunked body and forward the payload.
/// Returns `true` once the terminator has passed (stream ended).
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

/// Strip leading and trailing optional whitespace (SP / HTAB) from a value.
fn trim_ows(mut v: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = v {
        v = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = v {
        v = rest;
    }
    v
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
        let granted = progress(idle, std::future::poll_fn(|cx| send.poll_capacity(cx)))
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

/// Per-step progress budget (None = unbounded), the bridge-side counterpart
/// of the engine's `idle_guarded` (which is bound to io::Result; this wraps
/// any future, e.g. h2 body reads and flow-control capacity).
async fn progress<T, F>(idle: Option<Duration>, fut: F) -> Result<T>
where
    F: std::future::Future<Output = T>,
{
    match idle {
        Some(d) => tokio::time::timeout(d, fut)
            .await
            .map_err(|_| anyhow!("h2 bridge stalled: no progress within {:?}", d)),
        None => Ok(fut.await),
    }
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
        let resp = build_h2_response(head, worker::parse_response_status(head), false).unwrap();
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
