//! Local address discovery and Gateway `spec.addresses` resolution.
//!
//! Home of everything that answers "which IPs can this pod actually bind":
//! interface enumeration via `getifaddrs()`, the `portail.epheo.eu/Network`
//! address type (NetworkAttachmentDefinition subnet → local IP), and the
//! bind-address filter that separates pod-local IPs from LB VIPs.

use kube::api::Api;
use kube::Client;
use kube::ResourceExt;
use std::collections::{HashMap, HashSet};

use gateway_api::gateways::Gateway;

use crate::logging::{info, warn};

/// Custom address type for binding a Gateway to a named network interface.
/// Used in `spec.addresses[].type` to auto-discover UDN/Multus IPs.
pub(crate) const NETWORK_ADDRESS_TYPE: &str = "portail.epheo.eu/Network";

/// Addresses available to this gateway instance for binding / reporting.
/// `status.addresses` (LB VIP / cloud-assigned IPs) is owned by the operator,
/// so portail only needs locally-bindable IPs.
#[derive(Debug)]
pub(crate) struct UsableAddresses {
    /// IPs from local network interfaces (includes secondary/UDN interfaces).
    pub interface_ips: Vec<String>,
    /// IPs resolved from `portail.epheo.eu/Network` address type (network-name → IP).
    pub network_ips: HashMap<String, String>,
}

impl UsableAddresses {
    /// All known addresses as a set (for membership checks).
    pub fn all(&self) -> HashSet<&str> {
        self.interface_ips
            .iter()
            .chain(self.network_ips.values())
            .map(|s| s.as_str())
            .collect()
    }

    /// Best locally-bindable address — first interface IP, else `0.0.0.0`.
    pub fn preferred_ip(&self) -> &str {
        self.interface_ips
            .first()
            .map(|s| s.as_str())
            .unwrap_or("0.0.0.0")
    }

    pub fn is_empty(&self) -> bool {
        self.interface_ips.is_empty() && self.network_ips.is_empty()
    }
}

/// Resolve a network name to this pod's local IP on that network.
///
/// Reads the NetworkAttachmentDefinition to get the subnet CIDR, then matches
/// it against local interface IPs discovered via `getifaddrs()`.
async fn resolve_network_to_local_ip(
    client: &Client,
    network_name: &str,
    namespace: &str,
    local_ips: &[String],
) -> Option<String> {
    // Read the NAD in the Gateway's namespace
    let nad_api: Api<kube::api::DynamicObject> = Api::namespaced_with(
        client.clone(),
        namespace,
        &kube::discovery::ApiResource {
            group: "k8s.cni.cncf.io".into(),
            version: "v1".into(),
            api_version: "k8s.cni.cncf.io/v1".into(),
            kind: "NetworkAttachmentDefinition".into(),
            plural: "network-attachment-definitions".into(),
        },
    );
    let nad = match nad_api.get(network_name).await {
        Ok(n) => n,
        Err(e) => {
            warn!("Failed to read NetworkAttachmentDefinition {namespace}/{network_name}: {e}");
            return None;
        }
    };

    // Extract subnet from spec.config JSON
    let config_str = nad.data["spec"]["config"].as_str()?;
    let config: serde_json::Value = serde_json::from_str(config_str).ok()?;
    let subnets_str = config["subnets"].as_str()?;

    // Parse CIDR and match against local IPs
    let (net_addr, prefix_len) = parse_cidr(subnets_str)?;
    for ip_str in local_ips {
        if let Ok(ip) = ip_str.parse::<std::net::Ipv4Addr>() {
            if ip_in_subnet(ip, net_addr, prefix_len) {
                return Some(ip_str.clone());
            }
        }
    }
    warn!(
        "No local interface IP matches subnet {subnets_str} for network {namespace}/{network_name}"
    );
    None
}

fn parse_cidr(cidr: &str) -> Option<(std::net::Ipv4Addr, u32)> {
    let (addr_str, len_str) = cidr.split_once('/')?;
    let addr: std::net::Ipv4Addr = addr_str.parse().ok()?;
    let len: u32 = len_str.parse().ok()?;
    Some((addr, len))
}

