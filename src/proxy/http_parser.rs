//! Zero-copy HTTP request parsing for high-performance request processing.
//! All parsing functions use direct buffer references for maximum efficiency.

use crate::logging::debug;
use anyhow::{anyhow, Result};

/// Find the end of HTTP headers (\r\n\r\n boundary).
/// Returns the position just past the double CRLF.
/// Uses memchr SIMD-accelerated search for \r, then checks the following 3 bytes.
pub(crate) fn find_header_end(data: &[u8]) -> Option<usize> {
    let mut start = 0;
    while let Some(pos) = memchr::memchr(b'\r', &data[start..]) {
        let abs = start + pos;
        if abs + 3 < data.len()
            && data[abs + 1] == b'\n'
            && data[abs + 2] == b'\r'
            && data[abs + 3] == b'\n'
        {
            return Some(abs + 4);
        }
        start = abs + 1;
    }
    None
}

pub const MAX_STRIP_SPANS: usize = 8;

/// Byte spans (start..end, CRLF inclusive) of hop-by-hop header lines,
/// recorded during the parser's existing single pass so forwarding can skip
/// them by bulk gap-copy — no second scan, no allocation. Overflow or
/// Connection-nominated extra headers divert to the thorough slow path,
/// which strips by `is_hop_by_hop_request_header`: a name stripped here but
/// missing there reaches the backend whenever a client forces that path.
#[derive(Debug, Clone, Copy, Default)]
pub struct StripSpans {
    spans: [(u32, u32); MAX_STRIP_SPANS],
    len: u8,
    pub needs_slow_path: bool,
}

impl StripSpans {
    #[inline]
    fn push(&mut self, start: usize, end: usize) {
        if (self.len as usize) < MAX_STRIP_SPANS {
            self.spans[self.len as usize] = (start as u32, end as u32);
            self.len += 1;
        } else {
            self.needs_slow_path = true;
        }
    }

    /// Spans in ascending byte order (parse order).
    pub fn iter(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.spans[..self.len as usize]
            .iter()
            .map(|&(s, e)| (s as usize, e as usize))
    }
}

/// HTTP request information extracted by unified parser (zero-copy)
/// Contains all fields needed for routing decisions and execution
#[derive(Debug, Clone)]
pub struct HttpRequestInfo<'a> {
    pub method: &'a str,
    /// Port stripped; an IP literal keeps its brackets (`[::1]`).
    pub host: &'a str,
    pub path: &'a str,
    pub query_string: &'a str,
    /// Version-resolved: an explicit Connection token wins, else HTTP/1.1
    /// defaults to keep-alive and HTTP/1.0 to close.
    pub keepalive: bool,
    /// Raw header bytes for lazy header matching (zero-copy)
    pub header_data: &'a [u8],
    /// Content-Length of the request body (None if header absent)
    pub content_length: Option<usize>,
    /// Whether Transfer-Encoding: chunked is present
    pub is_chunked: bool,
    /// Whether this is a WebSocket/protocol upgrade request
    pub is_upgrade: bool,
    /// Set when a lenient backend could read this head differently from this
    /// parser. Header bytes are forwarded verbatim, so any such gap is a
    /// desync: request smuggling, or routing one vhost while the backend
    /// serves another. Covers:
    /// - framing: TE with CL, a repeated or non-`1*DIGIT` CL, any TE but one
    ///   exact `chunked`, TE on HTTP/1.0, Connection nominating CL, TE or
    ///   Host (forwarding strips what Connection nominates);
    /// - syntax: a line not ended by CRLF, obs-fold, a field-name with a
    ///   non-token byte (`Content-Length : 5`), a CTL in a value, no blank
    ///   line ending the head;
    /// - Host: repeated, empty on HTTP/1.1, or not `host [":" port]` with
    ///   host a DNS-alphabet name or a bracketed IPv6 address;
    /// - target: `*` without OPTIONS, CONNECT without authority-form, any
    ///   other non-origin target that is not `http(s)://` absolute-form,
    ///   userinfo, or an authority naming another host than Host.
    ///
    /// The caller MUST reject such requests with `400` and close.
    pub header_violation: bool,
    /// Header lines to strip when forwarding: hop-by-hop ones (RFC 7230
    /// 6.1) and the client's Forwarded / X-Forwarded-* claims, bar
    /// X-Forwarded-For.
    pub strip_spans: StripSpans,
    /// Span of the Upgrade header line; kept only for real upgrade requests.
    pub upgrade_span: Option<(u32, u32)>,
    /// Request carried `Expect: 100-continue`.
    pub expect_continue: bool,
}

