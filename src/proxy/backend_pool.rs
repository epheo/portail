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
//! signal to retry once with a fresh conn (`acquire_fresh`) if the pooled
//! conn turns out to be closed by the time we write to it, or closes before
//! answering a request it may replay.

use crate::logging::debug;
use crate::metrics::METRICS;
use crate::proxy::tls::Connection;
use anyhow::Result;
use dashmap::DashMap;
use socket2::TcpKeepalive;
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

/// Pop idle conns until one probes alive; dead ones are discarded. A peer
/// FIN, a reset, or a byte from a supposedly idle HTTP backend (poisoned or
/// desynced) all make a conn unusable. The probe cannot see a close racing
/// the next request: the caller's from_pool replay covers that.
fn take_live(conns: &mut Vec<Connection>) -> Option<Connection> {
    while let Some(mut conn) = conns.pop() {
        if conn.idle_and_open() {
            METRICS.pool_hits_total.inc();
            return Some(conn);
        }
        METRICS.pool_stale_discards_total.inc();
    }
    None
}

/// Open a TCP connection to a backend within `timeout`, Nagle off. The
/// keepalive is the one accepted sockets get: a backend that vanishes
/// without FIN or RST would otherwise park an idle client's task and both
/// fds forever. Shared by the pool and the L4 relay.
pub(crate) async fn connect_tcp(
    addr: SocketAddr,
    timeout: Duration,
    keepalive: &TcpKeepalive,
) -> Result<TcpStream> {
    let tcp = tokio::time::timeout(timeout, TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow::anyhow!("Backend connect timeout: {}", addr))?
        .map_err(|e| anyhow::anyhow!("Backend connect failed {}: {}", addr, e))?;
    tcp.set_nodelay(true)?;
    // _-prefixed: debug! compiles out in release builds.
    if let Err(_e) = socket2::SockRef::from(&tcp).set_tcp_keepalive(keepalive) {
        debug!("TCP keepalive setup failed for backend {}: {}", addr, _e);
    }
    Ok(tcp)
}

