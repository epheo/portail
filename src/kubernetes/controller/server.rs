//! `POST /v1/sync/{namespace}/{name}` over TLS: authenticate, record the
//! report, answer the current snapshot or hold the poll until a newer one.
//!
//! Order per request: leadership and caches, then the token against the
//! Gateway in the path, on the head alone, then TokenReview, and only then
//! the body: an unauthenticated peer never makes the controller read or
//! hold more than a capped request head.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures::channel::mpsc;
use http::{header, HeaderMap, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::Body;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use kube::runtime::reflector::ObjectRef;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Duration, Instant};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

use gateway_api::gateways::Gateway;

use super::auth::{Authenticator, Refusal};
use super::reconcile::{class_owner, ClassOwner};
use super::reports::{InstanceId, LIVENESS};
use super::ControllerCtx;
use crate::logging::{info, warn};
use crate::proxy::worker::AcceptBackoff;
use crate::sync::{GatewayKey, Snapshot, SyncRequest, HOLD, PROTOCOL};

/// Followed by `{namespace}/{name}` of the Gateway.
const SYNC_PREFIX: &str = "/v1/sync/";
/// A report grows with listeners and endpoints; konstruct's 13-listener
/// Gateway is two orders below.
const MAX_BODY: usize = 1024 * 1024;
/// Bodies read at once, as many as TokenReviews in flight: 16 MiB at most,
/// and a report reads in milliseconds. One pod reads one at a time: a data
/// plane sends one request at a time, and a tenant stalling bodies on its
/// own valid token then holds one read, not all.
const MAX_BODY_READS: usize = 16;
/// The longest request head, and hyper's read buffer: a token is at most
/// 16 KiB (the pre-check refuses more), the rest of a head a few hundred
/// bytes.
const MAX_HEAD: usize = 32 * 1024;
/// Open connections. A data plane holds one, two while a report change
/// replaces its poll: room for 512 data-plane pods per controller, each
/// connection at most its read buffer and a TLS record or two, about
/// 64 MiB in all.
const MAX_CONNECTIONS: usize = 1024;
/// Open connections per peer IP. A data plane holds one or two and pods
/// have their own IPs, but data planes behind the apiserver proxy all
/// arrive from the apiserver's: room for 16 of those, and no single pod
/// holds the whole cap.
const MAX_CONNECTIONS_PER_PEER: usize = 32;
/// The TLS handshake, the request head and the body each, and a response
/// write that makes no progress: in-cluster they take milliseconds; 5s
/// absorbs a loaded node and frees a stalled peer's connection in a sixth
/// of the hold.
const PEER_TIMEOUT: Duration = Duration::from_secs(5);
/// Every 503's: the data plane's first backoff step.
const RETRY_AFTER_SECS: u64 = 1;
/// The token's other header: the apiserver service proxy drops
/// Authorization on its hop.
const TOKEN_HEADER: &str = "x-portail-token";

/// The sync endpoint's state.
pub(crate) struct SyncServer {
    ctx: Arc<ControllerCtx>,
    auth: Authenticator,
    holds: Holds,
    lapses: Arc<Lapses>,
    connections: Arc<Semaphore>,
    /// Open connections by peer IP, and the most one may hold.
    peer_connections: Mutex<HashMap<IpAddr, usize>>,
    per_peer: usize,
    body_reads: Semaphore,
    /// The pods whose body is being read.
    body_readers: Mutex<std::collections::HashSet<InstanceId>>,
    shutdown: CancellationToken,
}

impl SyncServer {
    pub(crate) fn new(
        ctx: Arc<ControllerCtx>,
        auth: Authenticator,
        lapses: Arc<Lapses>,
        shutdown: CancellationToken,
    ) -> Arc<Self> {
        Self::with_connections(
            ctx,
            auth,
            lapses,
            shutdown,
            MAX_CONNECTIONS,
            MAX_CONNECTIONS_PER_PEER,
        )
    }

    fn with_connections(
        ctx: Arc<ControllerCtx>,
        auth: Authenticator,
        lapses: Arc<Lapses>,
        shutdown: CancellationToken,
        connections: usize,
        per_peer: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            ctx,
            auth,
            holds: Holds::default(),
            lapses,
            connections: Arc::new(Semaphore::new(connections)),
            peer_connections: Mutex::default(),
            per_peer,
            body_reads: Semaphore::new(MAX_BODY_READS),
            body_readers: Mutex::default(),
            shutdown,
        })
    }

    /// One request from `peer`, answered.
    pub(crate) async fn handle<B>(&self, request: Request<B>, peer: IpAddr) -> Response<Full<Bytes>>
    where
        B: Body<Data = Bytes>,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let Some(key) = request
            .uri()
            .path()
            .strip_prefix(SYNC_PREFIX)
            .and_then(|gateway| gateway.parse::<GatewayKey>().ok())
        else {
            return text(StatusCode::NOT_FOUND, "not found");
        };
        if request.method() != Method::POST {
            return text(StatusCode::METHOD_NOT_ALLOWED, "POST only");
        }
        // Only the leader writes status, so only it takes reports.
        if !self.ctx.leader.is_leader() {
            return unavailable("not the leader");
        }
        // A 404 is only authoritative once the store is complete.
        if !self.ctx.synced.is_synced() {
            return unavailable("caches not synced");
        }
        let token = token(request.headers()).unwrap_or_default().to_string();
        let claimed = match self.auth.precheck(&token) {
            Ok(claimed) => claimed,
            Err(refusal) => return refused(refusal),
        };
        if !self.serves(&key) {
            // No review for a Gateway we do not serve: its account cannot
            // fill the verdict cache. Only a token already reviewed as its
            // data plane learns that it is gone; anyone else gets the
            // pre-check's 401, no oracle of Gateway names.
            return if self.auth.reviewed_for(&token, &key) {
                not_served()
            } else {
                refused(self.auth.refuse_unreviewed())
            };
        }
        let instance = match self.auth.authenticate(&claimed, &token, &key, peer).await {
            Ok(instance) => instance,
            Err(refusal) => return refused(refusal),
        };
        let declared = request
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        if declared.is_some_and(|len| len > MAX_BODY as u64) {
            return text(StatusCode::PAYLOAD_TOO_LARGE, "body over 1 MiB");
        }
        // Both slots are held for the read only, never through the hold.
        let bytes = {
            let Some(_reader) = BodyReader::start(&self.body_readers, &instance) else {
                return unavailable("a body of this pod is being read");
            };
            let Ok(_reading) = self.body_reads.try_acquire() else {
                return unavailable("too many bodies being read");
            };
            let body = Limited::new(request.into_body(), MAX_BODY);
            match tokio::time::timeout(PEER_TIMEOUT, body.collect()).await {
                Ok(Ok(collected)) => collected.to_bytes(),
                Ok(Err(e)) if e.downcast_ref::<LengthLimitError>().is_some() => {
                    return text(StatusCode::PAYLOAD_TOO_LARGE, "body over 1 MiB")
                }
                Ok(Err(_)) | Err(_) => return text(StatusCode::BAD_REQUEST, "body not read"),
            }
        };
        let sync: SyncRequest = match serde_json::from_slice(&bytes) {
            Ok(sync) => sync,
            Err(e) => return text(StatusCode::BAD_REQUEST, &format!("not a sync request: {e}")),
        };
        if sync.gateway != key {
            return text(
                StatusCode::BAD_REQUEST,
                "the body names another Gateway than the path",
            );
        }
        if sync.protocol != PROTOCOL {
            return text(
                StatusCode::CONFLICT,
                &format!(
                    "protocol {} not supported: this controller speaks {PROTOCOL}",
                    sync.protocol
                ),
            );
        }
        if !self.serves(&key) {
            return not_served();
        }
        if let Some(report) = sync.report {
            let leaving = report.leaving;
            self.ctx.reports.record(&key, instance.clone(), report);
            // After the record: a wake earlier than the report's own
            // last-seen plus LIVENESS would find it still live.
            self.lapses.seen(&key, &instance);
            // A stopping data plane waits on this answer before it drains:
            // there is nothing left to hold it for.
            if leaving {
                return no_content();
            }
        }
        self.poll(&key, &sync.have, &instance).await
    }

    /// Whether `key` is a Gateway of ours. A missing class keeps it served:
    /// a deleted class never takes its Gateways down.
    fn serves(&self, key: &GatewayKey) -> bool {
        let Some(gateway) = self
            .ctx
            .cache
            .gateways
            .get(&ObjectRef::<Gateway>::new(&key.name).within(&key.namespace))
        else {
            return false;
        };
        class_owner(
            &self.ctx.cache.gateway_classes.state(),
            &gateway.spec.gateway_class_name,
            &self.ctx.controller_name,
        ) != ClassOwner::Other
    }

    /// The snapshot when its version is not `have`, at once or when one is
    /// published within the hold; else 204.
    async fn poll(
        &self,
        key: &GatewayKey,
        have: &str,
        instance: &InstanceId,
    ) -> Response<Full<Bytes>> {
        let hold = self.holds.begin(instance);
        // Subscribing creates the slot: checked before, and again after in
        // case the Gateway went in between, as a publish re-checks.
        let mut current = self.ctx.hub.subscribe(key);
        if !self.serves(key) {
            self.ctx.hub.remove(key);
            return not_served();
        }
        let deadline = Instant::now() + HOLD;
        loop {
            let snapshot = current.borrow_and_update().clone();
            if let Some(snapshot) = snapshot.filter(|s| s.version != have) {
                return served(&snapshot);
            }
            tokio::select! {
                changed = current.changed() => if changed.is_err() {
                    return not_served();
                },
                _ = tokio::time::sleep_until(deadline) => return no_content(),
                // The instance asked again: this answer has no reader.
                _ = hold.superseded.cancelled() => return no_content(),
                _ = self.shutdown.cancelled() => return unavailable("shutting down"),
            }
        }
    }
}

