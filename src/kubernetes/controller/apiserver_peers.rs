//! The apiserver's own addresses, from the EndpointSlices of Service
//! default/kubernetes. A sync request from one of them came through the
//! service proxy, which authenticated its caller and checked its RBAC
//! (`services/proxy` on the controller Service): every proxied data plane
//! arrives from those few addresses, so the per-peer caps would count them
//! all as one pod. They do not apply to these peers; the global caps do.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::api::{Api, ListParams};
use kube::Client;
use tokio_util::sync::CancellationToken;

use crate::logging::{info, warn};

/// Where the apiserver lists its endpoints: Service `kubernetes` in
/// `default`, whose slices carry the service-name label.
const NAMESPACE: &str = "default";
const SELECTOR: &str = "kubernetes.io/service-name=kubernetes";

/// The peers exempt from the per-peer sync caps.
#[derive(Debug, Default)]
pub(crate) struct ApiserverPeers(RwLock<BTreeSet<IpAddr>>);

impl ApiserverPeers {
    #[cfg(test)]
    pub(crate) fn of(ips: impl IntoIterator<Item = IpAddr>) -> Self {
        Self(RwLock::new(ips.into_iter().collect()))
    }

    pub(crate) fn contains(&self, peer: IpAddr) -> bool {
        self.0
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&peer.to_canonical())
    }

    /// Whether the set changed.
    fn replace(&self, ips: BTreeSet<IpAddr>) -> bool {
        let mut current = self.0.write().unwrap_or_else(|p| p.into_inner());
        let changed = *current != ips;
        *current = ips;
        changed
    }

    /// List the apiserver's endpoints now and every `every` until
    /// `shutdown`. A failed list keeps the last set and is tried again
    /// after `retry`.
    pub(crate) async fn keep_listed(
        self: Arc<Self>,
        client: Client,
        every: Duration,
        retry: Duration,
        shutdown: CancellationToken,
    ) {
        let api: Api<EndpointSlice> = Api::namespaced(client, NAMESPACE);
        loop {
            let wait = match list(&api).await {
                Ok(ips) => {
                    let shown: Vec<String> = ips.iter().map(IpAddr::to_string).collect();
                    if self.replace(ips) {
                        info!(
                            "Apiserver endpoints, exempt from the per-peer sync caps: [{}]",
                            shown.join(", ")
                        );
                    }
                    every
                }
                Err(e) => {
                    warn!(
                        "Listing the apiserver's endpoints failed: {}; retrying in {:?}",
                        e, retry
                    );
                    retry
                }
            };
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }
}

async fn list(api: &Api<EndpointSlice>) -> Result<BTreeSet<IpAddr>, kube::Error> {
    let slices = api.list(&ListParams::default().labels(SELECTOR)).await?;
    Ok(slices
        .items
        .iter()
        .flat_map(|slice| &slice.endpoints)
        .flat_map(|endpoint| &endpoint.addresses)
        .filter_map(|address| address.parse::<IpAddr>().ok())
        .map(|ip| ip.to_canonical())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::super::testkit::MockApi;
    use super::*;

    const SLICES: &str = "/apis/discovery.k8s.io/v1/namespaces/default/endpointslices";

    fn slices(addresses: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "discovery.k8s.io/v1", "kind": "EndpointSliceList", "metadata": {},
            "items": [{
                "metadata": {"name": "kubernetes"},
                "addressType": "IPv4",
                "endpoints": addresses.iter().map(|a| serde_json::json!({"addresses": [a]})).collect::<Vec<_>>(),
                "ports": [{"name": "https", "port": 6443}],
            }],
        })
    }

    #[tokio::test]
    async fn the_apiserver_endpoints_are_listed_by_their_service_label() {
        let api = MockApi::default();
        api.reply_to(SLICES, 200, slices(&["10.0.0.10", "10.0.0.11"]));
        let ips = list(&Api::namespaced(api.client(), NAMESPACE))
            .await
            .unwrap();
        let peers = ApiserverPeers::default();
        assert!(peers.replace(ips));
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert!(peers.contains(ip("10.0.0.10")) && peers.contains(ip("10.0.0.11")));
        assert!(
            peers.contains(ip("::ffff:10.0.0.10")),
            "as an accept may spell it"
        );
        assert!(!peers.contains(ip("10.0.0.12")));
        let request = &api.requests()[0];
        assert!(
            request
                .path
                .contains("labelSelector=kubernetes.io%2Fservice-name%3Dkubernetes"),
            "{}",
            request.path
        );
    }

    /// A failed list keeps the last set: an apiserver hiccup does not put
    /// every proxied data plane under one pod's caps.
    #[tokio::test(start_paused = true)]
    async fn a_failed_list_keeps_the_last_set() {
        let api = MockApi::default();
        api.reply_to(SLICES, 200, slices(&["10.0.0.10"]));
        let peers = Arc::new(ApiserverPeers::default());
        let shutdown = CancellationToken::new();
        let every = Duration::from_secs(600);
        let retry = Duration::from_secs(30);
        tokio::spawn(
            peers
                .clone()
                .keep_listed(api.client(), every, retry, shutdown.clone()),
        );
        let ten = "10.0.0.10".parse().unwrap();
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert!(peers.contains(ten));

        api.reply_to(SLICES, 500, serde_json::json!({}));
        tokio::time::sleep(every).await;
        assert!(peers.contains(ten), "kept through the failure");
        api.reply_to(SLICES, 200, slices(&["10.0.0.20"]));
        tokio::time::sleep(retry).await;
        assert!(!peers.contains(ten));
        assert!(peers.contains("10.0.0.20".parse().unwrap()));
        shutdown.cancel();
    }
}
