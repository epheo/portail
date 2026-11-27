//! A backend's response head: read off its connection, scanned, parsed
//! and framed. Nothing here knows which protocol the client speaks.

use anyhow::Result;
use tokio::io::AsyncReadExt;

use crate::logging::debug;
use crate::proxy::chunked::ChunkedStream;
use crate::proxy::http_filters::{header_lines, is_hop_by_hop_response_header, ResponseCuts};
use crate::proxy::http_parser::{
    field_value_end, find_header_end, is_field_value_byte, is_tchar, is_token, parse_digits,
    trim_ows,
};
use crate::proxy::tls::Connection;

/// Response framing derived from raw backend headers. Drives the body-phase loop:
/// when to stop reading from the backend, and whether its EOF ends the body or
/// truncates it.
pub(crate) struct ResponseFraming {
    pub(crate) status: u16,
    /// The backend announced it will close this connection (Connection:
    /// close, or an HTTP/1.0 response without keep-alive). Such a conn must
    /// not return to the pool: the next write would fail and burn the retry.
    pub(crate) backend_close: bool,
    /// `Some` iff the body is framed by Content-Length.
    pub(crate) content_length: Option<usize>,
    pub(crate) no_body: bool,
    /// Body bytes (within the declared framing) that arrived in the same
    /// backend read that completed the response headers. The header-forwarding
    /// phase writes exactly this many tail bytes to the client, and it seeds
    /// the body-phase byte counter.
    pub(crate) body_bytes_already_forwarded: usize,
    /// `Some` iff the final transfer coding is chunked. Pre-seeded
    /// with any chunked body bytes that arrived in the same read as the
    /// response headers, so the observer is already at the correct framing
    /// position when `forward_response_body` starts looping.
    pub(crate) chunked: Option<ChunkedStream>,
    /// The backend sent bytes past the end of the declared framing
    /// (Content-Length reached, chunked terminator passed, or a body on a
    /// no-body status). The excess is dropped — never forwarded to the
    /// client — and the connection must not be pooled: whatever follows the
    /// boundary would desync the next response.
    pub(crate) overshoot: bool,
}

