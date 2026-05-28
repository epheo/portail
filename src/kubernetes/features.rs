/// Canonical list of Gateway API features supported by this controller.
/// Used both in GatewayClass status reporting and conformance test configuration.
/// Keep sorted alphabetically for easy diffing.
pub const SUPPORTED_FEATURES: &[&str] = &[
    "Gateway",
    "GatewayAddressEmpty",
    "GatewayHTTPListenerIsolation",
    "GatewayPort8080",
    // GatewayStaticAddresses is deferred under the operator model: honoring a
    // specific spec.addresses requires the operator to drive the per-Gateway
    // LoadBalancer Service's IP assignment (e.g. MetalLB) and reflect
    // assignable-vs-unassignable addresses in Programmed status. Re-add once
    // the operator implements that. (Was supported in the single-binary model
    // via in-pod address binding.)
    "HTTPRoute",
    "HTTPRouteBackendProtocolWebSocket",
    "HTTPRouteBackendRequestHeaderModification",
    "HTTPRouteBackendTimeout",
    "HTTPRouteDestinationPortMatching",
    "HTTPRouteHostRewrite",
    "HTTPRouteMethodMatching",
    "HTTPRouteNamedRouteRule",
    "HTTPRouteParentRefPort",
    "HTTPRoutePathRedirect",
    "HTTPRoutePathRewrite",
    "HTTPRoutePortRedirect",
    "HTTPRouteQueryParamMatching",
    "HTTPRouteRequestHeaderModification",
    "HTTPRouteRequestMirror",
    "HTTPRouteRequestMultipleMirrors",
    "HTTPRouteRequestPercentageMirror",
    "HTTPRouteRequestTimeout",
    "HTTPRouteResponseHeaderModification",
    "HTTPRouteSchemeRedirect",
    "ReferenceGrant",
    "TLSRoute",
    "UDPRoute",
];
