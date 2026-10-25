//! Leader election: a `coordination.k8s.io/v1` Lease, the client-go
//! algorithm and defaults. Standbys keep their caches warm, write nothing
//! and answer polls 503.
//!
//! A held lease expires on the candidate's own clock: it may take over only
//! once the lease record stayed unchanged for its duration since the
//! candidate last saw it change, so clock skew between replicas cannot steal
//! a live lease. The leader stops leading when it has not renewed for the
//! renew deadline, shorter than the duration a candidate waits.

use std::sync::{Arc, Mutex};

use futures::channel::mpsc;
use futures::Stream;
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use k8s_openapi::chrono::Utc;
use kube::api::{Api, ObjectMeta, PostParams};
use kube::Client;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use crate::logging::{error, info, warn};

// client-go leaderelection defaults, as controller-runtime managers use them.
const LEASE_DURATION: Duration = Duration::from_secs(15);
const RENEW_DEADLINE: Duration = Duration::from_secs(10);
pub(super) const RETRY_PERIOD: Duration = Duration::from_secs(2);
/// client-go's JitterFactor on a candidate's retries: candidates that start
/// together do not try in lockstep.
const RETRY_JITTER: f64 = 1.2;
/// What client-go writes on release: a candidate reading the record before
/// it sees the empty holder still waits at most this long.
const RELEASED_DURATION_SECS: i32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Standby,
    Leading,
    /// The lease could not be renewed or another replica holds it: the
    /// process exits and restarts as a standby.
    Lost,
}

/// This replica's view of the election.
#[derive(Clone)]
pub(crate) struct Leader(Arc<Shared>);

struct Shared {
    role: watch::Sender<Role>,
    /// One sender per `gained` stream.
    gained: Mutex<Vec<mpsc::UnboundedSender<()>>>,
    /// When this replica last became the leader.
    since: Mutex<Option<Instant>>,
}

impl Leader {
    fn new(role: Role) -> Self {
        Leader(Arc::new(Shared {
            role: watch::Sender::new(role),
            gained: Mutex::default(),
            since: Mutex::new((role == Role::Leading).then(Instant::now)),
        }))
    }

    /// A replica that always leads.
    #[cfg(test)]
    pub(crate) fn always() -> Self {
        Leader::new(Role::Leading)
    }

    /// A replica that waits for the lease.
    #[cfg(test)]
    pub(crate) fn standby() -> Self {
        Leader::new(Role::Standby)
    }

    /// Whether this replica may write status, provision and serve.
    pub(crate) fn is_leader(&self) -> bool {
        *self.0.role.borrow() == Role::Leading
    }

    /// One item per leadership gained, one at once if leading already:
    /// every Gateway is reconciled again.
    pub(crate) fn gained(&self) -> impl Stream<Item = ()> + Send + Sync + 'static {
        let (tx, rx) = mpsc::unbounded();
        // Under the lock `set` takes: a gain lands either here or in `set`.
        let mut gained = self.0.gained.lock().unwrap_or_else(|p| p.into_inner());
        if self.is_leader() {
            let _ = tx.unbounded_send(());
        }
        gained.push(tx);
        rx
    }

    /// When this replica became the leader, while it leads.
    pub(crate) fn leading_since(&self) -> Option<Instant> {
        let since = *self.0.since.lock().unwrap_or_else(|p| p.into_inner());
        since.filter(|_| self.is_leader())
    }

    /// Resolves once the lease is lost.
    pub(crate) async fn lost(&self) {
        let mut role = self.0.role.subscribe();
        // The sender lives in `self`: the wait ends only on Lost.
        let _ = role.wait_for(|r| *r == Role::Lost).await;
    }

    fn set(&self, role: Role) {
        let mut gained = self.0.gained.lock().unwrap_or_else(|p| p.into_inner());
        let was = self.0.role.send_replace(role);
        if role == Role::Leading && was != Role::Leading {
            *self.0.since.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
            gained.retain(|tx| tx.unbounded_send(()).is_ok());
        }
    }
}

/// A replica's campaign for the lease.
pub(crate) struct Elector {
    leader: Leader,
    stop: CancellationToken,
    task: JoinHandle<()>,
}

impl Elector {
    pub(crate) fn leader(&self) -> Leader {
        self.leader.clone()
    }

