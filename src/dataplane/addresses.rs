//! Which IPs this pod can bind: interface enumeration via `getifaddrs()`,
//! Network addresses matched against the CIDRs the controller read from
//! their NetworkAttachmentDefinitions, and the filter that separates
//! pod-local IPs from LoadBalancer VIPs.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use crate::sync::NetworkCidrs;

/// IPv4/IPv6 addresses of every local interface, loopback and IPv6
/// link-local aside: what a Gateway address may bind, and what status
/// reports for a Gateway without one.
pub fn local_interface_ips() -> Vec<String> {
    local_interfaces().into_iter().map(|(_, ip)| ip).collect()
}

/// [`local_interface_ips`], each with the interface that holds it.
pub fn local_interfaces() -> Vec<(String, String)> {
    let mut found = Vec::new();
    unsafe {
        let mut ifaddrs: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifaddrs) != 0 {
            return found;
        }
        let mut ifa = ifaddrs;
        while !ifa.is_null() {
            let addr = (*ifa).ifa_addr;
            let name = || {
                std::ffi::CStr::from_ptr((*ifa).ifa_name)
                    .to_string_lossy()
                    .into_owned()
            };
            if !addr.is_null() {
                let family = (*addr).sa_family as i32;
                if family == libc::AF_INET {
                    let sa = addr as *const libc::sockaddr_in;
                    let ip = std::net::Ipv4Addr::from(u32::from_be((*sa).sin_addr.s_addr));
                    if !ip.is_loopback() {
                        found.push((name(), ip.to_string()));
                    }
                } else if family == libc::AF_INET6 {
                    let sa = addr as *const libc::sockaddr_in6;
                    let ip = std::net::Ipv6Addr::from((*sa).sin6_addr.s6_addr);
                    let is_link_local = (ip.segments()[0] & 0xffc0) == 0xfe80;
                    if !ip.is_loopback() && !is_link_local {
                        found.push((name(), ip.to_string()));
                    }
                }
            }
            ifa = (*ifa).ifa_next;
        }
        libc::freeifaddrs(ifaddrs);
    }
    found
}

/// What the networks a Gateway only dials out on take off its binds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DialOut {
    /// The Gateway has at least one.
    pub any: bool,
    /// One has no CIDRs read: its interface cannot be told apart, so
    /// nothing binds where it might be.
    pub unresolved: bool,
    /// Every address, any family, on an interface holding an IP inside one
    /// of their CIDRs: that interface is theirs, whatever else it carries.
    pub ips: Vec<String>,
}

impl DialOut {
    /// From this pod's `interfaces`, (name, IP). `known` are IPs whose
    /// interface listens: a listening Network's, the pod's own. While a
    /// dial-out network has no CIDRs read, any other interface may be its,
    /// so every address on it counts as dial-out: a named address there
    /// would otherwise bind on a network the Gateway only dials out on.
    pub fn of(
        interfaces: &[(String, String)],
        networks: &[NetworkCidrs],
        known: &[String],
    ) -> Self {
        let mut out = Self::declared(networks);
        let holds = |name: &str, test: &dyn Fn(&str) -> bool| {
            interfaces.iter().any(|(n, ip)| n == name && test(ip))
        };
        let dialed = |ip: &str| dialed_cidrs(networks).any(|c| in_cidr(ip, c));
        let listens = |ip: &str| known.iter().any(|k| same_ip(k, ip));
        let theirs =
            |name: &str| holds(name, &dialed) || (out.unresolved && !holds(name, &listens));
        out.ips = interfaces
            .iter()
            .filter(|(name, _)| theirs(name))
            .map(|(_, ip)| ip.clone())
            .collect();
        out
    }

    /// As an instance reported it, `ips` being its own count.
    pub fn reported(networks: &[NetworkCidrs], ips: &[String]) -> Self {
        Self {
            ips: ips.to_vec(),
            ..Self::declared(networks)
        }
    }

