//! Trait abstractions for Gateway API parent and backend reference types.
//!
//! Reduces boilerplate with macros — all four route types (HTTP, TCP, TLS, UDP)
//! share identical field layouts for parentRefs and backendRefs.

use gateway_api::httproutes::*;
use gateway_api::experimental::tcproutes::*;
use gateway_api::experimental::tlsroutes::*;
use gateway_api::experimental::udproutes::*;

use crate::config::ParentRef;

// ---------------------------------------------------------------------------
// ParentRefAccess — abstracts over HTTPRouteParentRefs, TCPRouteParentRefs, etc.
// ---------------------------------------------------------------------------

pub(crate) trait ParentRefAccess {
    fn ref_name(&self) -> &str;
    fn ref_section_name(&self) -> Option<&str>;
    fn ref_group(&self) -> Option<&str>;
    fn ref_kind(&self) -> Option<&str>;
    fn ref_namespace(&self) -> Option<&str>;
    fn ref_port(&self) -> Option<i32>;
}

macro_rules! impl_parent_ref_access {
    ($ty:ty) => {
        impl ParentRefAccess for $ty {
            fn ref_name(&self) -> &str { &self.name }
            fn ref_section_name(&self) -> Option<&str> { self.section_name.as_deref() }
            fn ref_group(&self) -> Option<&str> { self.group.as_deref() }
            fn ref_kind(&self) -> Option<&str> { self.kind.as_deref() }
            fn ref_namespace(&self) -> Option<&str> { self.namespace.as_deref() }
            fn ref_port(&self) -> Option<i32> { self.port }
        }
    };
}

impl_parent_ref_access!(HTTPRouteParentRefs);
impl_parent_ref_access!(TCPRouteParentRefs);
impl_parent_ref_access!(TLSRouteParentRefs);
impl_parent_ref_access!(UDPRouteParentRefs);

// ---------------------------------------------------------------------------
// L4BackendRefAccess — abstracts over TCP/TLS/UDP backend ref types
// ---------------------------------------------------------------------------

pub(crate) trait L4BackendRefAccess {
    fn br_name(&self) -> &str;
    fn br_namespace(&self) -> Option<&str>;
    fn br_port(&self) -> Option<i32>;
    fn br_weight(&self) -> Option<i32>;
    fn br_group(&self) -> Option<&str>;
    fn br_kind(&self) -> Option<&str>;
}

macro_rules! impl_l4_backend_ref_access {
    ($ty:ty) => {
        impl L4BackendRefAccess for $ty {
            fn br_name(&self) -> &str { &self.name }
            fn br_namespace(&self) -> Option<&str> { self.namespace.as_deref() }
            fn br_port(&self) -> Option<i32> { self.port }
            fn br_weight(&self) -> Option<i32> { self.weight }
            fn br_group(&self) -> Option<&str> { self.group.as_deref() }
            fn br_kind(&self) -> Option<&str> { self.kind.as_deref() }
        }
    };
}

impl_l4_backend_ref_access!(TCPRouteRulesBackendRefs);
impl_l4_backend_ref_access!(TLSRouteRulesBackendRefs);
impl_l4_backend_ref_access!(UDPRouteRulesBackendRefs);

// ---------------------------------------------------------------------------
// Utility functions operating on ParentRefAccess
// ---------------------------------------------------------------------------

/// Check if a parentRef matches this gateway (name + group + kind + namespace).
pub(crate) fn parent_ref_matches_gateway<T: ParentRefAccess>(pr: &T, gateway_name: &str, gateway_ns: &str) -> bool {
    if pr.ref_name() != gateway_name {
        return false;
    }
    // group must be "gateway.networking.k8s.io" or unset
    if let Some(group) = pr.ref_group() {
        if group != "gateway.networking.k8s.io" {
            return false;
        }
    }
    // kind must be "Gateway" or unset
    if let Some(kind) = pr.ref_kind() {
        if kind != "Gateway" {
            return false;
        }
    }
    // namespace must match the gateway's namespace (or be unset = route's own ns)
    if let Some(ns) = pr.ref_namespace() {
        if ns != gateway_ns {
            return false;
        }
    }
    true
}

/// Check if any parentRef in the route targets the given gateway.
pub(crate) fn route_targets_gateway<T: ParentRefAccess>(parent_refs: &Option<Vec<T>>, gateway_name: &str, gateway_ns: &str) -> bool {
    parent_refs
        .as_ref()
        .map(|refs| refs.iter().any(|pr| parent_ref_matches_gateway(pr, gateway_name, gateway_ns)))
        .unwrap_or(false)
}

/// Extract ParentRef configs for parentRefs targeting this gateway.
pub(crate) fn extract_parent_refs<T: ParentRefAccess>(
    parent_refs: &Option<Vec<T>>,
    gateway_name: &str,
) -> Vec<ParentRef> {
    parent_refs
        .as_ref()
        .map(|refs| {
            refs.iter()
                .filter(|pr| pr.ref_name() == gateway_name)
                .map(|pr| ParentRef {
                    name: pr.ref_name().to_string(),
                    section_name: pr.ref_section_name().map(String::from),
                    port: pr.ref_port(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Collect all sectionNames that target a specific gateway from parentRefs.
pub(crate) fn all_section_names_for_gateway<T: ParentRefAccess>(
    parent_refs: &Option<Vec<T>>,
    gateway_name: &str,
    gateway_ns: &str,
) -> Vec<Option<String>> {
    parent_refs.as_ref()
        .map(|refs| {
            refs.iter()
                .filter(|pr| parent_ref_matches_gateway(*pr, gateway_name, gateway_ns))
                .map(|pr| pr.ref_section_name().map(String::from))
                .collect()
        })
        .unwrap_or_else(|| vec![None])
}
