//! Which addresses a Service backend may resolve to when the controller
//! sent the configuration. Whoever can write a Service's Endpoints picks
//! the addresses a headless name answers with, and the data plane dials
//! them from its own pod.

use std::net::IpAddr;

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
}

impl DialRules {
    /// Why `ip` is not dialed for a Service backend, if it is not. `direct`:
    /// a headless or ExternalName answer, or an override, whose addresses
    /// whoever writes the Service picks. Never a ClusterIP: cluster DNS
    /// answers its VIP, which is none of the pod's own and which OVN cannot
    /// route onto a secondary network.
    pub fn refuses(&self, ip: IpAddr, direct: bool) -> Option<&'static str> {
        let ip = ip.to_canonical();
        if direct && dialed_by_the_pod_itself(ip) {
            Some("the Gateway pod's own: loopback, link-local, unspecified, broadcast or multicast")
        } else if self.refused.iter().any(|c| c.contains(ip)) {
            Some("inside a network the Gateway attaches")
        } else if direct && self.unresolved {
            Some("dialed directly while a network the Gateway attaches has no CIDRs read")
        } else {
            None
        }
    }
}

/// 127.0.0.1 reaches the data plane's admin port, 169.254.169.254 a cloud
/// metadata endpoint: whatever the networks, never a backend.
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
            unresolved: false,
        }
    }

    #[test]
    fn the_pods_own_addresses_are_refused_when_dialed_directly() {
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
        ] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(DialRules::default().refuses(ip, true).is_some(), "{ip}");
            assert!(transit().refuses(ip, true).is_some(), "{ip}");
        }
        // A name cluster DNS answers with a VIP, as a ClusterIP's, is not one
        // whoever writes the Service picks.
        assert_eq!(
            DialRules::default().refuses("127.0.0.1".parse().unwrap(), false),
            None
        );
    }

    #[test]
    fn an_attached_network_is_refused_mapped_or_not() {
        let rules = transit();
        for ip in ["10.250.3.4", "::ffff:10.250.3.4"] {
            assert!(rules.refuses(ip.parse().unwrap(), true).is_some(), "{ip}");
            assert!(rules.refuses(ip.parse().unwrap(), false).is_some(), "{ip}");
        }
        assert_eq!(rules.refuses("10.251.0.1".parse().unwrap(), true), None);
        assert_eq!(rules.refuses("172.30.0.10".parse().unwrap(), false), None);
    }

    #[test]
    fn an_unresolved_network_refuses_direct_addresses_only() {
        let rules = DialRules {
            refused: vec![],
            unresolved: true,
        };
        let ip: IpAddr = "10.128.2.7".parse().unwrap();
        assert!(rules.refuses(ip, true).is_some());
        assert_eq!(rules.refuses(ip, false), None);
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
