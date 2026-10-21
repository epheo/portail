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
//! checkout/check-in.
//!
//! Both key idle conns alike (see [`IdleConns`]): by address and protocol,
//! and TLS ones also by the SNI they were opened with.
//!
//! On a pool hit `acquire` returns `(Connection, true)`; the caller uses that
//! signal to retry once with a fresh conn (`acquire_fresh`) if the pooled
//! conn turns out to be closed by the time we write to it, or closes before
//! answering a request it may replay.

use crate::logging::{debug, warn};
use crate::metrics::METRICS;
use crate::proxy::tls::Connection;
use dashmap::DashMap;
use socket2::TcpKeepalive;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
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

/// Idle conns to one backend address over one protocol. TLS ones are kept
/// apart by the server name they were opened with: a conn opened for one
/// SNI must never carry a request for another, as the backend may have
/// picked its certificate or vhost by it. Plain conns carry no SNI and
/// share one list. A handful of names per address at most, so a scan.
#[derive(Default)]
struct IdleConns(Vec<(Box<str>, Vec<Connection>)>);

impl IdleConns {
    /// Pop idle conns opened for `sni` until one probes alive; dead ones
    /// are discarded. A peer FIN, a reset, or a byte from a supposedly
    /// idle HTTP backend (poisoned or desynced) all make a conn unusable.
    /// The probe cannot see a close racing the next request: the caller's
    /// from_pool replay covers that.
    fn take_live(&mut self, sni: &str) -> Option<Connection> {
        let (_, conns) = self.0.iter_mut().find(|(s, _)| **s == *sni)?;
        while let Some(mut conn) = conns.pop() {
            if conn.idle_and_open() {
                METRICS.pool_hits_total.inc();
                return Some(conn);
            }
            METRICS.pool_stale_discards_total.inc();
        }
        None
    }

    fn put(&mut self, sni: &str, conn: Connection, max_idle: usize) {
        let i = match self.0.iter().position(|(s, _)| **s == *sni) {
            Some(i) => i,
            None => {
                self.0.push((sni.into(), Vec::new()));
                self.0.len() - 1
            }
        };
        let conns = &mut self.0[i].1;
        if conns.len() < max_idle {
            conns.push(conn);
        } else {
            // Drop closes the fd.
            METRICS.pool_overflow_drops_total.inc();
        }
    }
}

/// The SNI an idle conn is keyed by: its server name over TLS, none in the
/// clear.
fn sni(use_tls: bool, server_name: &str) -> &str {
    if use_tls {
        server_name
    } else {
        ""
    }
}

/// Why a backend connection could not be opened.
#[derive(Debug)]
pub enum ConnectError {
    /// The proxy itself ran out of fds or kernel memory. The backend was
    /// never reached, so its health must not hear of it: one exhausted
    /// proxy would otherwise eject healthy backends route-wide.
    Local(io::Error),
    /// Refused, unreachable, timed out, out of ports toward it, or the TLS
    /// handshake failed.
    Backend(anyhow::Error),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Local(e) => write!(f, "proxy out of fds or memory: {}", e),
            ConnectError::Backend(e) => e.fmt(f),
        }
    }
}

/// Whether a socket error means the proxy ran out of fds or kernel memory,
/// whatever it was trying to reach.
pub(crate) fn out_of_fds_or_memory(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)
    )
}

/// Classify a socket error from opening a backend connection.
///
/// EADDRNOTAVAIL is not local: connect() returns it once the ephemeral
/// ports toward that one ip:port are all taken (TIME_WAIT pile-up), while
/// other backends stay reachable. Ejecting the backend moves its share.
fn connect_error(addr: SocketAddr, e: io::Error) -> ConnectError {
    if out_of_fds_or_memory(&e) {
        ConnectError::Local(e)
    } else {
        ConnectError::Backend(anyhow::anyhow!("Backend connect failed {}: {}", addr, e))
    }
}

/// Lines a log throttle lets through: one per window, the rest counted.
pub(crate) struct LogThrottle {
    /// The second, on the caller's clock, from which a line may go out.
    next: AtomicU64,
    suppressed: AtomicU64,
}

