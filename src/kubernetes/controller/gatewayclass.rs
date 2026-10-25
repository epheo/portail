//! GatewayClass status: Accepted, observedGeneration and supportedFeatures
//! on every class naming this controller, written by the leader where they
//! differ from the store.
//!
//! Stub: nothing is written.

use std::sync::Arc;

use futures::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;

use super::ControllerCtx;

/// Write class status on each item of `changes` (a class was added,
/// changed or deleted) until `shutdown`.
pub(crate) async fn run(
    _ctx: Arc<ControllerCtx>,
    changes: impl Stream<Item = ()> + Send,
    shutdown: CancellationToken,
) {
    let mut changes = std::pin::pin!(changes);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            change = changes.next() => if change.is_none() { return },
        }
    }
}
