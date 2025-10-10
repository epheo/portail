//! HTTP request/response proxying with keepalive and backend connection pooling.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use arc_swap::ArcSwap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use anyhow::Result;

use crate::backend_pool::BackendPool;
use crate::request_processor::{self, ProcessingDecision};
use crate::routing::{BackendSelector, RouteTable};

pub async fn handle_http_connection(
    mut client: TcpStream,
    buf: &mut Vec<u8>,
    initial_bytes: usize,
    initial_backend_addr: SocketAddr,
    initial_keepalive: bool,
    server_port: u16,
    routes: Arc<ArcSwap<RouteTable>>,
    pool: Arc<BackendPool>,
    selector: &mut BackendSelector,
    keep_alive_timeout: Duration,
) -> Result<()> {
    let mut backend_addr = initial_backend_addr;
    let mut keepalive = initial_keepalive;
    let mut request_bytes = initial_bytes;

    loop {
        let mut backend = pool.acquire(backend_addr).await?;

        backend.write_all(&buf[..request_bytes]).await?;

        let response_complete = forward_http_response(&mut backend, &mut client, buf).await?;

        if response_complete && keepalive {
            pool.release(backend_addr, backend);
        }

        if !keepalive {
            return Ok(());
        }

        // Wait for next request with idle timeout to prevent slow-client DoS
        request_bytes = match tokio::time::timeout(keep_alive_timeout, client.read(buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => return Ok(()), // Idle timeout — close gracefully
        };
        if request_bytes == 0 {
            return Ok(());
        }

        // Re-analyze for next request (host/path may differ)
        let decision = {
            let route_table = routes.load();
            request_processor::analyze_request(&route_table, selector, &buf[..request_bytes], server_port)?
        };

        match decision {
            ProcessingDecision::HttpForward { backend_addr: addr, keepalive: ka, .. } => {
                backend_addr = addr;
                keepalive = ka;
            }
            ProcessingDecision::HttpRedirect { status_code, location, keepalive: ka } => {
                send_redirect_response(&mut client, status_code, &location).await?;
                if !ka {
                    return Ok(());
                }
                continue;
            }
            ProcessingDecision::SendHttpError { error_code, close_connection } => {
                send_error_response(&mut client, error_code).await?;
                if close_connection {
                    return Ok(());
                }
                continue;
            }
            ProcessingDecision::TcpForward { .. } | ProcessingDecision::UdpForward { .. } | ProcessingDecision::CloseConnection => {
                return Ok(());
            }
        }
    }
}

/// Forward the backend's HTTP response to the client.
/// Returns true if the response was fully received (backend connection reusable).
async fn forward_http_response(
    backend: &mut TcpStream,
    client: &mut TcpStream,
    buf: &mut [u8],
) -> Result<bool> {
    let mut headers_parsed = false;
    let mut content_length: Option<usize> = None;
    let mut body_bytes_forwarded: usize = 0;
    let mut chunked = false;
    let mut header_buf = Vec::new();
    // Track trailing bytes across reads for chunked terminator detection.
    // The terminator "0\r\n\r\n" is 5 bytes and may span two reads.
    let mut chunk_trail = [0u8; 5];
    let mut chunk_trail_len: usize = 0;

    loop {
        let n = backend.read(buf).await?;
        if n == 0 {
            return Ok(false);
        }

        client.write_all(&buf[..n]).await?;

        if !headers_parsed {
            header_buf.extend_from_slice(&buf[..n]);

            if let Some(header_end) = find_header_end(&header_buf) {
                headers_parsed = true;
                let headers = &header_buf[..header_end];

                content_length = parse_content_length(headers);
                chunked = is_chunked_transfer(headers);

                let body_in_this_chunk = header_buf.len() - header_end;
                body_bytes_forwarded = body_in_this_chunk;

                if let Some(cl) = content_length {
                    if body_bytes_forwarded >= cl {
                        return Ok(true);
                    }
                }
                if is_no_body_status(headers) {
                    return Ok(true);
                }
                if chunked {
                    let body = &header_buf[header_end..];
                    update_chunk_trail(&mut chunk_trail, &mut chunk_trail_len, body);
                    if is_chunked_terminator(&chunk_trail, chunk_trail_len) {
                        return Ok(true);
                    }
                }
            }
        } else {
            body_bytes_forwarded += n;

            if let Some(cl) = content_length {
                if body_bytes_forwarded >= cl {
                    return Ok(true);
                }
            }
            if chunked {
                update_chunk_trail(&mut chunk_trail, &mut chunk_trail_len, &buf[..n]);
                if is_chunked_terminator(&chunk_trail, chunk_trail_len) {
                    return Ok(true);
                }
            }
        }
    }
}

// --- HTTP response parsing helpers ---

fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|pos| pos + 4)
}

