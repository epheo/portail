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
use std::sync::Mutex;

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

/// What a NetworkAttachmentDefinition says about this pod on its network.
enum NetworkIp {
    Local(String),
    /// No IP of this pod is in the network, or the network is gone.
    Absent,
    /// The NAD could not be read: says nothing about the network.
    Unknown(kube::Error),
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
) -> NetworkIp {
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
        Err(kube::Error::Api(e)) if e.code == 404 => {
            warn!("NetworkAttachmentDefinition {namespace}/{network_name} not found");
            return NetworkIp::Absent;
        }
        Err(e) => return NetworkIp::Unknown(e),
    };

    let local = local_ip_in(&nad.data, local_ips);
    if local.is_none() {
        warn!("No local interface IP is in the subnet of network {namespace}/{network_name}");
    }
    local.map_or(NetworkIp::Absent, NetworkIp::Local)
}

/// The first of `local_ips` in the subnet a NAD's spec.config names.
fn local_ip_in(nad: &serde_json::Value, local_ips: &[String]) -> Option<String> {
    let config_str = nad["spec"]["config"].as_str()?;
    let config: serde_json::Value = serde_json::from_str(config_str).ok()?;
    let (net_addr, prefix_len) = parse_cidr(config["subnets"].as_str()?)?;
    local_ips
        .iter()
        .find(|ip| {
            ip.parse::<std::net::Ipv4Addr>()
                .is_ok_and(|ip| ip_in_subnet(ip, net_addr, prefix_len))
        })
        .cloned()
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

/// The last local IP each network resolved to, by (namespace, network).
#[derive(Default)]
pub(crate) struct NetworkIps(Mutex<HashMap<(String, String), String>>);

/// Resolve `portail.epheo.eu/Network` entries in `spec.addresses` to local IPs.
/// Reads each named NetworkAttachmentDefinition for its subnet and matches it
/// against the addresses we already discovered via `getifaddrs()`.
///
/// A NAD that cannot be read keeps the network's last IP, while the pod
/// still has it: unbinding on an apiserver blip was an outage. Only an
/// answer that the network has no IP here (or is gone) unbinds it.
pub(crate) async fn resolve_network_addresses(
    client: &Client,
    gateway: &Gateway,
    gw_ns: &str,
    usable: &mut UsableAddresses,
    last: &NetworkIps,
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
        let key = (gw_ns.to_string(), network_name.to_string());
        let resolved =
            resolve_network_to_local_ip(client, network_name, gw_ns, &usable.interface_ips).await;
        let mut known = last.0.lock().unwrap_or_else(|p| p.into_inner());
        let ip = match resolved {
            NetworkIp::Local(ip) => {
                if known.get(&key) != Some(&ip) {
                    info!("Gateway {gw_ns}/{gw_name}: resolved network '{network_name}' to {ip}");
                    known.insert(key, ip.clone());
                }
                ip
            }
            NetworkIp::Absent => {
                known.remove(&key);
                continue;
            }
            NetworkIp::Unknown(e) => {
                match known
                    .get(&key)
                    .filter(|ip| usable.interface_ips.contains(ip))
                {
                    Some(ip) => {
                        warn!(
                            "Gateway {gw_ns}/{gw_name}: NetworkAttachmentDefinition {network_name} \
                             not read ({e}); keeping {ip}"
                        );
                        ip.clone()
                    }
                    None => {
                        warn!(
                            "Gateway {gw_ns}/{gw_name}: NetworkAttachmentDefinition {network_name} \
                             not read ({e}); network unresolved"
                        );
                        continue;
                    }
                }
            }
        };
        usable.network_ips.insert(network_name.to_string(), ip);
    }
}

/// Swap each resolved `portail.epheo.eu/Network` name in `addresses` for this
/// pod's IP on that network. An unresolved name stays as it is.
pub(crate) fn substitute_network_ips(addresses: &mut [String], usable: &UsableAddresses) {
    for addr in addresses {
        if let Some(ip) = usable.network_ips.get(addr.as_str()) {
            *addr = ip.clone();
        }
    }
}

