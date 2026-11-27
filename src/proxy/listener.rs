//! One listening socket: its accept loop, and the dispatch of each
//! connection it accepts by what its port serves.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use crate::logging::{debug, info, warn};
use crate::metrics::{ListenerStats, METRICS};
use crate::proxy::backend_pool::{BackendPool, PoolHandle, SharedBackendPool};
use crate::proxy::health::HealthRegistry;
use crate::proxy::http1::{self, ConnectionState};
use crate::proxy::proxy_protocol::{self, Header};
use crate::proxy::relay::handle_tls_passthrough;
use crate::proxy::tls::{self, Connection, Hello, TlsKind, TlsPlan};
use crate::routing::{BackendSelector, RouteTable};

/// Per-listener worker tunables, threaded from `PerformanceConfig` by the
/// data plane.
#[derive(Clone, Copy)]
pub struct ServeConfig {
    /// Idle backend connections retained per backend in each connection's pool.
    pub max_idle_per_backend: usize,
    /// Default backend connect budget (rule-level timeouts override per request).
    pub connect_timeout: Duration,
    /// Client TLS-handshake / request-header-completion budget (slow-loris
    /// guard), and the bound on a connection's close. Does not bound the
    /// idle wait between keepalive requests.
    pub client_header_timeout: Duration,
    /// Idle time before the kernel's first TCP keepalive probe on accepted
    /// sockets (see [`crate::config::PerformanceConfig::tcp_keepalive_time`]).
    pub tcp_keepalive_time: Duration,
    /// Per-step progress budget on body streaming (see
    /// [`crate::config::PerformanceConfig::idle_body_timeout`]); zero disables.
    pub idle_body_timeout: Duration,
    /// Advertise h2 via ALPN on TLS-terminate listeners and bridge h2
    /// streams through this engine (see `h2_bridge`). Off by default.
    pub http2: bool,
}

impl ServeConfig {
    pub fn from_perf(perf: &crate::config::PerformanceConfig, max_idle_per_backend: usize) -> Self {
        Self {
            max_idle_per_backend,
            connect_timeout: perf.backend_timeout,
            client_header_timeout: perf.client_header_timeout,
            tcp_keepalive_time: perf.tcp_keepalive_time,
            idle_body_timeout: perf.idle_body_timeout,
            http2: perf.http2,
        }
    }
}

/// Probe cadence once `tcp_keepalive_time` idle has elapsed: a peer that
/// answers none of `TCP_KEEPALIVE_RETRIES` probes spaced
/// `TCP_KEEPALIVE_INTERVAL` apart is reaped by the kernel (the parked read
/// returns ETIMEDOUT, dropping the task, its pooled backend connection, and
/// both fds). Fixed rather than configurable: reap latency is dominated by
/// the configurable idle threshold, and 10s x 5 keeps the tail short (~50s)
/// without being trigger-happy over one dropped ACK.
pub const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);

pub const TCP_KEEPALIVE_RETRIES: u32 = 5;

/// The keepalive of accepted and backend sockets alike: a dead backend pins
/// an idle client's task and both fds as surely as a dead client.
pub(crate) fn tcp_keepalive(time: Duration) -> socket2::TcpKeepalive {
    socket2::TcpKeepalive::new()
        .with_time(time)
        .with_interval(TCP_KEEPALIVE_INTERVAL)
        .with_retries(TCP_KEEPALIVE_RETRIES)
}

/// Whether `sock` carries what every client socket needs: TCP_NODELAY and
/// the keepalive of `tcp_keepalive(time)`. Linux copies both from a listener
/// to every socket it accepts: the data plane sets them before listen(), and
/// then accepting costs no setsockopt per connection.
pub(super) fn carries_client_options(sock: socket2::SockRef<'_>, time: Duration) -> bool {
    sock.tcp_nodelay().unwrap_or(false)
        && sock.keepalive().unwrap_or(false)
        && sock.tcp_keepalive_time().ok() == Some(time)
        && sock.tcp_keepalive_interval().ok() == Some(TCP_KEEPALIVE_INTERVAL)
        && sock.tcp_keepalive_retries().ok() == Some(TCP_KEEPALIVE_RETRIES)
}

/// Pause before retrying a failed accept(). EMFILE, ENFILE, ENOBUFS and
/// ENOMEM leave the listener readable, so an immediate retry fails the same
/// way: the loop spins a core and logs every turn until resources free up.
#[derive(Default)]
pub(crate) struct AcceptBackoff {
    delay: Option<Duration>,
}

impl AcceptBackoff {
    // Go net/http's schedule: a blip costs a few ms, a sustained shortage
    // one retry (and one log line) a second.
    const FIRST: Duration = Duration::from_millis(5);
    const MAX: Duration = Duration::from_secs(1);

