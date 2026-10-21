//! HTTP request/response modification filters.
//!
//! Applies header modifications, URL rewrites, and request mirroring
//! on raw byte slices — no intermediate String allocations on the request path.

use std::borrow::Cow;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::logging::warn;
use crate::proxy::http_parser::{is_forwarding_claim, StripSpans};
use crate::routing::{HttpFilter, HttpHeader, URLRewritePath};

/// Borrowed header modification parameters — zero per-request allocation.
/// References point into the `Arc<Vec<...>>` fields in `HttpFilter` variants.
#[derive(Debug)]
pub struct HeaderModifications<'a> {
    pub add: &'a [HttpHeader],
    pub set: &'a [HttpHeader],
    pub remove: &'a [String],
}

/// A URLRewrite applied to one request, borrowed from its rule.
#[derive(Debug, Clone)]
pub struct URLRewrite<'a> {
    pub hostname: Option<&'a str>,
    pub path: Option<RewrittenPath<'a>>,
}

/// A path modifier applied to one request: its new path, and the query its
/// value carries ([`crate::routing::PathValue`]), which goes before the
/// request's.
#[derive(Debug, Clone)]
pub struct RewrittenPath<'a> {
    /// A full path is the value's own; a prefix replacement is joined to
    /// the request's rest.
    pub path: Cow<'a, str>,
    pub query: Option<&'a str>,
}

#[cfg(test)]
impl<'a> RewrittenPath<'a> {
    /// The path a full-path value gives, its query split off as a rule's is.
    pub(crate) fn from_value(value: &'a str) -> Self {
        match value.split_once('?') {
            Some((path, query)) => Self {
                path: path.into(),
                query: Some(query),
            },
            None => Self {
                path: value.into(),
                query: None,
            },
        }
    }
}

/// The X-Forwarded-For and X-Forwarded-Proto lines of every request on a
/// client connection, formatted once: its peer and its scheme are fixed.
#[derive(Clone, Copy)]
pub(crate) struct ForwardedLines {
    bytes: [u8; Self::CAP],
    len: u8,
}

impl ForwardedLines {
    /// Longest IP text: six IPv6 groups with their colons, then a dotted
    /// IPv4 tail.
    const IP_TEXT_MAX: usize = 6 * 5 + 15;
    const CAP: usize =
        b"X-Forwarded-For: \r\nX-Forwarded-Proto: https\r\n".len() + Self::IP_TEXT_MAX;

