//! What a data plane sends: the poll request, and the report of what it
//! bound for the version it serves. Programmed status is computed from
//! these, never from the spec alone.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::GatewayKey;
use crate::config::Protocol;

/// The body of `POST /v1/sync`. The instance's identity is the pod its
/// token is bound to, never a field here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncRequest {
    pub protocol: u32,
    pub gateway: GatewayKey,
    /// The version served; empty before the first snapshot.
    #[serde(default)]
    pub have: String,
    #[serde(default)]
    pub data_plane_version: String,
    /// Absent before the first apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<InstanceReport>,
}

/// One data-plane instance's state after its last apply.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InstanceReport {
    /// The snapshot version this report describes.
    pub applied: String,
    /// Why that snapshot's route table did not build: the previous table
    /// serves on.
    pub apply_error: Option<String>,
    /// The snapshot's listeners, each with the endpoints it binds.
    pub listeners: Vec<ListenerReport>,
    /// The pod's addresses off loopback.
    pub interface_ips: Vec<String>,
    /// Network name to the pod's IP on it, for the snapshot's networks
    /// that matched a local interface.
    pub networks: BTreeMap<String, String>,
    /// DNS refresh swaps so far: a move means backend pods came or went,
    /// and named targetPorts may have moved with them.
    pub dns_epoch: u64,
    /// Sent on SIGTERM: the instance no longer counts as live.
    pub leaving: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ListenerReport {
    pub name: String,
    /// Empty when no address of the Gateway is on the pod: nothing binds.
    pub endpoints: Vec<EndpointReport>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointReport {
    /// `None`: every interface.
    #[serde(default)]
    pub address: Option<String>,
    /// The port bound: the listener's targetPort when it has one.
    pub port: u16,
    pub transport: Transport,
    pub bound: bool,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Transport {
    Tcp,
    Udp,
}

impl Transport {
    pub fn of(protocol: &Protocol) -> Self {
        match protocol {
            Protocol::UDP => Transport::Udp,
            Protocol::HTTP | Protocol::HTTPS | Protocol::TLS | Protocol::TCP => Transport::Tcp,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full request, with fields a newer data plane adds.
    #[test]
    fn a_request_parses_and_tolerates_new_fields() {
        let request: SyncRequest = serde_json::from_value(serde_json::json!({
            "protocol": 1,
            "gateway": {"namespace": "portail-system", "name": "public"},
            "have": "9f1c0e4b7a2d3c5e",
            "dataPlaneVersion": "0.2.0",
            "transport": "direct",
            "report": {
                "applied": "9f1c0e4b7a2d3c5e",
                "applyError": null,
                "listeners": [{"name": "https-desku-be", "endpoints": [{
                    "address": "172.16.87.21", "port": 443, "transport": "TCP",
                    "bound": true, "error": null, "fd": 7,
                }]}],
                "interfaceIps": ["10.42.0.17", "172.16.87.21"],
                "networks": {},
                "dnsEpoch": 41,
                "leaving": false,
                "rss": 1024,
            },
        }))
        .unwrap();
        let report = request.report.as_ref().unwrap();
        assert_eq!(request.gateway, GatewayKey::new("portail-system", "public"));
        assert_eq!(report.dns_epoch, 41);
        let endpoint = &report.listeners[0].endpoints[0];
        assert_eq!(endpoint.transport, Transport::Tcp);
        assert!(endpoint.bound);
        let again: SyncRequest =
            serde_json::from_str(&serde_json::to_string(&request).unwrap()).unwrap();
        assert_eq!(again, request);
    }

    #[test]
    fn a_first_poll_carries_no_report() {
        let request: SyncRequest = serde_json::from_value(serde_json::json!({
            "protocol": 1,
            "gateway": {"namespace": "ns", "name": "gw"},
        }))
        .unwrap();
        assert!(request.have.is_empty() && request.report.is_none());
        assert!(!serde_json::to_string(&request).unwrap().contains("report"));
    }
}
