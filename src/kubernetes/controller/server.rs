//! `POST /v1/sync` over TLS: authenticate, record the report, answer the
//! current snapshot or hold the poll until a newer one.
//!
//! Stub: nothing is served.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::ca::ControllerCa;
use super::ControllerCtx;

/// Serve sync requests on `addr` until `shutdown`.
pub(crate) async fn serve(
    _ctx: Arc<ControllerCtx>,
    _ca: ControllerCa,
    addr: SocketAddr,
    _shutdown: CancellationToken,
) -> anyhow::Result<()> {
    anyhow::bail!("sync server on {addr}: not implemented in this build")
}
