//! Linux capability handling for direct privileged-port binds.
//!
//! In multi-network/UDN mode and the host-networked DaemonSet the data plane
//! binds the published port (e.g. 80/443) directly — there is no fronting
//! Service to remap it to an unprivileged target. The container image carries
//! `cap_net_bind_service` as a **file** capability with the *permitted-only*
//! bit (`+p`), set in the `Containerfile`.
//!
//! Why `+p` and not `+ep`: a binary whose file capabilities include the
//! *effective* bit fails to `execve` when that capability is not in the
//! process's bounding set (`EPERM`) — exactly the service-mode pod, which drops
//! all capabilities. With `+p` the binary always execs; the capability only
//! lands in the process's *permitted* set when the pod also grants it
//! (`capabilities.add: [NET_BIND_SERVICE]`, which the operator sets for
//! network-mode Gateways and which OpenShift `restricted-v2` already permits).
//!
//! This module raises that capability from *permitted* to *effective* so a
//! privileged bind succeeds — under restricted Pod Security, non-root, with no
//! `allowPrivilegeEscalation` and no custom SCC. Linux capability sets are
//! per-thread but are *copied* to threads at creation, so the raise must run
//! **before** the Tokio runtime spawns its workers; they then inherit the
//! effective capability and the binds (which happen on worker threads) succeed.
//!
//! When the capability is absent — service-mode pods that only ever bind
//! unprivileged target ports — the raise is a harmless, expected no-op.

/// Raise `CAP_NET_BIND_SERVICE` from the permitted to the effective set for the
/// current thread, when present. Call once in `main` *before* building the
/// Tokio runtime. A missing capability (service-mode) is expected and logged at
/// debug; it is never an error.
pub fn raise_net_bind_service() {
    #[cfg(target_os = "linux")]
    {
        use crate::logging::{debug, info, warn};
        use caps::{CapSet, Capability};

        match caps::has_cap(None, CapSet::Permitted, Capability::CAP_NET_BIND_SERVICE) {
            Ok(true) => {
                match caps::raise(None, CapSet::Effective, Capability::CAP_NET_BIND_SERVICE) {
                    Ok(()) => {
                        info!("Raised CAP_NET_BIND_SERVICE (effective) for direct privileged binds")
                    }
                    Err(e) => {
                        warn!("CAP_NET_BIND_SERVICE is permitted but could not be raised: {e}")
                    }
                }
            }
            Ok(false) => debug!(
                "CAP_NET_BIND_SERVICE not permitted; direct privileged binds unavailable \
                 (service-mode: listeners use the Service's unprivileged target port)"
            ),
            Err(e) => debug!("Could not query process capabilities: {e}"),
        }
    }
}
