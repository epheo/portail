//! HTTP request/response modification filters.
//!
//! Applies header modifications, URL rewrites, and request mirroring
//! on raw byte slices — no intermediate String allocations on the request path.

use std::net::SocketAddr;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::request_processor::{HeaderModifications, URLRewrite, RewrittenPath};

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

/// Apply request header modifications only — returns modified headers (including trailing \r\n\r\n).
/// Body bytes are NOT included; the caller sends them separately from the original buffer.
/// This keeps body handling zero-copy on the filter path.
pub(crate) fn apply_request_header_modifications(
    header_region: &[u8],
    header_mods: Option<&HeaderModifications>,
    url_rewrite: Option<&URLRewrite>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(header_region.len() + 256);
    let rewrite_hostname = url_rewrite.and_then(|r| r.hostname.as_deref());

    // Track which set headers were applied (matched existing headers)
    let mut set_applied: Vec<bool> = header_mods
        .map(|m| vec![false; m.set.len()])
        .unwrap_or_default();

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
            if let Some((idx, set_header)) = mods.set.iter().enumerate()
                .find(|(_, h)| h.name.eq_ignore_ascii_case(name_str)) {
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

    // Add headers from "set" that didn't match any existing header (set = replace OR add)
    if let Some(mods) = header_mods {
        for (idx, h) in mods.set.iter().enumerate() {
            if !set_applied[idx] {
                out.extend_from_slice(h.name.as_bytes());
                out.extend_from_slice(b": ");
                out.extend_from_slice(h.value.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
        }
        // Add headers from "add" (always appended)
        for h in mods.add.iter() {
            out.extend_from_slice(h.name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(h.value.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
    }

    out.extend_from_slice(b"\r\n");
    out
}


/// Apply response header modifications to buffered response headers.
/// Returns the modified header region (including trailing \r\n\r\n).
/// Operates on raw bytes — no intermediate String allocations.
pub(crate) fn apply_response_header_mods(headers: &[u8], mods: &HeaderModifications) -> Vec<u8> {
    let mut out = Vec::with_capacity(headers.len() + 256);
    let mut pos = 0;
    let mut first_line = true;
    let mut set_applied = vec![false; mods.set.len()];

    while pos < headers.len() {
        let line_start = pos;
        // Find end of line
        while pos < headers.len() && headers[pos] != b'\r' && headers[pos] != b'\n' {
            pos += 1;
        }
        let line = &headers[line_start..pos];

        // Skip past CRLF
        if pos < headers.len() && headers[pos] == b'\r' { pos += 1; }
        if pos < headers.len() && headers[pos] == b'\n' { pos += 1; }

        if line.is_empty() {
            continue;
        }

        if first_line {
            // Status line: pass through unchanged
            first_line = false;
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

        let name_str = match std::str::from_utf8(name) {
            Ok(s) => s,
            Err(_) => {
                out.extend_from_slice(line);
                out.extend_from_slice(b"\r\n");
                continue;
            }
        };

        if mods.remove.iter().any(|r| r.eq_ignore_ascii_case(name_str)) {
            continue;
        }

        if let Some((idx, set_header)) = mods.set.iter().enumerate()
            .find(|(_, h)| h.name.eq_ignore_ascii_case(name_str)) {
            set_applied[idx] = true;
            out.extend_from_slice(set_header.name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(set_header.value.as_bytes());
            out.extend_from_slice(b"\r\n");
            continue;
        }

        out.extend_from_slice(line);
        out.extend_from_slice(b"\r\n");
    }

    // Add "set" headers that didn't match any existing header (set = replace OR add)
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
            add: std::sync::Arc::new(vec![HttpHeader { name: "X-Added".to_string(), value: "yes".to_string() }]),
            set: std::sync::Arc::new(vec![]),
            remove: std::sync::Arc::new(vec!["User-Agent".to_string()]),
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
            add: std::sync::Arc::new(vec![HttpHeader { name: "X-Custom".to_string(), value: "added".to_string() }]),
            set: std::sync::Arc::new(vec![]),
            remove: std::sync::Arc::new(vec![]),
        };
        let result = apply_response_header_mods(headers, &mods);
        let result_str = std::str::from_utf8(&result).unwrap();
        assert!(result_str.contains("X-Custom: added"));
    }

    #[test]
    fn test_apply_response_header_mods_remove() {
        let headers = b"HTTP/1.1 200 OK\r\nX-Internal: secret\r\nContent-Length: 5\r\n\r\n";
        let mods = HeaderModifications {
            add: std::sync::Arc::new(vec![]),
            set: std::sync::Arc::new(vec![]),
            remove: std::sync::Arc::new(vec!["X-Internal".to_string()]),
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
            add: std::sync::Arc::new(vec![]),
            set: std::sync::Arc::new(vec![HttpHeader { name: "Server".to_string(), value: "portail".to_string() }]),
            remove: std::sync::Arc::new(vec![]),
        };
        let result = apply_response_header_mods(headers, &mods);
        let result_str = std::str::from_utf8(&result).unwrap();
        assert!(result_str.contains("Server: portail"));
        assert!(!result_str.contains("old-server"));
    }
}
