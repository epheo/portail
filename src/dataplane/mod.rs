//! A Gateway's data plane in Kubernetes mode: it binds and serves what
//! the controller's snapshots say, and reports what it bound.
//!
//! Imports none of `kube`, `k8s_openapi`, `gateway_api` or
//! `crate::kubernetes` (CI greps for them): a data plane holds no API
//! client, permission or token.

pub mod addresses;
pub mod apply;
pub mod dns_refresh;
pub mod remote;

pub use apply::Applier;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::sync::watch;
use tokio::time::{sleep_until, Instant};
use tokio_util::sync::CancellationToken;

use crate::cli::DataplaneArgs;
use crate::config::PerformanceConfig;
use crate::logging::{error, info, warn};
use crate::metrics::{ControllerSync, CONTROLLER_SYNC};
use crate::proxy::data_plane::DataPlane;
use crate::routing::RouteTable;
use crate::sync::{GatewayKey, InstanceReport, Snapshot, SyncRequest, HOLD, PROTOCOL};
use remote::{Answer, Backoff, Client, CONNECT_TIMEOUT};

/// `portail dataplane`: serve the Gateway the controller describes until
/// `stop`, then report leaving, serve on for `--shutdown-delay` and drain.
pub async fn run(args: DataplaneArgs, stop: CancellationToken) -> anyhow::Result<()> {
    CONTROLLER_SYNC.start();
    let ca = remote::controller_ca(
        args.controller_ca_file.as_deref(),
        std::env::var(crate::cli::CONTROLLER_CA_ENV).ok(),
    )?;
    let mut client = Client::new(
        &args.controller_address,
        &args.gateway,
        &ca,
        args.token_file.clone(),
    )?;

    // Before listeners: a request must never race the writer task's spawn.
    if let Some(target) = &args.access_log {
        crate::access_log::init(target)?;
    }
    crate::rate_limit::spawn_sweeper();
    // Runtime knobs are not on the wire yet: every data plane runs the
    // defaults.
    let perf = PerformanceConfig {
        http2: args.http2,
        ..PerformanceConfig::default()
    };
    // No listener and no cert directory: every bind comes from a snapshot,
    // its certs in memory.
    let data_plane = DataPlane::new(&[], &perf, std::path::Path::new("/unused"))?;
    let drain = data_plane.drain_handle();
    let listener_exits = data_plane.listener_exits();

    let ready = Arc::new(AtomicBool::new(false));
    let admin_stop = CancellationToken::new();
    tokio::spawn(crate::admin::serve(
        SocketAddr::new(args.admin.admin_address, args.admin.readiness_port),
        ready.clone(),
        admin_stop.clone(),
    ));
    let applier = Applier::new(
        Arc::new(Mutex::new(data_plane)),
        Arc::new(ArcSwap::from_pointee(RouteTable::new())),
        perf,
        ready.clone(),
    );
    let refresh_stop = CancellationToken::new();
    applier.spawn_dns_refresh(refresh_stop.clone());
    info!(
        "Data plane for Gateway {} syncing from {}",
        args.gateway, args.controller_address
    );

    let mut sync = SyncLoop::new(args.gateway, applier, ready.clone(), listener_exits);
    sync.run(&mut client, &stop).await;

    info!("Serving {:?} more before the drain", args.shutdown_delay);
    let stopping = async {
        tokio::time::sleep(args.shutdown_delay).await;
        ready.store(false, Ordering::Release);
        drain.run().await;
    };
    tokio::join!(sync.leave(&mut client), stopping);
    refresh_stop.cancel();
    admin_stop.cancel();
    info!("Portail shutdown complete");
    Ok(())
}

/// The sync loop: what it serves, what it reports, when it polls again.
struct SyncLoop {
    gateway: GatewayKey,
    applier: Applier,
    ready: Arc<AtomicBool>,
    listener_exits: watch::Receiver<u64>,
    dns_epochs: watch::Receiver<u64>,
    /// The snapshot served: re-applied when a bind is owed.
    current: Option<Snapshot>,
    /// The version last received, served or refused: a refused one is not
    /// sent again at once.
    have: String,
    /// What the next poll reports; `None` before the first apply.
    report: Option<InstanceReport>,
    /// Polls that failed in a row.
    failures: Backoff,
    /// Re-applies of a snapshot not fully bound: a port held elsewhere, or
    /// an address not yet on the pod (interfaces are read on apply).
    rebinds: Backoff,
    rebind_at: Option<Instant>,
    /// The last poll's failure, for the no-snapshot warning.
    last_error: Option<String>,
    /// The version refused and why, until a snapshot is applied: every
    /// report says so, as the controller would not send it again.
    refused: Option<(String, String)>,
    metrics: &'static ControllerSync,
}