    fn declared(networks: &[NetworkCidrs]) -> Self {
        let dialed = || networks.iter().filter(|n| !n.listeners);
        Self {
            any: dialed().next().is_some(),
            unresolved: dialed().any(|n| n.cidrs.is_empty()),
            ips: Vec::new(),
        }
    }

    /// The dial-out networks whose CIDRs were never read.
    pub fn unresolved_names(networks: &[NetworkCidrs]) -> impl Iterator<Item = &str> {
        networks
            .iter()
            .filter(|n| !n.listeners && n.cidrs.is_empty())
            .map(|n| n.name.as_str())
    }
}

/// Equal as addresses, whatever their spelling.
fn same_ip(a: &str, b: &str) -> bool {
    match (a.parse::<std::net::IpAddr>(), b.parse::<std::net::IpAddr>()) {
        (Ok(a), Ok(b)) => a.to_canonical() == b.to_canonical(),
        _ => false,
    }
}

fn dialed_cidrs(networks: &[NetworkCidrs]) -> impl Iterator<Item = &str> + '_ {
    networks
        .iter()
        .filter(|n| !n.listeners)
        .flat_map(|n| n.cidrs.iter().map(String::as_str))
}

/// An IPv4 CIDR as (network address, prefix length).
pub fn parse_cidr(cidr: &str) -> Option<(Ipv4Addr, u32)> {
    let (addr_str, len_str) = cidr.split_once('/')?;
    let addr: Ipv4Addr = addr_str.parse().ok()?;
    let len: u32 = len_str.parse().ok()?;
    Some((addr, len))
}

fn ip_in_subnet(ip: Ipv4Addr, network: Ipv4Addr, prefix_len: u32) -> bool {
    if prefix_len > 32 {
        return false;
    }
    let mask = if prefix_len == 0 {
        0u32
    } else {
        !0u32 << (32 - prefix_len)
    };
    u32::from(ip) & mask == u32::from(network) & mask
}

/// Network address values among `addresses`: IPAddress values are IP
/// literals (CRD-validated), so anything else names a network.
fn network_names(addresses: &[String]) -> impl Iterator<Item = &str> {
    addresses
        .iter()
        .map(String::as_str)
        .filter(|a| a.parse::<std::net::IpAddr>().is_err())
}

/// Network name to this pod's IP on that network, for each Network
/// address, and each network the Gateway only dials out on, whose CIDRs
/// hold one of `local`'s IPv4 addresses.
pub fn resolve_networks(
    addresses: &[String],
    networks: &[NetworkCidrs],
    local: &[String],
) -> BTreeMap<String, String> {
    let dialed = networks
        .iter()
        .filter(|n| !n.listeners)
        .map(|n| n.name.as_str());
    network_names(addresses)
        .chain(dialed)
        .filter_map(|name| {
            let cidrs = &networks.iter().find(|n| n.name == name)?.cidrs;
            let ip = local.iter().find(|ip| {
                ip.parse::<Ipv4Addr>().is_ok_and(|ip| {
                    cidrs
                        .iter()
                        .filter_map(|c| parse_cidr(c))
                        .any(|(net, len)| ip_in_subnet(ip, net, len))
                })
            })?;
            Some((name.to_string(), ip.clone()))
        })
        .collect()
}

/// Which of a Gateway's `addresses` its listeners bind on this pod, the
/// Network ones matched against `networks`. LoadBalancer VIPs are external
/// routing identities kube-proxy or MetalLB deliver, so a Gateway with only
/// those binds every interface. `Some(empty)` binds every interface; `None`
/// binds nothing.
#[cfg(test)]
pub fn bind_addresses(
    addresses: &[String],
    networks: &[NetworkCidrs],
    interfaces: &[(String, String)],
    pod_ip: Option<&str>,
) -> Option<Vec<String>> {
    let local: Vec<String> = interfaces.iter().map(|(_, ip)| ip.clone()).collect();
    let resolved = resolve_networks(addresses, networks, &local);
    let known = listening_ips(&resolved, networks, pod_ip);
    bind_addresses_resolved(
        addresses,
        &resolved,
        &local,
        &DialOut::of(interfaces, networks, &known),
    )
}

