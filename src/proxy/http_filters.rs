//! HTTP request/response modification filters.
//!
//! Applies header modifications, URL rewrites, and request mirroring
//! on raw byte slices — no intermediate String allocations on the request path.

use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::logging::warn;
use crate::routing::{HttpFilter, HttpHeader, URLRewritePath};

/// Borrowed header modification parameters — zero per-request allocation.
/// References point into the `Arc<Vec<...>>` fields in `HttpFilter` variants.
#[derive(Debug)]
pub struct HeaderModifications<'a> {
    pub add: &'a [HttpHeader],
    pub set: &'a [HttpHeader],
    pub remove: &'a [String],
}

#[derive(Debug, Clone)]
pub struct URLRewrite {
    pub hostname: Option<String>,
    pub path: Option<RewrittenPath>,
}

#[derive(Debug, Clone)]
pub enum RewrittenPath {
    Full(String),
    PrefixReplaced(String),
}

/// Assemble the outgoing request header block into `out` (reused scratch):
/// copies `raw` minus the parser-recorded hop-by-hop spans via bulk gap
/// copies, then appends X-Forwarded-For / X-Forwarded-Proto and, for upgrade
/// requests, the Connection: upgrade the stripped original carried.
/// One pass, no allocation after scratch warmup.
///
/// `raw` is the full header block ending with the blank-line CRLF.
pub(crate) fn assemble_forward_headers(
    raw: &[u8],
    spans: &crate::proxy::http_parser::StripSpans,
    upgrade_span: Option<(u32, u32)>,
    keep_upgrade: bool,
    client_ip: std::net::IpAddr,
    is_tls: bool,
    out: &mut Vec<u8>,
) {
    out.clear();
    out.reserve(raw.len() + 64);
    let terminator_start = raw.len().saturating_sub(2);
    let strip_upgrade = match upgrade_span {
        Some((s, e)) if !keep_upgrade => Some((s as usize, e as usize)),
        _ => None,
    };

    // Both sources are in ascending parse order; merge while copying gaps.
    let mut cursor = 0usize;
    let mut upgrade_pending = strip_upgrade;
    for (s, e) in spans.iter() {
        if let Some((us, ue)) = upgrade_pending {
            if us < s {
                out.extend_from_slice(&raw[cursor..us]);
                cursor = ue;
                upgrade_pending = None;
            }
        }
        out.extend_from_slice(&raw[cursor..s]);
        cursor = e;
    }
    if let Some((us, ue)) = upgrade_pending {
        out.extend_from_slice(&raw[cursor..us]);
        cursor = ue;
    }
    out.extend_from_slice(&raw[cursor..terminator_start]);
    append_forward_headers(out, client_ip, is_tls, keep_upgrade);
    out.extend_from_slice(b"\r\n");
}

