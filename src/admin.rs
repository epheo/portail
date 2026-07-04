//! Management endpoint: liveness + readiness probes + metrics scrape.
//!
//! Serves a tiny HTTP/1.1 responder on a dedicated management port.
//! `GET /metrics` returns the process metrics in Prometheus text format.
//! `GET /livez` is always `200` once the process is up: it answers "should
//! kubelet restart this process", nothing more. A livenessProbe must point
//! here — never at a data-plane port or Gateway VIP: kubelet dials those
//! from the host netns, the one path with no delivery guarantee to a
//! localnet/UDN VIP, and a restart cannot fix fabric conditions anyway
//! (VIP reachability belongs to blackbox monitoring, from a client vantage).
//! Any other path returns the readiness state — `200` while the data plane
//! has live accept loops on this instance's listener ports, `503` otherwise.
//!
//! Wired by the operator as the data-plane pod's `readinessProbe` (default
//! path) and `livenessProbe` (`/livez`), so traffic gates on real binds while
//! restarts only trigger on a genuinely dead process — and the Gateway
//! `Programmed` condition (derived from readiness) stays meaningful.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::logging::{info, warn};

const READY_200: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nready";
const NOT_READY_503: &[u8] =
    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot ready";
const LIVE_200: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nlive";

/// Extract the request-target from the request line. Only the request line is
/// inspected — this is a dedicated management port, not a router. Accepts any
/// method and any sub-path fragment boundary ("/metrics?..." etc.).
fn request_path(buf: &[u8]) -> &[u8] {
    let line = buf.split(|&b| b == b'\r').next().unwrap_or(buf);
    let mut parts = line.split(|&b| b == b' ');
    let _method = parts.next();
    parts.next().unwrap_or(b"")
}

fn is_metrics_request(buf: &[u8]) -> bool {
    let path = request_path(buf);
    path == b"/metrics" || path.starts_with(b"/metrics?")
}

fn is_livez_request(buf: &[u8]) -> bool {
    let path = request_path(buf);
    path == b"/livez" || path.starts_with(b"/livez?")
}

fn metrics_response(ready: bool) -> Vec<u8> {
    let body = crate::metrics::render(ready);
    let mut resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    resp.extend_from_slice(body.as_bytes());
    resp
}

/// Run the management HTTP server until shutdown: `/metrics` scrapes, any
/// other path is the readiness probe.
pub async fn serve(port: u16, ready: Arc<AtomicBool>, shutdown: CancellationToken) {
    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            warn!("Admin endpoint failed to bind :{}: {}", port, e);
            return;
        }
    };
    info!(
        "Admin endpoint listening on :{} (/livez, /readyz, /metrics)",
        port
    );

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            accept = listener.accept() => {
                let (mut stream, _peer) = match accept {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let is_ready = ready.load(Ordering::Acquire);
                tokio::spawn(async move {
                    // Best-effort: read the request line/headers so the peer
                    // doesn't see an RST, then reply. Bounded so a slow client
                    // can't pin the task.
                    let mut buf = [0u8; 1024];
                    let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
                        .await
                        .ok()
                        .and_then(|r| r.ok())
                        .unwrap_or(0);
                    let resp: Vec<u8> = if is_metrics_request(&buf[..n]) {
                        metrics_response(is_ready)
                    } else if is_livez_request(&buf[..n]) {
                        // Liveness: the fact this handler ran IS the answer.
                        LIVE_200.to_vec()
                    } else if is_ready {
                        READY_200.to_vec()
                    } else {
                        NOT_READY_503.to_vec()
                    };
                    let _ = stream.write_all(&resp).await;
                    let _ = stream.flush().await;
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_path_dispatch() {
        assert!(is_metrics_request(
            b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n"
        ));
        assert!(is_metrics_request(b"GET /metrics?x=1 HTTP/1.1\r\n\r\n"));
        // Everything else keeps readiness semantics.
        assert!(!is_metrics_request(b"GET /readyz HTTP/1.1\r\n\r\n"));
        assert!(!is_metrics_request(b"GET / HTTP/1.1\r\n\r\n"));
        assert!(!is_metrics_request(b"GET /metricsx HTTP/1.1\r\n\r\n"));
        assert!(!is_metrics_request(b""));
    }

    #[test]
    fn livez_path_dispatch() {
        assert!(is_livez_request(b"GET /livez HTTP/1.1\r\n\r\n"));
        assert!(is_livez_request(b"GET /livez?verbose HTTP/1.1\r\n\r\n"));
        // Liveness must not swallow readiness or metrics paths.
        assert!(!is_livez_request(b"GET /readyz HTTP/1.1\r\n\r\n"));
        assert!(!is_livez_request(b"GET /livezz HTTP/1.1\r\n\r\n"));
        assert!(!is_livez_request(b"GET /metrics HTTP/1.1\r\n\r\n"));
        assert!(!is_livez_request(b""));
    }
}
