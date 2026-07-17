//! Zero-copy HTTP request parsing for high-performance request processing.
//! All parsing functions use direct buffer references for maximum efficiency.

use crate::logging::debug;
use anyhow::Result;

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

/// HTTP Connection header preference
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConnectionType {
    /// Explicit Connection: keep-alive
    KeepAlive,
    /// Explicit Connection: close
    Close,
    /// No Connection header - use HTTP version default
    Default,
}

/// HTTP version detected from request line
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HttpVersion {
    /// HTTP/1.0 - default connection: close
    Http10,
    /// HTTP/1.1 - default connection: keep-alive
    Http11,
}

pub const MAX_STRIP_SPANS: usize = 8;

/// Byte spans (start..end, CRLF inclusive) of hop-by-hop header lines,
/// recorded during the parser's existing single pass so forwarding can skip
/// them by bulk gap-copy — no second scan, no allocation. Overflow or
/// Connection-nominated extra headers divert to the thorough slow path.
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

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// HTTP request information extracted by unified parser (zero-copy)
/// Contains all fields needed for routing decisions and execution
#[derive(Debug, Clone)]
pub struct HttpRequestInfo<'a> {
    pub method: &'a str,
    pub host: &'a str,
    pub path: &'a str,
    pub query_string: &'a str,
    pub connection_type: ConnectionType,
    /// Raw header bytes for lazy header matching (zero-copy)
    pub header_data: &'a [u8],
    /// Content-Length of the request body (None if header absent)
    pub content_length: Option<usize>,
    /// Whether Transfer-Encoding: chunked is present
    pub is_chunked: bool,
    /// Whether this is a WebSocket/protocol upgrade request
    pub is_upgrade: bool,
    /// Set when the request carries headers that constitute a smuggling vector
    /// per RFC 7230 §3.3.3:
    /// - both `Content-Length` and `Transfer-Encoding: chunked` present,
    /// - multiple `Content-Length` lines with conflicting values, or
    /// - a `Transfer-Encoding` value other than a single, exact `chunked`
    ///   (e.g. `gzip, chunked`, `identity`, repeated TE lines): the proxy
    ///   would forward the TE header verbatim while framing the body
    ///   differently than the backend — the classic TE.CL desync.
    ///
    /// The caller MUST reject such requests with `400`.
    pub header_violation: bool,
    /// Hop-by-hop header lines to strip when forwarding (RFC 7230 §6.1).
    pub strip_spans: StripSpans,
    /// Span of the Upgrade header line; kept only for real upgrade requests.
    pub upgrade_span: Option<(u32, u32)>,
    /// Request carried `Expect: 100-continue`.
    pub expect_continue: bool,
}