    /// The pause owed after `error`; `None` when only the connection being
    /// accepted failed, which leaves the listener healthy: aborted while
    /// queued, or a network error Linux accept(2) passes on from it, which
    /// its man page says to retry at once like EAGAIN.
    pub(crate) fn after(&mut self, error: &std::io::Error) -> Option<Duration> {
        use std::io::ErrorKind::{ConnectionAborted, ConnectionReset};
        if matches!(error.kind(), ConnectionAborted | ConnectionReset)
            || matches!(
                error.raw_os_error(),
                Some(
                    libc::ENETDOWN
                        | libc::EPROTO
                        | libc::ENOPROTOOPT
                        | libc::EHOSTDOWN
                        | libc::ENONET
                        | libc::EHOSTUNREACH
                        | libc::EOPNOTSUPP
                        | libc::ENETUNREACH
                )
            )
        {
            return None;
        }
        let delay = self.delay.map_or(Self::FIRST, |d| (d * 2).min(Self::MAX));
        self.delay = Some(delay);
        Some(delay)
    }

    pub(crate) fn reset(&mut self) {
        self.delay = None;
    }
}

/// RAII in-flight-connection counter. One guard lives inside each spawned
/// per-connection task; `data_plane::Drain::run` polls the shared count and
/// drains until it reaches zero (or the drain timeout expires). Drop-based so
/// the count stays correct on every exit path, including panics and aborts.
struct ConnGuard {
    active: Arc<AtomicUsize>,
    stats: Arc<ListenerStats>,
}

impl ConnGuard {
    fn new(counter: Arc<AtomicUsize>, stats: Arc<ListenerStats>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        METRICS.connections_accepted_total.inc();
        METRICS.active_connections.inc();
        stats.accepted.inc();
        stats.active.inc();
        Self {
            active: counter,
            stats,
        }
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        // Release pairs with the Acquire load in the shutdown drain loop.
        self.active.fetch_sub(1, Ordering::Release);
        METRICS.active_connections.dec();
        self.stats.active.dec();
    }
}

/// Pool handle for one client connection: the process-wide shared pool when
/// `performance.backendPoolScope: process` is set, else a fresh
/// per-connection pool (the default).
pub(crate) fn make_pool(shared: &Option<Arc<SharedBackendPool>>, cfg: &ServeConfig) -> PoolHandle {
    match shared {
        Some(p) => PoolHandle::Shared(p.clone()),
        None => PoolHandle::PerConn(BackendPool::new(
            cfg.max_idle_per_backend,
            cfg.connect_timeout,
        )),
    }
}

/// Everything an accept loop shares across its connections, bundled so the
/// data plane's two spawn sites thread one value instead of eleven arguments.
pub struct ListenerCtx {
    pub server_port: u16,
    pub routes: Arc<ArcSwap<RouteTable>>,
    pub cfg: ServeConfig,
    /// Read per connection: plain, or a TLS dispatch.
    pub plan: crate::proxy::data_plane::PortPlan,
    pub health: Arc<HealthRegistry>,
    pub selector: Arc<BackendSelector>,
    pub active_connections: Arc<AtomicUsize>,
    pub shared_pool: Option<Arc<SharedBackendPool>>,
}

