//! A Managed Gateway's Service ports and the ports its data plane binds
//! behind them.

use std::collections::{BTreeMap, BTreeSet};

use gateway_api::gateways::GatewayListeners;
use k8s_openapi::api::core::v1::Service;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use crate::sync::Transport;

/// The highest privileged port: a non-root container without
/// NET_BIND_SERVICE cannot bind it.
pub(crate) const PRIVILEGED_PORT_MAX: u16 = 1023;

/// Where the targets of privileged published ports start (operator parity:
/// resources.go `targetPortPoolBase`).
const TARGET_PORT_POOL_BASE: u16 = 8000;

/// One Service port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DerivedPort {
    /// `<proto>-<port>`, e.g. `tcp-80`.
    pub name: String,
    pub port: u16,
    pub transport: Transport,
}

/// One port per published (port, transport) of `listeners`, in listener
/// order. Every protocol but UDP is TCP on the wire.
///
/// The operator deduplicated by port alone, so a UDP listener sharing a
/// TCP listener's port got no Service port.
pub(crate) fn derive_ports(listeners: &[GatewayListeners]) -> Vec<DerivedPort> {
    let mut ports: Vec<DerivedPort> = Vec::new();
    for l in listeners {
        let Ok(port) = u16::try_from(l.port) else {
            continue;
        };
        let transport = listener_transport(&l.protocol);
        if ports
            .iter()
            .any(|p| p.port == port && p.transport == transport)
        {
            continue;
        }
        ports.push(DerivedPort {
            name: format!("{}-{port}", protocol(transport).to_ascii_lowercase()),
            port,
            transport,
        });
    }
    ports
}

/// The transport of a Gateway API listener protocol.
pub(crate) fn listener_transport(protocol: &str) -> Transport {
    if protocol == "UDP" {
        Transport::Udp
    } else {
        Transport::Tcp
    }
}

/// The core/v1 Protocol of `transport`.
pub(crate) fn protocol(transport: Transport) -> &'static str {
    match transport {
        Transport::Tcp => "TCP",
        Transport::Udp => "UDP",
    }
}

/// The operator's `allocateTargetPorts` over one transport's published
/// ports: a port above the privileged range binds itself; a privileged one
/// keeps its `existing` target while that is unprivileged and free, else
/// draws the lowest free port from 8000 up. Deterministic whatever the
/// listener order, and collision-free.
pub(crate) fn allocate_target_ports(
    published: &[u16],
    existing: &BTreeMap<u16, u16>,
) -> BTreeMap<u16, u16> {
    let published: BTreeSet<u16> = published.iter().copied().collect();
    let mut target = BTreeMap::new();
    let mut used = BTreeSet::new();
    for &p in published.iter().filter(|&&p| p > PRIVILEGED_PORT_MAX) {
        target.insert(p, p);
        used.insert(p);
    }
    for &p in published.iter().filter(|&&p| p <= PRIVILEGED_PORT_MAX) {
        if let Some(&t) = existing.get(&p) {
            if t > PRIVILEGED_PORT_MAX && used.insert(t) {
                target.insert(p, t);
            }
        }
    }
    // A Gateway has at most 64 listeners (Gateway API maxItems), so the
    // pool never runs past u16.
    let mut next = TARGET_PORT_POOL_BASE;
    for &p in published.iter().filter(|&&p| p <= PRIVILEGED_PORT_MAX) {
        if target.contains_key(&p) {
            continue;
        }
        while used.contains(&next) {
            next += 1;
        }
        target.insert(p, next);
        used.insert(next);
        next += 1;
    }
    target
}

/// The port bound behind each of `ports` when a Service fronts them,
/// allocated per transport: UDP 8000 and TCP 8000 are different sockets.
pub(crate) fn bound_ports(
    ports: &[DerivedPort],
    existing: &BTreeMap<(Transport, u16), u16>,
) -> BTreeMap<(Transport, u16), u16> {
    let mut bound = BTreeMap::new();
    for transport in [Transport::Tcp, Transport::Udp] {
        let published: Vec<u16> = ports
            .iter()
            .filter(|p| p.transport == transport)
            .map(|p| p.port)
            .collect();
        let existing: BTreeMap<u16, u16> = existing
            .iter()
            .filter(|((t, _), _)| *t == transport)
            .map(|((_, port), target)| (*port, *target))
            .collect();
        for (port, target) in allocate_target_ports(&published, &existing) {
            bound.insert((transport, port), target);
        }
    }
    bound
}

