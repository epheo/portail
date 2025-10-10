//! Zero-copy HTTP request parsing for high-performance request processing.
//! All parsing functions use direct buffer references for maximum efficiency.

use anyhow::Result;
use crate::logging::debug;

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
    pub connection_type: ConnectionType,
    /// Raw header bytes for lazy header matching (zero-copy)
    pub header_data: &'a [u8],
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
        let mut http_version = HttpVersion::Http11;

        // Parse first line (method, path, version)
        let mut line_end = pos;
        while line_end < request_data.len() &&
              request_data[line_end] != b'\r' &&
              request_data[line_end] != b'\n' {
            line_end += 1;
        }

        if line_end > pos {
            // SAFETY: HTTP request line is ASCII by RFC 7230 - skip UTF-8 validation
            let first_line = unsafe { std::str::from_utf8_unchecked(&request_data[pos..line_end]) };
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

                    // SAFETY: Host header is ASCII by RFC 7230
                    host = Some(unsafe { std::str::from_utf8_unchecked(&line[value_start..normalized_end]) });

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

                    // SAFETY: Connection header is ASCII by RFC 7230
                    let value = unsafe { std::str::from_utf8_unchecked(&line[value_start..value_end]) };
                    if value.eq_ignore_ascii_case("close") {
                        raw_connection_type = ConnectionType::Close;
                    } else if value.eq_ignore_ascii_case("keep-alive") {
                        raw_connection_type = ConnectionType::KeepAlive;
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

        Ok(HttpRequestInfo {
            method: method.unwrap_or("GET"),
            host: host.unwrap_or("localhost"),
            path: path.unwrap_or("/"),
            connection_type,
            header_data,
        })
}
