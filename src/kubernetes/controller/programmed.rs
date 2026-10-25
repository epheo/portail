//! Programmed from data-plane reports: what the instances at the served
//! version bound, never the spec alone.
//!
//! Stub: v0.1.28's rule, over the first live instance at the served
//! version.

use std::collections::HashMap;

use gateway_api::gateways::Gateway;

use super::hub::Hub;
use super::reports::InstanceId;
use crate::dataplane::addresses::{bind_addresses_resolved, unresolved_networks};
use crate::kubernetes::addresses::UsableAddresses;
use crate::kubernetes::status::{GatewayCondition, ListenerStatus};
use crate::logging::warn;
use crate::sync::{GatewayKey, InstanceReport, ListenerReport, Snapshot};

pub(crate) struct Programmed {
    pub condition: GatewayCondition,
    /// What status.addresses is computed from.
    pub usable: UsableAddresses,
}

fn pending(message: String) -> GatewayCondition {
    GatewayCondition {
        ok: false,
        reason: "Pending".into(),
        message,
    }
}

/// Every endpoint of the listener is bound, and it has one.
fn bound(report: &InstanceReport, listener: &str) -> bool {
    report
        .listeners
        .iter()
        .find(|l| l.name == listener)
        .is_some_and(|l: &ListenerReport| {
            !l.endpoints.is_empty() && l.endpoints.iter().all(|e| e.bound)
        })
}

/// The Gateway's Programmed condition, and each accepted listener's
/// Programmed in `listeners`, from what the instances serving `served`
/// reported.
pub(crate) fn evaluate(
    gateway: &Gateway,
    key: &GatewayKey,
    hub: &Hub,
    served: Option<&Snapshot>,
    reports: &[(InstanceId, InstanceReport)],
    listeners: &mut HashMap<String, ListenerStatus>,
) -> Programmed {
    let mark_pending = |listeners: &mut HashMap<String, ListenerStatus>, message: &str| {
        for ls in listeners.values_mut().filter(|ls| ls.programmed) {
            ls.programmed = false;
            ls.programmed_reason = "Pending".into();
            ls.programmed_message = message.to_string();
        }
    };
    let Some(served) = served else {
        let message = "No configuration published for this Gateway yet".to_string();
        mark_pending(listeners, &message);
        return Programmed {
            condition: pending(message),
            usable: UsableAddresses::default(),
        };
    };
    let Some(report) = reports
        .iter()
        .map(|(_, r)| r)
        .find(|r| r.applied == served.version)
    else {
        let generation = served.gateway.generation;
        let message = match reports
            .first()
            .and_then(|(_, r)| hub.generation_of(key, &r.applied))
        {
            Some(older) => format!(
                "No data plane applied generation {generation} yet; one serves generation {older}"
            ),
            None => format!("No data plane applied generation {generation} yet"),
        };
        mark_pending(listeners, &message);
        return Programmed {
            condition: pending(message),
            usable: reports
                .first()
                .map(|(_, r)| UsableAddresses::of(r))
                .unwrap_or_default(),
        };
    };

    let config = &served.config;
    let bind = bind_addresses_resolved(
        &config.gateway.addresses,
        &report.networks,
        &report.interface_ips,
    );
    // Gateway API v1.4.1 ListenerReasonPending: the listener is "not yet
    // online and ready to accept client traffic".
    for l in &config.gateway.listeners {
        let Some(ls) = listeners.get_mut(&l.name).filter(|ls| ls.programmed) else {
            continue;
        };
        let message = match bind {
            None => "Not bound: no address of this Gateway is on the pod".to_string(),
            Some(_) if bound(report, &l.name) => continue,
            Some(_) => match report
                .listeners
                .iter()
                .filter(|r| r.name == l.name)
                .flat_map(|r| &r.endpoints)
                .find_map(|e| e.error.as_deref())
            {
                Some(err) => format!("Listener port not bound: {}", err),
                None => "Listener port not bound yet".to_string(),
            },
        };
        ls.programmed = false;
        ls.programmed_reason = "Pending".into();
        ls.programmed_message = message;
    }

    // Every listener skipped as unconvertible: zero endpoints make the
    // bind check vacuously true, but nothing serves.
    let condition = if let Some(e) = &report.apply_error {
        GatewayCondition::invalid(format!("Route table conversion failed: {}", e))
    } else if config.gateway.listeners.is_empty() && !gateway.spec.listeners.is_empty() {
        GatewayCondition::invalid("No listener could be programmed")
    } else if bind.is_none() {
        GatewayCondition {
            ok: false,
            reason: "AddressNotUsable".to_string(),
            message: format!(
                "Network address {} has no IP on this pod, or its NetworkAttachmentDefinition \
                 could not be read, so no listener of this Gateway is bound; check the \
                 NetworkAttachmentDefinition and the pod's network attachment",
                unresolved_networks(&config.gateway.addresses, &report.networks).join(", ")
            ),
        }
    } else if config
        .gateway
        .listeners
        .iter()
        .all(|l| bound(report, &l.name))
    {
        GatewayCondition::ok("Programmed", "Programmed")
    } else {
        warn!("Data plane not ready: not all listener ports are bound");
        GatewayCondition::invalid("Data plane not ready: not all listener ports are bound")
    };
    Programmed {
        condition,
        usable: UsableAddresses::of(report),
    }
}