    /// Stop campaigning and give the lease up if held: a standby takes over
    /// within one retry instead of a lease duration. Call it once nothing
    /// writes any more.
    pub(crate) async fn stop(self) {
        self.stop.cancel();
        let _ = self.task.await;
    }
}

/// Take part in the election for `args.leader_election_id` in
/// `args.namespace` until [`Elector::stop`].
pub(crate) fn start(client: &Client, args: &crate::cli::ControllerArgs) -> Elector {
    campaign(LeaseLock::new(
        Api::namespaced(client.clone(), &args.namespace),
        &args.leader_election_id,
        identity(),
    ))
}

/// `<hostname>_<random>`: a restarted container keeps its pod's hostname,
/// and must not take its previous process for itself (client-go identities
/// carry a UUID for the same reason).
fn identity() -> String {
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "portail-controller".to_string());
    format!("{host}_{:016x}", fastrand::u64(..))
}

fn campaign(lock: LeaseLock) -> Elector {
    let leader = Leader::new(Role::Standby);
    let stop = CancellationToken::new();
    let task = tokio::spawn(run_campaign(lock, leader.clone(), stop.clone()));
    Elector { leader, stop, task }
}

async fn run_campaign(mut lock: LeaseLock, leader: Leader, stop: CancellationToken) {
    info!(
        "Leader election: campaigning for Lease {} as {}",
        lock.name, lock.identity
    );
    // When the last successful renewal started: the apiserver saw it no
    // earlier, so no candidate counts the lease down from before it.
    let mut renewed: Option<Instant> = None;
    loop {
        let started = Instant::now();
        let budget = match renewed {
            Some(at) => RENEW_DEADLINE.saturating_sub(started - at),
            None => RENEW_DEADLINE,
        };
        let attempt = tokio::select! {
            _ = stop.cancelled() => break,
            attempt = tokio::time::timeout(budget, lock.try_acquire_or_renew(started)) => attempt,
        };
        let failure = match attempt {
            Ok(Ok(Outcome::Held)) => {
                if renewed.is_none() {
                    info!("Leader election: leading as {}", lock.identity);
                    leader.set(Role::Leading);
                }
                renewed = Some(started);
                None
            }
            Ok(Ok(Outcome::HeldBy(holder))) => {
                if renewed.is_some() {
                    error!(
                        "Leader election: Lease taken by {}, no longer leading",
                        holder
                    );
                    leader.set(Role::Lost);
                    return;
                }
                None
            }
            Ok(Ok(Outcome::Contended)) => Some("written concurrently".to_string()),
            Ok(Err(e)) => Some(e.to_string()),
            Err(_) => Some(format!("no answer within {:?}", budget)),
        };
        if let (Some(why), Some(at)) = (failure, renewed) {
            if at.elapsed() >= RENEW_DEADLINE {
                error!(
                    "Leader election: Lease not renewed for {:?} ({}), no longer leading",
                    RENEW_DEADLINE, why
                );
                leader.set(Role::Lost);
                return;
            }
            warn!("Leader election: renewing the Lease failed: {}", why);
        }
        let wait = if renewed.is_some() {
            RETRY_PERIOD
        } else {
            RETRY_PERIOD.mul_f64(1.0 + fastrand::f64() * RETRY_JITTER)
        };
        tokio::select! {
            _ = stop.cancelled() => break,
            _ = tokio::time::sleep(wait) => {}
        }
    }
    if renewed.is_some() {
        leader.set(Role::Standby);
        match tokio::time::timeout(RENEW_DEADLINE, lock.release()).await {
            Ok(Ok(())) => info!("Leader election: Lease released"),
            Ok(Err(e)) => warn!("Leader election: releasing the Lease failed: {}", e),
            Err(_) => warn!("Leader election: releasing the Lease timed out"),
        }
    }
}

enum Outcome {
    Held,
    HeldBy(String),
    /// Another writer changed the Lease between our read and write.
    Contended,
}

struct LeaseLock {
    api: Api<Lease>,
    name: String,
    identity: String,
    /// The record as last read, and when it was last seen to change.
    observed: Option<(LeaseSpec, Instant)>,
}

impl LeaseLock {
    fn new(api: Api<Lease>, name: &str, identity: String) -> Self {
        Self {
            api,
            name: name.to_string(),
            identity,
            observed: None,
        }
    }

    fn observe(&mut self, record: LeaseSpec, at: Instant) {
        if self.observed.as_ref().map(|(r, _)| r) != Some(&record) {
            self.observed = Some((record, at));
        }
    }

