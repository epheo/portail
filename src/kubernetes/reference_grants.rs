//! ReferenceGrant validation and route-to-listener authorization.
//!
//! Pure functions for checking namespace scoping, listener policy, and
//! cross-namespace reference permissions per Gateway API spec.

use std::collections::{BTreeMap, HashMap};

use gateway_api::gateways::{GatewayListeners, GatewayListenersAllowedRoutesNamespacesFrom};
use gateway_api::referencegrants::ReferenceGrant;

use super::hostname::hostnames_intersect;
use super::parent_ref::ParentRefAccess;

/// Check if a route in `route_ns` is allowed by the listener's allowedRoutes policy.
pub(crate) fn is_route_allowed_by_listener(
    listener: &GatewayListeners,
    gateway_ns: &str,
    route_ns: &str,
    route_kind: &str,
    namespace_labels: &HashMap<String, BTreeMap<String, String>>,
) -> bool {
    // The listener protocol decides the route kinds it takes. A listener the
    // data plane cannot run takes none.
    if !super::reconciler::listener_is_served(listener) {
        return false;
    }
    let protocol_allows = match listener.protocol.as_str() {
        "HTTP" | "HTTPS" => route_kind == "HTTPRoute",
        "TLS" => route_kind == "TLSRoute",
        "TCP" => route_kind == "TCPRoute",
        "UDP" => route_kind == "UDPRoute",
        _ => false,
    };
    if !protocol_allows {
        return false;
    }

    // Check allowedRoutes.kinds if specified. A kind of another group is not
    // ours: supportedKinds leaves it out (RouteGroupKind.group defaults to
    // gateway.networking.k8s.io).
    if let Some(ref allowed) = listener.allowed_routes {
        if let Some(ref kinds) = allowed.kinds {
            if !kinds.is_empty() {
                let kind_allowed = kinds.iter().any(|k| {
                    k.kind == route_kind
                        && k.group.as_deref().unwrap_or("gateway.networking.k8s.io")
                            == "gateway.networking.k8s.io"
                });
                if !kind_allowed {
                    return false;
                }
            }
        }
    }
    let allowed = match &listener.allowed_routes {
        None => return route_ns == gateway_ns,
        Some(ar) => ar,
    };
    let namespaces = match &allowed.namespaces {
        None => return route_ns == gateway_ns,
        Some(ns) => ns,
    };
    match &namespaces.from {
        None | Some(GatewayListenersAllowedRoutesNamespacesFrom::Same) => route_ns == gateway_ns,
        Some(GatewayListenersAllowedRoutesNamespacesFrom::All) => true,
        Some(GatewayListenersAllowedRoutesNamespacesFrom::Selector) => {
            let labels = match namespace_labels.get(route_ns) {
                Some(l) => l,
                None => return false,
            };
            let selector = match &namespaces.selector {
                Some(s) => s,
                None => return true,
            };
            // matchLabels: every key=value must be present
            if let Some(match_labels) = &selector.match_labels {
                for (k, v) in match_labels {
                    if labels.get(k) != Some(v) {
                        return false;
                    }
                }
            }
            // matchExpressions: each expression must match
            if let Some(exprs) = &selector.match_expressions {
                for expr in exprs {
                    let has = labels.get(&expr.key);
                    let vals = expr.values.as_deref().unwrap_or_default();
                    match expr.operator.as_str() {
                        "In" => {
                            if !has.is_some_and(|v| vals.contains(v)) {
                                return false;
                            }
                        }
                        "NotIn" => {
                            if has.is_some_and(|v| vals.contains(v)) {
                                return false;
                            }
                        }
                        "Exists" => {
                            if has.is_none() {
                                return false;
                            }
                        }
                        "DoesNotExist" => {
                            if has.is_some() {
                                return false;
                            }
                        }
                        _ => return false,
                    }
                }
            }
            true
        }
    }
}

