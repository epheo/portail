//! Backends of kind Gateway: another Gateway of this controller, reached
//! on a network both share, at the addresses its own data planes bound.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::net::IpAddr;
use std::sync::{Mutex, MutexGuard};

use futures::channel::mpsc;

use super::hub::Hub;
use super::reports::ReportStore;
use crate::config::{parse_gateway_backend_name, BackendRef, PortailConfig};
use crate::logging::debug;
use crate::proxy::proxy_protocol::Cidr;
use crate::sync::{BackendFrom, GatewayKey, NetworkCidrs, PendingBackend, Snapshot, Transport};

/// Where a Gateway can be reached: per (Network name, listener port), the
/// address and bound port of each live instance there; under
/// [`PRESENCE`], each live instance's address on the Network, bound or
/// not.
pub(crate) type Exposure = BTreeMap<(String, u16), BTreeSet<(String, u16)>>;

/// The port of a Network's presence entry: no listener and no backendRef
/// port is 0. A truster trusts presence, so its changes wake trusters.
const PRESENCE: u16 = 0;

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
    /// Per Gateway, the addresses its last published snapshot trusts for
    /// each Gateway it names: what a failed pass drops when one is gone.
    trusted: HashMap<GatewayKey, BTreeMap<GatewayKey, Vec<String>>>,
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
        state.trusted.retain(|truster, _| live(truster));
        let gone: Vec<GatewayKey> = state.exposed.keys().filter(|k| !live(k)).cloned().collect();
        for target in gone {
            state.exposed.remove(&target);
            for referrer in state.dependents(&target) {
                let _ = self.wakes.unbounded_send(referrer);
            }
        }
    }

    /// The addresses `truster`'s published snapshot trusts, by the Gateway
    /// they were trusted for.
    pub(crate) fn record_trusted(
        &self,
        truster: GatewayKey,
        trusted: BTreeMap<GatewayKey, Vec<String>>,
    ) {
        let mut state = self.state();
        if trusted.is_empty() {
            state.trusted.remove(&truster);
        } else {
            state.trusted.insert(truster, trusted);
        }
    }

    pub(crate) fn trusted_by(&self, truster: &GatewayKey) -> BTreeMap<GatewayKey, Vec<String>> {
        self.state()
            .trusted
            .get(truster)
            .cloned()
            .unwrap_or_default()
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
            let present = out.entry((network.name.clone(), PRESENCE)).or_default();
            if present.len() < MAX_ENDPOINTS {
                present.insert((ip.clone(), PRESENCE));
            } else {
                truncated(target);
            }
            // One bound port per listener on one address: a report listing
            // more never crowds out the instances visited after it.
            for listener in &served.config.gateway.listeners {
                let mut ports = report
                    .listeners
                    .iter()
                    .filter(|l| l.name == listener.name)
                    .flat_map(|l| &l.endpoints)
                    .filter(|e| e.bound && e.transport == Transport::Tcp)
                    .filter(|e| e.address.as_ref().is_none_or(|a| a == ip))
                    .map(|e| e.port);
                let Some(port) = ports.next() else {
                    continue;
                };
                let set = out
                    .entry((network.name.clone(), listener.port))
                    .or_default();
                if ports.next().is_some() || set.len() >= MAX_ENDPOINTS {
                    truncated(target);
                }
                if set.len() < MAX_ENDPOINTS {
                    set.insert((ip.clone(), port));
                }
            }
        }
    });
    out.retain(|_, endpoints| !endpoints.is_empty());
    out
}

/// A cap left addresses of `gateway`'s reports out.
fn truncated(gateway: &GatewayKey) {
    crate::metrics::METRICS
        .gateway_addresses_truncated_total
        .inc();
    debug!(
        "Gateway {}: its reports go over the address cap; the first instances by id are kept",
        gateway
    );
}

