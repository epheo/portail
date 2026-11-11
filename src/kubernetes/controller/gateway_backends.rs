//! Backends of kind Gateway: another Gateway of this controller, reached
//! on a network both share, at the addresses its own data planes bound.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::IpAddr;
use std::sync::{Mutex, MutexGuard};

use futures::channel::mpsc;

use super::hub::Hub;
use super::reports::ReportStore;
use crate::config::{BackendRef, PortailConfig};
use crate::kubernetes::converters::parse_gateway_backend_name;
use crate::proxy::proxy_protocol::Cidr;
use crate::sync::{GatewayKey, NetworkCidrs, Transport};

/// Where a Gateway can be reached: per (Network name, listener port), the
/// address and bound port of each live instance there.
pub(crate) type Exposure = BTreeMap<(String, u16), BTreeSet<(String, u16)>>;

/// Endpoints one Network and port take at most: a Gateway runs a few
/// replicas, so more is a forged or runaway report.
const MAX_ENDPOINTS: usize = 64;

/// Who names whom, and what each named Gateway exposes. A Gateway naming
/// another is reconciled when that one's exposure changes, never on its
/// other report churn: a busy target would keep the debounce from ever
/// letting its edge run.
pub(crate) struct GatewayBackends {
    state: Mutex<State>,
    wakes: mpsc::UnboundedSender<GatewayKey>,
    receiver: Mutex<Option<mpsc::UnboundedReceiver<GatewayKey>>>,
}

#[derive(Default)]
struct State {
    /// What each Gateway names, as of its last translation.
    targets: HashMap<GatewayKey, BTreeSet<GatewayKey>>,
    /// Each Gateway's exposure when last refreshed; none when empty.
    exposed: HashMap<GatewayKey, Exposure>,
}

impl State {
    fn dependents(&self, target: &GatewayKey) -> Vec<GatewayKey> {
        self.targets
            .iter()
            .filter(|(_, targets)| targets.contains(target))
            .map(|(referrer, _)| referrer.clone())
            .collect()
    }
}

impl Default for GatewayBackends {
    fn default() -> Self {
        let (wakes, receiver) = mpsc::unbounded();
        Self {
            state: Mutex::default(),
            wakes,
            receiver: Mutex::new(Some(receiver)),
        }
    }
}

impl GatewayBackends {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// What `referrer` names as of its last translation.
    pub(crate) fn record(&self, referrer: GatewayKey, targets: BTreeSet<GatewayKey>) {
        let mut state = self.state();
        if targets.is_empty() {
            state.targets.remove(&referrer);
        } else {
            state.targets.insert(referrer, targets);
        }
    }

    /// Recompute `target`'s exposure; when it moved, wake the Gateways
    /// naming it. Run on its reports, its lapses, the end of its
    /// reconcile, and when it leaves the hub.
    pub(crate) fn refresh(&self, target: &GatewayKey, hub: &Hub, reports: &ReportStore) {
        let now = exposure(hub, reports, target);
        let mut state = self.state();
        let unchanged = match state.exposed.get(target) {
            Some(before) => *before == now,
            None => now.is_empty(),
        };
        if unchanged {
            return;
        }
        if now.is_empty() {
            state.exposed.remove(target);
        } else {
            state.exposed.insert(target.clone(), now);
        }
        for referrer in state.dependents(target) {
            let _ = self.wakes.unbounded_send(referrer);
        }
    }

    /// Forget every Gateway `live` rejects: what it named, and what it
    /// exposed, which wakes those naming it.
    pub(crate) fn retain(&self, live: impl Fn(&GatewayKey) -> bool) {
        let mut state = self.state();
        state.targets.retain(|referrer, _| live(referrer));
        let gone: Vec<GatewayKey> = state.exposed.keys().filter(|k| !live(k)).cloned().collect();
        for target in gone {
            state.exposed.remove(&target);
            for referrer in state.dependents(&target) {
                let _ = self.wakes.unbounded_send(referrer);
            }
        }
    }

