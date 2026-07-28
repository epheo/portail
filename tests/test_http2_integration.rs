//! HTTP/2 bridge e2e: ALPN negotiation, the per-stream h1 replay through
//! the engine, response de-framing, and h1 coexistence on the same port.
//!
//! Skips silently when the openssl CLI is unavailable (mirrors the TLS e2e).

mod helpers;

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use helpers::{extract_status, generate_test_cert, PortailProcess, TestBackend};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Own range, clear of proxy (19000+), TLS (19700+), UDP (19800+).
fn h2_port(offset: u16) -> u16 {
    19100 + offset
}

const SNI: &str = "h2.example.test";

/// Accepts any server cert (self-signed test certs).
#[derive(Debug)]
struct AcceptAny;

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ED25519,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PKCS1_SHA256,
        ]
    }
}

async fn tls_connect(
    port: u16,
    alpn: &[&[u8]],
) -> tokio_rustls::client::TlsStream<tokio::net::TcpStream> {
    // Dev builds compile in both ring and aws-lc-rs; pick one explicitly.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let mut config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny))
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("tcp connect");
    connector
        .connect(ServerName::try_from(SNI.to_string()).unwrap(), tcp)
        .await
        .expect("tls connect")
}

fn negotiated_alpn(
    tls: &tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
) -> Option<Vec<u8>> {
    tls.get_ref().1.alpn_protocol().map(|p| p.to_vec())
}

/// Handshake an h2 client on an established TLS stream; the connection
/// driver runs in a background task for the rest of the test.
async fn h2_handshake(
    tls: tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
) -> h2::client::SendRequest<Bytes> {
    let (send_request, connection) = h2::client::handshake(tls).await.expect("h2 handshake");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    send_request
}

async fn read_response(resp: http::Response<h2::RecvStream>) -> (u16, Vec<u8>) {
    let status = resp.status().as_u16();
    let mut body = resp.into_body();
    let mut out = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.expect("h2 body chunk");
        out.extend_from_slice(&chunk);
        let _ = body.flow_control().release_capacity(chunk.len());
    }
    (status, out)
}

async fn h2_get(sr: &h2::client::SendRequest<Bytes>, uri: &str) -> (u16, Vec<u8>) {
    let mut sr = sr.clone().ready().await.expect("h2 ready");
    let req = http::Request::builder()
        .method("GET")
        .uri(uri)
        .body(())
        .unwrap();
    let (resp, _) = sr.send_request(req, true).expect("send_request");
    read_response(resp.await.expect("h2 response")).await
}

/// Backend that answers every request with a chunked-encoded body, to prove
/// the bridge de-frames chunked h1 responses into clean DATA frames.
fn spawn_chunked_backend(parts: &[&str]) -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind chunked backend");
    let addr = listener.local_addr().unwrap();
    let parts: Vec<String> = parts.iter().map(|s| s.to_string()).collect();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let parts = parts.clone();
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let mut resp: Vec<u8> = Vec::new();
                resp.extend_from_slice(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
                for p in &parts {
                    resp.extend_from_slice(format!("{:x}\r\n", p.len()).as_bytes());
                    resp.extend_from_slice(p.as_bytes());
                    resp.extend_from_slice(b"\r\n");
                }
                resp.extend_from_slice(b"0\r\n\r\n");
                let _ = s.write_all(&resp);
            });
        }
    });
    addr
}

/// Backend that reads the whole request (until the chunked terminator) and
/// echoes the raw received body back as the response, so a test can prove the
/// bridge forwarded the complete request body and did not truncate it.
fn spawn_request_echo_backend() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind echo backend");
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                s.set_read_timeout(Some(std::time::Duration::from_millis(300)))
                    .ok();
                let mut req: Vec<u8> = Vec::new();
                let mut buf = [0u8; 4096];
                // Read until the chunked terminator is seen or the peer stalls.
                loop {
                    match s.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            req.extend_from_slice(&buf[..n]);
                            if req.windows(5).any(|w| w == b"0\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let body_start = req
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map(|p| p + 4)
                    .unwrap_or(req.len());
                let body = &req[body_start..];
                let mut resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len())
                    .into_bytes();
                resp.extend_from_slice(body);
                let _ = s.write_all(&resp);
            });
        }
    });
    addr
}