/// Run an accept loop on a single listener, spawning a task per connection.
pub async fn serve(listener: TcpListener, ctx: ListenerCtx, shutdown: CancellationToken) {
    let ListenerCtx {
        server_port,
        routes,
        cfg,
        plan,
        health,
        selector,
        active_connections,
        shared_pool,
    } = ctx;
    // Per-port scrape identity: lets an operator answer "is anything accepting
    // on this port" from /metrics alone, without exec'ing into the pod. The
    // guard flips `portail_listener_up` to 0 when this loop exits.
    let (listener_stats, _up_guard) = crate::metrics::register_listener("tcp", server_port);

    // Selector is shared across all listeners so weighted round-robin counters
    // produce the correct distribution globally.

    // Dead-peer detection: a client that vanishes without FIN/RST would
    // otherwise park its connection task forever, since the idle wait
    // between keepalive requests is intentionally unbounded.
    let keepalive = tcp_keepalive(cfg.tcp_keepalive_time);
    // Read on the first socket accepted, not on the listener: what a kernel
    // copies on accept is then seen, not assumed.
    let mut options_inherited = None;

    let mut backoff = AcceptBackoff::default();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!("Listener on :{} shutting down", server_port);
                break;
            }
            result = listener.accept() => {
                match result {
                    Ok((tcp_stream, peer)) => {
                        backoff.reset();
                        let inherited = *options_inherited.get_or_insert_with(|| {
                            carries_client_options((&tcp_stream).into(), cfg.tcp_keepalive_time)
                        });
                        if !inherited {
                            let sock = socket2::SockRef::from(&tcp_stream);
                            // _-prefixed: debug! compiles out in release builds.
                            if let Err(_e) = sock
                                .set_tcp_keepalive(&keepalive)
                                .and_then(|()| sock.set_tcp_nodelay(true))
                            {
                                debug!("TCP option setup failed for {}: {}", peer, _e);
                            }
                        }
                        let routes = routes.clone();
                        let health = health.clone();
                        let selector = selector.clone();
                        let shared_pool = shared_pool.clone();
                        let listener_stats = listener_stats.clone();
                        let drain = shutdown.child_token();
                        let conn_guard =
                            ConnGuard::new(active_connections.clone(), listener_stats.clone());
                        let tls = plan.load_full();
                        tokio::spawn(async move {
                            let _guard = conn_guard;
                            let mut tcp_stream = tcp_stream;
                            let timeout = cfg.client_header_timeout;
                            let Some(peer) = client_of(&mut tcp_stream, peer, &routes, timeout).await
                            else {
                                return;
                            };
                            let Ok(local) = tcp_stream.local_addr() else {
                                return;
                            };
                            if let Some(tls) = tls {
                                serve_tls_connection(
                                    tcp_stream,
                                    peer,
                                    local,
                                    tls,
                                    server_port,
                                    routes,
                                    cfg,
                                    health,
                                    selector,
                                    shared_pool,
                                    listener_stats,
                                    drain,
                                )
                                .await;
                                return;
                            }
                            let state = ConnectionState::new(
                                server_port,
                                routes,
                                make_pool(&shared_pool, &cfg),
                                selector,
                                health,
                                false,
                                peer,
                                local,
                                &cfg,
                                listener_stats,
                                drain,
                            );
                            let conn = Connection::Plain { inner: tcp_stream };
                            let _ = http1::serve(conn, peer, state).await;
                        });
                    }
                    Err(e) => {
                        if let Some(pause) = backoff.after(&e) {
                            warn!(
                                "Listener on :{} accept error: {}; retrying in {:?}",
                                server_port, e, pause
                            );
                            // Cut short by a shutdown, which the loop then sees.
                            let _ = tokio::time::timeout(pause, shutdown.cancelled()).await;
                        } else {
                            debug!("Listener on :{} accept error: {}", server_port, e);
                        }
                    }
                }
            }
        }
    }
}

/// The client a connection speaks for: its peer, or, from a trusted
/// source, the client its PROXY v2 header names. None closes it: a
/// trusted source that sends no header, a malformed one, or none in time
/// never falls back to its own address.
async fn client_of(
    stream: &mut TcpStream,
    peer: SocketAddr,
    routes: &ArcSwap<RouteTable>,
    timeout: Duration,
) -> Option<SocketAddr> {
    if !routes.load().proxy_sources.trusts(peer.ip()) {
        return Some(peer);
    }
    match tokio::time::timeout(timeout, proxy_protocol::read_header(stream)).await {
        Ok(Ok(Header::Proxy(client))) => Some(client),
        Ok(Ok(Header::Local)) => Some(peer),
        _outcome => {
            METRICS.proxy_protocol_refused_total.inc();
            debug!(
                "PROXY protocol header from {} refused: {:?}",
                peer, _outcome
            );
            None
        }
    }
}

