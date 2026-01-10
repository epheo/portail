/// Canonical list of Gateway API features supported by this controller.
/// Used both in GatewayClass status reporting and conformance test configuration.
/// Keep sorted alphabetically for easy diffing.
pub const SUPPORTED_FEATURES: &[&str] = &[
    "Gateway",
    "GatewayPort8080",
    "HTTPRoute",
    "HTTPRouteBackendRequestHeaderModification",
    "HTTPRouteBackendTimeout",
    "HTTPRouteDestinationPortMatching",
    "HTTPRouteHostRewrite",
    "HTTPRouteMethodMatching",
    "HTTPRouteParentRefPort",
    "HTTPRoutePathRedirect",
    "HTTPRoutePathRewrite",
    "HTTPRoutePortRedirect",
    "HTTPRouteQueryParamMatching",
    "HTTPRouteRequestHeaderModification",
    "HTTPRouteRequestMirror",
    "HTTPRouteRequestMultipleMirrors",
    "HTTPRouteRequestTimeout",
    "HTTPRouteResponseHeaderModification",
    "HTTPRouteSchemeRedirect",
    "ReferenceGrant",
    "TLSRoute",
    "UDPRoute",
];