const TCHAR: u8 = 1;
const VALUE: u8 = 1 << 1;
const TARGET: u8 = 1 << 2;
const HOST: u8 = 1 << 3;

/// Bytes at or above a class's `scan` floor that the class still excludes.
/// The class is built from them, so the word test cannot miss one.
const VALUE_HOLES: [u8; 1] = [0x7f];
/// Routing reads `/admin#x` whole while a backend dropping the fragment, or
/// reading '\' as '/', serves another path: a rule on that path is bypassed.
const TARGET_HOLES: [u8; 3] = [0x7f, b'#', b'\\'];

/// Byte classes of the request-head grammar (RFC 9110, RFC 9112, RFC 3986).
/// HOST is the part of reg-name DNS names use. Sub-delims and '%' are out:
/// `evil.com,a.example.com` suffix-matches `*.example.com` here, while a
/// framework splitting Host on ',' or percent-decoding it reads evil.com.
/// The structural `:`, `[` and `]` are left to `split_host`.
static CLASS: [u8; 256] = {
    const fn any_of(b: u8, set: &[u8]) -> bool {
        let mut i = 0;
        while i < set.len() {
            if set[i] == b {
                return true;
            }
            i += 1;
        }
        false
    }
    let mut table = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let b = i as u8;
        if b.is_ascii_alphanumeric() || any_of(b, b"!#$%&'*+-.^_`|~") {
            table[i] |= TCHAR;
        }
        if b == b'\t' || (b >= 0x20 && !any_of(b, &VALUE_HOLES)) {
            table[i] |= VALUE;
        }
        if b > 0x20 && !any_of(b, &TARGET_HOLES) {
            table[i] |= TARGET;
        }
        if b.is_ascii_alphanumeric() || any_of(b, b"-._") {
            table[i] |= HOST;
        }
        i += 1;
    }
    table
};

#[inline(always)]
fn is(b: u8, class: u8) -> bool {
    CLASS[b as usize] & class != 0
}

const ONES: u64 = u64::from_ne_bytes([0x01; 8]);
const HIGHS: u64 = u64::from_ne_bytes([0x80; 8]);

/// End of the run of `class` bytes starting at `pos`. Every byte at or above
/// `floor` except `holes` must belong to `class`, so whole words of such bytes
/// skip the per-byte lookup; long values and targets cost a fraction of a
/// lookup per byte. `floor` must not exceed 0x80 for the word test to be exact.
#[inline(always)]
fn scan<const N: usize>(
    data: &[u8],
    mut pos: usize,
    class: u8,
    floor: u8,
    holes: [u8; N],
) -> usize {
    while let Some(word) = data[pos..].first_chunk::<8>() {
        let w = u64::from_ne_bytes(*word);
        let mut stop = w.wrapping_sub(ONES * floor as u64) & !w & HIGHS;
        for hole in holes {
            let x = w ^ (ONES * hole as u64);
            stop |= x.wrapping_sub(ONES) & !x & HIGHS;
        }
        if stop != 0 {
            break;
        }
        pos += 8;
    }
    while pos < data.len() && is(data[pos], class) {
        pos += 1;
    }
    pos
}