/// A live Service's published to target assignments. A named or zero
/// targetPort assigns nothing.
pub(crate) fn existing_targets(service: &Service) -> BTreeMap<(Transport, u16), u16> {
    service
        .spec
        .iter()
        .flat_map(|spec| spec.ports.iter().flatten())
        .filter_map(|p| {
            let transport = match p.protocol.as_deref() {
                Some("UDP") => Transport::Udp,
                Some("TCP") | None => Transport::Tcp,
                Some(_) => return None,
            };
            let port = u16::try_from(p.port).ok()?;
            let target = match &p.target_port {
                Some(IntOrString::Int(t)) => u16::try_from(*t).ok().filter(|t| *t != 0)?,
                _ => return None,
            };
            Some(((transport, port), target))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listener(name: &str, port: i32, protocol: &str) -> GatewayListeners {
        GatewayListeners {
            name: name.into(),
            port,
            protocol: protocol.into(),
            ..Default::default()
        }
    }

    fn allocate(published: &[u16], existing: &[(u16, u16)]) -> BTreeMap<u16, u16> {
        allocate_target_ports(published, &existing.iter().copied().collect())
    }

    /// resources_test.go TestDerivePorts.
    #[test]
    fn derive_ports_one_per_listener_port() {
        let ports = derive_ports(&[
            listener("http", 80, "HTTP"),
            listener("https", 443, "HTTPS"),
        ]);
        assert_eq!(
            ports,
            [
                DerivedPort {
                    name: "tcp-80".into(),
                    port: 80,
                    transport: Transport::Tcp
                },
                DerivedPort {
                    name: "tcp-443".into(),
                    port: 443,
                    transport: Transport::Tcp
                },
            ]
        );
    }

    /// resources_test.go TestDerivePortsDedup.
    #[test]
    fn derive_ports_dedups_a_shared_port() {
        let ports = derive_ports(&[
            listener("http", 80, "HTTP"),
            listener("http-alt", 80, "HTTP"),
            listener("https", 443, "HTTPS"),
        ]);
        assert_eq!(ports.len(), 2);
    }

    /// resources_test.go TestDerivePortsUDP: listener order is kept.
    #[test]
    fn derive_ports_udp() {
        let ports = derive_ports(&[listener("dns", 5300, "UDP"), listener("http", 80, "HTTP")]);
        assert_eq!(ports[0].transport, Transport::Udp);
        assert_eq!(ports[0].name, "udp-5300");
        assert_eq!(ports[1].transport, Transport::Tcp);
    }

    /// TCP and UDP on one port are two Service ports; TLS and TCP on one
    /// port are one.
    #[test]
    fn derive_ports_tcp_and_udp_share_a_port() {
        let ports = derive_ports(&[
            listener("dns-tcp", 53, "TCP"),
            listener("dns-udp", 53, "UDP"),
            listener("tls", 53, "TLS"),
        ]);
        let names: Vec<&str> = ports.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["tcp-53", "udp-53"]);
    }

    /// resources_test.go TestAllocateTargetPorts, "privileged draw from
    /// pool, unprivileged bind themselves".
    #[test]
    fn privileged_ports_draw_from_the_pool() {
        let got = allocate(&[80, 443, 8080], &[]);
        assert_eq!(got[&8080], 8080);
        assert!(got[&80] > PRIVILEGED_PORT_MAX && got[&443] > PRIVILEGED_PORT_MAX);
        assert_ne!(got[&80], got[&443]);
        assert_eq!((got[&80], got[&443]), (8000, 8001));
    }

    /// "pool avoids an unprivileged listener's port".
    #[test]
    fn the_pool_avoids_a_listener_port() {
        let got = allocate(&[80, 8000], &[]);
        assert_eq!(got[&8000], 8000);
        assert_eq!(got[&80], 8001);
        let got = allocate(&[80, 8080], &[]);
        assert_ne!(got[&80], 8080);
        assert_eq!(got[&8080], 8080);
    }

    /// "existing assignments are preserved": adding a lower port does not
    /// move the targets of the others.
    #[test]
    fn existing_assignments_are_preserved() {
        let got = allocate(&[70, 80, 90], &[(80, 8000), (90, 8001)]);
        assert_eq!((got[&80], got[&90]), (8000, 8001));
        assert_eq!(got[&70], 8002);
    }

    /// A privileged or taken existing target is not kept.
    #[test]
    fn an_unusable_existing_target_is_replaced() {
        let got = allocate(&[80, 443, 8000], &[(80, 80), (443, 8000)]);
        assert_eq!(got[&8000], 8000);
        assert_eq!((got[&80], got[&443]), (8001, 8002));
    }

    /// "deterministic regardless of listener order".
    #[test]
    fn allocation_ignores_listener_order() {
        assert_eq!(allocate(&[80, 443, 22], &[]), allocate(&[22, 443, 80], &[]));
    }

    /// Each transport allocates on its own.
    #[test]
    fn bound_ports_per_transport() {
        let ports = derive_ports(&[
            listener("dns-tcp", 53, "TCP"),
            listener("dns-udp", 53, "UDP"),
            listener("syslog", 8000, "UDP"),
        ]);
        let existing = [((Transport::Udp, 53), 8100)].into_iter().collect();
        let bound = bound_ports(&ports, &existing);
        assert_eq!(bound[&(Transport::Tcp, 53)], 8000);
        assert_eq!(bound[&(Transport::Udp, 53)], 8100);
        assert_eq!(bound[&(Transport::Udp, 8000)], 8000);
    }

    #[test]
    fn existing_targets_read_numbers_only() {
        let svc: Service = serde_json::from_value(serde_json::json!({
            "spec": {"ports": [
                {"name": "tcp-80", "port": 80, "targetPort": 8000},
                {"name": "udp-53", "port": 53, "protocol": "UDP", "targetPort": 8001},
                {"name": "named", "port": 443, "targetPort": "https"},
                {"name": "sctp", "port": 9, "protocol": "SCTP", "targetPort": 9},
            ]},
        }))
        .unwrap();
        let got = existing_targets(&svc);
        assert_eq!(
            got,
            [((Transport::Tcp, 80), 8000), ((Transport::Udp, 53), 8001)]
                .into_iter()
                .collect()
        );
    }
}
