//! Gateway validation: TLS certificateRef resolution against the Secret
//! reflector store, and the listener/Gateway condition computation that
//! feeds status (Accepted / ResolvedRefs / Programmed reasons).

use std::collections::{HashMap, HashSet};

use gateway_api::gateways::Gateway;
use gateway_api::referencegrants::ReferenceGrant;
use k8s_openapi::api::core::v1::Secret;
use kube::runtime::reflector::{ObjectRef, Store};

use crate::kubernetes::addresses::NETWORK_ADDRESS_TYPE;
use crate::kubernetes::reference_grants::is_reference_allowed;
use crate::kubernetes::status;
use crate::logging::{debug, warn};

/// Resolve TLS certificates for a Gateway from the reflector Secret store.
/// Iterates all listeners, checks cross-namespace ReferenceGrant permissions,
/// looks up Secrets, and validates PEM format.
pub(super) fn resolve_gateway_certs(
    gateway: &Gateway,
    gw_ns: &str,
    reference_grants: &[ReferenceGrant],
    store_secrets: &Store<Secret>,
) -> HashMap<(String, String), (Vec<u8>, Vec<u8>)> {
    let mut cert_data: HashMap<(String, String), (Vec<u8>, Vec<u8>)> = HashMap::new();
    for listener in &gateway.spec.listeners {
        let Some(tls) = &listener.tls else { continue };
        let Some(cert_refs) = &tls.certificate_refs else {
            continue;
        };
        for cert_ref in cert_refs {
            let secret_ns = cert_ref.namespace.as_deref().unwrap_or(gw_ns);

            // Cross-namespace cert ref requires a ReferenceGrant
            if secret_ns != gw_ns
                && !is_reference_allowed(
                    reference_grants,
                    "gateway.networking.k8s.io",
                    "Gateway",
                    gw_ns,
                    "",
                    "Secret",
                    secret_ns,
                    &cert_ref.name,
                )
            {
                warn!(
                    "Cross-namespace certificate ref {}/{} not allowed by ReferenceGrant",
                    secret_ns, cert_ref.name
                );
                continue;
            }

            // Look up secret from reflector cache
            let secret_ref = ObjectRef::<Secret>::new(&cert_ref.name).within(secret_ns);
            match store_secrets.get(&secret_ref) {
                Some(secret) => {
                    if let Some(data) = &secret.data {
                        let cert_pem = data.get("tls.crt").map(|b| b.0.clone());
                        let key_pem = data.get("tls.key").map(|b| b.0.clone());
                        if let (Some(cert), Some(key)) = (cert_pem, key_pem) {
                            let cert_str = String::from_utf8_lossy(&cert);
                            let key_str = String::from_utf8_lossy(&key);
                            if cert_str.contains("BEGIN CERTIFICATE")
                                && (key_str.contains("BEGIN PRIVATE KEY")
                                    || key_str.contains("BEGIN RSA PRIVATE KEY")
                                    || key_str.contains("BEGIN EC PRIVATE KEY"))
                            {
                                cert_data.insert(
                                    (cert_ref.name.clone(), secret_ns.to_string()),
                                    (cert, key),
                                );
                            } else {
                                warn!(
                                    "Secret {}/{} has malformed PEM data",
                                    secret_ns, cert_ref.name
                                );
                            }
                        } else {
                            warn!(
                                "Secret {}/{} missing tls.crt or tls.key",
                                secret_ns, cert_ref.name
                            );
                        }
                    }
                }
                None => {
                    debug!("Secret {}/{} not found in cache", secret_ns, cert_ref.name);
                }
            }
        }
    }
    cert_data
}