#[inline(always)]
fn trim_ows(mut v: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = v {
        v = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = v {
        v = rest;
    }
    v
}

/// Length of the uri-host opening `v` when all of `v` is `uri-host [":" port]`,
/// else `None`. Host and a request-target authority both go through here, so
/// the two are always compared under the same port stripping.
#[inline]
fn split_host(v: &[u8]) -> Option<usize> {
    let mut i = 0;
    if v.first() == Some(&b'[') {
        i = 1;
        // IPv6 only: a zone ID would bring '%' back.
        while i < v.len() && (v[i].is_ascii_hexdigit() || v[i] == b':' || v[i] == b'.') {
            i += 1;
        }
        if i == 1 || v.get(i) != Some(&b']') {
            return None;
        }
        i += 1;
    } else {
        while i < v.len() && is(v[i], HOST) {
            i += 1;
        }
    }
    let host_end = i;
    if i < v.len() {
        if v[i] != b':' {
            return None;
        }
        i += 1;
        while i < v.len() && v[i].is_ascii_digit() {
            i += 1;
        }
        if i < v.len() {
            return None;
        }
    }
    Some(host_end)
}

/// `1*DIGIT` only: a sign, a list or an overflow is a length some backend
/// reads differently, and that difference is the body boundary.
#[inline]
fn parse_content_length(v: &[u8]) -> Option<usize> {
    if v.is_empty() {
        return None;
    }
    let mut n: usize = 0;
    for &b in v {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((b - b'0') as usize)?;
    }
    Some(n)
}

/// Length of an `http://` or `https://` prefix (schemes are case-insensitive).
#[inline]
fn absolute_scheme_len(target: &[u8]) -> Option<usize> {
    if target.len() >= 7 && target[..7].eq_ignore_ascii_case(b"http://") {
        Some(7)
    } else if target.len() >= 8 && target[..8].eq_ignore_ascii_case(b"https://") {
        Some(8)
    } else {
        None
    }
}

/// Whether an Upgrade value offers h2c, with or without a version suffix.
fn offers_h2c(value: &[u8]) -> bool {
    value.split(|&b| b == b',').any(|offer| {
        let offer = trim_ows(offer);
        let name = match memchr::memchr(b'/', offer) {
            Some(i) => &offer[..i],
            None => offer,
        };
        trim_ows(name).eq_ignore_ascii_case(b"h2c")
    })
}

/// Extract complete HTTP routing information with zero-copy parsing, in one
/// pass that also validates the head. Header bytes are forwarded verbatim, so
/// a head a lenient backend could read differently is refused, never
/// normalized: `Err` when no request line or Host can be extracted,
/// `header_violation` for everything else. Both mean 400 and close.
#[inline(always)]
pub fn extract_routing_info(request_data: &[u8]) -> Result<HttpRequestInfo<'_>> {
    debug!(
        "UNIFIED_PARSER: Processing HTTP request - content length: {}",
        request_data.len()
    );
    let data = request_data;
    let len = data.len();

    // Exactly `method SP request-target SP HTTP-version CRLF`: any looser
    // split lets a backend find a different method, target or version.
    let mut pos = 0;
    while pos < len && is(data[pos], TCHAR) {
        pos += 1;
    }
    let method_end = pos;
    if method_end == 0 || data.get(pos) != Some(&b' ') {
        return Err(anyhow!("Malformed HTTP request line"));
    }
    let target_start = pos + 1;
    let target_end = scan(data, target_start, TARGET, 0x21, TARGET_HOLES);
    if target_end == target_start || data.get(target_end) != Some(&b' ') {
        return Err(anyhow!("Malformed HTTP request line"));
    }
    let is_http10 = match data.get(target_end + 1..target_end + 11) {
        Some(b"HTTP/1.1\r\n") => false,
        Some(b"HTTP/1.0\r\n") => true,
        _ => return Err(anyhow!("Malformed HTTP request line")),
    };
    let line = std::str::from_utf8(&data[..target_end])
        .map_err(|_| anyhow!("Non-UTF8 data in HTTP request line"))?;
    let method = &line[..method_end];
    let target = &line[target_start..];

    let mut violation = false;
    let mut target_host: Option<&[u8]> = None;
    let mut path = target;
    let target_bytes = target.as_bytes();
    let is_connect = method == "CONNECT";
    if target_bytes[0] != b'/' || is_connect {
        if is_connect {
            // RFC 9110 9.3.6: CONNECT names only the tunnel's host:port.
            match split_host(target_bytes) {
                Some(h) if h > 0 && h + 1 < target_bytes.len() => {
                    target_host = Some(&target_bytes[..h]);
                }
                _ => violation = true,
            }
        } else if target_bytes == b"*" {
            violation = method != "OPTIONS";
        } else if let Some(scheme_len) = absolute_scheme_len(target_bytes) {
            // RFC 3986 ends the authority at '/', '?' or '#', a naive parser
            // at '/' only. Holding everything up to '/' to host[:port] leaves
            // no '?', '#' or '@' for the two readings to disagree on.
            let rest = &target[scheme_len..];
            let authority_len = memchr::memchr(b'/', rest.as_bytes()).unwrap_or(rest.len());
            let authority = &rest.as_bytes()[..authority_len];
            match split_host(authority) {
                Some(h) if h > 0 => target_host = Some(&authority[..h]),
                _ => violation = true,
            }
            path = if authority_len < rest.len() {
                &rest[authority_len..]
            } else {
                "/"
            };
        } else {
            violation = true;
        }
    }

    pos = target_end + 11;
    let header_start = pos;
    let mut host: Option<&[u8]> = None;
    let mut explicit_keepalive: Option<bool> = None;
    let mut content_length: Option<usize> = None;
    let mut is_chunked = false;
    let mut has_upgrade_header = false;
    let mut offers_h2c_upgrade = false;
    let mut connection_has_upgrade = false;
    let mut strip_spans = StripSpans::default();
    let mut upgrade_span: Option<(u32, u32)> = None;
    let mut expect_continue = false;
    let mut terminated = false;

    while !violation && pos < len {
        let line_start = pos;
        if data[pos] == b'\r' {
            terminated = data.get(pos + 1) == Some(&b'\n');
            break;
        }
        while pos < len && is(data[pos], TCHAR) {
            pos += 1;
        }
        let name_end = pos;
        // An empty name is obs-fold or a stray separator; a non-token byte
        // (`Content-Length : 5`) makes a name this parser ignores but a
        // lenient backend still honors.
        if name_end == line_start || data.get(pos) != Some(&b':') {
            violation = true;
            break;
        }
        let value_start = pos + 1;
        let value_end = scan(data, value_start, VALUE, 0x20, VALUE_HOLES);
        if data.get(value_end) != Some(&b'\r') || data.get(value_end + 1) != Some(&b'\n') {
            violation = true;
            break;
        }
        pos = value_end + 2;
        let name = &data[line_start..name_end];
        let value = &data[value_start..value_end];

        enum LineKind {
            Keep,
            Strip,
            Upgrade,
        }
        let mut line_kind = LineKind::Keep;

        match (name.len(), name[0] | 0x20) {
            (4, b'h') if name.eq_ignore_ascii_case(b"host") => {
                let value = trim_ows(value);
                match split_host(value) {
                    Some(h) if host.is_none() => host = Some(&value[..h]),
                    _ => violation = true,
                }
            }
            (14, b'c') if name.eq_ignore_ascii_case(b"content-length") => {
                match parse_content_length(trim_ows(value)) {
                    Some(n) if content_length.is_none() => content_length = Some(n),
                    _ => violation = true,
                }
            }
            (17, b't') if name.eq_ignore_ascii_case(b"transfer-encoding") => {
                // The only transfer coding this proxy understands is a single,
                // exact `chunked`, and HTTP/1.0 has no chunked framing at all:
                // anything else is a body we would frame differently than the
                // backend (TE.CL desync).
                if is_http10 || is_chunked || !trim_ows(value).eq_ignore_ascii_case(b"chunked") {
                    violation = true;
                } else {
                    is_chunked = true;
                }
            }
            (7, b'u') if name.eq_ignore_ascii_case(b"upgrade") => {
                if offers_h2c(value) {
                    // An h2c tunnel would let the client multiplex any request
                    // to the backend past per-request routing.
                    offers_h2c_upgrade = true;
                    line_kind = LineKind::Strip;
                } else {
                    // Only one Upgrade span is tracked; the slow path handles
                    // every line.
                    strip_spans.needs_slow_path |= has_upgrade_header;
                    has_upgrade_header = true;
                    line_kind = LineKind::Upgrade;
                }
            }
            (10, b'c') if name.eq_ignore_ascii_case(b"connection") => {
                line_kind = LineKind::Strip;
                for token in value.split(|&b| b == b',') {
                    let token = trim_ows(token);
                    if token.eq_ignore_ascii_case(b"close") {
                        explicit_keepalive = Some(false);
                    } else if token.eq_ignore_ascii_case(b"keep-alive") {
                        explicit_keepalive = Some(true);
                    } else if token.eq_ignore_ascii_case(b"upgrade") {
                        connection_has_upgrade = true;
                    } else if token.eq_ignore_ascii_case(b"content-length")
                        || token.eq_ignore_ascii_case(b"transfer-encoding")
                        || token.eq_ignore_ascii_case(b"host")
                    {
                        // The slow path strips what Connection nominates: the
                        // backend would then frame or route a different
                        // message than the one framed and routed here.
                        violation = true;
                    } else if !token.is_empty() && !token.eq_ignore_ascii_case(b"http2-settings") {
                        // Connection nominated an extra header to strip;
                        // the span fast path cannot honor that. HTTP2-Settings
                        // lines are span-stripped anyway.
                        strip_spans.needs_slow_path = true;
                    }
                }
            }
            (6, b'e') if name.eq_ignore_ascii_case(b"expect") => {
                // The proxy answers 100-continue itself and strips the header;
                // other expectations are stripped and ignored (RFC 9110 lets
                // an intermediary absorb expectations it handles).
                line_kind = LineKind::Strip;
                if trim_ows(value).eq_ignore_ascii_case(b"100-continue") {
                    expect_continue = true;
                }
            }
            (10, b'k') if name.eq_ignore_ascii_case(b"keep-alive") => line_kind = LineKind::Strip,
            (16, b'p') if name.eq_ignore_ascii_case(b"proxy-connection") => {
                line_kind = LineKind::Strip;
            }
            (2, b't') if name.eq_ignore_ascii_case(b"te") => line_kind = LineKind::Strip,
            (7, b't') if name.eq_ignore_ascii_case(b"trailer") => line_kind = LineKind::Strip,
            (14, b'h') if name.eq_ignore_ascii_case(b"http2-settings") => {
                line_kind = LineKind::Strip;
            }
            // The proxy appends its own X-Forwarded-Proto and sets none of the
            // others: a client's word on the scheme, host or port it used
            // would reach the backend as if the proxy vouched for it.
            (9, b'f') if name.eq_ignore_ascii_case(b"forwarded") => line_kind = LineKind::Strip,
            (15, b'x') if name.eq_ignore_ascii_case(b"x-forwarded-ssl") => {
                line_kind = LineKind::Strip;
            }
            (16, b'x')
                if name.eq_ignore_ascii_case(b"x-forwarded-host")
                    || name.eq_ignore_ascii_case(b"x-forwarded-port") =>
            {
                line_kind = LineKind::Strip;
            }
            (17, b'x') if name.eq_ignore_ascii_case(b"x-forwarded-proto") => {
                line_kind = LineKind::Strip;
            }
            (18, b'x') if name.eq_ignore_ascii_case(b"x-forwarded-scheme") => {
                line_kind = LineKind::Strip;
            }
            _ => {}
        }
        if violation {
            break;
        }
        match line_kind {
            LineKind::Keep => {}
            LineKind::Strip => strip_spans.push(line_start, pos),
            LineKind::Upgrade => upgrade_span = Some((line_start as u32, pos as u32)),
        }
    }

    let header_data = &data[header_start..pos];
    if !terminated {
        violation = true;
    }

    // Per RFC 7230 §5.4: HTTP/1.1 requests MUST include a Host header.
    // HTTP/1.0 clients may omit it, so use empty string for routing to handle.
    let host = match host {
        Some(h) => h,
        None if is_http10 || violation => &[][..],
        None => {
            return Err(anyhow!("Missing required Host header in HTTP/1.1 request"));
        }
    };
    // Routing reads Host while an RFC 9112 recipient of an absolute-form or
    // CONNECT target reads the target's authority: both must name one host.
    if target_host.is_some_and(|t| !t.eq_ignore_ascii_case(host)) {
        violation = true;
    }
    violation |= (host.is_empty() && !is_http10) || (content_length.is_some() && is_chunked);
    let host = std::str::from_utf8(host).map_err(|_| anyhow!("Non-ASCII Host"))?;

    let (path, query_string) = match path.find('?') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => (path, ""),
    };

    Ok(HttpRequestInfo {
        method,
        host,
        path,
        query_string,
        keepalive: explicit_keepalive.unwrap_or(!is_http10),
        header_data,
        content_length,
        is_chunked,
        // RFC 7230 6.7: an upgrade is only requested when Connection also
        // nominates it. A bare Upgrade header must not switch protocols.
        is_upgrade: has_upgrade_header && connection_has_upgrade && !offers_h2c_upgrade,
        header_violation: violation,
        strip_spans,
        upgrade_span,
        expect_continue,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_scan_stops_exactly_where_a_byte_scan_would() {
        fn check<const N: usize>(class: u8, floor: u8, holes: [u8; N]) {
            for b in 0..=255u8 {
                for at in 0..20 {
                    let mut buf = [b'a'; 24];
                    buf[at] = b;
                    let want = if is(b, class) { buf.len() } else { at };
                    let scan_from = |pos| scan(&buf, pos, class, floor, holes);
                    assert_eq!(scan_from(0), want, "{b:#04x} at {at}");
                    assert_eq!(scan_from(1.min(at)), want, "{b:#04x} at {at}");
                }
            }
        }
        check(VALUE, 0x20, VALUE_HOLES);
        check(TARGET, 0x21, TARGET_HOLES);
    }
}
