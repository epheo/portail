//! Linux process setup that must run in `main` before the Tokio runtime:
//! capability handling for direct privileged-port binds, and resource-limit
//! raises.
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
            Err(_e) => debug!("Could not query process capabilities: {_e}"),
        }
    }
}

/// Raise the soft `RLIMIT_NOFILE` to the hard ceiling. Call once in `main`.
///
/// Container runtimes commonly hand processes a 1024 soft limit against a
/// six-figure hard limit — and a proxy spends two fds per proxied connection,
/// so 1024 caps it at roughly 500 concurrent connections. Worse, once
/// `accept()` hits EMFILE *every* listener breaks, including the admin
/// listener, turning fd pressure into a liveness-probe restart loop (observed
/// on MicroShift: leaked half-dead connections exhausted the soft limit in
/// ~16h, /livez timed out, kubelet restarted the pod). Raising soft to hard
/// needs no capability, and operators still bound the process via the
/// container's hard limit.
pub fn raise_nofile_limit() {
    #[cfg(target_os = "linux")]
    {
        use crate::logging::{info, warn};

        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit fills the struct we own; setrlimit only reads it.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
            warn!(
                "Could not query RLIMIT_NOFILE: {}",
                std::io::Error::last_os_error()
            );
            return;
        }
        if lim.rlim_cur >= lim.rlim_max {
            return;
        }
        let soft = lim.rlim_cur;
        lim.rlim_cur = lim.rlim_max;
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) } != 0 {
            warn!(
                "Could not raise RLIMIT_NOFILE {} -> {}: {}",
                soft,
                lim.rlim_max,
                std::io::Error::last_os_error()
            );
        } else {
            info!("Raised RLIMIT_NOFILE: {} -> {}", soft, lim.rlim_max);
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    /// The raise must leave soft == hard; a second call is a clean no-op.
    #[test]
    fn nofile_soft_raised_to_hard() {
        super::raise_nofile_limit();
        super::raise_nofile_limit();
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) }, 0);
        assert_eq!(lim.rlim_cur, lim.rlim_max);
    }
}
