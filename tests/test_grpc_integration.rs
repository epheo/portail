//! gRPC e2e through the binary: h2c with prior knowledge on a plain port
//! with gRPC routes, ALPN h2 on a TLS one without `--http2`, and neither on
//! ports without gRPC routes.
//!
//! Skips silently when the openssl CLI is unavailable (mirrors the TLS e2e).

mod helpers;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use helpers::{generate_test_cert, AcceptAny, PortailProcess, TestBackend};
use http::{HeaderMap, HeaderValue};
use tokio::net::{TcpListener, TcpStream};

/// Own range, clear of proxy (19000+), h2 (19100+), TLS (19700+), UDP
/// (19800+).
fn grpc_port(offset: u16) -> u16 {
    19400 + offset
}

const HOST: &str = "grpc.example.test";
/// A hostname with only an HTTP route, on every listener.
const WWW: &str = "www.example.test";

/// An h2c gRPC backend: each message echoed, then grpc-status 0.
async fn echo_backend() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (tcp, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut conn = h2::server::handshake(tcp).await.unwrap();
                while let Some(Ok((req, mut respond))) = conn.accept().await {
                    tokio::spawn(async move {
                        let mut body = req.into_body();
                        let head = http::Response::builder()
                            .header("content-type", "application/grpc")
                            .body(())
                            .unwrap();
                        let mut out = respond.send_response(head, false).unwrap();
                        while let Some(Ok(chunk)) = body.data().await {
                            let _ = body.flow_control().release_capacity(chunk.len());
                            out.send_data(chunk, false).unwrap();
                        }
                        let mut trailers = HeaderMap::new();
                        trailers.insert("grpc-status", HeaderValue::from_static("0"));
                        out.send_trailers(trailers).unwrap();
                    });
                }
            });
        }
    });
    addr
}

/// Four listeners: plain and TLS with gRPC routes, plain and TLS with only
/// an HTTP route. `HOST` has both routes, `WWW` only the HTTP one.
fn config(base: u16, grpc: SocketAddr, http: SocketAddr) -> String {
    let listener = |name: &str, protocol: &str, port: u16| {
        let tls = if protocol == "HTTPS" {
            r#", "tls": {"mode": "Terminate", "certificateRefs": [{"name": "grpc", "hostname": "grpc.example.test"}, {"name": "www", "hostname": "www.example.test"}]}"#
        } else {
            ""
        };
        format!(r#"{{"name": "{name}", "protocol": "{protocol}", "port": {port}{tls}}}"#)
    };
    let parents = |names: &[&str]| {
        names
            .iter()
            .map(|n| format!(r#"{{"name": "gw", "sectionName": "{n}"}}"#))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        r#"{{
  "gateway": {{"name": "gw", "listeners": [{}, {}, {}, {}]}},
  "grpcRoutes": [{{
    "parentRefs": [{}],
    "hostnames": ["{HOST}"],
    "rules": [{{"backendRefs": [{{"name": "{}", "port": {}}}]}}]
  }}],
  "httpRoutes": [{{
    "parentRefs": [{}],
    "hostnames": ["{HOST}", "{WWW}"],
    "rules": [{{"backendRefs": [{{"name": "{}", "port": {}}}]}}]
  }}],
  "observability": {{"logging": {{"level": "info", "format": "pretty", "output": "stderr"}}}},
  "performance": {{}}
}}"#,
        listener("grpc-plain", "HTTP", base),
        listener("grpc-tls", "HTTPS", base + 1),
        listener("http-plain", "HTTP", base + 2),
        listener("http-tls", "HTTPS", base + 3),
        parents(&["grpc-plain", "grpc-tls"]),
        grpc.ip(),
        grpc.port(),
        parents(&["grpc-plain", "grpc-tls", "http-plain", "http-tls"]),
        http.ip(),
        http.port(),
    )
}

fn spawn(base: u16, grpc: SocketAddr, http: SocketAddr) -> Option<PortailProcess> {
    let (cert, key) = generate_test_cert(HOST)?;
    let (www_cert, www_key) = generate_test_cert(WWW)?;
    let proxy = PortailProcess::spawn_with_certs(
        config(base, grpc, http),
        &[
            ("grpc", HOST, &cert, &key),
            ("www", WWW, &www_cert, &www_key),
        ],
        base,
        &[],
    );
    for port in base + 1..base + 4 {
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        assert!(helpers::wait_for_listener(addr, Duration::from_secs(5)));
    }
    Some(proxy)
}

async fn tls_connect(port: u16, sni: &'static str) -> tokio_rustls::client::TlsStream<TcpStream> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(rustls::pki_types::ServerName::try_from(sni).unwrap(), tcp)
        .await
        .unwrap()
}

/// One unary call: its answer's message and grpc-status.
async fn call<S>(io: S) -> Result<(Vec<u8>, String), h2::Error>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (send, conn) = h2::client::handshake(io).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let request = http::Request::builder()
        .method("POST")
        .uri(format!("http://{HOST}/test.Echo/Say"))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(())
        .unwrap();
    let mut send = send.ready().await?;
    let (response, mut body) = send.send_request(request, false)?;
    body.send_data(Bytes::from_static(b"\0\0\0\0\x02hi"), true)?;
    let mut answer = response.await?.into_body();
    let mut got = Vec::new();
    while let Some(chunk) = answer.data().await {
        got.extend_from_slice(&chunk?);
    }
    let trailers = answer.trailers().await?.unwrap_or_default();
    let status = trailers
        .get("grpc-status")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    Ok((got, status))
}

async fn within<F: std::future::Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .expect("timed out")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_grpc_port_serves_h2c_and_alpn_h2_without_the_http2_opt_in() {
    let grpc = echo_backend().await;
    let http = TestBackend::spawn("http");
    let base = grpc_port(0);
    let Some(_proxy) = spawn(base, grpc, http.addr) else {
        return;
    };

    let plain = TcpStream::connect(("127.0.0.1", base)).await.unwrap();
    let (got, status) = within(call(plain)).await.expect("h2c call");
    assert_eq!(
        (got.as_slice(), status.as_str()),
        (&b"\0\0\0\0\x02hi"[..], "0")
    );

    let tls = tls_connect(base + 1, HOST).await;
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    let (got, status) = within(call(tls)).await.expect("h2 call over TLS");
    assert_eq!(
        (got.as_slice(), status.as_str()),
        (&b"\0\0\0\0\x02hi"[..], "0")
    );
}

/// The opt-in stays what it was where no gRPC route is: no h2 offered by
/// ALPN, and a client's h2c preface is not taken.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_port_without_grpc_routes_speaks_no_h2() {
    let grpc = echo_backend().await;
    let http = TestBackend::spawn("http");
    let base = grpc_port(10);
    let Some(_proxy) = spawn(base, grpc, http.addr) else {
        return;
    };

    let plain = TcpStream::connect(("127.0.0.1", base + 2)).await.unwrap();
    assert!(within(call(plain)).await.is_err(), "h2c taken");

    let tls = tls_connect(base + 3, HOST).await;
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
}

/// On a TLS port with gRPC routes, h2 is offered only to a client naming a
/// hostname with some: a browser naming another stays on HTTP/1.1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h2_is_offered_by_the_hostname_a_client_names() {
    let grpc = echo_backend().await;
    let http = TestBackend::spawn("http");
    let base = grpc_port(20);
    let Some(_proxy) = spawn(base, grpc, http.addr) else {
        return;
    };
    let tls = tls_connect(base + 1, WWW).await;
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
    let tls = tls_connect(base + 1, HOST).await;
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
}
