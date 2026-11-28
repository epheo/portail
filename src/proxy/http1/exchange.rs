//! One request and its response through a backend: the head rewritten
//! and sent, replayed once on a pooled connection that died, and the
//! response head sent back.

use std::net::SocketAddr;
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

    /// Whether the whole body came with the head: the relay then reads
    /// nothing more from the client, and the request can be sent again
    /// from the buffer it sits in.
    fn body_held(&self, meta: &RequestMeta) -> bool {
        match (meta.content_length, &self.chunked) {
            (Some(cl), _) => self.read - self.header_end >= cl,
            (None, Some(stream)) => stream.is_terminated(),
            (None, None) => true,
        }
    }
}

/// Whether an attempt that fails before the client has been sent a byte of
/// its answer may be made again, and whether it asks to be.
pub(super) struct Again<'a> {
    /// The response statuses to try again after; `None` on the last
    /// attempt, or the only one.
    codes: Option<&'a [u16]>,
    /// The rule sets `timeouts.backendRequest`, the only budget the API
    /// retries after. Without it each attempt has the 30 s default, and
    /// 101 of them to a backend that never answers would hold the request
    /// most of an hour.
    after_timeouts: bool,
    asked: bool,
}

impl Again<'_> {
    /// After a connect the backend refused or reset.
    pub(super) fn after_refusal(&mut self) -> bool {
        self.asked = self.codes.is_some();
        self.asked
    }

    /// After a connect, or a response head, that took the whole budget.
    pub(super) fn after_timeout(&mut self) -> bool {
        self.asked = self.after_timeouts && self.codes.is_some();
        self.asked
    }

    fn after_status(&mut self, status: u16) -> bool {
        self.asked = self.codes.is_some_and(|codes| codes.contains(&status));
        self.asked
    }
}

/// What an exchange does once, however many attempts it takes.
#[derive(Default)]
struct Once {
    /// The client was told to send its body (`100 Continue`). A client may
    /// send it without waiting, and a small one then arrives whole with
    /// the head: such a request is retried, and must not hear `100
    /// Continue` twice.
    continued: bool,
    /// The request went to the rule's mirrors.
    mirrored: bool,
}

/// The wait before an attempt is made again, `made` being how many
/// already were: the base doubled each time, at most ten times the base,
/// and between half and all of that, so that requests failing together do
/// not come back together.
fn backoff(base: Duration, made: u32) -> Duration {
    let grown = base
        .saturating_mul(1 << made.saturating_sub(1).min(4))
        .min(base.saturating_mul(10));
    grown / 2 + grown.mul_f64(fastrand::f64() / 2.0)
}

