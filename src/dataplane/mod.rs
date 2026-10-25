//! A Gateway's data plane in Kubernetes mode: it binds and serves what
//! the controller's snapshots say, and reports what it bound.
//!
//! Imports none of `kube`, `k8s_openapi`, `gateway_api` or
//! `crate::kubernetes` (CI greps for them): a data plane holds no API
//! client, permission or token.

pub mod addresses;
pub mod apply;
pub mod dns_refresh;

pub use apply::Applier;

/// `portail dataplane`.
pub async fn run(args: crate::cli::DataplaneArgs) -> anyhow::Result<()> {
    anyhow::bail!(
        "portail dataplane --gateway {}: not implemented in this build",
        args.gateway
    )
}