/// Validate Gateway listeners and compute gateway-level acceptance.
pub(super) fn validate_gateway(
    gateway: &Gateway,
    gw_ns: &str,
    cert_data: &HashMap<(String, String), (Vec<u8>, Vec<u8>)>,
    reference_grants: &[ReferenceGrant],
) -> (
    HashMap<String, status::ListenerStatus>,
    status::GatewayCondition,
) {
    let mut listener_statuses: HashMap<String, status::ListenerStatus> = HashMap::new();

    // Detect protocol conflicts: same port, different protocols
    let mut port_protocols: HashMap<i32, String> = HashMap::new();
    let mut conflicted_ports: HashSet<i32> = HashSet::new();
    for l in &gateway.spec.listeners {
        match port_protocols.get(&l.port) {
            Some(existing_proto) if *existing_proto != l.protocol => {
                conflicted_ports.insert(l.port);
            }
            None => {
                port_protocols.insert(l.port, l.protocol.clone());
            }
            _ => {}
        }
    }

    for listener in &gateway.spec.listeners {
        let mut ls = status::ListenerStatus::default();
        let mut refs_failed = false;

        if conflicted_ports.contains(&listener.port) {
            ls.accepted = false;
            ls.accepted_reason = "ProtocolConflict".into();
            ls.accepted_message =
                "Listener port conflicts with another listener using a different protocol".into();
            ls.conflicted = true;
            ls.conflicted_reason = "ProtocolConflict".into();
            ls.conflicted_message =
                "Listener port shared with another listener using different protocol".into();
        } else {
            ls.accepted = true;
            ls.accepted_reason = "Accepted".into();
            ls.accepted_message = "Listener accepted".into();
        }

        // Validate TLS certificateRefs for HTTPS/TLS listeners
        if let Some(tls) = &listener.tls {
            if let Some(cert_refs) = &tls.certificate_refs {
                for cert_ref in cert_refs {
                    let group = cert_ref.group.as_deref().unwrap_or("");
                    let kind_str = cert_ref.kind.as_deref().unwrap_or("Secret");
                    if !group.is_empty() || kind_str != "Secret" {
                        refs_failed = true;
                        ls.resolved_refs_reason = "InvalidCertificateRef".into();
                        ls.resolved_refs_message = format!(
                            "Unsupported certificateRef group/kind: {}/{}",
                            group, kind_str
                        );
                        continue;
                    }

                    let secret_ns = cert_ref.namespace.as_deref().unwrap_or(gw_ns);

                    // Cross-namespace cert ref requires ReferenceGrant
                    if secret_ns != gw_ns
                        && !is_reference_allowed(
                            reference_grants,
                            "gateway.networking.k8s.io",
                            "Gateway",
                            gw_ns,
                            "",
                            "Secret",
                            secret_ns,
                            &cert_ref.name,
                        )
                    {
                        refs_failed = true;
                        ls.resolved_refs_reason = "RefNotPermitted".into();
                        ls.resolved_refs_message = format!(
                            "Cross-namespace certificate ref {}/{} not allowed by ReferenceGrant",
                            secret_ns, cert_ref.name
                        );
                        continue;
                    }

                    if !cert_data.contains_key(&(cert_ref.name.clone(), secret_ns.to_string())) {
                        refs_failed = true;
                        ls.resolved_refs_reason = "InvalidCertificateRef".into();
                        ls.resolved_refs_message = format!(
                            "Secret {}/{} not found or missing tls.crt/tls.key",
                            secret_ns, cert_ref.name
                        );
                    }
                }
            }
        } else if matches!(listener.protocol.as_str(), "HTTPS" | "TLS") {
            refs_failed = true;
            ls.resolved_refs_reason = "InvalidCertificateRef".into();
            ls.resolved_refs_message = "HTTPS/TLS listener requires TLS configuration".into();
        }

        // Check allowedRoutes.kinds validity and compute supportedKinds
        if let Some(allowed_routes) = &listener.allowed_routes {
            if let Some(kinds) = &allowed_routes.kinds {
                let valid_kinds = ["HTTPRoute", "TCPRoute", "TLSRoute", "UDPRoute", "GRPCRoute"];
                let mut supported = Vec::new();
                let mut has_invalid = false;
                for k in kinds {
                    let group = k.group.as_deref().unwrap_or("gateway.networking.k8s.io");
                    let kind_str = &k.kind;
                    if group == "gateway.networking.k8s.io"
                        && valid_kinds.contains(&kind_str.as_str())
                    {
                        supported.push(serde_json::json!({
                            "group": "gateway.networking.k8s.io",
                            "kind": kind_str,
                        }));
                    } else {
                        has_invalid = true;
                    }
                }
                ls.supported_kinds = supported;
                if has_invalid {
                    refs_failed = true;
                    ls.resolved_refs_reason = "InvalidRouteKinds".into();
                    ls.resolved_refs_message =
                        "One or more route kinds in allowedRoutes are not supported".into();
                }
            } else {
                ls.supported_kinds = status::supported_kinds_for_protocol(&listener.protocol);
            }
        } else {
            ls.supported_kinds = status::supported_kinds_for_protocol(&listener.protocol);
        }

        if !refs_failed {
            ls.resolved_refs = true;
            ls.resolved_refs_reason = "ResolvedRefs".into();
            ls.resolved_refs_message = "All references resolved".into();
        }

        if ls.accepted && ls.resolved_refs {
            ls.programmed = true;
            ls.programmed_reason = "Programmed".into();
            ls.programmed_message = "Programmed".into();
        } else {
            ls.programmed = false;
            ls.programmed_reason = "Invalid".into();
            ls.programmed_message = if !ls.accepted {
                "Listener not programmed due to acceptance failure".into()
            } else {
                "Listener has unresolved references".into()
            };
        }

        listener_statuses.insert(listener.name.clone(), ls);
    }

    // Gateway-level acceptance
    let supported_protocols = ["HTTP", "HTTPS", "TLS", "TCP", "UDP"];
    let mut accepted = true;
    let mut reason = "Accepted".to_string();
    let mut message = "Gateway accepted by portail controller".to_string();

    for listener in &gateway.spec.listeners {
        if !supported_protocols.contains(&listener.protocol.as_str()) {
            accepted = false;
            reason = "InvalidParameters".to_string();
            message = format!(
                "Listener '{}' uses unsupported protocol '{}'",
                listener.name, listener.protocol
            );
            break;
        }
    }

    if accepted {
        if let Some(addresses) = &gateway.spec.addresses {
            for addr in addresses {
                let addr_type = addr.r#type.as_deref().unwrap_or("IPAddress");
                match addr_type {
                    "IPAddress" | "Hostname" => {}
                    t if t == NETWORK_ADDRESS_TYPE => {}
                    _ => {
                        accepted = false;
                        reason = "UnsupportedAddress".to_string();
                        message =
                            format!("Unsupported address type '{}' in spec.addresses", addr_type);
                        break;
                    }
                }
            }
        }
    }

    if accepted && !listener_statuses.is_empty() {
        let all_rejected = listener_statuses.values().all(|ls| !ls.accepted);
        if all_rejected {
            accepted = false;
            reason = "InvalidListeners".to_string();
            message = "All listeners are rejected due to conflicts or errors".to_string();
        }
    }

    (
        listener_statuses,
        status::GatewayCondition {
            ok: accepted,
            reason,
            message,
        },
    )
}
