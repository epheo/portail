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
use crate::metrics::CONTROLLER_SYNC;
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
    let mut client = Client::new(&args.controller_address, &ca, args.token_file.clone())?;

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
            Answer::Unchanged => {
                self.succeeded(true);
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
        self.report = Some(InstanceReport {
            applied: version.clone(),
            apply_error: Some(error),
            listeners: Vec::new(),
            ..previous
        });
        self.have = version;
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
        CONTROLLER_SYNC.set_connected(true);
        if current {
            CONTROLLER_SYNC.confirmed();
        }
        if self.last_error.take().is_some() {
            info!("Sync of Gateway {} recovered", self.gateway);
        }
    }

    fn failed(&mut self, after: Option<Duration>, error: String) -> Instant {
        CONTROLLER_SYNC.set_connected(false);
        let backoff = self.failures.wait();
        // Capped at the hold, the backoff's own ceiling: a Retry-After of
        // hours would strand the data plane.
        let delay = after.map_or(backoff, |after| after.min(HOLD));
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
    /// live. Bounded by the connect timeout: shutdown does not wait on an
    /// unreachable controller.
    async fn leave(&self, client: &mut Client) {
        let Some(report) = &self.report else {
            return;
        };
        let request = SyncRequest {
            // Answered at once rather than held: a reachable controller
            // confirms within one round trip.
            have: String::new(),
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
        assert!(ready.load(Ordering::Acquire));
        assert_eq!(sync.rebind_at, None);

        sync.serve(on(&["here", "not-yet"])).await;
        assert!(ready.load(Ordering::Acquire));
        assert!(sync.rebind_at.is_some(), "no retry for the late network");
    }
}
