//! Signed controller answers. Through the apiserver's service proxy a data
//! plane's TLS ends at the apiserver, which dials the controller without
//! verifying it: whoever may edit the controller Service or its endpoints
//! could serve snapshots. So the controller signs every snapshot and every
//! gone answer with its serving leaf's key and sends that leaf; a data
//! plane checks the leaf against the controller CA, for the controller's
//! name, valid now, and the signature over the body as received, on either
//! transport. The CA's own key never signs at runtime.
//!
//! A 204 is not signed: it changes nothing a data plane serves.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use http::{HeaderMap, HeaderValue};
use rustls::client::danger::ServerCertVerifier;
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::sign::CertifiedKey;
use rustls::{RootCertStore, SignatureScheme};
use serde::{Deserialize, Serialize};

use super::{GatewayKey, HOLD};

/// The signature over an answer, base64: ECDSA P-256 SHA-256, DER.
pub const SIGNATURE_HEADER: &str = "x-portail-signature";
/// The leaf certificate whose key made it, base64 DER.
pub const SIGNER_HEADER: &str = "x-portail-signer";

/// A gone answer further than this from the data plane's clock, either
/// way, is refused as a replay. Two holds, the client's request timeout: a
/// genuine one is read well within it, and the rest absorbs clock skew
/// between the controller's node and the data plane's.
pub const GONE_MAX_AGE: Duration = Duration::from_secs(2 * HOLD.as_secs());

/// What a signature vouches for. Its context goes before the body in the
/// message signed: a snapshot's signature never passes for a gone answer's,
/// nor a TLS handshake's, made with the same key, for either.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signed {
    /// A 200's body.
    Snapshot,
    /// A 404's body, a [`Gone`].
    Gone,
}

impl Signed {
    fn message(self, body: &[u8]) -> Vec<u8> {
        let context: &[u8] = match self {
            Signed::Snapshot => b"portail sync v1 snapshot\n",
            Signed::Gone => b"portail sync v1 gone\n",
        };
        [context, body].concat()
    }
}

/// The body of a 404 to a Gateway's own data plane: the controller serves
/// it nothing, as of `issued_at` (Unix seconds).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Gone {
    pub gateway: GatewayKey,
    pub issued_at: u64,
}

impl Gone {
    pub fn now(gateway: GatewayKey) -> Self {
        Self {
            gateway,
            issued_at: unix_secs(SystemTime::now()),
        }
    }

    /// Whether `body` says `gateway` is gone, within [`GONE_MAX_AGE`] of
    /// `now`.
    pub fn check(body: &[u8], gateway: &GatewayKey, now: SystemTime) -> Result<(), String> {
        let gone: Gone =
            serde_json::from_slice(body).map_err(|e| format!("not a gone answer: {e}"))?;
        if gone.gateway != *gateway {
            return Err(format!("a gone answer for Gateway {}", gone.gateway));
        }
        let skew = unix_secs(now).abs_diff(gone.issued_at);
        if skew > GONE_MAX_AGE.as_secs() {
            return Err(format!(
                "a gone answer issued {skew}s away from this clock, over {}s",
                GONE_MAX_AGE.as_secs()
            ));
        }
        Ok(())
    }
}

fn unix_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The headers that sign `body`, an answer of `kind`, with `key`.
pub fn sign(
    key: &CertifiedKey,
    kind: Signed,
    body: &[u8],
) -> Result<[(&'static str, HeaderValue); 2], String> {
    let signer = key
        .key
        .choose_scheme(&[SignatureScheme::ECDSA_NISTP256_SHA256])
        .ok_or("the serving key does not sign ECDSA P-256 SHA-256")?;
    let signature = signer
        .sign(&kind.message(body))
        .map_err(|e| e.to_string())?;
    let leaf = key.end_entity_cert().map_err(|e| e.to_string())?;
    let value = |bytes: &[u8]| {
        HeaderValue::from_str(&STANDARD.encode(bytes)).expect("base64 is a header value")
    };
    Ok([
        (SIGNATURE_HEADER, value(&signature)),
        (SIGNER_HEADER, value(leaf)),
    ])
}

/// The certificates of `ca_pem`, the only roots a peer is trusted by;
/// `what` names the PEM in errors.
pub fn ca_roots(ca_pem: &[u8], what: &str) -> anyhow::Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(ca_pem) {
        roots
            .add(cert.with_context(|| what.to_string())?)
            .with_context(|| what.to_string())?;
    }
    anyhow::ensure!(!roots.is_empty(), "{what}: no certificate in the PEM");
    Ok(roots)
}

