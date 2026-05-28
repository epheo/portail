//! Minimal readiness endpoint for Kubernetes probes.
//!
//! Serves a tiny HTTP/1.1 responder on a dedicated management port. Returns
//! `200` once the data plane has bound this instance's listener ports (the
//! reconciler flips the shared flag after a successful bind), `503` otherwise.
//!
//! Wired by the operator as the data-plane pod's `readinessProbe`, so the
//! LoadBalancer Service only routes to pods that are actually serving — and
//! the Gateway `Programmed` condition (owned by the operator, derived from
//! AvailableReplicas) becomes meaningful.

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

/// Run the readiness HTTP server until shutdown. Any request path returns the
/// current readiness state — this is a dedicated probe port, not a router.
pub async fn serve_readiness(port: u16, ready: Arc<AtomicBool>, shutdown: CancellationToken) {
    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            warn!("Readiness endpoint failed to bind :{}: {}", port, e);
            return;
        }
    };
    info!("Readiness endpoint listening on :{}/readyz", port);

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
                    // Best-effort: consume the request line/headers so the peer
                    // doesn't see an RST, then reply. Bounded so a slow client
                    // can't pin the task.
                    let mut buf = [0u8; 1024];
                    let _ = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf)).await;
                    let resp = if is_ready { READY_200 } else { NOT_READY_503 };
                    let _ = stream.write_all(resp).await;
                    let _ = stream.flush().await;
                });
            }
        }
    }
}
