//! Names and label values derived from a Gateway: provisioning creates
//! objects under them and sync authorization expects the ServiceAccount
//! they name, so both must compute the same bytes.

use crate::sync::sha256_hex;

/// `app.kubernetes.io/name` on everything provisioned.
pub const LABEL_NAME: &str = "app.kubernetes.io/name";
pub const NAME: &str = "portail";
/// `app.kubernetes.io/managed-by`: what the owned-object reflectors select.
pub const LABEL_MANAGED_BY: &str = "app.kubernetes.io/managed-by";
pub const MANAGED_BY: &str = "portail-controller";
/// The Gateway a data-plane object belongs to, valued by [`label_value`];
/// the Deployment's selector.
pub const LABEL_GATEWAY: &str = "portail.epheo.eu/gateway";
/// The Gateway API convention for the same, valued by [`label_value`].
pub const LABEL_GATEWAY_NAME: &str = "gateway.networking.k8s.io/gateway-name";

/// The one field manager of every apply and status write.
pub const FIELD_MANAGER: &str = "portail-controller";

const PREFIX: &str = "portail-";
/// Hex digits of SHA-256 a shortened name or value ends with (operator
/// parity: resources.go `hashLen`).
const HASH_HEX_DIGITS: usize = 8;
/// DNS-1123 label and label-value length limit.
const MAX_LABEL: usize = 63;

/// The name of a Gateway's ServiceAccount, Deployment, Service and PDB:
/// `portail-<gateway>`, or, when that is no DNS-1123 label, the Gateway name
/// cut to fit with a hash of the whole name appended. Same bytes as the
/// operator's `resourceName`, so existing objects keep their names. `None`
/// for a name no cut can make a label (one with a dot), as the operator
/// refused it.
pub fn resource_name(gateway: &str) -> Option<String> {
    let name = format!("{PREFIX}{gateway}");
    if is_dns1123_label(&name) {
        return Some(name);
    }
    let max_cut = MAX_LABEL - PREFIX.len() - 1 - HASH_HEX_DIGITS;
    let cut = gateway
        .get(..max_cut)
        .unwrap_or(gateway)
        .trim_end_matches('-');
    let hash = &sha256_hex(gateway.as_bytes())[..HASH_HEX_DIGITS];
    let name = format!("{PREFIX}{cut}-{hash}");
    is_dns1123_label(&name).then_some(name)
}

/// A Gateway name as a label value: as it is up to 63 characters, else
/// its first 54 with a hash of the whole name appended. A Gateway name is a
/// DNS-1123 subdomain, so either form is a valid label value.
pub fn label_value(gateway: &str) -> String {
    if gateway.len() <= MAX_LABEL {
        return gateway.to_string();
    }
    let keep = MAX_LABEL - 1 - HASH_HEX_DIGITS;
    let hash = &sha256_hex(gateway.as_bytes())[..HASH_HEX_DIGITS];
    format!("{}-{hash}", gateway.get(..keep).unwrap_or(gateway))
}

/// The audience of the tokens data planes present: the controller
/// Service's in-cluster name, which the apiserver refuses as its own.
pub fn controller_audience(service: &str, namespace: &str) -> String {
    format!("{service}.{namespace}.svc")
}

/// RFC 1123 label: lowercase alphanumerics and '-', alphanumeric at both
/// ends, at most 63 characters.
fn is_dns1123_label(s: &str) -> bool {
    let alnum = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    let bytes = s.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_LABEL
        && bytes.iter().all(|&c| alnum(c) || c == b'-')
        && alnum(bytes[0])
        && alnum(bytes[bytes.len() - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_name_is_prefixed() {
        assert_eq!(resource_name("test-gw").as_deref(), Some("portail-test-gw"));
        // 8 + 55 = 63: still fits.
        let fits = "a".repeat(55);
        assert_eq!(resource_name(&fits), Some(format!("portail-{fits}")));
    }

    /// resources_test.go's 57-character conformance Gateway, with the hash
    /// sha256sum gives for it.
    #[test]
    fn a_long_name_is_cut_and_hashed() {
        let name =
            resource_name("gateway-with-one-not-matching-port-and-section-name-route").unwrap();
        assert_eq!(
            name,
            "portail-gateway-with-one-not-matching-port-and-section-f8dae0cc"
        );
        assert_eq!(name.len(), 63);
    }

    /// Go trims every '-' the cut leaves at its end.
    #[test]
    fn a_cut_ending_in_dashes_is_trimmed() {
        assert_eq!(
            resource_name("abcdefghij-abcdefghij-abcdefghij-abcdefghij-a-bcdefghijk").as_deref(),
            Some("portail-abcdefghij-abcdefghij-abcdefghij-abcdefghij-a-7f41598c")
        );
    }

    #[test]
    fn a_dotted_name_has_no_resource_name() {
        assert_eq!(resource_name("gw.example"), None);
    }

    #[test]
    fn label_values_hash_past_63() {
        assert_eq!(label_value("public"), "public");
        let max = "b".repeat(63);
        assert_eq!(label_value(&max), max);
        let long = "gateway-with-a-name-long-enough-to-pass-the-label-value-limit-of-63";
        let value = label_value(long);
        assert_eq!(
            value,
            "gateway-with-a-name-long-enough-to-pass-the-label-valu-7ed29937"
        );
        assert_eq!(value.len(), 63);
    }

    #[test]
    fn the_audience_is_the_service_name() {
        assert_eq!(
            controller_audience("portail-controller", "portail-system"),
            "portail-controller.portail-system.svc"
        );
    }
}
