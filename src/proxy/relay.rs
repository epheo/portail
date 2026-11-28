//! Opaque bytes both ways: TCP routes, TLS passthrough and upgraded
//! tunnels.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::logging::warn;
use crate::metrics::ListenerStats;
use crate::proxy::backend_pool::{connect_tcp, log_local_exhaustion, ConnectError, LogThrottle};
use crate::proxy::deadline::idle_opt;
use crate::proxy::health::HealthRegistry;
use crate::proxy::listener::{tcp_keepalive, ServeConfig};
use crate::proxy::proxy_protocol;
use crate::proxy::request_processor::{
    analyze_tcp_request, report_passthrough_miss, RoutingResult,
};
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
        Vec::new(),
        health,
        cfg.connect_timeout,
        &tcp_keepalive(cfg.tcp_keepalive_time),
        idle_opt(cfg.idle_body_timeout),
        "TLS passthrough",
        stats,
    )
    .await
}

/// A connection on a port with TCP routes and no HTTP route: routed by its
/// port alone and relayed before the client has sent anything. There is
/// nothing to sniff, and server-speaks-first protocols (MySQL, SMTP, FTP,
/// IMAP, POP3) send nothing before the backend's banner: a proxy waiting
/// for their first bytes would deadlock them.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_tcp_route(
    client: Connection,
    (peer, local): (SocketAddr, SocketAddr),
    server_port: u16,
    routes: &ArcSwap<RouteTable>,
    health: Arc<HealthRegistry>,
    selector: &BackendSelector,
    cfg: &ServeConfig,
    stats: &ListenerStats,
) {
    // The table is let go before the relay, as for a passthrough.
    let routed = analyze_tcp_request(&routes.load(), selector, server_port, &health);
    let Ok(RoutingResult::TcpForward {
        backend_addr,
        proxied,
    }) = routed
    else {
        return; // the router logged why
    };
    handle_tcp_connection(
        client,
        backend_addr,
        proxied.then(|| proxy_protocol::header(peer, local)),
        Vec::new(),
        health,
        cfg.connect_timeout,
        &tcp_keepalive(cfg.tcp_keepalive_time),
        idle_opt(cfg.idle_body_timeout),
        "TCP",
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
    initial_data: Vec<u8>,
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
            first.extend_from_slice(&initial_data);
            first
        }
        None => initial_data,
    };
    if let Err(e) = backend.write_all(&first).await {
        crate::metrics::record_backend_send_error(&backend_addr.to_string());
        stats.upstream_send_errors.inc();
        return fail_l4_backend(health, backend_addr, ctx, "took no first byte", &e);
    }
    // Not held for the tunnel's life: a sniffed connection's first read is
    // a whole client buffer.
    drop(first);
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

/// What a relay direction reads at a time: little to start, then doubled
/// up to `RELAY_BUF_MAX` while reads fill it and the other side takes each
/// whole. Bulk to a fast receiver grows at once; a chat never does, and a
/// receiver slower than its sender stops where its socket does, which
/// bounds what a tunnel holds while its write waits. On a TCP route, 8 KiB
/// reads cost 226 ms of proxy CPU per GiB and 64 KiB reads 97 ms: fewer
/// syscalls for the same bytes.
const RELAY_BUF: usize = 8 * 1024;

const RELAY_BUF_MAX: usize = 64 * 1024;

/// The sizes a direction reads at: `RELAY_BUF` doubled up to `RELAY_BUF_MAX`.
const SIZES: usize = (RELAY_BUF_MAX / RELAY_BUF).trailing_zeros() as usize + 1;

/// Buffers of each size a worker thread keeps for its relays.
const SPARE_PER_SIZE: usize = 4;

thread_local! {
    /// A direction borrows its buffer for a read and gives it back once the
    /// bytes are written, or at once when the read finds none. A tunnel
    /// with nothing to move holds no buffer: a pod holds thousands of
    /// them, and each held two for life.
    static SPARE: std::cell::RefCell<[Vec<Vec<u8>>; SIZES]> =
        const { std::cell::RefCell::new([const { Vec::new() }; SIZES]) };
}