/// A 200 whose snapshot this data plane cannot serve.
#[derive(Debug)]
struct Unusable {
    /// `None` when even the envelope did not say: the poll counts as
    /// failed, as nothing tells the controller which version was refused.
    version: Option<String>,
    error: String,
}

impl SyncLoop {
    fn new(
        gateway: GatewayKey,
        applier: Applier,
        ready: Arc<AtomicBool>,
        listener_exits: watch::Receiver<u64>,
    ) -> Self {
        let dns_epochs = applier.dns_epochs();
        Self {
            gateway,
            applier,
            ready,
            listener_exits,
            dns_epochs,
            current: None,
            have: String::new(),
            report: None,
            failures: Backoff::default(),
            rebinds: Backoff::default(),
            rebind_at: None,
            last_error: None,
            refused: None,
            metrics: &CONTROLLER_SYNC,
        }
    }

    fn request(&self) -> SyncRequest {
        SyncRequest {
            protocol: PROTOCOL,
            gateway: self.gateway.clone(),
            have: self.have.clone(),
            data_plane_version: env!("CARGO_PKG_VERSION").to_string(),
            report: self.report.clone(),
        }
    }

    /// Poll until `stop`. A report that moves while a poll is held cancels
    /// it and polls again with the new one; the wait of a backoff stands.
    async fn run(&mut self, client: &mut Client, stop: &CancellationToken) {
        // The hold: a controller that has this Gateway's snapshot answers a
        // first poll at once, and one still translating within the hold.
        let mut watchdog = Some(Instant::now() + HOLD);
        let mut poll_at = Instant::now();
        loop {
            let (request, at) = (self.request(), poll_at);
            let poll = async {
                sleep_until(at).await;
                client.poll(&request).await
            };
            tokio::pin!(poll);
            let answer = loop {
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => return,
                    answer = &mut poll => break Some(answer),
                    Ok(()) = self.listener_exits.changed() => {
                        if self.reapply().await {
                            break None;
                        }
                    }
                    _ = sleep_until(self.rebind_at.unwrap_or(poll_at)), if self.rebind_at.is_some() => {
                        if self.reapply().await {
                            break None;
                        }
                    }
                    Ok(()) = self.dns_epochs.changed() => {
                        if self.dns_moved() {
                            break None;
                        }
                    }
                    _ = sleep_until(watchdog.unwrap_or(poll_at)), if watchdog.is_some() => {
                        watchdog = None;
                        if self.current.is_none() {
                            self.warn_no_snapshot();
                        }
                    }
                }
            };
            if let Some(answer) = answer {
                poll_at = self.handle(answer).await;
            }
        }
    }

    /// Act on a poll's answer; returns when to poll next.
    async fn handle(&mut self, answer: Answer) -> Instant {
        let now = Instant::now();
        let snapshot = match answer {
            Answer::Snapshot(body) => decode(&body),
            // The version confirmed may be one refused: the config served
            // is older, and its age grows on.
            Answer::Unchanged => {
                self.succeeded(self.refused.is_none());
                return now;
            }
            // Answered at once, not held: paced like a failure.
            Answer::Gone => {
                self.answered(true);
                self.gone().await;
                return now + self.failures.wait();
            }
            Answer::Retry { after, error } => return self.failed(after, error),
        };
        match snapshot {
            Ok(snapshot) if snapshot.gateway.key() == self.gateway => {
                self.succeeded(true);
                self.serve(snapshot).await;
            }
            Ok(snapshot) => {
                self.succeeded(false);
                let error = format!("snapshot is for Gateway {}", snapshot.gateway.key());
                self.refuse(snapshot.version, error);
            }
            Err(Unusable {
                version: Some(version),
                error,
            }) => {
                self.succeeded(false);
                self.refuse(version, error);
            }
            Err(Unusable {
                version: None,
                error,
            }) => return self.failed(None, error),
        }
        now
    }

    async fn serve(&mut self, snapshot: Snapshot) {
        self.refused = None;
        let report = self.applier.apply(&snapshot).await;
        self.have = snapshot.version.clone();
        self.report = Some(report);
        self.current = Some(snapshot);
        self.rebinds.reset();
        self.schedule_rebind();
    }

    /// The table served stays; the report names the refused version, with
    /// none of its listeners bound and why.
    fn refuse(&mut self, version: String, error: String) {
        error!(
            "Snapshot {} for Gateway {} not applied, serving on: {}",
            version, self.gateway, error
        );
        let previous = self.report.take().unwrap_or_else(|| InstanceReport {
            interface_ips: addresses::local_interface_ips(),
            dns_epoch: self.applier.dns_epoch(),
            ..InstanceReport::default()
        });
        self.have = version.clone();
        self.refused = Some((version, error));
        self.report = Some(self.as_reported(previous));
    }

    /// `report` of the snapshot served, as sent while a later one stands
    /// refused: under the refused version, with why and no listener of it.
    fn as_reported(&self, report: InstanceReport) -> InstanceReport {
        match &self.refused {
            Some((version, error)) => InstanceReport {
                applied: version.clone(),
                apply_error: Some(error.clone()),
                listeners: Vec::new(),
                ..report
            },
            None => report,
        }
    }

    /// The controller serves this Gateway nothing: close every listener
    /// and poll on, as a first sync.
    async fn gone(&mut self) {
        if self.current.take().is_some() {
            info!(
                "Gateway {} is gone from the controller: closing every listener",
                self.gateway
            );
            self.applier.clear().await;
        }
        self.have.clear();
        self.report = None;
        self.refused = None;
        self.rebind_at = None;
    }

    /// The controller answered as the protocol says: the backoff starts
    /// over.
    fn succeeded(&mut self, current: bool) {
        self.answered(current);
        self.failures.reset();
    }

    /// The controller answered; `current`: what it said is served now.
    fn answered(&mut self, current: bool) {
        self.metrics.set_connected(true);
        if current {
            self.metrics.confirmed();
        }
        if self.last_error.take().is_some() {
            info!("Sync of Gateway {} recovered", self.gateway);
        }
    }

    fn failed(&mut self, after: Option<Duration>, error: String) -> Instant {
        self.metrics.set_connected(false);
        let delay = self.failures.wait_at_least(after);
        warn!(
            "Sync of Gateway {} failed: {}; polling again in {:.1}s",
            self.gateway,
            error,
            delay.as_secs_f32()
        );
        self.last_error = Some(error);
        Instant::now() + delay
    }

    /// Re-apply the snapshot served, which binds what has no live socket.
    /// Whether the report moved.
    async fn reapply(&mut self) -> bool {
        let Some(snapshot) = &self.current else {
            self.rebind_at = None;
            return false;
        };
        let report = self.applier.apply(snapshot).await;
        let report = self.as_reported(report);
        let moved = self.report.as_ref() != Some(&report);
        self.report = Some(report);
        self.schedule_rebind();
        moved
    }

    /// Retry while an endpoint is unbound, or a Network address has no
    /// interface on the pod yet: its listeners bind the other addresses
    /// and turn Ready meanwhile.
    fn schedule_rebind(&mut self) {
        let unresolved = match (&self.current, &self.report) {
            (Some(snapshot), Some(report)) => !addresses::unresolved_networks(
                &snapshot.config.gateway.addresses,
                &report.networks,
            )
            .is_empty(),
            _ => false,
        };
        self.rebind_at = if self.ready.load(Ordering::Acquire) && !unresolved {
            self.rebinds.reset();
            None
        } else {
            Some(Instant::now() + self.rebinds.next_ceiling())
        };
    }

    /// Backend pods came or went: the controller re-reads named target
    /// ports when the report says so. Whether the report moved.
    fn dns_moved(&mut self) -> bool {
        let epoch = *self.dns_epochs.borrow_and_update();
        match &mut self.report {
            Some(report) if report.dns_epoch != epoch => {
                report.dns_epoch = epoch;
                true
            }
            _ => false,
        }
    }

    fn warn_no_snapshot(&self) {
        let why = match &self.last_error {
            Some(error) => format!("last poll: {}", error),
            None => "the controller has none for it yet (Gateway unknown, or its first translation pending)".to_string(),
        };
        warn!(
            "No snapshot for Gateway {} {}s after start, not ready: {}",
            self.gateway,
            HOLD.as_secs(),
            why
        );
    }

    /// Report leaving, so the controller stops counting this instance as
    /// live; it answers such a poll at once. Bounded by the connect
    /// timeout: shutdown does not wait on an unreachable controller.
    async fn leave(&self, client: &mut Client) {
        let Some(report) = &self.report else {
            return;
        };
        let request = SyncRequest {
            report: Some(InstanceReport {
                leaving: true,
                ..report.clone()
            }),
            ..self.request()
        };
        match tokio::time::timeout(CONNECT_TIMEOUT, client.poll(&request)).await {
            Ok(Answer::Retry { error, .. }) => warn!("Leave not reported: {}", error),
            Ok(_) => info!("Leave reported to the controller"),
            Err(_) => warn!("Leave not reported within {:?}", CONNECT_TIMEOUT),
        }
    }
}