    /// One round; the lease's validity is judged at `started`, when the
    /// round began, the later of the two instants never in its favor.
    async fn try_acquire_or_renew(&mut self, started: Instant) -> Result<Outcome, kube::Error> {
        let wall = MicroTime(Utc::now());
        let duration_secs = i32::try_from(LEASE_DURATION.as_secs()).unwrap_or(i32::MAX);
        let Some(mut lease) = self.api.get_opt(&self.name).await? else {
            let lease = Lease {
                metadata: ObjectMeta {
                    name: Some(self.name.clone()),
                    ..Default::default()
                },
                spec: Some(LeaseSpec {
                    holder_identity: Some(self.identity.clone()),
                    lease_duration_seconds: Some(duration_secs),
                    acquire_time: Some(wall.clone()),
                    renew_time: Some(wall),
                    lease_transitions: Some(0),
                    ..Default::default()
                }),
            };
            return match self.api.create(&PostParams::default(), &lease).await {
                Ok(created) => {
                    self.observe(created.spec.unwrap_or_default(), Instant::now());
                    Ok(Outcome::Held)
                }
                Err(e) if is_conflict(&e) => Ok(Outcome::Contended),
                Err(e) => Err(e),
            };
        };
        let record = lease.spec.clone().unwrap_or_default();
        // When the read returned: a slow apiserver may have read the record
        // long after `started`, and the countdown must not start before it did.
        self.observe(record.clone(), Instant::now());
        let holder = record.holder_identity.clone().unwrap_or_default();
        let mine = holder == self.identity;
        let valid_for =
            Duration::from_secs(record.lease_duration_seconds.unwrap_or(0).max(0) as u64);
        let changed_at = self.observed.as_ref().map_or(started, |(_, at)| *at);
        if !holder.is_empty() && !mine && changed_at + valid_for > started {
            return Ok(Outcome::HeldBy(holder));
        }
        lease.spec = Some(LeaseSpec {
            holder_identity: Some(self.identity.clone()),
            lease_duration_seconds: Some(duration_secs),
            acquire_time: if mine {
                record.acquire_time.clone()
            } else {
                Some(wall.clone())
            },
            renew_time: Some(wall),
            lease_transitions: if mine {
                record.lease_transitions
            } else {
                Some(record.lease_transitions.unwrap_or(0).saturating_add(1))
            },
            ..record
        });
        match self
            .api
            .replace(&self.name, &PostParams::default(), &lease)
            .await
        {
            Ok(updated) => {
                self.observe(updated.spec.unwrap_or_default(), Instant::now());
                Ok(Outcome::Held)
            }
            Err(e) if is_conflict(&e) => Ok(Outcome::Contended),
            Err(e) => Err(e),
        }
    }

    /// Empty the holder, unless another replica took the Lease meanwhile.
    async fn release(&mut self) -> Result<(), kube::Error> {
        let Some(mut lease) = self.api.get_opt(&self.name).await? else {
            return Ok(());
        };
        let spec = lease.spec.get_or_insert_with(Default::default);
        if spec.holder_identity.as_deref() != Some(self.identity.as_str()) {
            return Ok(());
        }
        let wall = MicroTime(Utc::now());
        spec.holder_identity = Some(String::new());
        spec.lease_duration_seconds = Some(RELEASED_DURATION_SECS);
        spec.acquire_time = Some(wall.clone());
        spec.renew_time = Some(wall);
        self.api
            .replace(&self.name, &PostParams::default(), &lease)
            .await?;
        Ok(())
    }
}

