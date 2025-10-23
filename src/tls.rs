use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::{anyhow, Result};
use pin_project_lite::pin_project;
use rustls::ServerConfig;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

use crate::config::CertificateRef;

pin_project! {
    /// Unified connection type — avoids making every handler function generic.
    #[project = ConnectionProj]
    pub enum Connection {
        Plain { #[pin] inner: TcpStream },
        Tls { #[pin] inner: TlsStream<TcpStream> },
    }
}

impl AsyncRead for Connection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.project() {
            ConnectionProj::Plain { inner } => inner.poll_read(cx, buf),
            ConnectionProj::Tls { inner } => inner.poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Connection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.project() {
            ConnectionProj::Plain { inner } => inner.poll_write(cx, buf),
            ConnectionProj::Tls { inner } => inner.poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.project() {
            ConnectionProj::Plain { inner } => inner.poll_flush(cx),
            ConnectionProj::Tls { inner } => inner.poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.project() {
            ConnectionProj::Plain { inner } => inner.poll_shutdown(cx),
            ConnectionProj::Tls { inner } => inner.poll_shutdown(cx),
        }
    }
}

impl Connection {
    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        match self {
            Connection::Plain { inner } => inner.set_nodelay(nodelay),
            Connection::Tls { inner } => inner.get_ref().0.set_nodelay(nodelay),
        }
    }
}

/// Load PEM cert+key from disk and build a TLS acceptor.
///
/// In standalone mode, `name` from CertificateRef resolves to:
///   {cert_dir}/{name}.crt  and  {cert_dir}/{name}.key
pub fn build_tls_acceptor(cert_refs: &[CertificateRef], cert_dir: &Path) -> Result<TlsAcceptor> {
    if cert_refs.is_empty() {
        return Err(anyhow!("TLS Terminate mode requires at least one certificateRef"));
    }

    // Use the first certificate ref (multi-cert / SNI-based selection is future work)
    let cert_ref = &cert_refs[0];

    let cert_path = cert_dir.join(format!("{}.crt", cert_ref.name));
    let key_path = cert_dir.join(format!("{}.key", cert_ref.name));

    let cert_pem = std::fs::read(&cert_path)
        .map_err(|e| anyhow!("Failed to read certificate '{}': {}", cert_path.display(), e))?;
    let key_pem = std::fs::read(&key_path)
        .map_err(|e| anyhow!("Failed to read key '{}': {}", key_path.display(), e))?;

    let certs: Vec<_> = rustls_pemfile::certs(&mut &cert_pem[..])
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| anyhow!("Failed to parse certificate PEM: {}", e))?;

    if certs.is_empty() {
        return Err(anyhow!("No certificates found in '{}'", cert_path.display()));
    }

    let key = rustls_pemfile::private_key(&mut &key_pem[..])
        .map_err(|e| anyhow!("Failed to parse private key PEM: {}", e))?
        .ok_or_else(|| anyhow!("No private key found in '{}'", key_path.display()))?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow!("TLS config error: {}", e))?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Extract SNI hostname from a TLS ClientHello message.
///
/// Parses just enough of the TLS record layer and handshake to find the
/// server_name extension. Returns None if the data isn't a valid ClientHello
/// or doesn't contain SNI.
pub fn extract_sni(buf: &[u8]) -> Option<String> {
    // TLS record: type(1) + version(2) + length(2) + fragment
    if buf.len() < 5 || buf[0] != 0x16 {
        return None; // Not a TLS handshake record
    }

    let record_len = u16::from_be_bytes([buf[3], buf[4]]) as usize;
    if buf.len() < 5 + record_len {
        return None;
    }
    let hs = &buf[5..5 + record_len];

    // Handshake: type(1) + length(3)
    if hs.is_empty() || hs[0] != 0x01 {
        return None; // Not a ClientHello
    }
    if hs.len() < 4 {
        return None;
    }
    let hs_len = ((hs[1] as usize) << 16) | ((hs[2] as usize) << 8) | (hs[3] as usize);
    if hs.len() < 4 + hs_len {
        return None;
    }
    let ch = &hs[4..4 + hs_len];

    // ClientHello: version(2) + random(32) + session_id_len(1) + session_id(var)
    if ch.len() < 35 {
        return None;
    }
    let session_id_len = ch[34] as usize;
    let mut pos = 35 + session_id_len;

    // cipher_suites: length(2) + data(var)
    if ch.len() < pos + 2 {
        return None;
    }
    let cs_len = u16::from_be_bytes([ch[pos], ch[pos + 1]]) as usize;
    pos += 2 + cs_len;

    // compression_methods: length(1) + data(var)
    if ch.len() < pos + 1 {
        return None;
    }
    let cm_len = ch[pos] as usize;
    pos += 1 + cm_len;

    // extensions: length(2) + data(var)
    if ch.len() < pos + 2 {
        return None;
    }
    let ext_len = u16::from_be_bytes([ch[pos], ch[pos + 1]]) as usize;
    pos += 2;

    if ch.len() < pos + ext_len {
        return None;
    }
    let ext_end = pos + ext_len;

    while pos + 4 <= ext_end {
        let ext_type = u16::from_be_bytes([ch[pos], ch[pos + 1]]);
        let ext_data_len = u16::from_be_bytes([ch[pos + 2], ch[pos + 3]]) as usize;
        pos += 4;

        if pos + ext_data_len > ext_end {
            return None;
        }

        if ext_type == 0x0000 {
            // server_name extension
            let sni_data = &ch[pos..pos + ext_data_len];
            if sni_data.len() < 2 {
                return None;
            }
            let sni_list_len = u16::from_be_bytes([sni_data[0], sni_data[1]]) as usize;
            if sni_data.len() < 2 + sni_list_len || sni_list_len < 3 {
                return None;
            }
            // First entry: type(1) + length(2) + hostname(var)
            let name_type = sni_data[2];
            if name_type != 0x00 {
                return None; // Only host_name type
            }
            let name_len = u16::from_be_bytes([sni_data[3], sni_data[4]]) as usize;
            if sni_data.len() < 5 + name_len {
                return None;
            }
            return String::from_utf8(sni_data[5..5 + name_len].to_vec()).ok();
        }

        pos += ext_data_len;
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_sni_valid_client_hello() {
        // Minimal TLS 1.2 ClientHello with SNI "example.com"
        let hostname = b"example.com";
        let sni_ext = build_sni_extension(hostname);
        let client_hello = build_client_hello(&sni_ext);
        let record = build_tls_record(&client_hello);

        assert_eq!(extract_sni(&record), Some("example.com".to_string()));
    }

    #[test]
    fn test_extract_sni_no_sni_extension() {
        let client_hello = build_client_hello(&[]);
        let record = build_tls_record(&client_hello);
        assert_eq!(extract_sni(&record), None);
    }

    #[test]
    fn test_extract_sni_not_tls() {
        assert_eq!(extract_sni(b"GET / HTTP/1.1\r\n"), None);
    }

    #[test]
    fn test_extract_sni_truncated() {
        assert_eq!(extract_sni(&[0x16, 0x03, 0x01]), None);
    }

    // Test helpers to build minimal TLS structures
    fn build_sni_extension(hostname: &[u8]) -> Vec<u8> {
        let name_len = hostname.len() as u16;
        let sni_list_len = name_len + 3; // type(1) + len(2) + name
        let ext_data_len = sni_list_len + 2; // list_len(2) + list
        let mut ext = Vec::new();
        ext.extend_from_slice(&0u16.to_be_bytes()); // ext type: server_name
        ext.extend_from_slice(&ext_data_len.to_be_bytes());
        ext.extend_from_slice(&sni_list_len.to_be_bytes());
        ext.push(0x00); // host_name type
        ext.extend_from_slice(&name_len.to_be_bytes());
        ext.extend_from_slice(hostname);
        ext
    }

    fn build_client_hello(extensions: &[u8]) -> Vec<u8> {
        let mut ch = Vec::new();
        // version
        ch.extend_from_slice(&[0x03, 0x03]);
        // random (32 bytes)
        ch.extend_from_slice(&[0u8; 32]);
        // session_id_len = 0
        ch.push(0);
        // cipher_suites: 2 bytes length + 1 suite (2 bytes)
        ch.extend_from_slice(&[0x00, 0x02, 0x00, 0x2f]);
        // compression_methods: 1 byte length + null
        ch.extend_from_slice(&[0x01, 0x00]);
        // extensions
        let ext_len = extensions.len() as u16;
        ch.extend_from_slice(&ext_len.to_be_bytes());
        ch.extend_from_slice(extensions);

        // Wrap in handshake header: type(1) + length(3)
        let ch_len = ch.len();
        let mut hs = vec![0x01]; // ClientHello
        hs.push((ch_len >> 16) as u8);
        hs.push((ch_len >> 8) as u8);
        hs.push(ch_len as u8);
        hs.extend_from_slice(&ch);
        hs
    }

    fn build_tls_record(handshake: &[u8]) -> Vec<u8> {
        let mut rec = vec![0x16, 0x03, 0x01]; // TLS handshake, version 3.1
        let len = handshake.len() as u16;
        rec.extend_from_slice(&len.to_be_bytes());
        rec.extend_from_slice(handshake);
        rec
    }

    #[test]
    fn test_extract_sni_subdomain() {
        let hostname = b"api.staging.example.com";
        let sni_ext = build_sni_extension(hostname);
        let record = build_tls_record(&build_client_hello(&sni_ext));
        assert_eq!(extract_sni(&record), Some("api.staging.example.com".to_string()));
    }

    #[test]
    fn test_extract_sni_empty_buffer() {
        assert_eq!(extract_sni(&[]), None);
    }

    #[test]
    fn test_extract_sni_not_handshake_type() {
        // Application data (0x17) instead of handshake (0x16)
        assert_eq!(extract_sni(&[0x17, 0x03, 0x01, 0x00, 0x05, 0, 0, 0, 0, 0]), None);
    }

    #[test]
    fn test_build_tls_acceptor_empty_refs() {
        let result = build_tls_acceptor(&[], std::path::Path::new("/nonexistent"));
        assert!(result.is_err());
    }

    #[test]
    fn test_build_tls_acceptor_missing_cert_file() {
        let refs = vec![CertificateRef { name: "nonexistent".to_string() }];
        let result = build_tls_acceptor(&refs, std::path::Path::new("/tmp"));
        assert!(result.is_err());
    }
}
