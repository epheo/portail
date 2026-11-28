//! TLS to a backend under a BackendTLSPolicy: what its certificate must
//! chain to, and which name it must carry.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

use crate::config::{BackendCa, BackendTls, San};
use crate::logging::warn;

/// A backend's verified TLS, ready to connect with.
pub struct Verify {
    /// What tells this policy from every other: the name sent, the names
    /// accepted, and what is trusted. A bundle counts by its content, not
    /// its name: an edited one is another policy, and no connection or
    /// session opened under the old one serves it.
    key: Box<str>,
    server_name: ServerName<'static>,
    connector: tokio_rustls::TlsConnector,
}

impl std::fmt::Debug for Verify {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Verify").field("key", &self.key).finish()
    }
}

impl Verify {
    /// What `tls` asks, against the CA bundles of the config it came with.
    /// An error means no connection may be made for it: what would have
    /// used it fails rather than go out unverified.
    ///
    /// One per policy for the process, for as long as a route table holds
    /// it: the connector keeps the TLS sessions, and a table is rebuilt at
    /// every DNS refresh.
    pub fn of(tls: &BackendTls, bundles: &BTreeMap<String, String>) -> Result<Arc<Self>, String> {
        static KNOWN: Mutex<Option<HashMap<Box<str>, Weak<Verify>>>> = Mutex::new(None);

        let bundle = match &tls.ca {
            BackendCa::System => None,
            BackendCa::Bundle(name) => Some((
                name,
                bundles
                    .get(name)
                    .ok_or_else(|| format!("no CA bundle named {name}"))?,
            )),
        };
        let trusted = match bundle {
            None => "system".to_string(),
            Some((_, pem)) => crate::sync::sha256_hex(pem.as_bytes()),
        };
        let key = format!("{}\0{trusted}\0{:?}", tls.hostname, tls.subject_alt_names);

        let mut known = KNOWN.lock().unwrap_or_else(PoisonError::into_inner);
        let known = known.get_or_insert_with(HashMap::new);
        if let Some(verify) = known.get(key.as_str()).and_then(Weak::upgrade) {
            return Ok(verify);
        }

        let server_name = ServerName::try_from(tls.hostname.clone())
            .map_err(|_| format!("{:?} is no server name", tls.hostname))?;
        let roots = match bundle {
            None => system_roots()?,
            Some((name, pem)) => Arc::new(
                roots_of(pem)
                    .ok_or_else(|| format!("CA bundle {name} holds no certificate to trust"))?,
            ),
        };
        let config = ClientConfig::builder();
        let config = if tls.subject_alt_names.is_empty() {
            config.with_root_certificates(roots)
        } else {
            config
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AnyOfSans {
                    roots,
                    sans: tls.subject_alt_names.clone(),
                    algorithms: rustls::crypto::aws_lc_rs::default_provider()
                        .signature_verification_algorithms,
                }))
        }
        .with_no_client_auth();
        let verify = Arc::new(Self {
            key: key.into(),
            server_name,
            connector: tokio_rustls::TlsConnector::from(Arc::new(config)),
        });
        known.retain(|_, verify| verify.strong_count() > 0);
        known.insert(verify.key.clone(), Arc::downgrade(&verify));
        Ok(verify)
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    /// The handshake, the backend's certificate checked.
    pub async fn connect(&self, tcp: TcpStream) -> std::io::Result<TlsStream<TcpStream>> {
        self.connector.connect(self.server_name.clone(), tcp).await
    }
}

fn roots_of(pem: &str) -> Option<RootCertStore> {
    let mut roots = RootCertStore::empty();
    let certs = CertificateDer::pem_slice_iter(pem.as_bytes()).filter_map(Result::ok);
    roots.add_parsable_certificates(certs);
    (!roots.is_empty()).then_some(roots)
}

