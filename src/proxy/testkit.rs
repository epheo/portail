//! Helpers the proxy's tests share: backends, route tables, and one
//! connection served in memory.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use arc_swap::ArcSwap;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

use crate::metrics::ListenerStats;
use crate::proxy::health::HealthRegistry;
use crate::proxy::http1::{self, ConnectionState};
use crate::proxy::http_parser::find_header_end;
use crate::proxy::listener::{make_pool, ServeConfig};
use crate::proxy::proxy_protocol;
use crate::proxy::tls::Connection;
use crate::routing::{
    Backend, BackendSelector, HttpFilter, HttpRouteRule, PathMatchType, RouteTable,
};

pub(crate) const REFUSED: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nno";

pub(crate) const UPGRADE: &[u8] =
    b"GET /ws HTTP/1.1\r\nHost: a.test\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n";

/// Backend answering the first request head of each connection with
/// `reply`; yields all bytes a connection delivered once it closes.
pub(crate) async fn backend(reply: &[u8]) -> (SocketAddr, UnboundedReceiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let reply = reply.to_vec();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let (tx, reply) = (tx.clone(), reply.clone());
            tokio::spawn(async move {
                let (mut seen, mut chunk, mut replied) = (Vec::new(), [0u8; 4096], false);
                while let Ok(n @ 1..) = sock.read(&mut chunk).await {
                    seen.extend_from_slice(&chunk[..n]);
                    if !replied && find_header_end(&seen).is_some() {
                        replied = true;
                        if sock.write_all(&reply).await.is_err() {
                            break;
                        }
                    }
                }
                let _ = tx.send(seen);
            });
        }
    });
    (addr, rx)
}

/// Backend answering the first request head of each connection with
/// `reply`, then closing: the end of a close-delimited body.
pub(crate) async fn closing_backend(reply: &[u8]) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let reply = reply.to_vec();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let reply = reply.clone();
            tokio::spawn(async move {
                let (mut seen, mut chunk) = (Vec::new(), [0u8; 4096]);
                while let Ok(n @ 1..) = sock.read(&mut chunk).await {
                    seen.extend_from_slice(&chunk[..n]);
                    if find_header_end(&seen).is_some() {
                        let _ = sock.write_all(&reply).await;
                        return;
                    }
                }
            });
        }
    });
    addr
}

/// a.test proxies to `backend`; r.test redirects to https.
pub(crate) fn routes(backend: SocketAddr) -> RouteTable {
    let rule = |filters, backends| {
        HttpRouteRule::new(
            PathMatchType::Prefix,
            "/".to_string(),
            vec![],
            vec![],
            filters,
            backends,
        )
    };
    let mut rt = RouteTable::new();
    rt.add_http_route(
        "a.test",
        rule(
            vec![],
            vec![Backend {
                socket_addr: backend,
                weight: 1,
                filters: vec![],
                use_tls: false,
                server_name: String::new(),
                action: crate::routing::BackendAction::Forward,
            }],
        ),
    );
    rt.add_http_route(
        "r.test",
        rule(
            vec![HttpFilter::RequestRedirect {
                scheme: Some("https".to_string()),
                hostname: None,
                port: None,
                path: None,
                status_code: 301,
            }],
            vec![],
        ),
    );
    rt
}

pub(crate) fn conn_state(rt: RouteTable, is_tls: bool) -> ConnectionState {
    let cfg = ServeConfig {
        max_idle_per_backend: 4,
        connect_timeout: Duration::from_secs(2),
        client_header_timeout: Duration::from_secs(2),
        tcp_keepalive_time: Duration::from_secs(60),
        idle_body_timeout: Duration::from_secs(5),
        http2: false,
    };
    // As the listener decides it.
    let tcp_route = !is_tls && rt.has_tcp_route(8080);
    ConnectionState::new(
        8080,
        Arc::new(ArcSwap::from_pointee(rt)),
        make_pool(&None, &cfg),
        Arc::new(BackendSelector::new()),
        Arc::new(HealthRegistry::new()),
        is_tls,
        tcp_route,
        "127.0.0.1:40000".parse().unwrap(),
        "127.0.0.1:8080".parse().unwrap(),
        &cfg,
        Arc::new(ListenerStats::default()),
        CancellationToken::new(),
    )
}

