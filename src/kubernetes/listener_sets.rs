//! ListenerSet (Gateway API v1.7): listeners a Gateway takes from other
//! objects, when its `allowedListeners` admits them.
//!
//! A pass works on the merged Gateway: its own listeners, then those of its
//! admitted sets, oldest first, each under a name of its own
//! (`ls/{namespace}/{set}/{listener}`). The name says whose a listener is,
//! so what depends on it (allowedRoutes `Same`, a certificateRef's
//! namespace and its ReferenceGrant, status) reads it there. A Gateway's
//! own listener names are SectionNames, which hold no `/`: no collision.

use std::sync::Arc;

use gateway_api::gateways::{Gateway, GatewayAllowedListenersNamespacesFrom, GatewayListeners};
use gateway_api::listenersets::ListenerSet;
use k8s_openapi::api::core::v1::Namespace;
use kube::ResourceExt;

use super::reads::Recorded;

const PREFIX: &str = "ls/";
const GATEWAY_GROUP: &str = "gateway.networking.k8s.io";

/// A set's listener in the merged Gateway.
pub(crate) fn internal_name(namespace: &str, set: &str, listener: &str) -> String {
    format!("{PREFIX}{namespace}/{set}/{listener}")
}

/// Whose a merged listener is, when it is a set's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Owner<'a> {
    pub namespace: &'a str,
    pub set: &'a str,
    pub listener: &'a str,
}

pub(crate) fn owner(name: &str) -> Option<Owner<'_>> {
    let mut parts = name.strip_prefix(PREFIX)?.splitn(3, '/');
    Some(Owner {
        namespace: parts.next()?,
        set: parts.next()?,
        listener: parts.next()?,
    })
}

/// The namespace a merged listener is of: its set's, else the Gateway's.
pub(crate) fn namespace_of<'a>(listener: &'a str, gateway_ns: &'a str) -> &'a str {
    owner(listener).map_or(gateway_ns, |o| o.namespace)
}

/// A set naming a Gateway, and whether the Gateway admits it.
pub(crate) struct Attached {
    pub set: Arc<ListenerSet>,
    pub admitted: bool,
}

/// Whether `set`'s parentRef names `gateway_ns/gateway_name`.
pub(crate) fn names_gateway(set: &ListenerSet, gateway_name: &str, gateway_ns: &str) -> bool {
    let parent = &set.spec.parent_ref;
    parent.group.as_deref().unwrap_or(GATEWAY_GROUP) == GATEWAY_GROUP
        && parent.kind.as_deref().unwrap_or("Gateway") == "Gateway"
        && parent.name == gateway_name
        && parent
            .namespace
            .as_deref()
            .unwrap_or(&set.namespace().unwrap_or_default())
            == gateway_ns
}

/// The sets of `sets` naming `gateway`, in the merge's order (oldest, then
/// namespace and name), each admitted or not by its `allowedListeners`.
/// A namespace selector reads the set's namespace through `namespaces`.
pub(crate) fn attached(
    gateway: &Gateway,
    gateway_ns: &str,
    sets: &[Arc<ListenerSet>],
    namespaces: &Recorded<Namespace>,
) -> Vec<Attached> {
    let gateway_name = gateway.name_any();
    let mut named: Vec<&Arc<ListenerSet>> = sets
        .iter()
        .filter(|s| names_gateway(s, &gateway_name, gateway_ns))
        .collect();
    named.sort_by_key(|s| {
        (
            s.metadata.creation_timestamp.clone(),
            s.namespace().unwrap_or_default(),
            s.name_any(),
        )
    });
    let allowed = gateway
        .spec
        .allowed_listeners
        .as_ref()
        .and_then(|a| a.namespaces.as_ref());
    named
        .into_iter()
        .map(|set| {
            let set_ns = set.namespace().unwrap_or_default();
            let admitted = match allowed.and_then(|a| a.from.as_ref()) {
                None | Some(GatewayAllowedListenersNamespacesFrom::None) => false,
                Some(GatewayAllowedListenersNamespacesFrom::Same) => set_ns == gateway_ns,
                Some(GatewayAllowedListenersNamespacesFrom::All) => true,
                Some(GatewayAllowedListenersNamespacesFrom::Selector) => {
                    let selector = allowed.and_then(|a| a.selector.as_ref());
                    namespaces.get_cluster(&set_ns).is_some_and(|ns| {
                        let labels = ns.metadata.labels.clone().unwrap_or_default();
                        selector.is_some_and(|s| {
                            let expressions = s.match_expressions.iter().flatten().map(|e| {
                                (e.key.as_str(), e.operator.as_str(), e.values.as_deref())
                            });
                            selects(&labels, s.match_labels.as_ref(), expressions)
                        })
                    })
                }
            };
            Attached {
                set: set.clone(),
                admitted,
            }
        })
        .collect()
}

