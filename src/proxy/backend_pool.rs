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
// Keys are backend addresses from routes, never client input: no need for
// SipHash's flood resistance, which costs on every checkout and check-in.
use fnv::{FnvBuildHasher, FnvHashMap};
use socket2::TcpKeepalive;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;

use crate::proxy::upstream::tls::Verify;

/// Client TLS connector that skips certificate verification.
/// Backend services in k8s typically use self-signed certs.
///
/// One per process, built on the first TLS connect: it holds no config, and
/// building it was most of a churned connection's allocations. Its session
/// cache goes with it, so backend sessions resume across client connections,
/// as the process-wide pool's already did.
fn tls_connector() -> &'static tokio_rustls::TlsConnector {
    static CONNECTOR: OnceLock<tokio_rustls::TlsConnector> = OnceLock::new();
    CONNECTOR.get_or_init(|| {
        let client_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
            .with_no_client_auth();
        tokio_rustls::TlsConnector::from(Arc::new(client_config))
    })
}

/// [`tls_connector`], offering h2 by ALPN: gRPC's.
fn tls_connector_h2() -> &'static tokio_rustls::TlsConnector {
    static CONNECTOR: OnceLock<tokio_rustls::TlsConnector> = OnceLock::new();
    CONNECTOR.get_or_init(|| {
        let mut client_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
            .with_no_client_auth();
        client_config.alpn_protocols = vec![b"h2".to_vec()];
        tokio_rustls::TlsConnector::from(Arc::new(client_config))
    })
}

/// The name an unverified handshake sends: one that is no DNS name or IP
/// sends `localhost`, which nothing checks.
fn sni_of(server_name: &str) -> rustls::pki_types::ServerName<'static> {
    rustls::pki_types::ServerName::try_from(server_name.to_string()).unwrap_or_else(|_| {
        rustls::pki_types::ServerName::try_from("localhost".to_string()).unwrap()
    })
}

/// Idle conns to one backend address. One addr can be reachable both plain
/// and TLS (two routes, different backendRef TLS), and a plain conn handed
/// to a TLS request (or vice versa) would speak the wrong protocol. TLS
/// ones are also kept apart by the server name they were opened with: a
/// conn opened for one SNI must never carry a request for another, as the
/// backend may have picked its certificate or vhost by it. Plain conns
/// carry no SNI and share one list, found without a name compare.
#[derive(Default)]
struct IdleConns {
    plain: Vec<Connection>,
    /// A handful of keys per address at most, so a scan.
    tls: Vec<(TlsKey<Box<str>>, Vec<Connection>)>,
}

/// What an idle TLS conn is kept by: the server name it was opened with,
/// or the policy it was verified under. `verified` is its own field so
/// that no server name, whatever it spells, finds a verified conn, and no
/// policy an unverified one.
#[derive(Clone, Copy)]
struct TlsKey<S> {
    verified: bool,
    name: S,
}

impl TlsKey<&str> {
    fn is(&self, kept: &TlsKey<Box<str>>) -> bool {
        self.verified == kept.verified && *self.name == *kept.name
    }
}

impl IdleConns {
    /// Pop idle conns opened for `sni` (`None`: plain) until one probes
    /// alive; dead ones are discarded. A peer FIN, a reset, or a byte from
    /// a supposedly idle HTTP backend (poisoned or desynced) all make a
    /// conn unusable. The probe cannot see a close racing the next
    /// request: the caller's from_pool replay covers that.
    fn take_live(&mut self, key: Option<TlsKey<&str>>) -> Option<Connection> {
        let conns = match key {
            None => &mut self.plain,
            Some(key) => &mut self.tls.iter_mut().find(|(kept, _)| key.is(kept))?.1,
        };
        while let Some(mut conn) = conns.pop() {
            if conn.idle_and_open() {
                METRICS.pool_hits_total.inc();
                return Some(conn);
            }
            METRICS.pool_stale_discards_total.inc();
        }
        None
    }