/// Host CIDRs a Gateway on `networks` trusts for `named`: the IP each live
/// instance of `named` reports on one of those Networks, by name, inside
/// that Network's CIDRs on both sides, bound or not: its presence there,
/// which `named`'s exposure tracks, so any change wakes the truster. Never an
/// address on another network, which a recycled pod IP could give a
/// tenant. Nothing without a shared Network or a live report: trusting
/// less closes connections, trusting more lets a sender name any client.
/// An IP a live instance of another Gateway reports is left out: the
/// network's IPAM may have handed it on from an instance not yet lapsed.
pub(crate) fn trusted_addresses(
    hub: &Hub,
    reports: &ReportStore,
    named: &GatewayKey,
    networks: &[NetworkCidrs],
) -> Vec<String> {
    let Some(served) = hub.current(named) else {
        return Vec::new();
    };
    let parse = |n: &NetworkCidrs| -> Vec<Cidr> {
        n.cidrs.iter().filter_map(|c| Cidr::parse(c).ok()).collect()
    };
    let shared: Vec<(&str, Vec<Cidr>, Vec<Cidr>)> = networks
        .iter()
        .filter_map(|mine| {
            let theirs = served.networks.iter().find(|n| n.name == mine.name)?;
            Some((mine.name.as_str(), parse(mine), parse(theirs)))
        })
        .collect();
    if shared.is_empty() {
        return Vec::new();
    }
    let mut found: BTreeMap<&str, BTreeSet<IpAddr>> = BTreeMap::new();
    reports.for_each_live(named, |report| {
        for (name, mine, theirs) in &shared {
            let Some(ip) = report.networks.get(*name) else {
                continue;
            };
            let Ok(addr) = ip.parse::<IpAddr>().map(|a| a.to_canonical()) else {
                continue;
            };
            let inside = |cidrs: &[Cidr]| cidrs.iter().any(|c| c.contains(addr));
            if !inside(mine) || !inside(theirs) {
                continue;
            }
            let set = found.entry(name).or_default();
            if set.len() < MAX_ENDPOINTS {
                set.insert(addr);
            } else {
                truncated(named);
            }
        }
    });
    let candidates: HashSet<IpAddr> = found.into_values().flatten().collect();
    let claimed = reports.claimed_besides(named, &candidates);
    let mut out: Vec<String> = candidates
        .difference(&claimed)
        .map(|ip| match ip {
            IpAddr::V4(_) => format!("{ip}/32"),
            IpAddr::V6(_) => format!("{ip}/128"),
        })
        .collect();
    out.sort();
    out
}

