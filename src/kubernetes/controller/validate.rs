//! Gateway validation: TLS certificateRef resolution against the Secret
//! reflector store, and the listener/Gateway condition computation that
//! feeds status (Accepted / ResolvedRefs / Programmed reasons).
//!
//! Certificates are REALLY parsed here (`tls::validate_cert_key_pair` — the
//! same rustls path the data plane uses), so a Secret whose PEM cannot be
//! served never reaches `ResolvedRefs=True`. Resolution happens once; both
//! the config build and the status conditions consume its result.

use std::collections::{HashMap, HashSet};

use gateway_api::gateways::Gateway;
use gateway_api::referencegrants::ReferenceGrant;
use k8s_openapi::api::core::v1::Secret;
use kube::runtime::reflector::{ObjectRef, Store};

use crate::kubernetes::addresses::NETWORK_ADDRESS_TYPE;
use crate::kubernetes::converters::CertData;
use crate::kubernetes::reference_grants::is_reference_allowed;
use crate::kubernetes::status;
use crate::logging::warn;

/// Why a certificateRef could not be resolved. Maps 1:1 to the listener
/// `ResolvedRefs` condition reason.
#[derive(Debug)]
pub(super) enum CertRefError {
    /// Cross-namespace ref without a ReferenceGrant → `RefNotPermitted`.
    NotPermitted(String),
    /// Missing Secret, missing tls.crt/tls.key, or PEM the real rustls
    /// parse/key-load rejects → `InvalidCertificateRef`.
    Invalid(String),
}

impl CertRefError {
    pub fn reason(&self) -> &'static str {
        match self {
            CertRefError::NotPermitted(_) => "RefNotPermitted",
            CertRefError::Invalid(_) => "InvalidCertificateRef",
        }
    }
    pub fn message(&self) -> &str {
        match self {
            CertRefError::NotPermitted(m) | CertRefError::Invalid(m) => m,
        }
    }
}

/// Outcome of resolving every certificateRef on a Gateway: PEM bytes for the
/// refs that validated, the first error for each ref that did not.
pub(super) struct ResolvedCerts {
    pub valid: CertData,
    pub errors: HashMap<(String, String), CertRefError>,
}

/// Resolve one certificateRef to its PEM bytes. Pure — the Secret lookup and
/// the ReferenceGrant decision happen at the caller; this validates what was
/// found, including the full rustls parse + key load.
pub(super) fn resolve_cert_ref(
    secret: Option<&Secret>,
    cross_ns_allowed: bool,
    secret_ns: &str,
    name: &str,
) -> Result<(Vec<u8>, Vec<u8>), CertRefError> {
    if !cross_ns_allowed {
        return Err(CertRefError::NotPermitted(format!(
            "Cross-namespace certificate ref {}/{} not allowed by ReferenceGrant",
            secret_ns, name
        )));
    }
    let Some(secret) = secret else {
        return Err(CertRefError::Invalid(format!(
            "Secret {}/{} not found or missing tls.crt/tls.key",
            secret_ns, name
        )));
    };
    let (cert, key) = secret
        .data
        .as_ref()
        .and_then(|data| {
            Some((
                data.get("tls.crt")?.0.clone(),
                data.get("tls.key")?.0.clone(),
            ))
        })
        .ok_or_else(|| {
            CertRefError::Invalid(format!(
                "Secret {}/{} not found or missing tls.crt/tls.key",
                secret_ns, name
            ))
        })?;
    crate::tls::validate_cert_key_pair(&cert, &key)
        .map_err(|e| CertRefError::Invalid(format!("Secret {}/{}: {}", secret_ns, name, e)))?;
    Ok((cert, key))
}