    pub(crate) fn new(client_ip: std::net::IpAddr, is_tls: bool) -> Self {
        use std::io::Write;
        let mut bytes = [0u8; Self::CAP];
        let mut rest = &mut bytes[..];
        let proto = if is_tls { "https" } else { "http" };
        let written = write!(
            rest,
            "X-Forwarded-For: {client_ip}\r\nX-Forwarded-Proto: {proto}\r\n"
        );
        debug_assert!(written.is_ok(), "{client_ip} outgrew IP_TEXT_MAX");
        let len = (Self::CAP - rest.len()) as u8;
        Self { bytes, len }
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

/// An absolute-form request sent on to an origin server (RFC 9112 3.2.1):
/// its target's authority replaces the client's Host line, or follows the
/// forwarding lines where an HTTP/1.0 client sent none (3.2.2). Filters
/// then meet the same head as for an origin-form request.
#[derive(Clone, Copy)]
pub(crate) struct OriginForm<'a> {
    pub(crate) authority: &'a [u8],
    /// The client's Host line, CRLF included.
    pub(crate) host_line: Option<(u32, u32)>,
}

impl<'a> OriginForm<'a> {
    /// `None` unless `raw`, an accepted head, opens with an absolute-form
    /// target; `path_span` is its path within `raw`, `None` where the parser
    /// substituted "/". The common origin-form request costs one byte
    /// compare: its path starts the target.
    pub(crate) fn of(raw: &'a [u8], path_span: Option<(u32, u32)>) -> Option<Self> {
        let path_start = path_span.map(|(s, _)| s as usize);
        if path_start.is_some_and(|s| raw[s - 1] == b' ') {
            return None;
        }
        let target = memchr::memchr(b' ', raw)? + 1;
        // The parser admits origin-form, `*` and `http(s)://` targets only.
        if matches!(raw.get(target), Some(b'/' | b'*') | None) {
            return None;
        }
        let start = target
            + if raw.get(target + 4) == Some(&b':') {
                7
            } else {
                8
            };
        let end = match path_start {
            Some(s) => s,
            None => start + memchr::memchr(b' ', &raw[start..])?,
        };
        Some(Self {
            authority: &raw[start..end],
            host_line: host_line(raw),
        })
    }
}

/// Span of the Host line of `raw`, an accepted head: the parser took no
/// space before a field's colon and no second Host.
fn host_line(raw: &[u8]) -> Option<(u32, u32)> {
    let mut start = memchr::memchr(b'\n', raw)? + 1;
    while let Some(len) = memchr::memchr(b'\n', &raw[start..]) {
        let end = start + len + 1;
        if raw[start..end]
            .get(..5)
            .is_some_and(|n| n.eq_ignore_ascii_case(b"host:"))
        {
            return Some((start as u32, end as u32));
        }
        start = end;
    }
    None
}

fn put_host(authority: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(b"Host: ");
    out.extend_from_slice(authority);
    out.extend_from_slice(b"\r\n");
}

/// Assemble the outgoing request header block into `out` (reused scratch):
/// copies `raw` minus the parser-recorded hop-by-hop spans via bulk gap
/// copies, then appends X-Forwarded-For / X-Forwarded-Proto and, for upgrade
/// requests, the Connection: upgrade the stripped original carried. A
/// `path` takes the place of the request line's own, its query kept, and
/// `origin` the Host line. One pass, no allocation after scratch warmup.
///
/// `raw` is the full header block ending with the blank-line CRLF.
#[allow(clippy::too_many_arguments)]
pub(crate) fn assemble_forward_headers(
    raw: &[u8],
    path: Option<&[u8]>,
    spans: &StripSpans,
    upgrade_span: Option<(u32, u32)>,
    keep_upgrade: bool,
    origin: Option<OriginForm<'_>>,
    forwarded: &ForwardedLines,
    out: &mut Vec<u8>,
) {
    out.clear();
    out.reserve(raw.len() + 64);
    let terminator_start = raw.len().saturating_sub(2);
    let span = |(s, e): (u32, u32)| (s as usize, e as usize);
    let strip_upgrade = upgrade_span.filter(|_| !keep_upgrade).map(span);
    let host_line = origin.and_then(|o| o.host_line).map(span);
    // The lines cut besides the spans, in head order.
    let mut lines = match (strip_upgrade, host_line) {
        (Some(u), Some(h)) if h < u => [host_line, strip_upgrade],
        _ => [strip_upgrade, host_line],
    }
    .into_iter()
    .flatten()
    .peekable();
    let cut = |cursor: usize, (s, e): (usize, usize), out: &mut Vec<u8>| {
        out.extend_from_slice(&raw[cursor..s]);
        if let Some(origin) = origin.filter(|_| Some((s, e)) == host_line) {
            put_host(origin.authority, out);
        }
        e
    };

    // Both sources are in ascending parse order; merge while copying gaps.
    let mut cursor = 0usize;
    if let Some(path) = path {
        // No span covers the request line: the gaps resume at its CRLF.
        cursor = memchr::memchr(b'\r', raw).unwrap_or(0);
        put_request_line(&raw[..cursor], path, None, out);
    }
    for (s, e) in spans.iter() {
        while let Some(line) = lines.next_if(|&(ls, _)| ls < s) {
            cursor = cut(cursor, line, out);
        }
        out.extend_from_slice(&raw[cursor..s]);
        cursor = e;
    }
    for line in lines {
        cursor = cut(cursor, line, out);
    }
    out.extend_from_slice(&raw[cursor..terminator_start]);
    append_forward_headers(out, forwarded, keep_upgrade, origin);
    out.extend_from_slice(b"\r\n");
}

/// Thorough per-line rewrite for the rare requests the span fast path cannot
/// handle: span overflow or Connection-nominated extra headers (RFC 7230
/// §6.1 requires stripping every header the Connection field names).
pub(crate) fn assemble_forward_headers_slow(
    raw: &[u8],
    path: Option<&[u8]>,
    keep_upgrade: bool,
    origin: Option<OriginForm<'_>>,
    forwarded: &ForwardedLines,
    out: &mut Vec<u8>,
) {
    // Pass 1: collect nominated tokens from every Connection header,
    // borrowed from `raw` — the compares below are case-insensitive anyway.
    let mut nominated: Vec<&[u8]> = Vec::new();
    for line in header_lines(raw) {
        if let Some(value) = header_value_if(line, b"connection") {
            for token in value.split(',') {
                let token = token.trim();
                // Never honored for framing or routing headers: the proxy has
                // already framed and routed by them, and a backend that never
                // sees them would read the body as further requests.
                let pinned = ["content-length", "transfer-encoding", "host"]
                    .iter()
                    .any(|h| token.eq_ignore_ascii_case(h));
                if !token.is_empty() && !pinned {
                    nominated.push(token.as_bytes());
                }
            }
        }
    }

    // Pass 2: copy lines except hop-by-hop and nominated ones.
    out.clear();
    out.reserve(raw.len() + 64);
    let mut first = true;
    for line in header_lines(raw) {
        if first {
            // Request line is never a header.
            first = false;
            match path {
                Some(path) => put_request_line(line, path, None, out),
                None => out.extend_from_slice(line),
            }
            out.extend_from_slice(b"\r\n");
            continue;
        }
        let name_end = match memchr::memchr(b':', line) {
            Some(p) => p,
            None => {
                out.extend_from_slice(line);
                out.extend_from_slice(b"\r\n");
                continue;
            }
        };
        let name = &line[..name_end];
        if let Some(origin) = origin.filter(|_| name.eq_ignore_ascii_case(b"host")) {
            put_host(origin.authority, out);
            continue;
        }
        // A real upgrade nominates Upgrade in its own Connection header;
        // stripping it as nominated would break every WebSocket that also
        // carries an extra token or many hop-by-hop lines.
        let strip = if name.eq_ignore_ascii_case(b"upgrade") {
            !keep_upgrade
        } else {
            is_hop_by_hop_request_header(name)
                || nominated.iter().any(|n| name.eq_ignore_ascii_case(n))
        };
        if !strip {
            out.extend_from_slice(line);
            out.extend_from_slice(b"\r\n");
        }
    }
    append_forward_headers(out, forwarded, keep_upgrade, origin);
    out.extend_from_slice(b"\r\n");
}

fn append_forward_headers(
    out: &mut Vec<u8>,
    forwarded: &ForwardedLines,
    keep_upgrade: bool,
    origin: Option<OriginForm<'_>>,
) {
    // A fresh X-Forwarded-For line: multiple XFF headers concatenate as one
    // list, which avoids locating and splicing an existing header.
    out.extend_from_slice(forwarded.as_bytes());
    if keep_upgrade {
        out.extend_from_slice(b"Connection: upgrade\r\n");
    }
    if let Some(OriginForm {
        authority,
        host_line: None,
    }) = origin
    {
        put_host(authority, out);
    }
}

/// Request-target bounds within a request line: between its first two SPs.
fn target_bounds(line: &[u8]) -> Option<(usize, usize)> {
    let start = memchr::memchr(b' ', line)? + 1;
    let end = start + memchr::memchr(b' ', &line[start..])?;
    Some((start, end))
}

/// Where [`sanitize_response_headers`] cuts a head whose lines all end in
/// CRLF and none is empty, recorded by the pass that parsed it
/// (`worker::scan_response_head`): the copy is then a few bulk copies.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ResponseCuts {
    /// Hop-by-hop lines and every Content-Length line past the first.
    pub(crate) strip: StripSpans,
    /// The first Content-Length line, and its value's digits past any
    /// leading zeros: the `<n>` the copy writes in its place.
    pub(crate) length: Option<((u32, u32), (u32, u32))>,
}

/// Copy response headers into `out` minus hop-by-hop lines (RFC 7230 6.1).
/// The status line is copied untouched.
///
/// Framing is the caller's reading of the head (`worker::parse_response_head`),
/// never re-derived here. `content_length` is that reading's value: the
/// first Content-Length line becomes `Content-Length: <n>` and the others
/// go, since some clients (llhttp) refuse any repeat. `None` drops them all:
/// Transfer-Encoding overrides them (RFC 9112 6.3), and a client framing by
/// one would read past this body into the next response. The proxy owns the
/// client-side Connection header: `client_close` appends its own close
/// announcement. With `cuts` the copy follows them instead of splitting
/// lines, to the same bytes.
pub(crate) fn sanitize_response_headers(
    raw: &[u8],
    content_length: Option<usize>,
    cuts: Option<&ResponseCuts>,
    client_close: bool,
    out: &mut Vec<u8>,
) {
    out.clear();
    out.reserve(raw.len() + 24);
    match cuts {
        Some(cuts) => copy_around_cuts(raw, content_length.is_some(), cuts, out),
        None => copy_lines_sanitized(raw, content_length, out),
    }
    if client_close {
        out.extend_from_slice(b"Connection: close\r\n");
    }
    out.extend_from_slice(b"\r\n");
}

/// `sanitize_response_headers` over recorded cuts, up to the blank line.
fn copy_around_cuts(raw: &[u8], keep_length: bool, cuts: &ResponseCuts, out: &mut Vec<u8>) {
    // Copies up to the Content-Length line, then its canonical form in its
    // place; returns where the copy resumes.
    let put_length =
        |cursor: usize, ((s, e), (ds, de)): ((u32, u32), (u32, u32)), out: &mut Vec<u8>| {
            out.extend_from_slice(&raw[cursor..s as usize]);
            if keep_length {
                out.extend_from_slice(b"Content-Length: ");
                out.extend_from_slice(&raw[ds as usize..de as usize]);
                out.extend_from_slice(b"\r\n");
            }
            e as usize
        };
    // Both sources are in head order; merge while copying gaps.
    let mut cursor = 0usize;
    let mut length = cuts.length;
    for (s, e) in cuts.strip.iter() {
        if let Some(line) = length.filter(|&((ls, _), _)| (ls as usize) < s) {
            cursor = put_length(cursor, line, out);
            length = None;
        }
        out.extend_from_slice(&raw[cursor..s]);
        cursor = e;
    }
    if let Some(line) = length {
        cursor = put_length(cursor, line, out);
    }
    out.extend_from_slice(&raw[cursor..raw.len() - 2]);
}

