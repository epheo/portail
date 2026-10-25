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
//! bounds it. A known token whose verdict expired is reviewed again from a
//! pool of its own, so forgeries holding the unknown pool never stop a
//! running data plane; when that pool is busy or the apiserver fails, the
//! expired verdict serves one more TTL.

use std::collections::HashMap;
use std::net::IpAddr;
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
/// TokenReviews in flight per peer IP: one pod gets at most 2 of the 16
/// unknown permits, so a flood from one pod never fills them; a data plane
/// needs one at a time. Renewals count, but are not refused: known tokens
/// are a bounded set.
const MAX_REVIEWS_PER_PEER: usize = 2;
/// Renewals of known tokens in flight, apart from the unknown pool. Each
/// live instance renews once per TTL, spread over it: at milliseconds per
/// review, 4 renew hundreds of instances a second, and 16 + 4 stays at 10%
/// of the apiserver's mutating budget.
const MAX_RENEWALS_IN_FLIGHT: usize = 4;
/// How long a verdict is kept, either way. Accepted: a deleted pod's token
/// stops working within it. Rejected: such a token never turns valid, so
/// it only bounds memory and how long a replay is answered from memory.
const CACHE_TTL: Duration = HOLD;
/// How long an expired positive verdict may still serve while no renewal
/// can be made: one more TTL, so a revoked token works at most two. A
/// deleted pod can thus receive snapshots for up to about 90s after its
/// last positive review while renewals fail: the stale grace plus one hold.
const STALE_TTL: Duration = HOLD;
/// A TokenReview is one in-memory check by the apiserver (signature, and
/// the bound pod in its cache): milliseconds. 5s is generous and frees a
/// permit a slow apiserver holds long before a hold passes.
const REVIEW_TIMEOUT: Duration = Duration::from_secs(5);
/// Verdicts kept at most. Positives are about one per live data-plane pod
/// (two while its token rotates); the rest of the room is for rejections,
/// which a flood of forgeries fills first. A few hundred bytes each: about
/// 1 MiB at the cap.
const MAX_VERDICTS: usize = 4096;
/// Accepted verdicts kept per pod: its token and the one kubelet just
/// rotated out. A tenant minting tokens for its own pods pushes out only
/// its own.
const MAX_VERDICTS_PER_POD: usize = 2;

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

/// What a TokenReview said a token is: always a pod-bound token, the
/// instance it names.
#[derive(Clone, Debug)]
struct Reviewed {
    account: ServiceAccount,
    pod_uid: String,
}

type TokenHash = [u8; 32];

#[derive(Default)]
struct Verdicts {
    /// When each verdict was taken; `None`: rejected.
    by_token: HashMap<TokenHash, (Instant, Option<Reviewed>)>,
    swept: Option<Instant>,
}

/// What the cache knows of a token.
enum Cached {
    Fresh(Reviewed),
    Rejected,
    /// Accepted, past its TTL but within the stale one.
    Expired(Reviewed),
    Unknown,
}

/// Why a review gave no verdict.
enum Unreviewed {
    /// The apiserver did not answer in time, or answered an error.
    Failed,
    /// TokenReview said no.
    Rejected,
}

/// A review in flight for a peer IP, counted until dropped.
struct PeerReview<'a> {
    peers: &'a Mutex<HashMap<IpAddr, usize>>,
    peer: IpAddr,
}

impl Drop for PeerReview<'_> {
    fn drop(&mut self) {
        let mut peers = self.peers.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(n) = peers.get_mut(&self.peer) {
            *n -= 1;
            if *n == 0 {
                peers.remove(&self.peer);
            }
        }
    }
}