/// The bearer token, from Authorization or X-Portail-Token.
fn token(headers: &HeaderMap) -> Option<&str> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token);
    bearer
        .or_else(|| headers.get(TOKEN_HEADER).and_then(|v| v.to_str().ok()))
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

/// Every answer but a snapshot or a 204, each closing its connection: only
/// an answered poll keeps one, so a peer that never authenticates cannot
/// hold a slot with one junk request per head timeout.
fn text(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(format!("{body}\n"))));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        "text/plain; charset=utf-8".parse().unwrap(),
    );
    headers.insert(header::CONNECTION, "close".parse().unwrap());
    response
}

fn unavailable(body: &str) -> Response<Full<Bytes>> {
    let mut response = text(StatusCode::SERVICE_UNAVAILABLE, body);
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, RETRY_AFTER_SECS.into());
    response
}

fn refused(refusal: Refusal) -> Response<Full<Bytes>> {
    match refusal {
        // One body for every 401: no oracle of which check failed.
        Refusal::Unauthenticated => text(StatusCode::UNAUTHORIZED, "unauthenticated"),
        Refusal::Forbidden => text(StatusCode::FORBIDDEN, "forbidden"),
        Refusal::Throttled => unavailable("too many token reviews in flight"),
        Refusal::Unavailable => unavailable("token review failed"),
    }
}

fn not_served() -> Response<Full<Bytes>> {
    text(StatusCode::NOT_FOUND, "gateway not served")
}

fn no_content() -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::default());
    *response.status_mut() = StatusCode::NO_CONTENT;
    response
}

fn served(snapshot: &Snapshot) -> Response<Full<Bytes>> {
    match serde_json::to_vec(snapshot) {
        Ok(json) => {
            let mut response = Response::new(Full::new(Bytes::from(json)));
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
            response
        }
        Err(e) => text(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("snapshot not serialized: {e}"),
        ),
    }
}

/// One held poll per instance: a data plane asks again when its report
/// changes, and the poll it gave up on would otherwise hold to expiry.
#[derive(Default)]
struct Holds {
    by_instance: Mutex<HashMap<InstanceId, (u64, CancellationToken)>>,
    next: AtomicU64,
}

struct Hold<'a> {
    holds: &'a Holds,
    instance: InstanceId,
    id: u64,
    superseded: CancellationToken,
}

impl Holds {
    fn begin(&self, instance: &InstanceId) -> Hold<'_> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let superseded = CancellationToken::new();
        let previous = self
            .by_instance
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(instance.clone(), (id, superseded.clone()));
        if let Some((_, previous)) = previous {
            previous.cancel();
        }
        Hold {
            holds: self,
            instance: instance.clone(),
            id,
            superseded,
        }
    }
}

impl Drop for Hold<'_> {
    fn drop(&mut self) {
        let mut holds = self
            .holds
            .by_instance
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if holds
            .get(&self.instance)
            .is_some_and(|(id, _)| *id == self.id)
        {
            holds.remove(&self.instance);
        }
    }
}

/// Wakes a Gateway's reconcile when one of its instances has been silent
/// for the report liveness window: a vanished instance sends no report
/// that would.
pub(crate) struct Lapses {
    deadlines: Mutex<HashMap<(GatewayKey, InstanceId), Instant>>,
    wake: mpsc::UnboundedSender<GatewayKey>,
}

impl Lapses {
    pub(crate) fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<GatewayKey>) {
        let (wake, wakes) = mpsc::unbounded();
        let lapses = Arc::new(Self {
            deadlines: Mutex::default(),
            wake,
        });
        (lapses, wakes)
    }

    fn deadlines(&self) -> std::sync::MutexGuard<'_, HashMap<(GatewayKey, InstanceId), Instant>> {
        self.deadlines.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// `instance` reported for `gateway` now.
    fn seen(self: &Arc<Self>, gateway: &GatewayKey, instance: &InstanceId) {
        let key = (gateway.clone(), instance.clone());
        let first = self
            .deadlines()
            .insert(key.clone(), Instant::now() + LIVENESS)
            .is_none();
        if first {
            tokio::spawn(self.clone().watch(key));
        }
    }

    /// One task per live instance, until it lapses.
    async fn watch(self: Arc<Self>, key: (GatewayKey, InstanceId)) {
        loop {
            let deadline = {
                let mut deadlines = self.deadlines();
                match deadlines.get(&key) {
                    Some(&deadline) if deadline > Instant::now() => deadline,
                    Some(_) => {
                        deadlines.remove(&key);
                        let _ = self.wake.unbounded_send(key.0);
                        return;
                    }
                    None => return,
                }
            };
            tokio::time::sleep_until(deadline).await;
        }
    }
}

/// A pod's body read, until dropped.
struct BodyReader<'a> {
    readers: &'a Mutex<std::collections::HashSet<InstanceId>>,
    instance: InstanceId,
}

impl<'a> BodyReader<'a> {
    /// `None` while `instance` has a body being read.
    fn start(
        readers: &'a Mutex<std::collections::HashSet<InstanceId>>,
        instance: &InstanceId,
    ) -> Option<Self> {
        let started = readers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(instance.clone());
        started.then(|| Self {
            readers,
            instance: instance.clone(),
        })
    }
}

impl Drop for BodyReader<'_> {
    fn drop(&mut self) {
        self.readers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.instance);
    }
}

