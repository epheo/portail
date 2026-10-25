//! Names and label values derived from a Gateway: provisioning creates
//! objects under them and sync authorization expects the ServiceAccount
//! they name, so both must compute the same bytes.

use crate::sync::{is_dns1123_label, sha256_hex};

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
/// Label-value length limit, the DNS-1123 label one.
const MAX_LABEL: usize = crate::sync::MAX_DNS1123_LABEL;

/// The name of a Gateway's ServiceAccount, Deployment, Service and PDB:
/// `portail-<gateway>`, or, when that is no DNS-1123 label, the Gateway name
/// cut to fit with a hash of the whole name appended. Same bytes as the
/// operator's `resourceName` for every name it accepted, so existing objects
/// keep their names.
pub fn resource_name(gateway: &str) -> String {
    let name = format!("{PREFIX}{gateway}");
    if is_dns1123_label(&name) {
        return name;
    }
    // A Gateway name is a DNS-1123 subdomain: a dot is the one character a
    // label refuses. The hash is of the name as given, so `a.b` never
    // takes `a-b`'s name.
    let label = gateway.replace('.', "-");
    let max_cut = MAX_LABEL - PREFIX.len() - 1 - HASH_HEX_DIGITS;
    let cut = label.get(..max_cut).unwrap_or(&label).trim_end_matches('-');
    let hash = &sha256_hex(gateway.as_bytes())[..HASH_HEX_DIGITS];
    format!("{PREFIX}{cut}-{hash}")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_name_is_prefixed() {
        assert_eq!(resource_name("test-gw"), "portail-test-gw");
        // 8 + 55 = 63: still fits.
        let fits = "a".repeat(55);
        assert_eq!(resource_name(&fits), format!("portail-{fits}"));
    }

    /// resources_test.go's 57-character conformance Gateway, with the hash
    /// sha256sum gives for it.
    #[test]
    fn a_long_name_is_cut_and_hashed() {
        let name = resource_name("gateway-with-one-not-matching-port-and-section-name-route");
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
            resource_name("abcdefghij-abcdefghij-abcdefghij-abcdefghij-a-bcdefghijk"),
            "portail-abcdefghij-abcdefghij-abcdefghij-abcdefghij-a-7f41598c"
        );
    }

    /// Every legal Gateway name gets one, a dotted one too, with the
    /// hash sha256sum gives for the name as written: `a.b` and `a-b` never
    /// share a name.
    #[test]
    fn a_dotted_name_gets_a_label() {
        let long = "a.b.c.d.e.f.g.h.i.j.k.l.m.n.o.p.q.r.s.t.u.v.w.x.y.z-0123456789-0123456789";
        for (gateway, name) in [
            ("gw.example", "portail-gw-example-f8a6900a"),
            ("a.b", "portail-a-b-2e7336dc"),
            ("a-b", "portail-a-b"),
            (
                long,
                "portail-a-b-c-d-e-f-g-h-i-j-k-l-m-n-o-p-q-r-s-t-u-v-w-15874345",
            ),
        ] {
            assert_eq!(resource_name(gateway), name);
            assert!(is_dns1123_label(name), "{name}");
        }
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