pub(crate) struct Authenticator {
    reviews: Api<TokenReview>,
    audience: String,
    /// Never authorized, whatever Gateway is asked for: the controller's
    /// own ServiceAccount and the one a Gateway named after the controller
    /// Service, in its namespace, would run as.
    refused: Vec<ServiceAccount>,
    unknown: Semaphore,
    renewals: Semaphore,
    /// Reviews in flight by peer IP.
    peers: Mutex<HashMap<IpAddr, usize>>,
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
            unknown: Semaphore::new(MAX_REVIEWS_IN_FLIGHT),
            renewals: Semaphore::new(MAX_RENEWALS_IN_FLIGHT),
            peers: Mutex::default(),
            verdicts: Mutex::default(),
        }
    }

    /// The first stage, on the request headers alone: the ServiceAccount
    /// `token` claims to be.
    pub(crate) fn precheck(&self, token: &str) -> Result<ServiceAccount, Refusal> {
        precheck(token, &self.audience, SystemTime::now())
            .ok_or_else(|| rejected(SyncAuthStage::Precheck, Refusal::Unauthenticated))
    }

    /// The instance `token` belongs to, if it may sync `gateway`, asked from
    /// `peer`. `claimed` is what [`Self::precheck`] read from it.
    pub(crate) async fn authenticate(
        &self,
        claimed: &ServiceAccount,
        token: &str,
        gateway: &GatewayKey,
        peer: IpAddr,
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
            Cached::Fresh(reviewed) => reviewed,
            Cached::Rejected => {
                return Err(rejected(
                    SyncAuthStage::NegativeCache,
                    Refusal::Unauthenticated,
                ))
            }
            // The pre-check already refused a token past its own exp.
            Cached::Expired(stale) => {
                // A permit frees within a review's timeout unless the
                // apiserver hangs, and then the stale verdict serves.
                let renewal = tokio::time::timeout(REVIEW_TIMEOUT, self.renewals.acquire()).await;
                let reviewed = match renewal {
                    Ok(Ok(_permit)) => {
                        let _peer = self.peer_review(peer, usize::MAX);
                        self.review(hash, token).await
                    }
                    Ok(Err(_)) | Err(_) => Err(Unreviewed::Failed),
                };
                match reviewed {
                    Ok(reviewed) => reviewed,
                    Err(Unreviewed::Failed) => {
                        METRICS.sync_auth_stale_verdicts_total.inc();
                        stale
                    }
                    Err(Unreviewed::Rejected) => return Err(Refusal::Unauthenticated),
                }
            }
            Cached::Unknown => {
                let peer_review = self.peer_review(peer, MAX_REVIEWS_PER_PEER);
                let permit = self.unknown.try_acquire();
                let (Some(_peer), Ok(_permit)) = (peer_review, permit) else {
                    METRICS.sync_throttled_total.inc();
                    return Err(Refusal::Throttled);
                };
                match self.review(hash, token).await {
                    Ok(reviewed) => reviewed,
                    Err(Unreviewed::Failed) => return Err(Refusal::Unavailable),
                    Err(Unreviewed::Rejected) => return Err(Refusal::Unauthenticated),
                }
            }
        };
        if reviewed.account != expected {
            return Err(rejected(SyncAuthStage::Authz, Refusal::Forbidden));
        }
        Ok(InstanceId(reviewed.pod_uid))
    }

    /// Count a review for `peer`, unless it has `cap` in flight already.
    fn peer_review(&self, peer: IpAddr, cap: usize) -> Option<PeerReview<'_>> {
        let mut peers = self.peers.lock().unwrap_or_else(|p| p.into_inner());
        let n = peers.entry(peer).or_insert(0);
        if *n >= cap {
            return None;
        }
        *n += 1;
        Some(PeerReview {
            peers: &self.peers,
            peer,
        })
    }

    fn cached(&self, hash: &TokenHash) -> Cached {
        let verdicts = self.verdicts.lock().unwrap_or_else(|p| p.into_inner());
        let Some((at, verdict)) = verdicts.by_token.get(hash) else {
            return Cached::Unknown;
        };
        let age = at.elapsed();
        match verdict {
            Some(reviewed) if age < CACHE_TTL => Cached::Fresh(reviewed.clone()),
            Some(reviewed) if age < CACHE_TTL + STALE_TTL => Cached::Expired(reviewed.clone()),
            None if age < CACHE_TTL => Cached::Rejected,
            _ => Cached::Unknown,
        }
    }

    fn remember(&self, hash: TokenHash, verdict: Option<Reviewed>) {
        let mut verdicts = self.verdicts.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        let kept = |at: Instant, verdict: &Option<Reviewed>| {
            let ttl = match verdict {
                Some(_) => CACHE_TTL + STALE_TTL,
                None => CACHE_TTL,
            };
            now - at < ttl
        };
        // One sweep per TTL; the room a flood took is given back.
        if verdicts.swept.is_none_or(|at| now - at >= CACHE_TTL) {
            verdicts.by_token.retain(|_, (at, v)| kept(*at, v));
            verdicts.by_token.shrink_to_fit();
            verdicts.swept = Some(now);
        }
        if verdicts.by_token.len() >= MAX_VERDICTS && !verdicts.by_token.contains_key(&hash) {
            // Forgeries are what fills it: rejections go first, so no flood
            // evicts a running data plane's verdict.
            verdicts.by_token.retain(|_, (_, v)| v.is_some());
        }
        if verdicts.by_token.len() >= MAX_VERDICTS && !verdicts.by_token.contains_key(&hash) {
            // All accepted: the oldest quarter goes, re-reviewed on their
            // next poll.
            let mut ages: Vec<Instant> = verdicts.by_token.values().map(|(at, _)| *at).collect();
            ages.sort_unstable();
            let cutoff = ages[MAX_VERDICTS / 4];
            verdicts.by_token.retain(|_, (at, _)| *at > cutoff);
        }
        if let Some(reviewed) = &verdict {
            let mut same_pod: Vec<(Instant, TokenHash)> = verdicts
                .by_token
                .iter()
                .filter(|(h, (_, v))| {
                    **h != hash && v.as_ref().is_some_and(|r| r.pod_uid == reviewed.pod_uid)
                })
                .map(|(h, (at, _))| (*at, *h))
                .collect();
            if same_pod.len() >= MAX_VERDICTS_PER_POD {
                same_pod.sort_unstable();
                for (_, old) in &same_pod[..=same_pod.len() - MAX_VERDICTS_PER_POD] {
                    verdicts.by_token.remove(old);
                }
            }
        }
        verdicts.by_token.insert(hash, (now, verdict));
    }

    /// A TokenReview of `token`, its permits taken; its verdict is cached.
    async fn review(&self, hash: TokenHash, token: &str) -> Result<Reviewed, Unreviewed> {
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
            Err(_) => return Err(review_failed("no answer in time")),
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
                metrics::record_sync_auth_rejected(SyncAuthStage::TokenReview);
                debug!("TokenReview rejected a sync token: {:?}", status.error);
                Err(Unreviewed::Rejected)
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
        // A token bound to no pod names no instance to report as: rejected,
        // and kept as a rejection, so it never takes an accepted slot.
        let pod_uid = user
            .extra
            .as_ref()
            .and_then(|extra| extra.get(POD_UID))
            .and_then(|values| values.first())
            .cloned()?;
        Some(Reviewed { account, pod_uid })
    }
}