/// Proxy a single HTTP request to a backend and forward the response.
/// Returns `(keep, leftover, backend)`: `keep` when the response went out
/// whole and its head announced keep-alive, which the client may already
/// act on; `leftover` holds any bytes read past this request's body (a
/// pipelined next request), which the keepalive loop replays instead of
/// reading from the socket again; `backend` is the one last asked.
///
/// Owns all timing for the request lifecycle. Two timeouts come in from the rule:
///
/// - `backend_request_timeout` — single shared budget per attempt covering
///   pool acquire, TCP/TLS connect, request send, and response
///   time-to-first-byte. On expiry we are still in the SAFE zone (no client
///   bytes written), so a `504` is well-formed.
/// - `request_timeout` — total request lifetime, every attempt included,
///   applied as an absolute `Instant` deadline. Clamps the SAFE-zone phases
///   too, then bounds the body stream. On body-phase expiry we are in the
///   UNSAFE zone (response headers already on the wire); we reset the client
///   connection, since a `504` would corrupt framing and a synthetic
///   terminator would fake a complete body.
///
/// WebSocket upgrades skip the total deadline (long-lived stream).
///
/// Under the rule's `retry`, an attempt that fails before the client has
/// been sent a byte of its answer is made again: after a failed connect, a
/// listed status, or no answer within the backend's budget. Only while the
/// whole request is in hand, so never once part of the body is still the
/// client's to send, and never for an upgrade. Each attempt chooses its
/// backend again.
///
/// One function and one loop, not a loop over an attempt function: an
/// attempt's future is some 4 KiB, and awaiting one moves it, which cost
/// every request 300 instructions whether it was retried or not.
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
) -> Result<(bool, Option<Vec<u8>>, SocketAddr)> {
    use std::time::Instant;

    let total_deadline = if meta.is_upgrade {
        None
    } else {
        rule.request_timeout
            .filter(|d| !d.is_zero())
            .map(|d| Instant::now() + d)
    };
    let retry = rule
        .retry
        .as_ref()
        .filter(|_| !meta.is_upgrade && framing.body_held(meta));
    let mut left = retry.map_or(0, |retry| retry.attempts);
    let (mut backend_ref, mut pending, mut made) = (backend_ref, Some(framing), 0);
    let mut once = Once::default();
    // An attempt asked to be made again leaves by `continue 'attempts`,
    // nothing of an answer written and `buf` holding the request whole;
    // every other way out returns.
    'attempts: loop {
        if let (Some(retry), true) = (retry, made > 0) {
            left -= 1;
            METRICS.http_retries_total.inc();
            let wait = retry.backoff.map(|base| backoff(base, made));
            // No room left for another attempt, at once or after its wait:
            // the request timeout answers now. An attempt started past it
            // would time out before it connects, and count against its
            // backend.
            let room = wait.unwrap_or_default();
            if total_deadline.is_some_and(|deadline| Instant::now() + room >= deadline) {
                state.refused(504);
                send_error_response(client, 504, &state.cors_lines).await?;
                return Ok((false, None, backend_ref.socket_addr));
            }
            if let Some(wait) = wait {
                tokio::time::sleep(wait).await;
            }
            let chosen = state
                .selector
                .select_healthy_weighted_backend(rule, &state.health)
                .map(|index| &rule.backends[index]);
            backend_ref = match chosen.map(|backend| (backend, backend.action)) {
                Some((backend, BackendAction::Forward | BackendAction::ForwardProxied)) => backend,
                // A share that answers for itself ends the attempts with
                // its answer, as it would have answered a first one.
                refused => {
                    let code = match refused {
                        Some((_, BackendAction::Reject)) => 500,
                        _ => 503,
                    };
                    state.refused(code);
                    send_error_response(client, code, &state.cors_lines).await?;
                    return Ok((false, None, backend_ref.socket_addr));
                }
            };
        }
        made += 1;
        // Framed again from the buffer the request still sits in whole.
        let Some(framing) = pending.take().or_else(|| Framing::of(buf, meta)) else {
            return Ok((false, None, backend_ref.socket_addr));
        };
        let again = &mut Again {
            codes: retry.filter(|_| left > 0).map(|retry| &*retry.codes),
            after_timeouts: rule.backend_request_timeout.is_some_and(|d| !d.is_zero()),
            asked: false,
        };

        let backend_addr = backend_ref.socket_addr;
        let backend_budget = rule
            .backend_request_timeout
            .filter(|d| !d.is_zero())
            .unwrap_or(Duration::from_secs(30));
        let backend_phase_start = Instant::now();
        let backend_deadline = deadline_after(backend_phase_start, backend_budget, total_deadline);
        // Cleared per request so an Err exit can never surface the previous
        // keepalive response's status to the access log.
        state.last_status = 0;

        // The whole body came with the head, so the relay below reads nothing
        // into `buf`: a replay can still send it from there.
        let body_held = framing.body_held(meta);
        let Framing {
            read: request_bytes,
            header_end: req_header_end,
            end: request_end,
            chunked: req_chunked,
        } = framing;
        // Capture the held tail now: the response phase reuses `buf` as scratch
        // for backend reads and would overwrite it.
        let mut leftover: Option<Vec<u8>> = if request_end < request_bytes {
            Some(buf[request_end..request_bytes].to_vec())
        } else {
            None
        };
        // Settled again once the relay below is through.
        state.client_done = client_done(ClientSent {
            keepalive: meta.keepalive,
            request_in: body_held && !meta.is_upgrade,
            seen_past: leftover.is_some(),
        });

        // Connect (SAFE: no client bytes yet, helper sends 502/504 on failure).
        let connect = connect_to_backend(
            client,
            state,
            backend_ref,
            timer.as_mut(),
            backend_deadline,
            again,
        );
        let (backend, from_pool) = match connect.await? {
            Some(b) => b,
            None if again.asked => continue 'attempts,
            None => return Ok((false, None, backend_addr)),
        };

        // Copied once, whatever the attempts: a mirror is of the request, not
        // of each try at it.
        let has_mirrors = !once.mirrored
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
            again,
        )
        .await?
        {
            Some(b) => b,
            None if again.asked => continue 'attempts,
            None => return Ok((false, None, backend_addr)),
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
        if meta.expect_continue && !std::mem::replace(&mut once.continued, true) {
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
                        send_error_response(client, 408, &state.cors_lines).await?;
                        return Ok((false, None, backend_addr));
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
                let _ = send_error_response(client, 400, &state.cors_lines).await;
                return Ok((false, None, backend_addr));
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
            once.mirrored = true;
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
                &state.cors_lines,
                again,
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
                    return Ok((false, None, backend_addr));
                }
                // `backend` goes with its answer unread: never pooled.
                HeaderPhase::Again => continue 'attempts,
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
                        again,
                    )
                    .await?
                    {
                        Some(b) => b,
                        None if again.asked => continue 'attempts,
                        None => return Ok((false, None, backend_addr)),
                    };
                }
                HeaderPhase::Unanswered => {
                    if answered_early {
                        warn!("Backend {} stopped taking the request body", backend_addr);
                    } else {
                        warn!("Backend {} closed without answering", backend_addr);
                    }
                    state.refused(502);
                    send_error_response(client, 502, &state.cors_lines).await?;
                    return Ok((false, None, backend_addr));
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
                    return Ok((false, None, backend_addr));
                }
            }
            client.flush().await?;
            relay::both_ways(&mut *client, &mut backend, state.idle_body_timeout).await;
            return Ok((false, None, backend_addr));
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
                        return Ok((false, None, backend_addr));
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
                return Ok((false, None, backend_addr));
            }
            Ok(BodyEnd::Broken) => {
                warn!(
                    "Backend {} sent malformed chunked framing; resetting client connection",
                    backend_addr
                );
                reset_on_drop(client, state);
                return Ok((false, None, backend_addr));
            }
            Ok(BodyEnd::Truncated) => {
                debug!("Backend {} closed mid response body", backend_addr);
                reset_on_drop(client, state);
                return Ok((false, None, backend_addr));
            }
            Err(e) => {
                debug!("Response body from backend {} failed: {}", backend_addr, e);
                reset_on_drop(client, state);
                return Ok((false, None, backend_addr));
            }
        }

        client.flush().await?;
        if answered_early {
            // Never pooled: its request was cut short.
            state.close_after_reply();
            return Ok((false, None, backend_addr));
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
            state
                .pool
                .release(backend_addr, backend_ref.proto.as_ref(), backend);
        } else if framing_clean {
            debug!(
                "Backend {} conn not pooled (HEAD, refused upgrade, backend Connection: close, or a Gateway backend)",
                backend_addr
            );
        }

        if !keep {
            state.close_after_reply();
        }
        return Ok((keep, leftover, backend_addr));
    }
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
    again: &mut Again<'_>,
) -> Result<Option<(Connection, bool)>> {
    let backend_addr = backend_ref.socket_addr;
    match write_with_tail(&mut backend, &mut state.scratch_buf, headers_len, body).await {
        Ok(()) => return Ok(Some((backend, from_pool))),
        Err(e) if from_pool => {
            // Pool entry died between probe and write. Reconnect once and
            // replay. Do NOT record_failure here: a stale pool entry is
            // expected churn, not a backend problem.
            METRICS.pool_stale_retries_total.inc();
            debug!(
                "Backend {} pool conn died on send ({}); retrying with fresh conn",
                backend_addr, e
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
        again,
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
    again: &mut Again<'_>,
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
            backend_ref.proto.as_ref(),
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
            fail_connect(state, backend_ref, client, 502, again).await?;
            return Ok(None);
        }
        Err(_) => {
            warn!(
                "Backend {} fresh-conn acquire timeout after pool retry",
                backend_addr
            );
            fail_connect(state, backend_ref, client, 504, again).await?;
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
    /// Nothing was written to the client, and the attempt asks to be made
    /// again (see [`Again`]).
    Again,
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
    cors: &[u8],
    again: &mut Again<'_>,
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
            send_error_response(client, 502, cors).await?;
            return Ok(HeaderPhase::ErrorSent(502));
        }
        None if again.after_timeout() => return Ok(HeaderPhase::Again),
        None => {
            send_error_response(client, 504, cors).await?;
            return Ok(HeaderPhase::ErrorSent(504));
        }
    };
    let (resp_header_end, head, cuts) = match read {
        HeadRead::Head(end, head, cuts) => (end, head, cuts),
        HeadRead::Malformed => {
            warn!("Backend sent a response head with ambiguous framing; rejecting");
            send_error_response(client, 502, cors).await?;
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

    // Before any client byte, and before the scratch holding the request's
    // outgoing head is put to another use.
    if again.after_status(framing.status) {
        return Ok(HeaderPhase::Again);
    }

    // A 101 nobody asked for would turn the client connection into an
    // unframed tunnel; refuse it while the client wire is still clean.
    if framing.status == 101 && !expect_upgrade {
        warn!("Backend sent 101 without an upgrade request; rejecting");
        send_error_response(client, 502, cors).await?;
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
    // Last: the rule's CORS filter has the final word on these headers,
    // over the backend and over a header modifier.
    if !cors.is_empty() {
        crate::proxy::cors::set_on(headers, cors);
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::http1::CLIENT_BUF_INITIAL;
    use crate::proxy::http_parser::find_header_end;
    use crate::proxy::testkit::{
        backend, closing_backend, conn_state, delivered, exchange, l4_backend, read_one_reply,
        read_until_closed, refusing_backend, response, routes, serve, serve_state, tcp_client,
        GET_A, IDLE, OK, REFUSED, UPGRADE,
    };
    use crate::routing::{Backend, HttpHeader, PathMatchType, RouteTable, URLRewritePath};
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;
    use tokio::net::{TcpListener, TcpStream};
    use tokio_util::sync::CancellationToken;

    const SWITCHED: &[u8] =
        b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n";

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
                    proto: crate::proxy::backend_pool::BackendProto::Plain,
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
                        proto: crate::proxy::backend_pool::BackendProto::Plain,
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

    /// A masked WebSocket text frame ("hello"), as a client streams it.
    const WS_FRAME: &[u8] = b"\x81\x85\x37\xfa\x21\x3d\x7f\x9f\x4d\x51\x58";

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
                        proto: crate::proxy::backend_pool::BackendProto::Tls((*sni).into()),
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
                    proto: crate::proxy::backend_pool::BackendProto::Plain,
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
                proto: crate::proxy::backend_pool::BackendProto::Plain,
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
                &mut Again {
                    codes: None,
                    after_timeouts: false,
                    asked: false,
                },
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

    /// A backend that gives its `n`th request the `n`th of `answers`, the
    /// last one from then on, and never answers for `None`. Returns its
    /// address and the count of requests it has read.
    async fn scripted(answers: Vec<Option<&'static str>>) -> (SocketAddr, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(AtomicUsize::new(0));
        let counted = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let (answers, counted) = (answers.clone(), counted.clone());
                tokio::spawn(async move {
                    let (mut read, mut chunk) = (Vec::new(), [0u8; 4096]);
                    while let Ok(n @ 1..) = sock.read(&mut chunk).await {
                        read.extend_from_slice(&chunk[..n]);
                        while let Some(end) = find_header_end(&read) {
                            read.drain(..end);
                            let nth = counted.fetch_add(1, Ordering::SeqCst);
                            match answers[nth.min(answers.len() - 1)] {
                                Some(answer) => sock.write_all(answer.as_bytes()).await.unwrap(),
                                None => std::future::pending().await,
                            }
                        }
                    }
                });
            }
        });
        (addr, seen)
    }

    const FAILED: &str = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 2\r\n\r\nno";
    const BUSY: &str = "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 2\r\n\r\nno";

    /// a.test under `rule`, built as a snapshot or a file gives it.
    fn retrying(rule: serde_json::Value) -> RouteTable {
        let config: crate::config::PortailConfig = serde_json::from_value(serde_json::json!({
            "gateway": {"name": "g", "listeners": [{"name": "http", "protocol": "HTTP", "port": 8080}]},
            "httpRoutes": [{"parentRefs": [{"name": "g", "sectionName": "http"}],
                            "hostnames": ["a.test"], "rules": [rule]}]
        }))
        .unwrap();
        config.validate().unwrap();
        config.to_route_table_strict().unwrap()
    }

    fn to(backend: SocketAddr) -> serde_json::Value {
        serde_json::json!({"name": "127.0.0.1", "port": backend.port()})
    }

    /// A listed status is tried again while attempts remain, and the last
    /// attempt's answer is the client's, whatever it is. A status that is
    /// not listed is the client's at once.
    #[tokio::test]
    async fn a_listed_status_is_tried_again_while_attempts_remain() {
        for (attempts, answers, status, asked) in [
            (3, vec![FAILED, FAILED, OK], "200 OK", 3),
            (1, vec![FAILED, FAILED, OK], "500 Internal Server Error", 2),
            (0, vec![FAILED, OK], "500 Internal Server Error", 1),
            (3, vec![BUSY, OK], "503 Service Unavailable", 1),
        ] {
            let (backend, seen) = scripted(answers.into_iter().map(Some).collect()).await;
            let rt = retrying(serde_json::json!({
                "retry": {"codes": [500], "attempts": attempts}, "backendRefs": [to(backend)]
            }));
            let (mut client, _engine) = serve(rt);
            let answer = exchange(&mut client, GET_A).await;
            assert!(
                answer.starts_with(&format!("HTTP/1.1 {status}")),
                "{attempts}: {answer}"
            );
            assert_eq!(seen.load(Ordering::SeqCst), asked, "attempts {attempts}");
        }
    }

    /// With no status listed, a retry is for the connect that fails: the
    /// request goes to the rule's other backend, and without the stanza it
    /// is a 502.
    #[tokio::test]
    async fn a_failed_connect_is_tried_on_a_backend_chosen_again() {
        for (retry, status) in [
            (serde_json::json!({"attempts": 1}), "200 OK"),
            (serde_json::json!({"attempts": 0}), "502 Bad Gateway"),
        ] {
            let (_held, refusing) = refusing_backend();
            let (good, seen) = scripted(vec![Some(OK)]).await;
            let rt = retrying(serde_json::json!({
                "retry": retry, "backendRefs": [to(refusing), to(good)]
            }));
            let (mut client, _engine) = serve(rt);
            let answer = exchange(&mut client, GET_A).await;
            assert!(
                answer.starts_with(&format!("HTTP/1.1 {status}")),
                "{retry}: {answer}"
            );
            assert_eq!(seen.load(Ordering::SeqCst), usize::from(status == "200 OK"));
        }
    }

    /// Once part of the body is still the client's to send there is nothing
    /// whole to send again: the first answer is the client's.
    #[tokio::test]
    async fn a_request_still_arriving_is_not_tried_again() {
        let (backend, seen) = scripted(vec![Some(FAILED), Some(OK)]).await;
        let rt = retrying(serde_json::json!({
            "retry": {"codes": [500], "attempts": 3}, "backendRefs": [to(backend)]
        }));
        let (mut client, _engine) = serve(rt);
        client
            .write_all(b"POST / HTTP/1.1\r\nHost: a.test\r\nContent-Length: 8\r\n\r\nhalf")
            .await
            .unwrap();
        // The rest only once the backend has the head: the request was
        // sent on with half its body.
        while seen.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        client.write_all(b"half").await.unwrap();
        let answer = response(&mut client).await;
        assert!(answer.starts_with("HTTP/1.1 500 "), "{answer}");
        assert_eq!(seen.load(Ordering::SeqCst), 1);

        // The same request whole is tried again, its body sent each time.
        let (backend, seen) = scripted(vec![Some(FAILED), Some(OK)]).await;
        let rt = retrying(serde_json::json!({
            "retry": {"codes": [500], "attempts": 3}, "backendRefs": [to(backend)]
        }));
        let (mut client, _engine) = serve(rt);
        let whole = b"POST / HTTP/1.1\r\nHost: a.test\r\nContent-Length: 8\r\n\r\nhalfhalf";
        let answer = exchange(&mut client, whole).await;
        assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
        assert_eq!(seen.load(Ordering::SeqCst), 2);
    }

    /// The rule's request timeout is for all the attempts together: one
    /// that hangs after a failed one is cut by it, not given a budget of
    /// its own.
    #[tokio::test]
    async fn the_request_timeout_bounds_the_attempts_together() {
        let (backend, seen) = scripted(vec![Some(FAILED), None]).await;
        let rt = retrying(serde_json::json!({
            "retry": {"codes": [500], "attempts": 50},
            "timeouts": {"request": "300ms"},
            "backendRefs": [to(backend)]
        }));
        let state = conn_state(rt, false);
        let health = state.health.clone();
        let (mut client, _engine) = serve_state(state);
        let answer = exchange(&mut client, GET_A).await;
        assert!(answer.starts_with("HTTP/1.1 504 "), "{answer}");
        assert_eq!(seen.load(Ordering::SeqCst), 2);
        // No attempt was started past the timeout: each would have timed
        // out before connecting, and counted against the backend.
        assert!(health.is_healthy(&backend));
    }

    /// A wait the request timeout has no room for is not started: the
    /// timeout answers at once. Waited out, this test's own five seconds
    /// would pass first.
    #[tokio::test]
    async fn a_wait_past_the_request_timeout_answers_504_at_once() {
        let (backend, seen) = scripted(vec![Some(FAILED), Some(OK)]).await;
        let rt = retrying(serde_json::json!({
            "retry": {"codes": [500], "attempts": 2, "backoff": "20s"},
            "timeouts": {"request": "8s"},
            "backendRefs": [to(backend)]
        }));
        let (mut client, _engine) = serve(rt);
        let answer = exchange(&mut client, GET_A).await;
        assert!(answer.starts_with("HTTP/1.1 504 "), "{answer}");
        assert_eq!(seen.load(Ordering::SeqCst), 1);
    }

    /// A mirror is of the request, not of each attempt at it: one copy,
    /// and one still when the first attempt never connected.
    #[tokio::test]
    async fn a_retried_request_is_mirrored_once() {
        let (_held, refusing) = refusing_backend();
        let (backend, seen) = scripted(vec![Some(FAILED), Some(FAILED), Some(OK)]).await;
        let (mirror, copies) = scripted(vec![Some(OK)]).await;
        let rt = retrying(serde_json::json!({
            "retry": {"codes": [500], "attempts": 5},
            "filters": [{"type": "RequestMirror", "requestMirror": {"backendRef": to(mirror)}}],
            "backendRefs": [to(refusing), to(backend)]
        }));
        let (mut client, _engine) = serve(rt);
        let answer = exchange(&mut client, GET_A).await;
        assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
        assert_eq!(seen.load(Ordering::SeqCst), 3);
        // The copy is sent from a task of its own: wait for it, then for
        // the engine to be through, and count.
        while copies.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        _engine.await.unwrap().unwrap();
        assert_eq!(copies.load(Ordering::SeqCst), 1);
    }

    /// Doubled from the base, at most ten times it, and never under half
    /// of what it grew to.
    #[test]
    fn the_wait_between_attempts_grows_from_the_base_to_its_cap() {
        let base = Duration::from_millis(100);
        for (made, grown) in [
            (1, 100),
            (2, 200),
            (3, 400),
            (4, 800),
            (5, 1000),
            (40, 1000),
        ] {
            let grown = Duration::from_millis(grown);
            for _ in 0..200 {
                let wait = backoff(base, made);
                assert!(grown / 2 <= wait && wait <= grown, "{made}: {wait:?}");
            }
        }
    }

    /// A client may send its body with `Expect: 100-continue` without
    /// waiting, and a small one arrives whole with the head: retried, it
    /// hears `100 Continue` once.
    #[tokio::test]
    async fn a_retried_request_that_expects_100_continue_hears_it_once() {
        let (backend, seen) = scripted(vec![Some(FAILED), Some(OK)]).await;
        let rt = retrying(serde_json::json!({
            "retry": {"codes": [500], "attempts": 1}, "backendRefs": [to(backend)]
        }));
        let (mut client, _engine) = serve(rt);
        let request = b"POST / HTTP/1.1\r\nHost: a.test\r\nExpect: 100-continue\r\n\
                        Content-Length: 4\r\n\r\nbody";
        let answer = exchange(&mut client, request).await;
        assert_eq!(answer.matches("100 Continue").count(), 1, "{answer}");
        assert!(answer.contains("HTTP/1.1 200 OK"), "{answer}");
        assert_eq!(seen.load(Ordering::SeqCst), 2);
    }

    /// An attempt that takes the whole budget is tried again under the
    /// rule's own `backendRequest`.
    #[tokio::test]
    async fn a_backend_timeout_the_rule_sets_is_tried_again() {
        let (backend, seen) = scripted(vec![None, Some(OK)]).await;
        let rt = retrying(serde_json::json!({
            "retry": {"attempts": 1},
            "timeouts": {"backendRequest": "100ms"},
            "backendRefs": [to(backend)]
        }));
        let (mut client, _engine) = serve(rt);
        let answer = exchange(&mut client, GET_A).await;
        assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
        assert_eq!(seen.load(Ordering::SeqCst), 2);
    }

    /// Without it the budget is the 30 s default, and a backend that
    /// never answers is not given it again: one budget, then the 504.
    #[tokio::test(start_paused = true)]
    async fn without_a_backend_timeout_a_silent_backend_is_not_tried_again() {
        use tokio::io::AsyncWriteExt;
        let (backend, seen) = scripted(vec![None]).await;
        let rt = retrying(serde_json::json!({
            "retry": {"codes": [500], "attempts": 3}, "backendRefs": [to(backend)]
        }));
        let (mut client, _engine) = serve(rt);
        let started = tokio::time::Instant::now();
        client.write_all(GET_A).await.unwrap();
        client.shutdown().await.unwrap();
        // No timeout of the test's own: the paused clock runs to the
        // engine's next timer at once, so one budget is 30 s of it.
        let mut answer = Vec::new();
        client.read_to_end(&mut answer).await.unwrap();
        let answer = String::from_utf8_lossy(&answer);
        assert!(answer.starts_with("HTTP/1.1 504 "), "{answer}");
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "{:?}",
            started.elapsed()
        );
    }
}
