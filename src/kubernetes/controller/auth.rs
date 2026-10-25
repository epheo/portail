//! Sync request authentication and authorization. A local pre-check of the
//! token's unverified claims refuses junk without an apiserver call; then
//! TokenReview with the controller audience, the authority, its verdict
//! cached by token hash; then the ServiceAccount must be
//! `naming::resource_name` of the requested Gateway, in its namespace.
//!
//! Unbounded, each unknown token would cost a TokenReview, and any pod
//! reaching the Service could make the controller amplify junk onto the
//! apiserver. Rejections are cached too and reviews in flight are capped; a
//! forged but well-formed token passes the pre-check, and the cap is what
//! bounds it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use k8s_openapi::api::authentication::v1::{TokenReview, TokenReviewSpec, TokenReviewStatus};
use kube::api::{Api, PostParams};
use kube::Client;
use serde_json::Value;
use tokio::sync::Semaphore;
use tokio::time::{Duration, Instant};

use super::reports::InstanceId;
use crate::kubernetes::naming;
use crate::logging::{debug, warn};
use crate::metrics::{self, SyncAuthStage, TokenReviewResult, METRICS};
use crate::sync::{GatewayKey, HOLD};

/// Projected tokens are about 1 KiB; the cap bounds the decode.
const MAX_TOKEN_BYTES: usize = 16 * 1024;
/// 8% of the apiserver's default `--max-mutating-requests-inflight` (200; a
/// TokenReview is a create). A review takes milliseconds: 16 in flight
/// still clear hundreds of reconnects per second after a failover.
const MAX_REVIEWS_IN_FLIGHT: usize = 16;
/// How long a verdict is kept, either way. Accepted: a deleted pod's token
/// stops working within it. Rejected: such a token never turns valid, so
/// it only bounds memory and how long a replay is answered from memory.
const CACHE_TTL: Duration = HOLD;
/// The poll's hold that follows still ends inside the client's request
/// timeout of two holds.
const REVIEW_TIMEOUT: Duration = HOLD;

const SA_PREFIX: &str = "system:serviceaccount:";
/// The TokenReview extra a pod-bound token carries.
const POD_UID: &str = "authentication.kubernetes.io/pod-uid";
/// Where kubelet mounts this process's own API token.
const OWN_TOKEN: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";

/// Why a request is refused, as its status code.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// 401: no valid controller-audience token of this Gateway's
    /// ServiceAccount.
    Unauthenticated,
    /// 403: an identity that may not sync this Gateway.
    Forbidden,
    /// 503: TokenReviews in flight are at their cap.
    Throttled,
    /// 503: the TokenReview got no answer.
    Unavailable,
}

/// A ServiceAccount, as `system:serviceaccount:<namespace>:<name>` names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ServiceAccount {
    pub namespace: String,
    pub name: String,
}

impl ServiceAccount {
    fn from_username(username: &str) -> Option<Self> {
        let (namespace, name) = username.strip_prefix(SA_PREFIX)?.split_once(':')?;
        (!namespace.is_empty() && !name.is_empty() && !name.contains(':')).then(|| Self {
            namespace: namespace.to_string(),
            name: name.to_string(),
        })
    }

    /// The one a Gateway's data planes run as, Managed or Unmanaged.
    pub(crate) fn of(gateway: &GatewayKey) -> Self {
        Self {
            namespace: gateway.namespace.clone(),
            name: naming::resource_name(&gateway.name),
        }
    }
}

/// A JWT's claims, unverified: three base64url parts, JSON in the first two.
fn claims(token: &str) -> Option<Value> {
    if token.len() > MAX_TOKEN_BYTES {
        return None;
    }
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let json = |part: &str| -> Option<Value> {
        let value: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).ok()?).ok()?;
        value.is_object().then_some(value)
    };
    json(header)?;
    if signature.is_empty() || URL_SAFE_NO_PAD.decode(signature).is_err() {
        return None;
    }
    json(payload)
}