async fn connect_new(
    tls_connector: &tokio_rustls::TlsConnector,
    connect_timeout: Duration,
    addr: SocketAddr,
    use_tls: bool,
    server_name: &str,
    keepalive: &TcpKeepalive,
) -> Result<Connection> {
    debug!("Pool miss: connecting to {} (tls={})", addr, use_tls);
    let tcp = connect_tcp(addr, connect_timeout, keepalive).await?;
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
    /// Keyed by (addr, use_tls): one addr can be reachable both plain and
    /// TLS (two routes, different backendRef TLS), and a plain conn handed
    /// to a TLS request (or vice versa) would speak the wrong protocol.
    /// SNI stays out of the key to keep checkout allocation-free.
    pools: HashMap<(SocketAddr, bool), Vec<Connection>>,
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
    /// may retry once with `acquire_fresh` if the conn fails before the
    /// backend answered (see the module docs): the probe is best-effort and
    /// a backend can close a pooled conn between the probe and our request.
    ///
    /// `keepalive` comes per call, not per pool: the process-wide pool
    /// serves every listener, each with its own config.
    pub async fn acquire(
        &mut self,
        addr: SocketAddr,
        use_tls: bool,
        server_name: &str,
        keepalive: &TcpKeepalive,
    ) -> Result<(Connection, bool)> {
        if let Some(conns) = self.pools.get_mut(&(addr, use_tls)) {
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
            keepalive,
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
        keepalive: &TcpKeepalive,
    ) -> Result<Connection> {
        connect_new(
            &self.tls_connector,
            self.connect_timeout,
            addr,
            use_tls,
            server_name,
            keepalive,
        )
        .await
    }

    pub fn release(&mut self, addr: SocketAddr, use_tls: bool, conn: Connection) {
        let conns = self.pools.entry((addr, use_tls)).or_default();
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
        keepalive: &TcpKeepalive,
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
            keepalive,
        )
        .await?;
        Ok((conn, false))
    }

    pub async fn acquire_fresh(
        &self,
        addr: SocketAddr,
        use_tls: bool,
        server_name: &str,
        keepalive: &TcpKeepalive,
    ) -> Result<Connection> {
        connect_new(
            &self.tls_connector,
            self.connect_timeout,
            addr,
            use_tls,
            server_name,
            keepalive,
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
        keepalive: &TcpKeepalive,
    ) -> Result<(Connection, bool)> {
        match self {
            PoolHandle::PerConn(p) => p.acquire(addr, use_tls, server_name, keepalive).await,
            PoolHandle::Shared(p) => p.acquire(addr, use_tls, server_name, keepalive).await,
        }
    }

    pub async fn acquire_fresh(
        &self,
        addr: SocketAddr,
        use_tls: bool,
        server_name: &str,
        keepalive: &TcpKeepalive,
    ) -> Result<Connection> {
        match self {
            PoolHandle::PerConn(p) => p.acquire_fresh(addr, use_tls, server_name, keepalive).await,
            PoolHandle::Shared(p) => p.acquire_fresh(addr, use_tls, server_name, keepalive).await,
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
            PoolHandle::PerConn(p) => p.release(addr, use_tls, conn),
            PoolHandle::Shared(p) => p.release(addr, use_tls, server_name, conn),
        }
    }
}

/// Certificate verifier that accepts any server certificate.
/// Used for backend connections where services use self-signed certs.
/// pub(crate) so TLS tests can build a permissive client without a copy.
#[derive(Debug)]
pub(crate) struct NoVerifier;

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

    /// Lets what the remote end sent land before probing.
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn probe_idle_conn_returns_alive() {
        let (local, _remote) = pair().await;
        assert!(Connection::Plain { inner: local }.idle_and_open());
    }

    #[tokio::test]
    async fn probe_half_closed_returns_stale() {
        let (local, mut remote) = pair().await;
        remote.shutdown().await.unwrap();
        settle().await;
        assert!(!Connection::Plain { inner: local }.idle_and_open());
    }

    /// An idle HTTP backend never speaks first: the conn is poisoned.
    #[tokio::test]
    async fn probe_unsolicited_byte_returns_stale() {
        let (local, mut remote) = pair().await;
        remote.write_all(b"x").await.unwrap();
        settle().await;
        assert!(!Connection::Plain { inner: local }.idle_and_open());
    }

    /// A client TLS conn, idle after one exchange as when pooled, and the
    /// server end driving it; `None` without openssl. The exchange matters:
    /// a TLS 1.3 server's session tickets trail the handshake, and only the
    /// first response read consumes them.
    async fn tls_pair() -> Option<(Connection, tokio_rustls::server::TlsStream<TcpStream>)> {
        use tokio::io::AsyncReadExt;
        let (cert_pem, key_pem) = crate::proxy::tls::test_util::generate_test_cert("b.test")?;
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let cert = crate::config::CertificateRef {
            name: "b".to_string(),
            cert_pem: Some(cert_pem),
            key_pem: Some(key_pem),
            ..Default::default()
        };
        let server_cfg =
            crate::proxy::tls::build_server_config(&[cert], std::path::Path::new("/unused"), false)
                .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_cfg));
        let (local, remote) = pair().await;
        let sni = rustls::pki_types::ServerName::try_from("b.test").unwrap();
        let (client, server) = tokio::join!(new_tls_connector().connect(sni, local), async {
            acceptor.accept(remote).await
        });
        let (mut client, mut server) = (client.unwrap(), server.unwrap());
        server.write_all(b"x").await.unwrap();
        server.flush().await.unwrap();
        client.read_exact(&mut [0u8; 1]).await.unwrap();
        Some((Connection::ClientTls { inner: client }, server))
    }

    #[tokio::test]
    async fn probe_idle_tls_conn_returns_alive() {
        let Some((mut conn, _server)) = tls_pair().await else {
            return; // openssl unavailable
        };
        settle().await;
        assert!(conn.idle_and_open());
    }

    /// Node's https server ends an idle keep-alive conn with close_notify
    /// and FIN: probing only plain conns reused it, and the next request
    /// met EOF.
    #[tokio::test]
    async fn probe_tls_conn_closed_by_the_server_returns_stale() {
        let Some((mut conn, mut server)) = tls_pair().await else {
            return; // openssl unavailable
        };
        server.shutdown().await.unwrap();
        settle().await;
        assert!(!conn.idle_and_open());
    }

    /// Bytes rustls decrypted but nobody read are past the socket peek;
    /// pooled, they would answer the next request.
    #[tokio::test]
    async fn probe_tls_conn_holding_unread_plaintext_returns_stale() {
        use tokio::io::AsyncReadExt;
        let Some((mut conn, mut server)) = tls_pair().await else {
            return; // openssl unavailable
        };
        server.write_all(b"ab").await.unwrap();
        server.flush().await.unwrap();
        conn.read_exact(&mut [0u8; 1]).await.unwrap();
        assert!(!conn.idle_and_open());
    }
}
