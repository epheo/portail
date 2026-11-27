//! Bodies, both ways, between an HTTP/1.1 client and a backend.
//!
//! The native h2 path will lift a front-neutral relay out of this file.
//! Tests that drive [`super::serve`] stay with this front then: they pin
//! HTTP/1 framing as much as the backend's behaviour.

use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use bytes::BufMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::{ConnectionState, CLIENT_BUF};
use crate::logging::warn;
use crate::proxy::chunked::ChunkedStream;
use crate::proxy::deadline::idle_guarded;
use crate::proxy::http_filters::MIRROR_BODY_MAX;
use crate::proxy::tls::Connection;
use crate::proxy::upstream::response::ResponseFraming;

/// Read up to `CLIENT_BUF` of a body into `buf`, emptied first: every body
/// read gets the buffer's full size, however small the head was.
async fn read_body_chunk<R>(conn: &mut R, buf: &mut Vec<u8>) -> std::io::Result<usize>
where
    R: tokio::io::AsyncRead + Unpin,
{
    buf.clear();
    buf.reserve_exact(CLIENT_BUF);
    conn.read_buf(&mut (&mut *buf).limit(CLIENT_BUF)).await
}

/// How the request-body relay ended. Either incomplete outcome leaves a
/// partial request on the backend conn, which must then be dropped: pooled,
/// the next request's bytes would complete this body.
pub(super) enum RequestBody {
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
pub(super) async fn relay_request_body(
    client: &mut Connection,
    backend: &mut Connection,
    buf: &mut Vec<u8>,
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
            let n = idle_guarded(idle, read_body_chunk(client, buf)).await?;
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
            let n = idle_guarded(idle, read_body_chunk(client, buf)).await?;
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

/// How the response body ended.
pub(super) enum BodyEnd {
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
pub(super) async fn forward_response_body(
    backend: &mut Connection,
    client: &mut Connection,
    buf: &mut Vec<u8>,
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
        let n = idle_guarded(idle, read_body_chunk(backend, buf)).await?;
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
pub(super) fn reset_on_drop(client: &Connection, state: &mut ConnectionState) {
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