/// The IPs whose interfaces listen: each resolved listening Network's,
/// and the pod's own.
pub fn listening_ips(
    resolved: &BTreeMap<String, String>,
    networks: &[NetworkCidrs],
    pod_ip: Option<&str>,
) -> Vec<String> {
    let listening = |name: &str| networks.iter().any(|n| n.name == name && n.listeners);
    resolved
        .iter()
        .filter(|(name, _)| listening(name))
        .map(|(_, ip)| ip.clone())
        .chain(pod_ip.map(str::to_string))
        .collect()
}

/// [`bind_addresses`] once the networks are resolved: what a report's
/// `networks`, `interfaceIps` and `dialOutIps` say the instance bound.
///
/// Nothing binds on an interface of a network the Gateway only dials out
/// on. Where such a Gateway would bind every interface, it binds each
/// local IP off them instead, as read at this apply: an address the pod
/// gains later binds at the next one. While one of those networks has no
/// CIDRs read, it binds only the addresses it names, as for a Network
/// address not resolved. Every other Gateway keeps every interface, which
/// follows addresses as they come.
pub fn bind_addresses_resolved(
    addresses: &[String],
    resolved: &BTreeMap<String, String>,
    local: &[String],
    dial_out: &DialOut,
) -> Option<Vec<String>> {
    let dials_from = |ip: &str| dial_out.ips.iter().any(|d| d == ip);
    let bindable: Vec<String> = addresses
        .iter()
        .map(|a| resolved.get(a).unwrap_or(a))
        .filter(|a| local.contains(a) || resolved.values().any(|ip| ip == *a))
        .filter(|a| !dials_from(a))
        .cloned()
        .collect();
    // A Network name that did not resolve: falling back to every interface
    // would expose the listeners on networks the Gateway never named.
    if bindable.is_empty() && !unresolved_networks(addresses, resolved).is_empty() {
        return None;
    }
    if !bindable.is_empty() || !dial_out.any {
        return Some(bindable);
    }
    if dial_out.unresolved {
        return None;
    }
    let outside: Vec<String> = local.iter().filter(|ip| !dials_from(ip)).cloned().collect();
    (!outside.is_empty()).then_some(outside)
}

/// Whether `ip` is inside `cidr`, IPv4 or IPv6, host bits ignored.
pub(crate) fn in_cidr(ip: &str, cidr: &str) -> bool {
    let (Ok(ip), Some((net, len))) = (
        ip.parse::<std::net::IpAddr>(),
        cidr.split_once('/')
            .and_then(|(a, l)| Some((a.parse::<std::net::IpAddr>().ok()?, l.parse::<u32>().ok()?))),
    ) else {
        return false;
    };
    let bits = |ip: std::net::IpAddr| match ip.to_canonical() {
        std::net::IpAddr::V4(v4) => (u128::from(u32::from(v4)), 32),
        std::net::IpAddr::V6(v6) => (u128::from(v6), 128),
    };
    let ((ip, width), (net, net_width)) = (bits(ip), bits(net));
    if width != net_width || len > width {
        return false;
    }
    let mask = if len == 0 {
        0
    } else {
        (u128::MAX >> (128 - width)) & (u128::MAX << (width - len))
    };
    ip & mask == net & mask
}

