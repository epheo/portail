//! Backends of kind Gateway: another Gateway of this controller, reached
//! on a network both share, at the addresses its own data planes report.

use std::collections::{BTreeSet, HashMap};
use std::net::IpAddr;
use std::sync::{Mutex, MutexGuard};

use super::hub::Hub;
use super::reports::ReportStore;
use crate::config::{BackendRef, PortailConfig};
use crate::kubernetes::converters::parse_gateway_backend_name;
use crate::proxy::proxy_protocol::Cidr;
use crate::sync::{GatewayKey, NetworkCidrs, Transport};

/// The Gateways each Gateway names as backends: a report or lapse of one
/// reconciles those that name it, whose endpoints moved.
#[derive(Default)]
pub(crate) struct Dependents(Mutex<HashMap<GatewayKey, BTreeSet<GatewayKey>>>);

impl Dependents {
    fn map(&self) -> MutexGuard<'_, HashMap<GatewayKey, BTreeSet<GatewayKey>>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// What `referrer` names as of its last translation.
    pub(crate) fn record(&self, referrer: GatewayKey, targets: BTreeSet<GatewayKey>) {
        let mut map = self.map();
        if targets.is_empty() {
            map.remove(&referrer);
        } else {
            map.insert(referrer, targets);
        }
    }

    /// The Gateways that name `target`.
    pub(crate) fn of(&self, target: &GatewayKey) -> Vec<GatewayKey> {
        self.map()
            .iter()
            .filter(|(_, targets)| targets.contains(target))
            .map(|(referrer, _)| referrer.clone())
            .collect()
    }

    pub(crate) fn retain(&self, live: impl Fn(&GatewayKey) -> bool) {
        self.map().retain(|referrer, _| live(referrer));
    }
}