/// Backend that sends a chunked response ALSO carrying a (bogus) Content-Length
/// header, to prove the bridge strips content-length when it de-frames chunked
/// so the h2 client does not see a content-length that contradicts the DATA.
fn spawn_chunked_backend_with_bogus_cl(parts: &[&str]) -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind backend");
    let addr = listener.local_addr().unwrap();
    let parts: Vec<String> = parts.iter().map(|s| s.to_string()).collect();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let parts = parts.clone();
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let mut resp: Vec<u8> =
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 999\r\n\r\n"
                        .to_vec();
                for p in &parts {
                    resp.extend_from_slice(format!("{:x}\r\n", p.len()).as_bytes());
                    resp.extend_from_slice(p.as_bytes());
                    resp.extend_from_slice(b"\r\n");
                }
                resp.extend_from_slice(b"0\r\n\r\n");
                let _ = s.write_all(&resp);
            });
        }
    });
    addr
}

/// Cert + portail with `--http2`, one TestBackend route. `None` = no openssl.
fn spawn_h2_proxy(port: u16, backend_addr: SocketAddr) -> Option<PortailProcess> {
    let (cert, key) = generate_test_cert(SNI)?;
    Some(PortailProcess::spawn_with_tls_args(
        &[("h2test", SNI, &cert, &key)],
        &[(SNI, backend_addr)],
        port,
        &["--http2"],
    ))
}

#[tokio::test]
async fn test_h2_get_roundtrip_and_alpn() {
    let backend = TestBackend::spawn("hello-h2");
    let Some(_proxy) = spawn_h2_proxy(h2_port(1), backend.addr) else {
        eprintln!("openssl unavailable; skipping h2 e2e");
        return;
    };

    let tls = tls_connect(h2_port(1), &[b"h2", b"http/1.1"]).await;
    assert_eq!(negotiated_alpn(&tls).as_deref(), Some(b"h2".as_ref()));

    let sr = h2_handshake(tls).await;
    // Two sequential streams on one connection: each is its own engine replay.
    for _ in 0..2 {
        let (status, body) = h2_get(&sr, &format!("https://{}/", SNI)).await;
        assert_eq!(status, 200);
        assert_eq!(body, b"hello-h2");
    }
}

