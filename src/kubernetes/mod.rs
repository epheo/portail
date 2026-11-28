pub mod addresses;
pub mod backend_tls;
pub mod controller;
pub mod converters;
pub mod crds;
pub mod features;
pub mod hostname;
pub mod listener_sets;
pub mod naming;
pub mod parent_ref;
pub mod reads;
pub mod reconciler;
pub mod reference_grants;
pub mod services;
pub mod status;

/// An apiserver failure the next try may not meet: overload, unavailability,
/// or no answer. A refusal (401, 403, 422) is none: it holds until someone
/// changes something, and status should say so.
pub(crate) fn transient(e: &kube::Error) -> bool {
    match e {
        kube::Error::Api(status) => status.code == 429 || status.code >= 500,
        kube::Error::HyperError(_) | kube::Error::Service(_) | kube::Error::ReadEvents(_) => true,
        _ => false,
    }
}

/// A read that failed, and whether trying again may do.
#[derive(Debug)]
pub(crate) struct ReadFailed {
    pub message: String,
    pub transient: bool,
}

#[cfg(test)]
mod tests {
    /// Overload and unavailability may pass; a refusal holds.
    #[test]
    fn only_overload_and_unavailability_are_transient() {
        let api = |code: u16| {
            kube::Error::Api(Box::new(
                serde_json::from_value(serde_json::json!({"code": code})).unwrap(),
            ))
        };
        for code in [429, 500, 503, 504] {
            assert!(super::transient(&api(code)), "{code}");
        }
        for code in [401, 403, 404, 409, 422] {
            assert!(!super::transient(&api(code)), "{code}");
        }
    }
}
