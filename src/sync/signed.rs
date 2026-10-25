//! Signed controller answers. Through the apiserver's service proxy a data
//! plane's TLS ends at the apiserver, which dials the controller without
//! verifying it: whoever may edit the controller Service or its endpoints
//! could answer polls. So the controller signs every answer that steers a
//! data plane, 200, 204 and 404, with its serving leaf's key and sends
//! that leaf; a data plane checks the leaf against the controller CA, for
//! the controller's name, valid now, and the signature, on either
//! transport. The CA's own key never signs at runtime.
//!
//! The signature covers the poll's nonce: an answer to an earlier poll,
//! replayed, never passes for this one's, and no clock is compared.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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

use super::GatewayKey;

/// The signature over an answer, base64: ECDSA P-256 SHA-256, DER.
pub const SIGNATURE_HEADER: &str = "x-portail-signature";
/// The leaf certificate whose key made it, base64 DER.
pub const SIGNER_HEADER: &str = "x-portail-signer";

/// Random bytes in a poll's nonce: as many as a UUID's, never repeated nor
/// guessed in a data plane's life.
const NONCE_BYTES: usize = 16;

/// A fresh nonce for one poll, base64. From the system CSPRNG: a predicted
/// nonce would let whoever relays a poll have an answer signed for a later
/// one.
pub fn new_nonce() -> String {
    let mut bytes = [0u8; NONCE_BYTES];
    aws_lc_rs::rand::fill(&mut bytes).expect("the system CSPRNG is available");
    STANDARD.encode(bytes)
}

/// Whether `nonce` is one [`new_nonce`] could have drawn: the controller
/// signs over nothing else, so a caller cannot have it sign arbitrary
/// bytes there.
pub fn valid_nonce(nonce: &str) -> bool {
    STANDARD
        .decode(nonce)
        .is_ok_and(|bytes| bytes.len() == NONCE_BYTES)
}

/// What a signature vouches for. Its context goes first in the message
/// signed, then the poll's nonce: a snapshot's signature never passes for
/// another kind's, an answer to another poll never for this one's, nor a
/// TLS handshake's, made with the same key, for any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signed {
    /// A 200's body.
    Snapshot,
    /// A 404's body, a [`Gone`].
    Gone,
    /// A 204 over the version it confirms current: the one served, equal
    /// to the poll's `have`.
    Unchanged,
    /// A 204 while the Gateway has no snapshot yet: the controller
    /// answered, and confirms nothing.
    Pending,
    /// A 204 to a leaving poll. Never a confirmation: a token holder could
    /// send a leaving poll with any `have`.
    Leave,
}

impl Signed {
    fn message(self, nonce: &str, body: &[u8]) -> Vec<u8> {
        let context: &[u8] = match self {
            Signed::Snapshot => b"portail sync v1 snapshot\n",
            Signed::Gone => b"portail sync v1 gone\n",
            Signed::Unchanged => b"portail sync v1 unchanged\n",
            Signed::Pending => b"portail sync v1 pending\n",
            Signed::Leave => b"portail sync v1 leave\n",
        };
        [context, nonce.as_bytes(), b"\n", body].concat()
    }
}

/// The body of a 404 to a Gateway's own data plane: the controller serves
/// it nothing. It names the Gateway: a gone answer for another never
/// closes this one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gone {
    pub gateway: GatewayKey,
}

impl Gone {
    /// Whether `body` says `gateway` is gone.
    pub fn check(body: &[u8], gateway: &GatewayKey) -> Result<(), String> {
        let gone: Gone =
            serde_json::from_slice(body).map_err(|e| format!("not a gone answer: {e}"))?;
        if gone.gateway != *gateway {
            return Err(format!("a gone answer for Gateway {}", gone.gateway));
        }
        Ok(())
    }
}

