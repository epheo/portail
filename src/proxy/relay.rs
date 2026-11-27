//! Opaque bytes both ways: TCP routes, TLS passthrough and upgraded
//! tunnels.

use std::borrow::Cow;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::logging::warn;
use crate::metrics::ListenerStats;
use crate::proxy::backend_pool::{connect_tcp, log_local_exhaustion, ConnectError, LogThrottle};
use crate::proxy::deadline::idle_opt;
use crate::proxy::health::HealthRegistry;
use crate::proxy::listener::{tcp_keepalive, ServeConfig};
use crate::proxy::proxy_protocol;
use crate::proxy::request_processor::report_passthrough_miss;
use crate::proxy::tls::Connection;
use crate::routing::{BackendAction, BackendSelector, RouteTable};

/// L4 TLS passthrough of a connection whose ClientHello was peeked for its
/// `sni` (lowercase), which selected the passthrough listener with
/// hostname `listener`: forward raw TCP to its route's backend. One its
/// routes send nowhere closes, and [`report_passthrough_miss`] says why.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_tls_passthrough(
    client: TcpStream,
    (peer, local): (SocketAddr, SocketAddr),
    sni: Option<&str>,
    listener: Option<&str>,
    server_port: u16,
    routes: Arc<ArcSwap<RouteTable>>,
    health: Arc<HealthRegistry>,
    selector: Arc<BackendSelector>,
    cfg: &ServeConfig,
    stats: &ListenerStats,
) {
    // The table is let go before the relay: held across it, every open
    // connection would keep the table it started under alive.
    let resolved = routes
        .load()
        .resolve_tls_passthrough(sni, server_port, listener, &selector, &health)
        .map(|backend| {
            let proxied = backend.action == BackendAction::ForwardProxied;
            (backend.socket_addr, proxied)
        });
    let (backend_addr, proxied) = match resolved {
        Ok(backend) => backend,
        Err(miss) => return report_passthrough_miss(miss, sni, server_port),
    };
    let header = proxied.then(|| proxy_protocol::header(peer, local));

    // The peek consumed nothing, so the ClientHello is still on the socket:
    // the generic L4 forward replays it to the backend via the normal reads.
    handle_tcp_connection(
        Connection::Plain { inner: client },
        backend_addr,
        header,
        &[],
        health,
        cfg.connect_timeout,
        &tcp_keepalive(cfg.tcp_keepalive_time),
        idle_opt(cfg.idle_body_timeout),
        "TLS passthrough",
        stats,
    )
    .await
}

/// Relay `client` to `backend_addr`. `header`, a PROXY v2 header for a
/// proxied backend, goes before any byte of the client's. Raw TCP has no
/// status to answer with: a backend that cannot be reached, or takes no
/// first byte, closes the client, and its count and log line are all an
/// operator gets.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_tcp_connection(
    client: Connection,
    backend_addr: SocketAddr,
    header: Option<Vec<u8>>,
    initial_data: &[u8],
    health: Arc<HealthRegistry>,
    connect_timeout: Duration,
    keepalive: &socket2::TcpKeepalive,
    idle: Option<Duration>,
    ctx: &'static str,
    stats: &ListenerStats,
) {
    let mut backend = match connect_tcp(backend_addr, connect_timeout, keepalive).await {
        Ok(backend) => backend,
        Err(ConnectError::Local(e)) => return log_local_exhaustion(backend_addr, &e),
        Err(e) => {
            crate::metrics::record_backend_connect_error(&backend_addr.to_string());
            stats.upstream_connect_errors.inc();
            return fail_l4_backend(health, backend_addr, ctx, "connect failed", &e);
        }
    };
    health.record_success(backend_addr);
    let first = match header {
        Some(mut first) => {
            first.extend_from_slice(initial_data);
            Cow::Owned(first)
        }
        None => Cow::Borrowed(initial_data),
    };
    if let Err(e) = backend.write_all(&first).await {
        crate::metrics::record_backend_send_error(&backend_addr.to_string());
        stats.upstream_send_errors.inc();
        return fail_l4_backend(health, backend_addr, ctx, "took no first byte", &e);
    }
    both_ways(client, backend, idle).await;
}

