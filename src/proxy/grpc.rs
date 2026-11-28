//! gRPC: an h2 stream proxied to an h2 backend frame by frame, its
//! trailers both ways, which the bridge's HTTP/1 replay cannot carry.
//!
//! On a port with gRPC routes, a stream is gRPC by its content-type, and
//! then matched against the table's gRPC routes only
//! (`RouteTable::find_grpc_route`): no match is UNIMPLEMENTED, never an
//! HTTP route. Its backend is spoken h2 whatever its port's appProtocol:
//! h2c in the clear, ALPN h2 over TLS, one connection per backend for each
//! client connection (see [`Backends`]).
//!
//! No timeout of the proxy's bounds a call: gRPC's deadlines are the
//! client's, which cancels the stream when one passes.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::Poll;

use anyhow::{anyhow, Result};
use bytes::Bytes;
use http::header::{HeaderName, HeaderValue};
use http::HeaderMap;

use crate::logging::{debug, warn};
use crate::metrics::METRICS;
use crate::proxy::backend_pool::{connect_tcp, BackendProto, ConnectError};
use crate::proxy::h2_bridge::{unless_reset, H2Ctx};
use crate::proxy::health::HealthRegistry;
use crate::proxy::http_filters::{extract_header_mods, HeaderModifications};
use crate::routing::{Backend, BackendAction, HttpRouteRule, RouteTable};

/// gRPC status codes the proxy answers with itself.
const INTERNAL: u8 = 13;
const UNIMPLEMENTED: u8 = 12;
const UNAVAILABLE: u8 = 14;

/// Whether a request is gRPC: its content-type is `application/grpc`, or
/// one of its `+proto`, `+json` forms, parameters allowed. Not gRPC-Web
/// (`application/grpc-web`), another protocol, its trailers in the body.
pub(crate) fn is_grpc(headers: &HeaderMap) -> bool {
    const GRPC: &[u8] = b"application/grpc";
    headers.get(http::header::CONTENT_TYPE).is_some_and(|ct| {
        let ct = ct.as_bytes();
        ct.len() >= GRPC.len()
            && ct[..GRPC.len()].eq_ignore_ascii_case(GRPC)
            && matches!(ct.get(GRPC.len()), None | Some(b'+' | b';'))
    })
}

