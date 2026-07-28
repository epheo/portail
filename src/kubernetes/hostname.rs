//! Hostname intersection for Gateway API listener/route scoping.

use crate::routing::hostnames_intersect as patterns_intersect;

/// Check if a listener hostname and route hostnames have at least one overlap.
/// CRD validation guarantees lowercase hostnames, so no normalization here.
/// - No listener hostname: all routes match
/// - No route hostnames: matches any listener
pub(crate) fn hostnames_intersect(
    listener_hostname: Option<&str>,
    route_hostnames: &[String],
) -> bool {
    let listener_hn = match listener_hostname {
        None => return true,
        Some(h) => h,
    };
    if route_hostnames.is_empty() {
        return true;
    }
    route_hostnames
        .iter()
        .any(|route_hn| patterns_intersect(listener_hn, route_hn))
}
