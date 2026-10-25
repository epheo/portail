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
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::ServiceAccount;
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use kube::core::PartialObjectMeta;
use kube::runtime::reflector::store::Writer;
use kube::runtime::reflector::{ObjectRef, Store};

use super::params::Params;
use super::watch::Listings;
use super::ControllerCtx;
use crate::config::PortailConfig;
use crate::sync::Transport;

pub(crate) use render::ControllerEndpoint;

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

/// What provisioning reads: the owned-object caches, and how its data
/// planes reach the controller.
pub(crate) struct Owned {
    #[allow(dead_code)] // Read by reconcile once it provisions.
    service_accounts: Store<PartialObjectMeta<ServiceAccount>>,
    #[allow(dead_code)]
    deployments: Store<PartialObjectMeta<Deployment>>,
    #[allow(dead_code)]
    pdbs: Store<PartialObjectMeta<PodDisruptionBudget>>,
    #[allow(dead_code)]
    controller: ControllerEndpoint,
}

impl Owned {
    /// Start the owned-object reflectors, each tracked in `listings`. The
    /// stream names the Gateway of `gateways` each change to an owned
    /// object concerns.
    pub(crate) fn start(
        _client: &kube::Client,
        _gateways: Store<Gateway>,
        _listings: &Listings,
        controller: ControllerEndpoint,
    ) -> (Self, BoxStream<'static, ObjectRef<Gateway>>) {
        let owned = Owned {
            service_accounts: Writer::default().as_reader(),
            deployments: Writer::default().as_reader(),
            pdbs: Writer::default().as_reader(),
            controller,
        };
        (owned, Box::pin(futures::stream::pending()))
    }

    /// Over stores a test fills.
    #[cfg(test)]
    pub(crate) fn new(
        service_accounts: Store<PartialObjectMeta<ServiceAccount>>,
        deployments: Store<PartialObjectMeta<Deployment>>,
        pdbs: Store<PartialObjectMeta<PodDisruptionBudget>>,
        controller: ControllerEndpoint,
    ) -> Self {
        Owned {
            service_accounts,
            deployments,
            pdbs,
            controller,
        }
    }
}

/// Bring `gateway`'s objects in line with `params` before its snapshot is
/// published. `Err` is a failure worth retrying (a transport error, 5xx,
/// a conflict): nothing is published this pass. One a retry will not cure
/// is `Provisioned::problem`.
pub(crate) async fn reconcile(
    _ctx: &ControllerCtx,
    _gateway: &Gateway,
    _params: &Params,
) -> anyhow::Result<Provisioned> {
    Ok(Provisioned::unmanaged())
}

/// The Gateway left this controller (its class names another): delete its
/// owned objects. `Err` leaves some for a retry.
pub(crate) async fn release(_ctx: &ControllerCtx, _gateway: &Gateway) -> anyhow::Result<()> {
    Ok(())
}