/// The ServiceAccount `token` claims to be, if it is a JWT for `audience`
/// that has not expired at `now`. Reject junk only: TokenReview decides.
fn precheck(token: &str, audience: &str, now: SystemTime) -> Option<ServiceAccount> {
    let claims = claims(token)?;
    let for_us = match &claims["aud"] {
        Value::String(aud) => aud == audience,
        Value::Array(auds) => auds.iter().any(|aud| aud == audience),
        _ => false,
    };
    let now = now.duration_since(UNIX_EPOCH).ok()?.as_secs_f64();
    let live = claims["exp"].as_f64().is_some_and(|exp| exp > now);
    if !for_us || !live {
        return None;
    }
    ServiceAccount::from_username(claims["sub"].as_str()?)
}

/// This process's ServiceAccount, from its own API token's claims; `None`
/// out of a cluster.
pub(crate) fn own_service_account() -> Option<ServiceAccount> {
    let token = std::fs::read_to_string(OWN_TOKEN).ok()?;
    ServiceAccount::from_username(claims(token.trim())?["sub"].as_str()?)
}

/// What a TokenReview said a token is.
#[derive(Clone, Debug)]
struct Reviewed {
    account: ServiceAccount,
    /// `None` for a token not bound to a pod.
    pod_uid: Option<String>,
}

type TokenHash = [u8; 32];

#[derive(Default)]
struct Verdicts {
    /// When each verdict was taken; `None`: rejected.
    by_token: HashMap<TokenHash, (Instant, Option<Reviewed>)>,
    swept: Option<Instant>,
}

pub(crate) struct Authenticator {
    reviews: Api<TokenReview>,
    audience: String,
    /// Never authorized, whatever Gateway is asked for: the controller's
    /// own ServiceAccount and the one a Gateway named after the controller
    /// Service, in its namespace, would run as.
    refused: Vec<ServiceAccount>,
    in_flight: Semaphore,
    verdicts: Mutex<Verdicts>,
}

impl Authenticator {
    /// For the controller of Service `service` in `namespace`, running as
    /// `own` when known.
    pub(crate) fn new(
        client: Client,
        namespace: &str,
        service: &str,
        own: Option<ServiceAccount>,
    ) -> Self {
        let reserved = ServiceAccount {
            namespace: namespace.to_string(),
            name: service.to_string(),
        };
        Self {
            reviews: Api::all(client),
            audience: naming::controller_audience(service, namespace),
            refused: std::iter::once(reserved).chain(own).collect(),
            in_flight: Semaphore::new(MAX_REVIEWS_IN_FLIGHT),
            verdicts: Mutex::default(),
        }
    }

    /// The first stage, on the request headers alone: the ServiceAccount
    /// `token` claims to be.
    pub(crate) fn precheck(&self, token: &str) -> Result<ServiceAccount, Refusal> {
        precheck(token, &self.audience, SystemTime::now())
            .ok_or_else(|| rejected(SyncAuthStage::Precheck, Refusal::Unauthenticated))
    }

    /// The instance `token` belongs to, if it may sync `gateway`. `claimed`
    /// is what [`Self::precheck`] read from it.
    pub(crate) async fn authenticate(
        &self,
        claimed: &ServiceAccount,
        token: &str,
        gateway: &GatewayKey,
    ) -> Result<InstanceId, Refusal> {
        let expected = ServiceAccount::of(gateway);
        // Computed from the request: the store is not read for a stranger,
        // and the same 401 for every Gateway name is no oracle of them.
        if *claimed != expected {
            return Err(rejected(SyncAuthStage::Precheck, Refusal::Unauthenticated));
        }
        if self.refused.contains(&expected) {
            return Err(rejected(SyncAuthStage::Authz, Refusal::Forbidden));
        }
        let hash = token_hash(token);
        let reviewed = match self.cached(&hash) {
            Some(Some(reviewed)) => reviewed,
            Some(None) => {
                return Err(rejected(
                    SyncAuthStage::NegativeCache,
                    Refusal::Unauthenticated,
                ))
            }
            None => self.review(hash, token).await?,
        };
        if reviewed.account != expected {
            return Err(rejected(SyncAuthStage::Authz, Refusal::Forbidden));
        }
        // A token not bound to a pod names no instance to report as.
        match reviewed.pod_uid {
            Some(uid) => Ok(InstanceId(uid)),
            None => Err(rejected(SyncAuthStage::Authz, Refusal::Forbidden)),
        }
    }

