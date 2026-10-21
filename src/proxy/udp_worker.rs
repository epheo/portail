//! UDP worker — per-listener recv_from loop with per-session connected backend sockets.
//! Sessions are identified by source address, capped (per listener, or by one
//! budget all listeners share), moved off a backend that a route-table swap
//! dropped, and expire after an idle timeout.

use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

use crate::config::PerformanceConfig;
use crate::logging::{debug, info, warn};
use crate::metrics::METRICS;
use crate::proxy::backend_pool::{log_local_exhaustion, out_of_fds_or_memory};
use crate::proxy::health::HealthRegistry;
use crate::proxy::request_processor::{analyze_udp_request, RoutingResult};
use crate::routing::{BackendSelector, RouteTable};

/// Session bounds of the UDP listener sockets. Clones share the default
/// budget, so build one per process.
#[derive(Debug, Clone)]
pub struct SessionLimits {
    pub idle_timeout: Duration,
    cap: SessionCap,
}

#[derive(Debug, Clone)]
enum SessionCap {
    /// `udpMaxSessions`, for each listener socket.
    PerListener(usize),
    /// The default, for all listener sockets together: the fds it was sized
    /// from are the whole process's.
    Shared(Arc<SessionBudget>),
}

impl SessionLimits {
    /// Reads the fd limit, so build after `privileges::raise_nofile_limit`,
    /// which `main` runs before the data plane exists.
    pub fn from_perf(perf: &PerformanceConfig) -> Self {
        Self::with_nofile(perf, nofile_soft_limit())
    }

    fn with_nofile(perf: &PerformanceConfig, nofile_soft: Option<u64>) -> Self {
        // The soft limit containers commonly start with.
        const UNKNOWN_NOFILE: u64 = 1024;
        let cap = match perf.udp_max_sessions {
            Some(max) => SessionCap::PerListener(max),
            None => {
                // A quarter of the fd budget: each session holds one fd, and
                // the rest stays for TCP, whose accept loops all fail at EMFILE.
                let quarter = nofile_soft.unwrap_or(UNKNOWN_NOFILE) / 4;
                let max = usize::try_from(quarter).unwrap_or(usize::MAX).max(1);
                SessionCap::Shared(Arc::new(SessionBudget::new(max, true)))
            }
        };
        Self {
            idle_timeout: perf.udp_session_timeout,
            cap,
        }
    }

    /// The budget one listener socket opens its sessions against.
    fn budget(&self) -> Arc<SessionBudget> {
        match &self.cap {
            SessionCap::PerListener(max) => Arc::new(SessionBudget::new(*max, false)),
            SessionCap::Shared(budget) => Arc::clone(budget),
        }
    }
}

/// Sessions open against one cap. Counted as sessions open and close, never
/// per datagram.
#[derive(Debug)]
struct SessionBudget {
    max: usize,
    open: AtomicUsize,
    shared: bool,
}

impl SessionBudget {
    fn new(max: usize, shared: bool) -> Self {
        Self {
            max,
            open: AtomicUsize::new(0),
            shared,
        }
    }

    /// A slot for one more session, or `None` when all are taken.
    fn take(self: &Arc<Self>) -> Option<SessionSlot> {
        let mut open = self.open.load(Relaxed);
        while open < self.max {
            match self
                .open
                .compare_exchange_weak(open, open + 1, Relaxed, Relaxed)
            {
                Ok(_) => return Some(SessionSlot(Arc::clone(self))),
                Err(now) => open = now,
            }
        }
        None
    }

    fn full(&self) -> bool {
        self.open.load(Relaxed) >= self.max
    }

    /// What a log line calls this cap.
    fn name(&self) -> &'static str {
        if self.shared {
            "the session budget all UDP listeners share"
        } else {
            "udpMaxSessions"
        }
    }
}

/// One session's place in its budget, given back when the session goes.
#[derive(Debug)]
struct SessionSlot(Arc<SessionBudget>);

impl Drop for SessionSlot {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Relaxed);
    }
}

fn nofile_soft_limit() -> Option<u64> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit only writes the struct we own.
    (unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } == 0).then_some(lim.rlim_cur)
}