pub(crate) fn serve(rt: RouteTable) -> (DuplexStream, tokio::task::JoinHandle<Result<()>>) {
    serve_state(conn_state(rt, false))
}

/// `serve` for a state the test adjusted first.
pub(crate) fn serve_state(
    state: ConnectionState,
) -> (DuplexStream, tokio::task::JoinHandle<Result<()>>) {
    let (client, engine) = tokio::io::duplex(64 * 1024);
    let peer = "127.0.0.1:40000".parse().unwrap();
    let task = tokio::spawn(http1::serve(
        Connection::Mem {
            inner: engine,
            aborted: Default::default(),
        },
        peer,
        state,
    ));
    (client, task)
}

/// Send `input`, half-close, and collect everything until the engine
/// closes the connection.
pub(crate) async fn exchange(client: &mut DuplexStream, input: &[u8]) -> String {
    client.write_all(input).await.unwrap();
    client.shutdown().await.unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut out))
        .await
        .expect("engine never closed the connection")
        .unwrap();
    String::from_utf8_lossy(&out).into_owned()
}

/// Everything until the engine closes, without half-closing first: the
/// close must be the engine's own decision.
pub(crate) async fn read_until_closed(client: &mut DuplexStream) -> String {
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut out))
        .await
        .expect("engine never closed the connection")
        .unwrap();
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) async fn delivered(rx: &mut UnboundedReceiver<Vec<u8>>) -> String {
    let seen = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("backend connection never closed")
        .unwrap();
    String::from_utf8_lossy(&seen).into_owned()
}

/// A client's first flight: a ClientHello naming `sni`.
pub(crate) fn client_hello(sni: &str) -> Vec<u8> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(crate::proxy::backend_pool::NoVerifier))
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(sni.to_string()).unwrap();
    let mut conn = rustls::ClientConnection::new(Arc::new(config), name).unwrap();
    let mut hello = Vec::new();
    conn.write_tls(&mut hello).unwrap();
    hello
}

/// A backend nothing should reach: the test holds the listener.
pub(crate) async fn idle_backend() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").await.unwrap()
}

/// A backend address that refuses connections. The socket is bound but
/// not listening; held, it keeps a parallel test from taking the port.
pub(crate) fn refusing_backend() -> (tokio::net::TcpSocket, SocketAddr) {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = socket.local_addr().unwrap();
    (socket, addr)
}

/// Call once the engine task has finished: any connect it made is
/// already in the accept queue.
pub(crate) async fn assert_never_connected(listener: &TcpListener) {
    let accepted = tokio::time::timeout(Duration::from_millis(100), listener.accept()).await;
    assert!(accepted.is_err(), "the proxy connected to the backend");
}

/// Not HTTP, but holding the blank line that ends a head.
pub(crate) const OPAQUE: &[u8] = b"\x00\x01opaque\r\n\r\n";

/// `routes(http)` plus a TCP route on the engine's port.
pub(crate) fn routes_with_tcp(http: SocketAddr, tcp: SocketAddr) -> RouteTable {
    let mut rt = routes(http);
    rt.add_tcp_route(
        8080,
        vec![Backend {
            socket_addr: tcp,
            weight: 1,
            filters: vec![],
            use_tls: false,
            server_name: String::new(),
            action: crate::routing::BackendAction::Forward,
        }],
    );
    rt
}

