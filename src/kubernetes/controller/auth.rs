//! Sync request authentication and authorization: a local JWT pre-check,
//! then TokenReview with the controller audience, cached by token hash; the
//! ServiceAccount must be `naming::resource_name` of the requested Gateway,
//! in its namespace.
//!
//! Stub: nobody is authenticated.

// Called by the sync server, which serves nothing yet.
#![allow(dead_code)]

use super::reports::InstanceId;
use crate::sync::GatewayKey;

/// Why a request is refused, as its status code.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// 401: the token is no valid controller-audience token.
    Unauthenticated,
    /// 503: TokenReviews in flight are at their cap.
    Throttled,
}

/// The instance whose token this is, if it may sync `gateway`.
pub(crate) async fn authenticate(
    _ctx: &super::ControllerCtx,
    _token: &str,
    _gateway: &GatewayKey,
) -> Result<InstanceId, Refusal> {
    Err(Refusal::Unauthenticated)
}
