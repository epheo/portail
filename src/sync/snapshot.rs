//! The config a data plane serves, as the controller translated it, with
//! what the config file format keeps out of JSON: certificate PEMs and the
//! Service overrides the route table resolves backends by.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{sha256_hex, GatewayRef, PROTOCOL};
use crate::config::PortailConfig;

/// One Gateway's data-plane config. `version` is a content hash: equal
/// versions serve equal tables, so an unchanged translation is never
/// applied twice and a reconnect to another controller replica resumes.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub protocol: u32,
    #[serde(default)]
    pub version: String,
    pub gateway: GatewayRef,
    /// Listeners carry their bound `targetPort` and `rateLimit`, routes
    /// their attached parentRefs, backends `invalid`. No PEM, no override:
    /// those ride `certificates` and `backends`.
    pub config: PortailConfig,
    #[serde(default)]
    pub certificates: Vec<CertificatePem>,
    #[serde(default)]
    pub backends: Backends,
    /// CIDRs of the NetworkAttachmentDefinitions the Gateway's Network
    /// addresses name, read by the controller; the data plane matches its
    /// own interfaces against them. A name without an entry is unresolved.
    #[serde(default)]
    pub networks: Vec<NetworkCidrs>,
}

/// A certificateRef's resolved Secret. PEM is ASCII; [`Snapshot::from_config`]
/// stores it as text.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CertificatePem {
    pub namespace: String,
    pub name: String,
    pub cert_pem: String,
    pub key_pem: String,
}

/// Never the PEMs: snapshots get logged.
impl fmt::Debug for CertificatePem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CertificatePem")
            .field("namespace", &self.namespace)
            .field("name", &self.name)
            .field("cert_pem", &format_args!("<{} bytes>", self.cert_pem.len()))
            .field("key_pem", &"<redacted>")
            .finish()
    }
}

/// Service facts the route table resolves backends by, as sorted lists:
/// the config holds them in HashMaps, whose order would move the version.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Backends {
    #[serde(default)]
    pub headless_target_ports: Vec<HeadlessTargetPort>,
    #[serde(default)]
    pub external_names: Vec<ExternalName>,
    #[serde(default)]
    pub app_protocols: Vec<AppProtocol>,
}

/// A headless Service port's targetPort: DNS gives the pod IPs, not this.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadlessTargetPort {
    pub host: String,
    pub port: u16,
    pub target_port: u16,
}

/// What a backend name and port connect to instead, in order.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalName {
    pub host: String,
    pub port: u16,
    pub targets: Vec<(String, u16)>,
}

/// A Service port's appProtocol.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppProtocol {
    pub host: String,
    pub port: u16,
    pub app_protocol: String,
}

/// A network's subnets, by the name a Gateway's Network address uses.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkCidrs {
    pub name: String,
    pub cidrs: Vec<String>,
}

/// Hex digits of SHA-256 a version keeps: 64 bits make a collision between
/// two translations of one Gateway negligible, and a version short to log.
const VERSION_HEX_DIGITS: usize = 16;

