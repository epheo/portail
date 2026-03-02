//! Zero-copy HTTP request parsing for high-performance request processing.
//! All parsing functions use direct buffer references for maximum efficiency.

use anyhow::Result;
use crate::logging::debug;

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
}

/// Extract complete HTTP routing information with zero-copy parsing
/// Single-pass algorithm - parses method, host, path, headers in one scan
#[inline(always)]
pub fn extract_routing_info(
    request_data: &[u8],
) -> Result<HttpRequestInfo<'_>> {
        debug!("UNIFIED_PARSER: Processing HTTP request - content length: {}", request_data.len());

        // Single-pass state machine parser
        let mut pos = 0;
        let mut method: Option<&str> = None;
        let mut path: Option<&str> = None;
        let mut host: Option<&str> = None;
        let mut raw_connection_type = ConnectionType::Default;
        let mut content_length: Option<usize> = None;
        let mut is_chunked = false;
        let mut is_upgrade = false;
        let mut http_version = HttpVersion::Http11;

        // Parse first line (method, path, version)
        let mut line_end = pos;
        while line_end < request_data.len() &&
              request_data[line_end] != b'\r' &&
              request_data[line_end] != b'\n' {
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

            if line_end >= 8 && &request_data[line_end-8..line_end] == b"HTTP/1.0" {
                http_version = HttpVersion::Http10;
            } else if line_end >= 8 && &request_data[line_end-8..line_end] == b"HTTP/1.1" {
                http_version = HttpVersion::Http11;
            }
        }

        // Skip to headers (past CRLF)
        pos = line_end;
        if pos < request_data.len() && request_data[pos] == b'\r' { pos += 1; }
        if pos < request_data.len() && request_data[pos] == b'\n' { pos += 1; }

        let header_start = pos;

        // Parse headers in single pass
        while pos < request_data.len() {
            // Check for end of headers
            if pos + 1 < request_data.len() && &request_data[pos..pos+2] == b"\r\n" {
                break;
            }

            let line_start = pos;
            let mut line_end = pos;
            while line_end < request_data.len() &&
                  request_data[line_end] != b'\r' &&
                  request_data[line_end] != b'\n' {
                line_end += 1;
            }

            if line_end > line_start {
                let line = &request_data[line_start..line_end];

                // Fast header matching using first few bytes
                if line.len() >= 5 && line[..5].eq_ignore_ascii_case(b"Host:") {
                    let mut value_start = 5;
                    while value_start < line.len() && line[value_start] == b' ' {
                        value_start += 1;
                    }

                    let mut value_end = line.len();
                    while value_end > value_start &&
                          (line[value_end - 1] == b' ' || line[value_end - 1] == b'\t') {
                        value_end -= 1;
                    }

                    // Find port separator and truncate for Gateway API compliance
                    let normalized_end = line[value_start..value_end]
                        .iter()
                        .position(|&b| b == b':')
                        .map_or(value_end, |pos| value_start + pos);

                    host = std::str::from_utf8(&line[value_start..normalized_end]).ok();

                } else if line.len() >= 16 && line[..16].eq_ignore_ascii_case(b"Content-Length: ") {
                    if let Ok(val) = std::str::from_utf8(&line[16..]).ok()
                        .and_then(|s| s.trim().parse::<usize>().ok())
                        .ok_or(()) {
                        content_length = Some(val);
                    }
                } else if line.len() >= 15 && line[..15].eq_ignore_ascii_case(b"Content-Length:") {
                    let mut vs = 15;
                    while vs < line.len() && line[vs] == b' ' { vs += 1; }
                    if let Ok(val) = std::str::from_utf8(&line[vs..]).ok()
                        .and_then(|s| s.trim().parse::<usize>().ok())
                        .ok_or(()) {
                        content_length = Some(val);
                    }
                } else if line.len() >= 18 && line[..18].eq_ignore_ascii_case(b"Transfer-Encoding:") {
                    let mut vs = 18;
                    while vs < line.len() && line[vs] == b' ' { vs += 1; }
                    if let Ok(val) = std::str::from_utf8(&line[vs..]) {
                        if val.trim().eq_ignore_ascii_case("chunked") {
                            is_chunked = true;
                        }
                    }
                } else if line.len() >= 8 && line[..8].eq_ignore_ascii_case(b"Upgrade:") {
                    is_upgrade = true;
                } else if line.len() >= 11 && line[..11].eq_ignore_ascii_case(b"Connection:") {
                    let mut value_start = 11;
                    while value_start < line.len() && line[value_start] == b' ' {
                        value_start += 1;
                    }

                    let mut value_end = line.len();
                    while value_end > value_start &&
                          (line[value_end - 1] == b' ' || line[value_end - 1] == b'\t') {
                        value_end -= 1;
                    }

                    if let Ok(value) = std::str::from_utf8(&line[value_start..value_end]) {
                        if value.eq_ignore_ascii_case("close") {
                            raw_connection_type = ConnectionType::Close;
                        } else if value.eq_ignore_ascii_case("keep-alive") {
                            raw_connection_type = ConnectionType::KeepAlive;
                        }
                    }

                }
            }

            // Move to next line
            pos = line_end;
            if pos < request_data.len() && request_data[pos] == b'\r' { pos += 1; }
            if pos < request_data.len() && request_data[pos] == b'\n' { pos += 1; }
        }

        // Apply HTTP version-aware connection defaults
        let connection_type = match raw_connection_type {
            ConnectionType::Close => ConnectionType::Close,
            ConnectionType::KeepAlive => ConnectionType::KeepAlive,
            ConnectionType::Default => {
                match http_version {
                    HttpVersion::Http10 => ConnectionType::Close,
                    HttpVersion::Http11 => ConnectionType::KeepAlive,
                }
            }
        };

        // header_data covers everything from header_start to current pos (end of headers)
        let header_data = &request_data[header_start..pos];

        let raw_path = path.unwrap_or("/");
        let (clean_path, query_string) = match raw_path.find('?') {
            Some(pos) => (&raw_path[..pos], &raw_path[pos + 1..]),
            None => (raw_path, ""),
        };

        // Per RFC 7230 §5.4: HTTP/1.1 requests MUST include a Host header.
        // HTTP/1.0 clients may omit it, so use empty string for routing to handle.
        let resolved_host = match host {
            Some(h) => h,
            None if http_version == HttpVersion::Http11 => {
                return Err(anyhow::anyhow!("Missing required Host header in HTTP/1.1 request"));
            }
            None => "",
        };

        Ok(HttpRequestInfo {
            method: method.unwrap_or("GET"),
            host: resolved_host,
            path: clean_path,
            query_string,
            connection_type,
            header_data,
            content_length,
            is_chunked,
            is_upgrade,
        })
}
