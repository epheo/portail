//! What each data-plane instance last reported, by Gateway: Programmed and
//! status.addresses are computed from the live ones. A changed report
//! reconciles its Gateway.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use futures::channel::mpsc;
use tokio::time::{Duration, Instant};

use crate::sync::{GatewayKey, InstanceReport, HOLD};

/// A data-plane instance: the uid of the pod its token is bound to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct InstanceId(pub String);

/// A report moved: reconcile its Gateway.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Trigger {
    pub gateway: GatewayKey,
    /// Backend pods came or went: named targetPorts are read again.
    pub dns_moved: bool,
}

struct Entry {
    report: InstanceReport,
    seen: Instant,
}

/// A data plane polls again at once after each answer, and an answer comes
/// within a hold: an instance silent for two holds is gone.
pub(super) const LIVENESS: Duration = Duration::from_secs(2 * HOLD.as_secs());

pub(crate) struct ReportStore {
    gateways: Mutex<HashMap<GatewayKey, HashMap<InstanceId, Entry>>>,
    triggers: mpsc::UnboundedSender<Trigger>,
    receiver: Mutex<Option<mpsc::UnboundedReceiver<Trigger>>>,
}

impl Default for ReportStore {
    fn default() -> Self {
        let (triggers, receiver) = mpsc::unbounded();
        Self {
            gateways: Mutex::default(),
            triggers,
            receiver: Mutex::new(Some(receiver)),
        }
    }
}

impl ReportStore {
    fn gateways(&self) -> MutexGuard<'_, HashMap<GatewayKey, HashMap<InstanceId, Entry>>> {
        self.gateways.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Record `instance`'s report for `gateway` and when it was seen. A
    /// report that differs from the instance's last one triggers a
    /// reconcile of the Gateway.
    #[allow(dead_code)] // The sync server records; it serves nothing yet.
    pub(crate) fn record(
        &self,
        gateway: &GatewayKey,
        instance: InstanceId,
        report: InstanceReport,
    ) {
        let mut gateways = self.gateways();
        let instances = gateways.entry(gateway.clone()).or_default();
        instances.retain(|_, e| e.seen.elapsed() < LIVENESS);
        let previous = instances.insert(
            instance,
            Entry {
                report: report.clone(),
                seen: Instant::now(),
            },
        );
        if previous.as_ref().is_some_and(|p| p.report == report) {
            return;
        }
        let dns_moved = previous.is_some_and(|p| p.report.dns_epoch != report.dns_epoch);
        let _ = self.triggers.unbounded_send(Trigger {
            gateway: gateway.clone(),
            dns_moved,
        });
    }

    /// The reports of `gateway`'s live instances: seen within
    /// [`LIVENESS`], not leaving.
    pub(crate) fn live(&self, gateway: &GatewayKey) -> Vec<(InstanceId, InstanceReport)> {
        let gateways = self.gateways();
        let Some(instances) = gateways.get(gateway) else {
            return Vec::new();
        };
        let mut live: Vec<(InstanceId, InstanceReport)> = instances
            .iter()
            .filter(|(_, e)| e.seen.elapsed() < LIVENESS && !e.report.leaving)
            .map(|(id, e)| (id.clone(), e.report.clone()))
            .collect();
        live.sort_by(|a, b| a.0 .0.cmp(&b.0 .0));
        live
    }

    /// Forget every Gateway `live` rejects.
    pub(crate) fn retain(&self, live: impl Fn(&GatewayKey) -> bool) {
        self.gateways().retain(|key, _| live(key));
    }

    /// Every report change from here on. Taken once, by the controller.
    pub(crate) fn triggers(&self) -> Option<mpsc::UnboundedReceiver<Trigger>> {
        self.receiver
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn report(applied: &str, dns_epoch: u64) -> InstanceReport {
        InstanceReport {
            applied: applied.into(),
            dns_epoch,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_changed_report_triggers_its_gateway() {
        let store = ReportStore::default();
        let mut triggers = store.triggers().unwrap();
        assert!(store.triggers().is_none(), "taken once");
        let gw = GatewayKey::new("default", "gw");
        let pod = InstanceId("pod-1".into());

        store.record(&gw, pod.clone(), report("v1", 0));
        assert_eq!(
            triggers.next().await.unwrap(),
            Trigger {
                gateway: gw.clone(),
                dns_moved: false
            }
        );
        store.record(&gw, pod.clone(), report("v1", 0));
        store.record(&gw, pod.clone(), report("v2", 0));
        assert!(!triggers.next().await.unwrap().dns_moved, "v2 only");
        store.record(&gw, pod.clone(), report("v2", 1));
        assert!(triggers.next().await.unwrap().dns_moved);
        assert!(triggers.try_recv().is_err(), "nothing more pending");
    }

    #[tokio::test(start_paused = true)]
    async fn silent_or_leaving_instances_are_not_live() {
        let store = ReportStore::default();
        let gw = GatewayKey::new("default", "gw");
        store.record(&gw, InstanceId("a".into()), report("v1", 0));
        store.record(&gw, InstanceId("b".into()), report("v1", 0));
        assert_eq!(store.live(&gw).len(), 2);

        let mut leaving = report("v1", 0);
        leaving.leaving = true;
        store.record(&gw, InstanceId("b".into()), leaving);
        let live = store.live(&gw);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].0, InstanceId("a".into()));

        tokio::time::advance(LIVENESS).await;
        assert!(store.live(&gw).is_empty(), "silent for two holds");

        store.retain(|_| false);
        assert!(store.live(&gw).is_empty());
    }
}