/// The headers that sign `body`, an answer of `kind` to the poll of
/// `nonce`, with `key`.
pub fn sign(
    key: &CertifiedKey,
    kind: Signed,
    nonce: &str,
    body: &[u8],
) -> Result<[(&'static str, HeaderValue); 2], String> {
    let signer = key
        .key
        .choose_scheme(&[SignatureScheme::ECDSA_NISTP256_SHA256])
        .ok_or("the serving key does not sign ECDSA P-256 SHA-256")?;
    let signature = signer
        .sign(&kind.message(nonce, body))
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

    /// Whether `body`, an answer of `kind` with `headers` to the poll of
    /// `nonce`, is signed by a controller leaf valid at `now`.
    pub fn verify(
        &self,
        kind: Signed,
        nonce: &str,
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
                    &kind.message(nonce, body),
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
    use std::time::Duration;

    /// A CA, as PEM, and leaves it issues.
    pub struct TestCa {
        issuer: CertifiedIssuer<'static, KeyPair>,
        /// The CA's key, PKCS#8.
        key: Vec<u8>,
    }

    impl TestCa {
        pub fn new() -> Self {
            let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
            let key = KeyPair::generate().unwrap();
            let der = key.serialize_der();
            let issuer = CertifiedIssuer::self_signed(params, key).unwrap();
            Self { issuer, key: der }
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
            certified(cert.der().clone(), key.serialize_der())
        }

        /// A serving leaf for `names`, valid for a day around now.
        pub fn leaf(&self, names: &[&str]) -> CertifiedKey {
            let day = Duration::from_secs(24 * 60 * 60);
            let now = SystemTime::now();
            self.leaf_between(names, now - day, now + day)
        }

        /// A leaf for `names` whose only extended key usage is client
        /// auth.
        pub fn client_leaf(&self, names: &[&str]) -> CertifiedKey {
            let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
            let mut params = CertificateParams::new(names).unwrap();
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
            let key = KeyPair::generate().unwrap();
            let cert = params.signed_by(&key, &self.issuer).unwrap();
            certified(cert.der().clone(), key.serialize_der())
        }

        /// The CA certificate with its own key, as a signer.
        pub fn as_signer(&self) -> CertifiedKey {
            certified(self.issuer.der().clone(), self.key.clone())
        }
    }

    fn certified(cert: CertificateDer<'static>, pkcs8: Vec<u8>) -> CertifiedKey {
        CertifiedKey::from_der(
            vec![cert],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pkcs8)),
            &rustls::crypto::aws_lc_rs::default_provider(),
        )
        .unwrap()
    }

    /// `headers` with the signature of `body` by `key` in them.
    pub fn signed(key: &CertifiedKey, kind: Signed, nonce: &str, body: &[u8]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in sign(key, kind, nonce, body).unwrap() {
            headers.insert(name, value);
        }
        headers
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{signed, TestCa};
    use super::*;
    use std::time::Duration;

    const NAME: &str = "portail-controller.portail-system.svc";
    const NONCE: &str = "bm9uY2Ugb2YgdGhpcyBwb2xs";

    fn verifier(ca: &TestCa) -> AnswerVerifier {
        AnswerVerifier::new(ca.roots(), NAME).unwrap()
    }

    #[test]
    fn a_signed_snapshot_verifies_and_a_tampered_one_does_not() {
        let ca = TestCa::new();
        let key = ca.leaf(&[NAME]);
        let body = br#"{"version":"v1"}"#;
        let headers = signed(&key, Signed::Snapshot, NONCE, body);
        let now = SystemTime::now();
        verifier(&ca)
            .verify(Signed::Snapshot, NONCE, &headers, body, now)
            .unwrap();

        let tampered = br#"{"version":"v2"}"#;
        let refused = verifier(&ca)
            .verify(Signed::Snapshot, NONCE, &headers, tampered, now)
            .unwrap_err();
        assert!(refused.contains("bad signature"), "{refused}");
    }

    /// An answer signed for an earlier poll, replayed: its nonce is not
    /// this poll's.
    #[test]
    fn an_answer_to_another_poll_is_refused() {
        let ca = TestCa::new();
        let body = br#"{"version":"v1"}"#;
        let headers = signed(&ca.leaf(&[NAME]), Signed::Snapshot, NONCE, body);
        let refused = verifier(&ca)
            .verify(
                Signed::Snapshot,
                &new_nonce(),
                &headers,
                body,
                SystemTime::now(),
            )
            .unwrap_err();
        assert!(refused.contains("bad signature"), "{refused}");
    }

    /// The context in the message: one kind's signature is no other's,
    /// though the bytes are the same.
    #[test]
    fn a_signature_vouches_for_one_kind() {
        let ca = TestCa::new();
        let key = ca.leaf(&[NAME]);
        let kinds = [
            Signed::Snapshot,
            Signed::Gone,
            Signed::Unchanged,
            Signed::Pending,
            Signed::Leave,
        ];
        for signed_as in kinds {
            let headers = signed(&key, signed_as, NONCE, b"");
            for kind in kinds {
                let verified = verifier(&ca).verify(kind, NONCE, &headers, b"", SystemTime::now());
                assert_eq!(
                    verified.is_ok(),
                    kind == signed_as,
                    "{signed_as:?} as {kind:?}"
                );
            }
        }
    }

    /// Another CA's leaf, an expired leaf, one not yet valid, a leaf for
    /// another name, one for client auth only, the CA itself: each refused
    /// for its certificate, though its signature is good.
    #[test]
    fn the_signer_must_be_a_controller_serving_leaf_valid_now() {
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
            ("client auth only", ca.client_leaf(&[NAME])),
            ("the CA itself", ca.as_signer()),
        ];
        for (why, key) in cases {
            let headers = signed(&key, Signed::Snapshot, NONCE, body);
            let refused = verifier(&ca)
                .verify(Signed::Snapshot, NONCE, &headers, body, now)
                .unwrap_err();
            assert!(refused.contains("signer certificate"), "{why}: {refused}");
        }
    }

    #[test]
    fn an_answer_without_its_headers_is_unsigned() {
        let ca = TestCa::new();
        let body = b"{}";
        let headers = signed(&ca.leaf(&[NAME]), Signed::Snapshot, NONCE, body);
        for missing in [SIGNATURE_HEADER, SIGNER_HEADER] {
            let mut partial = headers.clone();
            partial.remove(missing);
            let refused = verifier(&ca)
                .verify(Signed::Snapshot, NONCE, &partial, body, SystemTime::now())
                .unwrap_err();
            assert!(refused.contains(missing), "{refused}");
        }
        let mut garbled = headers;
        garbled.insert(SIGNATURE_HEADER, HeaderValue::from_static("not base64!"));
        assert!(verifier(&ca)
            .verify(Signed::Snapshot, NONCE, &garbled, body, SystemTime::now())
            .is_err());
    }

    #[test]
    fn a_gone_answer_is_for_this_gateway() {
        let gateway = GatewayKey::new("ns", "gw");
        let body = |gateway: &GatewayKey| {
            serde_json::to_vec(&Gone {
                gateway: gateway.clone(),
            })
            .unwrap()
        };
        Gone::check(&body(&gateway), &gateway).unwrap();
        for refused in [
            body(&GatewayKey::new("ns", "other")),
            b"gateway not served\n".to_vec(),
        ] {
            assert!(
                Gone::check(&refused, &gateway).is_err(),
                "{}",
                String::from_utf8_lossy(&refused)
            );
        }
    }

    #[test]
    fn nonces_are_sixteen_fresh_bytes() {
        let (a, b) = (new_nonce(), new_nonce());
        assert_ne!(a, b);
        assert!(valid_nonce(&a) && valid_nonce(&b));
        let fifteen = STANDARD.encode([0u8; 15]);
        let seventeen = STANDARD.encode([0u8; 17]);
        for bad in ["", "not base64!", fifteen.as_str(), seventeen.as_str()] {
            assert!(!valid_nonce(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_ca_without_a_certificate_is_refused() {
        let error = ca_roots(b"no pem", "controller CA").unwrap_err();
        assert!(error.to_string().contains("no certificate"), "{error:#}");
    }
}