/// Pick which spec.addresses are bind-targets on this pod. LB VIPs are external
/// routing identities handled by kube-proxy/MetalLB and bind to 0.0.0.0; only
/// pod-local interface IPs and resolved Network-type IPs get bound specifically.
/// `Some(empty)` binds every interface; `None` binds nothing.
pub(crate) fn compute_bind_addresses(
    addresses: &[String],
    usable: &UsableAddresses,
) -> Option<Vec<String>> {
    let network_ip_set: HashSet<&str> = usable.network_ips.values().map(|s| s.as_str()).collect();
    let bindable: Vec<String> = addresses
        .iter()
        .filter(|addr| {
            usable.interface_ips.iter().any(|ip| ip == *addr)
                || network_ip_set.contains(addr.as_str())
        })
        .cloned()
        .collect();
    // A Network name left after substitution did not resolve. Falling back to
    // every interface would expose the listeners on networks the Gateway
    // never named.
    if bindable.is_empty() && !unresolved_networks(addresses).is_empty() {
        return None;
    }
    Some(bindable)
}

/// Network names that did not resolve: IPAddress values are IP literals
/// (CRD-validated), so anything else left after substitution is a name.
pub(crate) fn unresolved_networks(addresses: &[String]) -> Vec<&str> {
    addresses
        .iter()
        .filter(|a| a.parse::<std::net::IpAddr>().is_err())
        .map(String::as_str)
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
            Some(vec!["10.244.1.5".to_string(), "192.168.10.3".to_string()])
        );
        assert_eq!(compute_bind_addresses(&[], &usable), Some(vec![]));
        // A VIP alone binds every interface; the Service carries it.
        assert_eq!(
            compute_bind_addresses(&["203.0.113.10".to_string()], &usable),
            Some(vec![])
        );
    }

    fn gateway_with_addresses(addrs: &[(&str, &str)]) -> Gateway {
        use gateway_api::gateways::{GatewayAddresses, GatewaySpec};
        Gateway {
            metadata: kube::core::ObjectMeta {
                name: Some("gw".into()),
                namespace: Some("default".into()),
                ..Default::default()
            },
            spec: GatewaySpec {
                gateway_class_name: "portail".into(),
                addresses: Some(
                    addrs
                        .iter()
                        .map(|(t, v)| GatewayAddresses {
                            r#type: Some(t.to_string()),
                            value: Some(v.to_string()),
                        })
                        .collect(),
                ),
                ..Default::default()
            },
            status: None,
        }
    }

    /// A Network entry binds this pod's IP on that network, not every
    /// interface. convert_gateway used to drop it, so the substitution never
    /// ran and listeners bound the wildcard.
    #[test]
    fn network_address_binds_its_local_ip() {
        let gw = gateway_with_addresses(&[
            (NETWORK_ADDRESS_TYPE, "udn-a"),
            ("Hostname", "gw.example.com"),
        ]);
        let mut config =
            crate::kubernetes::converters::convert_gateway(&gw, &Default::default()).unwrap();
        assert_eq!(config.addresses, vec!["udn-a".to_string()]);

        let usable = UsableAddresses {
            interface_ips: vec!["10.244.1.5".into(), "192.168.10.3".into()],
            network_ips: HashMap::from([("udn-a".into(), "192.168.10.3".into())]),
        };
        substitute_network_ips(&mut config.addresses, &usable);
        assert_eq!(
            compute_bind_addresses(&config.addresses, &usable),
            Some(vec!["192.168.10.3".to_string()])
        );
    }

    /// An unresolved Network name never falls back to every interface.
    #[test]
    fn unresolved_network_binds_nothing() {
        let usable = UsableAddresses {
            interface_ips: vec!["10.244.1.5".into()],
            network_ips: HashMap::new(),
        };
        let mut addrs = vec!["udn-a".to_string()];
        substitute_network_ips(&mut addrs, &usable);
        assert_eq!(compute_bind_addresses(&addrs, &usable), None);
        assert_eq!(unresolved_networks(&addrs), vec!["udn-a"]);

        // A local address still binds; the unresolved name adds nothing.
        addrs.push("10.244.1.5".to_string());
        assert_eq!(
            compute_bind_addresses(&addrs, &usable),
            Some(vec!["10.244.1.5".to_string()])
        );
    }
}
