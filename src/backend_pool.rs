//! Per-worker connection pool for persistent backend connections.
//!
//! Each worker owns its own pool — no locks, no contention.
//! Reuses idle backend connections across requests to amortize TCP handshake cost.
//! Supports both plain TCP and TLS backend connections.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use anyhow::Result;
use crate::logging::debug;
use crate::tls::Connection;

pub struct BackendPool {
    pools: HashMap<SocketAddr, Vec<Connection>>,
    max_idle_per_backend: usize,
    connect_timeout: Duration,
    tls_connector: Arc<tokio_rustls::TlsConnector>,
}

impl BackendPool {
    pub fn new(max_idle_per_backend: usize, connect_timeout: Duration) -> Self {
        // Build a client TLS config that skips certificate verification.
        // Backend services in k8s typically use self-signed certs.
        let client_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
            .with_no_client_auth();

        Self {
            pools: HashMap::new(),
            max_idle_per_backend,
            connect_timeout,
            tls_connector: Arc::new(tokio_rustls::TlsConnector::from(Arc::new(client_config))),
        }
    }

    pub async fn acquire(&mut self, addr: SocketAddr, use_tls: bool, server_name: &str) -> Result<Connection> {
        // Try reuse an idle connection — pop and probe until we find a live one.
        if let Some(conns) = self.pools.get_mut(&addr) {
            while let Some(conn) = conns.pop() {
                let mut probe = [0u8; 0];
                match conn.try_read(&mut probe) {
                    // Would block = connection is alive and idle
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        debug!("Pool hit: reusing connection to {}", addr);
                        return Ok(conn);
                    }
                    // Zero bytes read or error = stale connection, try next
                    _ => continue,
                }
            }
        }

        // No reusable connection — open a new one
        debug!("Pool miss: connecting to {} (tls={})", addr, use_tls);
        let tcp = tokio::time::timeout(
            self.connect_timeout,
            TcpStream::connect(addr),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Backend connect timeout: {}", addr))?
        .map_err(|e| anyhow::anyhow!("Backend connect failed {}: {}", addr, e))?;

        tcp.set_nodelay(true)?;

        if use_tls {
            let domain = rustls::pki_types::ServerName::try_from(server_name.to_string())
                .unwrap_or_else(|_| rustls::pki_types::ServerName::try_from("localhost".to_string()).unwrap());
            let tls_stream = self.tls_connector.connect(domain, tcp).await
                .map_err(|e| anyhow::anyhow!("Backend TLS handshake failed {}: {}", addr, e))?;
            Ok(Connection::ClientTls { inner: tls_stream })
        } else {
            Ok(Connection::Plain { inner: tcp })
        }
    }

    pub fn release(&mut self, addr: SocketAddr, conn: Connection) {
        let conns = self.pools.entry(addr).or_default();
        if conns.len() < self.max_idle_per_backend {
            conns.push(conn);
        }
        // else: drop closes the fd
    }
}

/// Certificate verifier that accepts any server certificate.
/// Used for backend connections where services use self-signed certs.
#[derive(Debug)]
struct NoVerifier;

impl rustls::client::danger::ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
