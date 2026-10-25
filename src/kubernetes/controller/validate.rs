//! Gateway validation: TLS certificateRef resolution against the Secret
//! reflector store, and the listener/Gateway condition computation that
//! feeds status (Accepted / ResolvedRefs / Programmed reasons).
//!
//! Certificates are REALLY parsed here (`tls::validate_cert_key_pair` — the
//! same rustls path the data plane uses), so a Secret whose PEM cannot be
//! served never reaches `ResolvedRefs=True`. Resolution happens once; both
//! the config build and the status conditions consume its result.

use std::collections::HashMap;

use gateway_api::gateways::Gateway;
use gateway_api::referencegrants::ReferenceGrant;
use k8s_openapi::api::core::v1::Secret;
use kube::runtime::reflector::{ObjectRef, Store};

use crate::kubernetes::addresses::NETWORK_ADDRESS_TYPE;
use crate::kubernetes::converters::{secret_ref_key, CertData};
use crate::kubernetes::reconciler::{listener_conflicts, listener_is_served};
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

fn missing_secret(secret_ns: &str, name: &str) -> CertRefError {
    CertRefError::Invalid(format!(
        "Secret {}/{} not found or missing tls.crt/tls.key",
        secret_ns, name
    ))
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
        return Err(missing_secret(secret_ns, name));
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
        .ok_or_else(|| missing_secret(secret_ns, name))?;
    crate::proxy::tls::validate_cert_key_pair(&cert, &key)
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
        // validate_gateway reports the refs of another group or kind.
        for (cert_ref, key) in cert_refs
            .iter()
            .filter_map(|r| Some((r, secret_ref_key(r, gw_ns)?)))
        {
            let secret_ns = key.1.as_str();
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
    let conflicts = listener_conflicts(&gateway.spec.listeners);

    let supported_protocols = ["HTTP", "HTTPS", "TLS", "TCP", "UDP"];

    for listener in &gateway.spec.listeners {
        let mut ls = status::ListenerStatus::default();
        let mut refs_failed = false;

        if !supported_protocols.contains(&listener.protocol.as_str()) {
            ls.accepted = false;
            ls.accepted_reason = "UnsupportedProtocol".into();
            ls.accepted_message = format!("Protocol '{}' is not supported", listener.protocol);
        } else if !listener_is_served(listener) {
            // The reason GRPCRoute gives a listener whose protocol variant
            // the implementation cannot serve.
            ls.accepted = false;
            ls.accepted_reason = "UnsupportedProtocol".into();
            ls.accepted_message =
                "TLS listeners are served in Passthrough mode only; an HTTPS listener terminates"
                    .into();
        } else if let Some(&reason) = conflicts.get(listener.name.as_str()) {
            let message = if reason == "ProtocolConflict" {
                "Listener port shared with another listener using different protocol"
            } else {
                "Listener not distinct from another on its port"
            };
            ls.accepted = false;
            ls.accepted_reason = reason.into();
            ls.accepted_message = message.into();
            ls.conflicted = true;
            ls.conflicted_reason = reason.into();
            ls.conflicted_message = message.into();
        } else {
            ls.accepted = true;
            ls.accepted_reason = "Accepted".into();
            ls.accepted_message = "Listener accepted".into();
        }

        // Validate TLS certificateRefs for HTTPS/TLS listeners
        if let Some(tls) = &listener.tls {
            if let Some(cert_refs) = &tls.certificate_refs {
                for cert_ref in cert_refs {
                    let Some(key) = secret_ref_key(cert_ref, gw_ns) else {
                        refs_failed = true;
                        ls.resolved_refs_reason = "InvalidCertificateRef".into();
                        ls.resolved_refs_message = format!(
                            "Unsupported certificateRef group/kind: {}/{}",
                            cert_ref.group.as_deref().unwrap_or(""),
                            cert_ref.kind.as_deref().unwrap_or("Secret")
                        );
                        continue;
                    };

                    // Resolution (ReferenceGrant check + Secret lookup + real
                    // rustls parse) ran once in resolve_gateway_certs; the
                    // condition just reports its outcome.
                    if let Some(err) = certs.errors.get(&key) {
                        refs_failed = true;
                        ls.resolved_refs_reason = err.reason().into();
                        ls.resolved_refs_message = err.message().to_string();
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
                // No GRPCRoute: the data plane cannot serve gRPC, so advertising
                // it would claim attachment for routes that can never work.
                let valid_kinds = ["HTTPRoute", "TCPRoute", "TLSRoute", "UDPRoute"];
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

        // Reconcile turns this False again while the listener is not bound.
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

    // Gateway-level acceptance. An invalid listener stays a LISTENER
    // condition — its siblings keep serving and the Gateway stays Accepted
    // unless every listener is rejected.
    let mut cond =
        status::GatewayCondition::ok("Accepted", "Gateway accepted by portail controller");

    if let Some(addresses) = &gateway.spec.addresses {
        for addr in addresses {
            let addr_type = addr.r#type.as_deref().unwrap_or("IPAddress");
            match addr_type {
                "IPAddress" | "Hostname" => {}
                t if t == NETWORK_ADDRESS_TYPE => {}
                _ => {
                    cond = status::GatewayCondition {
                        ok: false,
                        reason: "UnsupportedAddress".to_string(),
                        message: format!(
                            "Unsupported address type '{}' in spec.addresses",
                            addr_type
                        ),
                    };
                    break;
                }
            }
        }
    }

    if cond.ok && !listener_statuses.is_empty() && listener_statuses.values().all(|ls| !ls.accepted)
    {
        cond = status::GatewayCondition {
            ok: false,
            reason: "ListenersNotValid".to_string(),
            message: "All listeners are rejected due to conflicts or errors".to_string(),
        };
    } else if cond.ok {
        // Gateway API, Listener: a Gateway with Conflicted listeners MUST
        // carry ListenersNotValid, accepted or not.
        let mut rejected: Vec<String> = gateway
            .spec
            .listeners
            .iter()
            .filter_map(|l| {
                let ls = listener_statuses.get(&l.name).filter(|ls| !ls.accepted)?;
                Some(format!("{} ({})", l.name, ls.accepted_reason))
            })
            .collect();
        if !rejected.is_empty() {
            rejected.sort();
            cond = status::GatewayCondition::ok(
                "ListenersNotValid",
                &format!(
                    "Listeners not accepted: {}; the others are",
                    rejected.join(", ")
                ),
            );
        }
    }

    (listener_statuses, cond)
}

#[cfg(test)]
mod tests {
    use super::super::testkit::{self, apply, Pipeline};
    use super::*;
    use crate::proxy::tls::test_util::generate_test_cert;
    use gateway_api::gateways::{
        GatewayListeners, GatewayListenersTls, GatewayListenersTlsCertificateRefs,
        GatewayListenersTlsMode, GatewaySpec,
    };
    use k8s_openapi::ByteString;
    use kube::core::ObjectMeta;
    use kube::runtime::reflector::store::Writer;
    use kube::runtime::watcher;
    use std::collections::BTreeMap;
    use std::sync::Arc;

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
    fn validate_gateway_grpcroute_kind_not_supported() {
        use gateway_api::gateways::{
            GatewayListenersAllowedRoutes, GatewayListenersAllowedRoutesKinds,
        };
        let mut gw = https_gateway("unused");
        gw.spec.listeners[0].tls = None;
        gw.spec.listeners[0].protocol = "HTTP".into();
        gw.spec.listeners[0].allowed_routes = Some(GatewayListenersAllowedRoutes {
            kinds: Some(vec![
                GatewayListenersAllowedRoutesKinds {
                    group: None,
                    kind: "HTTPRoute".into(),
                },
                GatewayListenersAllowedRoutesKinds {
                    group: None,
                    kind: "GRPCRoute".into(),
                },
            ]),
            namespaces: None,
        });
        let certs = ResolvedCerts {
            valid: CertData::new(),
            errors: HashMap::new(),
        };
        let (listeners, _) = validate_gateway(&gw, "default", &certs);
        let ls = &listeners["https"];
        assert!(!ls.resolved_refs);
        assert_eq!(ls.resolved_refs_reason, "InvalidRouteKinds");
        assert_eq!(ls.supported_kinds.len(), 1);
        assert_eq!(ls.supported_kinds[0]["kind"], "HTTPRoute");
    }

    /// Listener isolation: an unsupported-protocol listener gets its own
    /// UnsupportedProtocol condition; a valid sibling keeps the Gateway
    /// Accepted. All-invalid downgrades the Gateway with ListenersNotValid.
    #[test]
    fn validate_gateway_unsupported_protocol_isolated_per_listener() {
        let mut gw = https_gateway("unused");
        gw.spec.listeners[0].tls = None;
        gw.spec.listeners[0].protocol = "HTTP".into();
        gw.spec.listeners.push(GatewayListeners {
            name: "grpc".into(),
            port: 9090,
            protocol: "GRPC".into(),
            hostname: None,
            tls: None,
            allowed_routes: None,
        });
        let certs = ResolvedCerts {
            valid: CertData::new(),
            errors: HashMap::new(),
        };
        let (listeners, gw_cond) = validate_gateway(&gw, "default", &certs);
        let bad = &listeners["grpc"];
        assert!(!bad.accepted && !bad.programmed);
        assert_eq!(bad.accepted_reason, "UnsupportedProtocol");
        assert!(listeners["https"].accepted);
        assert!(gw_cond.ok, "valid sibling keeps the Gateway Accepted");

        gw.spec.listeners.remove(0);
        let (_, gw_cond) = validate_gateway(&gw, "default", &certs);
        assert!(!gw_cond.ok);
        assert_eq!(gw_cond.reason, "ListenersNotValid");
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

    fn listener(name: &str, port: i32, protocol: &str, hostname: Option<&str>) -> GatewayListeners {
        GatewayListeners {
            name: name.into(),
            port,
            protocol: protocol.into(),
            hostname: hostname.map(str::to_string),
            tls: None,
            allowed_routes: None,
        }
    }

    /// Listeners sharing a port and protocol must differ by hostname, and
    /// TCP and UDP have none: two on one port are both Conflicted. They
    /// used to be accepted, and only protocol mismatches were caught.
    #[test]
    fn same_protocol_listeners_on_one_port_conflict() {
        let mut gw = https_gateway("unused");
        gw.spec.listeners = vec![
            listener("tcp-a", 5432, "TCP", None),
            listener("tcp-b", 5432, "TCP", None),
            listener("dns-a", 53, "UDP", None),
            listener("dns-b", 53, "UDP", None),
            listener("web", 80, "HTTP", Some("a.example.com")),
            listener("web-dup", 80, "HTTP", Some("a.example.com")),
            listener("web-other", 80, "HTTP", Some("b.example.com")),
        ];
        let certs = ResolvedCerts {
            valid: CertData::new(),
            errors: HashMap::new(),
        };
        let (listeners, gw_cond) = validate_gateway(&gw, "default", &certs);
        for name in ["tcp-a", "tcp-b", "dns-a", "dns-b", "web", "web-dup"] {
            let ls = &listeners[name];
            assert!(ls.conflicted && !ls.accepted, "{name}");
            assert_eq!(ls.conflicted_reason, "HostnameConflict", "{name}");
        }
        assert!(listeners["web-other"].accepted, "distinct by hostname");

        // Some listeners rejected: still Accepted, but ListenersNotValid.
        assert!(gw_cond.ok);
        assert_eq!(gw_cond.reason, "ListenersNotValid");
        assert!(gw_cond.message.contains("tcp-a (HostnameConflict)"));
    }

    /// TCP and UDP listeners on one port are distinct by protocol and each
    /// binds its own socket: Gateway API v1.4.1 GatewaySpec.listeners,
    /// Compatible Listeners. They were ProtocolConflict, so DNS on 53 over
    /// both took neither. Two UDP listeners on a port still conflict.
    #[test]
    fn tcp_and_udp_listeners_share_a_port() {
        let mut gw = https_gateway("unused");
        gw.spec.listeners = vec![
            listener("dns-tcp", 53, "TCP", None),
            listener("dns-udp", 53, "UDP", None),
            listener("web", 8080, "HTTP", None),
            listener("quic", 8080, "UDP", None),
        ];
        let certs = ResolvedCerts {
            valid: CertData::new(),
            errors: HashMap::new(),
        };
        let (listeners, gw_cond) = validate_gateway(&gw, "default", &certs);
        for name in ["dns-tcp", "dns-udp", "web", "quic"] {
            let ls = &listeners[name];
            assert!(ls.accepted && !ls.conflicted && ls.programmed, "{name}");
        }
        assert!(gw_cond.ok);
        assert_eq!(gw_cond.reason, "Accepted");

        gw.spec
            .listeners
            .push(listener("dns-udp-b", 53, "UDP", None));
        let (listeners, _) = validate_gateway(&gw, "default", &certs);
        for name in ["dns-udp", "dns-udp-b"] {
            assert_eq!(
                listeners[name].conflicted_reason, "HostnameConflict",
                "{name}"
            );
        }
        assert!(
            listeners["dns-tcp"].accepted,
            "its UDP siblings' conflict is theirs"
        );
    }

    /// One socket serves HTTPS and TLS on a port, told apart by SNI, so
    /// they conflict only on a shared hostname. Both were ProtocolConflict,
    /// and nothing bound the port.
    #[test]
    fn https_and_tls_share_a_port_by_hostname() {
        let mut gw = https_gateway("unused");
        let passthrough = GatewayListeners {
            tls: Some(GatewayListenersTls {
                mode: Some(GatewayListenersTlsMode::Passthrough),
                ..Default::default()
            }),
            ..listener("db", 443, "TLS", Some("db.example.com"))
        };
        gw.spec.listeners[0].name = "web".into();
        gw.spec.listeners[0].hostname = Some("web.example.com".into());
        gw.spec.listeners.push(passthrough);
        let certs = ResolvedCerts {
            valid: CertData::new(),
            errors: HashMap::new(),
        };
        let (listeners, gw_cond) = validate_gateway(&gw, "default", &certs);
        for name in ["web", "db"] {
            let ls = &listeners[name];
            assert!(ls.accepted && !ls.conflicted, "{name}");
        }
        assert!(gw_cond.ok);
        assert_eq!(gw_cond.reason, "Accepted");

        gw.spec.listeners[1].hostname = Some("web.example.com".into());
        gw.spec.listeners.push(listener("plain", 443, "HTTP", None));
        let (listeners, _) = validate_gateway(&gw, "default", &certs);
        for name in ["web", "db", "plain"] {
            assert!(listeners[name].conflicted, "{name}");
        }
        assert_eq!(listeners["plain"].conflicted_reason, "ProtocolConflict");
    }

    /// The data plane serves a TLS listener in Passthrough mode only: a
    /// Terminate one was Accepted and Programmed, its TLSRoutes forwarded
    /// raw, and it shared its port with HTTPS as one family. It is rejected
    /// alone and takes no part in conflicts, like an unsupported TCP
    /// listener (GatewaySpec.listeners).
    #[test]
    fn a_tls_terminate_listener_is_rejected_alone() {
        let mut gw = https_gateway("unused");
        gw.spec.listeners[0].name = "web".into();
        gw.spec.listeners[0].hostname = Some("web.example.com".into());
        let terminate = GatewayListeners {
            tls: Some(GatewayListenersTls {
                mode: Some(GatewayListenersTlsMode::Terminate),
                certificate_refs: Some(vec![GatewayListenersTlsCertificateRefs {
                    name: "unused".into(),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..listener("db", 443, "TLS", Some("web.example.com"))
        };
        gw.spec.listeners.push(terminate);
        let certs = ResolvedCerts {
            valid: CertData::new(),
            errors: HashMap::new(),
        };
        let (listeners, gw_cond) = validate_gateway(&gw, "default", &certs);
        let db = &listeners["db"];
        assert!(!db.accepted && !db.programmed && !db.conflicted);
        assert_eq!(db.accepted_reason, "UnsupportedProtocol");
        assert!(
            db.accepted_message.contains("Passthrough"),
            "{}",
            db.accepted_message
        );
        let web = &listeners["web"];
        assert!(web.accepted && !web.conflicted);
        assert!(gw_cond.ok);
    }

    /// A Secret in the store as the apiserver holds it: with its UID.
    fn stored(writer: &mut Writer<Secret>, uid: &str, ns: &str, pair: &(Vec<u8>, Vec<u8>)) {
        let mut secret = tls_secret("cert", ns, &pair.0, &pair.1);
        secret.metadata.uid = Some(uid.into());
        writer.apply_watcher_event(&watcher::Event::Apply(secret));
    }

    /// `https_gateway` with its ref to Secret `cert` in `ns`.
    fn gateway_with_cert_in(ns: &str) -> Gateway {
        let mut gw = https_gateway("cert");
        let tls = gw.spec.listeners[0].tls.as_mut().unwrap();
        tls.certificate_refs.as_mut().unwrap()[0].namespace = Some(ns.into());
        gw
    }

    /// Gateways of `default` may reference Secrets of `certs`.
    fn secret_grant() -> ReferenceGrant {
        use gateway_api::referencegrants::{
            ReferenceGrantFrom, ReferenceGrantSpec, ReferenceGrantTo,
        };
        ReferenceGrant {
            metadata: ObjectMeta {
                name: Some("gateways-read-certs".into()),
                namespace: Some("certs".into()),
                ..Default::default()
            },
            spec: ReferenceGrantSpec {
                from: vec![ReferenceGrantFrom {
                    group: "gateway.networking.k8s.io".into(),
                    kind: "Gateway".into(),
                    namespace: "default".into(),
                }],
                to: vec![ReferenceGrantTo {
                    group: "".into(),
                    kind: "Secret".into(),
                    name: None,
                }],
            },
        }
    }

    fn cert_key(ns: &str) -> (String, String) {
        ("cert".into(), ns.into())
    }

    /// A revoked ReferenceGrant or a deleted Secret resolves no pair at
    /// all. The ref still reached the data plane, without PEM, and it took
    /// that unchanged set of refs for a bad rotation: it kept serving the
    /// cert.
    #[test]
    fn a_revoked_grant_or_a_deleted_secret_resolves_nothing() {
        let Some(good) = generate_test_cert("app.example.com") else {
            return; // openssl unavailable
        };
        let mut secrets = Writer::<Secret>::default();
        stored(&mut secrets, "uid-1", "certs", &good);
        let store = secrets.as_reader();
        let gw = gateway_with_cert_in("certs");
        let grants = [secret_grant()];
        let key = cert_key("certs");

        let resolved = resolve_gateway_certs(&gw, "default", &grants, &store);
        assert_eq!(resolved.valid.get(&key), Some(&good));

        let resolved = resolve_gateway_certs(&gw, "default", &[], &store);
        assert!(resolved.valid.is_empty(), "revoked grant");
        assert_eq!(resolved.errors[&key].reason(), "RefNotPermitted");

        let mut deleted = tls_secret("cert", "certs", &good.0, &good.1);
        deleted.metadata.uid = Some("uid-1".into());
        secrets.apply_watcher_event(&watcher::Event::Delete(deleted));
        let resolved = resolve_gateway_certs(&gw, "default", &grants, &store);
        assert!(resolved.valid.is_empty(), "deleted Secret");
        assert_eq!(resolved.errors[&key].reason(), "InvalidCertificateRef");
    }

    /// A certificateRef of another kind never resolves the Secret that
    /// shares its name: it was served, though reported unresolved.
    #[test]
    fn a_ref_of_another_kind_never_resolves_a_secret() {
        let Some(good) = generate_test_cert("app.example.com") else {
            return; // openssl unavailable
        };
        let mut secrets = Writer::<Secret>::default();
        stored(&mut secrets, "uid-1", "default", &good);
        let mut gw = https_gateway("cert");
        let tls = gw.spec.listeners[0].tls.as_mut().unwrap();
        tls.certificate_refs.as_mut().unwrap()[0].kind = Some("ConfigMap".into());

        let resolved = resolve_gateway_certs(&gw, "default", &[], &secrets.as_reader());
        assert!(resolved.valid.is_empty());
        let (listeners, _) = validate_gateway(&gw, "default", &resolved);
        assert!(!listeners["https"].resolved_refs);
        let config = crate::kubernetes::converters::convert_gateway(&gw, &resolved.valid).unwrap();
        let tls = config.listeners[0].tls.as_ref().unwrap();
        assert!(tls.certificate_refs.is_empty());
    }

    const SNI: &str = "app.example.com";

    /// Far past a loopback handshake: only a hung one reaches it.
    const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    /// The cert the data plane on `port` presents for [`SNI`], or why it
    /// presents none.
    async fn presented(port: u16) -> Result<Vec<u8>, String> {
        let config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(crate::proxy::backend_pool::NoVerifier))
            .with_no_client_auth();
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .map_err(|e| e.to_string())?;
        let name = rustls::pki_types::ServerName::try_from(SNI).unwrap();
        let handshake = tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp);
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake).await {
            Ok(Ok(tls)) => Ok(tls.get_ref().1.peer_certificates().unwrap()[0].to_vec()),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("handshake timed out".into()),
        }
    }

    fn der(pem: &[u8]) -> Vec<u8> {
        use rustls::pki_types::pem::PemObject;
        rustls::pki_types::CertificateDer::from_pem_slice(pem)
            .unwrap()
            .to_vec()
    }

    /// The pipeline's Gateway with its HTTPS listener on `port`, its cert
    /// Secret `cert` in `ns`.
    fn pipeline_gateway(port: u16, ns: &str) -> Gateway {
        let mut gw = testkit::gateway("gw", 1);
        gw.spec.listeners = gateway_with_cert_in(ns).spec.listeners;
        gw.spec.listeners[0].port = port.into();
        gw
    }

    /// The ResolvedRefs condition of the first listener in the last
    /// status write of the pipeline's Gateway.
    fn resolved_refs(p: &Pipeline) -> serde_json::Value {
        let patch = p.api.status_patch("gateways", "gw").unwrap();
        patch["status"]["listeners"][0]["conditions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["type"] == "ResolvedRefs")
            .unwrap()
            .clone()
    }

    /// A revoked ReferenceGrant refuses the listener's SNI in the pass
    /// that sees it. The data plane kept the cert, as for a bad rotation.
    #[tokio::test]
    async fn a_revoked_reference_grant_refuses_the_sni_at_once() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let Some(good) = generate_test_cert(SNI) else {
            return; // openssl unavailable
        };
        let port = crate::proxy::data_plane::test_ports::wildcard();
        let mut p = Pipeline::new();
        stored(&mut p.cluster.secrets, "uid-1", "certs", &good);
        apply(&mut p.cluster.reference_grants, secret_grant());
        let gw = pipeline_gateway(port, "certs");
        apply(&mut p.cluster.gateways, gw.clone());
        p.pass(&gw).await.unwrap();
        assert_eq!(presented(port).await, Ok(der(&good.0)));

        p.cluster
            .reference_grants
            .apply_watcher_event(&watcher::Event::Delete(secret_grant()));
        p.api.clear();
        p.pass(&gw).await.unwrap();
        let after = presented(port).await;
        assert!(after.is_err(), "served after the revoke: {after:?}");
        assert_eq!(resolved_refs(&p)["reason"], "RefNotPermitted");
    }
}