    fn cached(&self, hash: &TokenHash) -> Option<Option<Reviewed>> {
        let verdicts = self.verdicts.lock().unwrap_or_else(|p| p.into_inner());
        let (at, verdict) = verdicts.by_token.get(hash)?;
        (at.elapsed() < CACHE_TTL).then(|| verdict.clone())
    }

    fn remember(&self, hash: TokenHash, verdict: Option<Reviewed>) {
        let mut verdicts = self.verdicts.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        // One sweep per TTL keeps the map within two TTLs of tokens.
        if verdicts.swept.is_none_or(|at| now - at >= CACHE_TTL) {
            verdicts.by_token.retain(|_, (at, _)| now - *at < CACHE_TTL);
            verdicts.swept = Some(now);
        }
        verdicts.by_token.insert(hash, (now, verdict));
    }

    async fn review(&self, hash: TokenHash, token: &str) -> Result<Reviewed, Refusal> {
        let Ok(_permit) = self.in_flight.try_acquire() else {
            METRICS.sync_throttled_total.inc();
            return Err(Refusal::Throttled);
        };
        let request = TokenReview {
            spec: TokenReviewSpec {
                audiences: Some(vec![self.audience.clone()]),
                token: Some(token.to_string()),
            },
            ..Default::default()
        };
        let status = match tokio::time::timeout(
            REVIEW_TIMEOUT,
            self.reviews.create(&PostParams::default(), &request),
        )
        .await
        {
            Ok(Ok(review)) => review.status.unwrap_or_default(),
            // Not cached: the token may be fine.
            Ok(Err(e)) => return Err(review_failed(&e.to_string())),
            Err(_) => return Err(review_failed("no answer within the hold")),
        };
        let verdict = self.verdict(&status);
        self.remember(hash, verdict.clone());
        match verdict {
            Some(reviewed) => {
                metrics::record_tokenreview(TokenReviewResult::Authenticated);
                Ok(reviewed)
            }
            None => {
                metrics::record_tokenreview(TokenReviewResult::Rejected);
                debug!("TokenReview rejected a sync token: {:?}", status.error);
                Err(rejected(
                    SyncAuthStage::TokenReview,
                    Refusal::Unauthenticated,
                ))
            }
        }
    }

    fn verdict(&self, status: &TokenReviewStatus) -> Option<Reviewed> {
        if status.authenticated != Some(true) {
            return None;
        }
        // An authenticator that ignores audiences returns none of ours: the
        // token was not checked against it.
        if !status
            .audiences
            .iter()
            .flatten()
            .any(|a| *a == self.audience)
        {
            return None;
        }
        let user = status.user.as_ref()?;
        let account = ServiceAccount::from_username(user.username.as_deref()?)?;
        let pod_uid = user
            .extra
            .as_ref()
            .and_then(|extra| extra.get(POD_UID))
            .and_then(|values| values.first())
            .cloned();
        Some(Reviewed { account, pod_uid })
    }
}

fn rejected(stage: SyncAuthStage, refusal: Refusal) -> Refusal {
    metrics::record_sync_auth_rejected(stage);
    refusal
}

fn review_failed(why: &str) -> Refusal {
    metrics::record_tokenreview(TokenReviewResult::Error);
    warn!("TokenReview failed: {}", why);
    Refusal::Unavailable
}

fn token_hash(token: &str) -> TokenHash {
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, token.as_bytes());
    let mut hash = [0; 32];
    hash.copy_from_slice(digest.as_ref());
    hash
}

/// Tests that count auth metrics hold this: the counters are process-wide.
#[cfg(test)]
pub(super) static METRICS_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
pub(super) mod testing {
    use super::*;
    use serde_json::json;

    pub const NS: &str = "portail-system";
    pub const SERVICE: &str = "portail-controller";
    pub const AUDIENCE: &str = "portail-controller.portail-system.svc";
    pub const REVIEWS: &str = "/apis/authentication.k8s.io/v1/tokenreviews";

