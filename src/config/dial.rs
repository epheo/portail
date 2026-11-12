//! Which addresses a Service backend may resolve to when the controller
//! sent the configuration. Whoever can write a Service's Endpoints picks
//! the addresses a headless name answers with, whoever writes an
//! ExternalName Service picks its target's, and the data plane dials them
//! from its own pod.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::proxy::proxy_protocol::Cidr;

/// Never set by a standalone file: its backends are the owner's to pick,
/// 127.0.0.1 included.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DialRules {
    /// The networks the Gateway attaches for the platform. Empty when its
    /// parameters allow Service backends there.
    pub refused: Vec<Cidr>,
    /// One of those networks has no CIDRs read: no address dialed directly
    /// can be told outside it.
    pub unresolved: bool,
    /// Backend names the controller knows cluster DNS answers with a VIP.
    /// Every other name, an unknown one included, is dialed directly.
    pub cluster_ip: BTreeSet<String>,
}

impl DialRules {
    /// Why `ip` is not dialed, if it is not. `direct`: an address whoever
    /// writes the Service picks (a headless or ExternalName answer, an
    /// override target), so anything but a name marked as a ClusterIP's.
    /// Every address meets the pod's-own and the attached-network checks,
    /// a marked VIP's too: a real one is never either, and a Service turned
    /// ExternalName since it was marked is caught.
    pub fn refuses(&self, ip: IpAddr, direct: bool) -> Option<&'static str> {
        let ip = canonical(ip);
        if dialed_by_the_pod_itself(ip) {
            Some("the Gateway pod's own: loopback, link-local, unspecified, broadcast or multicast")
        } else if self.refused.iter().any(|c| c.contains(ip)) {
            Some("inside a network the Gateway attaches")
        } else if direct && self.unresolved {
            Some("dialed directly while a network the Gateway attaches has no CIDRs read")
        } else {
            None
        }
    }

    /// Whether backend `name`'s own DNS answers are dialed as is.
    pub fn direct(&self, name: &str) -> bool {
        !self.cluster_ip.contains(name)
    }
}

/// The IPv4 address an IPv6 one stands for: IPv4-mapped, or under the NAT64
/// well-known prefix 64:ff9b::/96, which a NAT64 gateway turns into it.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) if v6.segments()[..6] == NAT64[..6] => {
            let [.., a, b, c, d] = v6.octets();
            IpAddr::V4(Ipv4Addr::new(a, b, c, d))
        }
        other => other,
    }
}

const NAT64: [u16; 8] = Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0).segments();

/// 127.0.0.1 reaches the data plane's admin port, 169.254.169.254 a cloud
/// metadata endpoint: never a backend.
fn dialed_by_the_pod_itself(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_unspecified()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
        }
        IpAddr::V6(v6) => {
            v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_unicast_link_local()
                || v6.is_multicast()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transit() -> DialRules {
        DialRules {
            refused: vec![Cidr::covering("10.250.0.0/16").unwrap()],
            ..DialRules::default()
        }
    }

    #[test]
    fn the_pods_own_addresses_are_refused_direct_or_not() {
        for ip in [
            "127.0.0.1",
            "127.1.2.3",
            "::1",
            "169.254.169.254",
            "fe80::1",
            "0.0.0.0",
            "::",
            "255.255.255.255",
            "224.0.0.1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b::7f00:1",
        ] {
            let ip: IpAddr = ip.parse().unwrap();
            for direct in [true, false] {
                assert!(DialRules::default().refuses(ip, direct).is_some(), "{ip}");
                assert!(transit().refuses(ip, direct).is_some(), "{ip}");
            }
        }
    }

    #[test]
    fn an_attached_network_is_refused_mapped_or_through_nat64() {
        let rules = transit();
        for ip in ["10.250.3.4", "::ffff:10.250.3.4", "64:ff9b::afa:304"] {
            assert!(rules.refuses(ip.parse().unwrap(), true).is_some(), "{ip}");
            assert!(rules.refuses(ip.parse().unwrap(), false).is_some(), "{ip}");
        }
        assert_eq!(rules.refuses("10.251.0.1".parse().unwrap(), true), None);
        assert_eq!(rules.refuses("172.30.0.10".parse().unwrap(), false), None);
        assert_eq!(
            rules.refuses("64:ff9c::afa:304".parse().unwrap(), true),
            None
        );
    }

    #[test]
    fn an_unresolved_network_refuses_direct_addresses_only() {
        let rules = DialRules {
            unresolved: true,
            cluster_ip: ["vip.p.svc".to_string()].into(),
            ..DialRules::default()
        };
        let ip: IpAddr = "10.128.2.7".parse().unwrap();
        assert!(rules.refuses(ip, rules.direct("head.p.svc")).is_some());
        assert!(rules.refuses(ip, rules.direct("unknown.p.svc")).is_some());
        assert_eq!(rules.refuses(ip, rules.direct("vip.p.svc")), None);
    }

    #[test]
    fn a_subnet_written_with_its_gateway_address_covers_its_network() {
        let cidr = Cidr::covering("10.250.0.1/16").unwrap();
        assert!(cidr.contains("10.250.9.9".parse().unwrap()));
        assert!(!cidr.contains("10.251.0.1".parse().unwrap()));
        assert_eq!(Cidr::covering("10.250.0.1"), None);
        assert_eq!(Cidr::covering("10.250.0.1/33"), None);
        assert!(Cidr::covering("fd00::5/64")
            .unwrap()
            .contains("fd00::9".parse().unwrap()));
    }
}
