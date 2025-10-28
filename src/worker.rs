//! Tokio async TCP worker — one task per accepted connection.
//!
//! HTTP request/response proxying with keepalive and backend connection pooling,
//! raw TCP bidirectional forwarding, TLS termination, and TLS passthrough.

use std::net::SocketAddr;
use std::sync::Arc;
use arc_swap::ArcSwap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use anyhow::Result;

use crate::backend_pool::BackendPool;
use crate::http_filters::{apply_request_modifications, apply_response_header_mods, dispatch_mirrors};
use crate::http_parser::find_header_end;
use crate::logging::{warn, info, debug};
use crate::request_processor::{self, HeaderModifications, HttpFilterData, ProcessingDecision};
use crate::routing::{BackendSelector, RouteTable};
use crate::tls::{self, Connection};

struct ConnectionState {
    server_port: u16,
    routes: Arc<ArcSwap<RouteTable>>,
    pool: Arc<BackendPool>,
    selector: BackendSelector,
}

/// Run an accept loop on a single listener, spawning a task per connection.
pub async fn run_worker(
    worker_id: usize,
    listener: TcpListener,
    server_port: u16,
    routes: Arc<ArcSwap<RouteTable>>,
    pool: Arc<BackendPool>,
    shutdown: CancellationToken,
    tls_acceptor: Option<Arc<TlsAcceptor>>,
    tls_passthrough: bool,
) {
    info!("Worker {} accepting on port {}", worker_id, server_port);

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!("Worker {} shutting down", worker_id);
                break;
            }
            result = listener.accept() => {
                match result {
                    Ok((tcp_stream, peer)) => {
                        let routes = routes.clone();
                        let pool = pool.clone();
                        let acceptor = tls_acceptor.clone();

                        if tls_passthrough {
                            tokio::spawn(async move {
                                if let Err(_e) = handle_tls_passthrough(tcp_stream, server_port, routes).await {
                                    debug!("TLS passthrough from {} closed: {}", peer, _e);
                                }
                            });
                        } else if let Some(acceptor) = acceptor {
                            tokio::spawn(async move {
                                match acceptor.accept(tcp_stream).await {
                                    Ok(tls_stream) => {
                                        let conn = Connection::Tls { inner: tls_stream };
                                        let state = ConnectionState {
                                            server_port,
                                            routes,
                                            pool,
                                            selector: BackendSelector::new(),
                                        };
                                        if let Err(_e) = handle_connection(conn, peer, state).await {
                                            debug!("TLS connection from {} closed: {}", peer, _e);
                                        }
                                    }
                                    Err(e) => {
                                        debug!("TLS handshake failed from {}: {}", peer, e);
                                    }
                                }
                            });
                        } else {
                            let state = ConnectionState {
                                server_port,
                                routes,
                                pool,
                                selector: BackendSelector::new(),
                            };
                            tokio::spawn(async move {
                                let conn = Connection::Plain { inner: tcp_stream };
                                if let Err(_e) = handle_connection(conn, peer, state).await {
                                    debug!("Connection from {} closed: {}", peer, _e);
                                }
                            });
                        }
                    }
                    Err(e) => {
                        warn!("Worker {} accept error: {}", worker_id, e);
                    }
                }
            }
        }
    }
}

async fn handle_connection(
    mut client: Connection,
    _peer: SocketAddr,
    mut state: ConnectionState,
) -> Result<()> {
    client.set_nodelay(true)?;

    let mut buf = vec![0u8; 65536];

    let mut n = client.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }

    loop {
        let decision = {
            let route_table = state.routes.load();
            request_processor::analyze_request(&route_table, &mut state.selector, &buf[..n], state.server_port)?
        };

        match decision {
            ProcessingDecision::TcpForward { backend_addr } => {
                return handle_tcp_connection(client, backend_addr, &buf[..n]).await;
            }
            ProcessingDecision::HttpForward { backend_addr, keepalive, filters, backend_timeout } => {
                let ka = handle_http_forward(
                    &mut client, &mut buf, n, backend_addr, keepalive, filters, backend_timeout, &mut state,
                ).await?;
                if !ka {
                    return Ok(());
                }
            }
            ProcessingDecision::HttpRedirect { status_code, location, keepalive } => {
                send_redirect_response(&mut client, status_code, &location).await?;
                if !keepalive {
                    return Ok(());
                }
            }
            ProcessingDecision::SendHttpError { error_code, close_connection } => {
                send_error_response(&mut client, error_code).await?;
                if close_connection {
                    return Ok(());
                }
            }
            ProcessingDecision::UdpForward { .. } | ProcessingDecision::CloseConnection => {
                return Ok(());
            }
        }

        n = client.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
    }
}