/// One gRPC stream: matched in `routes`, sent to its backend, and its
/// answer streamed back with the backend's trailers.
///
/// `routes` is the table the stream started under, for as long as it
/// lasts: a streaming call holds it as an upgraded tunnel does.
pub(crate) async fn serve_stream(
    req: http::Request<h2::RecvStream>,
    mut respond: h2::server::SendResponse<Bytes>,
    ctx: Arc<H2Ctx>,
    routes: Arc<RouteTable>,
) {
    let (parts, body) = req.into_parts();
    let Some(host) = parts.uri.authority().map(|a| a.host()) else {
        METRICS.http_requests_rejected_total.inc();
        return answer(&mut respond, INTERNAL, "no authority");
    };
    let Some(rule) = route(&routes, host, &parts, ctx.server_port) else {
        return answer(&mut respond, UNIMPLEMENTED, "no route");
    };
    rule.requests.fetch_add(1, Ordering::Relaxed);
    ctx.listener_stats.http_requests.inc();
    let Some(backend) = ctx
        .selector
        .select_healthy_weighted_backend(rule, &ctx.health)
        .map(|index| &rule.backends[index])
    else {
        return answer(&mut respond, UNAVAILABLE, "no backend");
    };
    match backend.action {
        BackendAction::Forward => {}
        BackendAction::Reject => return answer(&mut respond, INTERNAL, "backend refused"),
        // A PROXY header goes on a connection of one client's: a backend's
        // h2 connection carries many.
        BackendAction::ForwardProxied | BackendAction::Unavailable => {
            return answer(&mut respond, UNAVAILABLE, "backend unavailable")
        }
    }
    let request = match backend_request(&parts, rule, backend, &ctx) {
        Ok(request) => request,
        Err(e) => {
            debug!("gRPC request from {} not forwardable: {}", ctx.peer, e);
            return answer(&mut respond, INTERNAL, "request not forwardable");
        }
    };
    // A connect waits out the client's reset: its slot frees on it.
    let connected = async { Ok::<_, h2::Error>(ready_connection(backend, &ctx).await) };
    let ready = unless_reset(connected, |cx| respond.poll_reset(cx));
    let mut send = match ready.await {
        Ok(Ok(send)) => send,
        Ok(Err(why)) => return answer(&mut respond, UNAVAILABLE, why),
        Err(_) => return,
    };
    let ended = body.is_end_stream();
    let (response, to_backend) = match send.send_request(request, ended) {
        Ok(sent) => sent,
        Err(e) => {
            debug!("gRPC request to {} failed: {}", backend.socket_addr, e);
            return answer(&mut respond, UNAVAILABLE, "backend failed");
        }
    };

    // The client's messages go on while the backend answers: a streaming
    // call's answer may begin before its request ends. The answer's end ends
    // the call: what the client still sends is dropped with its stream.
    let downloaded = {
        let mut upload = std::pin::pin!(async move {
            if !ended {
                pump(body, to_backend).await?;
            }
            Ok::<_, h2::Error>(())
        });
        let mut download = std::pin::pin!(async {
            let response = unless_reset(response, |cx| respond.poll_reset(cx)).await?;
            let (mut head, body) = response.into_parts();
            if let Some(mods) = response_mods(rule, backend) {
                apply_mods(&mut head.headers, &mods);
            }
            let ended = body.is_end_stream();
            let to_client = respond.send_response(http::Response::from_parts(head, ()), ended)?;
            if !ended {
                pump(body, to_client).await?;
            }
            Ok::<_, anyhow::Error>(())
        });
        let mut uploading = true;
        std::future::poll_fn(|cx| {
            if uploading {
                if let Poll::Ready(uploaded) = upload.as_mut().poll(cx) {
                    uploading = false;
                    if let Err(e) = uploaded {
                        debug!("gRPC request body to {} ended: {}", backend.socket_addr, e);
                    }
                }
            }
            download.as_mut().poll(cx)
        })
        .await
    };
    if let Err(e) = downloaded {
        debug!("gRPC answer from {} ended: {}", backend.socket_addr, e);
        // The backend's reason goes on to the client: a CANCEL stays one.
        let reason = e
            .downcast_ref::<h2::Error>()
            .and_then(h2::Error::reason)
            .unwrap_or(h2::Reason::INTERNAL_ERROR);
        respond.send_reset(reason);
    }
}

/// The rule a gRPC request takes, by its host and `/{service}/{method}`
/// path.
fn route<'a>(
    routes: &'a RouteTable,
    host: &str,
    parts: &http::request::Parts,
    server_port: u16,
) -> Option<&'a HttpRouteRule> {
    // Header matches read header lines, as the HTTP table's do.
    let mut lines = Vec::new();
    for (name, value) in &parts.headers {
        lines.extend_from_slice(name.as_str().as_bytes());
        lines.extend_from_slice(b": ");
        lines.extend_from_slice(value.as_bytes());
        lines.extend_from_slice(b"\r\n");
    }
    routes.find_grpc_route(host, parts.uri.path(), &lines, server_port)
}

/// An answer of the proxy's own, trailers only: the call ends with `code`.
fn answer(respond: &mut h2::server::SendResponse<Bytes>, code: u8, message: &'static str) {
    let response = http::Response::builder()
        .header(http::header::CONTENT_TYPE, "application/grpc")
        .header("grpc-status", u16::from(code))
        .header("grpc-message", message)
        .body(())
        .expect("a static head");
    let _ = respond.send_response(response, true);
}

/// The request as the backend gets it: the client's, with the rule's and
/// the backend's header modifications, and the client and its scheme
/// appended to `x-forwarded-for` and `x-forwarded-proto`, as HTTP/1 does.
fn backend_request(
    parts: &http::request::Parts,
    rule: &HttpRouteRule,
    backend: &Backend,
    ctx: &H2Ctx,
) -> Result<http::Request<()>> {
    let mut uri = parts.uri.clone().into_parts();
    uri.scheme = Some(match backend.proto {
        BackendProto::Plain => http::uri::Scheme::HTTP,
        _ => http::uri::Scheme::HTTPS,
    });
    let mut request = http::Request::builder()
        .method(parts.method.clone())
        .uri(http::Uri::from_parts(uri)?)
        .version(http::Version::HTTP_2)
        .body(())?;
    let headers = request.headers_mut();
    *headers = parts.headers.clone();
    headers.remove(http::header::HOST);
    for mods in [
        extract_header_mods(&rule.filters, false),
        extract_header_mods(&backend.filters, false),
    ]
    .into_iter()
    .flatten()
    {
        apply_mods(headers, &mods);
    }
    headers.append(
        HeaderName::from_static("x-forwarded-for"),
        HeaderValue::from_str(&ctx.peer.ip().to_string())?,
    );
    headers.append(
        HeaderName::from_static("x-forwarded-proto"),
        HeaderValue::from_static(if ctx.is_tls { "https" } else { "http" }),
    );
    Ok(request)
}