/// `keepalive_of` the socket in this process that `is_it` picks.
pub(crate) fn keepalive_where(
    is_it: impl Fn(&socket2::SockRef<'_>) -> bool,
) -> Option<(Duration, Duration, u32)> {
    socket_where(is_it, |sock| {
        sock.keepalive().unwrap().then(|| {
            (
                sock.tcp_keepalive_time().unwrap(),
                sock.tcp_keepalive_interval().unwrap(),
                sock.tcp_keepalive_retries().unwrap(),
            )
        })
    })
}

/// What `read` says of the socket in this process that `is_it` picks.
pub(crate) fn socket_where<T>(
    is_it: impl Fn(&socket2::SockRef<'_>) -> bool,
    read: impl Fn(&socket2::SockRef<'_>) -> T,
) -> T {
    use std::os::fd::FromRawFd;
    for entry in std::fs::read_dir("/proc/self/fd").unwrap() {
        let Ok(fd) = entry.unwrap().file_name().to_string_lossy().parse() else {
            continue;
        };
        // A dup, so a test closing `fd` meanwhile cannot pull it away.
        let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if dup < 0 {
            continue;
        }
        let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(dup) };
        let sock = socket2::SockRef::from(&owned);
        if !is_it(&sock) {
            continue;
        }
        return read(&sock);
    }
    panic!("no such socket");
}

pub(crate) fn gateway_backend(socket_addr: SocketAddr) -> Backend {
    Backend {
        socket_addr,
        weight: 1,
        filters: vec![],
        use_tls: false,
        server_name: String::new(),
        action: crate::routing::BackendAction::ForwardProxied,
    }
}

/// What `conn_state`'s client sends a Gateway backend first.
pub(crate) fn conn_state_header() -> Vec<u8> {
    proxy_protocol::header(
        "127.0.0.1:40000".parse().unwrap(),
        "127.0.0.1:8080".parse().unwrap(),
    )
}

pub(crate) async fn received(rx: &mut UnboundedReceiver<Vec<u8>>) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("backend connection never closed")
        .unwrap()
}

pub(crate) const OK: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";

/// The next response on a kept-open client connection, by its
/// Content-Length; whatever arrived when the engine closes instead.
pub(crate) async fn response(client: &mut DuplexStream) -> String {
    let (mut out, mut chunk) = (Vec::new(), [0u8; 4096]);
    loop {
        if let Some(end) = find_header_end(&out) {
            let head = String::from_utf8_lossy(&out[..end]).to_ascii_lowercase();
            let len: usize = head
                .split("\r\ncontent-length: ")
                .nth(1)
                .and_then(|v| v.split("\r\n").next()?.parse().ok())
                .unwrap_or(0);
            if out.len() >= end + len {
                let resp = String::from_utf8_lossy(&out[..end + len]).into_owned();
                out.drain(..end + len);
                assert!(out.is_empty(), "unexpected bytes after {resp}");
                return resp;
            }
        }
        let read = tokio::time::timeout(Duration::from_secs(5), client.read(&mut chunk));
        match read.await.expect("no response").unwrap() {
            0 => return String::from_utf8_lossy(&out).into_owned(),
            n => out.extend_from_slice(&chunk[..n]),
        }
    }
}

pub(crate) const GET_A: &[u8] = b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n";

/// A TCP backend running `serve` on each connection.
pub(crate) async fn l4_backend<F, Fut>(serve: F) -> SocketAddr
where
    F: Fn(TcpStream) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            tokio::spawn(serve(sock));
        }
    });
    addr
}

pub(crate) const IDLE: Duration = Duration::from_millis(200);

pub(crate) type TlsClient = tokio_rustls::client::TlsStream<TcpStream>;

