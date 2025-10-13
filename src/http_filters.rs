//! HTTP request/response modification filters.
//!
//! Applies header modifications, URL rewrites, and request mirroring
//! on raw byte slices — no intermediate String allocations on the request path.

use std::net::SocketAddr;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::request_processor::{HeaderModifications, URLRewrite, RewrittenPath};

pub(crate) fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|pos| pos + 4)
}

/// Fire-and-forget mirror dispatch. Response is discarded per Gateway API spec.
pub(crate) fn dispatch_mirrors(mirror_addrs: &[SocketAddr], data: &[u8]) {
    for &addr in mirror_addrs {
        let data = data.to_vec();
        tokio::spawn(async move {
            if let Ok(mut conn) = TcpStream::connect(addr).await {
                let _ = conn.write_all(&data).await;
            }
        });
    }
}

/// Apply request modifications: header mods and/or URL rewrite.
/// Operates on raw bytes — no intermediate String allocations.
pub(crate) fn apply_request_modifications(
    original: &[u8],
    header_mods: Option<&HeaderModifications>,
    url_rewrite: Option<&URLRewrite>,
) -> Vec<u8> {
    let header_end = find_header_end(original).unwrap_or(original.len());
    let header_region = &original[..header_end];
    let body = &original[header_end..];

    let mut out = Vec::with_capacity(original.len() + 256);
    let rewrite_hostname = url_rewrite.and_then(|r| r.hostname.as_deref());

    // Process line by line on raw bytes
    let mut pos = 0;
    let mut first_line = true;
    while pos < header_region.len() {
        let line_start = pos;
        // Find \r\n boundary
        while pos < header_region.len() && header_region[pos] != b'\r' && header_region[pos] != b'\n' {
            pos += 1;
        }
        let line = &header_region[line_start..pos];

        // Skip past CRLF
        if pos < header_region.len() && header_region[pos] == b'\r' { pos += 1; }
        if pos < header_region.len() && header_region[pos] == b'\n' { pos += 1; }

        if line.is_empty() {
            continue;
        }

        if first_line {
            first_line = false;
            // Request line: rewrite path if needed
            if let Some(rewrite) = url_rewrite {
                if let Some(ref rewritten) = rewrite.path {
                    let new_path = match rewritten {
                        RewrittenPath::Full(p) => p.as_bytes(),
                        RewrittenPath::PrefixReplaced(p) => p.as_bytes(),
                    };
                    // Find METHOD and VERSION by locating the two spaces
                    if let Some(sp1) = line.iter().position(|&b| b == b' ') {
                        if let Some(sp2) = line[sp1 + 1..].iter().position(|&b| b == b' ') {
                            out.extend_from_slice(&line[..sp1 + 1]);
                            out.extend_from_slice(new_path);
                            out.extend_from_slice(&line[sp1 + 1 + sp2..]);
                            out.extend_from_slice(b"\r\n");
                            continue;
                        }
                    }
                }
            }
            out.extend_from_slice(line);
            out.extend_from_slice(b"\r\n");
            continue;
        }

        // Header line: find colon to extract name
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

        if let Some(mods) = header_mods {
            // SAFETY: header names are ASCII per RFC 7230
            let name_str = unsafe { std::str::from_utf8_unchecked(name) };

            if mods.remove.iter().any(|r| r.eq_ignore_ascii_case(name_str)) {
                continue;
            }
            if let Some(set_header) = mods.set.iter().find(|h| h.name.eq_ignore_ascii_case(name_str)) {
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

    // Add headers
    if let Some(mods) = header_mods {
        for h in &mods.add {
            out.extend_from_slice(h.name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(h.value.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
    }

    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

/// Apply response header modifications to buffered response headers.
/// Returns the modified header region (including trailing \r\n\r\n).
pub(crate) fn apply_response_header_mods(headers: &[u8], mods: &HeaderModifications) -> Vec<u8> {
    let header_str = match std::str::from_utf8(headers) {
        Ok(s) => s,
        Err(_) => return headers.to_vec(),
    };

    let lines: Vec<&str> = header_str.lines().collect();
    if lines.is_empty() {
        return headers.to_vec();
    }

    let status_line = lines[0];
    let header_lines = &lines[1..];

    let mut result_headers: Vec<String> = Vec::with_capacity(header_lines.len() + mods.add.len() + mods.set.len());

    for line in header_lines {
        if line.is_empty() {
            continue;
        }
        let colon_pos = match line.find(':') {
            Some(p) => p,
            None => continue,
        };
        let name = &line[..colon_pos];

        if mods.remove.iter().any(|r| r.eq_ignore_ascii_case(name)) {
            continue;
        }

        if let Some(set_header) = mods.set.iter().find(|h| h.name.eq_ignore_ascii_case(name)) {
            result_headers.push(format!("{}: {}", set_header.name, set_header.value));
            continue;
        }

        result_headers.push(line.to_string());
    }

    for h in &mods.add {
        result_headers.push(format!("{}: {}", h.name, h.value));
    }

    let mut out = Vec::with_capacity(headers.len() + 256);
    out.extend_from_slice(status_line.as_bytes());
    out.extend_from_slice(b"\r\n");
    for h in &result_headers {
        out.extend_from_slice(h.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request_processor::{URLRewrite, RewrittenPath, HeaderModifications};
    use crate::routing::HttpHeader;

    #[test]
    fn test_apply_modifications_path_rewrite() {
        let request = b"GET /old/path HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let rewrite = URLRewrite {
            hostname: None,
            path: Some(RewrittenPath::Full("/new/path".to_string())),
        };
        let result = apply_request_modifications(request, None, Some(&rewrite));
        let result_str = std::str::from_utf8(&result).unwrap();

        assert!(result_str.starts_with("GET /new/path HTTP/1.1\r\n"));
        assert!(result_str.contains("Host: example.com"));
    }

    #[test]
    fn test_apply_modifications_host_rewrite() {
        let request = b"GET / HTTP/1.1\r\nHost: original.example.com\r\n\r\n";
        let rewrite = URLRewrite {
            hostname: Some("rewritten.example.com".to_string()),
            path: None,
        };
        let result = apply_request_modifications(request, None, Some(&rewrite));
        let result_str = std::str::from_utf8(&result).unwrap();

        assert!(result_str.contains("Host: rewritten.example.com"));
        assert!(!result_str.contains("original.example.com"));
    }

    #[test]
    fn test_apply_modifications_combined() {
        let request = b"GET /old HTTP/1.1\r\nHost: old.com\r\nUser-Agent: test\r\n\r\n";
        let rewrite = URLRewrite {
            hostname: Some("new.com".to_string()),
            path: Some(RewrittenPath::Full("/new".to_string())),
        };
        let mods = HeaderModifications {
            add: vec![HttpHeader { name: "X-Added".to_string(), value: "yes".to_string() }],
            set: vec![],
            remove: vec!["User-Agent".to_string()],
        };
        let result = apply_request_modifications(request, Some(&mods), Some(&rewrite));
        let result_str = std::str::from_utf8(&result).unwrap();

        assert!(result_str.starts_with("GET /new HTTP/1.1\r\n"));
        assert!(result_str.contains("Host: new.com"));
        assert!(result_str.contains("X-Added: yes"));
        assert!(!result_str.contains("User-Agent"));
    }

    #[test]
    fn test_apply_modifications_no_mods_passthrough() {
        let request = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let result = apply_request_modifications(request, None, None);
        let result_str = std::str::from_utf8(&result).unwrap();
        assert!(result_str.starts_with("GET / HTTP/1.1\r\n"));
        assert!(result_str.contains("Host: example.com"));
    }

    #[test]
    fn test_apply_response_header_mods_add() {
        let headers = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n";
        let mods = HeaderModifications {
            add: vec![HttpHeader { name: "X-Custom".to_string(), value: "added".to_string() }],
            set: vec![],
            remove: vec![],
        };
        let result = apply_response_header_mods(headers, &mods);
        let result_str = std::str::from_utf8(&result).unwrap();
        assert!(result_str.contains("X-Custom: added"));
    }

    #[test]
    fn test_apply_response_header_mods_remove() {
        let headers = b"HTTP/1.1 200 OK\r\nX-Internal: secret\r\nContent-Length: 5\r\n\r\n";
        let mods = HeaderModifications {
            add: vec![],
            set: vec![],
            remove: vec!["X-Internal".to_string()],
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
            add: vec![],
            set: vec![HttpHeader { name: "Server".to_string(), value: "uringress".to_string() }],
            remove: vec![],
        };
        let result = apply_response_header_mods(headers, &mods);
        let result_str = std::str::from_utf8(&result).unwrap();
        assert!(result_str.contains("Server: uringress"));
        assert!(!result_str.contains("old-server"));
    }
}