/// Thorough per-line rewrite for the rare requests the span fast path cannot
/// handle: span overflow or Connection-nominated extra headers (RFC 7230
/// §6.1 requires stripping every header the Connection field names).
pub(crate) fn assemble_forward_headers_slow(
    raw: &[u8],
    keep_upgrade: bool,
    client_ip: std::net::IpAddr,
    is_tls: bool,
    out: &mut Vec<u8>,
) {
    // Pass 1: collect nominated tokens from every Connection header.
    let mut nominated: Vec<String> = Vec::new();
    for line in header_lines(raw) {
        if let Some(value) = header_value_if(line, b"connection") {
            for token in value.split(',') {
                let token = token.trim();
                if !token.is_empty() {
                    nominated.push(token.to_ascii_lowercase());
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
            out.extend_from_slice(line);
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
        let is_hop = is_hop_by_hop_request_header(name)
            || (name.eq_ignore_ascii_case(b"upgrade") && !keep_upgrade)
            || nominated
                .iter()
                .any(|n| name.eq_ignore_ascii_case(n.as_bytes()));
        if !is_hop {
            out.extend_from_slice(line);
            out.extend_from_slice(b"\r\n");
        }
    }
    append_forward_headers(out, client_ip, is_tls, keep_upgrade);
    out.extend_from_slice(b"\r\n");
}

fn append_forward_headers(
    out: &mut Vec<u8>,
    client_ip: std::net::IpAddr,
    is_tls: bool,
    keep_upgrade: bool,
) {
    use std::io::Write;
    // A fresh X-Forwarded-For line: multiple XFF headers concatenate as one
    // list, which avoids locating and splicing an existing header.
    out.extend_from_slice(b"X-Forwarded-For: ");
    let _ = write!(out, "{}", client_ip);
    out.extend_from_slice(b"\r\nX-Forwarded-Proto: ");
    out.extend_from_slice(if is_tls { b"https" } else { b"http" });
    out.extend_from_slice(b"\r\n");
    if keep_upgrade {
        out.extend_from_slice(b"Connection: upgrade\r\n");
    }
}

/// Copy response headers into `out` minus hop-by-hop lines (RFC 7230 §6.1).
/// The proxy owns the client-side Connection header: `client_close` appends
/// its own close announcement. The status line is copied untouched.
pub(crate) fn sanitize_response_headers(raw: &[u8], client_close: bool, out: &mut Vec<u8>) {
    out.clear();
    out.reserve(raw.len() + 24);
    let mut first = true;
    for line in header_lines(raw) {
        if first {
            first = false;
            out.extend_from_slice(line);
            out.extend_from_slice(b"\r\n");
            continue;
        }
        let keep = match memchr::memchr(b':', line) {
            Some(p) => !is_hop_by_hop_response_header(&line[..p]),
            None => true,
        };
        if keep {
            out.extend_from_slice(line);
            out.extend_from_slice(b"\r\n");
        }
    }
    if client_close {
        out.extend_from_slice(b"Connection: close\r\n");
    }
    out.extend_from_slice(b"\r\n");
}

/// First-byte dispatch keeps the per-line cost to one comparison for the
/// overwhelmingly common non-hop-by-hop headers.
#[inline]
fn is_hop_by_hop_request_header(name: &[u8]) -> bool {
    match name.first().map(|b| b.to_ascii_lowercase()) {
        Some(b'c') => name.eq_ignore_ascii_case(b"connection"),
        Some(b'k') => name.eq_ignore_ascii_case(b"keep-alive"),
        Some(b'p') => name.eq_ignore_ascii_case(b"proxy-connection"),
        Some(b't') => name.eq_ignore_ascii_case(b"te") || name.eq_ignore_ascii_case(b"trailer"),
        Some(b'e') => name.eq_ignore_ascii_case(b"expect"),
        _ => false,
    }
}

#[inline]
fn is_hop_by_hop_response_header(name: &[u8]) -> bool {
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
fn header_lines(raw: &[u8]) -> impl Iterator<Item = &[u8]> {
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
/// Avoids double-slash: `/` + `/three` → `/three`, not `//three`.
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

/// Extract URL rewrite parameters from rule filters.
pub fn extract_url_rewrite(
    filters: &[HttpFilter],
    rule_path: &str,
    request_path: &str,
) -> Option<URLRewrite> {
    for filter in filters {
        if let HttpFilter::URLRewrite { hostname, path } = filter {
            let rewritten_path = path.as_ref().map(|p| match p {
                URLRewritePath::ReplaceFullPath(value) => RewrittenPath::Full(value.clone()),
                URLRewritePath::ReplacePrefixMatch(value) => RewrittenPath::PrefixReplaced(
                    join_prefix_path(value, prefix_remainder(rule_path, request_path)),
                ),
            });
            return Some(URLRewrite {
                hostname: hostname.clone(),
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

/// Fire-and-forget mirror dispatch with percentage-based filtering and timeout.
///
/// Iterates rule filters looking for `RequestMirror` variants.
/// For each target, rolls a random number to decide whether to mirror based on
/// the configured percentage. Spawned tasks are bounded by a total 5s timeout
/// (2s connect + 3s write) to prevent leaked tasks from slow/dead backends,
/// and by `MIRROR_PERMITS` in flight.
pub(crate) fn dispatch_mirrors(filters: &[HttpFilter], data: &[u8]) {
    for filter in filters {
        let (addr, percent) = match filter {
            HttpFilter::RequestMirror {
                backend_addr,
                mirror_percent,
            } => (*backend_addr, *mirror_percent),
            _ => continue,
        };

        // Probabilistic dispatch: skip if random roll exceeds configured percent
        if percent == 0 {
            continue;
        }
        if percent < 100 && fastrand::u32(0..100) >= percent {
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
    header_lines: &[u8],
    mods: Option<&HeaderModifications<'_>>,
    rewrite_hostname: Option<&str>,
    out: &mut Vec<u8>,
) {
    // Track which "set" headers were applied (matched existing headers)
    let mut set_applied: Vec<bool> = mods.map(|m| vec![false; m.set.len()]).unwrap_or_default();

    let mut pos = 0;
    while pos < header_lines.len() {
        let line_start = pos;
        while pos < header_lines.len() && header_lines[pos] != b'\r' && header_lines[pos] != b'\n' {
            pos += 1;
        }
        let line = &header_lines[line_start..pos];

        // Skip past CRLF
        if pos < header_lines.len() && header_lines[pos] == b'\r' {
            pos += 1;
        }
        if pos < header_lines.len() && header_lines[pos] == b'\n' {
            pos += 1;
        }

        if line.is_empty() {
            continue;
        }

        // Find colon to extract header name
        let colon_pos = match line.iter().position(|&b| b == b':') {
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
                continue;
            }
        }

        if let Some(mods) = mods {
            let name_str = match std::str::from_utf8(name) {
                Ok(s) => s,
                Err(_) => {
                    // Non-UTF8 header name: pass through unmodified
                    out.extend_from_slice(line);
                    out.extend_from_slice(b"\r\n");
                    continue;
                }
            };

            if mods.remove.iter().any(|r| r.eq_ignore_ascii_case(name_str)) {
                continue;
            }
            if let Some((idx, set_header)) = mods
                .set
                .iter()
                .enumerate()
                .find(|(_, h)| h.name.eq_ignore_ascii_case(name_str))
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
fn split_first_line(header_region: &[u8]) -> (&[u8], &[u8]) {
    let mut pos = 0;
    while pos < header_region.len() && header_region[pos] != b'\r' && header_region[pos] != b'\n' {
        pos += 1;
    }
    let first_line = &header_region[..pos];
    // Skip CRLF
    if pos < header_region.len() && header_region[pos] == b'\r' {
        pos += 1;
    }
    if pos < header_region.len() && header_region[pos] == b'\n' {
        pos += 1;
    }
    (first_line, &header_region[pos..])
}

/// Apply request header modifications only — returns modified headers (including trailing \r\n\r\n).
/// Body bytes are NOT included; the caller sends them separately from the original buffer.
/// This keeps body handling zero-copy on the filter path.
pub(crate) fn apply_request_header_modifications(
    header_region: &[u8],
    header_mods: Option<&HeaderModifications<'_>>,
    url_rewrite: Option<&URLRewrite>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(header_region.len() + 256);
    let rewrite_hostname = url_rewrite.and_then(|r| r.hostname.as_deref());

    let (first_line, rest) = split_first_line(header_region);

    // Request line: rewrite path if needed
    if let Some(rewrite) = url_rewrite {
        if let Some(ref rewritten) = rewrite.path {
            let new_path = match rewritten {
                RewrittenPath::Full(p) => p.as_bytes(),
                RewrittenPath::PrefixReplaced(p) => p.as_bytes(),
            };
            if let Some(sp1) = first_line.iter().position(|&b| b == b' ') {
                if let Some(sp2) = first_line[sp1 + 1..].iter().position(|&b| b == b' ') {
                    out.extend_from_slice(&first_line[..sp1 + 1]);
                    out.extend_from_slice(new_path);
                    out.extend_from_slice(&first_line[sp1 + 1 + sp2..]);
                    out.extend_from_slice(b"\r\n");

                    apply_header_mods_inner(rest, header_mods, rewrite_hostname, &mut out);
                    return out;
                }
            }
        }
    }

    // No path rewrite — pass first line through
    out.extend_from_slice(first_line);
    out.extend_from_slice(b"\r\n");

    apply_header_mods_inner(rest, header_mods, rewrite_hostname, &mut out);
    out
}

/// Apply response header modifications to buffered response headers.
/// Returns the modified header region (including trailing \r\n\r\n).
/// Operates on raw bytes — no intermediate String allocations.
pub(crate) fn apply_response_header_mods(
    headers: &[u8],
    mods: &HeaderModifications<'_>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(headers.len() + 256);

    let (status_line, rest) = split_first_line(headers);

    // Status line: pass through unchanged
    out.extend_from_slice(status_line);
    out.extend_from_slice(b"\r\n");

    apply_header_mods_inner(rest, Some(mods), None, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed_spans(req: &[u8]) -> crate::proxy::http_parser::HttpRequestInfo<'_> {
        crate::proxy::http_parser::extract_routing_info(req).unwrap()
    }

    #[test]
    fn test_assemble_strips_hop_by_hop_and_appends_forwarding() {
        let req = b"GET /a HTTP/1.1\r\nHost: h\r\nConnection: keep-alive\r\nKeep-Alive: timeout=5\r\nProxy-Connection: keep-alive\r\nTE: trailers\r\nAccept: */*\r\n\r\n";
        let info = parsed_spans(req);
        assert!(!info.strip_spans.needs_slow_path);
        assert_eq!(info.strip_spans.len(), 4);

        let mut out = Vec::new();
        assemble_forward_headers(
            req,
            &info.strip_spans,
            info.upgrade_span,
            false,
            "10.1.2.3".parse().unwrap(),
            false,
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
            &info.strip_spans,
            info.upgrade_span,
            true,
            "10.1.2.3".parse().unwrap(),
            true,
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
            &info.strip_spans,
            info.upgrade_span,
            false,
            "10.1.2.3".parse().unwrap(),
            false,
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
        assemble_forward_headers_slow(req, false, "10.1.2.3".parse().unwrap(), false, &mut out);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(!s.to_ascii_lowercase().contains("x-internal"));
        assert!(!s.to_ascii_lowercase().contains("connection:"));
        assert!(s.contains("Accept: */*\r\n"));
        assert!(s.ends_with("\r\n\r\n"));
    }

    #[test]
    fn test_sanitize_response_headers() {
        let resp =
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\nKeep-Alive: timeout=5\r\nServer: x\r\n\r\n";
        let mut out = Vec::new();
        sanitize_response_headers(resp, false, &mut out);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(s.contains("Content-Length: 2\r\n"));
        assert!(s.contains("Server: x\r\n"));
        assert!(!s.to_ascii_lowercase().contains("connection:"));
        assert!(!s.to_ascii_lowercase().contains("keep-alive"));
        assert!(s.ends_with("\r\n\r\n"));

        sanitize_response_headers(resp, true, &mut out);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.ends_with("Connection: close\r\n\r\n"));
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
        let rewrite = extract_url_rewrite(
            &[HttpFilter::URLRewrite {
                hostname: None,
                path: Some(URLRewritePath::ReplacePrefixMatch("/new".to_string())),
            }],
            r"^/a$|^/much-longer-alternative$",
            "/a",
        )
        .unwrap();
        assert!(matches!(rewrite.path, Some(RewrittenPath::PrefixReplaced(ref p)) if p == "/new"));
    }

    #[test]
    fn test_apply_modifications_path_rewrite() {
        let headers = b"GET /old/path HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let rewrite = URLRewrite {
            hostname: None,
            path: Some(RewrittenPath::Full("/new/path".to_string())),
        };
        let result = apply_request_header_modifications(headers, None, Some(&rewrite));
        let result_str = std::str::from_utf8(&result).unwrap();

        assert!(result_str.starts_with("GET /new/path HTTP/1.1\r\n"));
        assert!(result_str.contains("Host: example.com"));
    }

    #[test]
    fn test_apply_modifications_host_rewrite() {
        let headers = b"GET / HTTP/1.1\r\nHost: original.example.com\r\n\r\n";
        let rewrite = URLRewrite {
            hostname: Some("rewritten.example.com".to_string()),
            path: None,
        };
        let result = apply_request_header_modifications(headers, None, Some(&rewrite));
        let result_str = std::str::from_utf8(&result).unwrap();

        assert!(result_str.contains("Host: rewritten.example.com"));
        assert!(!result_str.contains("original.example.com"));
    }

    #[test]
    fn test_apply_modifications_combined() {
        let headers = b"GET /old HTTP/1.1\r\nHost: old.com\r\nUser-Agent: test\r\n\r\n";
        let rewrite = URLRewrite {
            hostname: Some("new.com".to_string()),
            path: Some(RewrittenPath::Full("/new".to_string())),
        };
        let mods = HeaderModifications {
            add: &[HttpHeader {
                name: "X-Added".to_string(),
                value: "yes".to_string(),
            }],
            set: &[],
            remove: &["User-Agent".to_string()],
        };
        let result = apply_request_header_modifications(headers, Some(&mods), Some(&rewrite));
        let result_str = std::str::from_utf8(&result).unwrap();

        assert!(result_str.starts_with("GET /new HTTP/1.1\r\n"));
        assert!(result_str.contains("Host: new.com"));
        assert!(result_str.contains("X-Added: yes"));
        assert!(!result_str.contains("User-Agent"));
    }

    #[test]
    fn test_apply_modifications_no_mods_passthrough() {
        let headers = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let result = apply_request_header_modifications(headers, None, None);
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
        let result = apply_response_header_mods(headers, &mods);
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
        let result = apply_response_header_mods(headers, &mods);
        let result_str = std::str::from_utf8(&result).unwrap();
        assert!(!result_str.contains("X-Internal"));
        assert!(result_str.contains("Content-Length: 5"));
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
        let result = apply_response_header_mods(headers, &mods);
        let result_str = std::str::from_utf8(&result).unwrap();
        assert!(result_str.contains("Server: portail"));
        assert!(!result_str.contains("old-server"));
    }
}