/// Handle HTTP forward requests in a keepalive loop.
/// Returns true if the connection should continue (keepalive), false to close.
async fn handle_http_forward(
    client: &mut Connection,
    buf: &mut [u8],
    initial_bytes: usize,
    initial_backend_addr: SocketAddr,
    initial_keepalive: bool,
    initial_filters: Option<Box<HttpFilterData>>,
    initial_backend_timeout: Option<std::time::Duration>,
    state: &mut ConnectionState,
) -> Result<bool> {
    let mut backend_addr = initial_backend_addr;
    let mut keepalive = initial_keepalive;
    let mut request_bytes = initial_bytes;
    let mut filter_data = initial_filters;
    let mut per_rule_timeout = initial_backend_timeout;

    loop {
        let timeout_dur = per_rule_timeout.unwrap_or(std::time::Duration::from_secs(30));
        let mut backend = match tokio::time::timeout(timeout_dur, state.pool.acquire(backend_addr)).await {
            Ok(result) => result?,
            Err(_) => {
                send_error_response(client, 504).await?;
                return Ok(false);
            }
        };

        if let Some(ref fd) = filter_data {
            let has_mods = fd.request_header_mods.is_some() || fd.url_rewrite.is_some();
            if has_mods {
                let modified = apply_request_modifications(
                    &buf[..request_bytes],
                    fd.request_header_mods.as_ref(),
                    fd.url_rewrite.as_ref(),
                );
                if !fd.mirror_addrs.is_empty() { dispatch_mirrors(&fd.mirror_addrs, &modified); }
                backend.write_all(&modified).await?;
            } else {
                if !fd.mirror_addrs.is_empty() { dispatch_mirrors(&fd.mirror_addrs, &buf[..request_bytes]); }
                backend.write_all(&buf[..request_bytes]).await?;
            }
        } else {
            backend.write_all(&buf[..request_bytes]).await?;
        }

        let resp_mods = filter_data.as_ref().and_then(|fd| fd.response_header_mods.as_ref());
        let response_complete = match tokio::time::timeout(
            timeout_dur,
            forward_http_response(&mut backend, client, buf, resp_mods),
        ).await {
            Ok(result) => result?,
            Err(_) => {
                send_error_response(client, 504).await?;
                return Ok(false);
            }
        };

        if response_complete && keepalive {
            state.pool.release(backend_addr, backend);
        }

        if !keepalive {
            return Ok(false);
        }

        request_bytes = client.read(buf).await?;
        if request_bytes == 0 {
            return Ok(false);
        }

        let decision = {
            let route_table = state.routes.load();
            request_processor::analyze_request(&route_table, &mut state.selector, &buf[..request_bytes], state.server_port)?
        };

        match decision {
            ProcessingDecision::HttpForward { backend_addr: addr, keepalive: ka, filters, backend_timeout: bt } => {
                backend_addr = addr;
                keepalive = ka;
                filter_data = filters;
                per_rule_timeout = bt;
            }
            ProcessingDecision::HttpRedirect { status_code, location, keepalive: ka } => {
                send_redirect_response(client, status_code, &location).await?;
                if !ka {
                    return Ok(false);
                }
                return Ok(true);
            }
            ProcessingDecision::SendHttpError { error_code, close_connection } => {
                send_error_response(client, error_code).await?;
                if close_connection {
                    return Ok(false);
                }
                return Ok(true);
            }
            ProcessingDecision::TcpForward { .. } | ProcessingDecision::UdpForward { .. } | ProcessingDecision::CloseConnection => {
                return Ok(false);
            }
        }
    }
}