/// The Network names among `addresses` that `resolved` has no IP for.
pub fn unresolved_networks<'a>(
    addresses: &'a [String],
    resolved: &BTreeMap<String, String>,
) -> Vec<&'a str> {
    network_names(addresses)
        .filter(|name| !resolved.contains_key(*name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// Each IP on an interface of its own.
    fn own(ips: &[String]) -> Vec<(String, String)> {
        ips.iter()
            .enumerate()
            .map(|(i, ip)| (format!("eth{i}"), ip.clone()))
            .collect()
    }

    fn table(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|(name, ip)| (name.to_string(), ip.to_string()))
            .collect()
    }

    fn network(name: &str, cidr: &str) -> NetworkCidrs {
        NetworkCidrs {
            name: name.into(),
            cidrs: vec![cidr.into()],
            listeners: true,
        }
    }

    #[test]
    fn parse_cidr_accepts_valid_and_rejects_garbage() {
        assert_eq!(
            parse_cidr("10.128.0.0/14"),
            Some(("10.128.0.0".parse().unwrap(), 14))
        );
        assert_eq!(parse_cidr("192.168.1.0/24").unwrap().1, 24);
        assert_eq!(parse_cidr("10.0.0.0"), None); // no prefix
        assert_eq!(parse_cidr("not-an-ip/24"), None);
        assert_eq!(parse_cidr("10.0.0.0/abc"), None);
        assert_eq!(parse_cidr(""), None);
    }

    #[test]
    fn ip_in_subnet_edge_prefixes() {
        let ip: Ipv4Addr = "10.128.2.7".parse().unwrap();
        let net: Ipv4Addr = "10.128.0.0".parse().unwrap();
        assert!(ip_in_subnet(ip, net, 14));
        assert!(!ip_in_subnet("10.132.0.1".parse().unwrap(), net, 14));
        // /0 matches everything
        assert!(ip_in_subnet("8.8.8.8".parse().unwrap(), net, 0));
        // /32 is an exact match only
        assert!(ip_in_subnet(net, net, 32));
        assert!(!ip_in_subnet(ip, net, 32));
        // out-of-range prefix is rejected, not wrapped
        assert!(!ip_in_subnet(ip, net, 33));
    }

    #[test]
    fn bind_addresses_keeps_local_ips_drops_lb_vips() {
        let local = strings(&["10.244.1.5", "192.168.10.3"]);
        let networks = [network("udn-a", "192.168.10.0/24")];
        let spec = strings(&[
            "10.244.1.5",   // pod IP: bindable
            "udn-a",        // Network: this pod's IP on it
            "203.0.113.10", // LB VIP: external, not bindable here
        ]);
        assert_eq!(
            bind_addresses(&spec, &networks, &own(&local), None),
            Some(strings(&["10.244.1.5", "192.168.10.3"]))
        );
        assert_eq!(
            bind_addresses(&[], &networks, &own(&local), None),
            Some(vec![])
        );
        // A VIP alone binds every interface; the Service carries it.
        assert_eq!(
            bind_addresses(&strings(&["203.0.113.10"]), &networks, &own(&local), None),
            Some(vec![])
        );
    }

    fn dialed(name: &str, cidr: &str) -> NetworkCidrs {
        NetworkCidrs {
            listeners: false,
            ..network(name, cidr)
        }
    }

    /// A Gateway without a network it only dials out on keeps every
    /// interface: an address it gains later binds with no apply.
    #[test]
    fn without_a_dialed_network_every_interface_binds() {
        let local = strings(&["10.244.1.5", "10.250.0.9"]);
        let networks = [network("udn-a", "192.168.10.0/24")];
        assert_eq!(
            bind_addresses(&[], &networks, &own(&local), None),
            Some(vec![])
        );
        let vip = strings(&["203.0.113.10"]);
        assert_eq!(
            bind_addresses(&vip, &networks, &own(&local), None),
            Some(vec![])
        );
    }

    /// The edge on transit, fronted by a LoadBalancer: its listeners bind
    /// every local IP off its transit interface, and nothing binds there.
    #[test]
    fn a_dialed_network_takes_no_listener() {
        let interfaces = table(&[
            ("eth0", "10.244.1.5"),
            ("eth0", "fd00:10:244::5"),
            ("net1", "10.250.0.9"),
        ]);
        let networks = [dialed("transit", "10.250.0.0/16")];
        let vip = strings(&["203.0.113.10"]);
        let want = Some(strings(&["10.244.1.5", "fd00:10:244::5"]));
        assert_eq!(bind_addresses(&vip, &networks, &interfaces, None), want);
        assert_eq!(bind_addresses(&[], &networks, &interfaces, None), want);
        // An IPAddress on transit binds nowhere, so the rest bind as above.
        let on_transit = strings(&["10.250.0.9"]);
        assert_eq!(
            bind_addresses(&on_transit, &networks, &interfaces, None),
            want
        );
        // Its IP there is still resolved, for reports and trust.
        let local = strings(&["10.244.1.5", "fd00:10:244::5", "10.250.0.9"]);
        assert_eq!(
            resolve_networks(&[], &networks, &local),
            BTreeMap::from([("transit".to_string(), "10.250.0.9".to_string())])
        );
    }

    /// The transit interface is the dial-out network's whole: an address on
    /// it outside the NAD's CIDRs, such as IPv6 on an IPv4-only NAD, never
    /// binds either.
    #[test]
    fn every_address_on_a_dialed_interface_is_off_limits() {
        let interfaces = table(&[
            ("eth0", "10.244.1.5"),
            ("net1", "10.250.0.9"),
            ("net1", "fd00:250::9"),
        ]);
        let networks = [dialed("transit", "10.250.0.0/16")];
        let dial_out = DialOut::of(&interfaces, &networks, &[]);
        assert_eq!(dial_out.ips, strings(&["10.250.0.9", "fd00:250::9"]));
        assert_eq!(
            bind_addresses(&[], &networks, &interfaces, None),
            Some(strings(&["10.244.1.5"]))
        );
        let named = strings(&["fd00:250::9"]);
        assert_eq!(
            bind_addresses(&named, &networks, &interfaces, None),
            Some(strings(&["10.244.1.5"])),
            "a named address on it binds nowhere"
        );
    }

    /// A dial-out network whose NAD is gone or names no subnet: its
    /// interface cannot be told apart, so only the addresses the Gateway
    /// names bind, as for a Network address not resolved.
    #[test]
    fn an_unresolved_dialed_network_binds_only_named_addresses() {
        let interfaces = table(&[("eth0", "10.244.1.5"), ("net1", "10.250.0.9")]);
        let networks = [NetworkCidrs {
            cidrs: vec![],
            ..dialed("transit", "10.250.0.0/16")
        }];
        assert_eq!(bind_addresses(&[], &networks, &interfaces, None), None);
        let vip = strings(&["203.0.113.10"]);
        assert_eq!(bind_addresses(&vip, &networks, &interfaces, None), None);
        let named = strings(&["10.244.1.5"]);
        assert_eq!(
            bind_addresses(&named, &networks, &interfaces, Some("10.244.1.5")),
            Some(named.clone())
        );
        assert_eq!(
            DialOut::unresolved_names(&networks).collect::<Vec<_>>(),
            ["transit"]
        );
    }

    /// While a dialed network is unresolved, its interface cannot be told
    /// apart: a named address binds only where the pod is known to listen,
    /// its own IP's interface or a listening Network's.
    #[test]
    fn an_unresolved_dialed_network_binds_named_addresses_only_where_known() {
        let interfaces = table(&[
            ("eth0", "10.244.1.5"),
            ("net1", "10.250.0.9"),
            ("net2", "192.168.10.3"),
        ]);
        let networks = [
            NetworkCidrs {
                cidrs: vec![],
                ..dialed("transit", "10.250.0.0/16")
            },
            network("udn-a", "192.168.10.0/24"),
        ];
        let pod = Some("10.244.1.5");
        let on_transit = strings(&["10.250.0.9"]);
        assert_eq!(
            bind_addresses(&on_transit, &networks, &interfaces, pod),
            None
        );
        let both = strings(&["10.250.0.9", "10.244.1.5"]);
        assert_eq!(
            bind_addresses(&both, &networks, &interfaces, pod),
            Some(strings(&["10.244.1.5"]))
        );
        let with_network = strings(&["udn-a", "10.250.0.9"]);
        assert_eq!(
            bind_addresses(&with_network, &networks, &interfaces, pod),
            Some(strings(&["192.168.10.3"]))
        );
        let known = listening_ips(
            &resolve_networks(&with_network, &networks, &strings(&["192.168.10.3"])),
            &networks,
            pod,
        );
        let dial_out = DialOut::of(&interfaces, &networks, &known);
        assert_eq!(
            dial_out.ips,
            strings(&["10.250.0.9"]),
            "reported as dial-out"
        );
        assert_eq!(
            bind_addresses(&strings(&["10.244.1.5"]), &networks, &interfaces, None),
            None,
            "without the pod's own IP nothing is known"
        );
    }

    /// A Network address not resolved still binds nothing on a Gateway that
    /// also dials out.
    #[test]
    fn an_unresolved_network_address_beats_enumeration() {
        let interfaces = table(&[("eth0", "10.244.1.5"), ("net1", "10.250.0.9")]);
        let networks = [dialed("transit", "10.250.0.0/16")];
        let spec = strings(&["udn-a"]);
        assert_eq!(bind_addresses(&spec, &networks, &interfaces, None), None);
    }

    /// Addresses it listens on bind as before; the dialed network adds none.
    #[test]
    fn a_dialed_network_leaves_listened_addresses_alone() {
        let interfaces = table(&[
            ("eth0", "10.244.1.5"),
            ("net1", "192.168.10.3"),
            ("net2", "10.250.0.9"),
        ]);
        let networks = [
            network("udn-a", "192.168.10.0/24"),
            dialed("transit", "10.250.0.0/16"),
        ];
        let spec = strings(&["udn-a"]);
        assert_eq!(
            bind_addresses(&spec, &networks, &interfaces, None),
            Some(strings(&["192.168.10.3"]))
        );
    }

    /// Only dialed networks on the pod: nothing binds, never every
    /// interface.
    #[test]
    fn a_pod_only_on_dialed_networks_binds_nothing() {
        let interfaces = table(&[("net1", "10.250.0.9"), ("net2", "fd00:250::9")]);
        let networks = [
            dialed("transit", "10.250.0.0/16"),
            dialed("transit6", "fd00:250::/64"),
        ];
        assert_eq!(bind_addresses(&[], &networks, &interfaces, None), None);
    }

    /// A Network address binds this pod's IP on that network, not every
    /// interface.
    #[test]
    fn a_network_binds_its_local_ip() {
        let local = strings(&["10.244.1.5", "192.168.10.3"]);
        let spec = strings(&["udn-a"]);
        let networks = [network("udn-a", "192.168.10.0/24")];
        assert_eq!(
            resolve_networks(&spec, &networks, &local),
            BTreeMap::from([("udn-a".to_string(), "192.168.10.3".to_string())])
        );
        assert_eq!(
            bind_addresses(&spec, &networks, &own(&local), None),
            Some(strings(&["192.168.10.3"]))
        );
    }

    /// An unresolved Network name never falls back to every interface.
    #[test]
    fn an_unresolved_network_binds_nothing() {
        let local = strings(&["10.244.1.5"]);
        // TEST-NET-1: no pod has an address there.
        let networks = [network("udn-a", "192.0.2.0/24")];
        let mut spec = strings(&["udn-a"]);
        assert_eq!(bind_addresses(&spec, &networks, &own(&local), None), None);
        assert_eq!(
            bind_addresses(&spec, &[], &own(&local), None),
            None,
            "no CIDRs read"
        );
        assert_eq!(unresolved_networks(&spec, &BTreeMap::new()), vec!["udn-a"]);

        // A local address still binds; the unresolved name adds nothing.
        spec.push("10.244.1.5".to_string());
        assert_eq!(
            bind_addresses(&spec, &networks, &own(&local), None),
            Some(strings(&["10.244.1.5"]))
        );
    }
}
