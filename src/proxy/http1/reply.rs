//! Replies the proxy writes itself: redirects and errors, as HTTP/1.1
//! bytes. What a reply says (status, Location, close) is the same for any
//! client protocol; only this serialization is HTTP/1's.

use std::borrow::Cow;

use anyhow::Result;
use tokio::io::AsyncWriteExt;

use super::ConnectionState;
use crate::proxy::request_processor::LocalReply;
use crate::proxy::tls::Connection;

/// Answer a request the proxy handles itself (redirect, 404, 429, 500, 503)
/// and access-log it. Returns whether the connection stays open; the reply
/// announces `Connection: close` exactly when it does not, and the close
/// then lingers unless the client is done. Staying open matters most for
/// 429: a throttled client's next request would otherwise cost a fresh TLS
/// handshake, more of our CPU than the refused request.
pub(crate) async fn send_local_reply(
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

/// Error reply after which the connection always closes (400, 408, 431,
/// 502, 504): what remains of the request on the wire is unknown or unread.
pub(crate) async fn send_error_response(client: &mut Connection, error_code: u16) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