/// What a pass that fails publishes of `served`: all but the endpoints of
/// the Gateways `gone` says are gone, and of the addresses `trusted`
/// attributes to a named Gateway, only those it would trust `now`
/// (nothing once gone). None when that changes nothing; else the snapshot
/// to publish, the trust it keeps by Gateway, and the Gateways it dropped
/// something of.
pub(crate) fn without_gone(
    served: &Snapshot,
    trusted: &BTreeMap<GatewayKey, Vec<String>>,
    gone: impl Fn(&GatewayKey) -> bool,
    now: impl Fn(&GatewayKey) -> Vec<String>,
) -> Option<(
    Snapshot,
    BTreeMap<GatewayKey, Vec<String>>,
    BTreeSet<GatewayKey>,
)> {
    let mut dropped = BTreeSet::new();
    let mut out = served.clone();
    out.backends.external_names.retain(|entry| {
        let Some((name, namespace)) = parse_gateway_backend_name(&entry.host) else {
            return true;
        };
        let key = GatewayKey::new(namespace, name);
        let keep = !gone(&key);
        if !keep {
            dropped.insert(key);
        }
        keep
    });
    let named_gone = |name: &str| {
        name.parse::<GatewayKey>()
            .ok()
            .or_else(|| parse_gateway_backend_name(name).map(|(gw, ns)| GatewayKey::new(ns, gw)))
            .is_some_and(|key| gone(&key))
    };
    out.pending.backends.retain(|b| !named_gone(&b.host));
    out.pending.trusted.retain(|t| !named_gone(&t.gateway));
    out.backends_from.retain(|b| !named_gone(&b.host));
    let mut changed = out.backends.external_names.len() != served.backends.external_names.len()
        || out.pending != served.pending
        || out.backends_from != served.backends_from;
    let mut kept_for = BTreeMap::new();
    for (named, ips) in trusted {
        let still: Vec<String> = if gone(named) {
            Vec::new()
        } else {
            let now = now(named);
            ips.iter().filter(|ip| now.contains(ip)).cloned().collect()
        };
        if still.len() != ips.len() {
            dropped.insert(named.clone());
        }
        if !still.is_empty() {
            kept_for.insert(named.clone(), still);
        }
    }
    let kept: HashSet<&String> = kept_for.values().flatten().collect();
    let drop: HashSet<&String> = trusted
        .values()
        .flatten()
        .filter(|ip| !kept.contains(ip))
        .collect();
    out.trusted_from = kept_for
        .iter()
        .map(|(named, sources): (&GatewayKey, &Vec<String>)| {
            let gateway = named.to_string();
            let uid = served
                .trusted_from
                .iter()
                .find(|t| t.gateway == gateway)
                .map(|t| t.uid.clone())
                .unwrap_or_default();
            crate::sync::TrustedFrom {
                gateway,
                uid,
                sources: sources.clone(),
            }
        })
        .collect();
    changed |= out.trusted_from != served.trusted_from;
    if let Some(proxy) = out.config.gateway.proxy_protocol.as_mut() {
        let before = proxy.trusted_sources.len();
        proxy
            .trusted_sources
            .retain(|source| !drop.contains(source));
        changed |= proxy.trusted_sources.len() != before;
        if proxy.trusted_sources.is_empty() {
            out.config.gateway.proxy_protocol = None;
        }
    }
    changed.then(|| (out.seal(), kept_for, dropped))
}

