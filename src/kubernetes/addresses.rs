//! Gateway `spec.addresses` on the controller side: the
//! `portail.epheo.eu/Network` address type, read as the CIDRs of its
//! NetworkAttachmentDefinition.
//! Matching CIDRs against interfaces is the data plane's
//! ([`crate::dataplane::addresses`]).

use kube::api::Api;
use kube::Client;
use kube::ResourceExt;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use gateway_api::gateways::Gateway;

use crate::logging::{info, warn};
use crate::sync::NetworkCidrs;

/// Custom address type for binding a Gateway to a named network interface.
/// Used in `spec.addresses[].type` to auto-discover UDN/Multus IPs.
pub(crate) const NETWORK_ADDRESS_TYPE: &str = "portail.epheo.eu/Network";

/// What a NetworkAttachmentDefinition says about its network.
enum NetworkRead {
    Cidrs(Vec<String>),
    /// The network is gone, or names no subnet.
    Absent,
    /// The NAD could not be read: says nothing about the network.
    Unknown(kube::Error),
}

/// Read a network's NetworkAttachmentDefinition in `namespace` for its
/// subnet.
async fn read_network(client: &Client, network_name: &str, namespace: &str) -> NetworkRead {
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
    match nad_api.get(network_name).await {
        Ok(nad) => match nad_cidrs(&nad.data) {
            Some(cidrs) => NetworkRead::Cidrs(cidrs),
            None => {
                warn!("NetworkAttachmentDefinition {namespace}/{network_name} names no subnet");
                NetworkRead::Absent
            }
        },
        Err(kube::Error::Api(e)) if e.code == 404 => {
            warn!("NetworkAttachmentDefinition {namespace}/{network_name} not found");
            NetworkRead::Absent
        }
        Err(e) => NetworkRead::Unknown(e),
    }
}

/// The subnet a NAD's spec.config names, when it parses as a CIDR.
fn nad_cidrs(nad: &serde_json::Value) -> Option<Vec<String>> {
    let config: serde_json::Value = serde_json::from_str(nad["spec"]["config"].as_str()?).ok()?;
    let subnets = config["subnets"].as_str()?;
    crate::dataplane::addresses::parse_cidr(subnets)?;
    Some(vec![subnets.to_string()])
}

/// The last CIDRs each network was read with, by (namespace, network).
#[derive(Default)]
pub(crate) struct NetworkCidrCache(Mutex<HashMap<(String, String), Vec<String>>>);

impl NetworkCidrCache {
    /// Keep only the networks some Gateway of `gateways` names. A dropped
    /// entry left its CIDRs for a later failed read of a re-added network
    /// to bind, and the controller would keep one per network ever named.
    pub(crate) fn retain_named(&self, gateways: &[Arc<Gateway>]) {
        let named: HashSet<(String, String)> = gateways
            .iter()
            .flat_map(|gw| {
                let ns = gw.namespace().unwrap_or_else(|| "default".to_string());
                gw.spec
                    .addresses
                    .iter()
                    .flatten()
                    .filter(|a| a.r#type.as_deref() == Some(NETWORK_ADDRESS_TYPE))
                    .map(move |a| (ns.clone(), a.value.clone().unwrap_or_default()))
            })
            .collect();
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|key, _| named.contains(key));
    }
}

/// The CIDRs of the networks `gateway`'s Network addresses name, for its
/// data planes to find their own IPs in.
///
/// A NAD that cannot be read keeps the network's last CIDRs: unbinding on
/// an apiserver blip was an outage. Only an answer that the network is gone
/// drops them. With no last CIDRs, `Err`: no read says the network is
/// gone, so the snapshot served stays as it is.
pub(crate) async fn resolve_network_cidrs(
    client: &Client,
    gateway: &Gateway,
    gw_ns: &str,
    last: &NetworkCidrCache,
) -> Result<Vec<NetworkCidrs>, String> {
    let Some(spec_addrs) = &gateway.spec.addresses else {
        return Ok(Vec::new());
    };
    let gw_name = gateway.name_any();
    let mut networks = Vec::new();
    for addr in spec_addrs {
        if addr.r#type.as_deref() != Some(NETWORK_ADDRESS_TYPE) {
            continue;
        }
        let network_name = addr.value.as_deref().unwrap_or("");
        let key = (gw_ns.to_string(), network_name.to_string());
        let read = read_network(client, network_name, gw_ns).await;
        let mut known = last.0.lock().unwrap_or_else(|p| p.into_inner());
        let cidrs = match read {
            NetworkRead::Cidrs(cidrs) => {
                if known.get(&key) != Some(&cidrs) {
                    info!(
                        "Gateway {gw_ns}/{gw_name}: network '{network_name}' is {}",
                        cidrs.join(", ")
                    );
                    known.insert(key, cidrs.clone());
                }
                cidrs
            }
            NetworkRead::Absent => {
                known.remove(&key);
                continue;
            }
            NetworkRead::Unknown(e) => match known.get(&key) {
                Some(cidrs) => {
                    warn!(
                        "Gateway {gw_ns}/{gw_name}: NetworkAttachmentDefinition {network_name} \
                         not read ({e}); keeping {}",
                        cidrs.join(", ")
                    );
                    cidrs.clone()
                }
                None => {
                    return Err(format!(
                        "NetworkAttachmentDefinition {gw_ns}/{network_name} not read ({e}), \
                         and never read before"
                    ));
                }
            },
        };
        networks.push(NetworkCidrs {
            name: network_name.to_string(),
            cidrs,
        });
    }
    Ok(networks)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// A Network entry reaches the config as its name, for the data plane
    /// to swap for its IP. convert_gateway used to drop it, so listeners
    /// bound the wildcard.
    #[test]
    fn a_network_address_reaches_the_config_by_name() {
        let gw = gateway_with_addresses(&[
            (NETWORK_ADDRESS_TYPE, "udn-a"),
            ("Hostname", "gw.example.com"),
        ]);
        let config =
            crate::kubernetes::converters::convert_gateway(&gw, &Default::default()).unwrap();
        assert_eq!(config.addresses, vec!["udn-a".to_string()]);
    }

    #[test]
    fn a_nad_names_its_subnet() {
        let nad =
            |config: serde_json::Value| serde_json::json!({"spec": {"config": config.to_string()}});
        assert_eq!(
            nad_cidrs(&nad(serde_json::json!({"subnets": "10.10.1.0/24"}))),
            Some(vec!["10.10.1.0/24".to_string()])
        );
        assert_eq!(
            nad_cidrs(&nad(serde_json::json!({"subnets": "junk"}))),
            None
        );
        assert_eq!(nad_cidrs(&nad(serde_json::json!({}))), None);
    }

    /// Only the networks some Gateway still names keep their last CIDRs.
    #[test]
    fn network_cidrs_keep_only_named_networks() {
        let cache = NetworkCidrCache::default();
        for (ns, network) in [
            ("default", "udn-a"),
            ("default", "udn-b"),
            ("gone", "udn-a"),
        ] {
            cache
                .0
                .lock()
                .unwrap()
                .insert((ns.into(), network.into()), vec!["192.168.10.0/24".into()]);
        }
        let gw = gateway_with_addresses(&[(NETWORK_ADDRESS_TYPE, "udn-a"), ("IPAddress", "udn-b")]);
        cache.retain_named(&[Arc::new(gw)]);
        let kept: Vec<_> = cache.0.lock().unwrap().keys().cloned().collect();
        assert_eq!(kept, vec![("default".to_string(), "udn-a".to_string())]);

        cache.retain_named(&[]);
        assert!(cache.0.lock().unwrap().is_empty());
    }
}