impl LogThrottle {
    pub(crate) const fn new() -> Self {
        Self {
            next: AtomicU64::new(0),
            suppressed: AtomicU64::new(0),
        }
    }

    /// `Some(lines suppressed since the last one)` when a line may go out
    /// at `now` (seconds), opening a window of `every`.
    fn admit(&self, now: u64, every: Duration) -> Option<u64> {
        let next = self.next.load(Relaxed);
        if now < next
            || self
                .next
                .compare_exchange(next, now + every.as_secs(), Relaxed, Relaxed)
                .is_err()
        {
            self.suppressed.fetch_add(1, Relaxed);
            return None;
        }
        Some(self.suppressed.swap(0, Relaxed))
    }

    /// [`Self::admit`] now, one line per `FLOOD_LOG_EVERY`.
    pub(crate) fn admit_now(&self) -> Option<u64> {
        static LOG_EPOCH: OnceLock<Instant> = OnceLock::new();
        let now = LOG_EPOCH.get_or_init(Instant::now).elapsed().as_secs();
        self.admit(now, FLOOD_LOG_EVERY)
    }
}

/// A failure that repeats as often as its trigger does (every connect out of
/// fds, every datagram from a spoofed source): one line per window keeps the
/// log from flooding while it lasts, and is still seen within seconds.
const FLOOD_LOG_EVERY: Duration = Duration::from_secs(10);

/// Log a backend socket (TCP connect, UDP session) the proxy could not open
/// for want of its own resources, at most once per `FLOOD_LOG_EVERY`.
pub(crate) fn log_local_exhaustion(addr: SocketAddr, e: &io::Error) {
    static THROTTLE: LogThrottle = LogThrottle::new();
    if let Some(suppressed) = THROTTLE.admit_now() {
        warn!(
            "Backend {} not reached: proxy out of its own resources ({}); backend health untouched, {} similar suppressed",
            addr, e, suppressed
        );
    }
}

/// Open a TCP connection to a backend within `timeout`, Nagle off. The
/// keepalive is the one accepted sockets get: a backend that vanishes
/// without FIN or RST would otherwise park an idle client's task and both
/// fds forever. Shared by the pool and the L4 relay.
pub(crate) async fn connect_tcp(
    addr: SocketAddr,
    timeout: Duration,
    keepalive: &TcpKeepalive,
) -> Result<TcpStream, ConnectError> {
    let tcp = match tokio::time::timeout(timeout, TcpStream::connect(addr)).await {
        Ok(Ok(tcp)) => tcp,
        Ok(Err(e)) => return Err(connect_error(addr, e)),
        Err(_) => {
            return Err(ConnectError::Backend(anyhow::anyhow!(
                "Backend connect timeout: {}",
                addr
            )))
        }
    };
    tcp.set_nodelay(true).map_err(|e| connect_error(addr, e))?;
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
) -> Result<Connection, ConnectError> {
    debug!("Pool miss: connecting to {} (tls={})", addr, use_tls);
    let tcp = connect_tcp(addr, connect_timeout, keepalive).await?;
    METRICS.upstream_connects_total.inc();

    if use_tls {
        let domain = rustls::pki_types::ServerName::try_from(server_name.to_string())
            .unwrap_or_else(|_| {
                rustls::pki_types::ServerName::try_from("localhost".to_string()).unwrap()
            });
        let tls_stream = tls_connector.connect(domain, tcp).await.map_err(|e| {
            ConnectError::Backend(anyhow::anyhow!(
                "Backend TLS handshake failed {}: {}",
                addr,
                e
            ))
        })?;
        Ok(Connection::ClientTls { inner: tls_stream })
    } else {
        Ok(Connection::Plain { inner: tcp })
    }
}