/// `sanitize_response_headers` line by line, up to the blank line.
fn copy_lines_sanitized(raw: &[u8], content_length: Option<usize>, out: &mut Vec<u8>) {
    use std::io::Write;
    let mut lines = header_lines(raw);
    if let Some(status_line) = lines.next() {
        out.extend_from_slice(status_line);
        out.extend_from_slice(b"\r\n");
    }
    let mut content_length = content_length;
    for line in lines {
        match memchr::memchr(b':', line) {
            Some(p) if line[..p].eq_ignore_ascii_case(b"content-length") => {
                if let Some(n) = content_length.take() {
                    let _ = write!(out, "Content-Length: {n}\r\n");
                }
            }
            Some(p) if is_hop_by_hop_response_header(&line[..p]) => {}
            _ => {
                out.extend_from_slice(line);
                out.extend_from_slice(b"\r\n");
            }
        }
    }
}

/// First-byte dispatch keeps the per-line cost to one comparison for the
/// overwhelmingly common non-hop-by-hop headers.
///
/// Also client headers that must never pass even though they are not
/// hop-by-hop: HTTP2-Settings, as h2c is never offered to a backend, and the
/// forwarding claims of `is_forwarding_claim`. The parser strips these via
/// spans on the fast path; this covers the paths that re-derive the set by
/// name.
#[inline]
pub(crate) fn is_hop_by_hop_request_header(name: &[u8]) -> bool {
    match name.first().map(|b| b.to_ascii_lowercase()) {
        Some(b'c') => name.eq_ignore_ascii_case(b"connection"),
        Some(b'k') => name.eq_ignore_ascii_case(b"keep-alive"),
        Some(b'p') => name.eq_ignore_ascii_case(b"proxy-connection"),
        Some(b't') => name.eq_ignore_ascii_case(b"te") || name.eq_ignore_ascii_case(b"trailer"),
        Some(b'e') => name.eq_ignore_ascii_case(b"expect"),
        Some(b'h') => name.eq_ignore_ascii_case(b"http2-settings"),
        Some(b'f' | b'x') => is_forwarding_claim(name),
        _ => false,
    }
}

#[inline]
pub(crate) fn is_hop_by_hop_response_header(name: &[u8]) -> bool {
    match name.first().map(|b| b.to_ascii_lowercase()) {
        Some(b'c') => name.eq_ignore_ascii_case(b"connection"),
        Some(b'k') => name.eq_ignore_ascii_case(b"keep-alive"),
        Some(b'p') => name.eq_ignore_ascii_case(b"proxy-connection"),
        Some(b't') => name.eq_ignore_ascii_case(b"te") || name.eq_ignore_ascii_case(b"trailer"),
        Some(b'u') => name.eq_ignore_ascii_case(b"upgrade"),
        _ => false,
    }
}

/// Iterate CRLF-separated lines of a header block, excluding the final blank
/// line. Tolerates a bare-LF separator the way the parsers upstream do.
pub(crate) fn header_lines(raw: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        while pos < raw.len() {
            let start = pos;
            let end = memchr::memchr(b'\n', &raw[pos..])
                .map(|p| pos + p)
                .unwrap_or(raw.len());
            pos = end + 1;
            let line = if end > start && raw[end - 1] == b'\r' {
                &raw[start..end - 1]
            } else {
                &raw[start..end]
            };
            if !line.is_empty() {
                return Some(line);
            }
        }
        None
    })
}

/// Case-insensitive header-name match returning the value as &str.
fn header_value_if<'a>(line: &'a [u8], name: &[u8]) -> Option<&'a str> {
    let p = memchr::memchr(b':', line)?;
    if !line[..p].eq_ignore_ascii_case(name) {
        return None;
    }
    std::str::from_utf8(&line[p + 1..]).ok().map(|v| v.trim())
}

/// Remainder of the request path after the matched prefix.
/// ReplacePrefixMatch is only defined for PathPrefix rules; on a rule whose
/// path is not a literal byte prefix of the request (regex patterns), slicing
/// by length would panic (abort) or split UTF-8, so treat the path as fully
/// consumed instead.
pub(crate) fn prefix_remainder<'a>(rule_path: &str, request_path: &'a str) -> &'a str {
    request_path.strip_prefix(rule_path).unwrap_or("")
}

/// Join a prefix replacement with the remaining suffix from the original path.
/// Avoids double-slash: `/` + `/three` is `/three`, not `//three`. The
/// value's query, split off when its rule compiled, follows the joined path.
pub(crate) fn join_prefix_path(prefix: &str, suffix: &str) -> String {
    // Strip trailing slash from prefix if suffix starts with slash
    let prefix = if prefix.ends_with('/') && suffix.starts_with('/') {
        &prefix[..prefix.len() - 1]
    } else {
        prefix
    };
    let needs_slash = !prefix.ends_with('/') && !suffix.starts_with('/') && !suffix.is_empty();
    let mut result = String::with_capacity(prefix.len() + suffix.len() + usize::from(needs_slash));
    result.push_str(prefix);
    if needs_slash {
        result.push('/');
    }
    result.push_str(suffix);
    if result.is_empty() {
        result.push('/');
    }
    result
}

/// Extract `HeaderModifications` from the first matching filter in a filter list.
/// Zero-copy: borrows from the Arc internals stored in the route table.
pub fn extract_header_mods<'a>(
    filters: &'a [HttpFilter],
    is_response: bool,
) -> Option<HeaderModifications<'a>> {
    for filter in filters {
        match filter {
            HttpFilter::RequestHeaderModifier { add, set, remove } if !is_response => {
                return Some(HeaderModifications { add, set, remove });
            }
            HttpFilter::ResponseHeaderModifier { add, set, remove } if is_response => {
                return Some(HeaderModifications { add, set, remove });
            }
            _ => {}
        }
    }
    None
}

/// Extract URL rewrite parameters from rule filters. Only a prefix
/// replacement allocates: the rest is the rule's, borrowed.
pub fn extract_url_rewrite<'a>(
    filters: &'a [HttpFilter],
    rule_path: &str,
    request_path: &str,
) -> Option<URLRewrite<'a>> {
    for filter in filters {
        if let HttpFilter::URLRewrite { hostname, path } = filter {
            let rewritten_path = path.as_ref().map(|p| match p {
                URLRewritePath::ReplaceFullPath(value) => RewrittenPath {
                    path: Cow::Borrowed(&value.path),
                    query: value.query.as_deref(),
                },
                URLRewritePath::ReplacePrefixMatch(value) => RewrittenPath {
                    path: Cow::Owned(join_prefix_path(
                        &value.path,
                        prefix_remainder(rule_path, request_path),
                    )),
                    query: value.query.as_deref(),
                },
            });
            return Some(URLRewrite {
                hostname: hostname.as_deref(),
                path: rewritten_path,
            });
        }
    }
    None
}