/// Record an L4 backend's failure for its health, and log it once per
/// window: the client picks how often it connects, and the counters carry
/// every failure.
fn fail_l4_backend(
    health: Arc<HealthRegistry>,
    addr: SocketAddr,
    ctx: &str,
    what: &str,
    e: &dyn std::fmt::Display,
) {
    static FAILURES: LogThrottle = LogThrottle::new();
    if let Some(suppressed) = FAILURES.admit_now() {
        warn!(
            "{} backend {} {}: {}; {} more L4 backend failures since the last such line",
            ctx, addr, what, e, suppressed
        );
    }
    if health.record_failure(addr) {
        HealthRegistry::spawn_probe(health, addr);
    }
}

/// What a relay direction reads at a time: little to start, so an idle
/// tunnel holds little, then doubled up to `RELAY_BUF_MAX` while reads fill
/// it and the other side takes each whole. Bulk to a fast receiver grows at
/// once; a chat never does, and a receiver slower than its sender stops
/// where its socket does. On a TCP route, 8 KiB reads cost 226 ms of proxy
/// CPU per GiB and 64 KiB reads 97 ms: fewer syscalls for the same bytes.
const RELAY_BUF: usize = 8 * 1024;

const RELAY_BUF_MAX: usize = 64 * 1024;

/// How long a direction waits for bytes on a grown buffer before giving it
/// back. A read that would have fitted `RELAY_BUF` gives it back at once.
/// Either way a tunnel quiet or chatty after a burst holds what one that
/// never grew does: a pod holds thousands of them.
const RELAY_BUF_IDLE: Duration = Duration::from_secs(1);

/// Copy both ways until both directions end. While both are open the relay
/// is unbounded, like any idle TCP session; once one side has finished, the
/// other must make progress in every `idle` window (None: unbounded), or a
/// peer that never closes would pin the task and both fds forever. Errors on
/// either side end both: the transfer is best-effort once started.
pub(super) async fn both_ways<A, B>(a: A, b: B, idle: Option<Duration>)
where
    A: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut a_rd, mut a_wr) = tokio::io::split(a);
    let (mut b_rd, mut b_wr) = tokio::io::split(b);
    let steps = AtomicU64::new(0);
    let a_to_b = pump(&mut a_rd, &mut b_wr, &steps);
    let b_to_a = pump(&mut b_rd, &mut a_wr, &steps);
    tokio::pin!(a_to_b, b_to_a);
    let a_done = tokio::select! {
        done = &mut a_to_b => if done.is_ok() { true } else { return },
        done = &mut b_to_a => if done.is_ok() { false } else { return },
    };
    if a_done {
        finish_half(b_to_a, idle, &steps).await;
    } else {
        finish_half(a_to_b, idle, &steps).await;
    }
}

/// Drive the direction still open until it ends, or until a whole `idle`
/// window passes without a step.
async fn finish_half<F: std::future::Future>(
    mut half: std::pin::Pin<&mut F>,
    idle: Option<Duration>,
    steps: &AtomicU64,
) {
    let Some(idle) = idle else {
        half.await;
        return;
    };
    loop {
        let seen = steps.load(Ordering::Relaxed);
        if tokio::time::timeout(idle, half.as_mut()).await.is_ok()
            || steps.load(Ordering::Relaxed) == seen
        {
            return;
        }
    }
}

