//! HTTP request/response modification filters.
//!
//! Applies header modifications, URL rewrites, and request mirroring
//! on raw byte slices — no intermediate String allocations on the request path.

use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::logging::warn;
use crate::request_processor::{HeaderModifications, RewrittenPath, URLRewrite};
use crate::routing::HttpFilter;

/// Maximum request body size (bytes) that will be buffered for mirroring.
/// Bodies exceeding this are silently skipped (primary request unaffected).
/// Matches Envoy's default `per_connection_buffer_limit_bytes`.
pub(crate) const MIRROR_BODY_MAX: usize = 1024 * 1024; // 1 MB

/// Fire-and-forget mirror dispatch with percentage-based filtering and timeout.
///
/// Iterates rule filters looking for `RequestMirror` variants.
/// For each target, rolls a random number to decide whether to mirror based on
/// the configured percentage. Spawned tasks are bounded by a total 5s timeout
/// (2s connect + 3s write) to prevent leaked tasks from slow/dead backends.
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

        let data = data.to_vec();
        tokio::spawn(async move {
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
    use crate::request_processor::{HeaderModifications, RewrittenPath, URLRewrite};
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