fn rejected(stage: SyncAuthStage, refusal: Refusal) -> Refusal {
    metrics::record_sync_auth_rejected(stage);
    refusal
}

fn review_failed(why: &str) -> Unreviewed {
    metrics::record_tokenreview(TokenReviewResult::Error);
    warn!("TokenReview failed: {}", why);
    Unreviewed::Failed
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
    /// The peer a test request comes from, unless it says otherwise.
    pub const PEER: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1));

    /// Peer `n`, another pod.
    pub fn peer(n: u32) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::from(0x0a01_0000 + n))
    }

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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

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
            let instance = auth
                .authenticate(&claimed, &token, &gw, PEER)
                .await
                .unwrap();
            assert_eq!(instance, InstanceId("pod-1".into()));
        }
        assert_eq!(reviews(&api), 1);
        assert_eq!(
            metrics::tokenreview_requests(TokenReviewResult::Authenticated),
            authenticated + 1
        );

        tokio::time::advance(HOLD).await;
        auth.authenticate(&claimed, &token, &gw, PEER)
            .await
            .unwrap();
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
                auth.authenticate(&claimed, &token, &gw, PEER).await,
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
            auth.authenticate(&claimed, &token, &gw, PEER).await,
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
                auth.authenticate(&claimed, &token, &gw, PEER).await,
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
        assert!(auth.authenticate(&claimed, &token, &gw, PEER).await.is_ok());
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
            let got = auth.authenticate(&claimed, &token, gw, PEER).await.err();
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
                auth.authenticate(&claimed, &token, &gw, PEER).await,
                Err(Refusal::Forbidden),
                "{gw}"
            );
        }
        assert_eq!(reviews(&api), 0);
        assert_eq!(counter(SyncAuthStage::Authz), authz + 2);

        // A token bound to no pod names no instance: rejected, and cached
        // as a rejection.
        api.reply_to(
            REVIEWS,
            201,
            review(true, "system:serviceaccount:apps:portail-web", None),
        );
        let token = gateway_token("apps", "web", 20);
        let claimed = auth.precheck(&token).unwrap();
        assert_eq!(
            auth.authenticate(&claimed, &token, &web, PEER).await,
            Err(Refusal::Unauthenticated)
        );
        assert!(matches!(auth.cached(&token_hash(&token)), Cached::Rejected));
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
            auth.authenticate(&claimed, &token, &GatewayKey::new("apps", "web"), PEER)
                .await,
            Err(Refusal::Unauthenticated)
        );
    }

    /// A kube client whose TokenReviews answer `known` tokens, as pods
    /// `pod-<n>` of `apps/web` (the first `slow` of them a second late), and
    /// leave every other one hanging, counting them.
    fn split_client(
        known: Vec<String>,
        slow: Arc<AtomicUsize>,
        hanging: Arc<AtomicUsize>,
    ) -> Client {
        let service = tower::service_fn(move |req: http::Request<kube::client::Body>| {
            let (known, slow, hanging) = (known.clone(), slow.clone(), hanging.clone());
            async move {
                let bytes = req.into_body().collect_bytes().await.unwrap();
                let body: Value = serde_json::from_slice(&bytes).unwrap();
                let token = body["spec"]["token"].as_str().unwrap_or_default();
                let Some(n) = known.iter().position(|k| k == token) else {
                    hanging.fetch_add(1, Ordering::SeqCst);
                    return futures::future::pending().await;
                };
                if n < slow.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                let answer = review(
                    true,
                    "system:serviceaccount:apps:portail-web",
                    Some(&format!("pod-{n}")),
                );
                Ok::<_, std::convert::Infallible>(
                    http::Response::builder()
                        .status(201)
                        .body(kube::client::Body::from(
                            serde_json::to_vec(&answer).unwrap(),
                        ))
                        .unwrap(),
                )
            }
        });
        Client::new(service, "default")
    }

    async fn settle() {
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
    }

    /// Forged well-formed tokens hold every review permit for unknown
    /// tokens: a running data plane's token, its verdict just expired, is
    /// still reviewed, from its own pool.
    #[tokio::test(start_paused = true)]
    async fn forged_tokens_do_not_starve_a_known_instance() {
        let _serial = METRICS_SERIAL.lock().await;
        let known = gateway_token("apps", "web", 0);
        let hanging = Arc::new(AtomicUsize::new(0));
        let auth = Arc::new(authenticator(split_client(
            vec![known.clone()],
            Arc::default(),
            hanging.clone(),
        )));
        let gw = GatewayKey::new("apps", "web");
        let claimed = auth.precheck(&known).unwrap();
        auth.authenticate(&claimed, &known, &gw, PEER)
            .await
            .unwrap();

        for _ in 0..3 {
            tokio::time::advance(HOLD).await;
            let flood: Vec<_> = (0..MAX_REVIEWS_IN_FLIGHT as u32)
                .map(|n| {
                    let auth = auth.clone();
                    tokio::spawn(async move {
                        let forged = gateway_token("apps", "web", 1000 + n);
                        let claimed = auth.precheck(&forged).unwrap();
                        let gw = GatewayKey::new("apps", "web");
                        auth.authenticate(&claimed, &forged, &gw, peer(n)).await
                    })
                })
                .collect();
            settle().await;
            assert_eq!(hanging.load(Ordering::SeqCst) % MAX_REVIEWS_IN_FLIGHT, 0);
            let extra = gateway_token("apps", "web", 999);
            let extra_claim = auth.precheck(&extra).unwrap();
            assert_eq!(
                auth.authenticate(&extra_claim, &extra, &gw, peer(999))
                    .await,
                Err(Refusal::Throttled),
                "the unknown pool is saturated"
            );
            assert_eq!(
                auth.authenticate(&claimed, &known, &gw, PEER).await,
                Ok(InstanceId("pod-0".into())),
                "the known instance syncs on"
            );
            flood.iter().for_each(|f| f.abort());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_expired_verdict_serves_one_more_hold_while_reviews_fail() {
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
        auth.authenticate(&claimed, &token, &gw, PEER)
            .await
            .unwrap();

        api.reply_to(
            REVIEWS,
            500,
            json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                   "reason": "InternalError", "message": "down", "code": 500}),
        );
        let stale = METRICS.sync_auth_stale_verdicts_total.get();
        tokio::time::advance(CACHE_TTL).await;
        assert_eq!(
            auth.authenticate(&claimed, &token, &gw, PEER).await,
            Ok(InstanceId("pod-1".into()))
        );
        assert_eq!(reviews(&api), 2, "a renewal was tried");
        assert_eq!(METRICS.sync_auth_stale_verdicts_total.get(), stale + 1);

        tokio::time::advance(STALE_TTL).await;
        assert_eq!(
            auth.authenticate(&claimed, &token, &gw, PEER).await,
            Err(Refusal::Unavailable),
            "two TTLs: unknown again"
        );
    }

    /// A kube client whose TokenReviews answer every token as pod `pod-1`
    /// of `apps/web` while `answer` is set, and hang otherwise.
    fn switched_client(answer: Arc<AtomicBool>, hanging: Arc<AtomicUsize>) -> Client {
        let service = tower::service_fn(move |req: http::Request<kube::client::Body>| {
            let (answer, hanging) = (answer.clone(), hanging.clone());
            async move {
                if !answer.load(Ordering::SeqCst) {
                    hanging.fetch_add(1, Ordering::SeqCst);
                    return futures::future::pending().await;
                }
                let bytes = req.into_body().collect_bytes().await.unwrap();
                let body: Value = serde_json::from_slice(&bytes).unwrap();
                let jti = claims(body["spec"]["token"].as_str().unwrap()).unwrap()["jti"].clone();
                let reply = review(
                    true,
                    "system:serviceaccount:apps:portail-web",
                    Some(&format!("pod-{jti}")),
                );
                Ok::<_, std::convert::Infallible>(
                    http::Response::builder()
                        .status(201)
                        .body(kube::client::Body::from(
                            serde_json::to_vec(&reply).unwrap(),
                        ))
                        .unwrap(),
                )
            }
        });
        Client::new(service, "default")
    }

    /// A review with no answer gives its permit back within 5s, and the
    /// expired verdict serves.
    #[tokio::test(start_paused = true)]
    async fn a_review_without_an_answer_frees_its_permit_in_5s() {
        let _serial = METRICS_SERIAL.lock().await;
        let answer = Arc::new(AtomicBool::new(true));
        let hanging = Arc::new(AtomicUsize::new(0));
        let auth = Arc::new(authenticator(switched_client(
            answer.clone(),
            hanging.clone(),
        )));
        let gw = GatewayKey::new("apps", "web");
        let tokens: Vec<String> = (0..MAX_RENEWALS_IN_FLIGHT as u32)
            .map(|n| gateway_token("apps", "web", n))
            .collect();
        for token in &tokens {
            let claimed = auth.precheck(token).unwrap();
            auth.authenticate(&claimed, token, &gw, PEER).await.unwrap();
        }

        answer.store(false, Ordering::SeqCst);
        tokio::time::advance(CACHE_TTL).await;
        let started = Instant::now();
        let renewing: Vec<_> = tokens
            .iter()
            .cloned()
            .enumerate()
            .map(|(n, token)| {
                let auth = auth.clone();
                tokio::spawn(async move {
                    let claimed = auth.precheck(&token).unwrap();
                    let gw = GatewayKey::new("apps", "web");
                    let got = auth
                        .authenticate(&claimed, &token, &gw, peer(n as u32))
                        .await;
                    (got, started.elapsed())
                })
            })
            .collect();
        for (n, renewal) in renewing.into_iter().enumerate() {
            let (got, elapsed) = renewal.await.unwrap();
            assert_eq!(got, Ok(InstanceId(format!("pod-{n}"))), "served stale");
            assert_eq!(elapsed, REVIEW_TIMEOUT);
        }
        assert_eq!(hanging.load(Ordering::SeqCst), MAX_RENEWALS_IN_FLIGHT);
    }

    /// Every renewal permit is taken by slow reviews: the next known token
    /// waits for one and is renewed, rather than served stale until it falls
    /// into the unknown pool.
    #[tokio::test(start_paused = true)]
    async fn a_renewal_waits_for_a_permit() {
        let _serial = METRICS_SERIAL.lock().await;
        let tokens: Vec<String> = (0..=MAX_RENEWALS_IN_FLIGHT as u32)
            .map(|n| gateway_token("apps", "web", n))
            .collect();
        let slow = Arc::new(AtomicUsize::new(0));
        let auth = Arc::new(authenticator(split_client(
            tokens.clone(),
            slow.clone(),
            Arc::default(),
        )));
        let gw = GatewayKey::new("apps", "web");
        for (n, token) in tokens.iter().enumerate() {
            let claimed = auth.precheck(token).unwrap();
            auth.authenticate(&claimed, token, &gw, peer(n as u32))
                .await
                .unwrap();
        }

        slow.store(MAX_RENEWALS_IN_FLIGHT, Ordering::SeqCst);
        tokio::time::advance(CACHE_TTL).await;
        let renewing: Vec<_> = tokens[..MAX_RENEWALS_IN_FLIGHT]
            .iter()
            .cloned()
            .enumerate()
            .map(|(n, token)| {
                let auth = auth.clone();
                tokio::spawn(async move {
                    let claimed = auth.precheck(&token).unwrap();
                    let gw = GatewayKey::new("apps", "web");
                    auth.authenticate(&claimed, &token, &gw, peer(n as u32))
                        .await
                })
            })
            .collect();
        settle().await;

        let last = &tokens[MAX_RENEWALS_IN_FLIGHT];
        let claimed = auth.precheck(last).unwrap();
        let stale = METRICS.sync_auth_stale_verdicts_total.get();
        let n = MAX_RENEWALS_IN_FLIGHT as u32;
        assert_eq!(
            auth.authenticate(&claimed, last, &gw, peer(n)).await,
            Ok(InstanceId(format!("pod-{n}")))
        );
        assert!(
            matches!(auth.cached(&token_hash(last)), Cached::Fresh(_)),
            "renewed, not served stale"
        );
        assert_eq!(METRICS.sync_auth_stale_verdicts_total.get(), stale);
        for renewal in renewing {
            assert!(renewal.await.unwrap().is_ok());
        }
    }

    /// A flood of forged tokens from one pod takes at most two review
    /// permits: a new pod elsewhere is still reviewed.
    #[tokio::test(start_paused = true)]
    async fn one_peer_takes_at_most_two_reviews() {
        let _serial = METRICS_SERIAL.lock().await;
        let newcomer = gateway_token("apps", "web", 0);
        let hanging = Arc::new(AtomicUsize::new(0));
        let auth = Arc::new(authenticator(split_client(
            vec![newcomer.clone()],
            Arc::default(),
            hanging.clone(),
        )));
        let gw = GatewayKey::new("apps", "web");
        let flood: Vec<_> = (0..MAX_REVIEWS_IN_FLIGHT as u32)
            .map(|n| {
                let auth = auth.clone();
                tokio::spawn(async move {
                    let forged = gateway_token("apps", "web", 1000 + n);
                    let claimed = auth.precheck(&forged).unwrap();
                    let gw = GatewayKey::new("apps", "web");
                    auth.authenticate(&claimed, &forged, &gw, PEER).await
                })
            })
            .collect();
        settle().await;
        assert_eq!(hanging.load(Ordering::SeqCst), MAX_REVIEWS_PER_PEER);
        let throttled = flood.iter().filter(|f| f.is_finished()).count();
        assert_eq!(throttled, MAX_REVIEWS_IN_FLIGHT - MAX_REVIEWS_PER_PEER);

        let claimed = auth.precheck(&newcomer).unwrap();
        assert_eq!(
            auth.authenticate(&claimed, &newcomer, &gw, peer(1)).await,
            Ok(InstanceId("pod-0".into()))
        );
        flood.iter().for_each(|f| f.abort());
    }

    fn hash(n: u32) -> TokenHash {
        let mut hash = [0; 32];
        hash[..4].copy_from_slice(&n.to_le_bytes());
        hash
    }

    fn accepted(n: u32) -> Option<Reviewed> {
        Some(Reviewed {
            account: sa("apps", "portail-web"),
            pod_uid: format!("pod-{n}"),
        })
    }

    /// A flood of rejections never grows the cache past its cap, nor evicts
    /// an accepted verdict; past the cap with accepted verdicts only, the
    /// oldest go; a sweep gives the room back.
    #[tokio::test(start_paused = true)]
    async fn the_verdict_cache_is_capped() {
        let auth = authenticator(MockApi::default().client());
        let len = |auth: &Authenticator| auth.verdicts.lock().unwrap().by_token.len();
        for n in 0..10 {
            auth.remember(hash(n), accepted(n));
        }
        for n in 10..10 + 2 * MAX_VERDICTS as u32 {
            auth.remember(hash(n), None);
        }
        assert!(len(&auth) <= MAX_VERDICTS, "{}", len(&auth));
        for n in 0..10 {
            assert!(matches!(auth.cached(&hash(n)), Cached::Fresh(_)), "{n}");
        }

        let base = 100_000;
        for n in base..base + MAX_VERDICTS as u32 {
            tokio::time::advance(Duration::from_micros(1)).await;
            auth.remember(hash(n), accepted(n));
        }
        assert!(len(&auth) <= MAX_VERDICTS);
        assert!(
            matches!(auth.cached(&hash(0)), Cached::Unknown),
            "the oldest went"
        );
        let newest = base + MAX_VERDICTS as u32 - 1;
        assert!(matches!(auth.cached(&hash(newest)), Cached::Fresh(_)));

        tokio::time::advance(CACHE_TTL + STALE_TTL).await;
        auth.remember(hash(1), None);
        let verdicts = auth.verdicts.lock().unwrap();
        assert_eq!(verdicts.by_token.len(), 1);
        assert!(verdicts.by_token.capacity() < MAX_VERDICTS, "shrunk");
    }

    /// A tenant minting pod-bound tokens for its own pod keeps two
    /// accepted verdicts, never pushing out a running data plane's.
    #[tokio::test(start_paused = true)]
    async fn one_pod_keeps_two_accepted_verdicts() {
        let auth = authenticator(MockApi::default().client());
        auth.remember(hash(0), accepted(0));
        let tenant = |n: u32| {
            let mut reviewed = accepted(7);
            reviewed.as_mut().unwrap().account = sa("tenant", "portail-x");
            let _ = n;
            reviewed
        };
        for n in 1..=2 * MAX_VERDICTS as u32 {
            tokio::time::advance(Duration::from_micros(1)).await;
            auth.remember(hash(n), tenant(n));
        }
        assert!(matches!(auth.cached(&hash(0)), Cached::Fresh(_)), "evicted");
        let verdicts = auth.verdicts.lock().unwrap();
        let of_tenant = verdicts
            .by_token
            .values()
            .filter(|(_, v)| v.is_some())
            .count()
            - 1;
        assert_eq!(of_tenant, 2);
        let newest = 2 * MAX_VERDICTS as u32;
        assert!(verdicts.by_token.contains_key(&hash(newest)));
        assert!(verdicts.by_token.contains_key(&hash(newest - 1)));
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
