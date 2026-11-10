//! The cluster's ServiceAccount issuer keys, so a sync token is verified
//! before any TokenReview: a forged one is refused without spending a review,
//! and reviews are then budgeted by the ServiceAccount the token proves, not
//! by the address it came from. Data planes polling through the apiserver
//! all share one address, so no other budget keeps one namespace from
//! delaying another's configuration.
//!
//! Without the keys (discovery off, or no RBAC on its URLs) tokens go to
//! TokenReview unverified, as before, and the controller does not claim
//! isolation.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use aws_lc_rs::signature::{
    self, EcdsaVerificationAlgorithm, RsaPublicKeyComponents, UnparsedPublicKey,
};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use kube::Client;
use serde_json::Value;
use tokio::sync::{watch, Mutex};
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use super::auth::ServiceAccount;
use crate::logging::{info, warn};

const DISCOVERY: &str = "/.well-known/openid-configuration";
const JWKS: &str = "/openid/v1/jwks";
/// An unknown key id reads the keys again at most this often: a rotation
/// is seen within it, and forged ids cost one read per period at most.
const REFETCH_EVERY: Duration = Duration::from_secs(30);
/// Re-read while the keys are unknown: RBAC granted later turns them on.
const RETRY_EVERY: Duration = Duration::from_secs(60);
/// Re-read while known: a rotation without new tokens yet costs nothing.
const REFRESH_EVERY: Duration = Duration::from_secs(600);
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Kubernetes' own validation leeway on nbf, for clock skew between the
/// apiserver that issued the token and this controller.
const NBF_LEEWAY_SECS: f64 = 60.0;

/// A token as its signature proves it: the ServiceAccount and the pod it is
/// bound to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Verified {
    pub account: ServiceAccount,
    pub pod_uid: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verification {
    /// No keys: the token goes to TokenReview unverified.
    Off,
    Verified(Verified),
    /// Not one the issuer signed for us, or not a live pod-bound token.
    Refused,
}

enum Key {
    Rsa {
        n: Vec<u8>,
        e: Vec<u8>,
    },
    Ec {
        alg: &'static str,
        verifier: &'static EcdsaVerificationAlgorithm,
        point: Vec<u8>,
    },
}

impl Key {
    fn verify(&self, alg: &str, message: &[u8], sig: &[u8]) -> bool {
        match self {
            Key::Rsa { n, e } => {
                alg == "RS256"
                    && RsaPublicKeyComponents { n, e }
                        .verify(&signature::RSA_PKCS1_2048_8192_SHA256, message, sig)
                        .is_ok()
            }
            Key::Ec {
                alg: want,
                verifier,
                point,
            } => {
                alg == *want
                    && UnparsedPublicKey::new(*verifier, point)
                        .verify(message, sig)
                        .is_ok()
            }
        }
    }
}

struct Keys {
    issuer: String,
    by_kid: HashMap<String, Key>,
}

pub(crate) struct IssuerKeys {
    client: Option<Client>,
    keys: RwLock<Option<Keys>>,
    /// The last read's start, for the bounded refetch; held across it so
    /// concurrent unknown ids wait on one read.
    read_at: Mutex<Option<Instant>>,
    active: watch::Sender<bool>,
    warned: AtomicBool,
}

impl IssuerKeys {
    pub(crate) fn new(client: Client) -> Self {
        Self::with_client(Some(client))
    }

    /// Never verifies: tokens go to TokenReview unverified.
    #[cfg(test)]
    pub(crate) fn off() -> Self {
        Self::with_client(None)
    }

    fn with_client(client: Option<Client>) -> Self {
        Self {
            client,
            keys: RwLock::new(None),
            read_at: Mutex::new(None),
            active: watch::channel(false).0,
            warned: AtomicBool::new(false),
        }
    }

    /// Whether tokens are verified locally; moves when the keys turn on or
    /// off.
    pub(crate) fn watch(&self) -> watch::Receiver<bool> {
        self.active.subscribe()
    }

