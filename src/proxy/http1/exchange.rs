//! One request and its response through a backend: the head rewritten
//! and sent, replayed once on a pooled connection that died, and the
//! response head sent back.

use std::pin::Pin;
use std::time::Duration;

use anyhow::Result;
use tokio::io::AsyncWriteExt;

use super::backend::{connect_to_backend, fail_connect, fail_send, refuse_locally};
use super::body::{forward_response_body, relay_request_body, reset_on_drop, BodyEnd, RequestBody};
use super::reply::send_error_response;
use super::{keep_open, ConnectionState, DrainWatch};
use crate::logging::{debug, warn};
use crate::metrics::METRICS;
use crate::proxy::backend_pool::ConnectError;
use crate::proxy::chunked::ChunkedStream;
use crate::proxy::deadline::{clamp_to_deadline, deadline_after, within};
use crate::proxy::http_filters::{
    apply_request_header_modifications, apply_response_header_mods, assemble_forward_headers,
    assemble_forward_headers_slow, dispatch_mirrors, extract_header_mods, extract_url_rewrite,
    sanitize_response_headers, ForwardedLines, HeaderModifications, OriginForm,
};
use crate::proxy::relay;
use crate::proxy::request_processor::{client_done, ClientSent, RequestMeta};
use crate::proxy::tls::Connection;
use crate::proxy::upstream::response::{
    arrived_head, frame_response, read_response_head, BodyFraming, HeadRead, ResponseFraming,
};
use crate::routing::{BackendAction, HttpFilter, HttpRouteRule};

/// Where a request ends within the bytes read with its head, by its own
/// framing, upgrade or not. Bytes past the end must never reach the backend
/// as body (a desync/smuggling vector: the backend would parse them as a
/// second request and answer it, poisoning the pooled connection). They are
/// held back as leftover: the next pipelined request for the keepalive loop,
/// or early tunnel bytes once the backend accepts an upgrade.
pub(super) struct Framing {
    read: usize,
    header_end: usize,
    end: usize,
    /// Carries on past what was read.
    chunked: Option<ChunkedStream>,
}