/// Fill `config`'s endpoint overrides for its valid Gateway backends: the
/// target's exposure on each of the referencing Gateway's own Networks,
/// by name, inside that Network's CIDRs. None yet: no override, and the
/// share answers 503 until an instance of the target reports.
pub(crate) fn resolve(
    hub: &Hub,
    reports: &ReportStore,
    config: &mut PortailConfig,
    networks: &[NetworkCidrs],
) {
    let refs: BTreeSet<(String, u16)> = gateway_refs(config)
        .filter(|br| br.is_gateway() && !br.invalid && br.port != PRESENCE)
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

/// Whether this controller has yet to hear from `named`: it serves no
/// snapshot of it, or no instance of it is live. After a start or a
/// takeover both stores begin empty; what spec says is never read here.
pub(crate) fn unheard(hub: &Hub, reports: &ReportStore, named: &GatewayKey) -> bool {
    if hub.current(named).is_none() {
        return true;
    }
    let mut live = false;
    reports.for_each_live(named, |_| live = true);
    !live
}

/// The Gateway backends of `config` that spec allows (its refs passed
/// every check) and that resolved to no endpoint because their target is
/// [`unheard`], each with the target's uid. A target without one is not
/// there to wait for. Sorted.
pub(crate) fn unheard_backends(
    hub: &Hub,
    reports: &ReportStore,
    config: &PortailConfig,
    uid_of: impl Fn(&GatewayKey) -> Option<String>,
) -> Vec<PendingBackend> {
    let unresolved: BTreeSet<(String, u16)> = gateway_refs(config)
        .filter(|br| br.is_gateway() && !br.invalid && br.port != PRESENCE)
        .filter(|br| {
            !config
                .endpoint_overrides
                .contains_key(&(br.name.clone(), br.port))
        })
        .map(|br| (br.name.clone(), br.port))
        .collect();
    unresolved
        .into_iter()
        .filter_map(|(host, port)| {
            let (gateway, namespace) = parse_gateway_backend_name(&host)?;
            let target = GatewayKey::new(namespace, gateway);
            if !unheard(hub, reports, &target) {
                return None;
            }
            let uid = uid_of(&target)?;
            Some(PendingBackend { host, port, uid })
        })
        .collect()
}

/// The uid of the target each resolved Gateway backend of `config` came
/// from. Sorted.
pub(crate) fn backends_from(
    config: &PortailConfig,
    uid_of: impl Fn(&GatewayKey) -> Option<String>,
) -> Vec<BackendFrom> {
    let mut out: Vec<BackendFrom> = config
        .endpoint_overrides
        .keys()
        .filter_map(|(host, port)| {
            let (gateway, namespace) = parse_gateway_backend_name(host)?;
            let uid = uid_of(&GatewayKey::new(namespace, gateway))?;
            Some(BackendFrom {
                host: host.clone(),
                port: *port,
                uid,
            })
        })
        .collect();
    out.sort();
    out
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
            listeners: true,
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

    /// What a Gateway on transit trusts for a Gateway it names: each live
    /// instance's transit address, inside transit on both sides, where it
    /// bound a listener, as a host. Never another interface, never an
    /// address another Gateway's live instance reports, and nothing
    /// without a report or a shared Network.
    #[test]
    fn trusted_addresses_are_the_named_gateways_own_on_a_shared_network() {
        let (hub, reports) = (hub(), ReportStore::default());
        let trusted =
            |networks: &[NetworkCidrs]| trusted_addresses(&hub, &reports, &target(), networks);
        assert!(trusted(&transit()).is_empty(), "no report");

        reports.record(
            &target(),
            InstanceId("a".into()),
            report(Some("10.250.0.4"), None, &["10.128.0.4", "10.250.0.4"]),
        );
        reports.record(
            &target(),
            InstanceId("b".into()),
            report(None, None, &["10.250.0.5"]),
        );
        let mut unbound = report(Some("10.250.0.6"), None, &["10.250.0.6"]);
        unbound.listeners[0].endpoints[0].bound = false;
        reports.record(&target(), InstanceId("c".into()), unbound);
        reports.record(
            &target(),
            InstanceId("d".into()),
            report(Some("10.251.0.7"), None, &["10.251.0.7"]),
        );
        assert_eq!(
            trusted(&transit()),
            ["10.250.0.4/32", "10.250.0.6/32"],
            "b names no transit address, d is outside transit; c is trusted though its \
             listener failed to bind there"
        );
        let elsewhere = vec![NetworkCidrs {
            name: "other".into(),
            cidrs: vec!["10.250.0.0/16".into()],
            listeners: true,
        }];
        assert!(
            trusted(&elsewhere).is_empty(),
            "a network of the same CIDRs but another name"
        );
        assert!(trusted(&[]).is_empty(), "no Network address of its own");
        let narrower = vec![NetworkCidrs {
            name: "transit".into(),
            cidrs: vec!["10.250.1.0/24".into()],
            listeners: true,
        }];
        assert!(trusted(&narrower).is_empty(), "outside the truster's CIDRs");

        // The network's IPAM handed a's address on before a lapsed.
        reports.record(
            &GatewayKey::new("q-publish", "publish"),
            InstanceId("q".into()),
            report(Some("10.250.0.4"), None, &["10.250.0.4"]),
        );
        assert_eq!(
            trusted(&transit()),
            ["10.250.0.6/32"],
            "claimed by another Gateway"
        );
    }

    /// A report listing many ports never crowds out the others: each
    /// instance counts once per listener, and the cap is counted when it
    /// bites.
    #[test]
    fn a_crowded_report_counts_once_per_listener() {
        let (hub, reports) = (hub(), ReportStore::default());
        let mut crowded = report(Some("10.250.0.4"), None, &["10.250.0.4"]);
        let endpoint = crowded.listeners[0].endpoints[0].clone();
        crowded.listeners[0].endpoints = (0..200)
            .map(|i| EndpointReport {
                port: 9000 + i,
                ..endpoint.clone()
            })
            .collect();
        reports.record(&target(), InstanceId("a".into()), crowded);
        reports.record(
            &target(),
            InstanceId("b".into()),
            report(Some("10.250.0.5"), None, &["10.250.0.5"]),
        );
        let before = crate::metrics::METRICS
            .gateway_addresses_truncated_total
            .get();
        let exposed = exposure(&hub, &reports, &target());
        assert_eq!(
            exposed[&("transit".to_string(), 80)],
            [
                ("10.250.0.4".to_string(), 9000),
                ("10.250.0.5".to_string(), 8000)
            ]
            .into()
        );
        assert!(
            crate::metrics::METRICS
                .gateway_addresses_truncated_total
                .get()
                > before
        );
    }

    /// More instances than the cap: the first by id are kept, the same
    /// ones on every pass, for backends and trust alike.
    #[test]
    fn the_cap_keeps_the_first_instances_by_id() {
        let (hub, reports) = (hub(), ReportStore::default());
        for i in 0..70u8 {
            let ip = format!("10.250.1.{i}");
            reports.record(
                &target(),
                InstanceId(format!("i{i:03}")),
                report(Some(&ip), None, &[&ip]),
            );
        }
        let exposed = &exposure(&hub, &reports, &target())[&("transit".to_string(), 80)];
        assert_eq!(exposed.len(), MAX_ENDPOINTS);
        assert!(exposed.contains(&("10.250.1.0".to_string(), 8000)));
        assert!(exposed.contains(&("10.250.1.63".to_string(), 8000)));
        assert!(!exposed.contains(&("10.250.1.64".to_string(), 8000)));

        let trusted = trusted_addresses(&hub, &reports, &target(), &transit());
        assert_eq!(trusted.len(), MAX_ENDPOINTS);
        assert!(trusted.contains(&"10.250.1.63/32".to_string()));
        assert!(!trusted.contains(&"10.250.1.64/32".to_string()));
    }

    /// What a failed pass keeps: all but the endpoints of, and the trust
    /// in, a Gateway gone. An address also trusted for a Gateway still
    /// there stays. Nothing of a gone Gateway: nothing to republish.
    #[test]
    fn a_failed_pass_drops_only_what_gone_gateways_gave() {
        let config: PortailConfig = serde_json::from_value(serde_json::json!({
            "gateway": {"name": "edge", "listeners": [
                {"name": "http", "protocol": "HTTP", "port": 80}
            ], "proxyProtocol": {"trustedSources": [
                "192.0.2.0/24", "10.250.0.4/32", "10.250.0.5/32"
            ]}}
        }))
        .unwrap();
        let gateway = GatewayRef {
            namespace: "edge".into(),
            name: "public".into(),
            uid: "u".into(),
            generation: 1,
        };
        let mut served = Snapshot::from_config(gateway, config, vec![]);
        let entry = |host: &str| crate::sync::ExternalName {
            host: host.into(),
            port: 80,
            targets: vec![("10.250.0.7".into(), 8000)],
        };
        served.backends.external_names = vec![
            entry("publish.p-publish.gateway"),
            entry("publish.q-publish.gateway"),
            entry("web.apps.svc"),
        ];
        let from = |gateway: &str, sources: &[&str]| crate::sync::TrustedFrom {
            gateway: gateway.into(),
            uid: format!("{gateway}-uid"),
            sources: sources.iter().map(|s| s.to_string()).collect(),
        };
        served.trusted_from = vec![
            from("p-publish/publish", &["10.250.0.4/32", "10.250.0.5/32"]),
            from("q-publish/publish", &["10.250.0.5/32"]),
        ];
        served.pending.trusted = vec![crate::sync::PendingTrusted {
            gateway: "p-publish/publish".into(),
            uid: "p-uid".into(),
        }];
        let served = served.seal();
        let (p, q) = (target(), GatewayKey::new("q-publish", "publish"));
        let trusted: BTreeMap<GatewayKey, Vec<String>> = [
            (
                p.clone(),
                vec!["10.250.0.4/32".into(), "10.250.0.5/32".into()],
            ),
            (q.clone(), vec!["10.250.0.5/32".into()]),
        ]
        .into();

        let as_trusted = |k: &GatewayKey| trusted.get(k).cloned().unwrap_or_default();
        let (pruned, kept, dropped) =
            without_gone(&served, &trusted, |k| *k == p, as_trusted).unwrap();
        assert_eq!(dropped, [p.clone()].into());
        assert_eq!(
            kept,
            [(q.clone(), vec!["10.250.0.5/32".to_string()])].into()
        );
        let hosts: Vec<&str> = pruned
            .backends
            .external_names
            .iter()
            .map(|e| e.host.as_str())
            .collect();
        assert_eq!(hosts, ["publish.q-publish.gateway", "web.apps.svc"]);
        assert_eq!(
            pruned.trusted_from,
            [from("q-publish/publish", &["10.250.0.5/32"])],
            "provenance follows the trust kept"
        );
        assert!(
            pruned.pending.is_empty(),
            "a gone Gateway is pending no more"
        );
        assert_eq!(
            pruned
                .config
                .gateway
                .proxy_protocol
                .unwrap()
                .trusted_sources,
            ["192.0.2.0/24", "10.250.0.5/32"],
            "10.250.0.5 is still trusted for q"
        );
        assert_ne!(pruned.version, served.version);

        let (lapsed, kept, dropped) =
            without_gone(&served, &trusted, |_| false, |_| Vec::new()).unwrap();
        assert_eq!(dropped, [p, q].into(), "live, but lapsed");
        assert!(kept.is_empty());
        assert_eq!(lapsed.backends.external_names.len(), 3);
        assert_eq!(
            lapsed
                .config
                .gateway
                .proxy_protocol
                .unwrap()
                .trusted_sources,
            ["192.0.2.0/24"]
        );

        assert!(
            without_gone(&served, &trusted, |_| false, as_trusted).is_none(),
            "nothing moved: nothing to republish"
        );
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
            listeners: true,
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
        assert_eq!(
            exposed[&("transit".into(), 80)],
            [("10.250.0.7".to_string(), 0)].into(),
            "one port per instance and listener"
        );
    }

    /// A truster wakes on the named Gateway's presence on the Network,
    /// whether or not a listener bound there.
    #[test]
    fn dependents_wake_on_presence_alone() {
        let (hub, reports) = (hub(), ReportStore::default());
        let backends = GatewayBackends::default();
        let mut wakes = backends.wakes().unwrap();
        let truster = GatewayKey::new("p-publish", "truster");
        backends.record(truster.clone(), [target()].into());
        let mut unbound = report(Some("10.250.0.7"), None, &["10.250.0.7"]);
        unbound.listeners[0].endpoints[0].bound = false;
        reports.record(&target(), InstanceId("a".into()), unbound.clone());
        backends.refresh(&target(), &hub, &reports);
        assert_eq!(
            wakes.try_recv().ok(),
            Some(truster.clone()),
            "present, bound nowhere"
        );
        unbound
            .networks
            .insert("transit".into(), "10.250.0.8".into());
        reports.record(&target(), InstanceId("a".into()), unbound);
        backends.refresh(&target(), &hub, &reports);
        assert_eq!(wakes.try_recv().ok(), Some(truster), "its presence moved");
    }

    /// A backendRef port is never 0: the presence entry is never dialed.
    #[test]
    fn presence_is_never_an_endpoint() {
        let (hub, reports) = (hub(), ReportStore::default());
        reports.record(
            &target(),
            InstanceId("a".into()),
            report(Some("10.250.0.7"), None, &["10.250.0.7"]),
        );
        let mut config = edge_config(0);
        resolve(&hub, &reports, &mut config, &transit());
        assert!(config.endpoint_overrides.is_empty());
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