/// Forward the backend's HTTP response to the client.
/// Returns true if the response was fully received (backend connection reusable).
///
/// Without mods (passthrough): writes each chunk to client immediately,
/// buffers headers only for completion tracking. Zero buffering delay.
/// With mods: buffers until headers complete, applies modifications, then writes.
async fn forward_http_response(
    backend: &mut TcpStream,
    client: &mut Connection,
    buf: &mut [u8],
    response_mods: Option<&HeaderModifications>,
) -> Result<bool> {
    let mut headers_parsed = false;
    let mut content_length: Option<usize> = None;
    let mut body_bytes_forwarded: usize = 0;
    let mut chunked = false;
    let mut header_buf = Vec::new();

    loop {
        let n = backend.read(buf).await?;
        if n == 0 {
            return Ok(false);
        }

        if !headers_parsed {
            // Passthrough: write to client before buffering for header parsing
            if response_mods.is_none() {
                client.write_all(&buf[..n]).await?;
            }

            header_buf.extend_from_slice(&buf[..n]);

            if let Some(header_end) = find_header_end(&header_buf) {
                headers_parsed = true;

                // With mods: apply modifications and write buffered data now
                if let Some(mods) = response_mods {
                    let modified = apply_response_header_mods(&header_buf[..header_end], mods);
                    client.write_all(&modified).await?;
                    if header_buf.len() > header_end {
                        client.write_all(&header_buf[header_end..]).await?;
                    }
                }

                let headers = &header_buf[..header_end];
                content_length = parse_content_length(headers);
                chunked = is_chunked_transfer(headers);
                body_bytes_forwarded = header_buf.len() - header_end;

                if let Some(cl) = content_length {
                    if body_bytes_forwarded >= cl {
                        return Ok(true);
                    }
                }
                if chunked && ends_with_chunked_terminator(&header_buf[header_end..]) {
                    return Ok(true);
                }
                if is_no_body_status(headers) {
                    return Ok(true);
                }
            }
        } else {
            body_bytes_forwarded += n;
            client.write_all(&buf[..n]).await?;

            if let Some(cl) = content_length {
                if body_bytes_forwarded >= cl {
                    return Ok(true);
                }
            }
            if chunked && buf[..n].ends_with(b"0\r\n\r\n") {
                return Ok(true);
            }
        }
    }
}

/// L4 TLS passthrough: peek ClientHello for SNI, then forward raw TCP to backend.
async fn handle_tls_passthrough(
    mut client: TcpStream,
    server_port: u16,
    routes: Arc<ArcSwap<RouteTable>>,
) -> Result<()> {
    let mut peek_buf = vec![0u8; 16384];
    let n = client.peek(&mut peek_buf).await?;
    if n == 0 {
        return Ok(());
    }

    let sni = tls::extract_sni(&peek_buf[..n]);
    let hostname = sni.as_deref().unwrap_or("");

    let route_table = routes.load();
    let backend_addr = route_table.resolve_tls_passthrough(hostname, server_port)
        .ok_or_else(|| anyhow::anyhow!("No TLS passthrough route for SNI '{}'", hostname))?;

    let mut backend = TcpStream::connect(backend_addr).await?;
    backend.set_nodelay(true)?;
    client.set_nodelay(true)?;

    tokio::io::copy_bidirectional(&mut client, &mut backend).await?;
    Ok(())
}

async fn handle_tcp_connection(
    mut client: Connection,
    backend_addr: SocketAddr,
    initial_data: &[u8],
) -> Result<()> {
    let mut backend = TcpStream::connect(backend_addr).await?;
    backend.set_nodelay(true)?;

    backend.write_all(initial_data).await?;

    tokio::io::copy_bidirectional(&mut client, &mut backend).await?;
    Ok(())
}

async fn send_redirect_response(client: &mut Connection, status_code: u16, location: &str) -> Result<()> {
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

async fn send_error_response(client: &mut Connection, error_code: u16) -> Result<()> {
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

// --- HTTP response parsing helpers ---

fn parse_content_length(headers: &[u8]) -> Option<usize> {
    crate::routing::find_header_value(headers, "content-length")?.parse().ok()
}

fn is_chunked_transfer(headers: &[u8]) -> bool {
    crate::routing::find_header_value(headers, "transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
}

fn is_no_body_status(headers: &[u8]) -> bool {
    if headers.len() < 12 { return false; }
    let status = &headers[9..12];
    status == b"204" || status == b"304" || status[0] == b'1'
}

fn ends_with_chunked_terminator(data: &[u8]) -> bool {
    data.ends_with(b"0\r\n\r\n")
}