/// A 200's body as this data plane serves it. The envelope is read first,
/// so an unsupported protocol, or a config this build cannot decode, still
/// names the version refused.
fn decode(body: &[u8]) -> Result<Snapshot, Unusable> {
    let unusable = |version: Option<&str>, error: String| Unusable {
        version: version.map(str::to_string),
        error,
    };
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| unusable(None, format!("not JSON: {}", e)))?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| unusable(None, "a snapshot without a version".to_string()))?;
    let protocol = value.get("protocol").and_then(serde_json::Value::as_u64);
    if protocol != Some(u64::from(PROTOCOL)) {
        let sent = protocol.map_or("none".to_string(), |p| p.to_string());
        return Err(unusable(
            Some(version),
            format!(
                "snapshot protocol {}, this data plane speaks {}",
                sent, PROTOCOL
            ),
        ));
    }
    let version = version.to_string();
    serde_json::from_value(value).map_err(|e| Unusable {
        version: Some(version),
        error: format!("snapshot does not decode: {}", e),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PortailConfig;
    use crate::sync::GatewayRef;

    fn snapshot() -> Snapshot {
        let config: PortailConfig = serde_json::from_value(serde_json::json!({
            "gateway": {"name": "gw", "listeners": []},
        }))
        .unwrap();
        let gateway = GatewayRef {
            namespace: "ns".into(),
            name: "gw".into(),
            uid: "uid".into(),
            generation: 1,
        };
        Snapshot::from_config(gateway, config, vec![])
    }

    #[test]
    fn a_snapshot_decodes_with_its_version_as_sent() {
        let sent = snapshot();
        let decoded = decode(&serde_json::to_vec(&sent).unwrap()).unwrap();
        assert_eq!(decoded.version, sent.version);
    }

    /// A newer protocol or a config this build cannot decode is refused
    /// under the version sent; a body without one is a failed poll.
    #[test]
    fn an_unusable_snapshot_still_names_its_version() {
        let mut json = serde_json::to_value(snapshot()).unwrap();
        json["protocol"] = serde_json::json!(PROTOCOL + 1);
        let refused = decode(&serde_json::to_vec(&json).unwrap()).unwrap_err();
        assert_eq!(refused.version.as_deref(), json["version"].as_str());
        assert!(refused.error.contains("protocol 2"), "{refused:?}");

        let mut json = serde_json::to_value(snapshot()).unwrap();
        json["config"]["gateway"]["newField"] = serde_json::json!(true);
        let refused = decode(&serde_json::to_vec(&json).unwrap()).unwrap_err();
        assert_eq!(refused.version.as_deref(), json["version"].as_str());
        assert!(refused.error.contains("newField"), "{refused:?}");

        for body in [&b"<html>"[..], br#"{"protocol": 1}"#, br#"{"version": ""}"#] {
            assert!(decode(body).unwrap_err().version.is_none());
        }
    }

    /// Envelope fields this build does not know are not a refusal.
    #[test]
    fn a_newer_envelope_decodes() {
        let mut json = serde_json::to_value(snapshot()).unwrap();
        json["runtime"] = serde_json::json!({"workers": 4});
        assert!(decode(&serde_json::to_vec(&json).unwrap()).is_ok());
    }

    fn sync_loop() -> (SyncLoop, Arc<AtomicBool>) {
        let perf = PerformanceConfig::default();
        let data_plane = DataPlane::new(&[], &perf, std::path::Path::new("/unused")).unwrap();
        let exits = data_plane.listener_exits();
        let ready = Arc::new(AtomicBool::new(false));
        let applier = Applier::new(
            Arc::new(Mutex::new(data_plane)),
            Arc::new(ArcSwap::from_pointee(RouteTable::new())),
            perf,
            ready.clone(),
        );
        let gateway = GatewayKey::new("ns", "gw");
        (SyncLoop::new(gateway, applier, ready.clone(), exits), ready)
    }

    /// A Network address whose interface is not on the pod yet keeps the
    /// rebinds coming, though the listeners bound another one and the pod
    /// is Ready: its Multus interface may come up after the first apply.
    #[tokio::test]
    async fn an_unresolved_network_keeps_rebinding() {
        let Some(ip) = addresses::local_interface_ips()
            .into_iter()
            .find(|ip| ip.parse::<std::net::Ipv4Addr>().is_ok())
        else {
            return; // no interface off loopback to match
        };
        let port = crate::proxy::data_plane::test_ports::wildcard();
        let on = |addresses: &[&str]| {
            let config: PortailConfig = serde_json::from_value(serde_json::json!({
                "gateway": {"name": "gw", "addresses": addresses, "listeners": [
                    {"name": "http", "protocol": "HTTP", "port": port},
                ]},
            }))
            .unwrap();
            let gateway = GatewayRef {
                namespace: "ns".into(),
                name: "gw".into(),
                uid: "uid".into(),
                generation: 1,
            };
            let networks = vec![crate::sync::NetworkCidrs {
                name: "here".into(),
                cidrs: vec![format!("{ip}/32")],
            }];
            Snapshot::from_config(gateway, config, networks)
        };

        let (mut sync, ready) = sync_loop();
        sync.serve(on(&["here"])).await;
        // As the loop retries a bind: a probe socket of this test's can sit
        // in a child another test forks until its exec.
        for _ in 0..200 {
            if ready.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
            sync.reapply().await;
        }
        assert!(ready.load(Ordering::Acquire));
        assert_eq!(sync.rebind_at, None);

        sync.serve(on(&["here", "not-yet"])).await;
        assert!(ready.load(Ordering::Acquire));
        assert!(sync.rebind_at.is_some(), "no retry for the late network");
    }

    /// Repeated 503s with Retry-After 1s slow down past it, as any failure
    /// backs off. They came back every second, the whole fleet in step.
    #[tokio::test]
    async fn repeated_503s_back_off_past_their_retry_after() {
        fastrand::seed(11);
        let (mut sync, _) = sync_loop();
        let after = Duration::from_secs(1);
        let waits: Vec<Duration> = (0..8)
            .map(|_| sync.failed(Some(after), "503".into()) - Instant::now())
            .collect();
        // Within a scheduling tick of the floor.
        let floor = after - Duration::from_millis(50);
        assert!(waits.iter().all(|w| *w >= floor), "{waits:?}");
        assert!(waits.iter().any(|w| *w > 2 * after), "{waits:?}");
    }

    fn body(snapshot: &Snapshot) -> bytes::Bytes {
        serde_json::to_vec(snapshot).unwrap().into()
    }

    /// A refused version stays in every report until a snapshot applies,
    /// and the 204s confirming it do not make the config served look
    /// current. A rebind reported the older version with no error, and
    /// each 204 reset the config age.
    #[tokio::test]
    async fn a_refusal_stays_visible_until_a_snapshot_applies() {
        let (mut sync, _) = sync_loop();
        let metrics: &'static ControllerSync = Box::leak(Box::new(ControllerSync::new()));
        sync.metrics = metrics;
        metrics.start();
        let served = snapshot();
        sync.handle(Answer::Snapshot(body(&served))).await;
        assert_eq!(sync.report.as_ref().unwrap().applied, served.version);

        let mut newer = serde_json::to_value(snapshot()).unwrap();
        newer["protocol"] = serde_json::json!(PROTOCOL + 1);
        newer["version"] = serde_json::json!("refused-version");
        sync.handle(Answer::Snapshot(serde_json::to_vec(&newer).unwrap().into()))
            .await;
        let refused = |sync: &SyncLoop| {
            let report = sync.report.as_ref().unwrap();
            report.applied == "refused-version" && report.apply_error.is_some()
        };
        assert!(refused(&sync));

        let pause = Duration::from_millis(50);
        tokio::time::sleep(pause).await;
        sync.handle(Answer::Unchanged).await;
        assert!(
            metrics.age().unwrap() >= pause,
            "a 204 for the refused version"
        );

        sync.reapply().await;
        assert!(
            refused(&sync),
            "a rebind hid the refusal: {:?}",
            sync.report
        );

        let next = Snapshot::from_config(
            crate::sync::GatewayRef {
                generation: 2,
                ..served.gateway.clone()
            },
            served.config.clone(),
            vec![],
        );
        sync.handle(Answer::Snapshot(body(&next))).await;
        let report = sync.report.as_ref().unwrap();
        assert_eq!(
            (report.applied.as_str(), &report.apply_error),
            (next.version.as_str(), &None)
        );
        sync.handle(Answer::Unchanged).await;
        assert!(metrics.age().unwrap() < pause);
    }
}