/// Resolve TLS certificates for a Gateway from the reflector Secret store.
/// Iterates all listeners, checks cross-namespace ReferenceGrant permissions,
/// looks up Secrets, and validates each pair with the real rustls parse.
pub(super) fn resolve_gateway_certs(
    gateway: &Gateway,
    gw_ns: &str,
    reference_grants: &[ReferenceGrant],
    store_secrets: &Store<Secret>,
) -> ResolvedCerts {
    let mut resolved = ResolvedCerts {
        valid: CertData::new(),
        errors: HashMap::new(),
    };
    for listener in &gateway.spec.listeners {
        let Some(tls) = &listener.tls else { continue };
        let Some(cert_refs) = &tls.certificate_refs else {
            continue;
        };
        for cert_ref in cert_refs {
            let secret_ns = cert_ref.namespace.as_deref().unwrap_or(gw_ns);
            let key = (cert_ref.name.clone(), secret_ns.to_string());
            if resolved.valid.contains_key(&key) || resolved.errors.contains_key(&key) {
                continue; // same Secret referenced by several listeners
            }

            let cross_ns_allowed = secret_ns == gw_ns
                || is_reference_allowed(
                    reference_grants,
                    "gateway.networking.k8s.io",
                    "Gateway",
                    gw_ns,
                    "",
                    "Secret",
                    secret_ns,
                    &cert_ref.name,
                );
            let secret_ref = ObjectRef::<Secret>::new(&cert_ref.name).within(secret_ns);
            let secret = store_secrets.get(&secret_ref);

            match resolve_cert_ref(
                secret.as_deref(),
                cross_ns_allowed,
                secret_ns,
                &cert_ref.name,
            ) {
                Ok(pair) => {
                    resolved.valid.insert(key, pair);
                }
                Err(e) => {
                    warn!(
                        "Certificate ref {}/{}: {}",
                        secret_ns,
                        cert_ref.name,
                        e.message()
                    );
                    resolved.errors.insert(key, e);
                }
            }
        }
    }
    resolved
}

