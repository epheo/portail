//! The controller-to-data-plane wire: what a data plane asks for and
//! reports ([`SyncRequest`], [`InstanceReport`]) and what it is served
//! ([`Snapshot`]). Envelopes ignore unknown fields, so either side can add
//! one; the config inside a snapshot stays as strict as a config file.
//!
//! Imports `crate::config` only: nothing here may reach Kubernetes code,
//! which the data plane must run without (CI greps for it).

mod report;
mod snapshot;

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use report::{EndpointReport, InstanceReport, ListenerReport, SyncRequest, Transport};
pub use snapshot::{
    AppProtocol, Backends, CertificatePem, ExternalName, HeadlessTargetPort, NetworkCidrs, Snapshot,
};

/// Wire protocol revision. Additive changes keep it; a peer outside the
/// controller's supported range is answered 409.
pub const PROTOCOL: u32 = 1;

/// How long the controller holds a poll whose version is current: below
/// the 60s idle timeout of common L7 hops a held request may cross (nginx
/// `proxy_read_timeout`, the AWS ALB default). The one protocol constant:
/// the client request timeout and report liveness are twice it, the
/// TokenReview cache lives as long.
pub const HOLD: Duration = Duration::from_secs(30);

/// Set on a 503 from a replica that serves no snapshot: a standby, one
/// still syncing its caches, or one stopping. Another replica may serve,
/// so the data plane dials again at once instead of backing off as from an
/// overloaded controller.
pub const STANDBY_HEADER: &str = "x-portail-standby";

/// A Gateway's identity on the wire: what a data plane serves, what the
/// controller keys snapshots and reports by.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GatewayKey {
    pub namespace: String,
    pub name: String,
}

impl GatewayKey {
    pub fn new(namespace: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            name: name.into(),
        }
    }
}

impl fmt::Display for GatewayKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.namespace, self.name)
    }
}

/// `namespace/name`, both non-empty.
impl FromStr for GatewayKey {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.split_once('/') {
            Some((ns, name)) if !ns.is_empty() && !name.is_empty() && !name.contains('/') => {
                Ok(Self::new(ns, name))
            }
            _ => Err(format!("expected 'namespace/name', got {:?}", s)),
        }
    }
}

/// The Gateway a snapshot was translated from: its key, and the object
/// version it reflects (a recreated Gateway has a new uid).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayRef {
    pub namespace: String,
    pub name: String,
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub generation: i64,
}

impl GatewayRef {
    pub fn key(&self) -> GatewayKey {
        GatewayKey::new(&self.namespace, &self.name)
    }
}

/// RFC 1123 label length limit.
pub(crate) const MAX_DNS1123_LABEL: usize = 63;

/// RFC 1123 label, as Kubernetes names namespaces: lowercase alphanumerics
/// and '-', alphanumeric at both ends, at most 63 characters.
pub fn is_dns1123_label(s: &str) -> bool {
    let alnum = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    let bytes = s.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_DNS1123_LABEL
        && bytes.iter().all(|&c| alnum(c) || c == b'-')
        && alnum(bytes[0])
        && alnum(bytes[bytes.len() - 1])
}

/// SHA-256 of `data` in lowercase hex.
pub(crate) fn sha256_hex(data: &[u8]) -> String {
    use std::fmt::Write;
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, data);
    digest
        .as_ref()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_key_parses_namespace_slash_name() {
        let key: GatewayKey = "portail-system/public".parse().unwrap();
        assert_eq!(key, GatewayKey::new("portail-system", "public"));
        assert_eq!(key.to_string(), "portail-system/public");
        for bad in ["public", "/public", "ns/", "a/b/c", ""] {
            assert!(bad.parse::<GatewayKey>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn sha256_hex_matches_a_known_digest() {
        // printf abc | sha256sum
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
