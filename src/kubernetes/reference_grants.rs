//! ReferenceGrant validation and route-to-listener authorization.
//!
//! Pure functions for checking namespace scoping, listener policy, and
//! cross-namespace reference permissions per Gateway API spec.

use std::collections::{BTreeMap, HashMap};

use gateway_api::gateways::{
    GatewayListeners,
    GatewayListenersAllowedRoutesNamespacesFrom,
};
use gateway_api::referencegrants::ReferenceGrant;

use super::hostname::hostnames_intersect;
use super::parent_ref::{ParentRefAccess, parent_ref_matches_gateway};

/// Result of checking whether a route is allowed to attach to a listener.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum RouteAllowResult {
    Allowed,
    Rejected(&'static str),
}

/// Check if a route in `route_ns` is allowed by the listener's allowedRoutes policy.
pub(crate) fn is_route_allowed_by_listener(
    listener: &GatewayListeners,
    gateway_ns: &str,
    route_ns: &str,
    route_kind: &str,
    namespace_labels: &HashMap<String, BTreeMap<String, String>>,
) -> bool {
    // Check protocol ↔ route kind enforcement
    let protocol_allows = match listener.protocol.as_str() {
        "HTTP" | "HTTPS" => route_kind == "HTTPRoute",
        "TLS" => route_kind == "TLSRoute",
        "TCP" => route_kind == "TCPRoute",
        "UDP" => route_kind == "UDPRoute",
        _ => true, // Unknown protocol, allow anything
    };
    if !protocol_allows {
        return false;
    }

    // Check allowedRoutes.kinds if specified
    if let Some(ref allowed) = listener.allowed_routes {
        if let Some(ref kinds) = allowed.kinds {
            if !kinds.is_empty() {
                let kind_allowed = kinds.iter().any(|k| {
                    k.kind == route_kind
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
        None | Some(GatewayListenersAllowedRoutesNamespacesFrom::Same) => {
            route_ns == gateway_ns
        }
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

/// Check if a route targets a specific listener by sectionName, and if so, whether
/// the listener's namespace/hostname/kind policy allows the route.
pub(crate) fn route_allowed_for_listener<T: ParentRefAccess>(
    parent_refs: &Option<Vec<T>>,
    gateway_name: &str,
    listener: &GatewayListeners,
    gateway_ns: &str,
    route_ns: &str,
    route_kind: &str,
    route_hostnames: &[String],
    namespace_labels: &HashMap<String, BTreeMap<String, String>>,
) -> RouteAllowResult {
    let refs = match parent_refs.as_ref() {
        Some(r) => r,
        None => return RouteAllowResult::Rejected("NoMatchingParent"),
    };
    let mut best_reason: &str = "NoMatchingParent";
    for pr in refs {
        if !parent_ref_matches_gateway(pr, gateway_name, gateway_ns) {
            continue;
        }
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
            best_reason = "NoMatchingListenerHostname";
            continue;
        }
        if is_route_allowed_by_listener(listener, gateway_ns, route_ns, route_kind, namespace_labels) {
            return RouteAllowResult::Allowed;
        }
        best_reason = "NotAllowedByListeners";
    }
    RouteAllowResult::Rejected(best_reason)
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

        let from_match = grant.spec.from.iter().any(|f| {
            f.group == from_group && f.kind == from_kind && f.namespace == from_namespace
        });

        let to_match = grant.spec.to.iter().any(|t| {
            t.group == to_group
                && t.kind == to_kind
                && t.name.as_ref().is_none_or(|n| n == to_name)
        });

        if from_match && to_match {
            return true;
        }
    }

    false
}