#[tokio::test]
async fn test_h2_post_content_length_body() {
    let backend = TestBackend::spawn("posted");
    let Some(_proxy) = spawn_h2_proxy(h2_port(2), backend.addr) else {
        return;
    };

    // Declared content-length: relayed as-is.
    let sr = h2_handshake(tls_connect(h2_port(2), &[b"h2"]).await).await;
    let mut s = sr.clone().ready().await.unwrap();
    let req = http::Request::builder()
        .method("POST")
        .uri(format!("https://{}/upload", SNI))
        .header("content-length", "5")
        .body(())
        .unwrap();
    let (resp, mut body) = s.send_request(req, false).unwrap();
    body.send_data(Bytes::from_static(b"hello"), true).unwrap();
    let (status, _) = read_response(resp.await.expect("h2 response")).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn test_h2_post_chunked_body() {
    let backend = TestBackend::spawn("posted");
    let Some(_proxy) = spawn_h2_proxy(h2_port(8), backend.addr) else {
        return;
    };

    // No content-length: the bridge re-frames DATA as a chunked h1 body.
    let sr = h2_handshake(tls_connect(h2_port(8), &[b"h2"]).await).await;
    let mut s = sr.clone().ready().await.unwrap();
    let req = http::Request::builder()
        .method("POST")
        .uri(format!("https://{}/upload", SNI))
        .body(())
        .unwrap();
    let (resp, mut body) = s.send_request(req, false).unwrap();
    body.send_data(Bytes::from_static(b"chunk-a"), false)
        .unwrap();
    body.send_data(Bytes::from_static(b"chunk-b"), true)
        .unwrap();
    let (status, _) = read_response(resp.await.expect("h2 response")).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn test_h2_sequential_posts_one_connection() {
    let backend = TestBackend::spawn("posted");
    let Some(_proxy) = spawn_h2_proxy(h2_port(9), backend.addr) else {
        return;
    };

    let sr = h2_handshake(tls_connect(h2_port(9), &[b"h2"]).await).await;
    for i in 0..2 {
        let mut s = sr.clone().ready().await.unwrap();
        let req = http::Request::builder()
            .method("POST")
            .uri(format!("https://{}/upload", SNI))
            .header("content-length", "5")
            .body(())
            .unwrap();
        let (resp, mut body) = s.send_request(req, false).unwrap();
        body.send_data(Bytes::from_static(b"hello"), true).unwrap();
        let (status, _) = read_response(resp.await.expect("h2 response")).await;
        assert_eq!(status, 200, "request {} on the shared connection", i);
    }
}

#[tokio::test]
async fn test_h2_mixed_body_framings_one_connection() {
    let backend = TestBackend::spawn("posted");
    let Some(_proxy) = spawn_h2_proxy(h2_port(10), backend.addr) else {
        return;
    };

    let sr = h2_handshake(tls_connect(h2_port(10), &[b"h2"]).await).await;

    // Content-length stream, then a chunk-reframed stream, same connection.
    let mut s = sr.clone().ready().await.unwrap();
    let req = http::Request::builder()
        .method("POST")
        .uri(format!("https://{}/upload", SNI))
        .header("content-length", "5")
        .body(())
        .unwrap();
    let (resp, mut body) = s.send_request(req, false).unwrap();
    body.send_data(Bytes::from_static(b"hello"), true).unwrap();
    let (status, _) = read_response(resp.await.expect("h2 response")).await;
    assert_eq!(status, 200);

    let mut s = sr.clone().ready().await.unwrap();
    let req = http::Request::builder()
        .method("POST")
        .uri(format!("https://{}/upload", SNI))
        .body(())
        .unwrap();
    let (resp, mut body) = s.send_request(req, false).unwrap();
    body.send_data(Bytes::from_static(b"chunk-a"), false)
        .unwrap();
    body.send_data(Bytes::from_static(b"chunk-b"), true)
        .unwrap();
    let (status, _) = read_response(resp.await.expect("h2 response")).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn test_h2_empty_interior_data_frame_does_not_truncate() {
    // Regression: a zero-length non-final DATA frame must not be re-framed as
    // the chunked terminator. The body has no content-length, so the bridge
    // chunk-frames it; an empty frame between two data frames must not cut it.
    let backend = spawn_request_echo_backend();
    let Some(_proxy) = spawn_h2_proxy(h2_port(11), backend) else {
        return;
    };

    let sr = h2_handshake(tls_connect(h2_port(11), &[b"h2"]).await).await;
    let mut s = sr.clone().ready().await.unwrap();
    let req = http::Request::builder()
        .method("POST")
        .uri(format!("https://{}/upload", SNI))
        .body(())
        .unwrap();
    let (resp, mut body) = s.send_request(req, false).unwrap();
    body.send_data(Bytes::from_static(b"hello"), false).unwrap();
    body.send_data(Bytes::new(), false).unwrap(); // empty interior frame
    body.send_data(Bytes::from_static(b"world"), true).unwrap();

    let (status, echoed) = read_response(resp.await.expect("h2 response")).await;
    assert_eq!(status, 200);
    // The backend echoed the raw chunked body it received; both payloads must
    // be present, i.e. the empty frame did not terminate the body early.
    assert!(
        echoed.windows(5).any(|w| w == b"hello") && echoed.windows(5).any(|w| w == b"world"),
        "request body truncated at empty frame: {:?}",
        String::from_utf8_lossy(&echoed)
    );
}

#[tokio::test]
async fn test_h2_chunked_response_with_content_length_is_clean() {
    // Regression: a chunked response also carrying Content-Length must reach
    // the h2 client cleanly (bridge strips content-length when de-framing);
    // otherwise the h2 crate rejects the CL/DATA mismatch and read errors.
    let backend = spawn_chunked_backend_with_bogus_cl(&["aa", "bb", "cc"]);
    let Some(_proxy) = spawn_h2_proxy(h2_port(12), backend) else {
        return;
    };

    let sr = h2_handshake(tls_connect(h2_port(12), &[b"h2"]).await).await;
    let (status, body) = h2_get(&sr, &format!("https://{}/", SNI)).await;
    assert_eq!(status, 200);
    assert_eq!(body, b"aabbcc");
}

#[tokio::test]
async fn test_h2_concurrent_streams() {
    let backend = TestBackend::spawn("concurrent");
    let Some(_proxy) = spawn_h2_proxy(h2_port(3), backend.addr) else {
        return;
    };

    let sr = h2_handshake(tls_connect(h2_port(3), &[b"h2"]).await).await;
    let uri = format!("https://{}/", SNI);
    let (a, b) = tokio::join!(h2_get(&sr, &uri), h2_get(&sr, &uri));
    assert_eq!((a.0, b.0), (200, 200));
    assert_eq!(a.1, b"concurrent");
    assert_eq!(b.1, b"concurrent");
}

#[tokio::test]
async fn test_h2_chunked_response_dechunked() {
    let backend_addr = spawn_chunked_backend(&["alpha-", "beta-", "gamma"]);
    let Some(_proxy) = spawn_h2_proxy(h2_port(4), backend_addr) else {
        return;
    };

    let sr = h2_handshake(tls_connect(h2_port(4), &[b"h2"]).await).await;
    let (status, body) = h2_get(&sr, &format!("https://{}/", SNI)).await;
    assert_eq!(status, 200);
    // Payload only - chunk sizes and CRLFs must not leak into DATA frames.
    assert_eq!(body, b"alpha-beta-gamma");
}

#[tokio::test]
async fn test_h2_unknown_host_gets_engine_error() {
    let backend = TestBackend::spawn("routed");
    let Some(_proxy) = spawn_h2_proxy(h2_port(5), backend.addr) else {
        return;
    };

    let sr = h2_handshake(tls_connect(h2_port(5), &[b"h2"]).await).await;
    let (status, _) = h2_get(&sr, "https://unrouted.test/").await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn test_h1_coexists_on_h2_port() {
    let backend = TestBackend::spawn("h1-still-works");
    let Some(_proxy) = spawn_h2_proxy(h2_port(6), backend.addr) else {
        return;
    };

    // A client that only speaks h1 negotiates http/1.1 on the same port.
    let mut tls = tls_connect(h2_port(6), &[b"http/1.1"]).await;
    assert_eq!(negotiated_alpn(&tls).as_deref(), Some(b"http/1.1".as_ref()));
    tls.write_all(
        format!(
            "GET / HTTP/1.1\r\nhost: {}\r\nconnection: close\r\n\r\n",
            SNI
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut resp = Vec::new();
    // Close without close_notify after a full response is fine here.
    let _ = tls.read_to_end(&mut resp).await;
    assert_eq!(extract_status(&resp), Some(200));
    assert!(resp.ends_with(b"h1-still-works"));
}

#[tokio::test]
async fn test_h2_not_offered_without_flag() {
    let backend = TestBackend::spawn("no-h2");
    let (cert, key) = match generate_test_cert(SNI) {
        Some(v) => v,
        None => return,
    };
    // No --http2: the server must not advertise h2 even to willing clients.
    let _proxy = PortailProcess::spawn_with_tls(
        &[("h2test", SNI, &cert, &key)],
        &[(SNI, backend.addr)],
        h2_port(7),
    );

    let tls = tls_connect(h2_port(7), &[b"h2", b"http/1.1"]).await;
    assert_eq!(negotiated_alpn(&tls).as_deref(), Some(b"http/1.1".as_ref()));
}