/// The rule's response modifier, else its backend's, as HTTP/1 takes one.
fn response_mods<'a>(
    rule: &'a HttpRouteRule,
    backend: &'a Backend,
) -> Option<HeaderModifications<'a>> {
    extract_header_mods(&rule.filters, true).or_else(|| extract_header_mods(&backend.filters, true))
}

/// A header modifier applied to h2 headers: remove, then set, then add, as
/// the HTTP/1 path does.
fn apply_mods(headers: &mut HeaderMap, mods: &HeaderModifications<'_>) {
    for name in mods.remove {
        headers.remove(name.as_str());
    }
    for (header, append) in mods
        .set
        .iter()
        .map(|h| (h, false))
        .chain(mods.add.iter().map(|h| (h, true)))
    {
        // Checked when the rule compiled: what fails here is skipped.
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(header.name.as_bytes()),
            HeaderValue::from_str(&header.value),
        ) else {
            continue;
        };
        if append {
            headers.append(name, value);
        } else {
            headers.insert(name, value);
        }
    }
}

/// Every frame of `from` sent on to `to` under `to`'s flow control, then
/// its trailers or its end. A frame's window is given back to the sender
/// only once it is sent on: a reader that stops reading holds the writer.
async fn pump(mut from: h2::RecvStream, mut to: h2::SendStream<Bytes>) -> Result<(), h2::Error> {
    while let Some(chunk) = from.data().await {
        let mut chunk = chunk?;
        let read = chunk.len();
        while !chunk.is_empty() {
            to.reserve_capacity(chunk.len());
            let granted = std::future::poll_fn(|cx| to.poll_capacity(cx))
                .await
                .ok_or(h2::Reason::CANCEL)??;
            // A window that shrank since: none granted yet.
            if granted == 0 {
                continue;
            }
            to.send_data(chunk.split_to(granted.min(chunk.len())), false)?;
        }
        from.flow_control().release_capacity(read)?;
    }
    match from.trailers().await? {
        Some(trailers) => to.send_trailers(trailers),
        None => to.send_data(Bytes::new(), true),
    }
}

/// What a backend's h2 connection is kept by: its address, and how it is
/// spoken to (see [`BackendProto`]).
#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    addr: SocketAddr,
    tls: Option<(bool, Box<str>)>,
}

impl Key {
    fn of(backend: &Backend) -> Self {
        let tls = match &backend.proto {
            BackendProto::Plain => None,
            BackendProto::Tls(name) => Some((false, name.clone())),
            BackendProto::Verified(verify) => Some((true, verify.key().into())),
        };
        Self {
            addr: backend.socket_addr,
            tls,
        }
    }
}

/// A backend's connection, and which it is: an entry is removed only by
/// its own connection's end or failure, never a newer one's.
struct Held {
    id: u64,
    send: h2::client::SendRequest<Bytes>,
}

type HeldMap = Mutex<HashMap<Key, Held>>;

/// A client connection's h2 connections to its gRPC backends, one per
/// backend, its streams sharing it as its HTTP/1 exchanges share its pool.
/// Per client, not per process: the h2 crate gives a stream's window back
/// to the connection only as the stream is read, so a client that stops
/// reading holds the connection's window, and stalls only its own streams.
/// They close with the client's connection.
#[derive(Default)]
pub(crate) struct Backends(Arc<HeldMap>);