/// One accepted connection on a TLS port: its SNI selects a listener by
/// the port's plan, read at accept. A passthrough listener forwards the raw
/// stream; an HTTPS one handshakes, then dispatches by ALPN: h2 goes to the
/// bridge, all else stays on the h1 path. A connection no listener takes,
/// or one that is not TLS, closes as a failed handshake.
#[allow(clippy::too_many_arguments)]
pub(super) async fn serve_tls_connection(
    tcp_stream: TcpStream,
    peer: SocketAddr,
    local: SocketAddr,
    plan: Arc<TlsPlan>,
    server_port: u16,
    routes: Arc<ArcSwap<RouteTable>>,
    cfg: ServeConfig,
    health: Arc<HealthRegistry>,
    selector: Arc<BackendSelector>,
    shared_pool: Option<Arc<SharedBackendPool>>,
    listener_stats: Arc<ListenerStats>,
    drain: CancellationToken,
) {
    let refuse = |_why: &str| {
        METRICS.tls_handshake_failures_total.inc();
        listener_stats.tls_handshake_failures.inc();
        debug!("TLS connection from {} closed: {}", peer, _why);
    };
    // Waits out fragmented hellos.
    let mut sni = match tls::peek_client_hello(&tcp_stream, cfg.client_header_timeout).await {
        Hello::Sni(sni) => Some(sni),
        Hello::Nameless => None,
        Hello::Unreadable => return refuse("no ClientHello"),
    };
    // In place: the peek already allocated it.
    if let Some(sni) = sni.as_mut() {
        sni.make_ascii_lowercase();
    }
    let Some(listener) = plan.select(sni.as_deref()) else {
        return refuse("no listener takes its SNI");
    };
    let config = match listener.kind {
        TlsKind::Passthrough => {
            handle_tls_passthrough(
                tcp_stream,
                (peer, local),
                sni.as_deref(),
                listener.hostname.as_deref(),
                server_port,
                routes,
                health,
                selector,
                &cfg,
                &listener_stats,
            )
            .await;
            return;
        }
        TlsKind::NoCert => return refuse("its listener's certs failed to load"),
        TlsKind::Terminate => match &plan.terminate {
            Some(config) => config.clone(),
            None => return refuse("its port's certs failed to load"),
        },
    };

    // Bound the handshake: a client that stalls mid-handshake would
    // otherwise pin this task (and its buffers) forever.
    let tls_stream = match tokio::time::timeout(
        cfg.client_header_timeout,
        tokio_rustls::TlsAcceptor::from(config).accept(tcp_stream),
    )
    .await
    {
        Ok(Ok(s)) => s,
        outcome => {
            METRICS.tls_handshake_failures_total.inc();
            listener_stats.tls_handshake_failures.inc();
            match outcome {
                Ok(Err(_e)) => debug!("TLS handshake failed from {}: {}", peer, _e),
                _ => debug!("TLS handshake from {} timed out", peer),
            }
            return;
        }
    };

    // ALPN decided the protocol during the handshake.
    if cfg.http2 && tls_stream.get_ref().1.alpn_protocol() == Some(b"h2".as_ref()) {
        METRICS.http2_connections_total.inc();
        // One pool per h2 connection: the process pool when set, else a
        // connection-scoped pool its streams share (h1 keepalive semantics).
        let pool = shared_pool.unwrap_or_else(|| {
            Arc::new(SharedBackendPool::new(
                cfg.max_idle_per_backend,
                cfg.connect_timeout,
            ))
        });
        crate::proxy::h2_bridge::serve(
            tls_stream,
            crate::proxy::h2_bridge::H2Ctx {
                server_port,
                routes,
                selector,
                health,
                pool,
                cfg,
                listener_stats,
                peer,
                local,
                drain,
            },
        )
        .await;
        return;
    }
    let conn = Connection::Tls {
        inner: Box::new(tls_stream),
    };
    let state = ConnectionState::new(
        server_port,
        routes,
        make_pool(&shared_pool, &cfg),
        selector,
        health,
        true,
        peer,
        local,
        &cfg,
        listener_stats,
        drain,
    );
    let _ = http1::serve(conn, peer, state).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_backoff_doubles_to_a_cap_and_resets_on_success() {
        let emfile = std::io::Error::from_raw_os_error(libc::EMFILE);
        let mut backoff = AcceptBackoff::default();
        let pauses: Vec<u128> = (0..10)
            .map(|_| backoff.after(&emfile).unwrap().as_millis())
            .collect();
        assert_eq!(pauses, [5, 10, 20, 40, 80, 160, 320, 640, 1000, 1000]);
        backoff.reset();
        assert_eq!(backoff.after(&emfile), Some(Duration::from_millis(5)));
        // Only that connection failed: retried at once, schedule kept.
        let aborted = std::io::Error::from(std::io::ErrorKind::ConnectionAborted);
        assert_eq!(backoff.after(&aborted), None);
        assert_eq!(backoff.after(&emfile), Some(Duration::from_millis(10)));
    }

    /// Linux accept(2) returns the queued connection's pending network
    /// error; its man page says to retry those at once, like EAGAIN.
    #[test]
    fn accept_retries_a_queued_connections_network_error_at_once() {
        let emfile = std::io::Error::from_raw_os_error(libc::EMFILE);
        let mut backoff = AcceptBackoff::default();
        assert_eq!(backoff.after(&emfile), Some(Duration::from_millis(5)));
        for errno in [
            libc::ENETDOWN,
            libc::EPROTO,
            libc::ENOPROTOOPT,
            libc::EHOSTDOWN,
            libc::ENONET,
            libc::EHOSTUNREACH,
            libc::EOPNOTSUPP,
            libc::ENETUNREACH,
        ] {
            let error = std::io::Error::from_raw_os_error(errno);
            assert_eq!(backoff.after(&error), None, "{error}");
        }
        assert_eq!(backoff.after(&emfile), Some(Duration::from_millis(10)));
    }
}