/// A TLS client connection to the engine serving `rt`, over a real
/// socket. `None` when openssl is unavailable.
pub(crate) async fn tls_client(
    rt: RouteTable,
) -> Option<(TlsClient, tokio::task::JoinHandle<Result<()>>)> {
    let (cert_pem, key_pem) = crate::proxy::tls::test_util::generate_test_cert("a.test")?;
    let cert = crate::config::CertificateRef {
        name: "a".to_string(),
        cert_pem: Some(cert_pem),
        key_pem: Some(key_pem),
        ..Default::default()
    };
    let server_cfg =
        crate::proxy::tls::build_server_config(&[cert], std::path::Path::new("/unused"), false)
            .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_cfg));
    let front = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let front_addr = front.local_addr().unwrap();
    let engine = tokio::spawn(async move {
        let (tcp, peer) = front.accept().await.unwrap();
        let tls = acceptor.accept(tcp).await.unwrap();
        let state = conn_state(rt, true);
        let tls = Connection::Tls {
            inner: Box::new(tls),
        };
        http1::serve(tls, peer, state).await
    });

    let client_cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(crate::proxy::backend_pool::NoVerifier))
        .with_no_client_auth();
    let tcp = TcpStream::connect(front_addr).await.unwrap();
    let sni = rustls::pki_types::ServerName::try_from("a.test").unwrap();
    let tls = tokio_rustls::TlsConnector::from(Arc::new(client_cfg))
        .connect(sni, tcp)
        .await
        .unwrap();
    Some((tls, engine))
}

/// Everything until the engine closes. The read fails when the close
/// came without close_notify: a strict client (OpenSSL 3, rustls) reads
/// a bare FIN as a truncation attack.
pub(crate) async fn tls_read_to_end(tls: &mut TlsClient) -> (std::io::Result<usize>, String) {
    let mut out = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), tls.read_to_end(&mut out))
        .await
        .expect("engine never closed the connection");
    (read, String::from_utf8_lossy(&out).into_owned())
}

/// A plain TCP client connection to the engine serving with `state`:
/// the kernel resets only real sockets.
pub(crate) async fn tcp_client(
    state: ConnectionState,
) -> (TcpStream, tokio::task::JoinHandle<Result<()>>) {
    let front = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let front_addr = front.local_addr().unwrap();
    let engine = tokio::spawn(async move {
        let (tcp, peer) = front.accept().await.unwrap();
        http1::serve(Connection::Plain { inner: tcp }, peer, state).await
    });
    (TcpStream::connect(front_addr).await.unwrap(), engine)
}

/// One response off `client`, by its Content-Length.
pub(crate) async fn read_one_reply<R: tokio::io::AsyncRead + Unpin>(client: &mut R) -> String {
    let (mut out, mut chunk) = (Vec::new(), [0u8; 4096]);
    loop {
        if let Some(end) = find_header_end(&out) {
            let head = String::from_utf8_lossy(&out[..end]).to_ascii_lowercase();
            let len = head
                .split("content-length: ")
                .nth(1)
                .and_then(|v| v.split("\r\n").next()?.parse::<usize>().ok())
                .unwrap();
            if out.len() >= end + len {
                return String::from_utf8_lossy(&out).into_owned();
            }
        }
        let n = client.read(&mut chunk).await.unwrap();
        assert!(n > 0, "closed mid reply: {}", String::from_utf8_lossy(&out));
        out.extend_from_slice(&chunk[..n]);
    }
}

/// Only a TCP route on the served port.
pub(crate) fn tcp_route(backend: SocketAddr) -> RouteTable {
    let mut rt = RouteTable::new();
    rt.add_tcp_route(
        8080,
        vec![Backend {
            socket_addr: backend,
            weight: 1,
            filters: vec![],
            use_tls: false,
            server_name: String::new(),
            action: crate::routing::BackendAction::Forward,
        }],
    );
    rt
}

/// Sends `banner` on accept, as MySQL or SMTP do, then echoes until EOF.
pub(crate) async fn banner_backend(banner: &'static [u8]) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut rd, mut wr) = sock.split();
                if wr.write_all(banner).await.is_ok() {
                    let _ = tokio::io::copy(&mut rd, &mut wr).await;
                }
            });
        }
    });
    addr
}