struct UdpSession {
    backend_addr: SocketAddr,
    backend_socket: Arc<UdpSocket>,
    /// Whatever removes a session, its reply task must not live on holding
    /// the backend socket's fd and port until it idles out.
    reply_task: AbortOnDropHandle<()>,
    /// Milliseconds since the worker epoch at the last datagram either way.
    last_active: Arc<AtomicU64>,
    /// Route-table generation `backend_addr` was last found routed in.
    generation: u64,
    slot: SessionSlot,
}

/// Session table of one listener socket. Only its recv loop touches it, so
/// it needs no locking.
struct Sessions {
    listener: Arc<UdpSocket>,
    server_port: u16,
    routes: Arc<ArcSwap<RouteTable>>,
    health: Arc<HealthRegistry>,
    selector: BackendSelector,
    idle_timeout: Duration,
    budget: Arc<SessionBudget>,
    epoch: Instant,
    map: HashMap<SocketAddr, UdpSession>,
    /// The table `generation` was taken from. Weak so a swapped-out table is
    /// freed, while its allocation stays pinned: a new table allocated at the
    /// same address cannot pass for it.
    seen: Weak<RouteTable>,
    generation: u64,
    /// Reaching the cap was logged since the budget last had room.
    full_logged: bool,
}

/// Run a UDP recv_from loop on a single socket, dispatching datagrams to per-session backends.
pub async fn run_udp_worker(
    socket: Arc<UdpSocket>,
    server_port: u16,
    routes: Arc<ArcSwap<RouteTable>>,
    limits: SessionLimits,
    shutdown: CancellationToken,
    health: Arc<HealthRegistry>,
) {
    let budget = limits.budget();
    info!(
        "UDP listener on :{} started (sessions capped at {} by {})",
        server_port,
        budget.max,
        budget.name()
    );

    // Same per-port scrape identity as the TCP accept loops; `accepted`
    // counts datagrams here since UDP has no connections.
    let (listener_stats, _up_guard) = crate::metrics::register_listener("udp", server_port);

    let mut sessions = Sessions {
        listener: Arc::clone(&socket),
        server_port,
        routes,
        health,
        selector: BackendSelector::new(),
        idle_timeout: limits.idle_timeout,
        budget,
        epoch: Instant::now(),
        map: HashMap::new(),
        seen: Weak::new(),
        generation: 0,
        full_logged: false,
    };
    let mut buf = vec![0u8; 65536];

    // Floored: interval(0) panics, and panic = abort takes the pod down.
    let mut reap = tokio::time::interval((limits.idle_timeout / 2).max(Duration::from_millis(1)));
    reap.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!("UDP listener on :{} shutting down", server_port);
                break;
            }
            _ = reap.tick() => sessions.reap(),
            result = socket.recv_from(&mut buf) => match result {
                Ok((n, client_addr)) => {
                    listener_stats.accepted.inc();
                    sessions.forward(&buf[..n], client_addr).await;
                }
                Err(e) => warn!("UDP listener on :{} recv_from error: {}", server_port, e),
            },
        }
    }
}

impl Sessions {
    async fn forward(&mut self, datagram: &[u8], client: SocketAddr) {
        let now = millis_since(self.epoch);
        let new_backend = {
            // A table guard must not be held across an await.
            let table = self.routes.load();
            if !std::ptr::eq(Arc::as_ptr(&table), self.seen.as_ptr()) {
                self.seen = Arc::downgrade(&table);
                self.generation += 1;
            }
            if self.reusable(&table, client) {
                None
            } else {
                // A session moving to another backend keeps its slot: given
                // back first, a listener sharing the budget could take it
                // before this one does, dropping a live client as overflow.
                let held = self.map.remove(&client).map(|old| old.slot);
                let Some(slot) = held.or_else(|| self.budget.take()) else {
                    self.overflow();
                    return;
                };
                match analyze_udp_request(&table, &self.selector, self.server_port, &self.health) {
                    Ok(RoutingResult::UdpForward { backend_addr }) => Some((backend_addr, slot)),
                    Ok(_) => return,
                    Err(e) => {
                        warn!("UDP routing error for port {}: {}", self.server_port, e);
                        return;
                    }
                }
            }
        };
        match new_backend {
            None => self.send(datagram, client, now).await,
            Some((backend_addr, slot)) => {
                self.open(datagram, client, backend_addr, slot, now).await
            }
        }
    }

