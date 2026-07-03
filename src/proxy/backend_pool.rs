//! Idle backend-connection pooling, in two scopes.
//!
//! [`BackendPool`] — one pool per accepted client connection (the default):
//! no locks, no contention; reuses idle backend connections across keepalive
//! requests on the same client conn to amortize TCP/TLS handshake cost. Zero
//! cross-client reuse: pooled conns die with their client.
//!
//! [`SharedBackendPool`] — one pool per process, shared by every client
//! connection (`performance.backendPoolScope: process`): cross-client reuse
//! under client-connection churn, at the cost of a DashMap shard lock per
//! checkout/check-in. Keyed by the full backend identity (addr, TLS, SNI)
//! because cross-client sharing loses the per-route context the
//! per-connection pool carried implicitly.
//!
//! On a pool hit `acquire` returns `(Connection, true)`; the caller uses that
//! signal to retry once with a fresh conn if the pooled conn turns out to be
//! half-closed by the time we write to it (`acquire_fresh`).

use crate::logging::debug;
use crate::metrics::METRICS;
use crate::proxy::tls::Connection;
use anyhow::Result;
use dashmap::DashMap;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;

/// Client TLS connector that skips certificate verification.
/// Backend services in k8s typically use self-signed certs.
fn new_tls_connector() -> Arc<tokio_rustls::TlsConnector> {
    let client_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerifier))
        .with_no_client_auth();
    Arc::new(tokio_rustls::TlsConnector::from(Arc::new(client_config)))
}

/// Pop idle conns until one probes alive; dead ones are discarded.
///
/// 1-byte probe: WouldBlock on an idle TCP socket means open with no readable
/// data — alive. Ok(0) is a peer FIN; Ok(1) is an unsolicited byte from a
/// supposedly-idle HTTP backend (poisoned or desynced). In both Ok cases the
/// conn is unusable; the byte we may have read in the poisoned case is on a
/// conn we're dropping anyway, so it's harmless.
///
/// For Connection::Tls / ClientTls this resolves to a hardcoded WouldBlock in
/// tls.rs — TLS state can't be peeked without consuming framing. The caller's
/// retry-on-from_pool path is the safety net for stale TLS pool entries.
fn take_live(conns: &mut Vec<Connection>) -> Option<Connection> {
    while let Some(conn) = conns.pop() {
        let mut probe = [0u8; 1];
        match conn.try_read(&mut probe) {
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                METRICS.pool_hits_total.inc();
                return Some(conn);
            }
            _ => {
                METRICS.pool_stale_discards_total.inc();
                continue;
            }
        }
    }
    None
}

async fn connect_new(
    tls_connector: &tokio_rustls::TlsConnector,
    connect_timeout: Duration,
    addr: SocketAddr,
    use_tls: bool,
    server_name: &str,
) -> Result<Connection> {
    debug!("Pool miss: connecting to {} (tls={})", addr, use_tls);
    let tcp = tokio::time::timeout(connect_timeout, TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow::anyhow!("Backend connect timeout: {}", addr))?
        .map_err(|e| anyhow::anyhow!("Backend connect failed {}: {}", addr, e))?;

    tcp.set_nodelay(true)?;
    METRICS.upstream_connects_total.inc();

    if use_tls {
        let domain = rustls::pki_types::ServerName::try_from(server_name.to_string())
            .unwrap_or_else(|_| {
                rustls::pki_types::ServerName::try_from("localhost".to_string()).unwrap()
            });
        let tls_stream = tls_connector
            .connect(domain, tcp)
            .await
            .map_err(|e| anyhow::anyhow!("Backend TLS handshake failed {}: {}", addr, e))?;
        Ok(Connection::ClientTls { inner: tls_stream })
    } else {
        Ok(Connection::Plain { inner: tcp })
    }
}

pub struct BackendPool {
    pools: HashMap<SocketAddr, Vec<Connection>>,
    max_idle_per_backend: usize,
    connect_timeout: Duration,
    tls_connector: Arc<tokio_rustls::TlsConnector>,
}