impl Backends {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Key, Held>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn forget(held: &HeldMap, key: &Key, id: u64) {
    let mut held = held.lock().unwrap_or_else(PoisonError::into_inner);
    if held.get(key).is_some_and(|h| h.id == id) {
        held.remove(key);
    }
}

/// The backend's h2 connection, ready for a stream: the one held, else a
/// new one. `Err` is why none is, for the client.
async fn ready_connection(
    backend: &Backend,
    ctx: &H2Ctx,
) -> Result<h2::client::SendRequest<Bytes>, &'static str> {
    let key = Key::of(backend);
    let held = ctx
        .grpc
        .lock()
        .get(&key)
        .map(|held| (held.id, held.send.clone()));
    if let Some((id, send)) = held {
        match send.ready().await {
            Ok(send) => return Ok(send),
            // Closed since, by a GOAWAY or a failure: nothing went on it, so
            // the stream takes a new one.
            Err(e) => {
                debug!("gRPC backend {} connection closed: {}", key.addr, e);
                forget(&ctx.grpc.0, &key, id);
            }
        }
    }
    // Boxed: a connect's future, TLS and h2 handshakes in it, is many times
    // the rest of a stream's, and would be moved into every stream's.
    Box::pin(connect(backend, ctx, key)).await
}

/// A new connection to `backend`, held for `ctx`'s next streams, and ready.
async fn connect(
    backend: &Backend,
    ctx: &H2Ctx,
    key: Key,
) -> Result<h2::client::SendRequest<Bytes>, &'static str> {
    let addr = backend.socket_addr;
    let (id, send) = match open(backend, ctx, key.clone()).await {
        Ok(opened) => opened,
        Err(ConnectError::Local(e)) => {
            warn!(
                "gRPC backend {}: no connection for want of resources: {}",
                addr, e
            );
            return Err("proxy out of resources");
        }
        Err(ConnectError::Backend(e)) => {
            warn!("gRPC backend {} connect failed: {}", addr, e);
            if ctx.health.record_failure(addr) {
                HealthRegistry::spawn_probe(ctx.health.clone(), addr, backend.proto.clone(), true);
            }
            return Err("backend connect failed");
        }
    };
    ctx.health.record_success(addr);
    // Two streams may have opened one each: the later is kept, the earlier
    // serves its own stream and closes when it is done.
    ctx.grpc.lock().insert(
        key.clone(),
        Held {
            id,
            send: send.clone(),
        },
    );
    send.ready().await.map_err(|e| {
        debug!("gRPC backend {} closed a new connection: {}", addr, e);
        forget(&ctx.grpc.0, &key, id);
        "backend closed the connection"
    })
}

async fn open(
    backend: &Backend,
    ctx: &H2Ctx,
    key: Key,
) -> Result<(u64, h2::client::SendRequest<Bytes>), ConnectError> {
    let addr = backend.socket_addr;
    let tcp = connect_tcp(
        addr,
        ctx.cfg.connect_timeout,
        &crate::proxy::listener::tcp_keepalive(ctx.cfg.tcp_keepalive_time),
    )
    .await?;
    let failed = |e: &dyn std::fmt::Display| {
        ConnectError::Backend(anyhow!("gRPC backend {} handshake failed: {}", addr, e))
    };
    // A backend that accepts and then says nothing holds no stream past the
    // connect's own budget, given again for the TLS and h2 handshakes.
    let handshakes = async {
        let conn = backend
            .proto
            .as_ref()
            .over_h2(tcp)
            .await
            .map_err(|e| failed(&e))?;
        handshake(conn, key, ctx).await.map_err(|e| failed(&e))
    };
    tokio::time::timeout(ctx.cfg.connect_timeout, handshakes)
        .await
        .unwrap_or_else(|_| Err(failed(&"timed out")))
}