    /// Read the keys now, then on a schedule, until `shutdown`.
    pub(crate) async fn run(self: std::sync::Arc<Self>, shutdown: CancellationToken) {
        loop {
            self.read().await;
            let wait = if *self.active.borrow() {
                REFRESH_EVERY
            } else {
                RETRY_EVERY
            };
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Read the keys, keeping the last ones when a read fails.
    pub(crate) async fn read(&self) {
        let mut read_at = self.read_at.lock().await;
        *read_at = Some(Instant::now());
        self.read_now().await;
    }

    async fn read_now(&self) {
        let Some(client) = &self.client else { return };
        match tokio::time::timeout(READ_TIMEOUT, fetch(client)).await {
            Ok(Ok(keys)) => {
                let count = keys.by_kid.len();
                *self.keys.write().unwrap_or_else(|p| p.into_inner()) = Some(keys);
                if !self.active.send_replace(true) {
                    info!(
                        "Sync tokens are verified against the cluster's ServiceAccount issuer keys ({} keys)",
                        count
                    );
                }
            }
            Ok(Err(why)) => self.unread(&why.to_string()),
            Err(_) => self.unread("no answer in time"),
        }
    }

    fn unread(&self, why: &str) {
        if self
            .keys
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
        {
            return;
        }
        if !self.warned.swap(true, Ordering::Relaxed) {
            warn!(
                "Sync tokens are not verified locally: the ServiceAccount issuer keys cannot be read ({}); \
                 forged tokens sent through one address can delay other Gateways' first poll",
                why
            );
        }
    }

    /// `token` checked for `audience`: signature, issuer, audience, expiry
    /// and pod binding. An unknown key id reads the keys again, at most
    /// once per [`REFETCH_EVERY`].
    pub(crate) async fn verify(&self, token: &str, audience: &str) -> Verification {
        let Some(parts) = Parts::of(token) else {
            return Verification::Refused;
        };
        match self.check(&parts, audience) {
            Checked::UnknownKid => {}
            Checked::Done(v) => return v,
        }
        {
            let mut read_at = self.read_at.lock().await;
            if read_at.is_some_and(|at| at.elapsed() < REFETCH_EVERY) {
                return Verification::Refused;
            }
            *read_at = Some(Instant::now());
            self.read_now().await;
        }
        match self.check(&parts, audience) {
            Checked::Done(v) => v,
            Checked::UnknownKid => Verification::Refused,
        }
    }

    fn check(&self, parts: &Parts, audience: &str) -> Checked {
        let keys = self.keys.read().unwrap_or_else(|p| p.into_inner());
        let Some(keys) = keys.as_ref() else {
            return Checked::Done(Verification::Off);
        };
        let Some(key) = keys.by_kid.get(&parts.kid) else {
            return Checked::UnknownKid;
        };
        if !key.verify(&parts.alg, parts.signed.as_bytes(), &parts.signature) {
            return Checked::Done(Verification::Refused);
        }
        Checked::Done(match claims(&parts.claims, &keys.issuer, audience, now()) {
            Some(verified) => Verification::Verified(verified),
            None => Verification::Refused,
        })
    }
}

enum Checked {
    Done(Verification),
    UnknownKid,
}

/// A JWS compact token, split; nothing in it is trusted yet.
struct Parts {
    alg: String,
    kid: String,
    claims: Value,
    signed: String,
    signature: Vec<u8>,
}

impl Parts {
    fn of(token: &str) -> Option<Self> {
        let (signed, signature) = token.rsplit_once('.')?;
        let (header, payload) = signed.split_once('.')?;
        let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header).ok()?).ok()?;
        let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
        Some(Self {
            alg: header["alg"].as_str()?.to_string(),
            kid: header["kid"].as_str().unwrap_or_default().to_string(),
            claims,
            signed: signed.to_string(),
            signature: URL_SAFE_NO_PAD.decode(signature).ok()?,
        })
    }
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

/// The claims of a signed token, if they are a live pod-bound
/// ServiceAccount token of `issuer` for `audience` at `now`.
fn claims(c: &Value, issuer: &str, audience: &str, now: f64) -> Option<Verified> {
    let for_us = match &c["aud"] {
        Value::String(aud) => aud == audience,
        Value::Array(auds) => auds.iter().any(|aud| aud == audience),
        _ => false,
    };
    let live = c["exp"].as_f64().is_some_and(|exp| exp > now)
        && c["nbf"]
            .as_f64()
            .is_none_or(|nbf| nbf <= now + NBF_LEEWAY_SECS);
    if !for_us || !live || c["iss"].as_str() != Some(issuer) {
        return None;
    }
    let (namespace, name) = c["sub"]
        .as_str()?
        .strip_prefix("system:serviceaccount:")?
        .split_once(':')?;
    let k8s = &c["kubernetes.io"];
    let pod_uid = k8s["pod"]["uid"].as_str().filter(|uid| !uid.is_empty())?;
    if k8s["namespace"].as_str() != Some(namespace)
        || k8s["serviceaccount"]["name"].as_str() != Some(name)
        || name.contains(':')
    {
        return None;
    }
    Some(Verified {
        account: ServiceAccount {
            namespace: namespace.to_string(),
            name: name.to_string(),
        },
        pod_uid: pod_uid.to_string(),
    })
}

async fn fetch(client: &Client) -> anyhow::Result<Keys> {
    let get = |path: &str| http::Request::get(path).body(Vec::new());
    let discovery: Value = client.request(get(DISCOVERY)?).await?;
    let issuer = discovery["issuer"]
        .as_str()
        .filter(|i| !i.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{DISCOVERY} names no issuer"))?
        .to_string();
    let jwks: Value = client.request(get(JWKS)?).await?;
    let by_kid: HashMap<String, Key> = jwks["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(jwk)
        .collect();
    if by_kid.is_empty() {
        anyhow::bail!("{JWKS} holds no key portail can verify with");
    }
    Ok(Keys { issuer, by_kid })
}

/// One JWK: RSA, or EC on P-256, P-384 or P-521, as the apiserver signs.
fn jwk(k: &Value) -> Option<(String, Key)> {
    let kid = k["kid"].as_str().unwrap_or_default().to_string();
    let field = |name: &str| URL_SAFE_NO_PAD.decode(k[name].as_str()?).ok();
    let key = match k["kty"].as_str()? {
        "RSA" => Key::Rsa {
            n: unsigned(field("n")?),
            e: unsigned(field("e")?),
        },
        "EC" => {
            let (alg, verifier, len): (_, &'static EcdsaVerificationAlgorithm, usize) =
                match k["crv"].as_str()? {
                    "P-256" => ("ES256", &signature::ECDSA_P256_SHA256_FIXED, 32),
                    "P-384" => ("ES384", &signature::ECDSA_P384_SHA384_FIXED, 48),
                    "P-521" => ("ES512", &signature::ECDSA_P521_SHA512_FIXED, 66),
                    _ => return None,
                };
            let (x, y) = (field("x")?, field("y")?);
            if x.len() != len || y.len() != len {
                return None;
            }
            let mut point = vec![4];
            point.extend(x);
            point.extend(y);
            Key::Ec {
                alg,
                verifier,
                point,
            }
        }
        _ => return None,
    };
    Some((kid, key))
}

/// A big-endian integer without leading zero bytes, which aws-lc refuses.
fn unsigned(mut bytes: Vec<u8>) -> Vec<u8> {
    let zeros = bytes.iter().take_while(|b| **b == 0).count();
    bytes.drain(..zeros.min(bytes.len().saturating_sub(1)));
    bytes
}

#[cfg(test)]
pub(super) mod testing {
    use super::*;
    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
    use serde_json::json;

    pub const ISSUER: &str = "https://kubernetes.default.svc.cluster.local";
    pub const DISCOVERY_PATH: &str = DISCOVERY;
    pub const JWKS_PATH: &str = JWKS;
    /// 2100-01-01, as auth's testing tokens use.
    const EXP: i64 = 4_102_444_800;

    fn b64(bytes: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(bytes)
    }

    /// An ES256 key as the apiserver signs with, under key id `kid`.
    pub struct Signer {
        key: EcdsaKeyPair,
        pub kid: String,
    }

    impl Signer {
        pub fn new(kid: &str) -> Self {
            Self {
                key: EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING).unwrap(),
                kid: kid.into(),
            }
        }

        pub fn jwk(&self) -> Value {
            let point = self.key.public_key().as_ref();
            json!({
                "kty": "EC", "crv": "P-256", "use": "sig", "alg": "ES256", "kid": self.kid,
                "x": b64(&point[1..33]), "y": b64(&point[33..]),
            })
        }

        pub fn sign(&self, claims: &Value) -> String {
            let header = json!({"alg": "ES256", "kid": self.kid});
            let signed = format!(
                "{}.{}",
                b64(&serde_json::to_vec(&header).unwrap()),
                b64(&serde_json::to_vec(claims).unwrap())
            );
            let sig = self
                .key
                .sign(&SystemRandom::new(), signed.as_bytes())
                .unwrap();
            format!("{signed}.{}", b64(sig.as_ref()))
        }
    }

    /// A projected token's claims for pod `pod` of ServiceAccount `ns:sa`,
    /// `n` telling tokens apart.
    pub fn pod_claims(ns: &str, sa: &str, pod: &str, audience: &str, n: u32) -> Value {
        json!({
            "aud": [audience],
            "iss": ISSUER,
            "sub": format!("system:serviceaccount:{ns}:{sa}"),
            "exp": EXP,
            "nbf": now() as i64 - 10,
            "jti": n,
            "kubernetes.io": {
                "namespace": ns,
                "pod": {"name": pod, "uid": pod},
                "serviceaccount": {"name": sa, "uid": "sa-uid"},
            },
        })
    }

    /// `api` serving the discovery document and `signer`'s key.
    pub fn serve(api: &super::super::testkit::MockApi, signer: &Signer) {
        api.reply_to(
            DISCOVERY,
            200,
            json!({"issuer": ISSUER, "jwks_uri": format!("{ISSUER}{JWKS}")}),
        );
        api.reply_to(JWKS, 200, json!({"keys": [signer.jwk()]}));
    }

    /// Keys read from `api`.
    pub async fn read_from(api: &super::super::testkit::MockApi) -> std::sync::Arc<IssuerKeys> {
        let keys = IssuerKeys::new(api.client());
        keys.read().await;
        std::sync::Arc::new(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::MockApi;
    use super::testing::*;
    use super::*;
    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::rsa::{KeySize, PublicKeyComponents};
    use aws_lc_rs::signature::{KeyPair, RsaKeyPair, RSA_PKCS1_SHA256};
    use serde_json::json;

    const AUD: &str = "portail-controller.portail-system.svc";

    fn verified(ns: &str, sa: &str, pod: &str) -> Verification {
        Verification::Verified(Verified {
            account: ServiceAccount {
                namespace: ns.into(),
                name: sa.into(),
            },
            pod_uid: pod.into(),
        })
    }

    fn jwks_reads(api: &MockApi) -> usize {
        api.requests()
            .iter()
            .filter(|r| r.path == JWKS_PATH)
            .count()
    }

    #[tokio::test]
    async fn a_token_the_issuer_signed_for_us_is_verified() {
        let api = MockApi::default();
        let signer = Signer::new("k1");
        serve(&api, &signer);
        let keys = read_from(&api).await;
        assert!(*keys.watch().borrow());
        let token = signer.sign(&pod_claims("apps", "portail-web", "pod-1", AUD, 0));
        assert_eq!(
            keys.verify(&token, AUD).await,
            verified("apps", "portail-web", "pod-1")
        );
    }

    #[tokio::test]
    async fn anything_else_is_refused() {
        let api = MockApi::default();
        let signer = Signer::new("k1");
        serve(&api, &signer);
        let keys = read_from(&api).await;
        let claims = pod_claims("apps", "portail-web", "pod-1", AUD, 0);

        // Another key under the same id: a forgery.
        let forger = Signer::new("k1");
        assert_eq!(
            keys.verify(&forger.sign(&claims), AUD).await,
            Verification::Refused
        );
        // A signature swapped onto other claims.
        let good = signer.sign(&claims);
        let (_, sig) = good.rsplit_once('.').unwrap();
        let other = signer.sign(&pod_claims("apps", "portail-db", "pod-1", AUD, 0));
        let (signed, _) = other.rsplit_once('.').unwrap();
        assert_eq!(
            keys.verify(&format!("{signed}.{sig}"), AUD).await,
            Verification::Refused
        );

        let mut edits: Vec<(&str, Value)> = vec![
            ("aud", json!(["someone-else"])),
            ("iss", json!("https://elsewhere")),
            ("exp", json!(now() as i64 - 1)),
            ("nbf", json!(now() as i64 + 600)),
        ];
        let mut unbound = claims.clone();
        unbound["kubernetes.io"]
            .as_object_mut()
            .unwrap()
            .remove("pod");
        let mut other_ns = claims.clone();
        other_ns["kubernetes.io"]["namespace"] = json!("db");
        for (field, value) in edits.drain(..) {
            let mut c = claims.clone();
            c[field] = value;
            assert_eq!(
                keys.verify(&signer.sign(&c), AUD).await,
                Verification::Refused,
                "{field}"
            );
        }
        assert_eq!(
            keys.verify(&signer.sign(&unbound), AUD).await,
            Verification::Refused,
            "no pod"
        );
        assert_eq!(
            keys.verify(&signer.sign(&other_ns), AUD).await,
            Verification::Refused,
            "namespaces differ"
        );
    }

    #[tokio::test]
    async fn an_rs256_key_verifies_its_tokens() {
        let pair = RsaKeyPair::generate(KeySize::Rsa2048).unwrap();
        let PublicKeyComponents { n, e } = PublicKeyComponents::<Vec<u8>>::from(pair.public_key());
        let api = MockApi::default();
        api.reply_to(DISCOVERY_PATH, 200, json!({"issuer": ISSUER}));
        api.reply_to(
            JWKS_PATH,
            200,
            json!({"keys": [{"kty": "RSA", "kid": "r1", "n": URL_SAFE_NO_PAD.encode(&n), "e": URL_SAFE_NO_PAD.encode(&e)}]}),
        );
        let keys = read_from(&api).await;
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","kid":"r1"}"#);
        let claims = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&pod_claims("apps", "portail-web", "pod-1", AUD, 0)).unwrap(),
        );
        let signed = format!("{header}.{claims}");
        let mut sig = vec![0; pair.public_modulus_len()];
        pair.sign(
            &RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            signed.as_bytes(),
            &mut sig,
        )
        .unwrap();
        let token = format!("{signed}.{}", URL_SAFE_NO_PAD.encode(&sig));
        assert_eq!(
            keys.verify(&token, AUD).await,
            verified("apps", "portail-web", "pod-1")
        );
        // The algorithm is the key's: an RSA key never verifies an ES256 header.
        let es = token.replacen(
            &header,
            &URL_SAFE_NO_PAD.encode(br#"{"alg":"ES256","kid":"r1"}"#),
            1,
        );
        assert_eq!(keys.verify(&es, AUD).await, Verification::Refused);
    }

    /// A key rotation is read on the first unknown id; forged ids read the
    /// keys once per period at most.
    #[tokio::test(start_paused = true)]
    async fn an_unknown_key_id_reads_the_keys_at_a_bounded_rate() {
        let api = MockApi::default();
        let old = Signer::new("old");
        serve(&api, &old);
        let keys = read_from(&api).await;
        assert_eq!(jwks_reads(&api), 1);
        tokio::time::advance(REFETCH_EVERY).await;

        let rotated = Signer::new("new");
        serve(&api, &rotated);
        let token = rotated.sign(&pod_claims("apps", "portail-web", "pod-1", AUD, 0));
        assert_eq!(
            keys.verify(&token, AUD).await,
            verified("apps", "portail-web", "pod-1")
        );
        assert_eq!(jwks_reads(&api), 2, "the rotation is read");

        let stranger = Signer::new("forged");
        for n in 0..50 {
            let forged = stranger.sign(&pod_claims("apps", "portail-web", "pod-1", AUD, n));
            assert_eq!(keys.verify(&forged, AUD).await, Verification::Refused);
        }
        assert_eq!(jwks_reads(&api), 2, "within the period, no read");
        tokio::time::advance(REFETCH_EVERY).await;
        let forged = stranger.sign(&pod_claims("apps", "portail-web", "pod-1", AUD, 99));
        assert_eq!(keys.verify(&forged, AUD).await, Verification::Refused);
        assert_eq!(jwks_reads(&api), 3, "one read per period");
    }

    /// Unreadable keys verify nothing: tokens go to TokenReview as before,
    /// and the controller does not claim isolation.
    #[tokio::test]
    async fn without_keys_tokens_are_not_verified() {
        let api = MockApi::default();
        api.reply_to(DISCOVERY_PATH, 200, json!({"issuer": ISSUER}));
        api.reply_to(JWKS_PATH, 403, json!({"kind": "Status", "code": 403}));
        let keys = read_from(&api).await;
        assert!(!*keys.watch().borrow());
        let token = Signer::new("k").sign(&pod_claims("apps", "portail-web", "pod-1", AUD, 0));
        assert_eq!(keys.verify(&token, AUD).await, Verification::Off);
    }
}
