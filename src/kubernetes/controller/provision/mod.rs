//! A Managed Gateway's data-plane objects: its ServiceAccount, Deployment,
//! Service and PodDisruptionBudget, named by `naming::resource_name`,
//! server-side applied, owned by the Gateway.
//!
//! Stub: nothing is provisioned, so every Gateway binds its published
//! ports, as an Unmanaged one does.

mod ports;
mod render;

use std::collections::BTreeMap;

use futures::stream::BoxStream;
use gateway_api::gateways::Gateway;
use kube::runtime::reflector::ObjectRef;

use super::params::Params;
use super::translate::Plan;
use super::ControllerCtx;
use crate::config::PortailConfig;
use crate::sync::Transport;

/// What provisioning left for the snapshot and the status.
pub(crate) struct Provisioned {
    /// The port bound per (transport, published port) where a Service maps
    /// one onto the other: the snapshot listener's `targetPort`.
    pub bound_ports: BTreeMap<(Transport, u16), u16>,
    /// Why the data plane cannot be provisioned, e.g. a name conflict:
    /// Programmed=False reason Invalid.
    pub problem: Option<String>,
}

impl Provisioned {
    /// Nothing provisioned: listeners bind their published ports.
    pub(crate) fn unmanaged() -> Self {
        Self {
            bound_ports: BTreeMap::new(),
            problem: None,
        }
    }

    /// Give `config`'s listeners their bound ports.
    pub(crate) fn stamp(&self, config: &mut PortailConfig) {
        for l in &mut config.gateway.listeners {
            l.target_port = self
                .bound_ports
                .get(&(Transport::of(&l.protocol), l.port))
                .copied();
        }
    }
}

/// The owned-object caches provisioning reads.
pub(crate) struct Owned;

impl Owned {
    /// Start the owned-object reflectors. The stream names the Gateway each
    /// change to an owned object concerns.
    pub(crate) fn start(_client: &kube::Client) -> (Self, BoxStream<'static, ObjectRef<Gateway>>) {
        (Owned, Box::pin(futures::stream::pending()))
    }
}

/// Bring `gateway`'s objects in line with `params` and `plan`, before its
/// snapshot is published.
pub(crate) async fn reconcile(
    _ctx: &ControllerCtx,
    _gateway: &Gateway,
    _params: &Params,
    _plan: &Plan,
) -> Provisioned {
    Provisioned::unmanaged()
}

/// The Gateway left this controller (its class names another): delete its
/// owned objects. `Err` leaves some for a retry.
pub(crate) async fn release(_ctx: &ControllerCtx, _gateway: &Gateway) -> anyhow::Result<()> {
    Ok(())
}
