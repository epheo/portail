//! Hostname matching and intersection for Gateway API listener/route scoping.

/// Check if a listener hostname and route hostnames have at least one overlap.
/// Per Gateway API spec:
/// - No listener hostname → all routes match
/// - No route hostnames → matches any listener
/// - Wildcard: listener `*.example.com` matches route `foo.example.com`
pub(crate) fn hostnames_intersect(listener_hostname: Option<&str>, route_hostnames: &[String]) -> bool {
    let listener_hn = match listener_hostname {
        None => return true, // No listener hostname restriction
        Some(h) => h,
    };
    if route_hostnames.is_empty() {
        return true; // No route hostname restriction
    }
    route_hostnames.iter().any(|route_hn| {
        hostname_matches(listener_hn, route_hn) || hostname_matches(route_hn, listener_hn)
    })
}

/// Check if `pattern` matches `candidate`.
/// Supports wildcard prefixes: `*.example.com` matches `foo.example.com`.
pub(crate) fn hostname_matches(pattern: &str, candidate: &str) -> bool {
    if pattern == candidate {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix("*.") {
        // candidate must be in a subdomain of suffix
        if let Some(cand_rest) = candidate.strip_suffix(suffix) {
            // cand_rest must end with '.' and have at least one char before it
            // e.g. "foo." for "foo.example.com" matching "*.example.com"
            return cand_rest.ends_with('.') && cand_rest.len() > 1;
        }
        // Per spec, *.example.com does NOT match example.com itself, only subdomains
    }
    false
}