/// A connection counted against its peer IP until dropped.
struct PeerConnection {
    server: Arc<SyncServer>,
    peer: IpAddr,
}

impl PeerConnection {
    /// `None` when `peer` holds its share already.
    fn open(server: &Arc<SyncServer>, peer: IpAddr) -> Option<Self> {
        let mut peers = server
            .peer_connections
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let open = peers.entry(peer).or_insert(0);
        if *open >= server.per_peer {
            return None;
        }
        *open += 1;
        Some(Self {
            server: server.clone(),
            peer,
        })
    }
}

impl Drop for PeerConnection {
    fn drop(&mut self) {
        let mut peers = self
            .server
            .peer_connections
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(open) = peers.get_mut(&self.peer) {
            *open -= 1;
            if *open == 0 {
                peers.remove(&self.peer);
            }
        }
    }
}

/// Fails a write that makes no progress for [`PEER_TIMEOUT`]: hyper waits
/// on a flush to a peer that stopped reading, a pipelining one included,
/// with no timer of its own.
struct StallGuard<T> {
    io: T,
    stalled: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<T> StallGuard<T> {
    fn new(io: T) -> Self {
        Self { io, stalled: None }
    }

    fn progress<R>(
        &mut self,
        cx: &mut Context<'_>,
        poll: Poll<std::io::Result<R>>,
    ) -> Poll<std::io::Result<R>> {
        if poll.is_ready() {
            self.stalled = None;
            return poll;
        }
        let stalled = self
            .stalled
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(PEER_TIMEOUT)));
        match stalled.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the peer stopped reading",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for StallGuard<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for StallGuard<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.io).poll_write(cx, buf);
        this.progress(cx, poll)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.io).poll_write_vectored(cx, bufs);
        this.progress(cx, poll)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.io).poll_flush(cx);
        this.progress(cx, poll)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.io).poll_shutdown(cx);
        this.progress(cx, poll)
    }
}

/// Serve sync requests on `listener` until the server's shutdown; held
/// polls then answer 503 and connections close.
pub(crate) async fn serve(
    server: Arc<SyncServer>,
    tls: Arc<rustls::ServerConfig>,
    listener: TcpListener,
) -> anyhow::Result<()> {
    let acceptor = TlsAcceptor::from(tls);
    info!(
        "Sync endpoint listening on {} ({}{{namespace}}/{{name}})",
        listener.local_addr()?,
        SYNC_PREFIX
    );
    let mut backoff = AcceptBackoff::default();
    loop {
        // At the cap, connections wait in the listen backlog.
        let slot = tokio::select! {
            _ = server.shutdown.cancelled() => return Ok(()),
            slot = server.connections.clone().acquire_owned() => {
                slot.expect("the connection semaphore is never closed")
            }
        };
        let accepted = tokio::select! {
            _ = server.shutdown.cancelled() => return Ok(()),
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((stream, peer)) => {
                backoff.reset();
                // Over its share the connection closes as it is dropped.
                let Some(counted) = PeerConnection::open(&server, peer.ip()) else {
                    continue;
                };
                tokio::spawn(connection(
                    server.clone(),
                    acceptor.clone(),
                    stream,
                    peer.ip(),
                    (slot, counted),
                ));
            }
            Err(e) => {
                if let Some(pause) = backoff.after(&e) {
                    warn!("Sync endpoint accept error: {}; retrying in {:?}", e, pause);
                    let _ = tokio::time::timeout(pause, server.shutdown.cancelled()).await;
                }
            }
        }
    }
}