/// The roots the host trusts, read once. A host with none refuses every
/// backend verified against them, and says so: the controller cannot know
/// what an image carries.
fn system_roots() -> Result<Arc<RootCertStore>, String> {
    static ROOTS: OnceLock<Option<Arc<RootCertStore>>> = OnceLock::new();
    ROOTS
        .get_or_init(|| {
            let found = rustls_native_certs::load_native_certs();
            let mut roots = RootCertStore::empty();
            roots.add_parsable_certificates(found.certs);
            if roots.is_empty() {
                warn!(
                    "The host's certificate store holds no root ({} errors reading it): every backend verified against the system roots is refused",
                    found.errors.len()
                );
                return None;
            }
            Some(Arc::new(roots))
        })
        .clone()
        .ok_or_else(|| "the host's certificate store holds no root".to_string())
}

/// A chain to `roots`, and one of `sans` on the certificate. The server
/// name is not checked: with names to match, the policy's hostname only
/// selects the certificate.
#[derive(Debug)]
struct AnyOfSans {
    roots: Arc<RootCertStore>,
    sans: Vec<San>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for AnyOfSans {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let parsed = rustls::server::ParsedCertificate::try_from(end_entity)?;
        rustls::client::verify_server_cert_signed_by_trust_anchor(
            &parsed,
            &self.roots,
            intermediates,
            now,
            self.algorithms.all,
        )?;
        let cert = webpki::EndEntityCert::try_from(end_entity)
            .map_err(|_| rustls::Error::InvalidCertificate(CertificateError::BadEncoding))?;
        let carried = |san: &San| match san {
            San::Hostname(name) => ServerName::try_from(name.as_str())
                .is_ok_and(|name| cert.verify_is_valid_for_subject_name(&name).is_ok()),
            San::Uri(uri) => cert.valid_uri_names().any(|carried| carried == uri),
        };
        if self.sans.iter().any(carried) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::testkit::{TestCa, OK};
    use std::net::SocketAddr;

    fn policy(hostname: &str, sans: &[San], bundle: &str) -> BackendTls {
        BackendTls {
            hostname: hostname.to_string(),
            subject_alt_names: sans.to_vec(),
            ca: BackendCa::Bundle(bundle.to_string()),
        }
    }

    fn bundles(named: &[(&str, &TestCa)]) -> BTreeMap<String, String> {
        named
            .iter()
            .map(|(name, ca)| (name.to_string(), ca.pem()))
            .collect()
    }

    async fn handshake(verify: &Verify, backend: SocketAddr) -> std::io::Result<()> {
        let tcp = TcpStream::connect(backend).await.unwrap();
        verify.connect(tcp).await.map(drop)
    }

    #[tokio::test]
    async fn a_backend_must_chain_to_the_bundle_and_carry_the_hostname() {
        let (ca, other) = (TestCa::new(), TestCa::new());
        let (backend, _seen) = ca.backend(&["svc.test"], &[], OK.as_bytes()).await;
        let bundles = bundles(&[("ca", &ca), ("other", &other)]);

        let right = Verify::of(&policy("svc.test", &[], "ca"), &bundles).unwrap();
        handshake(&right, backend).await.unwrap();

        let wrong_name = Verify::of(&policy("else.test", &[], "ca"), &bundles).unwrap();
        handshake(&wrong_name, backend).await.unwrap_err();

        let wrong_ca = Verify::of(&policy("svc.test", &[], "other"), &bundles).unwrap();
        handshake(&wrong_ca, backend).await.unwrap_err();
    }

    /// With names to match, the hostname only selects the certificate: one
    /// of the names must be on it, and the chain is checked all the same.
    #[tokio::test]
    async fn with_subject_alt_names_one_of_them_must_be_on_the_certificate() {
        let (ca, other) = (TestCa::new(), TestCa::new());
        let id = "spiffe://td.test/ns/a/sa/b";
        let (backend, _seen) = ca
            .backend(&["svc.test", "*.wild.test"], &[id], OK.as_bytes())
            .await;
        let bundles = bundles(&[("ca", &ca), ("other", &other)]);
        let host = |name: &str| San::Hostname(name.to_string());
        let uri = |uri: &str| San::Uri(uri.to_string());

        for sans in [
            vec![host("svc.test")],
            vec![host("under.wild.test")],
            vec![uri(id)],
            vec![
                host("else.test"),
                uri("spiffe://td.test/ns/a/sa/c"),
                uri(id),
            ],
        ] {
            let verify = Verify::of(&policy("unchecked.test", &sans, "ca"), &bundles).unwrap();
            let done = handshake(&verify, backend).await;
            assert!(done.is_ok(), "{sans:?}: {done:?}");
        }
        for (sans, bundle) in [
            // The hostname is on the certificate, and is not a name to match.
            (vec![host("else.test")], "ca"),
            (vec![uri("spiffe://td.test/ns/a/sa")], "ca"),
            (vec![uri("svc.test")], "ca"),
            (vec![host(id)], "ca"),
            (vec![host("svc.test"), uri(id)], "other"),
        ] {
            let verify = Verify::of(&policy("svc.test", &sans, bundle), &bundles).unwrap();
            let done = handshake(&verify, backend).await;
            assert!(done.is_err(), "{sans:?} against {bundle} passed");
        }
    }

    #[test]
    fn a_policy_that_cannot_be_honoured_gives_no_verify() {
        let ca = TestCa::new();
        let mut bundles = bundles(&[("ca", &ca)]);
        bundles.insert("empty".to_string(), "no certificate here".to_string());

        for (tls, why) in [
            (
                policy("svc.test", &[], "absent"),
                "no CA bundle named absent",
            ),
            (policy("svc.test", &[], "empty"), "holds no certificate"),
            (policy("not a name", &[], "ca"), "is no server name"),
        ] {
            let refused = Verify::of(&tls, &bundles).unwrap_err();
            assert!(refused.contains(why), "{refused}");
        }
    }

    /// One connector per policy for the process: it holds the sessions. A
    /// bundle counts by what it holds, so an edited one under the same
    /// name shares nothing with the old.
    #[test]
    fn a_policy_keeps_its_verify_until_its_bundle_changes() {
        let (ca, other) = (TestCa::new(), TestCa::new());
        let tls = policy("svc.test", &[], "ca");

        let first = Verify::of(&tls, &bundles(&[("ca", &ca)])).unwrap();
        let again = Verify::of(&tls, &bundles(&[("ca", &ca)])).unwrap();
        assert!(Arc::ptr_eq(&first, &again));

        let renamed = Verify::of(&policy("svc.test", &[], "x"), &bundles(&[("x", &ca)])).unwrap();
        assert!(Arc::ptr_eq(&first, &renamed), "one bundle under two names");

        let edited = Verify::of(&tls, &bundles(&[("ca", &other)])).unwrap();
        assert!(!Arc::ptr_eq(&first, &edited));
        assert_ne!(first.key(), edited.key());

        let other_names = policy("svc.test", &[San::Hostname("svc.test".to_string())], "ca");
        let other_names = Verify::of(&other_names, &bundles(&[("ca", &ca)])).unwrap();
        assert_ne!(first.key(), other_names.key());
    }

    /// A second connection under one policy resumes the first's session:
    /// the backend is not asked for its certificate again.
    #[tokio::test]
    async fn a_policy_resumes_its_sessions() {
        let ca = TestCa::new();
        let (backend, _seen) = ca.backend(&["svc.test"], &[], OK.as_bytes()).await;
        let verify = Verify::of(&policy("svc.test", &[], "ca"), &bundles(&[("ca", &ca)])).unwrap();

        let mut kinds = Vec::new();
        for _ in 0..2 {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let tcp = TcpStream::connect(backend).await.unwrap();
            let mut tls = verify.connect(tcp).await.unwrap();
            kinds.push(tls.get_ref().1.handshake_kind());
            // The ticket comes after the handshake, with the first bytes read.
            tls.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
            tls.read_exact(&mut [0u8; OK.len()]).await.unwrap();
        }
        assert_eq!(
            kinds,
            [
                Some(rustls::HandshakeKind::Full),
                Some(rustls::HandshakeKind::Resumed)
            ]
        );
    }
}