fn ip_in_subnet(ip: std::net::Ipv4Addr, network: std::net::Ipv4Addr, prefix_len: u32) -> bool {
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

/// Enumerate IPv4/IPv6 addresses from all local network interfaces.
/// Discovers routable IPs for bind-address filtering and status reporting.
fn discover_local_interface_ips() -> Vec<String> {
    let mut ips = Vec::new();
    unsafe {
        let mut ifaddrs: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifaddrs) != 0 {
            return ips;
        }
        let mut ifa = ifaddrs;
        while !ifa.is_null() {
            let addr = (*ifa).ifa_addr;
            if !addr.is_null() {
                let family = (*addr).sa_family as i32;
                if family == libc::AF_INET {
                    let sa = addr as *const libc::sockaddr_in;
                    let ip = std::net::Ipv4Addr::from(u32::from_be((*sa).sin_addr.s_addr));
                    if !ip.is_loopback() {
                        ips.push(ip.to_string());
                    }
                } else if family == libc::AF_INET6 {
                    let sa = addr as *const libc::sockaddr_in6;
                    let ip = std::net::Ipv6Addr::from((*sa).sin6_addr.s6_addr);
                    let is_link_local = (ip.segments()[0] & 0xffc0) == 0xfe80;
                    if !ip.is_loopback() && !is_link_local {
                        ips.push(ip.to_string());
                    }
                }
            }
            ifa = (*ifa).ifa_next;
        }
        libc::freeifaddrs(ifaddrs);
    }
    ips
}

/// Enumerate IPs available on this pod's local interfaces (pod IP, host IPs
/// in hostNetwork mode, secondary network IPs from Multus, etc.). The operator
/// owns Gateway `status.addresses`, so portail only needs locally-bindable IPs.
pub(crate) fn discover_usable_addresses() -> UsableAddresses {
    UsableAddresses {
        interface_ips: discover_local_interface_ips(),
        network_ips: HashMap::new(),
    }
}

/// Resolve `portail.epheo.eu/Network` entries in `spec.addresses` to local IPs.
/// Reads each named NetworkAttachmentDefinition for its subnet and matches it
/// against the addresses we already discovered via `getifaddrs()`.
pub(crate) async fn resolve_network_addresses(
    client: &Client,
    gateway: &Gateway,
    gw_ns: &str,
    usable: &mut UsableAddresses,
) {
    let Some(spec_addrs) = &gateway.spec.addresses else {
        return;
    };
    let gw_name = gateway.name_any();
    for addr in spec_addrs {
        if addr.r#type.as_deref() != Some(NETWORK_ADDRESS_TYPE) {
            continue;
        }
        let network_name = addr.value.as_deref().unwrap_or("");
        match resolve_network_to_local_ip(client, network_name, gw_ns, &usable.interface_ips).await
        {
            Some(ip) => {
                info!("Gateway {gw_ns}/{gw_name}: resolved network '{network_name}' to {ip}");
                usable.network_ips.insert(network_name.to_string(), ip);
            }
            None => warn!(
                "Gateway {gw_ns}/{gw_name}: could not resolve network '{network_name}' to a local IP"
            ),
        }
    }
}

/// Pick which spec.addresses are bind-targets on this pod. LB VIPs are external
/// routing identities handled by kube-proxy/MetalLB and bind to 0.0.0.0; only
/// pod-local interface IPs and resolved Network-type IPs get bound specifically.
pub(crate) fn compute_bind_addresses(
    addresses: &[String],
    usable: &UsableAddresses,
) -> Vec<String> {
    let network_ip_set: HashSet<&str> = usable.network_ips.values().map(|s| s.as_str()).collect();
    addresses
        .iter()
        .filter(|addr| {
            usable.interface_ips.iter().any(|ip| ip == *addr)
                || network_ip_set.contains(addr.as_str())
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let ip: std::net::Ipv4Addr = "10.128.2.7".parse().unwrap();
        let net: std::net::Ipv4Addr = "10.128.0.0".parse().unwrap();
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
    fn compute_bind_addresses_keeps_local_ips_drops_lb_vips() {
        let usable = UsableAddresses {
            interface_ips: vec!["10.244.1.5".into()],
            network_ips: HashMap::from([("udn-a".into(), "192.168.10.3".into())]),
        };
        let spec_addrs = vec![
            "10.244.1.5".to_string(),   // pod IP — bindable
            "192.168.10.3".to_string(), // resolved Network IP — bindable
            "203.0.113.10".to_string(), // LB VIP — external, not bindable here
        ];
        assert_eq!(
            compute_bind_addresses(&spec_addrs, &usable),
            vec!["10.244.1.5".to_string(), "192.168.10.3".to_string()]
        );
        assert!(compute_bind_addresses(&[], &usable).is_empty());
    }
}
