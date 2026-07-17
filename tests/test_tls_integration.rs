//! TLS termination e2e: SNI certificate selection across multiple certs on
//! one port — the surface behind the v0.1.13 shared-port cert-clobber
//! regression, previously covered only by unit tests.
//!
//! Skips silently when the openssl CLI is unavailable (mirrors the lib's
//! cert-dependent unit tests).

mod helpers;

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use helpers::{extract_body, extract_status, generate_test_cert, PortailProcess, TestBackend};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme};

/// Own range, clear of test_proxy_integration (19000+) and benches (29xxx).
fn tls_port(offset: u16) -> u16 {
    19700 + offset
}

/// Accepts any server cert (self-signed) while capturing the presented
/// end-entity DER so tests can assert WHICH cert SNI selected.
#[derive(Debug)]
struct AcceptAnyCapture(Mutex<Option<Vec<u8>>>);

impl ServerCertVerifier for AcceptAnyCapture {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        *self.0.lock().unwrap() = Some(end_entity.as_ref().to_vec());
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

/// One HTTPS request. Returns (response bytes, presented end-entity DER),
/// or Err on handshake/IO failure.
fn https_request(
    addr: SocketAddr,
    sni: &str,
    request: &[u8],
) -> Result<(Vec<u8>, Vec<u8>), String> {
    // Dev-deps compile in both ring and aws-lc-rs; pick one explicitly.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let verifier = Arc::new(AcceptAnyCapture(Mutex::new(None)));
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_no_client_auth();
    let server_name = ServerName::try_from(sni.to_string()).map_err(|e| e.to_string())?;
    let mut conn =
        ClientConnection::new(Arc::new(config), server_name).map_err(|e| e.to_string())?;
    let mut sock = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
    sock.set_write_timeout(Some(Duration::from_secs(5))).ok();
    let mut tls = rustls::Stream::new(&mut conn, &mut sock);

    tls.write_all(request)
        .map_err(|e| format!("write: {}", e))?;
    let mut resp = Vec::new();
    match tls.read_to_end(&mut resp) {
        Ok(_) => {}
        // Close without close_notify after a complete response is fine here;
        // the framing assertions below still hold.
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && !resp.is_empty() => {}
        Err(e) => return Err(format!("read: {}", e)),
    }
    let presented = verifier
        .0
        .lock()
        .unwrap()
        .take()
        .ok_or("no cert captured")?;
    Ok((resp, presented))
}

fn pem_to_der(pem: &[u8]) -> Vec<u8> {
    rustls_pemfile::certs(&mut &pem[..])
        .next()
        .expect("cert in PEM")
        .expect("valid PEM")
        .as_ref()
        .to_vec()
}

#[test]
fn test_sni_selects_cert_and_routes_on_shared_port() {
    let (Some((cert_a, key_a)), Some((cert_b, key_b))) = (
        generate_test_cert("a.example.test"),
        generate_test_cert("*.wild.test"),
    ) else {
        eprintln!("openssl unavailable; skipping TLS e2e");
        return;
    };

    let backend_a = TestBackend::spawn("from backend A");
    let backend_b = TestBackend::spawn("from backend B");
    let port = tls_port(1);
    let _proxy = PortailProcess::spawn_with_tls(
        &[
            ("cert-a", "a.example.test", &cert_a, &key_a),
            ("cert-b", "*.wild.test", &cert_b, &key_b),
        ],
        &[
            ("a.example.test", backend_a.addr),
            ("x.wild.test", backend_b.addr),
        ],
        port,
    );
    let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

    let req_a = b"GET / HTTP/1.1\r\nHost: a.example.test\r\nConnection: close\r\n\r\n";
    let (resp, presented) = https_request(addr, "a.example.test", req_a).expect("https to a");
    assert_eq!(extract_status(&resp), Some(200));
    assert_eq!(
        std::str::from_utf8(extract_body(&resp)).unwrap().trim(),
        "from backend A"
    );
    assert_eq!(
        presented,
        pem_to_der(&cert_a),
        "SNI a.example.test must be served cert-a"
    );

    let req_b = b"GET / HTTP/1.1\r\nHost: x.wild.test\r\nConnection: close\r\n\r\n";
    let (resp, presented) = https_request(addr, "x.wild.test", req_b).expect("https to b");
    assert_eq!(extract_status(&resp), Some(200));
    assert_eq!(
        std::str::from_utf8(extract_body(&resp)).unwrap().trim(),
        "from backend B"
    );
    assert_eq!(
        presented,
        pem_to_der(&cert_b),
        "SNI x.wild.test must be served the wildcard cert-b"
    );
}

#[test]
fn test_sni_without_matching_cert_fails_handshake() {
    let (Some((cert_a, key_a)), Some((cert_b, key_b))) = (
        generate_test_cert("a.example.test"),
        generate_test_cert("*.wild.test"),
    ) else {
        eprintln!("openssl unavailable; skipping TLS e2e");
        return;
    };

    let backend = TestBackend::spawn("unreachable");
    let port = tls_port(2);
    let _proxy = PortailProcess::spawn_with_tls(
        &[
            ("cert-a", "a.example.test", &cert_a, &key_a),
            ("cert-b", "*.wild.test", &cert_b, &key_b),
        ],
        &[("a.example.test", backend.addr)],
        port,
    );
    let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

    // With multiple certs loaded there is no fallback: unmatched SNI must
    // abort the handshake rather than present an arbitrary cert.
    let req = b"GET / HTTP/1.1\r\nHost: unknown.example.test\r\nConnection: close\r\n\r\n";
    assert!(
        https_request(addr, "unknown.example.test", req).is_err(),
        "handshake must fail for SNI with no matching cert"
    );
}