    /// Gateways to reconcile because a Gateway they name moved. Taken
    /// once, by the controller.
    pub(crate) fn wakes(&self) -> Option<mpsc::UnboundedReceiver<GatewayKey>> {
        self.receiver
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }

    #[cfg(test)]
    pub(crate) fn dependents(&self, target: &GatewayKey) -> Vec<GatewayKey> {
        self.state().dependents(target)
    }
}

/// `target`'s exposure: for each Network its served snapshot names, the IP
/// each live instance reports on it, inside that Network's CIDRs, at each
/// port it bound there (on that IP or all interfaces). An interface on
/// another network never counts, whatever its address.
pub(crate) fn exposure(hub: &Hub, reports: &ReportStore, target: &GatewayKey) -> Exposure {
    let mut out = Exposure::new();
    let Some(served) = hub.current(target) else {
        return out;
    };
    let networks: Vec<(&NetworkCidrs, Vec<Cidr>)> = served
        .networks
        .iter()
        .map(|n| {
            (
                n,
                n.cidrs.iter().filter_map(|c| Cidr::parse(c).ok()).collect(),
            )
        })
        .collect();
    reports.for_each_live(target, |report| {
        for (network, cidrs) in &networks {
            let Some(ip) = report.networks.get(&network.name) else {
                continue;
            };
            let on_network = ip
                .parse::<IpAddr>()
                .is_ok_and(|addr| cidrs.iter().any(|c| c.contains(addr)));
            if !on_network {
                continue;
            }
            for listener in &served.config.gateway.listeners {
                let ports = report
                    .listeners
                    .iter()
                    .filter(|l| l.name == listener.name)
                    .flat_map(|l| &l.endpoints)
                    .filter(|e| e.bound && e.transport == Transport::Tcp)
                    .filter(|e| e.address.as_ref().is_none_or(|a| a == ip))
                    .map(|e| e.port);
                let set = out
                    .entry((network.name.clone(), listener.port))
                    .or_default();
                for port in ports {
                    if set.len() >= MAX_ENDPOINTS {
                        break;
                    }
                    set.insert((ip.clone(), port));
                }
            }
        }
    });
    out.retain(|_, endpoints| !endpoints.is_empty());
    out
}

/// Fill `config`'s endpoint overrides for its valid Gateway backends: the
/// target's exposure on each of the referencing Gateway's own Networks,
/// by name, inside that Network's CIDRs. None yet: no override, and the
/// share is rejected until an instance of the target reports.
pub(crate) fn resolve(
    hub: &Hub,
    reports: &ReportStore,
    config: &mut PortailConfig,
    networks: &[NetworkCidrs],
) {
    let refs: BTreeSet<(String, u16)> = gateway_refs(config)
        .filter(|br| br.is_gateway() && !br.invalid)
        .map(|br| (br.name.clone(), br.port))
        .collect();
    let networks: Vec<(&NetworkCidrs, Vec<Cidr>)> = networks
        .iter()
        .map(|n| {
            (
                n,
                n.cidrs.iter().filter_map(|c| Cidr::parse(c).ok()).collect(),
            )
        })
        .collect();
    let mut exposures: HashMap<GatewayKey, Exposure> = HashMap::new();
    for (name, port) in refs {
        let Some((gateway, namespace)) = parse_gateway_backend_name(&name) else {
            continue;
        };
        let key = GatewayKey::new(namespace, gateway);
        let exposed = exposures
            .entry(key.clone())
            .or_insert_with(|| exposure(hub, reports, &key));
        let endpoints: BTreeSet<(String, u16)> = networks
            .iter()
            .flat_map(|(network, cidrs)| {
                exposed
                    .get(&(network.name.clone(), port))
                    .into_iter()
                    .flatten()
                    .filter(|(ip, _)| {
                        ip.parse::<IpAddr>()
                            .is_ok_and(|addr| cidrs.iter().any(|c| c.contains(addr)))
                    })
            })
            .cloned()
            .collect();
        if !endpoints.is_empty() {
            config
                .endpoint_overrides
                .insert((name, port), endpoints.into_iter().collect());
        }
    }
}

