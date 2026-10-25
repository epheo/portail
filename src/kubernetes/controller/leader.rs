//! Leader election: a `coordination.k8s.io/v1` Lease, the client-go
//! algorithm and defaults. Standbys keep their caches warm, write nothing
//! and answer polls 503.
//!
//! Stub: every replica leads.

use futures::Stream;

#[derive(Clone)]
pub(crate) struct Leader;

impl Leader {
    /// A replica that always leads.
    pub(crate) fn always() -> Self {
        Leader
    }

    /// Whether this replica may write status, provision and serve.
    pub(crate) fn is_leader(&self) -> bool {
        true
    }

    /// One item per leadership gained: every Gateway is reconciled again.
    pub(crate) fn gained(&self) -> impl Stream<Item = ()> + Send + Sync + 'static {
        futures::stream::pending()
    }
}

/// Take part in the election for `args.leader_election_id` in
/// `args.namespace` until `shutdown`.
pub(crate) fn start(
    _client: &kube::Client,
    _args: &crate::cli::ControllerArgs,
    _shutdown: &tokio_util::sync::CancellationToken,
) -> Leader {
    Leader::always()
}