impl BackendPool {
    pub fn new(max_idle_per_backend: usize, connect_timeout: Duration) -> Self {
        Self {
            pools: HashMap::new(),
            max_idle_per_backend,
            connect_timeout,
            tls_connector: new_tls_connector(),
        }
    }

    /// Get a backend connection — pooled if available and alive, otherwise fresh.
    ///
    /// Returns `(Connection, from_pool)`. `from_pool == true` means the caller
    /// may retry once with `acquire_fresh` if the conn errors during the SAFE
    /// zone (before any client bytes are read for the body or written): the
    /// probe is best-effort and a backend can close a pooled conn between the
    /// probe and our write.
    pub async fn acquire(
        &mut self,
        addr: SocketAddr,
        use_tls: bool,
        server_name: &str,
    ) -> Result<(Connection, bool)> {
        if let Some(conns) = self.pools.get_mut(&addr) {
            if let Some(conn) = take_live(conns) {
                debug!("Pool hit: reusing connection to {}", addr);
                return Ok((conn, true));
            }
        }

        METRICS.pool_misses_total.inc();
        let conn = connect_new(
            &self.tls_connector,
            self.connect_timeout,
            addr,
            use_tls,
            server_name,
        )
        .await?;
        Ok((conn, false))
    }

    /// Open a brand-new backend connection, skipping the pool entirely.
    /// Used by the caller's retry path when a pooled conn turns out to be stale.
    pub async fn acquire_fresh(
        &self,
        addr: SocketAddr,
        use_tls: bool,
        server_name: &str,
    ) -> Result<Connection> {
        connect_new(
            &self.tls_connector,
            self.connect_timeout,
            addr,
            use_tls,
            server_name,
        )
        .await
    }

    pub fn release(&mut self, addr: SocketAddr, conn: Connection) {
        let conns = self.pools.entry(addr).or_default();
        if conns.len() < self.max_idle_per_backend {
            conns.push(conn);
        } else {
            // Drop closes the fd.
            METRICS.pool_overflow_drops_total.inc();
        }
    }
}

/// Process-wide idle pool shared by every client connection. Same probe and
/// retry semantics as [`BackendPool`]; `max_idle_per_backend` caps each
/// distinct backend identity process-wide. No background reaper — like the
/// per-connection pool, dead idle conns are discarded at checkout by the
/// probe, with the caller's from_pool retry as the safety net.
pub struct SharedBackendPool {
    pools: DashMap<(SocketAddr, bool, String), Vec<Connection>>,
    max_idle_per_backend: usize,
    connect_timeout: Duration,
    tls_connector: Arc<tokio_rustls::TlsConnector>,
}

impl SharedBackendPool {
    pub fn new(max_idle_per_backend: usize, connect_timeout: Duration) -> Self {
        Self {
            pools: DashMap::new(),
            max_idle_per_backend,
            connect_timeout,
            tls_connector: new_tls_connector(),
        }
    }

    pub async fn acquire(
        &self,
        addr: SocketAddr,
        use_tls: bool,
        server_name: &str,
    ) -> Result<(Connection, bool)> {
        // Shard lock held only for the sync pop+probe loop, never across await.
        if let Some(mut entry) = self
            .pools
            .get_mut(&(addr, use_tls, server_name.to_string()))
        {
            if let Some(conn) = take_live(&mut entry) {
                debug!("Shared pool hit: reusing connection to {}", addr);
                return Ok((conn, true));
            }
        }

        METRICS.pool_misses_total.inc();
        let conn = connect_new(
            &self.tls_connector,
            self.connect_timeout,
            addr,
            use_tls,
            server_name,
        )
        .await?;
        Ok((conn, false))
    }

