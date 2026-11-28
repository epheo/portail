//! The CA bundles of the ConfigMaps BackendTLSPolicies name.
//!
//! ConfigMaps are watched by metadata only: a cluster holds thousands, some
//! large, and policies name few. A pass fetches the data of those it needs,
//! once per resourceVersion, and keeps their `ca.crt` alone.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use gateway_api::gateways::Gateway;
use k8s_openapi::api::core::v1::ConfigMap;
use kube::core::PartialObjectMeta;
use kube::{Api, Client, ResourceExt};

use crate::kubernetes::backend_tls::{CaRef, ConfigMapKey, CA_KEY};
use crate::kubernetes::reads::Recorded;
use crate::sync::GatewayKey;

#[derive(Default)]
pub(super) struct CaBundles {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// What each Gateway's last pass needed: an entry none needs goes.
    wanted: HashMap<GatewayKey, BTreeSet<ConfigMapKey>>,
    /// By the resourceVersion it was fetched at.
    fetched: HashMap<ConfigMapKey, (Option<String>, CaRef)>,
}

impl Inner {
    fn evict(&mut self) {
        let needed: HashSet<&ConfigMapKey> = self.wanted.values().flatten().collect();
        self.fetched.retain(|key, _| needed.contains(key));
    }
}

impl CaBundles {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The ConfigMaps of `wanted`, for `gateway`'s pass. Each is read
    /// through `metadata`, so its change wakes the pass, and fetched unless
    /// held at the version the metadata shows. One gone before its fetch is
    /// missing; any other failed fetch fails the pass, which keeps what the
    /// data plane serves rather than refusing its backends.
    pub(super) async fn resolve(
        &self,
        client: &Client,
        metadata: &Recorded<PartialObjectMeta<ConfigMap>>,
        gateway: &GatewayKey,
        wanted: BTreeSet<ConfigMapKey>,
    ) -> Result<HashMap<ConfigMapKey, CaRef>, kube::Error> {
        let mut found = HashMap::new();
        let mut fetch = Vec::new();
        {
            let mut inner = self.lock();
            for key in &wanted {
                let Some(meta) = metadata.get(&key.0, &key.1) else {
                    inner.fetched.remove(key);
                    found.insert(key.clone(), CaRef::Missing);
                    continue;
                };
                match inner.fetched.get(key) {
                    Some((version, ca)) if *version == meta.metadata.resource_version => {
                        found.insert(key.clone(), ca.clone());
                    }
                    _ => fetch.push(key.clone()),
                }
            }
        }
        let fetched = futures::future::try_join_all(fetch.into_iter().map(|key| async move {
            let api = Api::<ConfigMap>::namespaced(client.clone(), &key.0);
            api.get_opt(&key.1).await.map(|cm| (key, cm))
        }))
        .await?;
        let mut inner = self.lock();
        for (key, cm) in fetched {
            let Some(cm) = cm else {
                inner.fetched.remove(&key);
                found.insert(key, CaRef::Missing);
                continue;
            };
            let ca = match cm.data.as_ref().and_then(|d| d.get(CA_KEY)) {
                Some(pem) => CaRef::Pem(pem.clone()),
                None => CaRef::NoKey,
            };
            inner
                .fetched
                .insert(key.clone(), (cm.metadata.resource_version, ca.clone()));
            found.insert(key, ca);
        }
        inner.wanted.insert(gateway.clone(), wanted);
        inner.evict();
        Ok(found)
    }

    /// Forget the Gateways not among `gateways`, and what only they needed.
    pub(super) fn retain(&self, gateways: &[Arc<Gateway>]) {
        let live: HashSet<GatewayKey> = gateways
            .iter()
            .map(|g| GatewayKey::new(g.namespace().unwrap_or_default(), g.name_any()))
            .collect();
        let mut inner = self.lock();
        inner.wanted.retain(|gateway, _| live.contains(gateway));
        inner.evict();
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::MockApi;
    use super::*;
    use crate::kubernetes::reads::Kind;
    use serde_json::json;

    fn meta(name: &str, version: &str) -> PartialObjectMeta<ConfigMap> {
        serde_json::from_value(json!({
            "apiVersion": "meta.k8s.io/v1",
            "kind": "PartialObjectMetadata",
            "metadata": {"name": name, "namespace": "apps", "resourceVersion": version},
        }))
        .unwrap()
    }

    fn configmap(name: &str, version: &str, data: serde_json::Value) -> serde_json::Value {
        json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": name, "namespace": "apps", "resourceVersion": version},
            "data": data,
        })
    }

    fn gets(api: &MockApi) -> Vec<String> {
        let gets = api
            .requests()
            .into_iter()
            .filter(|r| r.method == "GET")
            .map(|r| r.path)
            .collect();
        api.clear();
        gets
    }

    fn key(name: &str) -> ConfigMapKey {
        ("apps".into(), name.into())
    }

    /// A ConfigMap is fetched once per resourceVersion; one the metadata
    /// does not show is missing unfetched; a Gateway gone takes what only
    /// it needed.
    #[tokio::test]
    async fn a_ca_bundle_is_fetched_once_per_version() {
        let api = MockApi::default();
        let client = api.client();
        let cas = CaBundles::default();
        let gw = GatewayKey::new("apps", "gw");
        let path = "/api/v1/namespaces/apps/configmaps/ca";
        api.reply_to(path, 200, configmap("ca", "1", json!({CA_KEY: "PEM"})));
        let wanted = || BTreeSet::from([key("ca"), key("absent")]);

        let store = Recorded::of(Kind::ConfigMap, [meta("ca", "1")]);
        let found = cas.resolve(&client, &store, &gw, wanted()).await.unwrap();
        assert_eq!(found[&key("ca")], CaRef::Pem("PEM".into()));
        assert_eq!(found[&key("absent")], CaRef::Missing);
        assert_eq!(gets(&api), [path]);

        let found = cas.resolve(&client, &store, &gw, wanted()).await.unwrap();
        assert_eq!(found[&key("ca")], CaRef::Pem("PEM".into()));
        assert!(gets(&api).is_empty(), "held at its version");

        api.reply_to(path, 200, configmap("ca", "2", json!({"other": "x"})));
        let store = Recorded::of(Kind::ConfigMap, [meta("ca", "2")]);
        let found = cas.resolve(&client, &store, &gw, wanted()).await.unwrap();
        assert_eq!(found[&key("ca")], CaRef::NoKey);
        assert_eq!(gets(&api), [path]);

        cas.retain(&[]);
        assert!(cas.lock().fetched.is_empty());
    }

    /// A ConfigMap deleted before its fetch is missing; a fetch that fails
    /// otherwise fails the pass.
    #[tokio::test]
    async fn a_failed_fetch_fails_the_pass() {
        let api = MockApi::default();
        let client = api.client();
        let cas = CaBundles::default();
        let gw = GatewayKey::new("apps", "gw");
        let store = Recorded::of(Kind::ConfigMap, [meta("ca", "1")]);
        let path = "/api/v1/namespaces/apps/configmaps/ca";
        let status = |code: u16, reason: &str| {
            json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                   "reason": reason, "code": code, "message": reason})
        };

        api.reply_to(path, 404, status(404, "NotFound"));
        let found = cas
            .resolve(&client, &store, &gw, BTreeSet::from([key("ca")]))
            .await
            .unwrap();
        assert_eq!(found[&key("ca")], CaRef::Missing);

        api.reply_to(path, 500, status(500, "InternalError"));
        let failed = cas
            .resolve(&client, &store, &gw, BTreeSet::from([key("ca")]))
            .await;
        assert!(failed.is_err());
    }
}