/// Maximum request body size (bytes) that will be buffered for mirroring.
/// Bodies exceeding this are silently skipped (primary request unaffected).
/// Matches Envoy's default `per_connection_buffer_limit_bytes`.
pub(crate) const MIRROR_BODY_MAX: usize = 1024 * 1024; // 1 MB

/// Bound on concurrently in-flight mirror tasks across the process. Mirroring
/// is best-effort by contract, so when a slow/dead mirror backend makes tasks
/// pile up (each holding up to `MIRROR_BODY_MAX` of copied request), excess
/// mirrors are shed instead of accumulating 5s × RPS tasks of buffered bodies.
static MIRROR_PERMITS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(256);

/// Fire-and-forget mirror dispatch with fraction-based sampling and timeout.
///
/// Iterates rule filters looking for `RequestMirror` variants.
/// For each target, draws against the configured fraction to decide whether
/// to mirror. Spawned tasks are bounded by a total 5s timeout
/// (2s connect + 3s write) to prevent leaked tasks from slow/dead backends,
/// and by `MIRROR_PERMITS` in flight.
pub(crate) fn dispatch_mirrors(filters: &[HttpFilter], data: &[u8]) {
    for filter in filters {
        let (addr, fraction) = match filter {
            HttpFilter::RequestMirror {
                backend_addr,
                fraction,
            } => (*backend_addr, *fraction),
            _ => continue,
        };

        if !fraction.sample() {
            continue;
        }

        let permit = match MIRROR_PERMITS.try_acquire() {
            Ok(p) => p,
            Err(_) => {
                warn!("mirror capacity exhausted, shedding mirror to {}", addr);
                continue;
            }
        };

        let data = data.to_vec();
        tokio::spawn(async move {
            let _permit = permit;
            // Total timeout: 5s covers connect + write. Prevents leaked tasks
            // when the mirror backend is slow or unreachable.
            let result = tokio::time::timeout(Duration::from_secs(5), async {
                let connect_result =
                    tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(addr)).await;
                if let Ok(Ok(mut conn)) = connect_result {
                    let _ = conn.write_all(&data).await;
                }
            })
            .await;

            if result.is_err() {
                warn!("mirror dispatch to {} timed out", addr);
            }
        });
    }
}