pub struct BackendPool {
    /// Keyed by (addr, use_tls): one addr can be reachable both plain and
    /// TLS (two routes, different backendRef TLS), and a plain conn handed
    /// to a TLS request (or vice versa) would speak the wrong protocol.
    pools: HashMap<(SocketAddr, bool), IdleConns>,
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
    ) -> Result<(Connection, bool), ConnectError> {
        if let Some(idle) = self.pools.get_mut(&(addr, use_tls)) {
            if let Some(conn) = idle.take_live(sni(use_tls, server_name)) {
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
    ) -> Result<Connection, ConnectError> {
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

    pub fn release(
        &mut self,
        addr: SocketAddr,
        use_tls: bool,
        server_name: &str,
        conn: Connection,
    ) {
        self.pools.entry((addr, use_tls)).or_default().put(
            sni(use_tls, server_name),
            conn,
            self.max_idle_per_backend,
        );
    }
}

/// Process-wide idle pool shared by every client connection. Same probe and
/// retry semantics as [`BackendPool`]; `max_idle_per_backend` caps each
/// distinct backend identity process-wide. No background reaper — like the
/// per-connection pool, dead idle conns are discarded at checkout by the
/// probe, with the caller's from_pool retry as the safety net.
pub struct SharedBackendPool {
    pools: DashMap<(SocketAddr, bool), IdleConns>,
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
    ) -> Result<(Connection, bool), ConnectError> {
        // Shard lock held only for the sync pop+probe loop, never across await.
        if let Some(mut idle) = self.pools.get_mut(&(addr, use_tls)) {
            if let Some(conn) = idle.take_live(sni(use_tls, server_name)) {
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
    ) -> Result<Connection, ConnectError> {
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
        self.pools.entry((addr, use_tls)).or_default().put(
            sni(use_tls, server_name),
            conn,
            self.max_idle_per_backend,
        );
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
    ) -> Result<(Connection, bool), ConnectError> {
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
    ) -> Result<Connection, ConnectError> {
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
            PoolHandle::PerConn(p) => p.release(addr, use_tls, server_name, conn),
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

    /// EADDRNOTAVAIL is the backend's: the ports run out toward its ip:port
    /// only, so ejecting it moves its share to backends still reachable.
    #[test]
    fn running_out_of_fds_or_memory_is_local() {
        let addr = "127.0.0.1:1".parse().unwrap();
        let class = |errno| connect_error(addr, io::Error::from_raw_os_error(errno));
        for errno in [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM] {
            assert!(matches!(class(errno), ConnectError::Local(_)), "{errno}");
        }
        for errno in [
            libc::ECONNREFUSED,
            libc::EHOSTUNREACH,
            libc::ETIMEDOUT,
            libc::EADDRNOTAVAIL,
        ] {
            assert!(matches!(class(errno), ConnectError::Backend(_)), "{errno}");
        }
    }

    #[test]
    fn log_throttle_lets_one_line_per_window_and_counts_the_rest() {
        let throttle = LogThrottle::new();
        let every = Duration::from_secs(10);
        assert_eq!(throttle.admit(100, every), Some(0));
        assert_eq!(throttle.admit(100, every), None);
        assert_eq!(throttle.admit(109, every), None);
        assert_eq!(throttle.admit(110, every), Some(2));
        assert_eq!(throttle.admit(500, every), Some(0));
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

    /// The FIN wakes the reactor, so the gated peek runs and sees it.
    #[tokio::test]
    async fn probe_half_closed_returns_stale() {
        let (local, mut remote) = pair().await;
        remote.shutdown().await.unwrap();
        settle().await;
        let before = peek_syscalls();
        assert!(!Connection::Plain { inner: local }.idle_and_open());
        assert_eq!(peek_syscalls(), before + 1);
    }

    fn peek_syscalls() -> usize {
        crate::proxy::tls::PEEK_SYSCALLS.with(|n| n.get())
    }

    /// A pooled conn quiet since its response costs no probe syscall: the
    /// request path is at its minimum of four, and a fifth on every reuse
    /// costs real throughput.
    #[tokio::test]
    async fn quiet_pooled_conn_is_reused_without_a_syscall() {
        use tokio::io::AsyncReadExt;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            sock.read_exact(&mut [0u8; 4]).await.unwrap();
            sock.write_all(b"pong").await.unwrap();
            let _ = sock.read_to_end(&mut Vec::new()).await;
        });
        // The pool's TLS client config needs one, even for plain backends.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut pool = BackendPool::new(4, Duration::from_secs(2));
        let keepalive = TcpKeepalive::new();
        let (mut conn, _) = pool.acquire(addr, false, "", &keepalive).await.unwrap();
        conn.write_all(b"ping").await.unwrap();
        // Read as the engine does, with room to spare: a short read tells
        // tokio the socket is drained.
        let (mut pong, mut got) = ([0u8; 64], 0);
        while got < 4 {
            got += conn.read(&mut pong[got..]).await.unwrap();
        }
        pool.release(addr, false, "", conn);

        let before = peek_syscalls();
        let (_, reused) = pool.acquire(addr, false, "", &keepalive).await.unwrap();
        assert!(reused);
        assert_eq!(
            peek_syscalls(),
            before,
            "probing a quiet conn made a syscall"
        );
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

    /// TLS server greeting each connection with one byte, which a client
    /// reads past the session tickets as a response would; `None` without
    /// openssl.
    async fn tls_server() -> Option<SocketAddr> {
        let (cert_pem, key_pem) = crate::proxy::tls::test_util::generate_test_cert("s.test")?;
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let cert = crate::config::CertificateRef {
            name: "s".to_string(),
            cert_pem: Some(cert_pem),
            key_pem: Some(key_pem),
            ..Default::default()
        };
        let server_cfg =
            crate::proxy::tls::build_server_config(&[cert], std::path::Path::new("/unused"), false)
                .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_cfg));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    if tls.write_all(b"x").await.is_ok() && tls.flush().await.is_ok() {
                        let _ = tls.read_to_end(&mut Vec::new()).await;
                    }
                });
            }
        });
        Some(addr)
    }

    /// A conn opened with one SNI must never carry a request for another:
    /// the backend picked its certificate and vhost by it. Plain conns
    /// carry no SNI, and both pool scopes key alike.
    #[tokio::test]
    async fn tls_conns_are_pooled_by_sni_plain_ones_by_address() {
        use tokio::io::AsyncReadExt;
        let Some(tls) = tls_server().await else {
            return; // openssl unavailable
        };
        let plain = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let plain_addr = plain.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = plain.accept().await {
                held.push(sock);
            }
        });
        let keepalive = TcpKeepalive::new();
        let timeout = Duration::from_secs(2);
        for mut pool in [
            PoolHandle::PerConn(BackendPool::new(4, timeout)),
            PoolHandle::Shared(Arc::new(SharedBackendPool::new(4, timeout))),
        ] {
            let (mut conn, _) = pool.acquire(tls, true, "a.test", &keepalive).await.unwrap();
            conn.read_exact(&mut [0u8; 1]).await.unwrap();
            pool.release(tls, true, "a.test", conn);
            let (_, reused) = pool.acquire(tls, true, "b.test", &keepalive).await.unwrap();
            assert!(!reused, "a conn opened for a.test served b.test");
            let (_, reused) = pool.acquire(tls, true, "a.test", &keepalive).await.unwrap();
            assert!(reused, "a.test lost its pooled conn");

            let (conn, _) = pool
                .acquire(plain_addr, false, "x", &keepalive)
                .await
                .unwrap();
            pool.release(plain_addr, false, "x", conn);
            let (_, reused) = pool
                .acquire(plain_addr, false, "y", &keepalive)
                .await
                .unwrap();
            assert!(reused, "a plain conn was keyed by server name");
        }
    }

    #[tokio::test]
    async fn probe_idle_tls_conn_returns_alive() {
        let Some((mut conn, _server)) = tls_pair().await else {
            return; // openssl unavailable
        };
        settle().await;
        let before = peek_syscalls();
        assert!(conn.idle_and_open());
        assert_eq!(
            peek_syscalls(),
            before,
            "probing a quiet conn made a syscall"
        );
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