    /// Whether `client`'s session can carry its next datagram: its reply path
    /// is alive, and its backend is still routed. Checked once per table
    /// generation, so a swap that keeps the backend keeps the client on it.
    fn reusable(&mut self, table: &RouteTable, client: SocketAddr) -> bool {
        let (generation, port) = (self.generation, self.server_port);
        let Some(session) = self.map.get_mut(&client) else {
            return false;
        };
        if session.reply_task.is_finished() {
            return false;
        }
        if session.generation != generation {
            let routed = table.find_udp_backends(port).is_ok_and(|backends| {
                backends
                    .iter()
                    .any(|b| b.weight > 0 && b.socket_addr == session.backend_addr)
            });
            if !routed {
                return false;
            }
            session.generation = generation;
        }
        true
    }

    async fn send(&mut self, datagram: &[u8], client: SocketAddr, now: u64) {
        let Some(session) = self.map.get(&client) else {
            return;
        };
        session.last_active.store(now, Relaxed);
        let sent = session.backend_socket.send(datagram).await;
        if let Err(e) = sent {
            debug!("UDP send to backend failed for {}: {}", client, e);
            self.map.remove(&client);
        }
    }

    async fn open(
        &mut self,
        datagram: &[u8],
        client: SocketAddr,
        backend_addr: SocketAddr,
        slot: SessionSlot,
        now: u64,
    ) {
        let backend_socket = match connect_backend(backend_addr).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                if out_of_own_resources(&e) {
                    log_local_exhaustion(backend_addr, &e);
                } else {
                    warn!("UDP backend socket to {} failed: {}", backend_addr, e);
                }
                return;
            }
        };
        if let Err(e) = backend_socket.send(datagram).await {
            if out_of_own_resources(&e) {
                log_local_exhaustion(backend_addr, &e);
            } else {
                warn!("UDP initial send to {} failed: {}", backend_addr, e);
            }
            return;
        }
        let last_active = Arc::new(AtomicU64::new(now));
        let reply_task = AbortOnDropHandle::new(tokio::spawn(relay_replies(
            Arc::clone(&self.listener),
            Arc::clone(&backend_socket),
            client,
            Arc::clone(&last_active),
            self.epoch,
        )));
        self.map.insert(
            client,
            UdpSession {
                backend_addr,
                backend_socket,
                reply_task,
                last_active,
                generation: self.generation,
                slot,
            },
        );
    }

    /// The one expiry mechanism: drops sessions idle past the timeout, and
    /// sessions whose reply path died.
    fn reap(&mut self) {
        let now = millis_since(self.epoch);
        let idle_ms = self.idle_timeout.as_millis() as u64;
        self.map.retain(|_, s| {
            !s.reply_task.is_finished()
                && now.saturating_sub(s.last_active.load(Relaxed)) <= idle_ms
        });
        if !self.budget.full() {
            self.full_logged = false;
        }
    }

    fn overflow(&mut self) {
        METRICS.udp_session_overflow_drops_total.inc();
        if !self.full_logged {
            self.full_logged = true;
            warn!(
                "UDP listener on :{} reached {} ({}): dropping datagrams that would open a session",
                self.server_port,
                self.budget.name(),
                self.budget.max
            );
        }
    }
}

/// Whether a session failed to open for want of the proxy's own fds, memory
/// or ports: the bind takes its port from the host's range, whatever the
/// backend, and fails EADDRINUSE once none is left. Every datagram from a
/// new source then fails alike, at packet rate under a spoofed flood.
fn out_of_own_resources(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(libc::EADDRINUSE) || out_of_fds_or_memory(e)
}