/// The h2 handshake on a new backend connection, which then runs until the
/// backend or the client's connection ends it, and leaves `ctx`'s held ones.
async fn handshake<S>(
    io: S,
    key: Key,
    ctx: &H2Ctx,
) -> Result<(u64, h2::client::SendRequest<Bytes>), h2::Error>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    static IDS: AtomicU64 = AtomicU64::new(0);
    // The client side's windows: what the backend sends a stream waits
    // there for its client as the client's upload waits for its backend.
    let (send, connection) = h2::client::Builder::new()
        .initial_window_size(crate::proxy::h2_bridge::STREAM_WINDOW)
        .initial_connection_window_size(crate::proxy::h2_bridge::CONNECTION_WINDOW)
        .max_header_list_size(crate::proxy::h2_bridge::MAX_HEADER_LIST_SIZE)
        .handshake::<_, Bytes>(io)
        .await?;
    let id = IDS.fetch_add(1, Ordering::Relaxed);
    // Weak: the held connection must not keep its own list alive.
    let held = Arc::downgrade(&ctx.grpc.0);
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            debug!("gRPC backend {} connection ended: {}", key.addr, e);
        }
        if let Some(held) = held.upgrade() {
            forget(&held, &key, id);
        }
    });
    Ok((id, send))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use arc_swap::ArcSwap;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
    use tokio_util::sync::CancellationToken;

    use crate::metrics::ListenerStats;
    use crate::proxy::backend_pool::SharedBackendPool;
    use crate::proxy::listener::ServeConfig;
    use crate::routing::BackendSelector;

    /// An h2c gRPC backend. Each message it gets is echoed at once, so a
    /// call's answer goes on while its request does; the call ends with
    /// grpc-status 0 and an `x-trailer`. `/test.Echo/Cancel` is reset with
    /// CANCEL; `/test.Echo/Flood` answers 8 MiB as fast as its windows let
    /// it, counted in `FLOODED`. With `one_call`, a connection closes after
    /// its first stream.
    async fn echo_backend(one_call: bool) -> (SocketAddr, UnboundedReceiver<http::request::Parts>) {
        echo_backend_over(one_call, None).await
    }

    /// [`echo_backend`] over TLS by `tls`, which, as grpc-go since 1.67,
    /// refuses a client that negotiates no h2.
    async fn echo_backend_over(
        one_call: bool,
        tls: Option<Arc<rustls::ServerConfig>>,
    ) -> (SocketAddr, UnboundedReceiver<http::request::Parts>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen, rx) = unbounded_channel();
        tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let (seen, tls) = (seen.clone(), tls.clone());
                tokio::spawn(async move {
                    let Some(tls) = tls else {
                        return serve_echo(tcp, seen, one_call).await;
                    };
                    let Ok(tls) = tokio_rustls::TlsAcceptor::from(tls).accept(tcp).await else {
                        return;
                    };
                    if tls.get_ref().1.alpn_protocol() == Some(b"h2") {
                        serve_echo(tls, seen, one_call).await;
                    }
                });
            }
        });
        (addr, rx)
    }

    async fn serve_echo<S>(
        io: S,
        seen: tokio::sync::mpsc::UnboundedSender<http::request::Parts>,
        one_call: bool,
    ) where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let Ok(mut conn) = h2::server::handshake(io).await else {
            return;
        };
        while let Some(Ok((req, mut respond))) = conn.accept().await {
            let (parts, mut body) = req.into_parts();
            let cancel = parts.uri.path() == "/test.Echo/Cancel";
            let flood = parts.uri.path() == "/test.Echo/Flood";
            let _ = seen.send(parts);
            if cancel {
                respond.send_reset(h2::Reason::CANCEL);
            } else {
                tokio::spawn(async move {
                    let head = http::Response::builder()
                        .header("content-type", "application/grpc")
                        .body(())
                        .unwrap();
                    let mut out = respond.send_response(head, false).unwrap();
                    let mut flooding = if flood { 8 << 20 } else { 0 };
                    while flooding > 0 {
                        out.reserve_capacity(flooding);
                        let Some(Ok(granted)) =
                            std::future::poll_fn(|cx| out.poll_capacity(cx)).await
                        else {
                            return;
                        };
                        let granted = granted.min(flooding);
                        if granted > 0 {
                            out.send_data(Bytes::from(vec![7; granted]), false).unwrap();
                            FLOODED.fetch_add(granted, Ordering::Relaxed);
                            flooding -= granted;
                        }
                    }
                    while let Some(Ok(chunk)) = body.data().await {
                        let _ = body.flow_control().release_capacity(chunk.len());
                        out.send_data(chunk, false).unwrap();
                    }
                    let mut trailers = HeaderMap::new();
                    trailers.insert("grpc-status", HeaderValue::from_static("0"));
                    trailers.insert("x-trailer", HeaderValue::from_static("seen"));
                    out.send_trailers(trailers).unwrap();
                });
            }
            if one_call {
                conn.graceful_shutdown();
            }
        }
    }

    static FLOODED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn table(backend: SocketAddr) -> RouteTable {
        let config: crate::config::PortailConfig = serde_json::from_value(serde_json::json!({
            "gateway": {"name": "g", "listeners": [{"name": "l", "protocol": "HTTP", "port": 8080}]},
            "grpcRoutes": [{
                "parentRefs": [{"name": "g", "sectionName": "l"}],
                "hostnames": ["grpc.test"],
                "rules": [{
                    "matches": [{"method": {"service": "test.Echo"}}],
                    "filters": [
                        {"type": "RequestHeaderModifier",
                         "requestHeaderModifier": {"set": [{"name": "x-set", "value": "yes"}]}},
                        {"type": "ResponseHeaderModifier",
                         "responseHeaderModifier": {"add": [{"name": "x-added", "value": "yes"}]}}
                    ],
                    "backendRefs": [{"name": backend.ip().to_string(), "port": backend.port()}]
                }]
            }]
        }))
        .unwrap();
        config.validate().unwrap();
        config.to_route_table_strict().unwrap()
    }

    /// An h2c client of the h2 server, serving `routes` on port 8080.
    async fn client(routes: RouteTable) -> h2::client::SendRequest<Bytes> {
        let (io, server_io) = tokio::io::duplex(64 * 1024);
        let cfg = ServeConfig {
            max_idle_per_backend: 4,
            connect_timeout: Duration::from_secs(2),
            client_header_timeout: Duration::from_secs(5),
            tcp_keepalive_time: Duration::from_secs(60),
            idle_body_timeout: Duration::from_secs(5),
            http2: false,
        };
        let ctx = H2Ctx {
            server_port: 8080,
            routes: Arc::new(ArcSwap::from_pointee(routes)),
            selector: Arc::new(BackendSelector::new()),
            health: Arc::new(HealthRegistry::new()),
            pool: Arc::new(SharedBackendPool::new(4, cfg.connect_timeout)),
            cfg,
            listener_stats: Arc::new(ListenerStats::default()),
            peer: "10.1.2.3:40000".parse().unwrap(),
            local: "127.0.0.1:8080".parse().unwrap(),
            drain: CancellationToken::new(),
            is_tls: false,
            grpc: Default::default(),
        };
        tokio::spawn(crate::proxy::h2_bridge::serve(server_io, ctx));
        let (send, conn) = h2::client::handshake(io).await.unwrap();
        tokio::spawn(async move {
            let _ = conn.await;
        });
        send
    }

    fn call(path: &str) -> http::Request<()> {
        http::Request::builder()
            .method("POST")
            .uri(format!("http://grpc.test{path}"))
            .header("content-type", "application/grpc")
            .header("te", "trailers")
            .body(())
            .unwrap()
    }

    async fn within<F: std::future::Future>(f: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(5), f)
            .await
            .expect("timed out")
    }

    #[test]
    fn grpc_is_told_by_its_content_type_and_grpc_web_is_not_grpc() {
        let of = |ct: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(
                http::header::CONTENT_TYPE,
                HeaderValue::from_str(ct).unwrap(),
            );
            is_grpc(&headers)
        };
        for grpc in [
            "application/grpc",
            "application/grpc+proto",
            "application/grpc;charset=utf-8",
            "Application/gRPC+json",
        ] {
            assert!(of(grpc), "{grpc}");
        }
        for other in [
            "application/grpc-web",
            "application/grpc-web-text+proto",
            "application/grpcx",
            "application/json",
        ] {
            assert!(!of(other), "{other}");
        }
        assert!(!is_grpc(&HeaderMap::new()));
    }

    #[tokio::test]
    async fn a_call_reaches_its_backend_and_its_trailers_come_back() {
        let (backend, mut seen) = echo_backend(false).await;
        let send = client(table(backend)).await;
        let mut send = send.ready().await.unwrap();
        let (response, mut body) = send.send_request(call("/test.Echo/Say"), false).unwrap();
        body.send_data(Bytes::from_static(b"\0\0\0\0\x02hi"), true)
            .unwrap();
        let response = within(response).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["x-added"], "yes");
        let mut answer = response.into_body();
        let mut got = Vec::new();
        while let Some(chunk) = within(answer.data()).await {
            got.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(got, b"\0\0\0\0\x02hi");
        let trailers = within(answer.trailers()).await.unwrap().unwrap();
        assert_eq!(trailers["grpc-status"], "0");
        assert_eq!(trailers["x-trailer"], "seen");

        let parts = within(seen.recv()).await.unwrap();
        assert_eq!(parts.uri.path(), "/test.Echo/Say");
        assert_eq!(parts.uri.authority().unwrap(), "grpc.test");
        assert_eq!(parts.headers["x-set"], "yes");
        assert_eq!(parts.headers["x-forwarded-for"], "10.1.2.3");
        assert_eq!(parts.headers["x-forwarded-proto"], "http");
        assert_eq!(parts.headers["te"], "trailers");
    }

    /// Each message is answered before the next is sent: nothing waits for
    /// the request to end.
    #[tokio::test]
    async fn a_streaming_call_is_answered_as_it_goes() {
        let (backend, _seen) = echo_backend(false).await;
        let send = client(table(backend)).await;
        let mut send = send.ready().await.unwrap();
        let (response, mut body) = send.send_request(call("/test.Echo/Chat"), false).unwrap();
        body.send_data(Bytes::from_static(b"one"), false).unwrap();
        let mut answer = within(response).await.unwrap().into_body();
        assert_eq!(&within(answer.data()).await.unwrap().unwrap()[..], b"one");
        body.send_data(Bytes::from_static(b"two"), true).unwrap();
        assert_eq!(&within(answer.data()).await.unwrap().unwrap()[..], b"two");
        assert!(within(answer.data()).await.is_none());
        let trailers = within(answer.trailers()).await.unwrap().unwrap();
        assert_eq!(trailers["grpc-status"], "0");
    }

    /// A backend under a BackendTLSPolicy, as the controller writes it: the
    /// ref's `tls` and the bundle in `backendCas`. The call goes over TLS,
    /// its certificate checked, h2 by ALPN.
    #[tokio::test]
    async fn a_call_reaches_a_backend_verified_by_its_policy() {
        use crate::proxy::testkit::TestCa;
        let ca = TestCa::new();
        let mut tls = ca.server_config(&["abc.example.com"], &[]);
        tls.alpn_protocols = vec![b"h2".to_vec()];
        let (backend, mut seen) = echo_backend_over(false, Some(Arc::new(tls))).await;
        let config: crate::config::PortailConfig = serde_json::from_value(serde_json::json!({
            "gateway": {"name": "g", "listeners": [{"name": "l", "protocol": "HTTP", "port": 8080}]},
            "grpcRoutes": [{
                "parentRefs": [{"name": "g", "sectionName": "l"}],
                "hostnames": ["grpc.test"],
                "rules": [{"backendRefs": [{
                    "name": backend.ip().to_string(), "port": backend.port(),
                    "tls": {"hostname": "abc.example.com", "ca": {"bundle": "ca"}}
                }]}]
            }],
            "backendCas": {"ca": ca.pem()}
        }))
        .unwrap();
        config.validate().unwrap();
        let send = client(config.to_route_table_strict().unwrap()).await;
        let mut send = send.ready().await.unwrap();
        let (response, _) = send.send_request(call("/test.Echo/Say"), true).unwrap();
        let response = within(response).await.unwrap();
        assert_eq!(
            response.headers().get("grpc-status"),
            None,
            "answered by the proxy"
        );
        let mut answer = response.into_body();
        while within(answer.data()).await.is_some() {}
        let trailers = within(answer.trailers()).await.unwrap().unwrap();
        assert_eq!(trailers["grpc-status"], "0");
        assert_eq!(
            within(seen.recv()).await.unwrap().uri.scheme_str(),
            Some("https")
        );
    }

    /// A TLS backend that accepts and never answers its handshake: the
    /// call is UNAVAILABLE once the connect's budget passes again, the
    /// client not held until it gives up.
    #[tokio::test(start_paused = true)]
    async fn a_backend_silent_in_its_handshake_is_unavailable() {
        let silent = crate::proxy::testkit::idle_backend().await;
        let addr = silent.local_addr().unwrap();
        let config: crate::config::PortailConfig = serde_json::from_value(serde_json::json!({
            "gateway": {"name": "g", "listeners": [{"name": "l", "protocol": "HTTP", "port": 8080}]},
            "grpcRoutes": [{
                "parentRefs": [{"name": "g", "sectionName": "l"}],
                "hostnames": ["grpc.test"],
                "rules": [{"backendRefs": [{
                    "name": addr.ip().to_string(), "port": addr.port(), "app_protocol": "https"
                }]}]
            }]
        }))
        .unwrap();
        config.validate().unwrap();
        let send = client(config.to_route_table_strict().unwrap()).await;
        let mut send = send.ready().await.unwrap();
        let (response, _) = send.send_request(call("/test.Echo/Say"), true).unwrap();
        let response = within(response).await.unwrap();
        assert_eq!(response.headers()["grpc-status"], "14");
        drop(silent);
    }

    /// No rule: UNIMPLEMENTED, trailers only, and no HTTP route asked.
    #[tokio::test]
    async fn a_call_no_rule_takes_is_unimplemented() {
        let (backend, mut seen) = echo_backend(false).await;
        let send = client(table(backend)).await;
        let mut send = send.ready().await.unwrap();
        let (response, _) = send.send_request(call("/other.Svc/Say"), true).unwrap();
        let response = within(response).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["grpc-status"], "12");
        assert!(response.body().is_end_stream());
        assert!(seen.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_backend_reset_reaches_the_client_with_its_reason() {
        let (backend, _seen) = echo_backend(false).await;
        let send = client(table(backend)).await;
        let mut send = send.ready().await.unwrap();
        let (response, _) = send.send_request(call("/test.Echo/Cancel"), true).unwrap();
        let reset = within(response).await.unwrap_err();
        assert_eq!(reset.reason(), Some(h2::Reason::CANCEL));
    }

    /// A backend that closes its connection after each call: the next call
    /// opens a new one rather than failing on the closed one.
    #[tokio::test]
    async fn a_closed_backend_connection_is_opened_again() {
        let (backend, mut seen) = echo_backend(true).await;
        let send = client(table(backend)).await;
        for _ in 0..3 {
            let mut ready = send.clone().ready().await.unwrap();
            let (response, _) = ready.send_request(call("/test.Echo/Say"), true).unwrap();
            let mut answer = within(response).await.unwrap().into_body();
            while within(answer.data()).await.is_some() {}
            let trailers = within(answer.trailers()).await.unwrap().unwrap();
            assert_eq!(trailers["grpc-status"], "0");
            within(seen.recv()).await.unwrap();
        }
    }

    /// A backend that refuses: UNAVAILABLE, and its health hears of it.
    #[tokio::test]
    async fn a_refusing_backend_is_unavailable() {
        let (_socket, refusing) = crate::proxy::testkit::refusing_backend();
        let send = client(table(refusing)).await;
        let mut send = send.ready().await.unwrap();
        let (response, _) = send.send_request(call("/test.Echo/Say"), true).unwrap();
        let response = within(response).await.unwrap();
        assert_eq!(response.headers()["grpc-status"], "14");
    }

    /// A client that stops reading holds its own backend connection only:
    /// another client's call to the same backend goes through. Three
    /// streams stalled at the stream window hold more than a connection's
    /// window, so one connection shared by both would stall the second.
    #[tokio::test]
    async fn a_client_that_stops_reading_stalls_no_other() {
        let (backend, _seen) = echo_backend(false).await;
        let routes = table(backend);
        let stalled = client(routes.clone()).await;
        let mut answers = Vec::new();
        for _ in 0..3 {
            let mut send = stalled.clone().ready().await.unwrap();
            let (response, _body) = send.send_request(call("/test.Echo/Flood"), false).unwrap();
            answers.push((within(response).await.unwrap().into_body(), _body));
        }
        // Nothing is read: the backend is granted the windows of the proxy's
        // connection to it once, and no more.
        let full = crate::proxy::h2_bridge::CONNECTION_WINDOW as usize;
        within(async {
            while FLOODED.load(Ordering::Relaxed) < full {
                tokio::task::yield_now().await;
            }
        })
        .await;

        // Its answer is DATA, which needs window: HEADERS and trailers do not.
        let other = client(routes).await;
        let mut other = other.ready().await.unwrap();
        let (response, mut body) = other.send_request(call("/test.Echo/Say"), false).unwrap();
        body.send_data(Bytes::from_static(b"hi"), true).unwrap();
        let mut answer = within(response).await.unwrap().into_body();
        assert_eq!(&within(answer.data()).await.unwrap().unwrap()[..], b"hi");
        assert!(within(answer.data()).await.is_none());
        let trailers = within(answer.trailers()).await.unwrap().unwrap();
        assert_eq!(trailers["grpc-status"], "0");
    }
}