/// Extract complete HTTP routing information with zero-copy parsing
/// Single-pass algorithm - parses method, host, path, headers in one scan
#[inline(always)]
pub fn extract_routing_info(request_data: &[u8]) -> Result<HttpRequestInfo<'_>> {
    debug!(
        "UNIFIED_PARSER: Processing HTTP request - content length: {}",
        request_data.len()
    );

    // Single-pass state machine parser
    let mut pos = 0;
    let mut method: Option<&str> = None;
    let mut path: Option<&str> = None;
    let mut host: Option<&str> = None;
    let mut raw_connection_type = ConnectionType::Default;
    let mut content_length: Option<usize> = None;
    let mut is_chunked = false;
    let mut has_upgrade_header = false;
    let mut connection_has_upgrade = false;
    let mut http_version = HttpVersion::Http11;
    // RFC 7230 §3.3.3 smuggling guards. We flag (but keep parsing) so the
    // request_processor can reject with a clean 400.
    let mut duplicate_content_length_conflict = false;
    let mut transfer_encoding_violation = false;
    let mut strip_spans = StripSpans::default();
    let mut upgrade_span: Option<(u32, u32)> = None;
    let mut expect_continue = false;

    // Parse first line (method, path, version)
    let mut line_end = pos;
    while line_end < request_data.len()
        && request_data[line_end] != b'\r'
        && request_data[line_end] != b'\n'
    {
        line_end += 1;
    }

    if line_end > pos {
        let first_line = match std::str::from_utf8(&request_data[pos..line_end]) {
            Ok(s) => s,
            Err(_) => return Err(anyhow::anyhow!("Non-UTF8 data in HTTP request line")),
        };
        let mut parts = first_line.split_whitespace();
        method = parts.next();
        path = parts.next();

        if line_end >= 8 && &request_data[line_end - 8..line_end] == b"HTTP/1.0" {
            http_version = HttpVersion::Http10;
        } else if line_end >= 8 && &request_data[line_end - 8..line_end] == b"HTTP/1.1" {
            http_version = HttpVersion::Http11;
        }
    }

    // Skip to headers (past CRLF)
    pos = line_end;
    if pos < request_data.len() && request_data[pos] == b'\r' {
        pos += 1;
    }
    if pos < request_data.len() && request_data[pos] == b'\n' {
        pos += 1;
    }

    let header_start = pos;

    // Parse headers in single pass
    while pos < request_data.len() {
        // Check for end of headers
        if pos + 1 < request_data.len() && &request_data[pos..pos + 2] == b"\r\n" {
            break;
        }

        let line_start = pos;
        let mut line_end = pos;
        while line_end < request_data.len()
            && request_data[line_end] != b'\r'
            && request_data[line_end] != b'\n'
        {
            line_end += 1;
        }

        // Header-line disposition, applied after the CRLF advance below
        // (spans must include the terminator).
        enum LineKind {
            Keep,
            Strip,
            Upgrade,
        }
        let mut line_kind = LineKind::Keep;

        if line_end > line_start {
            let line = &request_data[line_start..line_end];

            // Fast header matching using first few bytes
            if line.len() >= 5 && line[..5].eq_ignore_ascii_case(b"Host:") {
                let mut value_start = 5;
                while value_start < line.len() && line[value_start] == b' ' {
                    value_start += 1;
                }

                let mut value_end = line.len();
                while value_end > value_start
                    && (line[value_end - 1] == b' ' || line[value_end - 1] == b'\t')
                {
                    value_end -= 1;
                }

                // Find port separator and truncate for Gateway API compliance
                let normalized_end = line[value_start..value_end]
                    .iter()
                    .position(|&b| b == b':')
                    .map_or(value_end, |pos| value_start + pos);

                host = std::str::from_utf8(&line[value_start..normalized_end]).ok();
            } else if line.len() >= 15 && line[..15].eq_ignore_ascii_case(b"Content-Length:") {
                let mut vs = 15;
                while vs < line.len() && line[vs] == b' ' {
                    vs += 1;
                }
                if let Some(val) = std::str::from_utf8(&line[vs..])
                    .ok()
                    .and_then(|s| s.trim().parse::<usize>().ok())
                {
                    record_content_length(
                        val,
                        &mut content_length,
                        &mut duplicate_content_length_conflict,
                    );
                }
            } else if line.len() >= 18 && line[..18].eq_ignore_ascii_case(b"Transfer-Encoding:") {
                let mut vs = 18;
                while vs < line.len() && line[vs] == b' ' {
                    vs += 1;
                }
                // The only transfer coding this proxy understands is a single,
                // exact `chunked`. Anything else — `gzip, chunked`, `identity`,
                // a repeated TE line, a non-UTF8 value — is flagged: we cannot
                // frame such a body ourselves, so forwarding it would desync
                // our framing from the backend's.
                match std::str::from_utf8(&line[vs..]) {
                    Ok(val) if val.trim().eq_ignore_ascii_case("chunked") && !is_chunked => {
                        is_chunked = true;
                    }
                    _ => transfer_encoding_violation = true,
                }
            } else if line.len() >= 8 && line[..8].eq_ignore_ascii_case(b"Upgrade:") {
                has_upgrade_header = true;
                line_kind = LineKind::Upgrade;
            } else if line.len() >= 11 && line[..11].eq_ignore_ascii_case(b"Connection:") {
                line_kind = LineKind::Strip;
                let mut value_start = 11;
                while value_start < line.len() && line[value_start] == b' ' {
                    value_start += 1;
                }

                let mut value_end = line.len();
                while value_end > value_start
                    && (line[value_end - 1] == b' ' || line[value_end - 1] == b'\t')
                {
                    value_end -= 1;
                }

                if let Ok(value) = std::str::from_utf8(&line[value_start..value_end]) {
                    // Token list ("keep-alive, upgrade"): scan each. Only runs
                    // when a Connection header is present, on a short value.
                    for token in value.split(',') {
                        let token = token.trim();
                        if token.eq_ignore_ascii_case("close") {
                            raw_connection_type = ConnectionType::Close;
                        } else if token.eq_ignore_ascii_case("keep-alive") {
                            raw_connection_type = ConnectionType::KeepAlive;
                        } else if token.eq_ignore_ascii_case("upgrade") {
                            connection_has_upgrade = true;
                        } else if !token.is_empty() {
                            // Connection nominated an extra header to strip;
                            // the span fast path cannot honor that.
                            strip_spans.needs_slow_path = true;
                        }
                    }
                }
            } else if (line.len() >= 11 && line[..11].eq_ignore_ascii_case(b"Keep-Alive:"))
                || (line.len() >= 17 && line[..17].eq_ignore_ascii_case(b"Proxy-Connection:"))
                || (line.len() >= 3 && line[..3].eq_ignore_ascii_case(b"TE:"))
                || (line.len() >= 8 && line[..8].eq_ignore_ascii_case(b"Trailer:"))
            {
                line_kind = LineKind::Strip;
            } else if line.len() >= 7 && line[..7].eq_ignore_ascii_case(b"Expect:") {
                // The proxy answers 100-continue itself and strips the header;
                // other expectations are stripped and ignored (RFC 9110 lets
                // an intermediary absorb expectations it handles).
                line_kind = LineKind::Strip;
                if let Ok(value) = std::str::from_utf8(&line[7..]) {
                    if value.trim().eq_ignore_ascii_case("100-continue") {
                        expect_continue = true;
                    }
                }
            }
        }

        // Move to next line
        pos = line_end;
        if pos < request_data.len() && request_data[pos] == b'\r' {
            pos += 1;
        }
        if pos < request_data.len() && request_data[pos] == b'\n' {
            pos += 1;
        }
        match line_kind {
            LineKind::Keep => {}
            LineKind::Strip => strip_spans.push(line_start, pos),
            LineKind::Upgrade => upgrade_span = Some((line_start as u32, pos as u32)),
        }
    }

    // Apply HTTP version-aware connection defaults
    let connection_type = match raw_connection_type {
        ConnectionType::Close => ConnectionType::Close,
        ConnectionType::KeepAlive => ConnectionType::KeepAlive,
        ConnectionType::Default => match http_version {
            HttpVersion::Http10 => ConnectionType::Close,
            HttpVersion::Http11 => ConnectionType::KeepAlive,
        },
    };

    // header_data covers everything from header_start to current pos (end of headers)
    let header_data = &request_data[header_start..pos];

    // Handle absolute-form URIs (RFC 7230 §5.3.2):
    // "GET http://host/path HTTP/1.1" → extract "/path"
    let raw_path = path.unwrap_or("/");
    let raw_path = if raw_path.starts_with("http://") || raw_path.starts_with("https://") {
        // Find the path after scheme://authority
        match raw_path.find("://") {
            Some(scheme_end) => match raw_path[scheme_end + 3..].find('/') {
                Some(path_start) => &raw_path[scheme_end + 3 + path_start..],
                None => "/",
            },
            None => raw_path,
        }
    } else {
        raw_path
    };
    let (clean_path, query_string) = match raw_path.find('?') {
        Some(pos) => (&raw_path[..pos], &raw_path[pos + 1..]),
        None => (raw_path, ""),
    };

    // Per RFC 7230 §5.4: HTTP/1.1 requests MUST include a Host header.
    // HTTP/1.0 clients may omit it, so use empty string for routing to handle.
    let resolved_host = match host {
        Some(h) => h,
        None if http_version == HttpVersion::Http11 => {
            return Err(anyhow::anyhow!(
                "Missing required Host header in HTTP/1.1 request"
            ));
        }
        None => "",
    };

    // RFC 7230 §3.3.3: reject TE+CL coexistence, conflicting duplicate
    // Content-Length values, and unsupported Transfer-Encoding values — all
    // classic request-smuggling vectors.
    let header_violation = duplicate_content_length_conflict
        || transfer_encoding_violation
        || (content_length.is_some() && is_chunked);

    Ok(HttpRequestInfo {
        method: method.unwrap_or("GET"),
        host: resolved_host,
        path: clean_path,
        query_string,
        connection_type,
        header_data,
        content_length,
        is_chunked,
        // RFC 7230 6.7: an upgrade is only requested when Connection also
        // nominates it. A bare Upgrade header must not switch protocols.
        is_upgrade: has_upgrade_header && connection_has_upgrade,
        header_violation,
        strip_spans,
        upgrade_span,
        expect_continue,
    })
}

/// Record a parsed `Content-Length` value, flagging a conflict if a previous
/// value was different. Multiple identical CL lines are not flagged (RFC 7230
/// §3.3.2 allows them).
#[inline]
fn record_content_length(val: usize, current: &mut Option<usize>, conflict: &mut bool) {
    match *current {
        Some(prev) if prev != val => *conflict = true,
        Some(_) => {} // duplicate but equal — benign
        None => *current = Some(val),
    }
}