/// Byte-level Content-Length parser — no UTF-8 conversion needed.
fn parse_content_length(headers: &[u8]) -> Option<usize> {
    const NAME: &[u8] = b"content-length:";

    let mut pos = 0;
    while pos + NAME.len() < headers.len() {
        if headers[pos..pos + NAME.len()].eq_ignore_ascii_case(NAME) {
            let mut start = pos + NAME.len();
            while start < headers.len() && (headers[start] == b' ' || headers[start] == b'\t') {
                start += 1;
            }
            let mut val: usize = 0;
            let mut found_digit = false;
            let mut i = start;
            while i < headers.len() && headers[i].is_ascii_digit() {
                val = val.wrapping_mul(10).wrapping_add((headers[i] - b'0') as usize);
                found_digit = true;
                i += 1;
            }
            if found_digit {
                return Some(val);
            }
        }
        // Advance to start of next line
        while pos < headers.len() && headers[pos] != b'\n' {
            pos += 1;
        }
        pos += 1;
    }
    None
}

/// Byte-level Transfer-Encoding check — no UTF-8 conversion needed.
fn is_chunked_transfer(headers: &[u8]) -> bool {
    const NAME: &[u8] = b"transfer-encoding:";
    const CHUNKED: &[u8] = b"chunked";

    let mut pos = 0;
    while pos + NAME.len() < headers.len() {
        if headers[pos..pos + NAME.len()].eq_ignore_ascii_case(NAME) {
            let mut start = pos + NAME.len();
            while start < headers.len() && (headers[start] == b' ' || headers[start] == b'\t') {
                start += 1;
            }
            let mut end = start;
            while end < headers.len() && headers[end] != b'\r' && headers[end] != b'\n' {
                end += 1;
            }
            while end > start && (headers[end - 1] == b' ' || headers[end - 1] == b'\t') {
                end -= 1;
            }
            if end - start == CHUNKED.len()
                && headers[start..end].eq_ignore_ascii_case(CHUNKED)
            {
                return true;
            }
        }
        while pos < headers.len() && headers[pos] != b'\n' {
            pos += 1;
        }
        pos += 1;
    }
    false
}

fn is_no_body_status(headers: &[u8]) -> bool {
    if headers.len() < 12 { return false; }
    let status = &headers[9..12];
    status == b"204" || status == b"304" || status[0] == b'1'
}

/// Keep the last 5 bytes across reads so chunked terminator "0\r\n\r\n" is detected
/// even when split across two reads.
#[inline]
fn update_chunk_trail(trail: &mut [u8; 5], trail_len: &mut usize, new_data: &[u8]) {
    if new_data.len() >= 5 {
        trail.copy_from_slice(&new_data[new_data.len() - 5..]);
        *trail_len = 5;
    } else {
        let total = (*trail_len + new_data.len()).min(5);
        let keep = total - new_data.len();
        if keep > 0 && *trail_len > keep {
            trail.copy_within((*trail_len - keep)..*trail_len, 0);
        }
        trail[keep..total].copy_from_slice(new_data);
        *trail_len = total;
    }
}

#[inline]
fn is_chunked_terminator(trail: &[u8; 5], trail_len: usize) -> bool {
    trail_len >= 5 && &trail[..5] == b"0\r\n\r\n"
}

async fn send_error_response(client: &mut TcpStream, error_code: u16) -> Result<()> {
    let (status_text, body) = match error_code {
        400 => ("Bad Request", "400 Bad Request"),
        404 => ("Not Found", "404 Not Found"),
        502 => ("Bad Gateway", "502 Bad Gateway"),
        503 => ("Service Unavailable", "503 Service Unavailable"),
        504 => ("Gateway Timeout", "504 Gateway Timeout"),
        _ => ("Internal Server Error", "500 Internal Server Error"),
    };

    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        error_code, status_text, body.len(), body
    );

    client.write_all(response.as_bytes()).await?;
    Ok(())
}

async fn send_redirect_response(client: &mut TcpStream, status_code: u16, location: &str) -> Result<()> {
    let status_text = match status_code {
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        _ => "Found",
    };

    let body = format!("{} {}", status_code, status_text);
    let response = format!(
        "HTTP/1.1 {} {}\r\nLocation: {}\r\nContent-Length: {}\r\n\r\n{}",
        status_code, status_text, location, body.len(), body
    );

    client.write_all(response.as_bytes()).await?;
    Ok(())
}