fn gateway_refs(config: &PortailConfig) -> impl Iterator<Item = &BackendRef> {
    let http = config
        .http_routes
        .iter()
        .flat_map(|r| &r.rules)
        .flat_map(|r| &r.backend_refs);
    let tcp = config
        .tcp_routes
        .iter()
        .flat_map(|r| &r.rules)
        .flat_map(|r| &r.backend_refs);
    let tls = config
        .tls_routes
        .iter()
        .flat_map(|r| &r.rules)
        .flat_map(|r| &r.backend_refs);
    http.chain(tcp).chain(tls)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::{EndpointReport, GatewayRef, InstanceReport, ListenerReport, Snapshot};

    use super::super::reports::InstanceId;

    fn target() -> GatewayKey {
        GatewayKey::new("p-publish", "publish")
    }

    fn transit() -> Vec<NetworkCidrs> {
        vec![NetworkCidrs {
            name: "transit".into(),
            cidrs: vec!["10.250.0.0/16".into()],
        }]
    }

    /// The target serves `http` on `port`, bound at 8000, on transit.
    fn publish(hub: &Hub, port: u16) {
        let config: PortailConfig = serde_json::from_value(serde_json::json!({
            "gateway": {"name": "publish", "listeners": [
                {"name": "http", "protocol": "HTTP", "port": port, "target_port": 8000}
            ]}
        }))
        .unwrap();
        let gateway = GatewayRef {
            namespace: "p-publish".into(),
            name: "publish".into(),
            uid: "u".into(),
            generation: i64::from(port),
        };
        hub.publish(Snapshot::from_config(gateway, config, transit()));
    }

    fn hub() -> Hub {
        let hub = Hub::default();
        publish(&hub, 80);
        hub
    }

    /// An instance at `network` on transit, `http` bound on `address`
    /// (None: all interfaces) at 8000.
    fn report(
        network: Option<&str>,
        address: Option<&str>,
        interface_ips: &[&str],
    ) -> InstanceReport {
        InstanceReport {
            listeners: vec![ListenerReport {
                name: "http".into(),
                endpoints: vec![EndpointReport {
                    address: address.map(String::from),
                    port: 8000,
                    transport: Transport::Tcp,
                    bound: true,
                    error: None,
                }],
            }],
            interface_ips: interface_ips.iter().map(|s| s.to_string()).collect(),
            networks: network
                .map(|ip| [("transit".to_string(), ip.to_string())].into())
                .unwrap_or_default(),
            ..Default::default()
        }
    }

    fn edge_config(port: u16) -> PortailConfig {
        serde_json::from_value(serde_json::json!({
            "httpRoutes": [{"rules": [{"backendRefs": [{
                "name": "publish.p-publish.gateway", "port": port,
                "group": "gateway.networking.k8s.io", "kind": "Gateway"
            }]}]}]
        }))
        .unwrap()
    }

    fn resolved(
        hub: &Hub,
        reports: &ReportStore,
        port: u16,
        networks: &[NetworkCidrs],
    ) -> Vec<(String, u16)> {
        let mut config = edge_config(port);
        resolve(hub, reports, &mut config, networks);
        config
            .endpoint_overrides
            .remove(&("publish.p-publish.gateway".to_string(), port))
            .unwrap_or_default()
    }

    /// The IP each instance reports on the shared Network, at its bound
    /// port: an all-interfaces bind gives that IP alone, never an
    /// interface on another network inside the same CIDR.
    #[test]
    fn endpoints_are_each_instance_on_the_shared_network() {
        let (hub, reports) = (hub(), ReportStore::default());
        let record = |id: &str, r| reports.record(&target(), InstanceId(id.into()), r);
        record(
            "a",
            report(Some("10.250.0.7"), Some("10.250.0.7"), &["10.250.0.7"]),
        );
        record(
            "b",
            report(Some("10.250.0.8"), None, &["10.250.0.8", "10.250.9.9"]),
        );
        record("c", report(Some("10.250.0.10"), Some("10.128.0.11"), &[]));
        record("d", report(None, None, &["10.250.0.12"]));
        record("e", report(Some("10.128.0.13"), None, &["10.128.0.13"]));
        assert_eq!(
            resolved(&hub, &reports, 80, &transit()),
            [
                ("10.250.0.7".to_string(), 8000),
                ("10.250.0.8".to_string(), 8000)
            ]
        );
        assert!(
            resolved(&hub, &reports, 443, &transit()).is_empty(),
            "no listener"
        );
        assert!(
            resolved(&hub, &reports, 80, &[]).is_empty(),
            "the edge is on no network"
        );
        let elsewhere = vec![NetworkCidrs {
            name: "other".into(),
            cidrs: vec!["10.250.0.0/16".into()],
        }];
        assert!(
            resolved(&hub, &reports, 80, &elsewhere).is_empty(),
            "another Network, the same CIDR"
        );
    }

    /// A report as large as its sender likes adds at most MAX_ENDPOINTS
    /// per Network and port.
    #[test]
    fn a_runaway_report_is_capped() {
        let (hub, reports) = (hub(), ReportStore::default());
        let mut big = report(Some("10.250.0.7"), None, &[]);
        big.listeners[0].endpoints = (0..10_000)
            .map(|port| EndpointReport {
                address: None,
                port,
                transport: Transport::Tcp,
                bound: true,
                error: None,
            })
            .collect();
        reports.record(&target(), InstanceId("a".into()), big);
        let exposed = exposure(&hub, &reports, &target());
        assert_eq!(exposed[&("transit".into(), 80)].len(), MAX_ENDPOINTS);
    }

    /// Dependents wake when the target's exposure moves: an address, a
    /// listener, the target gone. A new version applied or a DNS refresh
    /// moves nothing.
    #[test]
    fn dependents_wake_only_when_the_exposure_moves() {
        let (hub, reports) = (hub(), ReportStore::default());
        let backends = GatewayBackends::default();
        let mut wakes = backends.wakes().unwrap();
        let edge = GatewayKey::new("edge", "public");
        backends.record(edge.clone(), [target()].into());
        let mut woken = || {
            let mut out = vec![];
            while let Ok(key) = wakes.try_recv() {
                out.push(key);
            }
            out
        };
        let refresh = || backends.refresh(&target(), &hub, &reports);

        let mut instance = report(Some("10.250.0.7"), None, &[]);
        reports.record(&target(), InstanceId("a".into()), instance.clone());
        refresh();
        assert_eq!(woken(), std::slice::from_ref(&edge), "first exposure");

        instance.applied = "v2".into();
        instance.dns_epoch += 1;
        reports.record(&target(), InstanceId("a".into()), instance.clone());
        refresh();
        assert!(woken().is_empty(), "applied and dns_epoch alone");

        instance
            .networks
            .insert("transit".into(), "10.250.0.8".into());
        reports.record(&target(), InstanceId("a".into()), instance);
        refresh();
        assert_eq!(woken(), std::slice::from_ref(&edge), "an address moved");

        publish(&hub, 81);
        refresh();
        assert_eq!(woken(), std::slice::from_ref(&edge), "a listener moved");

        backends.retain(|key| key != &target());
        assert_eq!(woken(), std::slice::from_ref(&edge), "the target is gone");
        backends.retain(|key| key != &target());
        assert!(woken().is_empty(), "once");

        backends.record(edge.clone(), Default::default());
        refresh();
        assert!(woken().is_empty(), "the edge names it no more");
        assert!(backends.dependents(&target()).is_empty());
    }
}