fn spare_of(size: usize) -> usize {
    (size / RELAY_BUF).trailing_zeros() as usize
}

fn borrow(size: usize) -> Vec<u8> {
    #[cfg(test)]
    BORROWED.with(|n| n.set(n.get() + 1));
    SPARE
        .with_borrow_mut(|spare| spare[spare_of(size)].pop())
        .unwrap_or_else(|| vec![0u8; size])
}

fn give_back(buf: Vec<u8>) {
    #[cfg(test)]
    BORROWED.with(|n| n.set(n.get() - 1));
    SPARE.with_borrow_mut(|spare| {
        let kept = &mut spare[spare_of(buf.len())];
        if kept.len() < SPARE_PER_SIZE {
            kept.push(buf);
        }
    });
}

#[cfg(test)]
thread_local! {
    /// Buffers out of this thread's hands: borrowed and not given back.
    static BORROWED: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

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

/// A read of up to `size` bytes into a borrowed buffer, which comes back
/// with the count. While the read waits it holds none: the buffer is given
/// back each time it finds nothing, and the reader keeps the task's waker.
async fn read_borrowing<R>(rd: &mut R, size: usize) -> std::io::Result<(Vec<u8>, usize)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use std::task::Poll;
    std::future::poll_fn(|cx| {
        let mut buf = borrow(size);
        let polled = {
            let mut read = tokio::io::ReadBuf::new(&mut buf);
            std::pin::Pin::new(&mut *rd)
                .poll_read(cx, &mut read)
                .map_ok(|()| read.filled().len())
        };
        match polled {
            Poll::Ready(Ok(n)) => Poll::Ready(Ok((buf, n))),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => {
                give_back(buf);
                Poll::Pending
            }
        }
    })
    .await
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
    let mut size = RELAY_BUF;
    loop {
        let (buf, n) = read_borrowing(rd, size).await?;
        step();
        if n == 0 {
            give_back(buf);
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
        give_back(buf);
        if n == size && writes == 1 {
            size = (size * 2).min(RELAY_BUF_MAX);
        } else if n <= RELAY_BUF {
            size = RELAY_BUF;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::testkit::{
        backend, banner_backend, client_hello, conn_state_header, exchange, gateway_backend,
        l4_backend, read_until_closed, received, refusing_backend, tcp_route, IDLE, REFUSED,
    };
    use crate::routing::Backend;
    use std::pin::Pin;
    use tokio::io::{AsyncReadExt, DuplexStream};
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
    /// the small size at a read that would have fitted it.
    #[tokio::test(start_paused = true)]
    async fn a_relay_reads_more_at_a_time_through_a_burst() {
        const K: usize = 1024;
        const FULL: usize = usize::MAX;
        const WAIT: Duration = Duration::from_millis(100);
        let burst = [FULL; 5];
        let grown = [8 * K, 16 * K, 32 * K, 64 * K];

        let sizes = offered(&burst, tokio::io::sink(), WAIT).await;
        assert_eq!(sizes, grown);

        // A chat after the burst: back to the small size after its first
        // short read.
        let chat = [FULL, FULL, FULL, FULL, FULL, 100];
        let sizes = offered(&chat, tokio::io::sink(), WAIT).await;
        assert_eq!(sizes, [8 * K, 16 * K, 32 * K, 64 * K, 8 * K]);

        // Reads that would not fit the small size keep the grown one.
        let stream = [FULL, FULL, FULL, FULL, 20 * K, 20 * K];
        let sizes = offered(&stream, tokio::io::sink(), WAIT).await;
        assert_eq!(sizes, grown);

        // A receiver that takes writes in pieces: no growth to begin with.
        let sizes = offered(&burst, HalfWrites, WAIT).await;
        assert_eq!(sizes, [8 * K]);
    }

    /// A direction holds a buffer from a read that found bytes to the end
    /// of their write, and none while it waits: an idle tunnel held two for
    /// life, 16 KiB of the 20 it cost.
    #[tokio::test]
    async fn a_direction_waiting_for_bytes_holds_no_buffer() {
        let (mut client, relayed) = tokio::io::duplex(1024);
        let (backend, mut served) = tokio::io::duplex(1024);
        let out = || BORROWED.with(|n| n.get());
        let before = out();
        let mut relay = std::pin::pin!(both_ways(relayed, backend, None));
        // Both directions have looked for bytes and found none.
        assert!(futures::poll!(relay.as_mut()).is_pending());
        assert_eq!(
            out(),
            before,
            "a buffer held by a direction with nothing to move"
        );

        client.write_all(b"ping").await.unwrap();
        assert!(futures::poll!(relay.as_mut()).is_pending());
        let mut got = [0u8; 4];
        served.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"ping");
        assert_eq!(out(), before, "a buffer kept after its bytes were written");
    }

    /// A thread keeps a few buffers of each size and lets the rest go.
    #[test]
    fn a_thread_keeps_few_spare_buffers() {
        let kept = |size: usize| SPARE.with_borrow(|spare| spare[spare_of(size)].len());
        for size in [RELAY_BUF, RELAY_BUF_MAX] {
            let out: Vec<_> = (0..SPARE_PER_SIZE + 3).map(|_| borrow(size)).collect();
            assert!(out.iter().all(|buf| buf.len() == size));
            out.into_iter().for_each(give_back);
            assert_eq!(kept(size), SPARE_PER_SIZE);
            assert_eq!(borrow(size).len(), size);
            assert_eq!(kept(size), SPARE_PER_SIZE - 1);
        }
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

    /// A TCP-only port relaying to `rt`'s route, served in memory: the
    /// client's end, the relay's task and its listener's counters. The
    /// addresses are the testkit's, so `conn_state_header` is its header.
    fn serve_tcp_route(
        rt: RouteTable,
        idle: Option<Duration>,
    ) -> (
        DuplexStream,
        tokio::task::JoinHandle<()>,
        Arc<ListenerStats>,
    ) {
        let (client, relayed) = tokio::io::duplex(64 * 1024);
        let stats = Arc::new(ListenerStats::default());
        let cfg = ServeConfig {
            idle_body_timeout: idle.unwrap_or_default(),
            ..ServeConfig::from_perf(&Default::default(), 4)
        };
        let counted = stats.clone();
        let task = tokio::spawn(async move {
            handle_tcp_route(
                Connection::Mem {
                    inner: relayed,
                    aborted: Default::default(),
                },
                (
                    "127.0.0.1:40000".parse().unwrap(),
                    "127.0.0.1:8080".parse().unwrap(),
                ),
                8080,
                &ArcSwap::from_pointee(rt),
                Arc::default(),
                &BackendSelector::new(),
                &cfg,
                &counted,
            )
            .await
        });
        (client, task, stats)
    }

    /// Once one side has finished, a peer that never closes pinned the
    /// relay, its task and both fds forever.
    #[tokio::test]
    async fn half_closed_l4_relay_ends_after_the_idle_budget() {
        // The client is done; the backend never answers nor closes.
        let silent = l4_backend(|mut sock| async move {
            let _ = sock.read_to_end(&mut Vec::new()).await;
            std::future::pending::<()>().await;
        })
        .await;
        let (mut client, task, _) = serve_tcp_route(tcp_route(silent), Some(IDLE));
        client.write_all(b"QUIT\r\n").await.unwrap();
        client.shutdown().await.unwrap();
        let ended = tokio::time::timeout(IDLE * 10, task).await;
        assert!(ended.is_ok(), "relay pinned after the client's FIN");

        // The backend is done; the client never closes.
        let done = l4_backend(|mut sock| async move {
            let _ = sock.write_all(b"bye").await;
            let _ = sock.shutdown().await;
            std::future::pending::<()>().await;
        })
        .await;
        let (mut client, task, _) = serve_tcp_route(tcp_route(done), Some(IDLE));
        client.read_exact(&mut [0u8; 3]).await.unwrap();
        let ended = tokio::time::timeout(IDLE * 10, task).await;
        assert!(ended.is_ok(), "relay pinned after the backend's FIN");
    }

    /// The bound is on a stalled half, not on how long it runs; with both
    /// sides open, or the budget disabled, the relay stays unbounded.
    #[tokio::test]
    async fn l4_relay_bounds_only_a_stalled_half() {
        let trickle = l4_backend(|mut sock| async move {
            let _ = sock.read_to_end(&mut Vec::new()).await;
            for _ in 0..5 {
                tokio::time::sleep(IDLE / 2).await;
                if sock.write_all(b"x").await.is_err() {
                    return;
                }
            }
        })
        .await;
        let (mut client, _task, _) = serve_tcp_route(tcp_route(trickle), Some(IDLE));
        assert_eq!(exchange(&mut client, b"GO").await, "xxxxx");

        let (mut client, task, _) =
            serve_tcp_route(tcp_route(banner_backend(b"").await), Some(IDLE));
        tokio::time::sleep(IDLE * 3).await;
        assert!(!task.is_finished(), "relay cut with both sides open");
        assert_eq!(exchange(&mut client, b"ping").await, "ping");

        let silent = l4_backend(|mut sock| async move {
            let _ = sock.read_to_end(&mut Vec::new()).await;
            std::future::pending::<()>().await;
        })
        .await;
        let (mut client, task, _) = serve_tcp_route(tcp_route(silent), None);
        client.shutdown().await.unwrap();
        tokio::time::sleep(IDLE * 3).await;
        assert!(!task.is_finished(), "relay cut with the budget disabled");
    }

    /// Raw TCP has no status to answer with. A backend that refuses the
    /// connect is counted on its listener and by address, as an HTTP
    /// backend is: for a TCP route, and for TLS passthrough.
    #[tokio::test]
    async fn an_l4_backend_that_refuses_is_counted() {
        let (_held, refused) = refusing_backend();
        let by_address = || {
            let metrics = crate::metrics::render(true);
            let line = format!("backend=\"{refused}\"");
            metrics
                .lines()
                .filter(|l| {
                    l.starts_with("portail_upstream_connect_errors_total") && l.contains(&line)
                })
                .count()
        };
        assert_eq!(by_address(), 0);

        let (mut client, task, stats) = serve_tcp_route(tcp_route(refused), None);
        assert_eq!(read_until_closed(&mut client).await, "");
        task.await.unwrap();
        assert_eq!(stats.upstream_connect_errors.get(), 1);
        assert_eq!(by_address(), 1);

        let mut rt = RouteTable::new();
        rt.add_tls_route(0, "down.test", vec![gateway_backend(refused)]);
        let routes = Arc::new(ArcSwap::from_pointee(rt));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, peer) = listener.accept().await.unwrap();
        let local = server.local_addr().unwrap();
        let stats = ListenerStats::default();
        handle_tls_passthrough(
            server,
            (peer, local),
            Some("down.test"),
            None,
            8443,
            routes,
            Arc::default(),
            Arc::default(),
            &ServeConfig::from_perf(&Default::default(), 4),
            &stats,
        )
        .await;
        assert_eq!(client.read(&mut [0u8; 8]).await.unwrap(), 0);
        assert_eq!(stats.upstream_connect_errors.get(), 1);
    }

    /// Raw TCP to a Gateway backend: the header goes before any byte of
    /// the client's.
    #[tokio::test]
    async fn a_gateway_backend_hears_who_connected_before_the_bytes() {
        let (tcp, mut rx) = backend(REFUSED).await;
        let mut rt = RouteTable::new();
        rt.add_tcp_route(8080, vec![gateway_backend(tcp)]);
        let (mut client, _task, _) = serve_tcp_route(rt, None);
        client.write_all(b"hello").await.unwrap();
        client.shutdown().await.unwrap();
        assert_eq!(
            received(&mut rx).await,
            [conn_state_header(), b"hello".to_vec()].concat()
        );
    }
}