fn is_conflict(e: &kube::Error) -> bool {
    matches!(e, kube::Error::Api(status) if status.code == 409)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicBool, Ordering};

    const NS: &str = "portail-system";
    const NAME: &str = "portail-controller";

    /// One Lease, shared by the replicas' clients, with resourceVersion
    /// conflicts as the apiserver checks them.
    #[derive(Default)]
    struct FakeLeases {
        lease: Mutex<Option<Value>>,
        version: Mutex<u64>,
    }

    impl FakeLeases {
        fn holder(&self) -> Option<String> {
            let lease = self.lease.lock().unwrap();
            lease.as_ref()?["spec"]["holderIdentity"]
                .as_str()
                .map(String::from)
        }

        /// Another writer takes the Lease.
        fn overwrite_holder(&self, holder: &str) {
            let mut lease = self.lease.lock().unwrap();
            let lease = lease.as_mut().unwrap();
            let mut version = self.version.lock().unwrap();
            *version += 1;
            lease["spec"]["holderIdentity"] = json!(holder);
            lease["metadata"]["resourceVersion"] = json!(version.to_string());
        }

        fn status(code: u16, reason: &str) -> (u16, Value) {
            (
                code,
                json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                       "message": reason, "reason": reason, "code": code}),
            )
        }

        fn answer(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
            let collection = format!("/apis/coordination.k8s.io/v1/namespaces/{NS}/leases");
            let item = format!("{collection}/{NAME}");
            let mut lease = self.lease.lock().unwrap();
            let mut version = self.version.lock().unwrap();
            match (method, path) {
                ("GET", p) if p == item => match lease.as_ref() {
                    Some(l) => (200, l.clone()),
                    None => Self::status(404, "NotFound"),
                },
                ("POST", p) if p == collection => {
                    if lease.is_some() {
                        return Self::status(409, "AlreadyExists");
                    }
                    *version += 1;
                    let mut created = body;
                    created["metadata"]["resourceVersion"] = json!(version.to_string());
                    *lease = Some(created.clone());
                    (201, created)
                }
                ("PUT", p) if p == item => {
                    let current = lease
                        .as_ref()
                        .map(|l| l["metadata"]["resourceVersion"].clone());
                    if current != Some(body["metadata"]["resourceVersion"].clone()) {
                        return Self::status(409, "Conflict");
                    }
                    *version += 1;
                    let mut updated = body;
                    updated["metadata"]["resourceVersion"] = json!(version.to_string());
                    *lease = Some(updated.clone());
                    (200, updated)
                }
                _ => Self::status(405, "MethodNotAllowed"),
            }
        }
    }

    /// A replica's client to `leases`; every request fails while `down`.
    fn client(leases: Arc<FakeLeases>, down: Arc<AtomicBool>) -> Client {
        slow_client(leases, down, Arc::default())
    }

    /// Like [`client`]; the next GET first waits `slow` at the apiserver,
    /// then reads the Lease.
    fn slow_client(
        leases: Arc<FakeLeases>,
        down: Arc<AtomicBool>,
        slow: Arc<Mutex<Option<Duration>>>,
    ) -> Client {
        let service = tower::service_fn(move |req: http::Request<kube::client::Body>| {
            let (leases, down, slow) = (leases.clone(), down.clone(), slow.clone());
            async move {
                let (parts, body) = req.into_parts();
                let bytes = body.collect_bytes().await.unwrap_or_default();
                let wait = (parts.method == http::Method::GET)
                    .then(|| slow.lock().unwrap().take())
                    .flatten();
                if let Some(wait) = wait {
                    tokio::time::sleep(wait).await;
                }
                let (code, reply) = if down.load(Ordering::SeqCst) {
                    FakeLeases::status(500, "InternalError")
                } else {
                    leases.answer(
                        parts.method.as_str(),
                        parts.uri.path(),
                        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
                    )
                };
                Ok::<_, std::convert::Infallible>(
                    http::Response::builder()
                        .status(code)
                        .body(kube::client::Body::from(
                            serde_json::to_vec(&reply).unwrap(),
                        ))
                        .unwrap(),
                )
            }
        });
        Client::new(service, NS)
    }

    struct Replica {
        elector: Elector,
        down: Arc<AtomicBool>,
    }

    impl Replica {
        fn start(leases: &Arc<FakeLeases>, identity: &str) -> Self {
            let down = Arc::new(AtomicBool::new(false));
            let api = Api::namespaced(client(leases.clone(), down.clone()), NS);
            let elector = campaign(LeaseLock::new(api, NAME, identity.into()));
            Replica { elector, down }
        }

        fn leads(&self) -> bool {
            self.elector.leader().is_leader()
        }
    }

    async fn sleep(secs: f64) {
        tokio::time::sleep(Duration::from_secs_f64(secs)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_replica_leads_and_the_second_waits() {
        let leases = Arc::new(FakeLeases::default());
        let a = Replica::start(&leases, "a");
        let mut gained = a.elector.leader().gained();
        sleep(0.1).await;
        assert!(a.leads());
        assert_eq!(gained.next().await, Some(()));
        assert_eq!(leases.holder().as_deref(), Some("a"));

        let b = Replica::start(&leases, "b");
        sleep(60.0).await;
        assert!(a.leads(), "renewed every retry");
        assert!(!b.leads(), "a live lease is never taken");
        let mut gained = a.elector.leader().gained();
        assert_eq!(gained.next().await, Some(()), "leading already");
    }

    /// A replica that stops renewing stops leading at the renew deadline;
    /// the other takes the lease only after it stayed unchanged for a lease
    /// duration on its own clock.
    #[tokio::test(start_paused = true)]
    async fn a_lease_is_not_taken_before_it_expires_unchanged() {
        let leases = Arc::new(FakeLeases::default());
        let a = Replica::start(&leases, "a");
        sleep(0.1).await;
        let b = Replica::start(&leases, "b");
        sleep(20.0).await;
        assert!(a.leads() && !b.leads());

        // a's last renewal started within the retry before this.
        a.down.store(true, Ordering::SeqCst);
        let lost = a.elector.leader();
        sleep(10.0).await;
        assert!(!a.leads(), "past the renew deadline");
        tokio::time::timeout(Duration::ZERO, lost.lost())
            .await
            .expect("lost resolves");

        // Earliest legal takeover: last renewal (>= down - retry) + lease.
        sleep(15.0 - 2.0 - 10.0 - 0.1).await;
        assert!(!b.leads(), "the lease has not been unchanged for 15s");
        sleep(15.0).await;
        assert!(b.leads());
        assert_eq!(leases.holder().as_deref(), Some("b"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_stopped_leader_releases_and_the_standby_takes_over_within_a_retry() {
        let leases = Arc::new(FakeLeases::default());
        let a = Replica::start(&leases, "a");
        sleep(0.1).await;
        let b = Replica::start(&leases, "b");
        sleep(5.0).await;
        let a_leader = a.elector.leader();
        a.elector.stop().await;
        assert!(!a_leader.is_leader());
        assert_eq!(leases.holder().as_deref(), Some(""));

        // A jittered candidate retry is at most retry x (1 + jitter).
        sleep(RETRY_PERIOD.as_secs_f64() * (1.0 + RETRY_JITTER) + 0.1).await;
        assert!(b.leads());
    }

    #[tokio::test(start_paused = true)]
    async fn a_leader_that_sees_another_holder_loses_at_once() {
        let leases = Arc::new(FakeLeases::default());
        let a = Replica::start(&leases, "a");
        sleep(0.1).await;
        assert!(a.leads());
        leases.overwrite_holder("x");
        sleep(RETRY_PERIOD.as_secs_f64()).await;
        assert!(!a.leads());
        tokio::time::timeout(Duration::ZERO, a.elector.leader().lost())
            .await
            .expect("lost resolves");
        assert_eq!(leases.holder().as_deref(), Some("x"), "never written back");
    }

    /// A read the apiserver answers late saw the Lease when it returned, not
    /// when it was sent: the countdown starts then, as client-go stamps
    /// observedTime after its Get.
    #[tokio::test(start_paused = true)]
    async fn a_slow_read_counts_down_from_when_it_returned() {
        let leases = Arc::new(FakeLeases::default());
        let a = Replica::start(&leases, "a");
        sleep(0.1).await;
        a.down.store(true, Ordering::SeqCst);
        let slow = Arc::new(Mutex::new(Some(Duration::from_secs(9))));
        let api = Api::namespaced(slow_client(leases.clone(), Arc::default(), slow), NS);
        let mut b = LeaseLock::new(api, NAME, "b".into());

        let sent = Instant::now();
        assert!(matches!(
            b.try_acquire_or_renew(sent).await.unwrap(),
            Outcome::HeldBy(_)
        ));
        let read = Instant::now();
        assert_eq!(read - sent, Duration::from_secs(9));

        tokio::time::sleep_until(read + LEASE_DURATION - Duration::from_millis(100)).await;
        assert!(
            matches!(
                b.try_acquire_or_renew(Instant::now()).await.unwrap(),
                Outcome::HeldBy(_)
            ),
            "the record has been seen unchanged for less than a lease"
        );
        tokio::time::sleep_until(read + LEASE_DURATION).await;
        assert!(matches!(
            b.try_acquire_or_renew(Instant::now()).await.unwrap(),
            Outcome::Held
        ));
    }

    #[test]
    fn identities_differ_per_process_start() {
        assert_ne!(identity(), identity());
    }
}