/// Checks answers as signed by a leaf of the controller CA for the
/// controller's name.
#[derive(Debug)]
pub struct AnswerVerifier {
    certs: Arc<WebPkiServerVerifier>,
    signer: ServerName<'static>,
}

impl AnswerVerifier {
    /// Answers signed by a leaf chaining to `roots`, for `signer`.
    pub fn new(roots: RootCertStore, signer: &str) -> anyhow::Result<Self> {
        let certs = WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
        )
        .build()?;
        let signer = ServerName::try_from(signer.to_string())
            .with_context(|| format!("{signer:?} is no controller name"))?;
        Ok(Self { certs, signer })
    }

    /// Whether `body`, an answer of `kind` with `headers`, is signed by a
    /// controller leaf valid at `now`.
    pub fn verify(
        &self,
        kind: Signed,
        headers: &HeaderMap,
        body: &[u8],
        now: SystemTime,
    ) -> Result<(), String> {
        let decoded = |name: &str| -> Result<Vec<u8>, String> {
            let value = headers
                .get(name)
                .ok_or_else(|| format!("unsigned: no {name} header"))?;
            STANDARD
                .decode(value.as_bytes())
                .map_err(|_| format!("{name} is not base64"))
        };
        let signature = decoded(SIGNATURE_HEADER)?;
        let leaf = CertificateDer::from(decoded(SIGNER_HEADER)?);
        let at = UnixTime::since_unix_epoch(now.duration_since(UNIX_EPOCH).unwrap_or_default());
        self.certs
            .verify_server_cert(&leaf, &[], &self.signer, &[], at)
            .map_err(|e| format!("signer certificate refused: {e}"))?;
        webpki::EndEntityCert::try_from(&leaf)
            .and_then(|cert| {
                cert.verify_signature(
                    webpki::aws_lc_rs::ECDSA_P256_SHA256,
                    &kind.message(body),
                    &signature,
                )
            })
            .map_err(|e| format!("bad signature: {e}"))
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa,
        KeyPair, KeyUsagePurpose,
    };
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};

    /// A CA, as PEM, and leaves it issues.
    pub struct TestCa {
        issuer: CertifiedIssuer<'static, KeyPair>,
    }

    impl TestCa {
        pub fn new() -> Self {
            let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
            let issuer =
                CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
            Self { issuer }
        }

        pub fn pem(&self) -> String {
            self.issuer.pem()
        }

        pub fn roots(&self) -> RootCertStore {
            ca_roots(self.pem().as_bytes(), "test CA").unwrap()
        }

        /// A serving leaf for `names`, valid from `not_before` to
        /// `not_after`.
        pub fn leaf_between(
            &self,
            names: &[&str],
            not_before: SystemTime,
            not_after: SystemTime,
        ) -> CertifiedKey {
            let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
            let mut params = CertificateParams::new(names).unwrap();
            params.not_before = not_before.into();
            params.not_after = not_after.into();
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
            let key = KeyPair::generate().unwrap();
            let cert = params.signed_by(&key, &self.issuer).unwrap();
            CertifiedKey::from_der(
                vec![cert.der().clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
                &rustls::crypto::aws_lc_rs::default_provider(),
            )
            .unwrap()
        }

        /// A serving leaf for `names`, valid for a day around now.
        pub fn leaf(&self, names: &[&str]) -> CertifiedKey {
            let day = Duration::from_secs(24 * 60 * 60);
            let now = SystemTime::now();
            self.leaf_between(names, now - day, now + day)
        }
    }

    /// `headers` with the signature of `body` by `key` in them.
    pub fn signed(key: &CertifiedKey, kind: Signed, body: &[u8]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in sign(key, kind, body).unwrap() {
            headers.insert(name, value);
        }
        headers
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{signed, TestCa};
    use super::*;

    const NAME: &str = "portail-controller.portail-system.svc";

    fn verifier(ca: &TestCa) -> AnswerVerifier {
        AnswerVerifier::new(ca.roots(), NAME).unwrap()
    }

    #[test]
    fn a_signed_snapshot_verifies_and_a_tampered_one_does_not() {
        let ca = TestCa::new();
        let key = ca.leaf(&[NAME]);
        let body = br#"{"version":"v1"}"#;
        let headers = signed(&key, Signed::Snapshot, body);
        let now = SystemTime::now();
        verifier(&ca)
            .verify(Signed::Snapshot, &headers, body, now)
            .unwrap();

        let tampered = br#"{"version":"v2"}"#;
        let refused = verifier(&ca)
            .verify(Signed::Snapshot, &headers, tampered, now)
            .unwrap_err();
        assert!(refused.contains("bad signature"), "{refused}");
    }

    /// The context in the message: a snapshot's signature is no gone
    /// answer's, though the bytes are the same.
    #[test]
    fn a_signature_vouches_for_one_kind() {
        let ca = TestCa::new();
        let body = b"{}";
        let headers = signed(&ca.leaf(&[NAME]), Signed::Snapshot, body);
        let refused = verifier(&ca)
            .verify(Signed::Gone, &headers, body, SystemTime::now())
            .unwrap_err();
        assert!(refused.contains("bad signature"), "{refused}");
    }

    /// Another CA's leaf, an expired leaf, a leaf for another name: each
    /// refused for its certificate, though its signature is good.
    #[test]
    fn the_signer_must_be_a_controller_leaf_valid_now() {
        let ca = TestCa::new();
        let body = b"{}";
        let now = SystemTime::now();
        let day = Duration::from_secs(24 * 60 * 60);
        let cases = [
            ("another CA", TestCa::new().leaf(&[NAME])),
            (
                "expired",
                ca.leaf_between(&[NAME], now - 3 * day, now - day),
            ),
            (
                "not yet valid",
                ca.leaf_between(&[NAME], now + day, now + 3 * day),
            ),
            ("another name", ca.leaf(&["portail-controller.other.svc"])),
        ];
        for (why, key) in cases {
            let headers = signed(&key, Signed::Snapshot, body);
            let refused = verifier(&ca)
                .verify(Signed::Snapshot, &headers, body, now)
                .unwrap_err();
            assert!(refused.contains("signer certificate"), "{why}: {refused}");
        }
    }

    #[test]
    fn an_answer_without_its_headers_is_unsigned() {
        let ca = TestCa::new();
        let body = b"{}";
        let headers = signed(&ca.leaf(&[NAME]), Signed::Snapshot, body);
        for missing in [SIGNATURE_HEADER, SIGNER_HEADER] {
            let mut partial = headers.clone();
            partial.remove(missing);
            let refused = verifier(&ca)
                .verify(Signed::Snapshot, &partial, body, SystemTime::now())
                .unwrap_err();
            assert!(refused.contains(missing), "{refused}");
        }
        let mut garbled = headers;
        garbled.insert(SIGNATURE_HEADER, HeaderValue::from_static("not base64!"));
        assert!(verifier(&ca)
            .verify(Signed::Snapshot, &garbled, body, SystemTime::now())
            .is_err());
    }

    #[test]
    fn a_gone_answer_is_for_this_gateway_and_recent() {
        let gateway = GatewayKey::new("ns", "gw");
        let now = SystemTime::now();
        let body = |gateway: &GatewayKey, at: SystemTime| {
            serde_json::to_vec(&Gone {
                gateway: gateway.clone(),
                issued_at: unix_secs(at),
            })
            .unwrap()
        };
        Gone::check(&body(&gateway, now), &gateway, now).unwrap();
        let edge = now - GONE_MAX_AGE;
        Gone::check(&body(&gateway, edge), &gateway, now).unwrap();

        let second = Duration::from_secs(1);
        let refused = [
            body(&GatewayKey::new("ns", "other"), now),
            body(&gateway, edge - second),
            body(&gateway, now + GONE_MAX_AGE + second),
            b"gateway not served\n".to_vec(),
        ];
        for body in refused {
            assert!(
                Gone::check(&body, &gateway, now).is_err(),
                "{}",
                String::from_utf8_lossy(&body)
            );
        }
    }

    #[test]
    fn a_gone_answer_reads_as_sent() {
        let gone = Gone::now(GatewayKey::new("ns", "gw"));
        let json = serde_json::to_value(&gone).unwrap();
        assert_eq!(json["gateway"]["name"], "gw");
        assert!(json["issuedAt"].as_u64().unwrap() > 0);
    }

    #[test]
    fn a_ca_without_a_certificate_is_refused() {
        let error = ca_roots(b"no pem", "controller CA").unwrap_err();
        assert!(error.to_string().contains("no certificate"), "{error:#}");
    }
}