    pub fn b64(value: &Value) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap())
    }

    pub fn jwt(claims: Value) -> String {
        format!(
            "{}.{}.{}",
            b64(&json!({"alg": "RS256", "kid": "k"})),
            b64(&claims),
            URL_SAFE_NO_PAD.encode(b"signature")
        )
    }

    pub fn exp_in(secs: i64) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + secs
    }

    /// A projected token's claims for ServiceAccount `ns:sa`, `n` telling
    /// tokens of one account apart.
    pub fn token_of(ns: &str, sa: &str, n: u32) -> String {
        jwt(json!({
            "aud": [AUDIENCE],
            "sub": format!("system:serviceaccount:{ns}:{sa}"),
            "exp": exp_in(3600),
            "jti": n,
        }))
    }

    /// The token of Gateway `ns/name`'s data planes.
    pub fn gateway_token(ns: &str, name: &str, n: u32) -> String {
        token_of(ns, &naming::resource_name(name), n)
    }

    /// A TokenReview answer.
    pub fn review(authenticated: bool, username: &str, pod_uid: Option<&str>) -> Value {
        let mut extra = json!({});
        if let Some(uid) = pod_uid {
            extra[POD_UID] = json!([uid]);
        }
        json!({
            "apiVersion": "authentication.k8s.io/v1",
            "kind": "TokenReview",
            "spec": {},
            "status": {
                "authenticated": authenticated,
                "audiences": [AUDIENCE],
                "user": {"username": username, "extra": extra},
            },
        })
    }

    pub fn authenticator(client: Client) -> Authenticator {
        let own = ServiceAccount {
            namespace: NS.into(),
            name: "portail-ctl".into(),
        };
        Authenticator::new(client, NS, SERVICE, Some(own))
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::MockApi;
    use super::testing::*;
    use super::*;
    use serde_json::json;

    fn reviews(api: &MockApi) -> usize {
        api.requests()
            .iter()
            .filter(|r| r.path.starts_with(REVIEWS))
            .count()
    }

    fn counter(stage: SyncAuthStage) -> u64 {
        metrics::sync_auth_rejected(stage)
    }

    fn sa(ns: &str, name: &str) -> ServiceAccount {
        ServiceAccount {
            namespace: ns.into(),
            name: name.into(),
        }
    }

    #[test]
    fn the_precheck_reads_unverified_claims() {
        let now = SystemTime::now();
        let good = gateway_token("apps", "web", 0);
        assert_eq!(
            precheck(&good, AUDIENCE, now),
            Some(sa("apps", "portail-web"))
        );
        let single_aud = jwt(json!({
            "aud": AUDIENCE, "sub": "system:serviceaccount:a:b", "exp": exp_in(60),
        }));
        assert_eq!(precheck(&single_aud, AUDIENCE, now), Some(sa("a", "b")));

        let refused = [
            ("not a JWT", "junk".to_string()),
            ("two parts", good.rsplit_once('.').unwrap().0.to_string()),
            ("four parts", format!("{good}.x")),
            ("not base64url", good.replacen('.', ".!", 1)),
            (
                "claims not JSON",
                format!("{}.{}.c2ln", b64(&json!({})), URL_SAFE_NO_PAD.encode("x")),
            ),
            (
                "oversized",
                format!("{good}{}", "A".repeat(MAX_TOKEN_BYTES)),
            ),
            (
                "wrong aud",
                jwt(
                    json!({"aud": ["https://kubernetes.default.svc"], "sub": "system:serviceaccount:a:b", "exp": exp_in(60)}),
                ),
            ),
            (
                "no aud",
                jwt(json!({"sub": "system:serviceaccount:a:b", "exp": exp_in(60)})),
            ),
            (
                "user sub",
                jwt(json!({"aud": [AUDIENCE], "sub": "alice", "exp": exp_in(60)})),
            ),
            (
                "sub with extra part",
                jwt(
                    json!({"aud": [AUDIENCE], "sub": "system:serviceaccount:a:b:c", "exp": exp_in(60)}),
                ),
            ),
            (
                "expired",
                jwt(
                    json!({"aud": [AUDIENCE], "sub": "system:serviceaccount:a:b", "exp": exp_in(-1)}),
                ),
            ),
            (
                "no exp",
                jwt(json!({"aud": [AUDIENCE], "sub": "system:serviceaccount:a:b"})),
            ),
        ];
        for (why, token) in refused {
            assert_eq!(precheck(&token, AUDIENCE, now), None, "{why}");
        }
    }

    /// A valid token is reviewed once per hold.
    #[tokio::test(start_paused = true)]
    async fn an_accepted_token_is_reviewed_once_per_hold() {
        let _serial = METRICS_SERIAL.lock().await;
        let api = MockApi::default();
        api.reply_to(
            REVIEWS,
            201,
            review(
                true,
                "system:serviceaccount:apps:portail-web",
                Some("pod-1"),
            ),
        );
        let auth = authenticator(api.client());
        let gw = GatewayKey::new("apps", "web");
        let token = gateway_token("apps", "web", 0);
        let claimed = auth.precheck(&token).unwrap();
        let authenticated = metrics::tokenreview_requests(TokenReviewResult::Authenticated);

        for _ in 0..3 {
            let instance = auth.authenticate(&claimed, &token, &gw).await.unwrap();
            assert_eq!(instance, InstanceId("pod-1".into()));
        }
        assert_eq!(reviews(&api), 1);
        assert_eq!(
            metrics::tokenreview_requests(TokenReviewResult::Authenticated),
            authenticated + 1
        );

        tokio::time::advance(HOLD).await;
        auth.authenticate(&claimed, &token, &gw).await.unwrap();
        assert_eq!(reviews(&api), 2, "the verdict expired with the hold");
    }

    #[tokio::test(start_paused = true)]
    async fn a_rejected_token_is_answered_from_memory_for_a_hold() {
        let _serial = METRICS_SERIAL.lock().await;
        let api = MockApi::default();
        api.reply_to(REVIEWS, 201, review(false, "", None));
        let auth = authenticator(api.client());
        let gw = GatewayKey::new("apps", "web");
        let token = gateway_token("apps", "web", 0);
        let claimed = auth.precheck(&token).unwrap();
        let (by_review, by_cache) = (
            counter(SyncAuthStage::TokenReview),
            counter(SyncAuthStage::NegativeCache),
        );
        let rejected = metrics::tokenreview_requests(TokenReviewResult::Rejected);

        for _ in 0..2 {
            assert_eq!(
                auth.authenticate(&claimed, &token, &gw).await,
                Err(Refusal::Unauthenticated)
            );
        }
        assert_eq!(reviews(&api), 1);
        assert_eq!(counter(SyncAuthStage::TokenReview), by_review + 1);
        assert_eq!(counter(SyncAuthStage::NegativeCache), by_cache + 1);
        assert_eq!(
            metrics::tokenreview_requests(TokenReviewResult::Rejected),
            rejected + 1
        );

        tokio::time::advance(HOLD).await;
        assert_eq!(
            auth.authenticate(&claimed, &token, &gw).await,
            Err(Refusal::Unauthenticated)
        );
        assert_eq!(reviews(&api), 2);
    }

    #[tokio::test]
    async fn a_failed_review_is_not_cached() {
        let _serial = METRICS_SERIAL.lock().await;
        let api = MockApi::default();
        api.reply_to(
            REVIEWS,
            503,
            json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                   "reason": "ServiceUnavailable", "message": "etcd", "code": 503}),
        );
        let auth = authenticator(api.client());
        let gw = GatewayKey::new("apps", "web");
        let token = gateway_token("apps", "web", 0);
        let claimed = auth.precheck(&token).unwrap();
        let errors = metrics::tokenreview_requests(TokenReviewResult::Error);

        for _ in 0..2 {
            assert_eq!(
                auth.authenticate(&claimed, &token, &gw).await,
                Err(Refusal::Unavailable)
            );
        }
        assert_eq!(reviews(&api), 2);
        assert_eq!(
            metrics::tokenreview_requests(TokenReviewResult::Error),
            errors + 2
        );

        api.reply_to(
            REVIEWS,
            201,
            review(
                true,
                "system:serviceaccount:apps:portail-web",
                Some("pod-1"),
            ),
        );
        assert!(auth.authenticate(&claimed, &token, &gw).await.is_ok());
    }

    /// Who may sync which Gateway: only its own ServiceAccount, in its own
    /// namespace, and never the controller's.
    #[tokio::test]
    async fn authorization_matrix() {
        let _serial = METRICS_SERIAL.lock().await;
        let api = MockApi::default();
        let auth = authenticator(api.client());
        let web = GatewayKey::new("apps", "web");

        let cases = [
            // (claimed namespace, account, Gateway, TokenReview username, pod uid, answer)
            (
                "namespace mismatch",
                "other",
                "portail-web",
                &web,
                None,
                Some(Refusal::Unauthenticated),
            ),
            (
                "another Gateway's account",
                "apps",
                "portail-api",
                &web,
                None,
                Some(Refusal::Unauthenticated),
            ),
            (
                "the Gateway's account",
                "apps",
                "portail-web",
                &web,
                Some("system:serviceaccount:apps:portail-web"),
                None,
            ),
            (
                "reviewed as another account",
                "apps",
                "portail-web",
                &web,
                Some("system:serviceaccount:apps:portail-api"),
                Some(Refusal::Forbidden),
            ),
        ];
        for (n, (why, ns, name, gw, username, answer)) in cases.into_iter().enumerate() {
            if let Some(username) = username {
                api.reply_to(REVIEWS, 201, review(true, username, Some("pod-1")));
            }
            let token = token_of(ns, name, n as u32);
            let claimed = auth.precheck(&token).unwrap();
            let got = auth.authenticate(&claimed, &token, gw).await.err();
            assert_eq!(got, answer, "{why}");
        }

        // Refused before any review: the Gateway named after the controller
        // Service in its namespace, and one whose account is the
        // controller's own.
        api.clear();
        let authz = counter(SyncAuthStage::Authz);
        for (gw, n) in [
            (GatewayKey::new(NS, "controller"), 10),
            (GatewayKey::new(NS, "ctl"), 11),
        ] {
            let token = gateway_token(&gw.namespace, &gw.name, n);
            let claimed = auth.precheck(&token).unwrap();
            assert_eq!(
                auth.authenticate(&claimed, &token, &gw).await,
                Err(Refusal::Forbidden),
                "{gw}"
            );
        }
        assert_eq!(reviews(&api), 0);
        assert_eq!(counter(SyncAuthStage::Authz), authz + 2);

        // A token bound to no pod names no instance.
        api.reply_to(
            REVIEWS,
            201,
            review(true, "system:serviceaccount:apps:portail-web", None),
        );
        let token = gateway_token("apps", "web", 20);
        let claimed = auth.precheck(&token).unwrap();
        assert_eq!(
            auth.authenticate(&claimed, &token, &web).await,
            Err(Refusal::Forbidden)
        );
    }

    #[tokio::test]
    async fn an_audience_unaware_review_rejects() {
        let _serial = METRICS_SERIAL.lock().await;
        let api = MockApi::default();
        let mut answer = review(true, "system:serviceaccount:apps:portail-web", Some("p"));
        answer["status"]["audiences"] = json!([]);
        api.reply_to(REVIEWS, 201, answer);
        let auth = authenticator(api.client());
        let token = gateway_token("apps", "web", 0);
        let claimed = auth.precheck(&token).unwrap();
        assert_eq!(
            auth.authenticate(&claimed, &token, &GatewayKey::new("apps", "web"))
                .await,
            Err(Refusal::Unauthenticated)
        );
    }

    #[test]
    fn service_accounts_parse_from_usernames() {
        assert_eq!(
            ServiceAccount::from_username("system:serviceaccount:ns:sa"),
            Some(sa("ns", "sa"))
        );
        for bad in [
            "system:serviceaccount:ns",
            "system:serviceaccount::sa",
            "system:serviceaccount:ns:",
            "system:node:n1",
            "ns:sa",
        ] {
            assert_eq!(ServiceAccount::from_username(bad), None, "{bad}");
        }
    }
}