/// Core header modification engine shared by request and response paths.
///
/// Iterates raw header bytes line-by-line, applying set/add/remove operations.
/// The first line (request-line or status-line) is written to `out` by the caller
/// before invoking this function — this only processes header lines that follow.
///
/// `rewrite_hostname`: if Some, replaces the Host header value (request path only).
fn apply_header_mods_inner(
    raw: &[u8],
    mods: Option<&HeaderModifications<'_>>,
    rewrite_hostname: Option<&str>,
    out: &mut Vec<u8>,
) {
    // Track which "set" headers were applied (matched existing headers)
    let mut set_applied: Vec<bool> = mods.map(|m| vec![false; m.set.len()]).unwrap_or_default();
    // Like `set`, a hostname rewrite adds the Host it finds no line for.
    let mut host_missing = rewrite_hostname;

    for line in header_lines(raw) {
        // Find colon to extract header name
        let colon_pos = match memchr::memchr(b':', line) {
            Some(p) => p,
            None => {
                out.extend_from_slice(line);
                out.extend_from_slice(b"\r\n");
                continue;
            }
        };
        let name = &line[..colon_pos];

        // Replace Host header if hostname rewrite is active
        if let Some(new_host) = rewrite_hostname {
            if name.eq_ignore_ascii_case(b"host") {
                out.extend_from_slice(b"Host: ");
                out.extend_from_slice(new_host.as_bytes());
                out.extend_from_slice(b"\r\n");
                host_missing = None;
                continue;
            }
        }

        if let Some(mods) = mods {
            // Compared as bytes: ASCII case folding leaves other bytes as
            // they are, so a name that is no UTF-8 matches no configured one.
            if mods
                .remove
                .iter()
                .any(|r| r.as_bytes().eq_ignore_ascii_case(name))
            {
                continue;
            }
            if let Some((idx, set_header)) = mods
                .set
                .iter()
                .enumerate()
                .find(|(_, h)| h.name.as_bytes().eq_ignore_ascii_case(name))
            {
                set_applied[idx] = true;
                out.extend_from_slice(set_header.name.as_bytes());
                out.extend_from_slice(b": ");
                out.extend_from_slice(set_header.value.as_bytes());
                out.extend_from_slice(b"\r\n");
                continue;
            }
        }

        out.extend_from_slice(line);
        out.extend_from_slice(b"\r\n");
    }

    if let Some(new_host) = host_missing {
        out.extend_from_slice(b"Host: ");
        out.extend_from_slice(new_host.as_bytes());
        out.extend_from_slice(b"\r\n");
    }

    // Add "set" headers that didn't match any existing header (set = replace OR add)
    if let Some(mods) = mods {
        for (idx, h) in mods.set.iter().enumerate() {
            if !set_applied[idx] {
                out.extend_from_slice(h.name.as_bytes());
                out.extend_from_slice(b": ");
                out.extend_from_slice(h.value.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
        }
        // Add headers (always appended)
        for h in mods.add.iter() {
            out.extend_from_slice(h.name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(h.value.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
    }

    out.extend_from_slice(b"\r\n");
}

/// Split raw header bytes into (first_line, rest) at the first CRLF boundary.
/// Returns (first_line_bytes, remaining_bytes_after_crlf).
fn split_first_line(raw: &[u8]) -> (&[u8], &[u8]) {
    let nl = memchr::memchr(b'\n', raw).map_or(raw.len(), |p| p + 1);
    let line = raw[..nl].strip_suffix(b"\n").unwrap_or(&raw[..nl]);
    (line.strip_suffix(b"\r").unwrap_or(line), &raw[nl..])
}

/// Apply request header modifications into `out` (cleared first), including
/// the trailing \r\n\r\n. Body bytes are NOT included; the caller sends them
/// separately from the original buffer. `out` is a caller-owned reusable
/// buffer so the filter path allocates nothing after warmup.
pub(crate) fn apply_request_header_modifications(
    header_region: &[u8],
    header_mods: Option<&HeaderModifications<'_>>,
    url_rewrite: Option<&URLRewrite<'_>>,
    out: &mut Vec<u8>,
) {
    let path = url_rewrite
        .and_then(|r| r.path.as_ref())
        .map(|p| (p.path.as_bytes(), p.query));
    let rewrite_hostname = url_rewrite.and_then(|r| r.hostname);
    rewrite_request_head(header_region, path, header_mods, rewrite_hostname, out);
}

/// The request line with `path` in place of its target's path, then the
/// header lines through `apply_header_mods_inner`, into `out` (cleared
/// first). Shared by URLRewrite paths and the origin-form rewrite. `path`
/// carries the query of the value it came from, if any.
fn rewrite_request_head(
    header_region: &[u8],
    path: Option<(&[u8], Option<&str>)>,
    header_mods: Option<&HeaderModifications<'_>>,
    rewrite_hostname: Option<&str>,
    out: &mut Vec<u8>,
) {
    out.clear();
    out.reserve(header_region.len() + 256);
    let (first_line, rest) = split_first_line(header_region);
    match path {
        Some((path, value_query)) => put_request_line(first_line, path, value_query, out),
        None => out.extend_from_slice(first_line),
    }
    out.extend_from_slice(b"\r\n");
    apply_header_mods_inner(rest, header_mods, rewrite_hostname, out);
}

/// Request line `line` (no CRLF) into `out`, with `path` in place of all
/// its target holds before the query. The query stays: path normalization
/// and path modifiers (HTTPPathModifier) change the path only. A modifier
/// value's own query (`value_query`) comes first, the request's after it
/// ([`query_glue`]). The parser let no '?' into an absolute-form
/// authority, so such a target leaves origin-form.
fn put_request_line(line: &[u8], path: &[u8], value_query: Option<&str>, out: &mut Vec<u8>) {
    let Some((start, end)) = target_bounds(line) else {
        out.extend_from_slice(line);
        return;
    };
    let query = memchr::memchr(b'?', &line[start..end]).map_or(end, |q| start + q);
    out.extend_from_slice(&line[..start]);
    out.extend_from_slice(path);
    match value_query {
        // As sent, a bare '?' included.
        None => out.extend_from_slice(&line[query..end]),
        Some(value_query) => {
            out.push(b'?');
            out.extend_from_slice(value_query.as_bytes());
            if end > query + 1 {
                out.extend_from_slice(query_glue(value_query).as_bytes());
                out.extend_from_slice(&line[query + 1..end]);
            }
        }
    }
    out.extend_from_slice(&line[end..]);
}

/// What goes between a path modifier value's query and the request's
/// non-empty one: a '&', unless the value's is empty or already ends in one.
/// Gateway API v1.4.1 bounds the value by length only, so it may carry a
/// query.
pub(crate) fn query_glue(value_query: &str) -> &'static str {
    if value_query.is_empty() || value_query.ends_with('&') {
        ""
    } else {
        "&"
    }
}

/// Apply response header modifications into `out` (cleared first), including
/// the trailing \r\n\r\n. Operates on raw bytes — no intermediate String
/// allocations.
pub(crate) fn apply_response_header_mods(
    headers: &[u8],
    mods: &HeaderModifications<'_>,
    out: &mut Vec<u8>,
) {
    out.clear();
    out.reserve(headers.len() + 256);

    let (status_line, rest) = split_first_line(headers);

    // Status line: pass through unchanged
    out.extend_from_slice(status_line);
    out.extend_from_slice(b"\r\n");

    apply_header_mods_inner(rest, Some(mods), None, out);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed_spans(req: &[u8]) -> crate::proxy::http_parser::HttpRequestInfo<'_> {
        crate::proxy::http_parser::extract_routing_info(req).unwrap()
    }

    /// `path_span` as the request processor derives it from the parser:
    /// `None` where the parser substituted "/".
    fn path_span(req: &[u8]) -> Option<(u32, u32)> {
        let path = parsed_spans(req).path.as_bytes();
        let start = (path.as_ptr() as usize).checked_sub(req.as_ptr() as usize)?;
        let end = start + path.len();
        (end <= req.len()).then_some((start as u32, end as u32))
    }

    #[test]
    fn test_origin_form_of_splits_the_authority_and_finds_host() {
        let span = |req: &[u8], line: &[u8]| {
            let at = req.windows(line.len()).position(|w| w == line).unwrap();
            Some((at as u32, (at + line.len()) as u32))
        };
        for (req, want) in [
            (
                &b"GET http://a.test:8080/p?q=1 HTTP/1.1\r\nX: 1\r\nHOST: a.test\r\n\r\n"[..],
                Some((&b"a.test:8080"[..], Some(&b"HOST: a.test\r\n"[..]))),
            ),
            (
                b"GET http://a.test HTTP/1.1\r\nHost: a.test\r\n\r\n",
                Some((&b"a.test"[..], Some(&b"Host: a.test\r\n"[..]))),
            ),
            (
                b"GET HTTPS://[::1] HTTP/1.0\r\nX-Host: y\r\n\r\n",
                Some((&b"[::1]"[..], None)),
            ),
            // Origin-form, hostless or not, and asterisk-form stay as sent.
            (b"GET /p?q HTTP/1.1\r\nHost: a.test\r\n\r\n", None),
            (b"GET /x HTTP/1.0\r\n\r\n", None),
            (b"OPTIONS * HTTP/1.1\r\nHost: a.test\r\n\r\n", None),
        ] {
            let got = OriginForm::of(req, path_span(req));
            let want = want.map(|(authority, line)| (authority, line.and_then(|l| span(req, l))));
            assert_eq!(
                got.map(|o| (o.authority, o.host_line)),
                want,
                "{}",
                String::from_utf8_lossy(req)
            );
        }
    }

    fn fwd(is_tls: bool) -> ForwardedLines {
        ForwardedLines::new("10.1.2.3".parse().unwrap(), is_tls)
    }

    #[test]
    fn forwarded_lines_fit_every_peer() {
        for ip in [
            "10.1.2.3",
            "255.255.255.255",
            "::ffff:255.255.255.255",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "::1",
        ] {
            let lines = ForwardedLines::new(ip.parse().unwrap(), true);
            assert_eq!(
                std::str::from_utf8(lines.as_bytes()).unwrap(),
                format!("X-Forwarded-For: {ip}\r\nX-Forwarded-Proto: https\r\n")
            );
        }
    }

    #[test]
    fn test_assemble_strips_hop_by_hop_and_appends_forwarding() {
        let req = b"GET /a HTTP/1.1\r\nHost: h\r\nConnection: keep-alive\r\nKeep-Alive: timeout=5\r\nProxy-Connection: keep-alive\r\nTE: trailers\r\nAccept: */*\r\n\r\n";
        let info = parsed_spans(req);
        assert!(!info.strip_spans.needs_slow_path);
        assert_eq!(info.strip_spans.iter().count(), 4);

        let mut out = Vec::new();
        assemble_forward_headers(
            req,
            None,
            &info.strip_spans,
            info.upgrade_span,
            false,
            None,
            &fwd(false),
            &mut out,
        );
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.starts_with("GET /a HTTP/1.1\r\nHost: h\r\n"));
        assert!(!s.to_ascii_lowercase().contains("connection:"));
        assert!(!s.to_ascii_lowercase().contains("keep-alive:"));
        assert!(!s.to_ascii_lowercase().contains("proxy-connection:"));
        assert!(!s.to_ascii_lowercase().contains("te:"));
        assert!(s.contains("Accept: */*\r\n"));
        assert!(s.contains("X-Forwarded-For: 10.1.2.3\r\n"));
        assert!(s.contains("X-Forwarded-Proto: http\r\n"));
        assert!(s.ends_with("\r\n\r\n"));
    }

    #[test]
    fn test_assemble_keeps_upgrade_for_upgrade_requests() {
        let req = b"GET /ws HTTP/1.1\r\nHost: h\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: k\r\n\r\n";
        let info = parsed_spans(req);
        assert!(info.is_upgrade);

        let mut out = Vec::new();
        assemble_forward_headers(
            req,
            None,
            &info.strip_spans,
            info.upgrade_span,
            true,
            None,
            &fwd(true),
            &mut out,
        );
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("Upgrade: websocket\r\n"));
        assert!(s.contains("Connection: upgrade\r\n"));
        assert!(s.contains("X-Forwarded-Proto: https\r\n"));

        // Non-upgrade request: a bare Upgrade header is stripped.
        let req = b"GET / HTTP/1.1\r\nHost: h\r\nUpgrade: websocket\r\n\r\n";
        let info = parsed_spans(req);
        assert!(!info.is_upgrade);
        assemble_forward_headers(
            req,
            None,
            &info.strip_spans,
            info.upgrade_span,
            false,
            None,
            &fwd(false),
            &mut out,
        );
        let s = std::str::from_utf8(&out).unwrap();
        assert!(!s.to_ascii_lowercase().contains("upgrade"));
    }

    #[test]
    fn test_assemble_slow_path_strips_nominated_headers() {
        let req = b"GET / HTTP/1.1\r\nHost: h\r\nConnection: close, X-Internal\r\nX-Internal: secret\r\nAccept: */*\r\n\r\n";
        let info = parsed_spans(req);
        assert!(info.strip_spans.needs_slow_path);

        let mut out = Vec::new();
        assemble_forward_headers_slow(req, None, false, None, &fwd(false), &mut out);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(!s.to_ascii_lowercase().contains("x-internal"));
        assert!(!s.to_ascii_lowercase().contains("connection:"));
        assert!(s.contains("Accept: */*\r\n"));
        assert!(s.ends_with("\r\n\r\n"));
    }

    #[test]
    fn test_assemble_slow_path_keeps_upgrade_for_upgrade_requests() {
        // An extra Connection token diverts a WebSocket to the slow path.
        let req = b"GET /ws HTTP/1.1\r\nHost: h\r\nUpgrade: websocket\r\nConnection: keep-alive, Upgrade, X-Foo\r\nX-Foo: 1\r\nSec-WebSocket-Key: k\r\n\r\n";
        let mut out = Vec::new();
        assemble_forward_headers_slow(req, None, true, None, &fwd(false), &mut out);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("Upgrade: websocket\r\n"), "{s}");
        assert!(s.ends_with("Connection: upgrade\r\n\r\n"), "{s}");
        assert_eq!(s.to_ascii_lowercase().matches("connection:").count(), 1);
        assert!(!s.to_ascii_lowercase().contains("x-foo"), "{s}");
        assert!(s.contains("Sec-WebSocket-Key: k\r\n"));

        // Not an upgrade: the Upgrade line goes with the other nominated ones.
        assemble_forward_headers_slow(req, None, false, None, &fwd(false), &mut out);
        let s = std::str::from_utf8(&out).unwrap().to_ascii_lowercase();
        assert!(!s.contains("upgrade"), "{s}");
    }

    #[test]
    fn test_assemble_slow_path_replaces_client_forwarded_proto() {
        let req = b"GET / HTTP/1.1\r\nHost: h\r\nX-Forwarded-Proto: https\r\nConnection: X-Internal\r\n\r\n";
        let mut out = Vec::new();
        assemble_forward_headers_slow(req, None, false, None, &fwd(false), &mut out);
        let s = std::str::from_utf8(&out).unwrap();
        assert_eq!(s.matches("X-Forwarded-Proto").count(), 1, "{s}");
        assert!(s.contains("X-Forwarded-Proto: http\r\n"), "{s}");
    }

    #[test]
    fn test_assemble_sends_absolute_form_as_origin_form() {
        for (req, want) in [
            (
                &b"GET http://a.test:8080/p?q HTTP/1.1\r\nhost: a.test\r\nKeep-Alive: 5\r\nAccept: */*\r\n\r\n"[..],
                "GET /p?q HTTP/1.1\r\nHost: a.test:8080\r\nAccept: */*\r\nX-Forwarded-For: 10.1.2.3\r\nX-Forwarded-Proto: http\r\n\r\n",
            ),
            // An HTTP/1.0 client may have sent no Host to replace.
            (
                b"GET http://a.test HTTP/1.0\r\nAccept: */*\r\n\r\n",
                "GET / HTTP/1.0\r\nAccept: */*\r\nX-Forwarded-For: 10.1.2.3\r\nX-Forwarded-Proto: http\r\nHost: a.test\r\n\r\n",
            ),
        ] {
            let info = parsed_spans(req);
            let origin = OriginForm::of(req, path_span(req));
            let path = Some(info.path.as_bytes());
            let mut out = Vec::new();
            assemble_forward_headers(
                req,
                path,
                &info.strip_spans,
                info.upgrade_span,
                false,
                origin,
                &fwd(false),
                &mut out,
            );
            assert_eq!(std::str::from_utf8(&out).unwrap(), want);
            assemble_forward_headers_slow(req, path, false, origin, &fwd(false), &mut out);
            assert_eq!(std::str::from_utf8(&out).unwrap(), want);
        }
    }

    /// Like `set`, a URLRewrite hostname adds the Host it finds no line to
    /// replace: an HTTP/1.0 origin-form request may carry none.
    #[test]
    fn test_url_rewrite_hostname_adds_a_missing_host() {
        let rewrite = URLRewrite {
            hostname: Some("internal.example"),
            path: None,
        };
        let mut out = Vec::new();
        apply_request_header_modifications(
            b"GET /x HTTP/1.0\r\nAccept: */*\r\n\r\n",
            None,
            Some(&rewrite),
            &mut out,
        );
        assert_eq!(
            std::str::from_utf8(&out).unwrap(),
            "GET /x HTTP/1.0\r\nAccept: */*\r\nHost: internal.example\r\n\r\n"
        );
    }

    /// The fast path strips these by parser span; a Connection-nominated
    /// extra header must not let them through the slow path instead.
    #[test]
    fn test_assemble_slow_path_drops_client_forwarding_claims() {
        let req = b"GET / HTTP/1.1\r\nHost: h\r\nForwarded: for=6.6.6.6;proto=https\r\nX-Forwarded-Host: evil\r\nx-forwarded-port: 443\r\nX-Forwarded-Ssl: on\r\nX-FORWARDED-SCHEME: https\r\nX-Forwarded-For: 6.6.6.6\r\nX-Forwarded-Hostname: kept\r\nConnection: X-Internal\r\n\r\n";
        let mut out = Vec::new();
        assemble_forward_headers_slow(req, None, false, None, &fwd(false), &mut out);
        let s = std::str::from_utf8(&out).unwrap();
        let lower = s.to_ascii_lowercase();
        for name in [
            "\nforwarded:",
            "\nx-forwarded-host:",
            "\nx-forwarded-port:",
            "\nx-forwarded-ssl:",
            "\nx-forwarded-scheme:",
        ] {
            assert!(!lower.contains(name), "{name:?} forwarded: {s}");
        }
        // X-Forwarded-For appends: the client's list, then the proxy's peer.
        assert!(s.contains("X-Forwarded-For: 6.6.6.6\r\n"), "{s}");
        assert!(s.contains("X-Forwarded-For: 10.1.2.3\r\n"), "{s}");
        assert!(s.contains("X-Forwarded-Hostname: kept\r\n"), "{s}");
    }

    #[test]
    fn test_assemble_slow_path_never_strips_framing_or_host() {
        // Stripping Content-Length here would make the backend read the body
        // as a second request while the proxy relays it as this one's body.
        let req = b"POST / HTTP/1.1\r\nHost: h\r\nContent-Length: 5\r\nConnection: keep-alive, Content-Length, HOST, Transfer-Encoding, X-Internal\r\nX-Internal: s\r\n\r\n";
        let mut out = Vec::new();
        assemble_forward_headers_slow(req, None, false, None, &fwd(false), &mut out);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("Content-Length: 5\r\n"), "{s}");
        assert!(s.contains("Host: h\r\n"), "{s}");
        assert!(!s.to_ascii_lowercase().contains("x-internal"), "{s}");
    }

    #[test]
    fn test_sanitize_response_headers() {
        let resp =
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\nKeep-Alive: timeout=5\r\nServer: x\r\n\r\n";
        let mut out = Vec::new();
        sanitize_response_headers(resp, Some(2), None, false, &mut out);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(s.contains("Content-Length: 2\r\n"));
        assert!(s.contains("Server: x\r\n"));
        assert!(!s.to_ascii_lowercase().contains("connection:"));
        assert!(!s.to_ascii_lowercase().contains("keep-alive"));
        assert!(s.ends_with("\r\n\r\n"));

        sanitize_response_headers(resp, Some(2), None, true, &mut out);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.ends_with("Connection: close\r\n\r\n"));
    }

    #[test]
    fn test_sanitize_forwards_one_canonical_content_length_or_none() {
        // Under Transfer-Encoding, none.
        let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\ncontent-length: 5\r\nServer: x\r\n\r\n";
        let mut out = Vec::new();
        sanitize_response_headers(resp, None, None, false, &mut out);
        assert_eq!(
            std::str::from_utf8(&out).unwrap(),
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nServer: x\r\n\r\n"
        );

        // Else one, where the first was, with the value framed by.
        let resp =
            b"HTTP/1.1 200 OK\r\ncontent-length:  05\r\nServer: x\r\nContent-Length: 5\r\n\r\n";
        sanitize_response_headers(resp, Some(5), None, false, &mut out);
        assert_eq!(
            std::str::from_utf8(&out).unwrap(),
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nServer: x\r\n\r\n"
        );
    }

    /// The parser strips by span what the slow path strips by name.
    #[test]
    fn test_parser_and_slow_path_agree_on_forwarding_claims() {
        for name in [
            "Forwarded",
            "X-Forwarded-Proto",
            "x-forwarded-host",
            "X-FORWARDED-PORT",
            "X-Forwarded-Ssl",
            "X-Forwarded-Scheme",
            "X-Forwarded-For",
            "X-Forwarded-Hostname",
            "X-Forwarded-",
            "Forwarded-For",
            "From",
        ] {
            let req = format!("GET / HTTP/1.1\r\nHost: h\r\n{name}: v\r\n\r\n");
            let span_stripped = parsed_spans(req.as_bytes()).strip_spans.iter().count() == 1;
            assert_eq!(
                span_stripped,
                is_hop_by_hop_request_header(name.as_bytes()),
                "{name}"
            );
        }
    }

    #[test]
    fn test_request_set_strips_forwarding_claims_and_h2c_settings() {
        for name in [
            "X-Forwarded-Proto",
            "x-forwarded-proto",
            "HTTP2-Settings",
            "Forwarded",
            "X-Forwarded-Host",
            "X-FORWARDED-PORT",
            "x-forwarded-ssl",
            "X-Forwarded-Scheme",
        ] {
            assert!(is_hop_by_hop_request_header(name.as_bytes()), "{name}");
        }
        for name in [
            "Host",
            "X-Forwarded-For",
            "Content-Length",
            "X-Forwarded-",
            "X-Forwarded-Hostname",
            "Forwarded-For",
            "From",
        ] {
            assert!(!is_hop_by_hop_request_header(name.as_bytes()), "{name}");
        }
    }

    #[test]
    fn test_prefix_remainder_regex_rule_no_panic() {
        // Regression: slicing by rule.path.len() aborted when the rule path
        // was a regex pattern longer than the matched request path.
        assert_eq!(prefix_remainder("/v1", "/v1/users"), "/users");
        assert_eq!(prefix_remainder("/", "/three"), "three");
        assert_eq!(
            prefix_remainder(r"^/a$|^/much-longer-alternative$", "/a"),
            ""
        );
        let filters = [HttpFilter::URLRewrite {
            hostname: None,
            path: Some(URLRewritePath::ReplacePrefixMatch("/new".into())),
        }];
        let rewrite =
            extract_url_rewrite(&filters, r"^/a$|^/much-longer-alternative$", "/a").unwrap();
        assert!(matches!(rewrite.path, Some(RewrittenPath { path: ref p, .. }) if p == "/new"));
    }

    #[test]
    fn test_apply_modifications_path_rewrite() {
        let headers = b"GET /old/path HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let rewrite = URLRewrite {
            hostname: None,
            path: Some(RewrittenPath::from_value("/new/path")),
        };
        let mut result = Vec::new();
        apply_request_header_modifications(headers, None, Some(&rewrite), &mut result);
        let result_str = std::str::from_utf8(&result).unwrap();

        assert!(result_str.starts_with("GET /new/path HTTP/1.1\r\n"));
        assert!(result_str.contains("Host: example.com"));
    }

    /// A path modifier replaces the path only (HTTPPathModifier): the
    /// query goes on as sent.
    #[test]
    fn test_url_rewrite_keeps_the_query() {
        let mut out = Vec::new();
        for (path, head, line) in [
            (
                RewrittenPath::from_value("/new"),
                &b"GET /old?a=1&b=%2F HTTP/1.1\r\nHost: h\r\n\r\n"[..],
                "GET /new?a=1&b=%2F HTTP/1.1\r\n",
            ),
            (
                RewrittenPath::from_value("/v2/x"),
                b"GET /v1/x?a HTTP/1.1\r\nHost: h\r\n\r\n",
                "GET /v2/x?a HTTP/1.1\r\n",
            ),
            (
                RewrittenPath::from_value("/new"),
                b"GET /old? HTTP/1.1\r\nHost: h\r\n\r\n",
                "GET /new? HTTP/1.1\r\n",
            ),
            (
                RewrittenPath::from_value("/new"),
                b"GET /old HTTP/1.1\r\nHost: h\r\n\r\n",
                "GET /new HTTP/1.1\r\n",
            ),
        ] {
            let rewrite = URLRewrite {
                hostname: None,
                path: Some(path),
            };
            apply_request_header_modifications(head, None, Some(&rewrite), &mut out);
            let s = std::str::from_utf8(&out).unwrap();
            assert!(s.starts_with(line), "{s}");
        }
    }

    /// A value with a query of its own made two: /login?next=home?a=1.
    /// The value's query comes first, then the request's after a '&';
    /// either alone stays as it is.
    #[test]
    fn test_url_rewrite_joins_a_value_query_and_the_request_query() {
        let mut out = Vec::new();
        // A prefix value's rule matched "/old" on its request path; a
        // full-path value has none.
        for (value, prefix_of, target, want) in [
            (
                "/login?next=home",
                None,
                "/old?a=1&b",
                "/login?next=home&a=1&b",
            ),
            ("/login?next=home", None, "/old", "/login?next=home"),
            ("/login?next=home", None, "/old?", "/login?next=home"),
            ("/login?", None, "/old?a=1", "/login?a=1"),
            (
                "/new?src=gw",
                Some("/old/x"),
                "/old/x?a=1",
                "/new/x?src=gw&a=1",
            ),
            ("/?src=gw", Some("/old/x"), "/old/x", "/x?src=gw"),
            // A value's query already ending in '&' takes the request's
            // as is: it made /login?next=home&&a=1.
            (
                "/login?next=home&",
                None,
                "/old?a=1",
                "/login?next=home&a=1",
            ),
            (
                "/new?src=gw&",
                Some("/old/x"),
                "/old/x?a=1",
                "/new/x?src=gw&a=1",
            ),
        ] {
            let path = match prefix_of {
                None => URLRewritePath::ReplaceFullPath(value.into()),
                Some(_) => URLRewritePath::ReplacePrefixMatch(value.into()),
            };
            let filters = [HttpFilter::URLRewrite {
                hostname: None,
                path: Some(path),
            }];
            let rewrite = extract_url_rewrite(&filters, "/old", prefix_of.unwrap_or("/old"));
            let head = format!("GET {target} HTTP/1.1\r\nHost: h\r\n\r\n");
            apply_request_header_modifications(head.as_bytes(), None, rewrite.as_ref(), &mut out);
            let s = std::str::from_utf8(&out).unwrap();
            assert!(s.starts_with(&format!("GET {want} HTTP/1.1\r\n")), "{s}");
        }
    }

    #[test]
    fn test_apply_modifications_host_rewrite() {
        let headers = b"GET / HTTP/1.1\r\nHost: original.example.com\r\n\r\n";
        let rewrite = URLRewrite {
            hostname: Some("rewritten.example.com"),
            path: None,
        };
        let mut result = Vec::new();
        apply_request_header_modifications(headers, None, Some(&rewrite), &mut result);
        let result_str = std::str::from_utf8(&result).unwrap();

        assert!(result_str.contains("Host: rewritten.example.com"));
        assert!(!result_str.contains("original.example.com"));
    }

    #[test]
    fn test_apply_modifications_combined() {
        let headers = b"GET /old HTTP/1.1\r\nHost: old.com\r\nUser-Agent: test\r\n\r\n";
        let rewrite = URLRewrite {
            hostname: Some("new.com"),
            path: Some(RewrittenPath::from_value("/new")),
        };
        let mods = HeaderModifications {
            add: &[HttpHeader {
                name: "X-Added".to_string(),
                value: "yes".to_string(),
            }],
            set: &[],
            remove: &["User-Agent".to_string()],
        };
        let mut result = Vec::new();
        apply_request_header_modifications(headers, Some(&mods), Some(&rewrite), &mut result);
        let result_str = std::str::from_utf8(&result).unwrap();

        assert!(result_str.starts_with("GET /new HTTP/1.1\r\n"));
        assert!(result_str.contains("Host: new.com"));
        assert!(result_str.contains("X-Added: yes"));
        assert!(!result_str.contains("User-Agent"));
    }

    #[test]
    fn test_apply_modifications_no_mods_passthrough() {
        let headers = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let mut result = Vec::new();
        apply_request_header_modifications(headers, None, None, &mut result);
        let result_str = std::str::from_utf8(&result).unwrap();
        assert!(result_str.starts_with("GET / HTTP/1.1\r\n"));
        assert!(result_str.contains("Host: example.com"));
    }

    #[test]
    fn test_apply_response_header_mods_add() {
        let headers = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n";
        let mods = HeaderModifications {
            add: &[HttpHeader {
                name: "X-Custom".to_string(),
                value: "added".to_string(),
            }],
            set: &[],
            remove: &[],
        };
        let mut result = Vec::new();
        apply_response_header_mods(headers, &mods, &mut result);
        let result_str = std::str::from_utf8(&result).unwrap();
        assert!(result_str.contains("X-Custom: added"));
    }

    #[test]
    fn test_apply_response_header_mods_remove() {
        let headers = b"HTTP/1.1 200 OK\r\nX-Internal: secret\r\nContent-Length: 5\r\n\r\n";
        let mods = HeaderModifications {
            add: &[],
            set: &[],
            remove: &["X-Internal".to_string()],
        };
        let mut result = Vec::new();
        apply_response_header_mods(headers, &mods, &mut result);
        let result_str = std::str::from_utf8(&result).unwrap();
        assert!(!result_str.contains("X-Internal"));
        assert!(result_str.contains("Content-Length: 5"));
    }

    #[test]
    fn test_header_mods_pass_a_non_utf8_name_through() {
        let mods = HeaderModifications {
            add: &[],
            set: &[HttpHeader {
                name: "X\u{ff}".to_string(),
                value: "v".to_string(),
            }],
            remove: &["X\u{ff}".to_string()],
        };
        let mut out = Vec::new();
        apply_response_header_mods(b"HTTP/1.1 200 OK\r\nX\xff: 1\r\n\r\n", &mods, &mut out);
        assert_eq!(out, b"HTTP/1.1 200 OK\r\nX\xff: 1\r\nX\xc3\xbf: v\r\n\r\n");
    }

    #[test]
    fn test_apply_response_header_mods_set() {
        let headers = b"HTTP/1.1 200 OK\r\nServer: old-server\r\n\r\n";
        let mods = HeaderModifications {
            add: &[],
            set: &[HttpHeader {
                name: "Server".to_string(),
                value: "portail".to_string(),
            }],
            remove: &[],
        };
        let mut result = Vec::new();
        apply_response_header_mods(headers, &mods, &mut result);
        let result_str = std::str::from_utf8(&result).unwrap();
        assert!(result_str.contains("Server: portail"));
        assert!(!result_str.contains("old-server"));
    }
}