    fn put(&mut self, key: Option<TlsKey<&str>>, conn: Connection, max_idle: usize) {
        let conns = match key {
            None => &mut self.plain,
            Some(key) => {
                let i = match self.tls.iter().position(|(kept, _)| key.is(kept)) {
                    Some(i) => i,
                    None => {
                        let kept = TlsKey {
                            verified: key.verified,
                            name: key.name.into(),
                        };
                        self.tls.push((kept, Vec::new()));
                        self.tls.len() - 1
                    }
                };
                &mut self.tls[i].1
            }
        };
        if conns.len() < max_idle {
            conns.push(conn);
        } else {
            // Drop closes the fd.
            METRICS.pool_overflow_drops_total.inc();
        }
    }
}

/// How a backend is spoken to, and so what an idle connection to it is
/// kept by.
#[derive(Clone, Copy)]
pub enum Proto<'a> {
    Plain,
    /// TLS to this server name, its certificate not checked.
    Tls(&'a str),
    /// TLS under a BackendTLSPolicy.
    Verified(&'a Verify),
}

impl<'a> Proto<'a> {
    fn key(self) -> Option<TlsKey<&'a str>> {
        let (verified, name) = match self {
            Proto::Plain => return None,
            Proto::Tls(server_name) => (false, server_name),
            Proto::Verified(verify) => (true, verify.key()),
        };
        Some(TlsKey { verified, name })
    }

    /// `tcp`, the handshake done when there is one.
    pub(crate) async fn over(self, tcp: TcpStream) -> std::io::Result<Connection> {
        let tls = match self {
            Proto::Plain => return Ok(Connection::Plain { inner: tcp }),
            Proto::Tls(server_name) => tls_connector().connect(sni_of(server_name), tcp).await?,
            Proto::Verified(verify) => verify.connect(tcp).await?,
        };
        Ok(Connection::ClientTls {
            inner: Box::new(tls),
        })
    }

    /// [`Self::over`], offering h2 by ALPN: how gRPC reaches a backend, and
    /// a gRPC server may refuse a handshake without it (grpc-go since 1.67).
    pub(crate) async fn over_h2(self, tcp: TcpStream) -> std::io::Result<Connection> {
        let tls = match self {
            Proto::Plain => return Ok(Connection::Plain { inner: tcp }),
            Proto::Tls(server_name) => tls_connector_h2().connect(sni_of(server_name), tcp).await?,
            Proto::Verified(verify) => verify.connect_h2(tcp).await?,
        };
        Ok(Connection::ClientTls {
            inner: Box::new(tls),
        })
    }
}

/// A [`Proto`] a route table keeps.
#[derive(Debug, Clone)]
pub enum BackendProto {
    Plain,
    Tls(Box<str>),
    Verified(Arc<Verify>),
}