/// A label selector over `labels`: every matchLabels pair present, every
/// expression holding. An unknown operator selects nothing.
fn selects<'a>(
    labels: &std::collections::BTreeMap<String, String>,
    match_labels: Option<&std::collections::BTreeMap<String, String>>,
    expressions: impl IntoIterator<Item = (&'a str, &'a str, Option<&'a [String]>)>,
) -> bool {
    let pairs = match_labels
        .into_iter()
        .flatten()
        .all(|(k, v)| labels.get(k) == Some(v));
    pairs
        && expressions.into_iter().all(|(key, operator, values)| {
            let has = labels.get(key);
            let values = values.unwrap_or_default();
            match operator {
                "In" => has.is_some_and(|v| values.contains(v)),
                "NotIn" => !has.is_some_and(|v| values.contains(v)),
                "Exists" => has.is_some(),
                "DoesNotExist" => has.is_none(),
                _ => false,
            }
        })
}

/// `gateway` with the listeners of `admitted` after its own, in order,
/// each under its internal name, a certificateRef without a namespace
/// given its set's.
pub(crate) fn merged<'a>(
    gateway: &Gateway,
    admitted: impl IntoIterator<Item = &'a ListenerSet>,
) -> Gateway {
    let mut merged = gateway.clone();
    for set in admitted {
        let set_ns = set.namespace().unwrap_or_default();
        let set_name = set.name_any();
        for entry in &set.spec.listeners {
            // The two generated types are one JSON shape.
            let Ok(mut listener) =
                serde_json::to_value(entry).and_then(serde_json::from_value::<GatewayListeners>)
            else {
                continue;
            };
            listener.name = internal_name(&set_ns, &set_name, &entry.name);
            for cert in listener
                .tls
                .iter_mut()
                .flat_map(|tls| tls.certificate_refs.iter_mut().flatten())
            {
                cert.namespace.get_or_insert_with(|| set_ns.clone());
            }
            merged.spec.listeners.push(listener);
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
    use serde_json::json;

    fn gateway(allowed: serde_json::Value) -> Gateway {
        serde_json::from_value(json!({
            "metadata": {"name": "gw", "namespace": "infra"},
            "spec": {
                "gatewayClassName": "portail",
                "listeners": [{"name": "http", "port": 80, "protocol": "HTTP"}],
                "allowedListeners": allowed,
            },
        }))
        .unwrap()
    }

    fn set(ns: &str, name: &str, second: i64, parent: serde_json::Value) -> Arc<ListenerSet> {
        let mut set: ListenerSet = serde_json::from_value(json!({
            "metadata": {"name": name, "namespace": ns},
            "spec": {
                "parentRef": parent,
                "listeners": [{
                    "name": "https", "port": 443, "protocol": "HTTPS",
                    "tls": {"certificateRefs": [{"name": "cert"}]},
                }],
            },
        }))
        .unwrap();
        set.metadata.creation_timestamp = Some(Time(
            k8s_openapi::jiff::Timestamp::from_second(second).unwrap(),
        ));
        Arc::new(set)
    }

    fn namespaces() -> Recorded<Namespace> {
        crate::kubernetes::reads::namespaces(&std::collections::HashMap::from([
            ("infra".to_string(), Default::default()),
            (
                "team".to_string(),
                std::collections::BTreeMap::from([("gateway".to_string(), "yes".to_string())]),
            ),
            ("other".to_string(), Default::default()),
        ]))
    }

    fn admitted(gw: &Gateway, sets: &[Arc<ListenerSet>]) -> Vec<(String, bool)> {
        attached(gw, "infra", sets, &namespaces())
            .into_iter()
            .map(|a| (a.set.name_any(), a.admitted))
            .collect()
    }

    /// The Gateway admits nothing unless it says so; then the namespaces
    /// it names, by sameness, all, or label.
    #[test]
    fn allowed_listeners_admit_by_namespace() {
        let sets = [
            set("infra", "same", 1, json!({"name": "gw"})),
            set(
                "team",
                "labelled",
                2,
                json!({"name": "gw", "namespace": "infra"}),
            ),
            set(
                "other",
                "plain",
                3,
                json!({"name": "gw", "namespace": "infra"}),
            ),
        ];
        let none = admitted(&gateway(json!(null)), &sets);
        assert!(none.iter().all(|(_, a)| !a), "{none:?}");
        let same = admitted(&gateway(json!({"namespaces": {"from": "Same"}})), &sets);
        assert_eq!(
            same,
            [
                ("same".into(), true),
                ("labelled".into(), false),
                ("plain".into(), false)
            ]
        );
        let all = admitted(&gateway(json!({"namespaces": {"from": "All"}})), &sets);
        assert!(all.iter().all(|(_, a)| *a), "{all:?}");
        let labelled = admitted(
            &gateway(json!({"namespaces": {"from": "Selector",
                "selector": {"matchLabels": {"gateway": "yes"}}}})),
            &sets,
        );
        assert_eq!(
            labelled,
            [
                ("same".into(), false),
                ("labelled".into(), true),
                ("plain".into(), false)
            ]
        );
    }

    /// Only sets naming this Gateway, oldest first then by namespace and
    /// name; a parentRef without a namespace means the set's.
    #[test]
    fn sets_naming_the_gateway_in_the_merge_order() {
        let sets = [
            set("infra", "b", 5, json!({"name": "gw"})),
            set("infra", "a", 5, json!({"name": "gw"})),
            set("infra", "old", 1, json!({"name": "gw"})),
            set("team", "elsewhere", 1, json!({"name": "gw"})),
            set("infra", "other-gw", 1, json!({"name": "gw2"})),
        ];
        let names: Vec<_> = admitted(&gateway(json!({"namespaces": {"from": "All"}})), &sets)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, ["old", "a", "b"]);
    }

    /// The merged Gateway: its own listeners first, a set's under its
    /// internal name, a certificateRef given the set's namespace.
    #[test]
    fn a_sets_listeners_follow_the_gateways_own() {
        let gw = gateway(json!({"namespaces": {"from": "All"}}));
        let team = set(
            "team",
            "web",
            1,
            json!({"name": "gw", "namespace": "infra"}),
        );
        let merged = merged(&gw, [&*team]);
        let names: Vec<_> = merged
            .spec
            .listeners
            .iter()
            .map(|l| l.name.as_str())
            .collect();
        assert_eq!(names, ["http", "ls/team/web/https"]);
        let cert = &merged.spec.listeners[1]
            .tls
            .as_ref()
            .unwrap()
            .certificate_refs
            .as_ref()
            .unwrap()[0];
        assert_eq!(cert.namespace.as_deref(), Some("team"));
        assert_eq!(
            owner("ls/team/web/https"),
            Some(Owner {
                namespace: "team",
                set: "web",
                listener: "https"
            })
        );
        assert_eq!(owner("http"), None);
        assert_eq!(namespace_of("ls/team/web/https", "infra"), "team");
        assert_eq!(namespace_of("http", "infra"), "infra");
    }
}