/// Fill `config`'s endpoint overrides for its valid Gateway backends and
/// return every Gateway its refs name, valid or not: one that comes to
/// exist, or to report, reconciles this one. An endpoint is where a live instance of
/// the target bound the listener on the ref's port, inside a CIDR of the
/// referencing Gateway's own networks: the address it can reach, and the
/// only one it dials. None yet: the share is rejected until one reports.
pub(crate) fn resolve(
    hub: &Hub,
    reports: &ReportStore,
    config: &mut PortailConfig,
    networks: &[NetworkCidrs],
) -> BTreeSet<GatewayKey> {
    let refs: BTreeSet<(String, u16, bool)> = gateway_refs(config)
        .filter(|br| br.is_gateway())
        .map(|br| (br.name.clone(), br.port, br.invalid))
        .collect();
    if refs.is_empty() {
        return BTreeSet::new();
    }
    let cidrs: Vec<Cidr> = networks
        .iter()
        .flat_map(|n| &n.cidrs)
        .filter_map(|c| Cidr::parse(c).ok())
        .collect();
    let mut targets = BTreeSet::new();
    for (name, port, invalid) in refs {
        let Some((gateway, namespace)) = parse_gateway_backend_name(&name) else {
            continue;
        };
        let key = GatewayKey::new(namespace, gateway);
        if invalid {
            targets.insert(key);
            continue;
        }
        let endpoints = endpoints(hub, reports, &key, port, &cidrs);
        targets.insert(key);
        if !endpoints.is_empty() {
            config.endpoint_overrides.insert((name, port), endpoints);
        }
    }
    targets
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

/// Sorted: equal endpoints keep the snapshot's version.
fn endpoints(
    hub: &Hub,
    reports: &ReportStore,
    target: &GatewayKey,
    port: u16,
    cidrs: &[Cidr],
) -> Vec<(String, u16)> {
    let Some(served) = hub.current(target) else {
        return Vec::new();
    };
    let Some(listener) = served
        .config
        .gateway
        .listeners
        .iter()
        .find(|l| l.port == port)
    else {
        return Vec::new();
    };
    let reachable = |ip: &str| {
        ip.parse::<IpAddr>()
            .is_ok_and(|ip| cidrs.iter().any(|c| c.contains(ip)))
    };
    let mut out: Vec<(String, u16)> = reports
        .live(target)
        .iter()
        .flat_map(|(_, report)| {
            let bound = report
                .listeners
                .iter()
                .filter(|l| l.name == listener.name)
                .flat_map(|l| &l.endpoints)
                .filter(|e| e.bound && e.transport == Transport::Tcp);
            bound
                .flat_map(|e| match &e.address {
                    Some(address) => vec![(address.clone(), e.port)],
                    None => report
                        .interface_ips
                        .iter()
                        .map(|ip| (ip.clone(), e.port))
                        .collect(),
                })
                .collect::<Vec<_>>()
        })
        .filter(|(ip, _)| reachable(ip))
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::{EndpointReport, GatewayRef, InstanceReport, ListenerReport, Snapshot};

    use super::super::reports::InstanceId;

    fn target() -> GatewayKey {
        GatewayKey::new("p-publish", "publish")
    }

    /// The target serves `http` on 80, bound at 8000.
    fn hub() -> Hub {
        let hub = Hub::default();
        let config: PortailConfig = serde_json::from_value(serde_json::json!({
            "gateway": {"name": "publish", "listeners": [
                {"name": "http", "protocol": "HTTP", "port": 80, "target_port": 8000}
            ]}
        }))
        .unwrap();
        let gateway = GatewayRef {
            namespace: "p-publish".into(),
            name: "publish".into(),
            uid: "u".into(),
            generation: 1,
        };
        hub.publish(Snapshot::from_config(gateway, config, vec![]));
        hub
    }

    fn report(address: Option<&str>, interface_ips: &[&str]) -> InstanceReport {
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
            ..Default::default()
        }
    }

    fn edge_config(port: u16, invalid: bool) -> PortailConfig {
        serde_json::from_value(serde_json::json!({
            "httpRoutes": [{"rules": [{"backendRefs": [{
                "name": "publish.p-publish.gateway", "port": port,
                "group": "gateway.networking.k8s.io", "kind": "Gateway", "invalid": invalid
            }]}]}]
        }))
        .unwrap()
    }

    fn transit() -> Vec<NetworkCidrs> {
        vec![NetworkCidrs {
            name: "transit".into(),
            cidrs: vec!["10.250.0.0/16".into()],
        }]
    }

    /// Where a live instance bound the ref's listener, inside the edge's
    /// own networks: an address on another network is never dialed.
    #[test]
    fn endpoints_are_bound_addresses_on_a_shared_network() {
        let (hub, reports) = (hub(), ReportStore::default());
        reports.record(
            &target(),
            InstanceId("a".into()),
            report(Some("10.250.0.7"), &["10.128.0.9", "10.250.0.7"]),
        );
        reports.record(
            &target(),
            InstanceId("b".into()),
            report(None, &["10.128.0.10", "10.250.0.8"]),
        );
        reports.record(
            &target(),
            InstanceId("c".into()),
            report(Some("10.128.0.11"), &["10.128.0.11"]),
        );
        let mut config = edge_config(80, false);
        let targets = resolve(&hub, &reports, &mut config, &transit());
        assert_eq!(targets, [target()].into());
        assert_eq!(
            config.endpoint_overrides[&("publish.p-publish.gateway".to_string(), 80)],
            [
                ("10.250.0.7".to_string(), 8000),
                ("10.250.0.8".to_string(), 8000)
            ]
        );
    }

    /// No live instance on the network, or no listener on the port: no
    /// override, so the share is rejected. The target is still named, so
    /// its first report reconciles the edge; an invalid ref's too.
    #[test]
    fn no_endpoint_leaves_no_override_but_names_the_target() {
        let (hub, reports) = (hub(), ReportStore::default());
        for (case, mut config, networks) in [
            ("no report", edge_config(80, false), transit()),
            ("no listener", edge_config(443, false), transit()),
            ("off the network", edge_config(80, false), vec![]),
            ("invalid", edge_config(80, true), transit()),
        ] {
            if case == "off the network" {
                reports.record(
                    &target(),
                    InstanceId("a".into()),
                    report(None, &["10.250.0.7"]),
                );
            }
            let targets = resolve(&hub, &reports, &mut config, &networks);
            assert_eq!(targets, [target()].into(), "{case}");
            assert!(config.endpoint_overrides.is_empty(), "{case}");
        }
        let mut none: PortailConfig = Default::default();
        assert!(resolve(&hub, &reports, &mut none, &transit()).is_empty());
    }
}