/// A read into `buf`, or `None` once it has waited `idle` for bytes. Bytes
/// already there come back before any clock is read: a timeout built up
/// front reads it on every call, which a relay under load makes per chunk.
async fn read_or_idle<R>(
    rd: &mut R,
    buf: &mut [u8],
    idle: Duration,
) -> Option<std::io::Result<usize>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use std::{future::Future, task::Poll};
    let mut read = std::pin::pin!(rd.read(buf));
    let now = std::future::poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx))).await;
    match now {
        Poll::Ready(read) => Some(read),
        Poll::Pending => tokio::time::timeout(idle, read).await.ok(),
    }
}

/// One direction of a relay: copy until EOF, then pass the half-close on.
/// Every read and every write counts a step, which `finish_half` watches: a
/// count, not a time, so the copy reads no clock.
async fn pump<R, W>(rd: &mut R, wr: &mut W, steps: &AtomicU64) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    // Both directions run on one task: a plain add, no locked one.
    let step = || steps.store(steps.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
    let mut buf = vec![0u8; RELAY_BUF];
    loop {
        let n = if buf.len() == RELAY_BUF {
            rd.read(&mut buf).await?
        } else {
            match read_or_idle(rd, &mut buf, RELAY_BUF_IDLE).await {
                Some(read) => read?,
                None => {
                    buf = vec![0u8; RELAY_BUF];
                    continue;
                }
            }
        };
        step();
        if n == 0 {
            return wr.shutdown().await;
        }
        let (mut written, mut writes) = (0, 0);
        while written < n {
            written += match wr.write(&buf[written..n]).await? {
                0 => return Err(std::io::ErrorKind::WriteZero.into()),
                wrote => wrote,
            };
            writes += 1;
            step();
        }
        // TLS holds written records until flushed.
        wr.flush().await?;
        if n == buf.len() && writes == 1 && n < RELAY_BUF_MAX {
            buf.resize((n * 2).min(RELAY_BUF_MAX), 0);
        } else if n <= RELAY_BUF && buf.len() > RELAY_BUF {
            buf = vec![0u8; RELAY_BUF];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::testkit::client_hello;
    use crate::routing::Backend;
    use std::pin::Pin;
    use tokio::net::TcpListener;

    /// What a relay direction reads next: `Some(n)` bytes, the whole buffer
    /// it is offered when `n` is larger, then silence. Records each buffer
    /// size it is offered.
    struct Scripted {
        reads: std::collections::VecDeque<usize>,
        offered: Arc<std::sync::Mutex<Vec<usize>>>,
    }

    impl tokio::io::AsyncRead for Scripted {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            self.offered.lock().unwrap().push(buf.remaining());
            let Some(n) = self.reads.pop_front() else {
                return std::task::Poll::Pending;
            };
            let n = n.min(buf.remaining());
            buf.initialize_unfilled_to(n);
            buf.advance(n);
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// Takes half of every write: a receiver slower than its sender.
    struct HalfWrites;

    impl tokio::io::AsyncWrite for HalfWrites {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Ok(buf.len().div_ceil(2)))
        }
        fn poll_flush(
            self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// The buffer sizes `pump` offers over `reads`, each once, run for
    /// `wait` after the script ends in silence.
    async fn offered<W: tokio::io::AsyncWrite + Unpin>(
        reads: &[usize],
        mut wr: W,
        wait: Duration,
    ) -> Vec<usize> {
        let sizes = Arc::default();
        let mut rd = Scripted {
            reads: reads.iter().copied().collect(),
            offered: Arc::clone(&sizes),
        };
        let steps = AtomicU64::new(0);
        let relayed = tokio::time::timeout(wait, pump(&mut rd, &mut wr, &steps)).await;
        assert!(relayed.is_err(), "the script ends in silence");
        let mut sizes = sizes.lock().unwrap().clone();
        sizes.dedup();
        sizes
    }

    /// A relay direction reads more at a time while a burst fills its
    /// buffer and the other side takes each read whole, and goes back to
    /// the small buffer after it: thousands of tunnels each hold theirs.
    #[tokio::test(start_paused = true)]
    async fn a_relay_buffer_grows_with_a_burst_and_is_given_back_after() {
        const K: usize = 1024;
        const FULL: usize = usize::MAX;
        let burst = [FULL; 5];
        let grown = [8 * K, 16 * K, 32 * K, 64 * K];

        // Silence after the burst: given back once it has waited.
        let sizes = offered(&burst, tokio::io::sink(), RELAY_BUF_IDLE / 2).await;
        assert_eq!(sizes, grown, "still grown before the wait is over");
        let sizes = offered(&burst, tokio::io::sink(), RELAY_BUF_IDLE * 2).await;
        assert_eq!(sizes, [8 * K, 16 * K, 32 * K, 64 * K, 8 * K]);

        // A chat after the burst: given back at its first short read.
        let chat = [FULL, FULL, FULL, FULL, FULL, 100];
        let sizes = offered(&chat, tokio::io::sink(), RELAY_BUF_IDLE / 2).await;
        assert_eq!(sizes, [8 * K, 16 * K, 32 * K, 64 * K, 8 * K]);

        // Reads that would not fit the small buffer keep the grown one.
        let stream = [FULL, FULL, FULL, FULL, 20 * K, 20 * K];
        let sizes = offered(&stream, tokio::io::sink(), RELAY_BUF_IDLE / 2).await;
        assert_eq!(sizes, grown);

        // A receiver that takes writes in pieces: no growth to begin with.
        let sizes = offered(&burst, HalfWrites, RELAY_BUF_IDLE / 2).await;
        assert_eq!(sizes, [8 * K]);
    }

    /// A passthrough connection that drew an invalid backendRef's share
    /// was said to find no route or no healthy backend.
    #[tokio::test]
    async fn tls_passthrough_says_why_it_has_no_backend() {
        let mut rt = RouteTable::new();
        rt.add_tls_route(0, "rejected.test", vec![Backend::rejecting(1)]);
        rt.add_tls_route(0, "unreported.test", vec![Backend::unavailable(1)]);
        let mut weightless = Backend::new("127.0.0.1".to_string(), 9).unwrap();
        weightless.weight = 0;
        rt.add_tls_route(0, "weightless.test", vec![weightless]);
        rt.add_tls_route(0, "empty.test", vec![]);
        let routes = Arc::new(ArcSwap::from_pointee(rt));
        let cfg = ServeConfig {
            max_idle_per_backend: 4,
            connect_timeout: Duration::from_secs(2),
            client_header_timeout: Duration::from_secs(2),
            tcp_keepalive_time: Duration::from_secs(60),
            idle_body_timeout: Duration::from_secs(5),
            http2: false,
        };
        for (sni, why) in [
            ("rejected.test", "drew an invalid backendRef's share"),
            ("unreported.test", "before any instance reported"),
            ("weightless.test", "no backend with positive weight"),
            ("empty.test", "no backends available"),
            ("unrouted.test", "no route"),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            client.write_all(&client_hello(sni)).await.unwrap();
            let (server, peer) = listener.accept().await.unwrap();
            let local = server.local_addr().unwrap();
            let (health, selector): (Arc<HealthRegistry>, Arc<BackendSelector>) =
                Default::default();
            let miss = routes
                .load()
                .resolve_tls_passthrough(Some(sni), 8443, None, &selector, &health)
                .unwrap_err()
                .to_string();
            assert!(miss.contains(why), "{sni}: {miss}");
            // The handler closes it, with nothing left to report.
            handle_tls_passthrough(
                server,
                (peer, local),
                Some(sni),
                None,
                8443,
                Arc::clone(&routes),
                health,
                selector,
                &cfg,
                &ListenerStats::default(),
            )
            .await;
            // A reset: its ClientHello was never read.
            let read = client.read(&mut [0u8; 8]).await;
            assert!(!matches!(read, Ok(n) if n > 0), "{sni}: {read:?}");
        }
    }
}