async fn connection(
    server: Arc<SyncServer>,
    acceptor: TlsAcceptor,
    stream: TcpStream,
    peer: IpAddr,
    _slots: (OwnedSemaphorePermit, PeerConnection),
) {
    // A peer that cannot complete a handshake is no data plane; the data
    // plane logs its own side.
    let Ok(Ok(tls)) = tokio::time::timeout(PEER_TIMEOUT, acceptor.accept(stream)).await else {
        return;
    };
    let service = hyper::service::service_fn({
        let server = server.clone();
        move |request| {
            let server = server.clone();
            async move { Ok::<_, Infallible>(server.handle(request, peer).await) }
        }
    });
    let conn = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(PEER_TIMEOUT)
        .max_buf_size(MAX_HEAD)
        .max_header_size(MAX_HEAD)
        .serve_connection(TokioIo::new(StallGuard::new(tls)), service);
    let mut conn = std::pin::pin!(conn);
    tokio::select! {
        // A reset or a timed-out head: the peer polls again.
        _ = conn.as_mut() => {}
        _ = server.shutdown.cancelled() => {
            conn.as_mut().graceful_shutdown();
            let _ = conn.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::auth::testing::{
        authenticator, gateway_token, peer, review, token_of, PEER, REVIEWS,
    };
    use super::super::auth::METRICS_SERIAL;
    use super::super::testkit::{self, apply, gateway_class, MockApi, TestCluster, CONTROLLER};
    use super::*;
    use crate::config::PortailConfig;
    use crate::metrics::{self, SyncAuthStage};
    use crate::sync::GatewayRef;
    use serde_json::{json, Value};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::task::{Context, Poll};

    const SA: &str = "system:serviceaccount:default:portail-gw";

    struct Fixture {
        api: MockApi,
        cluster: TestCluster,
        ctx: Arc<ControllerCtx>,
        server: Arc<SyncServer>,
        wakes: mpsc::UnboundedReceiver<GatewayKey>,
        shutdown: CancellationToken,
    }

    /// A leading, synced controller serving Gateway `default/gw`, whose
    /// tokens TokenReview accepts as pod `pod-1`.
    fn fixture() -> Fixture {
        fixture_with(|_| {})
    }

    fn fixture_with(edit: impl FnOnce(&mut ControllerCtx)) -> Fixture {
        let api = MockApi::default();
        api.reply_to(REVIEWS, 201, review(true, SA, Some("pod-1")));
        let mut cluster = TestCluster::default();
        apply(
            &mut cluster.gateway_classes,
            gateway_class("portail", CONTROLLER),
        );
        apply(&mut cluster.gateways, testkit::gateway("gw", 1));
        let mut ctx = cluster.ctx(api.client());
        edit(Arc::get_mut(&mut ctx).unwrap());
        let (lapses, wakes) = Lapses::new();
        let shutdown = CancellationToken::new();
        let server = SyncServer::new(
            ctx.clone(),
            authenticator(api.client()),
            lapses,
            shutdown.clone(),
        );
        Fixture {
            api,
            cluster,
            ctx,
            server,
            wakes,
            shutdown,
        }
    }

    impl Fixture {
        fn reviews(&self) -> usize {
            self.api
                .requests()
                .iter()
                .filter(|r| r.path.starts_with(REVIEWS))
                .count()
        }

        /// Publish a snapshot of `gw` at `generation`; its version.
        fn publish(&self, generation: i64) -> String {
            let snapshot = Snapshot::from_config(
                GatewayRef {
                    namespace: "default".into(),
                    name: "gw".into(),
                    uid: "gw-uid".into(),
                    generation,
                },
                PortailConfig::default(),
                vec![],
            );
            let version = snapshot.version.clone();
            assert!(self.ctx.hub.publish(snapshot));
            version
        }

        async fn sync(&self, request: Request<Full<Bytes>>) -> Response<Full<Bytes>> {
            self.server.handle(request, PEER).await
        }
    }

    fn key() -> GatewayKey {
        GatewayKey::new("default", "gw")
    }

    fn body(gateway: &GatewayKey, have: &str, report: Option<Value>) -> Value {
        let mut body = json!({
            "protocol": PROTOCOL,
            "gateway": gateway,
            "have": have,
            "dataPlaneVersion": "test",
        });
        if let Some(report) = report {
            body["report"] = report;
        }
        body
    }

    fn path_of(gateway: &GatewayKey) -> String {
        format!("{SYNC_PREFIX}{}/{}", gateway.namespace, gateway.name)
    }

    /// A sync request to the path of the Gateway its body names.
    fn post(token: Option<&str>, body: &Value) -> Request<Full<Bytes>> {
        let gateway: GatewayKey = serde_json::from_value(body["gateway"].clone()).unwrap();
        let mut builder = Request::post(path_of(&gateway));
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        builder
            .body(Full::new(Bytes::from(serde_json::to_vec(body).unwrap())))
            .unwrap()
    }

    fn poll(have: &str) -> Request<Full<Bytes>> {
        post(
            Some(&gateway_token("default", "gw", 0)),
            &body(&key(), have, None),
        )
    }

    async fn text_of(response: Response<Full<Bytes>>) -> (StatusCode, String) {
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn version_of(response: Response<Full<Bytes>>) -> String {
        let (status, body) = text_of(response).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let snapshot: Snapshot = serde_json::from_str(&body).unwrap();
        snapshot.version
    }

    #[tokio::test(start_paused = true)]
    async fn a_poll_holds_answers_204_at_expiry_and_200_on_a_change() {
        let f = Arc::new(fixture());

        // Nothing published yet: held, then 204.
        let started = Instant::now();
        let response = f.sync(poll("")).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(started.elapsed(), HOLD);

        // A snapshot the data plane lacks: at once.
        let v1 = f.publish(1);
        let started = Instant::now();
        assert_eq!(version_of(f.sync(poll("")).await).await, v1);
        assert_eq!(started.elapsed(), Duration::ZERO);

        // Current: held until a newer one is published.
        let held = tokio::spawn({
            let f = f.clone();
            let v1 = v1.clone();
            async move { f.sync(poll(&v1)).await }
        });
        tokio::time::sleep(HOLD / 2).await;
        assert!(!held.is_finished());
        let v2 = f.publish(2);
        assert_eq!(version_of(held.await.unwrap()).await, v2);

        let started = Instant::now();
        assert_eq!(f.sync(poll(&v2)).await.status(), StatusCode::NO_CONTENT);
        assert_eq!(started.elapsed(), HOLD);
    }

    #[tokio::test(start_paused = true)]
    async fn a_newer_request_of_the_instance_ends_its_held_poll() {
        let f = Arc::new(fixture());
        let v1 = f.publish(1);
        let first = tokio::spawn({
            let (f, v1) = (f.clone(), v1.clone());
            async move { f.sync(poll(&v1)).await }
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!first.is_finished());

        let second = tokio::spawn({
            let (f, v1) = (f.clone(), v1.clone());
            async move { f.sync(poll(&v1)).await }
        });
        let first = first.await.unwrap();
        assert_eq!(first.status(), StatusCode::NO_CONTENT);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!second.is_finished(), "the newer one holds");
        let v2 = f.publish(2);
        assert_eq!(version_of(second.await.unwrap()).await, v2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_removed_gateway_ends_its_polls_with_404() {
        let f = Arc::new(fixture());
        let v1 = f.publish(1);
        let held = tokio::spawn({
            let f = f.clone();
            async move { f.sync(poll(&v1)).await }
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(f.ctx.hub.remove(&key()));
        assert_eq!(held.await.unwrap().status(), StatusCode::NOT_FOUND);
    }

    /// The store says first: a poll for a Gateway that is not there, or
    /// whose class names another controller, gets 404 and leaves no slot.
    #[tokio::test(start_paused = true)]
    async fn a_gateway_not_ours_gets_no_slot() {
        let _serial = METRICS_SERIAL.lock().await;
        let mut f = fixture();
        let ghost = GatewayKey::new("default", "ghost");
        let response = f
            .sync(post(
                Some(&gateway_token("default", "ghost", 0)),
                &body(&ghost, "", Some(json!({"applied": "x"}))),
            ))
            .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(!f.ctx.hub.remove(&ghost), "no slot was created");
        assert!(f.ctx.reports.live(&ghost).is_empty(), "no report recorded");

        assert_eq!(f.sync(poll("")).await.status(), StatusCode::NO_CONTENT);
        assert!(f.ctx.hub.remove(&key()));
        let mut other = testkit::gateway("gw", 1);
        other.spec.gateway_class_name = "theirs".into();
        apply(
            &mut f.cluster.gateway_classes,
            gateway_class("theirs", "example.com/other"),
        );
        apply(&mut f.cluster.gateways, other);
        assert_eq!(f.sync(poll("")).await.status(), StatusCode::NOT_FOUND);
        assert!(!f.ctx.hub.remove(&key()));
    }

    /// A Gateway the store lacks, or of another class, costs no review:
    /// its account cannot fill the verdict cache, and a stranger gets the
    /// pre-check's 401, no oracle of Gateway names. A data plane already
    /// reviewed for it learns it is gone.
    #[tokio::test]
    async fn a_gateway_not_served_costs_no_review() {
        let _serial = METRICS_SERIAL.lock().await;
        let mut f = fixture();
        let ghost = GatewayKey::new("default", "ghost");
        let (status, text) = text_of(
            f.sync(post(
                Some(&gateway_token("default", "ghost", 0)),
                &body(&ghost, "", None),
            ))
            .await,
        )
        .await;
        assert_eq!(
            (status, text.as_str()),
            (StatusCode::UNAUTHORIZED, "unauthenticated\n")
        );
        assert_eq!(f.reviews(), 0);

        let v1 = f.publish(1);
        assert_eq!(version_of(f.sync(poll("")).await).await, v1);
        assert_eq!(f.reviews(), 1);
        let mut theirs = testkit::gateway("gw", 1);
        theirs.spec.gateway_class_name = "theirs".into();
        apply(
            &mut f.cluster.gateway_classes,
            gateway_class("theirs", "example.com/other"),
        );
        apply(&mut f.cluster.gateways, theirs);
        assert_eq!(f.sync(poll("")).await.status(), StatusCode::NOT_FOUND);
        let stranger = post(
            Some(&gateway_token("default", "gw", 1)),
            &body(&key(), "", None),
        );
        assert_eq!(f.sync(stranger).await.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(f.reviews(), 1, "neither cost a review");
    }

    #[tokio::test]
    async fn a_missing_class_keeps_the_gateway_served() {
        let mut f = fixture();
        let v1 = f.publish(1);
        let mut orphan = testkit::gateway("gw", 1);
        orphan.spec.gateway_class_name = "deleted".into();
        apply(&mut f.cluster.gateways, orphan);
        assert_eq!(version_of(f.sync(poll("")).await).await, v1);
    }

    #[tokio::test]
    async fn a_standby_or_unsynced_controller_answers_503_before_any_review() {
        for f in [
            fixture_with(|ctx| ctx.leader = super::super::leader::Leader::standby()),
            fixture_with(|ctx| {
                let _ = ctx.synced.track("Gateway", false);
            }),
        ] {
            let response = f.sync(poll("")).await;
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(response.headers()[header::RETRY_AFTER], "1");
            assert_eq!(f.reviews(), 0);
        }
    }

    #[tokio::test]
    async fn the_token_comes_from_authorization_or_x_portail_token() {
        let _serial = METRICS_SERIAL.lock().await;
        let f = fixture();
        let v1 = f.publish(1);
        let token = gateway_token("default", "gw", 0);
        let request = |header_name: &str, value: String| {
            Request::post(path_of(&key()))
                .header(header_name, value)
                .body(Full::new(Bytes::from(
                    serde_json::to_vec(&body(&key(), "", None)).unwrap(),
                )))
                .unwrap()
        };
        for request in [
            request("authorization", format!("Bearer {token}")),
            request("authorization", format!("bearer {token}")),
            request(TOKEN_HEADER, token.clone()),
        ] {
            assert_eq!(version_of(f.sync(request).await).await, v1);
        }
        let none = post(None, &body(&key(), "", None));
        assert_eq!(f.sync(none).await.status(), StatusCode::UNAUTHORIZED);
        let basic = request("authorization", format!("Basic {token}"));
        assert_eq!(f.sync(basic).await.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn precheck_failures_answer_one_401_without_a_review() {
        let _serial = METRICS_SERIAL.lock().await;
        let f = fixture();
        let other_sa = token_of("default", "portail-other", 0);
        let wrong_ns = token_of("apps", "portail-gw", 0);
        let wrong_aud = super::super::auth::testing::jwt(json!({
            "aud": ["https://kubernetes.default.svc"], "sub": SA,
            "exp": super::super::auth::testing::exp_in(60),
        }));
        let not_sa = super::super::auth::testing::jwt(json!({
            "aud": [super::super::auth::testing::AUDIENCE], "sub": "alice",
            "exp": super::super::auth::testing::exp_in(60),
        }));
        let expired = super::super::auth::testing::jwt(json!({
            "aud": [super::super::auth::testing::AUDIENCE], "sub": SA,
            "exp": super::super::auth::testing::exp_in(-60),
        }));
        let oversized = format!(
            "{}{}",
            gateway_token("default", "gw", 0),
            "A".repeat(16 * 1024)
        );
        let tokens = [
            "not.a.jwt".to_string(),
            oversized,
            wrong_aud,
            not_sa,
            other_sa,
            wrong_ns,
            expired,
        ];
        let before = metrics::sync_auth_rejected(SyncAuthStage::Precheck);
        let mut bodies = Vec::new();
        for token in &tokens {
            let (status, text) =
                text_of(f.sync(post(Some(token), &body(&key(), "", None))).await).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{token:.40}");
            bodies.push(text);
        }
        bodies.dedup();
        assert_eq!(bodies, ["unauthenticated\n"], "one body for every refusal");
        assert_eq!(f.reviews(), 0);
        assert_eq!(
            metrics::sync_auth_rejected(SyncAuthStage::Precheck),
            before + tokens.len() as u64
        );
    }

    /// A body that records whether it was read.
    struct Watched {
        read: Arc<AtomicBool>,
        left: usize,
    }

    impl Body for Watched {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, Infallible>>> {
            self.read.store(true, Ordering::SeqCst);
            if self.left == 0 {
                return Poll::Ready(None);
            }
            let chunk = self.left.min(64 * 1024);
            self.left -= chunk;
            Poll::Ready(Some(Ok(hyper::body::Frame::data(Bytes::from(vec![
                b' ';
                chunk
            ])))))
        }
    }

    #[tokio::test]
    async fn a_junk_token_is_refused_before_its_body_is_read() {
        let _serial = METRICS_SERIAL.lock().await;
        let f = fixture();
        let read = Arc::new(AtomicBool::new(false));
        let request = Request::post(path_of(&key()))
            .header(header::AUTHORIZATION, "Bearer junk")
            .body(Watched {
                read: read.clone(),
                left: 64 * 1024 * 1024,
            })
            .unwrap();
        assert_eq!(
            f.server.handle(request, PEER).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert!(!read.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_body_over_1_mib_is_refused() {
        let f = fixture();
        let read = Arc::new(AtomicBool::new(false));
        let request = Request::post(path_of(&key()))
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", gateway_token("default", "gw", 0)),
            )
            .body(Watched {
                read: read.clone(),
                left: MAX_BODY + 1,
            })
            .unwrap();
        assert_eq!(
            f.server.handle(request, PEER).await.status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );

        let declared = Request::post(path_of(&key()))
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", gateway_token("default", "gw", 0)),
            )
            .header(header::CONTENT_LENGTH, (MAX_BODY + 1).to_string())
            .body(Full::new(Bytes::new()))
            .unwrap();
        assert_eq!(
            f.sync(declared).await.status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    /// 401, 403, 404, 409 for the Gateways and accounts a request names;
    /// Managed or Unmanaged makes no difference to who may sync.
    #[tokio::test]
    async fn the_answer_matrix() {
        let _serial = METRICS_SERIAL.lock().await;
        let mut f = fixture();
        let mut unmanaged = testkit::gateway("gw", 1);
        unmanaged.spec.infrastructure = Some(serde_json::from_value(json!({
            "parametersRef": {"group": "portail.epheo.eu", "kind": "GatewayParameters", "name": "unmanaged"},
        })).unwrap());
        let v1 = f.publish(1);
        for gateway in [testkit::gateway("gw", 1), unmanaged] {
            apply(&mut f.cluster.gateways, gateway);
            assert_eq!(version_of(f.sync(poll("")).await).await, v1);
        }

        let ns = super::super::auth::testing::NS;
        let controller = GatewayKey::new(ns, "controller");
        for name in ["ctl", "controller"] {
            let mut reserved = testkit::gateway(name, 1);
            reserved.metadata.namespace = Some(ns.into());
            apply(&mut f.cluster.gateways, reserved);
        }
        let wrong_protocol = {
            let mut b = body(&key(), "", None);
            b["protocol"] = json!(PROTOCOL + 1);
            b
        };
        let cases = [
            (
                "another namespace's account",
                token_of("apps", "portail-gw", 1),
                body(&key(), "", None),
                StatusCode::UNAUTHORIZED,
            ),
            (
                "another Gateway's account",
                token_of("default", "portail-web", 2),
                body(&key(), "", None),
                StatusCode::UNAUTHORIZED,
            ),
            (
                "the controller's own account",
                token_of(ns, "portail-ctl", 3),
                body(&GatewayKey::new(ns, "ctl"), "", None),
                StatusCode::FORBIDDEN,
            ),
            (
                "a Gateway named after the controller Service",
                gateway_token(ns, "controller", 4),
                body(&controller, "", None),
                StatusCode::FORBIDDEN,
            ),
            (
                "its own account",
                gateway_token("default", "gw", 5),
                body(&GatewayKey::new("default", "gw"), "", None),
                StatusCode::OK,
            ),
            (
                "a newer protocol",
                gateway_token("default", "gw", 6),
                wrong_protocol,
                StatusCode::CONFLICT,
            ),
        ];
        let authz = metrics::sync_auth_rejected(SyncAuthStage::Authz);
        for (why, token, body, status) in cases {
            let (got, text) = text_of(f.sync(post(Some(&token), &body)).await).await;
            assert_eq!(got, status, "{why}: {text}");
        }
        assert_eq!(metrics::sync_auth_rejected(SyncAuthStage::Authz), authz + 2);

        let reviews = f.reviews();
        let unknown = body(&GatewayKey::new("default", "web"), "", None);
        let response = f
            .sync(post(Some(&gateway_token("default", "web", 7)), &unknown))
            .await;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "an unknown Gateway"
        );
        assert_eq!(f.reviews(), reviews);
    }

    #[tokio::test(start_paused = true)]
    async fn a_report_is_recorded_and_wakes_its_gateway_when_it_lapses() {
        let mut f = fixture();
        let mut triggers = f.ctx.reports.triggers().unwrap();
        let v1 = f.publish(1);
        let report = json!({"applied": v1, "dnsEpoch": 3});
        let request = post(
            Some(&gateway_token("default", "gw", 0)),
            &body(&key(), &v1, Some(report)),
        );
        let poll = tokio::spawn({
            let server = f.server.clone();
            async move { server.handle(request, PEER).await }
        });
        tokio::time::sleep(Duration::from_millis(1)).await;
        let live = f.ctx.reports.live(&key());
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].0, InstanceId("pod-1".into()));
        assert_eq!(live[0].1.dns_epoch, 3);
        use futures::StreamExt;
        assert_eq!(triggers.next().await.unwrap().gateway, key());

        assert_eq!(poll.await.unwrap().status(), StatusCode::NO_CONTENT);
        assert!(f.wakes.try_recv().is_err(), "nothing lapsed yet");
        // Last seen at the poll's start: the hold has passed since.
        tokio::time::sleep(LIVENESS - HOLD - Duration::from_millis(10)).await;
        assert!(f.wakes.try_recv().is_err());
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(f.wakes.try_recv().unwrap(), key());
        assert!(
            f.ctx.reports.live(&key()).is_empty(),
            "the wake sees it gone"
        );
    }

    /// A stopping data plane's last report is taken and answered at once:
    /// it waits on that answer, bounded, before it drains.
    #[tokio::test(start_paused = true)]
    async fn a_leaving_report_is_answered_at_once() {
        let f = fixture();
        let v1 = f.publish(1);
        let started = Instant::now();
        let response = f
            .sync(post(
                Some(&gateway_token("default", "gw", 0)),
                &body(&key(), &v1, Some(json!({"applied": v1, "leaving": true}))),
            ))
            .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert!(f.ctx.reports.live(&key()).is_empty(), "no longer live");
    }

    /// A data plane gone silent turns its Gateway Pending on the lapse
    /// wake, not at the resync: the pass the wake runs no longer counts it.
    #[tokio::test]
    async fn a_silent_instance_turns_its_gateway_pending_on_the_lapse() {
        use super::super::reconcile::{finish, publish, Step};
        let mut p = testkit::Pipeline::new();
        p.api
            .reply_to(REVIEWS, 201, review(true, SA, Some("pod-1")));
        let gw = testkit::gateway("gw", 1);
        apply(&mut p.cluster.gateways, gw.clone());
        let (lapses, mut wakes) = Lapses::new();
        let server = SyncServer::new(
            p.ctx.clone(),
            authenticator(p.api.client()),
            lapses,
            CancellationToken::new(),
        );
        let pass = |p: &testkit::Pipeline, gw: Gateway| {
            let ctx = p.ctx.clone();
            async move {
                match publish(&gw, &ctx).await {
                    Step::Done(_) => {}
                    Step::Continue(pass) => {
                        finish(&gw, &ctx, *pass).await.unwrap();
                    }
                }
            }
        };
        let programmed = |p: &testkit::Pipeline| {
            let written = p.api.status_patch("gateways", "gw").unwrap();
            written["status"]["conditions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["type"] == "Programmed")
                .unwrap()
                .clone()
        };

        pass(&p, gw.clone()).await;
        let gw = testkit::with_gateway_status(gw, &p.api.status_patch("gateways", "gw").unwrap());
        apply(&mut p.cluster.gateways, gw.clone());
        let snapshot = p.ctx.hub.current(&key()).unwrap();
        let report = serde_json::to_value(p.applier.apply(&snapshot).await).unwrap();
        let response = server
            .handle(post(
                Some(&gateway_token("default", "gw", 0)),
                &body(&key(), "", Some(report)),
            ))
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        p.api.clear();
        pass(&p, gw.clone()).await;
        assert_eq!(programmed(&p)["status"], "True");
        let gw = testkit::with_gateway_status(gw, &p.api.status_patch("gateways", "gw").unwrap());
        apply(&mut p.cluster.gateways, gw.clone());

        tokio::time::pause();
        let silent = Instant::now();
        use futures::StreamExt;
        assert_eq!(wakes.next().await.unwrap(), key());
        assert!(p.ctx.reports.live(&key()).is_empty(), "woken while live");
        // tokio's timer wheel rounds a deadline up to the millisecond.
        assert!(
            silent.elapsed() <= LIVENESS + Duration::from_millis(1),
            "{:?}",
            silent.elapsed()
        );
        p.api.clear();
        pass(&p, gw).await;
        assert_eq!(programmed(&p)["reason"], "Pending");
    }

    #[tokio::test(start_paused = true)]
    async fn a_report_pushes_its_lapse_back() {
        let mut f = fixture();
        let v1 = f.publish(1);
        let report = || {
            post(
                Some(&gateway_token("default", "gw", 0)),
                &body(&key(), "", Some(json!({"applied": v1}))),
            )
        };
        f.sync(report()).await;
        tokio::time::sleep(LIVENESS - Duration::from_secs(1)).await;
        f.sync(report()).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(f.wakes.try_recv().is_err(), "seen a second ago");
        tokio::time::sleep(LIVENESS).await;
        assert_eq!(f.wakes.try_recv().unwrap(), key());
    }

    /// A kube client whose every request hangs, counting them.
    fn stalled_client(in_flight: Arc<AtomicUsize>) -> kube::Client {
        let service = tower::service_fn(move |_req: http::Request<kube::client::Body>| {
            let in_flight = in_flight.clone();
            async move {
                in_flight.fetch_add(1, Ordering::SeqCst);
                futures::future::pending::<Result<http::Response<kube::client::Body>, Infallible>>()
                    .await
            }
        });
        kube::Client::new(service, "default")
    }

    #[tokio::test]
    async fn seventeen_unknown_tokens_put_sixteen_reviews_in_flight() {
        let _serial = METRICS_SERIAL.lock().await;
        let in_flight = Arc::new(AtomicUsize::new(0));
        let f = fixture();
        let server = SyncServer::new(
            f.ctx.clone(),
            authenticator(stalled_client(in_flight.clone())),
            Lapses::new().0,
            CancellationToken::new(),
        );
        let throttled = METRICS.sync_throttled_total.get();
        let polls: Vec<_> = (0..17)
            .map(|n| {
                let server = server.clone();
                let request = post(
                    Some(&gateway_token("default", "gw", 100 + n)),
                    &body(&key(), "", None),
                );
                tokio::spawn(async move { server.handle(request, peer(n)).await })
            })
            .collect();
        let mut waited = 0;
        while (in_flight.load(Ordering::SeqCst) < 16 || !polls.iter().any(|p| p.is_finished()))
            && waited < 1000
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
            waited += 1;
        }
        assert_eq!(in_flight.load(Ordering::SeqCst), 16);
        let (finished, held): (Vec<_>, Vec<_>) = polls.into_iter().partition(|p| p.is_finished());
        assert_eq!((finished.len(), held.len()), (1, 16));
        let response = finished.into_iter().next().unwrap().await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
        assert_eq!(METRICS.sync_throttled_total.get(), throttled + 1);
        held.iter().for_each(|p| p.abort());
    }

    use crate::metrics::METRICS;

    /// The whole path: TLS from the CA to a client trusting only it, for
    /// the Service name, and held polls end 503 on shutdown.
    #[tokio::test]
    async fn serves_over_tls_to_a_client_trusting_the_ca() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let ns = super::super::auth::testing::NS;
        let ca = super::super::ca::ControllerCa::generated(ns, "portail-controller");
        let f = fixture();
        let v1 = f.publish(1);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let serving = tokio::spawn(serve(f.server.clone(), ca.server_config(), listener));

        let mut roots = rustls::RootCertStore::empty();
        let der =
            rustls::pki_types::pem::PemObject::from_pem_slice(ca.ca_pem().as_bytes()).unwrap();
        roots.add(der).unwrap();
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let name = rustls::pki_types::ServerName::try_from(format!("portail-controller.{ns}.svc"))
            .unwrap();
        let tcp = TcpStream::connect(addr).await.unwrap();
        let tls = connector.connect(name, tcp).await.unwrap();
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
            .await
            .unwrap();
        tokio::spawn(conn);

        let answer = sender.send_request(poll("")).await.unwrap();
        let bytes = answer.into_body().collect().await.unwrap().to_bytes();
        let snapshot: Snapshot = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(snapshot.version, v1);

        let held = tokio::spawn(sender.send_request(poll(&v1)));
        tokio::time::sleep(Duration::from_millis(50)).await;
        f.shutdown.cancel();
        let answer = held.await.unwrap().unwrap();
        assert_eq!(answer.status(), StatusCode::SERVICE_UNAVAILABLE);
        serving.await.unwrap().unwrap();

        let wrong =
            rustls::pki_types::ServerName::try_from("portail-controller.other.svc").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let g = fixture();
        tokio::spawn(serve(g.server.clone(), ca.server_config(), listener));
        let tcp = TcpStream::connect(addr).await.unwrap();
        assert!(connector.connect(wrong, tcp).await.is_err());
        g.shutdown.cancel();
    }

    /// The CA and a TLS client trusting only it, for the Service's name.
    fn tls_client() -> (
        super::super::ca::ControllerCa,
        tokio_rustls::TlsConnector,
        rustls::pki_types::ServerName<'static>,
    ) {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let ns = super::super::auth::testing::NS;
        let ca = super::super::ca::ControllerCa::generated(ns, "portail-controller");
        let mut roots = rustls::RootCertStore::empty();
        let der =
            rustls::pki_types::pem::PemObject::from_pem_slice(ca.ca_pem().as_bytes()).unwrap();
        roots.add(der).unwrap();
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(format!("portail-controller.{ns}.svc"))
            .unwrap();
        (ca, tokio_rustls::TlsConnector::from(Arc::new(config)), name)
    }

    /// Another Gateway's account is refused from the path, before any
    /// review; a token TokenReview rejects never has its body read either.
    #[tokio::test]
    async fn the_body_is_read_only_once_the_token_is_reviewed() {
        let _serial = METRICS_SERIAL.lock().await;
        let f = fixture();
        let watched = |token: &str, read: &Arc<AtomicBool>| {
            Request::post(path_of(&key()))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Watched {
                    read: read.clone(),
                    left: 1024,
                })
                .unwrap()
        };
        let read = Arc::new(AtomicBool::new(false));
        let other = token_of("default", "portail-other", 0);
        let response = f.server.handle(watched(&other, &read), PEER).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(!read.load(Ordering::SeqCst));
        assert_eq!(f.reviews(), 0);

        f.api.reply_to(REVIEWS, 201, review(false, "", None));
        let rejected = gateway_token("default", "gw", 1);
        let response = f.server.handle(watched(&rejected, &read), PEER).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(!read.load(Ordering::SeqCst));
        assert_eq!(f.reviews(), 1);
    }

    #[tokio::test]
    async fn the_body_must_name_the_gateway_of_the_path() {
        let f = fixture();
        let request = Request::post(path_of(&key()))
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", gateway_token("default", "gw", 0)),
            )
            .body(Full::new(Bytes::from(
                serde_json::to_vec(&body(&GatewayKey::new("default", "other"), "", None)).unwrap(),
            )))
            .unwrap();
        assert_eq!(f.sync(request).await.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_path_without_one_gateway_is_404() {
        let f = fixture();
        let token = gateway_token("default", "gw", 0);
        for path in [
            "/v1/sync",
            "/v1/sync/",
            "/v1/sync/default",
            "/v1/sync/default/",
            "/v1/sync//gw",
            "/v1/sync/default/gw/x",
            "/v2/sync/default/gw",
        ] {
            let request = Request::post(path)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Full::new(Bytes::from(
                    serde_json::to_vec(&body(&key(), "", None)).unwrap(),
                )))
                .unwrap();
            assert_eq!(
                f.sync(request).await.status(),
                StatusCode::NOT_FOUND,
                "{path}"
            );
        }
        assert_eq!(f.reviews(), 0);
    }

    /// A body that never ends.
    struct Stalled;

    impl Body for Stalled {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, Infallible>>> {
            Poll::Pending
        }
    }

    #[tokio::test(start_paused = true)]
    async fn body_reads_are_capped_and_a_stalled_body_ends_in_5s() {
        let f = Arc::new(fixture());
        f.publish(1);
        // One reviewed token per pod: one pod reads one body at a time.
        let tokens: Vec<String> = (0..=MAX_BODY_READS as u32)
            .map(|n| gateway_token("default", "gw", n))
            .collect();
        for (n, token) in tokens.iter().enumerate() {
            f.api
                .reply_to(REVIEWS, 201, review(true, SA, Some(&format!("pod-{n}"))));
            let reviewed = post(Some(token), &body(&key(), "", None));
            assert_eq!(f.sync(reviewed).await.status(), StatusCode::OK);
        }
        let started = Instant::now();
        let stalled: Vec<_> = tokens[..MAX_BODY_READS]
            .iter()
            .map(|token| {
                let request = Request::post(path_of(&key()))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Stalled)
                    .unwrap();
                let server = f.server.clone();
                tokio::spawn(async move { server.handle(request, PEER).await })
            })
            .collect();
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        let last = post(Some(&tokens[MAX_BODY_READS]), &body(&key(), "", None));
        let over = f.sync(last).await;
        assert_eq!(over.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(over.headers()[header::RETRY_AFTER], "1");

        for request in stalled {
            assert_eq!(request.await.unwrap().status(), StatusCode::BAD_REQUEST);
        }
        assert_eq!(started.elapsed(), PEER_TIMEOUT);
        let v2 = f.publish(2);
        assert_eq!(version_of(f.sync(poll("")).await).await, v2, "freed");
    }

    #[tokio::test]
    async fn connections_over_the_cap_wait_to_be_accepted() {
        let (ca, connector, name) = tls_client();
        let f = fixture();
        let server = SyncServer::with_connections(
            f.ctx.clone(),
            authenticator(f.api.client()),
            Lapses::new().0,
            f.shutdown.clone(),
            1,
            MAX_CONNECTIONS_PER_PEER,
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(server, ca.server_config(), listener));

        let holder = TcpStream::connect(addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let waiting = TcpStream::connect(addr).await.unwrap();
        let handshake = connector.connect(name.clone(), waiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(300), handshake)
                .await
                .is_err(),
            "accepted over the cap"
        );

        drop(holder);
        let next = TcpStream::connect(addr).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), connector.connect(name, next))
            .await
            .expect("accepted once a slot freed")
            .unwrap();
        f.shutdown.cancel();
    }

    #[tokio::test]
    async fn a_request_head_over_32_kib_is_refused() {
        let (ca, connector, name) = tls_client();
        let f = fixture();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(f.server.clone(), ca.server_config(), listener));
        let tcp = TcpStream::connect(addr).await.unwrap();
        let tls = connector.connect(name, tcp).await.unwrap();
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
            .await
            .unwrap();
        tokio::spawn(conn);
        let mut request = poll("");
        request
            .headers_mut()
            .insert("x-pad", "a".repeat(MAX_HEAD).parse().unwrap());
        let answer = sender.send_request(request).await.unwrap();
        assert_eq!(answer.status(), StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE);
        assert_eq!(f.reviews(), 0);
        f.shutdown.cancel();
    }

    /// A peer that connects and says nothing holds its connection 5s.
    #[tokio::test(start_paused = true)]
    async fn a_silent_peer_is_dropped_in_5s() {
        let (ca, _, _) = tls_client();
        let f = fixture();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(f.server.clone(), ca.server_config(), listener));
        let mut silent = TcpStream::connect(addr).await.unwrap();
        let started = Instant::now();
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 16];
        assert!(!matches!(silent.read(&mut buf).await, Ok(n) if n > 0));
        assert_eq!(started.elapsed(), PEER_TIMEOUT);
        f.shutdown.cancel();
    }

    /// Nothing but an answered poll keeps a connection: a refusal closes it,
    /// so a peer cannot pin a slot with one junk request per head timeout.
    #[tokio::test]
    async fn refusals_close_the_connection() {
        let _serial = METRICS_SERIAL.lock().await;
        let f = fixture();
        f.publish(1);
        let closes = |response: &Response<Full<Bytes>>| {
            response
                .headers()
                .get(header::CONNECTION)
                .is_some_and(|v| v == "close")
        };
        let served = f.sync(poll("")).await;
        assert_eq!(served.status(), StatusCode::OK);
        assert!(!closes(&served));
        let refusals = [
            post(Some("junk"), &body(&key(), "", None)),
            post(
                Some(&gateway_token("default", "gw", 0)),
                &body(&GatewayKey::new("default", "ghost"), "", None),
            ),
            Request::get(path_of(&key())).body(Full::default()).unwrap(),
        ];
        for request in refusals {
            let response = f.sync(request).await;
            assert!(response.status().is_client_error(), "{}", response.status());
            assert!(closes(&response), "{}", response.status());
        }
    }

    #[tokio::test]
    async fn one_peer_holds_its_share_of_connections() {
        let (ca, connector, name) = tls_client();
        let f = fixture();
        let server = SyncServer::with_connections(
            f.ctx.clone(),
            authenticator(f.api.client()),
            Lapses::new().0,
            f.shutdown.clone(),
            MAX_CONNECTIONS,
            1,
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(server, ca.server_config(), listener));

        let holder = TcpStream::connect(addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let over = TcpStream::connect(addr).await.unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_secs(2),
                connector.connect(name.clone(), over)
            )
            .await
            .expect("closed at once, not left waiting")
            .is_err(),
            "a second connection from the same peer"
        );
        drop(holder);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let next = TcpStream::connect(addr).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), connector.connect(name, next))
            .await
            .unwrap()
            .expect("its share is free again");
        f.shutdown.cancel();
    }

    /// A write that makes no progress fails after the peer timeout; one that
    /// does resets it.
    #[tokio::test(start_paused = true)]
    async fn a_stalled_write_fails_in_5s() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (near, mut far) = tokio::io::duplex(64);
        let mut guarded = StallGuard::new(near);
        guarded.write_all(&[0; 64]).await.unwrap();
        tokio::time::sleep(PEER_TIMEOUT - Duration::from_secs(1)).await;
        let mut drained = [0; 64];
        far.read_exact(&mut drained).await.unwrap();
        guarded.write_all(&[0; 64]).await.unwrap();

        let started = Instant::now();
        let stalled = guarded.write_all(&[0; 64]).await.unwrap_err();
        assert_eq!(stalled.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(started.elapsed(), PEER_TIMEOUT);
    }

    /// A data plane that asks for a snapshot and never reads it loses its
    /// connection, and the slot, once the write stalls.
    #[tokio::test]
    async fn a_peer_that_stops_reading_frees_its_slot() {
        let (ca, connector, name) = tls_client();
        let f = fixture();
        let mut snapshot = Snapshot::from_config(
            crate::sync::GatewayRef {
                namespace: "default".into(),
                name: "gw".into(),
                uid: "gw-uid".into(),
                generation: 1,
            },
            PortailConfig::default(),
            vec![],
        );
        // Past what loopback socket buffers absorb.
        snapshot.certificates.push(crate::sync::CertificatePem {
            namespace: "default".into(),
            name: "big".into(),
            cert_pem: "a".repeat(64 * 1024 * 1024),
            key_pem: String::new(),
        });
        snapshot.version = "big".into();
        assert!(f.ctx.hub.publish(snapshot));
        let server = SyncServer::with_connections(
            f.ctx.clone(),
            authenticator(f.api.client()),
            Lapses::new().0,
            f.shutdown.clone(),
            1,
            MAX_CONNECTIONS_PER_PEER,
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(server, ca.server_config(), listener));

        let tcp = TcpStream::connect(addr).await.unwrap();
        let mut tls = connector.connect(name.clone(), tcp).await.unwrap();
        let request = format!(
            "POST {} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n{}",
            path_of(&key()),
            gateway_token("default", "gw", 0),
            serde_json::to_vec(&body(&key(), "", None)).unwrap().len(),
            body(&key(), "", None),
        );
        use tokio::io::AsyncWriteExt;
        tls.write_all(request.as_bytes()).await.unwrap();

        let started = std::time::Instant::now();
        let next = TcpStream::connect(addr).await.unwrap();
        tokio::time::timeout(Duration::from_secs(15), connector.connect(name, next))
            .await
            .expect("the stalled connection gave its slot back")
            .unwrap();
        assert!(started.elapsed() >= PEER_TIMEOUT - Duration::from_millis(100));
        drop(tls);
        f.shutdown.cancel();
    }

    /// One pod reads one body at a time: a tenant with a valid token
    /// stalling bodies holds one read, not every other data plane's.
    #[tokio::test(start_paused = true)]
    async fn one_pod_reads_one_body_at_a_time() {
        let f = Arc::new(fixture());
        let v1 = f.publish(1);
        assert_eq!(
            version_of(f.sync(poll("")).await).await,
            v1,
            "pod-1 reviewed"
        );
        let token = gateway_token("default", "gw", 0);
        let stalled: Vec<_> = (0..MAX_BODY_READS as u32)
            .map(|n| {
                let request = Request::post(path_of(&key()))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Stalled)
                    .unwrap();
                let server = f.server.clone();
                tokio::spawn(async move { server.handle(request, peer(n)).await })
            })
            .collect();
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        let refused = stalled.iter().filter(|s| s.is_finished()).count();
        assert_eq!(refused, MAX_BODY_READS - 1, "all but one answered at once");

        f.api
            .reply_to(REVIEWS, 201, review(true, SA, Some("pod-2")));
        let other = post(
            Some(&gateway_token("default", "gw", 1)),
            &body(&key(), "", None),
        );
        assert_eq!(version_of(f.sync(other).await).await, v1, "pod-2 syncs");
        stalled.iter().for_each(|s| s.abort());
    }

    /// More data planes hold polls than bodies may be read at once.
    #[tokio::test(start_paused = true)]
    async fn held_polls_hold_no_body_read() {
        let f = Arc::new(fixture());
        let v1 = f.publish(1);
        let pods = 0..=MAX_BODY_READS as u32;
        for n in pods.clone() {
            f.api
                .reply_to(REVIEWS, 201, review(true, SA, Some(&format!("pod-{n}"))));
            let reviewed = post(
                Some(&gateway_token("default", "gw", n)),
                &body(&key(), "", None),
            );
            assert_eq!(version_of(f.sync(reviewed).await).await, v1);
        }
        let held: Vec<_> = pods
            .map(|n| {
                let request = post(
                    Some(&gateway_token("default", "gw", n)),
                    &body(&key(), &v1, None),
                );
                let server = f.server.clone();
                tokio::spawn(async move { server.handle(request, peer(n)).await })
            })
            .collect();
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert!(held.iter().all(|h| !h.is_finished()), "every poll holds");
        let v2 = f.publish(2);
        for poll in held {
            assert_eq!(version_of(poll.await.unwrap()).await, v2);
        }
    }
}