/// Validate Gateway listeners and compute gateway-level acceptance.
pub(super) fn validate_gateway(
    gateway: &Gateway,
    gw_ns: &str,
    certs: &ResolvedCerts,
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
                    let key = (cert_ref.name.clone(), secret_ns.to_string());

                    // Resolution (ReferenceGrant check + Secret lookup + real
                    // rustls parse) ran once in resolve_gateway_certs; the
                    // condition just reports its outcome.
                    if let Some(err) = certs.errors.get(&key) {
                        refs_failed = true;
                        ls.resolved_refs_reason = err.reason().into();
                        ls.resolved_refs_message = err.message().to_string();
                    } else if !certs.valid.contains_key(&key) {
                        // Not resolved at all — e.g. the Secret watch is gated
                        // off by --watch-shape. Same condition as "not found".
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::test_util::generate_test_cert;
    use gateway_api::gateways::{
        GatewayListeners, GatewayListenersTls, GatewayListenersTlsCertificateRefs,
        GatewayListenersTlsMode, GatewaySpec,
    };
    use k8s_openapi::ByteString;
    use kube::core::ObjectMeta;
    use std::collections::BTreeMap;

    fn tls_secret(name: &str, ns: &str, cert: &[u8], key: &[u8]) -> Secret {
        Secret {
            metadata: ObjectMeta {
                name: Some(name.into()),
                namespace: Some(ns.into()),
                ..Default::default()
            },
            data: Some(BTreeMap::from([
                ("tls.crt".to_string(), ByteString(cert.to_vec())),
                ("tls.key".to_string(), ByteString(key.to_vec())),
            ])),
            ..Default::default()
        }
    }

    fn https_gateway(cert_name: &str) -> Gateway {
        Gateway {
            metadata: ObjectMeta {
                name: Some("gw".into()),
                namespace: Some("default".into()),
                ..Default::default()
            },
            spec: GatewaySpec {
                gateway_class_name: "portail".into(),
                listeners: vec![GatewayListeners {
                    name: "https".into(),
                    port: 443,
                    protocol: "HTTPS".into(),
                    hostname: None,
                    tls: Some(GatewayListenersTls {
                        mode: Some(GatewayListenersTlsMode::Terminate),
                        certificate_refs: Some(vec![GatewayListenersTlsCertificateRefs {
                            name: cert_name.into(),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    }),
                    allowed_routes: None,
                }],
                ..Default::default()
            },
            status: None,
        }
    }

    #[test]
    fn resolve_cert_ref_missing_secret_is_invalid() {
        let err = resolve_cert_ref(None, true, "default", "no-such").unwrap_err();
        assert_eq!(err.reason(), "InvalidCertificateRef");
        assert!(err.message().contains("not found"));
    }

    #[test]
    fn resolve_cert_ref_cross_ns_without_grant_is_not_permitted() {
        let err = resolve_cert_ref(None, false, "other", "cert").unwrap_err();
        assert_eq!(err.reason(), "RefNotPermitted");
    }

    #[test]
    fn resolve_cert_ref_missing_key_is_invalid() {
        let mut secret = tls_secret("cert", "default", b"x", b"y");
        secret.data.as_mut().unwrap().remove("tls.key");
        let err = resolve_cert_ref(Some(&secret), true, "default", "cert").unwrap_err();
        assert_eq!(err.reason(), "InvalidCertificateRef");
    }

    /// Regression for the closed status gap: PEM that passes the old
    /// substring check ("BEGIN CERTIFICATE" present) but fails the real
    /// rustls parse must be Invalid — it previously reached
    /// ResolvedRefs=True while the data plane silently skipped the cert.
    #[test]
    fn resolve_cert_ref_substring_passing_garbage_is_invalid() {
        let fake_cert = b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
        let fake_key = b"-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n";
        let secret = tls_secret("fake", "default", fake_cert, fake_key);
        let err = resolve_cert_ref(Some(&secret), true, "default", "fake").unwrap_err();
        assert_eq!(err.reason(), "InvalidCertificateRef");
    }

    #[test]
    fn resolve_cert_ref_valid_pair_ok() {
        let Some((cert, key)) = generate_test_cert("ok.example.com") else {
            return; // openssl unavailable
        };
        let secret = tls_secret("ok", "default", &cert, &key);
        let (c, k) = resolve_cert_ref(Some(&secret), true, "default", "ok").unwrap();
        assert_eq!((c, k), (cert, key));
    }

    #[test]
    fn validate_gateway_propagates_cert_error_into_resolved_refs() {
        let gw = https_gateway("bad-cert");
        let mut certs = ResolvedCerts {
            valid: CertData::new(),
            errors: HashMap::new(),
        };
        certs.errors.insert(
            ("bad-cert".into(), "default".into()),
            CertRefError::Invalid(
                "Secret default/bad-cert: Failed to parse certificate PEM".into(),
            ),
        );
        let (listeners, gw_cond) = validate_gateway(&gw, "default", &certs);
        let ls = &listeners["https"];
        assert!(!ls.resolved_refs);
        assert_eq!(ls.resolved_refs_reason, "InvalidCertificateRef");
        assert!(!ls.programmed, "unresolved refs must block Programmed");
        assert!(
            gw_cond.ok,
            "listener-level failure keeps the Gateway Accepted"
        );
    }

    #[test]
    fn validate_gateway_valid_cert_resolves() {
        let gw = https_gateway("good");
        let mut certs = ResolvedCerts {
            valid: CertData::new(),
            errors: HashMap::new(),
        };
        certs
            .valid
            .insert(("good".into(), "default".into()), (vec![1], vec![2]));
        let (listeners, _) = validate_gateway(&gw, "default", &certs);
        let ls = &listeners["https"];
        assert!(ls.resolved_refs && ls.programmed);
    }

    #[test]
    fn validate_gateway_https_without_tls_config() {
        let mut gw = https_gateway("unused");
        gw.spec.listeners[0].tls = None;
        let certs = ResolvedCerts {
            valid: CertData::new(),
            errors: HashMap::new(),
        };
        let (listeners, _) = validate_gateway(&gw, "default", &certs);
        let ls = &listeners["https"];
        assert!(!ls.resolved_refs);
        assert_eq!(ls.resolved_refs_reason, "InvalidCertificateRef");
    }

    #[test]
    fn validate_gateway_protocol_conflict() {
        let mut gw = https_gateway("unused");
        gw.spec.listeners[0].tls = None;
        gw.spec.listeners[0].protocol = "HTTP".into();
        gw.spec.listeners.push(GatewayListeners {
            name: "tcp".into(),
            port: 443,
            protocol: "TCP".into(),
            hostname: None,
            tls: None,
            allowed_routes: None,
        });
        let certs = ResolvedCerts {
            valid: CertData::new(),
            errors: HashMap::new(),
        };
        let (listeners, gw_cond) = validate_gateway(&gw, "default", &certs);
        assert!(listeners.values().all(|ls| ls.conflicted && !ls.accepted));
        assert!(
            !gw_cond.ok,
            "all listeners rejected -> Gateway not accepted"
        );
    }
}