impl Framing {
    /// `None` when the chunked body read so far is malformed: settled before
    /// the request is dispatched, so it costs no backend connection.
    pub(super) fn of(read: &[u8], meta: &RequestMeta) -> Option<Self> {
        let header_end = meta.head_len;
        let tail = &read[header_end..];
        let (end, chunked) = if let Some(cl) = meta.content_length {
            (header_end + tail.len().min(cl), None)
        } else if meta.is_chunked {
            let mut stream = ChunkedStream::new();
            let consumed = stream.observe(tail);
            if stream.is_broken() {
                return None;
            }
            (header_end + consumed, Some(stream))
        } else {
            // No declared body: the entire tail is the next pipelined request.
            (header_end, None)
        };
        Some(Self {
            read: read.len(),
            header_end,
            end,
            chunked,
        })
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
///
/// `timer` is the connection's own, for the backend budget (see `within`).
#[allow(clippy::too_many_arguments)]
pub(super) async fn proxy_http_request(
    client: &mut Connection,
    buf: &mut Vec<u8>,
    framing: Framing,
    backend_ref: &crate::routing::Backend,
    rule: &HttpRouteRule,
    meta: &RequestMeta,
    state: &mut ConnectionState,
    mut timer: Pin<&mut tokio::time::Sleep>,
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
    let backend_deadline = deadline_after(backend_phase_start, backend_budget, total_deadline);
    // Cleared per request so an Err exit can never surface the previous
    // keepalive response's status to the access log.
    state.last_status = 0;

    let Framing {
        read: request_bytes,
        header_end: req_header_end,
        end: request_end,
        chunked: req_chunked,
    } = framing;
    let tail_len = request_bytes - req_header_end;
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
    let connect = connect_to_backend(client, state, backend_ref, timer.as_mut(), backend_deadline);
    let (backend, from_pool) = match connect.await? {
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
        let head_wait = (!answered_early).then_some((timer.as_mut(), backend_deadline));
        // With the body cut short, the connection neither tunnels nor
        // stays open.
        match forward_response_headers(
            client,
            &mut backend,
            &mut state.header_buf,
            &mut state.scratch_buf,
            &mut state.filter_buf,
            resp_mods.as_ref(),
            head_wait,
            meta.is_head,
            meta.is_upgrade && !answered_early,
            meta.keepalive && !answered_early,
            &state.drain_watch,
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
        relay::both_ways(&mut *client, &mut backend, state.idle_body_timeout).await;
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
    if backend_conn_poolable(framing_clean, meta.is_head, meta.is_upgrade)
        && !framing.backend_close
        && backend_ref.action != BackendAction::ForwardProxied
    {
        state.pool.release(
            backend_addr,
            backend_ref.use_tls,
            &backend_ref.server_name,
            backend,
        );
    } else if framing_clean {
        debug!(
            "Backend {} conn not pooled (HEAD, refused upgrade, backend Connection: close, or a Gateway backend)",
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
pub(super) fn backend_conn_poolable(framing_clean: bool, is_head: bool, was_upgrade: bool) -> bool {
    framing_clean && !is_head && !was_upgrade
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
    // The backend is an origin server, sent origin-form only (RFC 9112
    // 3.2.1).
    let origin = OriginForm::of(raw, meta.path_span);
    let path = match origin {
        Some(_) => Some(meta.path(raw).as_bytes()),
        None => meta.normalized_path.as_deref().map(str::as_bytes),
    };
    let (peer_ip, is_tls) = (state.peer.ip(), state.is_tls);
    let forwarded = state
        .forwarded
        .get_or_insert_with(|| ForwardedLines::new(peer_ip, is_tls));
    if meta.strip_spans.needs_slow_path {
        assemble_forward_headers_slow(
            raw,
            path,
            meta.is_upgrade,
            origin,
            forwarded,
            &mut state.scratch_buf,
        );
    } else {
        assemble_forward_headers(
            raw,
            path,
            &meta.strip_spans,
            meta.upgrade_span,
            meta.is_upgrade,
            origin,
            forwarded,
            &mut state.scratch_buf,
        );
    }

    // has_filters already covers every backend's filters (see routing build).
    if rule.has_filters {
        let rule_mods = extract_header_mods(&rule.filters, false);
        let url_rewrite = extract_url_rewrite(&rule.filters, &rule.path, meta.path(raw));
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
pub(super) async fn send_request_with_retry(
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

/// Read response headers from the backend, waiting on `head_wait`'s timer
/// up to its deadline, and forward them to the client. This is the
/// SAFE→UNSAFE transition: on entry no client bytes have been written and
/// timeout → `504`; on success the client has the headers, and any later
/// failure must reset the client connection instead. A `None` wait takes
/// only a head that has already arrived.
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
    head_wait: Option<(Pin<&mut tokio::time::Sleep>, Option<tokio::time::Instant>)>,
    is_head_request: bool,
    expect_upgrade: bool,
    keepalive: bool,
    drain: &DrainWatch,
) -> Result<HeaderPhase> {
    let read = match head_wait {
        Some((timer, deadline)) => {
            within(timer, deadline, read_response_head(backend, header_buf)).await
        }
        None => Some(arrived_head(backend, header_buf).await),
    };
    let read = match read {
        Some(Ok(read)) => read,
        Some(Err(e)) => {
            warn!("Backend response head failed: {}", e);
            send_error_response(client, 502).await?;
            return Ok(HeaderPhase::ErrorSent(502));
        }
        None => {
            send_error_response(client, 504).await?;
            return Ok(HeaderPhase::ErrorSent(504));
        }
    };
    let (resp_header_end, head, cuts) = match read {
        HeadRead::Head(end, head, cuts) => (end, head, cuts),
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
        cuts.as_ref(),
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
