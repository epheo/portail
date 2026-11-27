//! Opening a connection to a backend. The file the native h2 path grows:
//! what is here already needs no client and no connection state.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::AsyncWriteExt;

use crate::metrics::METRICS;
use crate::proxy::backend_pool::{connect_tcp, ConnectError};
use crate::proxy::tls::Connection;

/// Metrics label for a backend: the CONFIGURED name and target port
/// ("jellyfin.jellyfin:8096") — stable while STRICT_DNS re-resolution churns
/// the pod IPs behind it. Falls back to the socket address when a backend
/// was built without a name (tests, manual construction).
pub(crate) fn backend_metric_label(backend: &crate::routing::Backend) -> String {
    if backend.server_name.is_empty() {
        backend.socket_addr.to_string()
    } else {
        format!("{}:{}", backend.server_name, backend.socket_addr.port())
    }
}

/// A fresh plain connection to a proxied backend, its PROXY v2 header sent.
pub(crate) async fn connect_proxied(
    addr: SocketAddr,
    connect_timeout: Duration,
    keepalive: &socket2::TcpKeepalive,
    header: Vec<u8>,
) -> Result<(Connection, bool), ConnectError> {
    let mut tcp = connect_tcp(addr, connect_timeout, keepalive).await?;
    METRICS.upstream_connects_total.inc();
    tcp.write_all(&header).await.map_err(|e| {
        ConnectError::Backend(anyhow::anyhow!("PROXY header to {} failed: {}", addr, e))
    })?;
    Ok((Connection::Plain { inner: tcp }, false))
}