impl Snapshot {
    /// The sealed snapshot of `config`: PEMs and overrides move out of its
    /// serde-skipped fields into the lists the wire carries.
    pub fn from_config(
        gateway: GatewayRef,
        mut config: PortailConfig,
        mut networks: Vec<NetworkCidrs>,
    ) -> Self {
        let mut certificates: Vec<CertificatePem> = Vec::new();
        for tls in config
            .gateway
            .listeners
            .iter_mut()
            .filter_map(|l| l.tls.as_mut())
        {
            for r in &mut tls.certificate_refs {
                let (Some(cert), Some(key)) = (r.cert_pem.take(), r.key_pem.take()) else {
                    continue;
                };
                let namespace = r.namespace.clone().unwrap_or_default();
                if certificates
                    .iter()
                    .any(|c| c.namespace == namespace && c.name == r.name)
                {
                    continue;
                }
                // Lossy only outside the PEM blocks: what is inside them is
                // base64, and the Secret passed a rustls parse.
                certificates.push(CertificatePem {
                    namespace,
                    name: r.name.clone(),
                    cert_pem: String::from_utf8_lossy(&cert).into_owned(),
                    key_pem: String::from_utf8_lossy(&key).into_owned(),
                });
            }
        }
        certificates.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));

        let mut backends = Backends {
            headless_target_ports: std::mem::take(&mut config.headless_target_ports)
                .into_iter()
                .map(|((host, port), target_port)| HeadlessTargetPort {
                    host,
                    port,
                    target_port,
                })
                .collect(),
            external_names: std::mem::take(&mut config.endpoint_overrides)
                .into_iter()
                .map(|((host, port), targets)| ExternalName {
                    host,
                    port,
                    targets,
                })
                .collect(),
            app_protocols: std::mem::take(&mut config.app_protocol_overrides)
                .into_iter()
                .map(|((host, port), app_protocol)| AppProtocol {
                    host,
                    port,
                    app_protocol,
                })
                .collect(),
        };
        backends.headless_target_ports.sort_unstable();
        backends.external_names.sort_unstable();
        backends.app_protocols.sort_unstable();
        networks.sort_unstable();

        Snapshot {
            protocol: PROTOCOL,
            version: String::new(),
            gateway,
            config,
            certificates,
            backends,
            networks,
        }
        .seal()
    }

    /// Stamp `version` from the content; call after any change to it.
    pub fn seal(mut self) -> Self {
        self.version = self.content_version();
        self
    }

    /// Whether `version` is this content's: a snapshot changed after its
    /// seal, or by a peer hashing otherwise, is refused.
    pub fn verify(&self) -> anyhow::Result<()> {
        if self.protocol != PROTOCOL {
            anyhow::bail!("snapshot protocol {} is not {}", self.protocol, PROTOCOL);
        }
        let content = self.content_version();
        if self.version != content {
            anyhow::bail!(
                "snapshot version {} does not match its content ({})",
                self.version,
                content
            );
        }
        Ok(())
    }

    /// The config a route table builds from: PEMs and overrides back in
    /// their fields.
    pub fn to_config(&self) -> PortailConfig {
        let mut config = self.config.clone();
        for tls in config
            .gateway
            .listeners
            .iter_mut()
            .filter_map(|l| l.tls.as_mut())
        {
            for r in &mut tls.certificate_refs {
                let namespace = r.namespace.as_deref().unwrap_or_default();
                if let Some(pem) = self
                    .certificates
                    .iter()
                    .find(|c| c.namespace == namespace && c.name == r.name)
                {
                    r.cert_pem = Some(pem.cert_pem.as_bytes().to_vec());
                    r.key_pem = Some(pem.key_pem.as_bytes().to_vec());
                }
            }
        }
        config.headless_target_ports = self
            .backends
            .headless_target_ports
            .iter()
            .map(|h| ((h.host.clone(), h.port), h.target_port))
            .collect();
        config.endpoint_overrides = self
            .backends
            .external_names
            .iter()
            .map(|e| ((e.host.clone(), e.port), e.targets.clone()))
            .collect();
        config.app_protocol_overrides = self
            .backends
            .app_protocols
            .iter()
            .map(|a| ((a.host.clone(), a.port), a.app_protocol.clone()))
            .collect();
        config
    }

    /// The first [`VERSION_HEX_DIGITS`] of SHA-256 over the canonical JSON
    /// of everything but `version`. Canonical: struct fields serialize in
    /// declaration order, and every list is sorted or ordered by the spec.
    fn content_version(&self) -> String {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Content<'a> {
            protocol: u32,
            gateway: &'a GatewayRef,
            config: &'a PortailConfig,
            certificates: &'a [CertificatePem],
            backends: &'a Backends,
            networks: &'a [NetworkCidrs],
        }
        let json = serde_json::to_vec(&Content {
            protocol: self.protocol,
            gateway: &self.gateway,
            config: &self.config,
            certificates: &self.certificates,
            backends: &self.backends,
            networks: &self.networks,
        })
        // No map with non-string keys and no fallible serializer anywhere
        // in these types.
        .expect("a snapshot serializes");
        let mut hex = sha256_hex(&json);
        hex.truncate(VERSION_HEX_DIGITS);
        hex
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CertificateRef, TlsConfig, TlsMode};
    use std::collections::HashMap;

    fn gateway() -> GatewayRef {
        GatewayRef {
            namespace: "portail-system".into(),
            name: "public".into(),
            uid: "uid-1".into(),
            generation: 7,
        }
    }

    /// An HTTPS listener with its PEMs, two backends of which one is
    /// refused, and one entry in each override map.
    fn translated() -> PortailConfig {
        let mut config: PortailConfig = serde_json::from_value(serde_json::json!({
            "gateway": {"name": "public", "listeners": [{
                "name": "https", "protocol": "HTTPS", "port": 443, "target_port": 8443,
                "rate_limit": {"requestsPerSecond": 5},
            }]},
            "httpRoutes": [{
                "parentRefs": [{"name": "public", "sectionName": "https"}],
                "hostnames": ["a.example"],
                "rules": [{
                    "backendRefs": [
                        {"name": "web.default.svc", "port": 80},
                        {"name": "db.other.svc", "port": 80, "invalid": true},
                    ],
                    "timeouts": {"request": "90s"},
                }],
            }],
        }))
        .unwrap();
        config.gateway.listeners[0].tls = Some(TlsConfig {
            mode: TlsMode::Terminate,
            certificate_refs: vec![CertificateRef {
                name: "tls-a".into(),
                namespace: Some("certs".into()),
                hostname: None,
                cert_pem: Some(b"-----BEGIN CERTIFICATE-----\nAAAA\n".to_vec()),
                key_pem: Some(b"-----BEGIN PRIVATE KEY-----\nSECRET\n".to_vec()),
            }],
        });
        config
            .headless_target_ports
            .insert(("db.prod.svc".into(), 5432), 5433);
        config.endpoint_overrides.insert(
            ("ext.default.svc".into(), 443),
            vec![("api.example.com".into(), 443)],
        );
        config
            .app_protocol_overrides
            .insert(("web.default.svc".into(), 80), "https".into());
        config
    }

    fn over_the_wire(snapshot: &Snapshot) -> Snapshot {
        serde_json::from_str(&serde_json::to_string(snapshot).unwrap()).unwrap()
    }

    #[test]
    fn a_snapshot_round_trips_with_its_pems_and_overrides() {
        let config = translated();
        let snapshot = Snapshot::from_config(gateway(), config.clone(), vec![]);
        let received = over_the_wire(&snapshot);
        received.verify().unwrap();
        assert_eq!(received.version, snapshot.version);

        let restored = received.to_config();
        let cert = &restored.gateway.listeners[0]
            .tls
            .as_ref()
            .unwrap()
            .certificate_refs[0];
        let original = &config.gateway.listeners[0]
            .tls
            .as_ref()
            .unwrap()
            .certificate_refs[0];
        assert_eq!(cert.cert_pem, original.cert_pem);
        assert_eq!(cert.key_pem, original.key_pem);
        assert_eq!(restored.headless_target_ports, config.headless_target_ports);
        assert_eq!(restored.endpoint_overrides, config.endpoint_overrides);
        assert_eq!(
            restored.app_protocol_overrides,
            config.app_protocol_overrides
        );
        assert_eq!(
            serde_json::to_value(&restored).unwrap(),
            serde_json::to_value(&config).unwrap()
        );
    }

    /// Every field of the config schema the examples use survives the wire.
    #[test]
    fn the_examples_round_trip() {
        for (name, yaml, _) in crate::config::examples::EXAMPLES {
            let config: PortailConfig = serde_yaml::from_str(yaml).unwrap();
            let received = over_the_wire(&Snapshot::from_config(gateway(), config.clone(), vec![]));
            received.verify().unwrap();
            assert_eq!(
                serde_json::to_value(received.to_config()).unwrap(),
                serde_json::to_value(&config).unwrap(),
                "{name}"
            );
        }
    }

    #[test]
    fn invalid_backends_and_cert_namespaces_survive_the_wire() {
        let received = over_the_wire(&Snapshot::from_config(gateway(), translated(), vec![]));
        let config = received.to_config();
        let refs = &config.http_routes[0].rules[0].backend_refs;
        assert!(!refs[0].invalid && refs[1].invalid);
        let cert = &config.gateway.listeners[0]
            .tls
            .as_ref()
            .unwrap()
            .certificate_refs[0];
        assert_eq!(cert.namespace.as_deref(), Some("certs"));
        assert!(cert.cert_pem.is_some(), "PEMs found by namespace and name");
    }

    #[test]
    fn keys_stay_out_of_debug_and_the_config_json() {
        let snapshot = Snapshot::from_config(gateway(), translated(), vec![]);
        assert!(!format!("{snapshot:?}").contains("SECRET"));
        assert!(!serde_json::to_string(&snapshot.config)
            .unwrap()
            .contains("SECRET"));
        assert_eq!(snapshot.certificates.len(), 1);
    }

    /// Two configs equal but for the insertion order of their maps.
    #[test]
    fn the_version_ignores_map_order() {
        let entries: Vec<((String, u16), u16)> = (0..32)
            .map(|i| ((format!("svc-{i}.default.svc"), 80), 8000 + i))
            .collect();
        let mut forward = translated();
        let mut backward = translated();
        forward.headless_target_ports = entries.iter().cloned().collect::<HashMap<_, _>>();
        backward.headless_target_ports = entries.iter().rev().cloned().collect::<HashMap<_, _>>();
        let networks = |names: &[&str]| -> Vec<NetworkCidrs> {
            names
                .iter()
                .map(|n| NetworkCidrs {
                    name: n.to_string(),
                    cidrs: vec!["10.10.1.0/24".into()],
                })
                .collect()
        };
        assert_eq!(
            Snapshot::from_config(gateway(), forward, networks(&["a", "b"])).version,
            Snapshot::from_config(gateway(), backward, networks(&["b", "a"])).version
        );
    }

    #[test]
    fn the_version_moves_with_the_content() {
        let base = Snapshot::from_config(gateway(), translated(), vec![]);
        assert_eq!(base.version.len(), VERSION_HEX_DIGITS);

        let mut rotated = translated();
        rotated.gateway.listeners[0]
            .tls
            .as_mut()
            .unwrap()
            .certificate_refs[0]
            .cert_pem = Some(b"-----BEGIN CERTIFICATE-----\nBBBB\n".to_vec());
        assert_ne!(
            Snapshot::from_config(gateway(), rotated, vec![]).version,
            base.version
        );

        let bumped = GatewayRef {
            generation: 8,
            ..gateway()
        };
        assert_ne!(
            Snapshot::from_config(bumped, translated(), vec![]).version,
            base.version
        );
    }

    #[test]
    fn a_changed_snapshot_fails_verification() {
        let mut snapshot = Snapshot::from_config(gateway(), translated(), vec![]);
        snapshot.config.http_routes.clear();
        assert!(snapshot.verify().is_err());
        assert!(snapshot.seal().verify().is_ok());
    }

    /// Envelopes take fields a newer peer adds; the config does not, so a
    /// data plane that cannot read a newer config keeps its table.
    #[test]
    fn unknown_fields_pass_the_envelope_not_the_config() {
        let mut json =
            serde_json::to_value(Snapshot::from_config(gateway(), translated(), vec![])).unwrap();
        json["runtime"] = serde_json::json!({"workers": 4});
        json["gateway"]["labels"] = serde_json::json!({});
        let received: Snapshot = serde_json::from_value(json.clone()).unwrap();
        received.verify().unwrap();

        json["config"]["gateway"]["shards"] = serde_json::json!(2);
        assert!(serde_json::from_value::<Snapshot>(json).is_err());
    }
}