/// A socket connected to `backend`, bound in the backend's address family:
/// an IPv4 socket cannot reach an IPv6 backend.
async fn connect_backend(backend: SocketAddr) -> std::io::Result<UdpSocket> {
    let local: SocketAddr = match backend {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = UdpSocket::bind(local).await?;
    socket.connect(backend).await?;
    Ok(socket)
}

/// Backend-to-client half of a session. No timeout of its own: a backend
/// that stays silent must not end a session its client still uses, and the
/// reaper aborts this task with its session.
async fn relay_replies(
    listener: Arc<UdpSocket>,
    backend: Arc<UdpSocket>,
    client: SocketAddr,
    last_active: Arc<AtomicU64>,
    epoch: Instant,
) {
    let mut buf = vec![0u8; 65536];
    while let Ok(n) = backend.recv(&mut buf).await {
        last_active.store(millis_since(epoch), Relaxed);
        if listener.send_to(&buf[..n], client).await.is_err() {
            return;
        }
    }
}

fn millis_since(epoch: Instant) -> u64 {
    epoch.elapsed().as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::Backend;
    use tokio::task::JoinHandle;

    /// Replies "{tag}:{payload}" to every datagram.
    async fn echo_backend(bind: &str, tag: &'static str) -> Option<(SocketAddr, JoinHandle<()>)> {
        let socket = UdpSocket::bind(bind).await.ok()?;
        let addr = socket.local_addr().ok()?;
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            while let Ok((n, peer)) = socket.recv_from(&mut buf).await {
                let mut reply = format!("{}:", tag).into_bytes();
                reply.extend_from_slice(&buf[..n]);
                let _ = socket.send_to(&reply, peer).await;
            }
        });
        Some((addr, task))
    }

    fn udp_table(port: u16, backends: &[SocketAddr]) -> RouteTable {
        let mut t = RouteTable::new();
        let backends = backends
            .iter()
            .map(|b| Backend::new(b.ip().to_string(), b.port()).unwrap())
            .collect();
        t.add_udp_route(port, backends);
        t
    }

    struct Proxy {
        addr: SocketAddr,
        routes: Arc<ArcSwap<RouteTable>>,
        shutdown: CancellationToken,
    }

    impl Drop for Proxy {
        fn drop(&mut self) {
            self.shutdown.cancel();
        }
    }

    fn limits(idle_timeout: Duration, max_sessions: usize) -> SessionLimits {
        SessionLimits {
            idle_timeout,
            cap: SessionCap::PerListener(max_sessions),
        }
    }

    async fn start_proxy(backends: &[SocketAddr], limits: SessionLimits) -> Proxy {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr = socket.local_addr().unwrap();
        let routes = Arc::new(ArcSwap::from_pointee(udp_table(addr.port(), backends)));
        let shutdown = CancellationToken::new();
        tokio::spawn(run_udp_worker(
            socket,
            addr.port(),
            Arc::clone(&routes),
            limits,
            shutdown.clone(),
            Arc::new(HealthRegistry::new()),
        ));
        Proxy {
            addr,
            routes,
            shutdown,
        }
    }

    async fn client() -> UdpSocket {
        UdpSocket::bind("127.0.0.1:0").await.unwrap()
    }

    async fn roundtrip(client: &UdpSocket, proxy: SocketAddr, payload: &[u8]) -> Option<String> {
        roundtrip_within(client, proxy, payload, Duration::from_millis(500)).await
    }

    async fn roundtrip_within(
        client: &UdpSocket,
        proxy: SocketAddr,
        payload: &[u8],
        wait: Duration,
    ) -> Option<String> {
        client.send_to(payload, proxy).await.ok()?;
        let mut buf = vec![0u8; 2048];
        let (n, _) = tokio::time::timeout(wait, client.recv_from(&mut buf))
            .await
            .ok()?
            .ok()?;
        Some(String::from_utf8_lossy(&buf[..n]).into_owned())
    }

    #[test]
    fn default_cap_is_a_quarter_of_the_fd_budget() {
        let perf = PerformanceConfig::default();
        let cap = |nofile| SessionLimits::with_nofile(&perf, nofile).budget().max;
        assert_eq!(cap(Some(1_048_576)), 262_144);
        assert_eq!(cap(Some(1_024)), 256);
        assert_eq!(cap(Some(3)), 1, "never zero");
        assert_eq!(
            cap(None),
            256,
            "an unreadable limit counts as the usual 1024"
        );
    }

    #[test]
    fn explicit_cap_wins_over_the_fd_budget() {
        let perf = PerformanceConfig {
            udp_max_sessions: Some(7),
            ..Default::default()
        };
        let limits = SessionLimits::with_nofile(&perf, Some(1_048_576));
        assert_eq!(limits.budget().max, 7);
        assert!(
            !Arc::ptr_eq(&limits.budget(), &limits.budget()),
            "udpMaxSessions is each listener's own"
        );
    }

    #[tokio::test]
    async fn ipv6_backend_is_reachable() {
        let Some((backend, _echo)) = echo_backend("[::1]:0", "v6").await else {
            return; // no IPv6 loopback here
        };
        let proxy = start_proxy(&[backend], limits(Duration::from_secs(30), 16)).await;
        let client = client().await;
        assert_eq!(
            roundtrip(&client, proxy.addr, b"hi").await.as_deref(),
            Some("v6:hi")
        );
    }

    /// A session must leave a backend the route table dropped, and only then:
    /// a swap that keeps its backend keeps the session.
    #[tokio::test]
    async fn session_follows_route_table_swap() {
        let (a, _ea) = echo_backend("127.0.0.1:0", "a").await.unwrap();
        let (b, _eb) = echo_backend("127.0.0.1:0", "b").await.unwrap();
        let proxy = start_proxy(&[a], limits(Duration::from_secs(30), 16)).await;
        let port = proxy.addr.port();
        let client = client().await;
        assert_eq!(
            roundtrip(&client, proxy.addr, b"1").await.as_deref(),
            Some("a:1")
        );

        // A fresh session would round-robin onto b here.
        proxy.routes.store(Arc::new(udp_table(port, &[a, b])));
        assert_eq!(
            roundtrip(&client, proxy.addr, b"2").await.as_deref(),
            Some("a:2")
        );

        proxy.routes.store(Arc::new(udp_table(port, &[b])));
        assert_eq!(
            roundtrip(&client, proxy.addr, b"3").await.as_deref(),
            Some("b:3")
        );
    }

    #[tokio::test]
    async fn new_clients_dropped_at_session_cap() {
        let (a, _echo) = echo_backend("127.0.0.1:0", "a").await.unwrap();
        let proxy = start_proxy(&[a], limits(Duration::from_millis(400), 1)).await;
        let (first, second) = (client().await, client().await);
        assert_eq!(
            roundtrip(&first, proxy.addr, b"1").await.as_deref(),
            Some("a:1")
        );

        // Waits well under the idle timeout, so the first session is still up.
        let drops = METRICS.udp_session_overflow_drops_total.get();
        let dropped = roundtrip_within(&second, proxy.addr, b"2", Duration::from_millis(150));
        assert_eq!(dropped.await, None);
        assert!(METRICS.udp_session_overflow_drops_total.get() > drops);
        assert_eq!(
            roundtrip(&first, proxy.addr, b"3").await.as_deref(),
            Some("a:3"),
            "the established session keeps working"
        );

        // Idle past the timeout plus a reaper period: the slot frees up.
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        assert_eq!(
            roundtrip(&second, proxy.addr, b"4").await.as_deref(),
            Some("a:4")
        );
    }

    /// Unset, the cap is one budget for every UDP listener socket: a quarter
    /// of the fd limit each let four sockets take every fd TCP needs.
    #[tokio::test]
    async fn default_cap_is_shared_by_all_listeners() {
        let (a, _echo) = echo_backend("127.0.0.1:0", "a").await.unwrap();
        let perf = PerformanceConfig {
            udp_session_timeout: Duration::from_millis(400),
            ..Default::default()
        };
        // Two sessions in all.
        let limits = SessionLimits::with_nofile(&perf, Some(8));
        let one = start_proxy(&[a], limits.clone()).await;
        let two = start_proxy(&[a], limits.clone()).await;
        let clients = [client().await, client().await, client().await];
        assert_eq!(
            roundtrip(&clients[0], one.addr, b"1").await.as_deref(),
            Some("a:1")
        );
        assert_eq!(
            roundtrip(&clients[1], two.addr, b"2").await.as_deref(),
            Some("a:2")
        );

        let drops = METRICS.udp_session_overflow_drops_total.get();
        let dropped = roundtrip_within(&clients[2], two.addr, b"3", Duration::from_millis(150));
        assert_eq!(dropped.await, None, "a third session opened");
        assert!(METRICS.udp_session_overflow_drops_total.get() > drops);

        // Idle past the timeout plus a reaper period: both slots free up.
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        assert_eq!(
            roundtrip(&clients[2], two.addr, b"4").await.as_deref(),
            Some("a:4")
        );
        assert_eq!(
            roundtrip(&clients[0], two.addr, b"5").await.as_deref(),
            Some("a:5")
        );
    }

    /// A session moving to another backend keeps its slot. Given back and
    /// taken again, it could go to a listener sharing the full budget in
    /// between, and the live client was dropped as overflow.
    #[tokio::test]
    async fn a_rerouted_session_keeps_its_slot_in_a_full_shared_budget() {
        let (a, _ea) = echo_backend("127.0.0.1:0", "a").await.unwrap();
        let (b, _eb) = echo_backend("127.0.0.1:0", "b").await.unwrap();
        // One session in all.
        let limits = SessionLimits::with_nofile(&PerformanceConfig::default(), Some(4));
        let budget = limits.budget();
        let proxy = start_proxy(&[a], limits).await;
        let client = client().await;
        assert_eq!(
            roundtrip(&client, proxy.addr, b"1").await.as_deref(),
            Some("a:1")
        );

        // The rival's take, landed between the give-back and the take: no
        // test can time it there, so it is counted up front.
        budget.open.fetch_add(1, Relaxed);
        let _rival = SessionSlot(Arc::clone(&budget));
        proxy
            .routes
            .store(Arc::new(udp_table(proxy.addr.port(), &[b])));
        assert_eq!(
            roundtrip(&client, proxy.addr, b"2").await.as_deref(),
            Some("b:2")
        );
    }

    /// A session's port comes from the host's range whatever the backend:
    /// none left is the proxy's own shortage, like fds.
    #[test]
    fn running_out_of_ports_is_local() {
        let local = |errno| out_of_own_resources(&std::io::Error::from_raw_os_error(errno));
        for errno in [libc::EADDRINUSE, libc::EMFILE, libc::ENFILE, libc::ENOBUFS] {
            assert!(local(errno), "{errno}");
        }
        for errno in [libc::ENETUNREACH, libc::ECONNREFUSED, libc::EPERM] {
            assert!(!local(errno), "{errno}");
        }
    }

    /// A log sink a test reads back.
    #[derive(Clone, Default)]
    struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Out of fds, every datagram from a new source fails to open its
    /// session, and a spoofed flood sends those at packet rate: one line
    /// per window, not one per datagram.
    #[test]
    fn running_out_of_fds_logs_once_per_window() {
        use crate::proxy::test_util::{in_own_process, FdsExhausted};
        if !in_own_process(module_path!(), "running_out_of_fds_logs_once_per_window") {
            return;
        }
        let log = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer({
                let log = log.clone();
                move || log.clone()
            })
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();
        let _subscriber = tracing::subscriber::set_default(subscriber);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let backend = runtime.block_on(async {
            let (a, _echo) = echo_backend("127.0.0.1:0", "a").await.unwrap();
            let proxy = start_proxy(&[a], limits(Duration::from_secs(30), 16)).await;
            let clients = [client().await, client().await, client().await];

            let exhausted = FdsExhausted::new();
            for client in &clients {
                let wait = Duration::from_millis(100);
                assert_eq!(roundtrip_within(client, proxy.addr, b"x", wait).await, None);
            }
            drop(exhausted);
            assert_eq!(
                roundtrip(&clients[0], proxy.addr, b"1").await.as_deref(),
                Some("a:1")
            );
            a
        });
        let log = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        assert_eq!(log.matches(&backend.to_string()).count(), 1, "{log}");
    }

    #[tokio::test]
    async fn dropping_a_session_aborts_its_reply_task() {
        let marker = Arc::new(());
        let held = Arc::clone(&marker);
        let task = tokio::spawn(async move {
            let _held = held;
            std::future::pending::<()>().await
        });
        let session = UdpSession {
            backend_addr: "127.0.0.1:9".parse().unwrap(),
            backend_socket: Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap()),
            reply_task: AbortOnDropHandle::new(task),
            last_active: Arc::new(AtomicU64::new(0)),
            generation: 0,
            slot: Arc::new(SessionBudget::new(1, false)).take().unwrap(),
        };
        drop(session);
        for _ in 0..10 {
            if Arc::strong_count(&marker) == 1 {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("reply task outlived its session");
    }
}