/// Listeners one parentRef selects (sectionName/port) and is allowed to
/// attach to (hostname intersection + allowedRoutes policy). Acceptance is a
/// property of a single parentRef, not the whole route: a ref pinned to a
/// section that rejects it must not inherit Accepted from a sibling ref.
///
/// The caller has already matched the ref against the Gateway. `Err` carries
/// the most specific rejection across the selected listeners
/// (NotAllowedByListeners > NoMatchingListenerHostname > NoMatchingParent).
pub(crate) fn listeners_for_parent_ref<T: ParentRefAccess>(
    pr: &T,
    listeners: &[GatewayListeners],
    gateway_ns: &str,
    route_ns: &str,
    route_kind: &str,
    route_hostnames: &[String],
    namespace_labels: &HashMap<String, BTreeMap<String, String>>,
) -> Result<Vec<String>, &'static str> {
    let priority = |r: &str| match r {
        "NotAllowedByListeners" => 2,
        "NoMatchingListenerHostname" => 1,
        _ => 0,
    };
    let mut allowed = Vec::new();
    let mut best_reason: &'static str = "NoMatchingParent";
    for listener in listeners {
        if let Some(section) = pr.ref_section_name() {
            if section != listener.name {
                continue;
            }
        }
        if let Some(port) = pr.ref_port() {
            if port != listener.port {
                continue;
            }
        }
        if !hostnames_intersect(listener.hostname.as_deref(), route_hostnames) {
            if priority("NoMatchingListenerHostname") > priority(best_reason) {
                best_reason = "NoMatchingListenerHostname";
            }
            continue;
        }
        if is_route_allowed_by_listener(
            listener,
            gateway_ns,
            route_ns,
            route_kind,
            namespace_labels,
        ) {
            allowed.push(listener.name.clone());
        } else if priority("NotAllowedByListeners") > priority(best_reason) {
            best_reason = "NotAllowedByListeners";
        }
    }
    if allowed.is_empty() {
        Err(best_reason)
    } else {
        Ok(allowed)
    }
}

/// Check if a cross-namespace reference is allowed by a ReferenceGrant.
/// Same-namespace references are always allowed.
pub(crate) fn is_reference_allowed(
    grants: &[ReferenceGrant],
    from_group: &str,
    from_kind: &str,
    from_namespace: &str,
    to_group: &str,
    to_kind: &str,
    to_namespace: &str,
    to_name: &str,
) -> bool {
    if from_namespace == to_namespace {
        return true;
    }

    // Scan grants that live in the target namespace
    for grant in grants {
        let grant_ns = grant.metadata.namespace.as_deref().unwrap_or("default");
        if grant_ns != to_namespace {
            continue;
        }

        let from_match =
            grant.spec.from.iter().any(|f| {
                f.group == from_group && f.kind == from_kind && f.namespace == from_namespace
            });

        let to_match = grant.spec.to.iter().any(|t| {
            t.group == to_group && t.kind == to_kind && t.name.as_ref().is_none_or(|n| n == to_name)
        });

        if from_match && to_match {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway_api::gateways::{
        GatewayListenersAllowedRoutes, GatewayListenersAllowedRoutesKinds,
    };

    fn http_listener(kinds: Vec<(Option<&str>, &str)>) -> GatewayListeners {
        GatewayListeners {
            name: "http".into(),
            port: 80,
            protocol: "HTTP".into(),
            hostname: None,
            tls: None,
            allowed_routes: Some(GatewayListenersAllowedRoutes {
                kinds: Some(
                    kinds
                        .into_iter()
                        .map(|(group, kind)| GatewayListenersAllowedRoutesKinds {
                            group: group.map(str::to_string),
                            kind: kind.into(),
                        })
                        .collect(),
                ),
                namespaces: None,
            }),
        }
    }

    /// allowedRoutes.kinds names a group too: HTTPRoute of example.com is
    /// not ours, and supportedKinds already said so. It used to admit
    /// HTTPRoutes all the same.
    #[test]
    fn allowed_kind_of_another_group_admits_nothing() {
        let allowed = |listener: &GatewayListeners| {
            is_route_allowed_by_listener(listener, "ns", "ns", "HTTPRoute", &HashMap::new())
        };
        assert!(!allowed(&http_listener(vec![(
            Some("example.com"),
            "HTTPRoute"
        )])));
        assert!(allowed(&http_listener(vec![(None, "HTTPRoute")])));
        assert!(allowed(&http_listener(vec![(
            Some("gateway.networking.k8s.io"),
            "HTTPRoute"
        )])));
    }
}
