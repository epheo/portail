//! A Gateway's GatewayParameters: built-in defaults, then its
//! GatewayClass's parametersRef, then its own infrastructure parametersRef,
//! field by field; then validated.
//!
//! Stub: the built-in defaults, whatever the refs say.

use gateway_api::gateways::Gateway;

use super::ControllerCtx;
use crate::kubernetes::crds::{GatewayParametersSpec, Provisioning};

/// A Gateway's parameters with every default applied.
pub(crate) type Params = GatewayParametersSpec;

/// `Err` says why the parameters cannot be used: the Gateway is
/// Accepted=False InvalidParameters, and its last snapshot and objects
/// stay as they are.
pub(crate) fn resolve(ctx: &ControllerCtx, _gateway: &Gateway) -> Result<Params, String> {
    Ok(GatewayParametersSpec {
        provisioning: Some(Provisioning::Managed),
        replicas: Some(1),
        image: Some(ctx.dataplane_image.clone()),
        ..Default::default()
    })
}