impl BackendProto {
    pub fn as_ref(&self) -> Proto<'_> {
        match self {
            BackendProto::Plain => Proto::Plain,
            BackendProto::Tls(server_name) => Proto::Tls(server_name),
            BackendProto::Verified(verify) => Proto::Verified(verify),
        }
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
/// A line names the backend it met; the count it carries covers every one.
pub(crate) fn log_local_exhaustion(addr: SocketAddr, e: &io::Error) {
    static THROTTLE: LogThrottle = LogThrottle::new();
    if let Some(suppressed) = THROTTLE.admit_now() {
        warn!(
            "Backend {} not reached: proxy out of its own resources ({}); backend health untouched; {} more since the last such line, any backend",
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
    if let Err(e) = socket2::SockRef::from(&tcp).set_tcp_keepalive(keepalive) {
        debug!("TCP keepalive setup failed for backend {}: {}", addr, e);
    }
    Ok(tcp)
}

async fn connect_new(
    connect_timeout: Duration,
    addr: SocketAddr,
    proto: Proto<'_>,
    keepalive: &TcpKeepalive,
) -> Result<Connection, ConnectError> {
    debug!(
        "Pool miss: connecting to {} (tls={})",
        addr,
        proto.key().is_some()
    );
    let tcp = connect_tcp(addr, connect_timeout, keepalive).await?;
    METRICS.upstream_connects_total.inc();
    proto.over(tcp).await.map_err(|e| {
        ConnectError::Backend(anyhow::anyhow!(
            "Backend TLS handshake failed {}: {}",
            addr,
            e
        ))
    })
}

pub struct BackendPool {
    pools: FnvHashMap<SocketAddr, IdleConns>,
    max_idle_per_backend: usize,
    connect_timeout: Duration,
}

impl BackendPool {
    pub fn new(max_idle_per_backend: usize, connect_timeout: Duration) -> Self {
        Self {
            pools: FnvHashMap::default(),
            max_idle_per_backend,
            connect_timeout,
        }
    }

    /// A live idle conn opened for this address, protocol and SNI, if the
    /// pool holds one. Never waits.
    pub fn take_idle(&mut self, addr: SocketAddr, proto: Proto<'_>) -> Option<Connection> {
        let idle = self.pools.get_mut(&addr)?;
        let conn = idle.take_live(proto.key())?;
        debug!("Pool hit: reusing connection to {}", addr);
        Some(conn)
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
        proto: Proto<'_>,
        keepalive: &TcpKeepalive,
    ) -> Result<(Connection, bool), ConnectError> {
        if let Some(conn) = self.take_idle(addr, proto) {
            return Ok((conn, true));
        }

        METRICS.pool_misses_total.inc();
        let conn = connect_new(self.connect_timeout, addr, proto, keepalive).await?;
        Ok((conn, false))
    }

    /// Open a brand-new backend connection, skipping the pool entirely.
    /// Used by the caller's retry path when a pooled conn turns out to be stale.
    pub async fn acquire_fresh(
        &self,
        addr: SocketAddr,
        proto: Proto<'_>,
        keepalive: &TcpKeepalive,
    ) -> Result<Connection, ConnectError> {
        connect_new(self.connect_timeout, addr, proto, keepalive).await
    }

    pub fn release(&mut self, addr: SocketAddr, proto: Proto<'_>, conn: Connection) {
        self.pools
            .entry(addr)
            .or_default()
            .put(proto.key(), conn, self.max_idle_per_backend);
    }
}

/// Process-wide idle pool shared by every client connection. Same probe and
/// retry semantics as [`BackendPool`]; `max_idle_per_backend` caps each
/// distinct backend identity process-wide. No background reaper — like the
/// per-connection pool, dead idle conns are discarded at checkout by the
/// probe, with the caller's from_pool retry as the safety net.
pub struct SharedBackendPool {
    pools: DashMap<SocketAddr, IdleConns, FnvBuildHasher>,
    max_idle_per_backend: usize,
    connect_timeout: Duration,
}

impl SharedBackendPool {
    pub fn new(max_idle_per_backend: usize, connect_timeout: Duration) -> Self {
        Self {
            pools: DashMap::default(),
            max_idle_per_backend,
            connect_timeout,
        }
    }

    pub fn take_idle(&self, addr: SocketAddr, proto: Proto<'_>) -> Option<Connection> {
        // Shard lock held only for the sync pop+probe loop.
        let conn = self.pools.get_mut(&addr)?.take_live(proto.key())?;
        debug!("Shared pool hit: reusing connection to {}", addr);
        Some(conn)
    }

    pub async fn acquire(
        &self,
        addr: SocketAddr,
        proto: Proto<'_>,
        keepalive: &TcpKeepalive,
    ) -> Result<(Connection, bool), ConnectError> {
        if let Some(conn) = self.take_idle(addr, proto) {
            return Ok((conn, true));
        }

        METRICS.pool_misses_total.inc();
        let conn = connect_new(self.connect_timeout, addr, proto, keepalive).await?;
        Ok((conn, false))
    }

    pub async fn acquire_fresh(
        &self,
        addr: SocketAddr,
        proto: Proto<'_>,
        keepalive: &TcpKeepalive,
    ) -> Result<Connection, ConnectError> {
        connect_new(self.connect_timeout, addr, proto, keepalive).await
    }

    pub fn release(&self, addr: SocketAddr, proto: Proto<'_>, conn: Connection) {
        self.pools
            .entry(addr)
            .or_default()
            .put(proto.key(), conn, self.max_idle_per_backend);
    }
}

/// A connection task's handle to whichever pool scope is configured
/// (`performance.backendPoolScope`). Same API either way; the engine never
/// cares which scope it got.
pub enum PoolHandle {
    PerConn(BackendPool),
    Shared(Arc<SharedBackendPool>),
}

impl PoolHandle {
    /// A live idle conn, if the pool holds one: a hit needs no connect
    /// budget, and arming one costs a timer.
    pub fn take_idle(&mut self, addr: SocketAddr, proto: Proto<'_>) -> Option<Connection> {
        match self {
            PoolHandle::PerConn(p) => p.take_idle(addr, proto),
            PoolHandle::Shared(p) => p.take_idle(addr, proto),
        }
    }

    pub async fn acquire(
        &mut self,
        addr: SocketAddr,
        proto: Proto<'_>,
        keepalive: &TcpKeepalive,
    ) -> Result<(Connection, bool), ConnectError> {
        match self {
            PoolHandle::PerConn(p) => p.acquire(addr, proto, keepalive).await,
            PoolHandle::Shared(p) => p.acquire(addr, proto, keepalive).await,
        }
    }

    pub async fn acquire_fresh(
        &self,
        addr: SocketAddr,
        proto: Proto<'_>,
        keepalive: &TcpKeepalive,
    ) -> Result<Connection, ConnectError> {
        match self {
            PoolHandle::PerConn(p) => p.acquire_fresh(addr, proto, keepalive).await,
            PoolHandle::Shared(p) => p.acquire_fresh(addr, proto, keepalive).await,
        }
    }

    pub fn release(&mut self, addr: SocketAddr, proto: Proto<'_>, conn: Connection) {
        match self {
            PoolHandle::PerConn(p) => p.release(addr, proto, conn),
            PoolHandle::Shared(p) => p.release(addr, proto, conn),
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

    /// One throttle covers every backend: the count a line carries is not
    /// the named backend's. It read "N similar suppressed" after one address.
    #[test]
    fn exhaustion_line_counts_every_backend() {
        use crate::proxy::test_util::{in_own_process, Captured};
        if !in_own_process(module_path!(), "exhaustion_line_counts_every_backend") {
            return;
        }
        let (log, _subscriber) = Captured::warnings();
        let e = io::Error::from_raw_os_error(libc::EMFILE);
        log_local_exhaustion("127.0.0.1:1".parse().unwrap(), &e);
        log_local_exhaustion("127.0.0.1:2".parse().unwrap(), &e);
        let log = log.text();
        assert_eq!(log.matches("not reached").count(), 1, "{log}");
        assert!(
            log.contains("Backend 127.0.0.1:1 not reached")
                && log.contains("; 0 more since the last such line, any backend"),
            "{log}"
        );
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
        let mut pool = BackendPool::new(4, Duration::from_secs(2));
        let keepalive = TcpKeepalive::new();
        let (mut conn, _) = pool.acquire(addr, Proto::Plain, &keepalive).await.unwrap();
        conn.write_all(b"ping").await.unwrap();
        // Read as the engine does, with room to spare: a short read tells
        // tokio the socket is drained.
        let (mut pong, mut got) = ([0u8; 64], 0);
        while got < 4 {
            got += conn.read(&mut pong[got..]).await.unwrap();
        }
        pool.release(addr, Proto::Plain, conn);

        let before = peek_syscalls();
        let (_, reused) = pool.acquire(addr, Proto::Plain, &keepalive).await.unwrap();
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
        let (client, server) = tokio::join!(tls_connector().connect(sni, local), async {
            acceptor.accept(remote).await
        });
        let (mut client, mut server) = (client.unwrap(), server.unwrap());
        server.write_all(b"x").await.unwrap();
        server.flush().await.unwrap();
        client.read_exact(&mut [0u8; 1]).await.unwrap();
        let client = Connection::ClientTls {
            inner: Box::new(client),
        };
        Some((client, server))
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
            let (mut conn, _) = pool
                .acquire(tls, Proto::Tls("a.test"), &keepalive)
                .await
                .unwrap();
            conn.read_exact(&mut [0u8; 1]).await.unwrap();
            pool.release(tls, Proto::Tls("a.test"), conn);
            let (_, reused) = pool
                .acquire(tls, Proto::Tls("b.test"), &keepalive)
                .await
                .unwrap();
            assert!(!reused, "a conn opened for a.test served b.test");
            let (_, reused) = pool
                .acquire(tls, Proto::Tls("a.test"), &keepalive)
                .await
                .unwrap();
            assert!(reused, "a.test lost its pooled conn");

            // One address, both protocols: neither kind serves the other.
            let (conn, _) = pool.acquire(tls, Proto::Plain, &keepalive).await.unwrap();
            pool.release(tls, Proto::Plain, conn);
            let (mut conn, reused) = pool
                .acquire(tls, Proto::Tls("a.test"), &keepalive)
                .await
                .unwrap();
            assert!(!reused, "a plain conn served a TLS request");
            conn.read_exact(&mut [0u8; 1]).await.unwrap();
            pool.release(tls, Proto::Tls("a.test"), conn);
            let (_, reused) = pool.acquire(tls, Proto::Plain, &keepalive).await.unwrap();
            assert!(reused, "the plain conn was lost");
            let (_, reused) = pool.acquire(tls, Proto::Plain, &keepalive).await.unwrap();
            assert!(!reused, "a TLS conn served a plain request");

            let (conn, _) = pool
                .acquire(plain_addr, Proto::Plain, &keepalive)
                .await
                .unwrap();
            pool.release(plain_addr, Proto::Plain, conn);
            let (_, reused) = pool
                .acquire(plain_addr, Proto::Plain, &keepalive)
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

    /// A conn is handed back only to what asks exactly as it was opened:
    /// never one verified under a policy to another policy or to a request
    /// that verifies nothing, never an unverified one to a policy, even
    /// when a server name spells a policy's key.
    #[tokio::test]
    async fn a_verified_conn_serves_its_own_policy_only() {
        use crate::config::{BackendCa, BackendTls};
        use crate::proxy::testkit::{TestCa, GET_A as GET, OK};
        let ca = TestCa::new();
        let (backend, _seen) = ca.backend(&["a.test", "b.test"], &[], OK.as_bytes()).await;
        let bundles = [("ca".to_string(), ca.pem())].into_iter().collect();
        let policy = |hostname: &str| {
            let tls = BackendTls {
                hostname: hostname.to_string(),
                subject_alt_names: vec![],
                ca: BackendCa::Bundle("ca".to_string()),
            };
            Verify::of(&tls, &bundles).unwrap()
        };
        let (a, b) = (policy("a.test"), policy("b.test"));
        let keepalive = TcpKeepalive::new();
        let timeout = Duration::from_secs(2);
        for mut pool in [
            PoolHandle::PerConn(BackendPool::new(4, timeout)),
            PoolHandle::Shared(Arc::new(SharedBackendPool::new(4, timeout))),
        ] {
            let reused = async |pool: &mut PoolHandle, proto: Proto<'_>, keep: bool| {
                use tokio::io::AsyncReadExt;
                let (mut conn, reused) = pool.acquire(backend, proto, &keepalive).await.unwrap();
                if keep {
                    // As a conn comes back in use: an exchange done, and
                    // with it the session tickets read.
                    conn.write_all(GET).await.unwrap();
                    conn.read_exact(&mut [0u8; OK.len()]).await.unwrap();
                    pool.release(backend, proto, conn);
                }
                reused
            };
            assert!(!reused(&mut pool, Proto::Verified(&a), true).await);
            for other in [
                Proto::Verified(&b),
                Proto::Tls("a.test"),
                Proto::Tls(a.key()),
                Proto::Plain,
            ] {
                assert!(!reused(&mut pool, other, false).await);
            }
            assert!(reused(&mut pool, Proto::Verified(&a), false).await);

            assert!(!reused(&mut pool, Proto::Tls(a.key()), true).await);
            assert!(!reused(&mut pool, Proto::Verified(&a), false).await);
            assert!(reused(&mut pool, Proto::Tls(a.key()), false).await);
        }
    }
}