    pub async fn acquire_fresh(
        &self,
        addr: SocketAddr,
        use_tls: bool,
        server_name: &str,
    ) -> Result<Connection> {
        connect_new(
            &self.tls_connector,
            self.connect_timeout,
            addr,
            use_tls,
            server_name,
        )
        .await
    }

    pub fn release(&self, addr: SocketAddr, use_tls: bool, server_name: &str, conn: Connection) {
        let mut conns = self
            .pools
            .entry((addr, use_tls, server_name.to_string()))
            .or_default();
        if conns.len() < self.max_idle_per_backend {
            conns.push(conn);
        } else {
            // Drop closes the fd.
            METRICS.pool_overflow_drops_total.inc();
        }
    }
}

/// A connection task's handle to whichever pool scope is configured
/// (`performance.backendPoolScope`). Same API either way; the worker never
/// cares which scope it got.
pub enum PoolHandle {
    PerConn(BackendPool),
    Shared(Arc<SharedBackendPool>),
}

impl PoolHandle {
    pub async fn acquire(
        &mut self,
        addr: SocketAddr,
        use_tls: bool,
        server_name: &str,
    ) -> Result<(Connection, bool)> {
        match self {
            PoolHandle::PerConn(p) => p.acquire(addr, use_tls, server_name).await,
            PoolHandle::Shared(p) => p.acquire(addr, use_tls, server_name).await,
        }
    }

    pub async fn acquire_fresh(
        &self,
        addr: SocketAddr,
        use_tls: bool,
        server_name: &str,
    ) -> Result<Connection> {
        match self {
            PoolHandle::PerConn(p) => p.acquire_fresh(addr, use_tls, server_name).await,
            PoolHandle::Shared(p) => p.acquire_fresh(addr, use_tls, server_name).await,
        }
    }

    pub fn release(
        &mut self,
        addr: SocketAddr,
        use_tls: bool,
        server_name: &str,
        conn: Connection,
    ) {
        match self {
            PoolHandle::PerConn(p) => p.release(addr, conn),
            PoolHandle::Shared(p) => p.release(addr, use_tls, server_name, conn),
        }
    }
}

/// Certificate verifier that accepts any server certificate.
/// Used for backend connections where services use self-signed certs.
#[derive(Debug)]
struct NoVerifier;

impl rustls::client::danger::ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    /// Open a pair of connected tokio TcpStreams: returns (local, remote).
    /// `remote` is the server-side; closing/writing to it drives the probe outcome.
    async fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (client, server) =
            tokio::join!(async { TcpStream::connect(addr).await.unwrap() }, async {
                listener.accept().await.unwrap().0
            },);
        (client, server)
    }

    /// Probe an idle TcpStream the way `acquire` does and report the branch taken.
    #[derive(Debug, PartialEq)]
    enum ProbeOutcome {
        Alive,
        Stale,
    }

    fn probe_tcp(stream: &TcpStream) -> ProbeOutcome {
        let mut buf = [0u8; 1];
        match stream.try_read(&mut buf) {
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => ProbeOutcome::Alive,
            _ => ProbeOutcome::Stale,
        }
    }

    #[tokio::test]
    async fn probe_idle_conn_returns_alive() {
        let (local, _remote) = pair().await;
        assert_eq!(probe_tcp(&local), ProbeOutcome::Alive);
    }

    #[tokio::test]
    async fn probe_half_closed_returns_stale() {
        let (local, mut remote) = pair().await;
        // Remote half-closes — local should see Ok(0) on read.
        remote.shutdown().await.unwrap();
        // Give the FIN a moment to land.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(probe_tcp(&local), ProbeOutcome::Stale);
    }

    #[tokio::test]
    async fn probe_unsolicited_byte_returns_stale() {
        let (local, mut remote) = pair().await;
        // Remote sends an unsolicited byte — an idle HTTP backend shouldn't
        // speak first. Probe returns Ok(1); treated as stale (poisoned conn).
        remote.write_all(b"x").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(probe_tcp(&local), ProbeOutcome::Stale);
    }
}