impl ResponseFraming {
    /// Only the backend's close ends this body, so the client connection
    /// closes after it.
    pub(crate) fn close_delimited(&self) -> bool {
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
pub(crate) enum HeadRead {
    /// The final (non-interim) head and where it ends: `header_buf` holds
    /// `[headers..header_end][same-read body tail...]`.
    Head(usize, ResponseHead, Option<ResponseCuts>),
    /// A head, interim ones included, a recipient could frame differently
    /// (see `parse_response_head`).
    Malformed,
    /// Closed or reset before a single byte; from [`arrived_head`], also no
    /// complete head yet.
    Unanswered,
}

/// [`read_response_head`] over what the backend has already sent, with no
/// wait for more.
pub(crate) async fn arrived_head(
    backend: &mut Connection,
    header_buf: &mut Vec<u8>,
) -> Result<HeadRead> {
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
pub(crate) async fn read_response_head(
    backend: &mut Connection,
    header_buf: &mut Vec<u8>,
) -> Result<HeadRead> {
    header_buf.clear();
    let mut interim_seen = 0u32;
    let mut answered = false;

    loop {
        // Check before reading: a swallowed interim response leaves the next
        // response's bytes already buffered.
        match scan_response_head(header_buf) {
            HeadScan::Partial => {}
            HeadScan::Malformed => return Ok(HeadRead::Malformed),
            HeadScan::Head(header_end, head, cuts) => {
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
                return Ok(HeadRead::Head(header_end, head, cuts));
            }
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
pub(crate) fn frame_response(
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

/// Whether a response of this status carries no body (RFC 7230 3.3): HEAD
/// requests, 204, 304, and all 1xx. Shared with the h2 bridge so both
/// framing paths agree on which responses end at their headers.
#[inline]
pub(crate) fn response_has_no_body(status: u16, is_head: bool) -> bool {
    is_head || status == 204 || status == 304 || (100..200).contains(&status)
}

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

/// What [`scan_response_head`] found at the front of a buffer.
pub(crate) enum HeadScan {
    /// No blank line ends a head yet.
    Partial,
    /// A whole head a recipient could frame differently (see
    /// [`parse_response_head`]).
    Malformed,
    /// A head and where it ends; with the sanitizer's cuts when its lines
    /// were regular.
    Head(usize, ResponseHead, Option<ResponseCuts>),
}

/// The head at the front of `data`, read exactly as `find_header_end` then
/// [`parse_response_head`] read it. A regular head (every line ended by
/// CRLF, none empty) takes one pass that also records the sanitizer's
/// cuts; anything else goes through those two.
pub(crate) fn scan_response_head(data: &[u8]) -> HeadScan {
    if let Some(scan) = scan_regular_head(data) {
        return scan;
    }
    let Some(end) = find_header_end(data) else {
        return HeadScan::Partial;
    };
    match parse_response_head(&data[..end]) {
        Some(head) => HeadScan::Head(end, head, None),
        None => HeadScan::Malformed,
    }
}

/// How a regular line ends.
enum LineEnd {
    /// At the CR of its CRLF.
    Cr(usize),
    /// Past the end of the data so far.
    Partial,
    /// A bare CR or LF: only the two-pass reading decides such a head.
    Irregular,
}

/// The CR ending the line whose value starts at `pos`. Other CTLs are value
/// bytes to `parse_response_head`, so they are passed over.
#[inline(always)]
fn line_end(data: &[u8], mut pos: usize) -> LineEnd {
    loop {
        pos = field_value_end(data, pos);
        match data.get(pos..pos + 2) {
            Some(b"\r\n") => return LineEnd::Cr(pos),
            Some([b'\r' | b'\n', _]) => return LineEnd::Irregular,
            Some(_) => pos += 1,
            None => return LineEnd::Partial,
        }
    }
}

/// [`scan_response_head`]'s one pass. `None` for any head but a regular,
/// well-formed one, or a regular start of one (`Partial`): no "\r\n\r\n"
/// can hide in a run of CRLF-ended lines but at their blank line.
fn scan_regular_head(data: &[u8]) -> Option<HeadScan> {
    let status_end = match line_end(data, 0) {
        LineEnd::Cr(cr) => cr,
        LineEnd::Partial => return Some(HeadScan::Partial),
        LineEnd::Irregular => return None,
    };
    let (status, is_http10) = parse_status_line(&data[..status_end])?;
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    let (mut close, mut keep_alive) = (false, false);
    let mut cuts = ResponseCuts::default();

    let mut pos = status_end + 2;
    loop {
        match data.get(pos..pos + 2) {
            Some(b"\r\n") => break,
            Some([b'\r' | b'\n', _]) => return None,
            Some(_) => {}
            None => return Some(HeadScan::Partial),
        }
        let start = pos;
        while data.get(pos).is_some_and(|&b| is_tchar(b)) {
            pos += 1;
        }
        match data.get(pos) {
            Some(b':') if pos > start => {}
            None => return Some(HeadScan::Partial),
            Some(_) => return None,
        }
        let colon = pos;
        let cr = match line_end(data, colon + 1) {
            LineEnd::Cr(cr) => cr,
            LineEnd::Partial => return Some(HeadScan::Partial),
            LineEnd::Irregular => return None,
        };
        pos = cr + 2;
        let (name, value) = (&data[start..colon], &data[colon + 1..cr]);
        match (name.len(), name[0] | 0x20) {
            (14, b'c') if name.eq_ignore_ascii_case(b"content-length") => {
                let digits = trim_ows(value);
                let n = parse_digits(digits)?;
                match content_length {
                    None => {
                        let at = digits.as_ptr() as usize - data.as_ptr() as usize;
                        let zeros = digits.iter().take_while(|&&b| b == b'0').count();
                        let from = at + zeros.min(digits.len() - 1);
                        let to = at + digits.len();
                        cuts.length = Some(((start as u32, pos as u32), (from as u32, to as u32)));
                    }
                    Some(seen) if seen != n => return None,
                    Some(_) => cuts.strip.push(start, pos),
                }
                content_length = Some(n);
            }
            (17, b't') if name.eq_ignore_ascii_case(b"transfer-encoding") => {
                if chunked || !trim_ows(value).eq_ignore_ascii_case(b"chunked") {
                    return None;
                }
                chunked = true;
            }
            (10, b'c') if name.eq_ignore_ascii_case(b"connection") => {
                for token in value.split(|&b| b == b',').map(trim_ows) {
                    close |= token.eq_ignore_ascii_case(b"close");
                    keep_alive |= token.eq_ignore_ascii_case(b"keep-alive");
                }
                cuts.strip.push(start, pos);
            }
            _ if is_hop_by_hop_response_header(name) => cuts.strip.push(start, pos),
            _ => {}
        }
    }

    if chunked && is_http10 {
        return None;
    }
    let body = match (chunked, content_length) {
        (true, _) => BodyFraming::Chunked,
        (false, Some(n)) => BodyFraming::Length(n),
        (false, None) => BodyFraming::Close,
    };
    let head = ResponseHead {
        status,
        body,
        backend_close: close || (is_http10 && !keep_alive),
    };
    let cuts = (!cuts.strip.needs_slow_path).then_some(cuts);
    Some(HeadScan::Head(pos + 2, head, cuts))
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
    use crate::proxy::http_filters::sanitize_response_headers;

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

    /// The two-pass reading `scan_response_head` must equal: `Partial` is
    /// `None`, `Malformed` is `Some((end, None))`.
    fn two_pass(data: &[u8]) -> Option<(usize, Option<ResponseHead>)> {
        let end = find_header_end(data)?;
        Some((end, parse_response_head(&data[..end])))
    }

    fn sanitized(head: &[u8], parsed: &ResponseHead, cuts: Option<&ResponseCuts>) -> Vec<u8> {
        let content_length = match parsed.body {
            BodyFraming::Length(n) => Some(n),
            BodyFraming::Chunked | BodyFraming::Close => None,
        };
        let mut out = Vec::new();
        for close in [false, true] {
            sanitize_response_headers(head, content_length, cuts, close, &mut out);
            out.push(b'|');
        }
        out
    }

    /// Every prefix of random heads, regular or not, reads the same through
    /// the one pass as through the two, and its cuts sanitize to the same
    /// bytes as the line-by-line copy.
    #[test]
    fn one_pass_head_scan_reads_as_the_two_passes() {
        let status: &[&[u8]] = &[
            b"HTTP/1.1 200 OK\r\n",
            b"HTTP/1.0 200 OK\r\n",
            b"HTTP/1.1 204\r\n",
            b"HTTP/1.1 100 Continue\r\n",
            b"HTTP/1.1 200 O\x00K\r\n",
            b"HTTP/1.1 200 OK\n",
            b"HTTP/2 200\r\n",
            b"\r\n",
            b"\n",
        ];
        let pieces: &[&[u8]] = &[
            b"Content-Length: 5\r\n",
            b"content-length:  005 \r\n",
            b"Content-Length: 0\r\n",
            b"Content-Length: 000\r\n",
            b"Content-Length: 6\r\n",
            b"Content-Length: 5, 5\r\n",
            b"Transfer-Encoding: chunked\r\n",
            b"transfer-encoding: \tChunked\r\n",
            b"Transfer-Encoding: gzip\r\n",
            b"Connection: close\r\n",
            b"connection: keep-alive, Upgrade\r\n",
            b"Keep-Alive: timeout=5\r\n",
            b"Proxy-Connection: x\r\n",
            b"TE: trailers\r\n",
            b"Trailer: X\r\n",
            b"Upgrade: h2c\r\n",
            b"Server: x\r\n",
            b"X-A: \x00\x7f\xff\tv \r\n",
            b"X-B:\r\n",
            b"X(A): 1\r\n",
            b": x\r\n",
            b"X-A 1\r\n",
            b" folded\r\n",
            b"X-A: 1\n",
            b"X-A: 1\r\r\n",
            b"X-A: 1\rX\r\n",
            b"\r\n",
            b"\n",
            b"\r",
            b"ok",
        ];
        let mut rng = fastrand::Rng::with_seed(9112);
        let (mut regular, mut irregular) = (0, 0);
        for _ in 0..20_000 {
            let mut data = status[rng.usize(..status.len())].to_vec();
            for _ in 0..rng.usize(0..12) {
                data.extend_from_slice(pieces[rng.usize(..pieces.len())]);
            }
            if rng.bool() {
                data.extend_from_slice(b"\r\n");
            }
            for len in 0..=data.len() {
                let data = &data[..len];
                let want = two_pass(data);
                match scan_response_head(data) {
                    HeadScan::Partial => assert_eq!(want, None, "{data:?}"),
                    HeadScan::Malformed => assert!(matches!(want, Some((_, None))), "{data:?}"),
                    HeadScan::Head(end, head, cuts) => {
                        assert_eq!(want, Some((end, Some(head))), "{data:?}");
                        if cuts.is_some() {
                            regular += 1;
                        } else {
                            irregular += 1;
                        }
                        assert_eq!(
                            sanitized(&data[..end], &head, cuts.as_ref()),
                            sanitized(&data[..end], &head, None),
                            "{data:?}"
                        );
                    }
                }
            }
        }
        // Both paths ran.
        assert!(regular > 1000 && irregular > 1000, "{regular} {irregular}");
    }

    #[test]
    fn a_regular_head_is_cut_in_one_pass() {
        let wire = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\ncontent-length: 02\r\nConnection: keep-alive\r\nETag: x\r\n\r\nok";
        let HeadScan::Head(end, head, Some(cuts)) = scan_response_head(wire) else {
            panic!("not read as a regular head");
        };
        assert_eq!(end, wire.len() - 2);
        assert_eq!(head.body, BodyFraming::Length(2));
        let mut out = Vec::new();
        sanitize_response_headers(&wire[..end], Some(2), Some(&cuts), true, &mut out);
        assert_eq!(
            std::str::from_utf8(&out).unwrap(),
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nETag: x\r\nConnection: close\r\n\r\n"
        );
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
