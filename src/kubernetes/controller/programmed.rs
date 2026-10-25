//! A Gateway's status from its translation, its data planes' reports and
//! the Service fronting them: Programmed is what the live instances at the
//! published version bound, never the spec alone.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use gateway_api::gateways::Gateway;
use k8s_openapi::api::core::v1::Service;
use kube::runtime::reflector::ObjectRef;
use kube::ResourceExt;
use serde_json::Value;
use tokio::time::Instant;

use super::reports::InstanceId;
use super::translate::Statuses;
use super::ControllerCtx;
use crate::dataplane::addresses::{bind_addresses_resolved, unresolved_networks};
use crate::kubernetes::addresses::NETWORK_ADDRESS_TYPE;
use crate::kubernetes::naming;
use crate::kubernetes::status::{self, condition, GatewayCondition, ListenerStatus};
use crate::logging::debug;
use crate::sync::{GatewayKey, InstanceReport, Snapshot, HOLD};

/// How long the written Programmed stands while no live instance applied
/// the published version: one hold. A live data plane applies a version
/// about a second after its publish, a replacement or a reconnecting one
/// within a hold. Conformance waits 60s (LatestObservedGenerationSet) for
/// the latest observedGeneration: the lapse to Pending, debounce included,
/// must come well before.
const KEEP: Duration = HOLD;

const IP_ADDRESS: &str = "IPAddress";
const HOSTNAME: &str = "Hostname";

/// Why a pass published nothing: the snapshot served, if any, is an older
/// generation's.
pub(crate) enum Unpublished {
    /// The translation failed.
    Rejected(String),
    InvalidParameters(String),
}

/// What a pass writes for its Gateway.
pub(crate) struct GatewayStatus {
    /// For [`status::write_gateway_status`].
    pub desired: Value,
    /// Programmed=True, computed or kept: route parents say Programmed.
    pub programmed: bool,
    /// When a kept Programmed lapses: a pass then writes Pending.
    pub recheck: Option<Duration>,
    /// Route and policy statuses are due: they come from a translation
    /// that was published.
    pub routes: bool,
}

/// The Gateway's status this pass, from the translation's `statuses`, the
/// live reports of the snapshot served and the Service it owns.
pub(crate) fn gateway_status(
    ctx: &ControllerCtx,
    gateway: &Gateway,
    key: &GatewayKey,
    statuses: &Statuses,
    attached: &HashMap<String, i32>,
    problem: Option<&str>,
    unpublished: Option<&Unpublished>,
) -> GatewayStatus {
    let served = ctx.hub.current(key);
    let reports = ctx.reports.live(key);
    let service = owned_service(ctx, gateway);
    decide(&Inputs {
        gateway,
        accepted: &statuses.accepted,
        listeners: &statuses.listeners,
        attached,
        problem,
        unpublished,
        served: served.as_deref(),
        reports: &reports,
        older: reports
            .first()
            .and_then(|(_, r)| ctx.hub.generation_of(key, &r.applied)),
        service: service.as_deref(),
        published_at: ctx.hub.published_at(key),
        first_published: ctx.hub.first_published_at(key),
        last_leaving: ctx.reports.last_leaving(key),
        now: Instant::now(),
    })
}

/// The Service provisioning made for `gateway`: its name, a controller
/// ownerReference to this Gateway's uid, our managed-by label. A
/// hand-written Service of that name is not the Gateway's address.
fn owned_service(ctx: &ControllerCtx, gateway: &Gateway) -> Option<Arc<Service>> {
    let uid = gateway.metadata.uid.as_deref()?;
    let name = naming::resource_name(&gateway.name_any());
    let ns = gateway.namespace()?;
    let svc = ctx.cache.services.get(&ObjectRef::new(&name).within(&ns))?;
    let owned = svc.metadata.deletion_timestamp.is_none()
        && svc
            .labels()
            .get(naming::LABEL_MANAGED_BY)
            .map(String::as_str)
            == Some(naming::MANAGED_BY)
        && svc
            .owner_references()
            .iter()
            .any(|o| o.controller == Some(true) && o.uid == uid);
    owned.then_some(svc)
}

/// What a Gateway's status is computed from.
struct Inputs<'a> {
    gateway: &'a Gateway,
    accepted: &'a GatewayCondition,
    listeners: &'a HashMap<String, ListenerStatus>,
    attached: &'a HashMap<String, i32>,
    /// Why the data plane cannot be provisioned.
    problem: Option<&'a str>,
    unpublished: Option<&'a Unpublished>,
    served: Option<&'a Snapshot>,
    /// The live instances.
    reports: &'a [(InstanceId, InstanceReport)],
    /// The generation the first live instance serves, for the message.
    older: Option<i64>,
    service: Option<&'a Service>,
    /// When the served version was published.
    published_at: Option<Instant>,
    /// When this controller first published the Gateway.
    first_published: Option<Instant>,
    /// When an instance last said it was leaving.
    last_leaving: Option<Instant>,
    now: Instant,
}

enum Verdict {
    Fresh(GatewayCondition),
    /// No live instance applied the served version yet: the written
    /// Programmed stands until then, `otherwise` where none was written.
    Kept {
        until: Instant,
        otherwise: GatewayCondition,
    },
}

/// What status.addresses says.
enum Addresses {
    /// From a source that knows, maybe none: written as it is.
    Known(Vec<(&'static str, String)>),
    /// Nothing knows: what was written stays.
    Unknown,
}

fn decide(i: &Inputs) -> GatewayStatus {
    let generation = i.gateway.metadata.generation;
    let observed = status::observed_gateway_status(i.gateway);
    if let Some(why) = i.unpublished {
        return unpublished(i, why, &observed);
    }

    let mut listeners = i.listeners.clone();
    let service = i.service.map(ServiceView::of);
    let (verdict, addresses, pending) = evaluate(i, &mut listeners, service.as_ref());
    let verdict = if let Some(problem) = i.problem {
        Verdict::Fresh(GatewayCondition::invalid(problem))
    } else if !i.accepted.ok {
        Verdict::Fresh(GatewayCondition::invalid(format!(
            "Gateway not accepted: {}",
            i.accepted.message
        )))
    } else if let Some(message) = service.as_ref().and_then(|s| s.unassigned(i.gateway)) {
        Verdict::Fresh(GatewayCondition {
            ok: false,
            reason: "AddressNotAssigned".to_string(),
            message,
        })
    } else {
        verdict
    };

    let mut entries: Vec<Value> = i
        .gateway
        .spec
        .listeners
        .iter()
        .map(|l| {
            let ls = listeners.get(&l.name).cloned().unwrap_or_default();
            let attached = i.attached.get(&l.name).copied().unwrap_or(0);
            status::listener_entry(l, &ls, attached, generation)
        })
        .collect();
    let fresh =
        |c: &GatewayCondition| condition("Programmed", c.ok, &c.reason, &c.message, generation);
    let (programmed, recheck) = match &verdict {
        Verdict::Fresh(c) => (fresh(c), None),
        Verdict::Kept { until, otherwise } => {
            keep_listener_programmed(&mut entries, &observed, &pending);
            match status::observed_condition(&observed["conditions"], "Programmed") {
                Some(kept) => (kept, Some(until.saturating_duration_since(i.now))),
                None => (fresh(otherwise), None),
            }
        }
    };
    let accepted = condition(
        "Accepted",
        i.accepted.ok,
        &i.accepted.reason,
        &i.accepted.message,
        generation,
    );
    let written = status::observed_list(&observed, "addresses");
    let addresses = match addresses {
        // An empty list only clears what was written: no placeholder.
        Addresses::Known(known) if known.is_empty() => written.map(|_| Vec::new()),
        Addresses::Known(known) => Some(known.iter().map(|(t, v)| status::address(t, v)).collect()),
        Addresses::Unknown => written,
    };
    GatewayStatus {
        programmed: programmed["status"] == "True",
        desired: status::desired_gateway_status([accepted, programmed], entries, addresses),
        recheck,
        routes: true,
    }
}

/// Nothing was published this pass: Accepted says why, Programmed is
/// False, and the listeners and addresses written for the translation that
/// was published stay as they are. A route's status is not written either.
fn unpublished(i: &Inputs, why: &Unpublished, observed: &Value) -> GatewayStatus {
    let generation = i.gateway.metadata.generation;
    let (accepted, message) = match why {
        Unpublished::Rejected(e) => (i.accepted.clone(), format!("Reconciliation failed: {e}")),
        Unpublished::InvalidParameters(m) => (
            GatewayCondition {
                ok: false,
                reason: "InvalidParameters".into(),
                message: m.clone(),
            },
            format!("Nothing published for this generation: {m}"),
        ),
    };
    let conditions = [
        condition(
            "Accepted",
            accepted.ok,
            &accepted.reason,
            &accepted.message,
            generation,
        ),
        condition("Programmed", false, "Invalid", &message, generation),
    ];
    GatewayStatus {
        desired: status::desired_gateway_status(
            conditions,
            status::observed_list(observed, "listeners").unwrap_or_default(),
            status::observed_list(observed, "addresses"),
        ),
        programmed: false,
        recheck: None,
        routes: false,
    }
}

/// Each listener entry `pending` names takes back its written Programmed
/// condition, when it has one.
fn keep_listener_programmed(entries: &mut [Value], observed: &Value, pending: &[String]) {
    let written = observed["listeners"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    for entry in entries {
        let name = entry["name"].as_str().unwrap_or_default().to_string();
        if !pending.contains(&name) {
            continue;
        }
        let Some(kept) = written
            .iter()
            .find(|w| w["name"] == entry["name"])
            .and_then(|w| status::observed_condition(&w["conditions"], "Programmed"))
        else {
            continue;
        };
        if let Some(c) = entry["conditions"]
            .as_array_mut()
            .and_then(|cs| cs.iter_mut().find(|c| c["type"] == "Programmed"))
        {
            *c = kept;
        }
    }
}

fn pending(message: &str) -> GatewayCondition {
    GatewayCondition {
        ok: false,
        reason: "Pending".into(),
        message: message.to_string(),
    }
}

/// Turn every listener the translation found programmable Pending;
/// returns their names.
fn mark_pending(listeners: &mut HashMap<String, ListenerStatus>, message: &str) -> Vec<String> {
    let mut marked = Vec::new();
    for (name, ls) in listeners.iter_mut().filter(|(_, ls)| ls.programmed) {
        ls.programmed = false;
        ls.programmed_reason = "Pending".into();
        ls.programmed_message = message.to_string();
        marked.push(name.clone());
    }
    marked
}

/// Every endpoint of the listener is bound, and it has one.
fn bound(report: &InstanceReport, listener: &str) -> bool {
    report
        .listeners
        .iter()
        .find(|l| l.name == listener)
        .is_some_and(|l| !l.endpoints.is_empty() && l.endpoints.iter().all(|e| e.bound))
}

/// Until when the written Programmed stands with no live instance at the
/// served version, a hold after the latest of: the publish, while an
/// instance at an older version is live to apply it; a goodbye, whose
/// replacement is due; this controller's first publish of the Gateway, as
/// data planes reconnect to a new leader. A data plane that died silently
/// keeps nothing across new versions.
fn keep_until(i: &Inputs) -> Option<Instant> {
    let live_older = !i.reports.is_empty();
    [
        i.published_at.filter(|_| live_older),
        i.last_leaving,
        i.first_published,
    ]
    .into_iter()
    .flatten()
    .max()
    .map(|t| t + KEEP)
}

/// Whether the spec names an address an instance could hold.
fn names_addresses(gateway: &Gateway) -> bool {
    gateway.spec.addresses.iter().flatten().any(|a| {
        let t = a.r#type.as_deref().unwrap_or(IP_ADDRESS);
        t == IP_ADDRESS || t == NETWORK_ADDRESS_TYPE
    })
}

/// The Gateway's Programmed from the reports, each listener's in
/// `listeners`, the addresses, and the listeners turned Pending for want
/// of an instance at the served version.
fn evaluate(
    i: &Inputs,
    listeners: &mut HashMap<String, ListenerStatus>,
    service: Option<&ServiceView>,
) -> (Verdict, Addresses, Vec<String>) {
    let from_service = || service.map(|s| Addresses::Known(s.addresses.clone()));
    let Some(served) = i.served else {
        let message = "No configuration published for this Gateway yet";
        let marked = mark_pending(listeners, message);
        let addresses = from_service().unwrap_or(Addresses::Unknown);
        return (Verdict::Fresh(pending(message)), addresses, marked);
    };
    let at_version: Vec<&InstanceReport> = i
        .reports
        .iter()
        .map(|(_, r)| r)
        .filter(|r| r.applied == served.version)
        .collect();
    if at_version.is_empty() {
        let generation = served.gateway.generation;
        let message = match i.older {
            Some(older) => format!(
                "No data plane applied generation {generation} yet; one serves generation {older}"
            ),
            None => format!("No data plane applied generation {generation} yet"),
        };
        let marked = mark_pending(listeners, &message);
        let (verdict, addresses) = match keep_until(i).filter(|until| *until > i.now) {
            Some(until) => (
                Verdict::Kept {
                    until,
                    otherwise: pending(&message),
                },
                from_service().unwrap_or(Addresses::Unknown),
            ),
            None => {
                // No pod and no address of its own to come back with: it
                // has none. A spec address will be back with its pod.
                let none = i.reports.is_empty() && !names_addresses(i.gateway);
                let addresses = from_service().unwrap_or(if none {
                    Addresses::Known(Vec::new())
                } else {
                    Addresses::Unknown
                });
                (Verdict::Fresh(pending(&message)), addresses)
            }
        };
        return (verdict, addresses, marked);
    }

    let config = &served.config;
    let binds: Vec<Option<Vec<String>>> = at_version
        .iter()
        .map(|r| bind_addresses_resolved(&config.gateway.addresses, &r.networks, &r.interface_ips))
        .collect();
    let unusable = binds.iter().all(Option::is_none);
    // Only listeners the translation could program count: one without a
    // usable certificate is Invalid on its own, the rest still serve
    // (Gateway API v1.4.1: Programmed says the configuration was generated).
    let counted: Vec<&str> = config
        .gateway
        .listeners
        .iter()
        .map(|l| l.name.as_str())
        .filter(|name| i.listeners.get(*name).is_some_and(|ls| ls.programmed))
        .collect();
    // Gateway API v1.4.1 ListenerReasonPending: the listener is "not yet
    // online and ready to accept client traffic"; the data plane retries
    // its binds.
    let mut all_bound = true;
    for name in &counted {
        let error = at_version
            .iter()
            .flat_map(|r| r.listeners.iter().filter(|x| x.name == *name))
            .flat_map(|x| &x.endpoints)
            .find_map(|e| e.error.as_deref());
        let message = match error {
            Some(err) => format!("Listener port not bound: {err}"),
            None if at_version.iter().any(|r| bound(r, name)) => continue,
            None if unusable => "Not bound: no address of this Gateway is on the pod".into(),
            None => "Listener port not bound yet".into(),
        };
        all_bound = false;
        if let Some(ls) = listeners.get_mut(*name) {
            ls.programmed = false;
            ls.programmed_reason = "Pending".into();
            ls.programmed_message = message;
        }
    }

    let condition = if let Some(e) = at_version.iter().find_map(|r| r.apply_error.as_deref()) {
        GatewayCondition::invalid(format!("Route table conversion failed: {e}"))
    } else if counted.is_empty() && !i.gateway.spec.listeners.is_empty() {
        // Zero listeners to bind make the bind check vacuously true, but
        // nothing serves.
        GatewayCondition::invalid("No listener could be programmed")
    } else if unusable {
        GatewayCondition {
            ok: false,
            reason: "AddressNotUsable".to_string(),
            message: format!(
                "Network address {} has no IP on this pod, or its NetworkAttachmentDefinition \
                 could not be read, so no listener of this Gateway is bound; check the \
                 NetworkAttachmentDefinition and the pod's network attachment",
                unresolved_networks(&config.gateway.addresses, &at_version[0].networks).join(", ")
            ),
        }
    } else if all_bound {
        GatewayCondition::ok("Programmed", "Programmed")
    } else {
        debug!("Data plane not ready: not all listener ports are bound");
        GatewayCondition::invalid("Data plane not ready: not all listener ports are bound")
    };
    let addresses = from_service().unwrap_or_else(|| {
        let held = instance_addresses(i.gateway, &at_version, &binds);
        if held.is_empty() {
            Addresses::Unknown
        } else {
            Addresses::Known(held)
        }
    });
    (Verdict::Fresh(condition), addresses, Vec::new())
}

/// status.addresses without a Service: per instance that binds, the
/// spec's IPAddress values it holds and its IP on each Network named,
/// else its first interface IP; the union, sorted.
fn instance_addresses(
    gateway: &Gateway,
    at_version: &[&InstanceReport],
    binds: &[Option<Vec<String>>],
) -> Vec<(&'static str, String)> {
    let spec = gateway.spec.addresses.as_deref().unwrap_or_default();
    let mut all = BTreeSet::new();
    for (report, bind) in at_version.iter().zip(binds) {
        if bind.is_none() {
            continue;
        }
        let held: Vec<String> = spec
            .iter()
            .filter_map(|a| {
                let value = a.value.as_deref()?;
                match a.r#type.as_deref().unwrap_or(IP_ADDRESS) {
                    IP_ADDRESS => report
                        .interface_ips
                        .iter()
                        .any(|ip| ip == value)
                        .then(|| value.to_string()),
                    t if t == NETWORK_ADDRESS_TYPE => report.networks.get(value).cloned(),
                    _ => None,
                }
            })
            .collect();
        if held.is_empty() {
            all.extend(report.interface_ips.first().cloned());
        } else {
            all.extend(held);
        }
    }
    all.into_iter().map(|ip| (IP_ADDRESS, ip)).collect()
}

/// The Service in front of a Gateway's data plane, as status reads it.
struct ServiceView {
    /// namespace/name, for messages.
    name: String,
    load_balancer: bool,
    /// LoadBalancer: its ingress IPs and hostnames; else its cluster IP.
    /// Sorted.
    addresses: Vec<(&'static str, String)>,
}

impl ServiceView {
    fn of(svc: &Service) -> Self {
        let spec = svc.spec.as_ref();
        let load_balancer = spec.and_then(|s| s.type_.as_deref()) == Some("LoadBalancer");
        let mut addresses: Vec<(&'static str, String)> = if load_balancer {
            svc.status
                .iter()
                .filter_map(|s| s.load_balancer.as_ref())
                .flat_map(|lb| lb.ingress.iter().flatten())
                .flat_map(|ingress| {
                    let ip = ingress.ip.iter().map(|ip| (IP_ADDRESS, ip.clone()));
                    let host = ingress.hostname.iter().map(|h| (HOSTNAME, h.clone()));
                    ip.chain(host)
                })
                .collect()
        } else {
            spec.and_then(|s| s.cluster_ip.clone())
                .filter(|ip| !ip.is_empty() && ip != "None")
                .map(|ip| (IP_ADDRESS, ip))
                .into_iter()
                .collect()
        };
        addresses.sort();
        addresses.dedup();
        Self {
            name: format!("{}/{}", svc.namespace().unwrap_or_default(), svc.name_any()),
            load_balancer,
            addresses,
        }
    }

    /// Why the Gateway's addresses are not this Service's: AddressNotAssigned.
    /// Static addresses are not requested from the load balancer, so an
    /// IPAddress the spec names is assigned only when the Service has it.
    fn unassigned(&self, gateway: &Gateway) -> Option<String> {
        if self.load_balancer && self.addresses.is_empty() {
            return Some(format!(
                "LoadBalancer Service {} has no ingress address yet",
                self.name
            ));
        }
        let missing: Vec<&str> = gateway
            .spec
            .addresses
            .iter()
            .flatten()
            .filter(|a| a.r#type.as_deref().unwrap_or(IP_ADDRESS) == IP_ADDRESS)
            .filter_map(|a| a.value.as_deref())
            .filter(|v| {
                !self
                    .addresses
                    .iter()
                    .any(|(t, a)| *t == IP_ADDRESS && a == v)
            })
            .collect();
        (!missing.is_empty()).then(|| {
            format!(
                "IPAddress {} not assigned: Service {} does not hold it, and static addresses \
                 are not requested from it",
                missing.join(", "),
                self.name
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::reconcile::{finish, publish, Step};
    use super::super::testkit::{self, apply, Pipeline};
    use super::*;
    use crate::sync::{EndpointReport, GatewayRef, ListenerReport, Transport};
    use serde_json::json;

    const V1: &str = "v1";

    fn gateway(generation: i64, addresses: &[(&str, &str)]) -> Gateway {
        let mut gw = testkit::gateway("gw", generation);
        if !addresses.is_empty() {
            gw.spec.addresses = Some(
                addresses
                    .iter()
                    .map(|(t, v)| gateway_api::gateways::GatewayAddresses {
                        r#type: Some(t.to_string()),
                        value: Some(v.to_string()),
                    })
                    .collect(),
            );
        }
        gw
    }

    /// The snapshot served for `gw`, its one listener `http` on 8080.
    fn snapshot(gw: &Gateway, config_addresses: &[&str]) -> Snapshot {
        let config = serde_json::from_value(json!({
            "gateway": {
                "name": "gw",
                "listeners": [{"name": "http", "protocol": "HTTP", "port": 8080}],
                "addresses": config_addresses,
            },
        }))
        .unwrap();
        let mut snapshot = Snapshot::from_config(
            GatewayRef {
                namespace: "default".into(),
                name: "gw".into(),
                uid: "gw-uid".into(),
                generation: gw.metadata.generation.unwrap(),
            },
            config,
            vec![],
        );
        snapshot.version = format!("v{}", gw.metadata.generation.unwrap());
        snapshot
    }

    /// An instance at `applied` holding `ips`, its listener bound or not.
    fn report(applied: &str, ips: &[&str], bound: bool) -> InstanceReport {
        InstanceReport {
            applied: applied.into(),
            listeners: vec![ListenerReport {
                name: "http".into(),
                endpoints: vec![EndpointReport {
                    address: None,
                    port: 8080,
                    transport: Transport::Tcp,
                    bound,
                    error: None,
                }],
            }],
            interface_ips: ips.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn live(reports: Vec<InstanceReport>) -> Vec<(InstanceId, InstanceReport)> {
        reports
            .into_iter()
            .enumerate()
            .map(|(n, r)| (InstanceId(format!("pod-{n}")), r))
            .collect()
    }

    fn programmable() -> HashMap<String, ListenerStatus> {
        let ls = ListenerStatus {
            accepted: true,
            resolved_refs: true,
            programmed: true,
            programmed_reason: "Programmed".into(),
            programmed_message: "Programmed".into(),
            ..Default::default()
        };
        [("http".to_string(), ls)].into()
    }

    /// One evaluation: `gw` served as `served`, these live reports and
    /// this Service, at `now`.
    struct Case {
        gw: Gateway,
        served: Option<Snapshot>,
        reports: Vec<(InstanceId, InstanceReport)>,
        service: Option<Service>,
        accepted: GatewayCondition,
        problem: Option<String>,
        unpublished: Option<Unpublished>,
        listeners: HashMap<String, ListenerStatus>,
        published_at: Option<Instant>,
        first_published: Option<Instant>,
        last_leaving: Option<Instant>,
        now: Instant,
    }

    impl Case {
        fn new(gw: Gateway, served: Snapshot, reports: Vec<InstanceReport>) -> Self {
            Self {
                gw,
                served: Some(served),
                reports: live(reports),
                service: None,
                accepted: GatewayCondition::ok("Accepted", "ok"),
                problem: None,
                unpublished: None,
                listeners: programmable(),
                published_at: None,
                first_published: None,
                last_leaving: None,
                now: Instant::now(),
            }
        }

        fn run(&self) -> GatewayStatus {
            decide(&Inputs {
                gateway: &self.gw,
                accepted: &self.accepted,
                listeners: &self.listeners,
                attached: &HashMap::new(),
                problem: self.problem.as_deref(),
                unpublished: self.unpublished.as_ref(),
                served: self.served.as_ref(),
                reports: &self.reports,
                older: None,
                service: self.service.as_ref(),
                published_at: self.published_at,
                first_published: self.first_published,
                last_leaving: self.last_leaving,
                now: self.now,
            })
        }
    }

    fn programmed_of(desired: &Value) -> &Value {
        condition_of(&desired["conditions"], "Programmed")
    }

    fn listener_programmed(desired: &Value) -> &Value {
        condition_of(&desired["listeners"][0]["conditions"], "Programmed")
    }

    fn condition_of<'a>(conditions: &'a Value, cond_type: &str) -> &'a Value {
        conditions
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["type"] == cond_type)
            .unwrap()
    }

    fn addresses(desired: &Value) -> Vec<(String, String)> {
        desired["addresses"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|e| {
                        (
                            e["type"].as_str().unwrap().into(),
                            e["value"].as_str().unwrap().into(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn ip(value: &str) -> (String, String) {
        ("IPAddress".into(), value.into())
    }

    /// Every observedGeneration in `v`.
    fn generations(v: &Value) -> Vec<i64> {
        match v {
            Value::Object(map) => map
                .iter()
                .flat_map(|(k, v)| match (k.as_str(), v.as_i64()) {
                    ("observedGeneration", Some(g)) => vec![g],
                    _ => generations(v),
                })
                .collect(),
            Value::Array(items) => items.iter().flat_map(generations).collect(),
            _ => vec![],
        }
    }

    /// `gw` as the store holds it after `desired` was written.
    fn written(gw: &Gateway, desired: &Value) -> Gateway {
        let mut stored = gw.clone();
        stored.status = Some(serde_json::from_value(stamped(desired)).unwrap());
        stored
    }

    fn stamped(desired: &Value) -> Value {
        let mut v = desired.clone();
        fn stamp(v: &mut Value) {
            match v {
                Value::Object(map) => {
                    if map.contains_key("reason") {
                        map.insert("lastTransitionTime".into(), json!("2020-01-01T00:00:00Z"));
                    }
                    map.values_mut().for_each(stamp);
                }
                Value::Array(items) => items.iter_mut().for_each(stamp),
                _ => {}
            }
        }
        stamp(&mut v);
        v
    }

    /// A live instance at the served version with every endpoint bound:
    /// Programmed, the listener too, every condition at the Gateway's
    /// generation, its first interface IP the address.
    #[test]
    fn a_bound_instance_at_the_served_version_programs() {
        let gw = gateway(3, &[]);
        let served = snapshot(&gw, &[]);
        let out = Case::new(
            gw,
            served,
            vec![report("v3", &["10.0.0.5", "10.0.1.5"], true)],
        )
        .run();
        assert!(out.programmed);
        assert!(out.routes);
        assert_eq!(out.recheck, None);
        assert_eq!(programmed_of(&out.desired)["status"], "True");
        assert_eq!(listener_programmed(&out.desired)["status"], "True");
        assert_eq!(addresses(&out.desired), [ip("10.0.0.5")]);
        let g = generations(&out.desired);
        assert!(!g.is_empty() && g.iter().all(|g| *g == 3), "{g:?}");
    }

    /// A bind error keeps the listener Pending with the error, and the
    /// Gateway not Programmed.
    #[test]
    fn a_bind_error_is_not_programmed() {
        let gw = gateway(1, &[]);
        let served = snapshot(&gw, &[]);
        let mut failed = report(V1, &["10.0.0.5"], false);
        failed.listeners[0].endpoints[0].error = Some("Address in use".into());
        let out = Case::new(gw, served, vec![failed]).run();
        let listener = listener_programmed(&out.desired);
        assert_eq!(listener["reason"], "Pending");
        assert_eq!(
            listener["message"],
            "Listener port not bound: Address in use"
        );
        assert_eq!(programmed_of(&out.desired)["reason"], "Invalid");
        assert!(!out.programmed);
    }

    /// Another instance at the version reporting a bind error holds the
    /// listener back, though one bound it.
    #[test]
    fn one_instance_failing_its_bind_holds_the_listener() {
        let gw = gateway(1, &[]);
        let served = snapshot(&gw, &[]);
        let mut failed = report(V1, &["10.0.0.6"], false);
        failed.listeners[0].endpoints[0].error = Some("Permission denied".into());
        let out = Case::new(gw, served, vec![report(V1, &["10.0.0.5"], true), failed]).run();
        assert_eq!(listener_programmed(&out.desired)["status"], "False");
        assert!(!out.programmed);
    }

    #[test]
    fn a_route_table_that_did_not_build_is_invalid() {
        let gw = gateway(1, &[]);
        let served = snapshot(&gw, &[]);
        let mut broken = report(V1, &["10.0.0.5"], true);
        broken.apply_error = Some("bad regex".into());
        let out = Case::new(gw, served, vec![broken]).run();
        let programmed = programmed_of(&out.desired);
        assert_eq!(programmed["reason"], "Invalid");
        assert_eq!(
            programmed["message"],
            "Route table conversion failed: bad regex"
        );
    }

    /// No live instance at the served version and nothing written yet:
    /// Pending, and no address, not even a placeholder.
    #[test]
    fn no_instance_at_the_version_is_pending_without_addresses() {
        let gw = gateway(2, &[]);
        let served = snapshot(&gw, &[]);
        let out = Case::new(gw, served, vec![report(V1, &["10.0.0.5"], true)]).run();
        let programmed = programmed_of(&out.desired);
        assert_eq!(programmed["reason"], "Pending");
        assert_eq!(programmed["observedGeneration"], 2);
        assert_eq!(
            programmed["message"],
            "No data plane applied generation 2 yet"
        );
        assert_eq!(listener_programmed(&out.desired)["reason"], "Pending");
        assert!(out.desired.get("addresses").is_none(), "{}", out.desired);
    }

    /// An edit publishes a new version: until the live instance applies
    /// it, the written Programmed and addresses stand for a hold, the
    /// Gateway's and each listener's, at their older generation. Then
    /// Pending, the addresses still as written: the instance holds them.
    #[test]
    fn the_written_programmed_stands_for_a_hold() {
        let first = gateway(1, &[]);
        let before = Case::new(
            first.clone(),
            snapshot(&first, &[]),
            vec![report(V1, &["10.0.0.5"], true)],
        )
        .run();
        let edited = {
            let mut gw = written(&first, &before.desired);
            gw.metadata.generation = Some(2);
            gw
        };
        let mut case = Case::new(
            edited.clone(),
            snapshot(&edited, &[]),
            vec![report(V1, &["10.0.0.5"], true)],
        );
        case.published_at = Some(case.now - (KEEP - Duration::from_secs(20)));

        let kept = case.run();
        let programmed = programmed_of(&kept.desired);
        assert_eq!(programmed["status"], "True");
        assert_eq!(programmed["observedGeneration"], 1, "as written");
        assert_eq!(listener_programmed(&kept.desired)["observedGeneration"], 1);
        assert_eq!(
            condition_of(&kept.desired["conditions"], "Accepted")["observedGeneration"],
            2
        );
        assert_eq!(addresses(&kept.desired), [ip("10.0.0.5")]);
        assert!(kept.programmed);
        assert_eq!(kept.recheck, Some(Duration::from_secs(20)));

        case.now += Duration::from_secs(20);
        let lapsed = case.run();
        let programmed = programmed_of(&lapsed.desired);
        assert_eq!(programmed["reason"], "Pending");
        assert_eq!(programmed["observedGeneration"], 2);
        assert_eq!(listener_programmed(&lapsed.desired)["reason"], "Pending");
        assert_eq!(
            addresses(&lapsed.desired),
            [ip("10.0.0.5")],
            "never a placeholder"
        );
        assert_eq!(lapsed.recheck, None);
    }

    /// A listener added by the edit has no written Programmed to keep.
    #[test]
    fn a_new_listener_is_pending_while_the_rest_is_kept() {
        let first = gateway(1, &[]);
        let before = Case::new(
            first.clone(),
            snapshot(&first, &[]),
            vec![report(V1, &["10.0.0.5"], true)],
        )
        .run();
        let mut edited = written(&first, &before.desired);
        edited.metadata.generation = Some(2);
        let mut added = edited.spec.listeners[0].clone();
        added.name = "admin".into();
        edited.spec.listeners.push(added);
        let mut case = Case::new(edited.clone(), snapshot(&edited, &[]), vec![]);
        case.first_published = Some(case.now);
        let http = case.listeners["http"].clone();
        case.listeners.insert("admin".into(), http);
        let out = case.run();
        let by_name = |name: &str| {
            let entry = out.desired["listeners"]
                .as_array()
                .unwrap()
                .iter()
                .find(|l| l["name"] == name)
                .unwrap();
            condition_of(&entry["conditions"], "Programmed").clone()
        };
        assert_eq!(by_name("http")["status"], "True");
        assert_eq!(by_name("admin")["reason"], "Pending");
    }

    /// A Gateway written Programmed, edited to generation 2 and published
    /// long after this controller first published it.
    fn edited_after_programmed(spec_addresses: &[(&str, &str)]) -> Case {
        let first = gateway(1, spec_addresses);
        let before = Case::new(
            first.clone(),
            snapshot(&first, &[]),
            vec![report(V1, &["10.0.0.5"], true)],
        )
        .run();
        let mut edited = written(&first, &before.desired);
        edited.metadata.generation = Some(2);
        let mut case = Case::new(edited.clone(), snapshot(&edited, &[]), vec![]);
        case.published_at = Some(case.now);
        case.first_published = Some(case.now - Duration::from_secs(600));
        case
    }

    /// A data plane that died silently keeps nothing: a new version with
    /// no live instance is Pending at once. Each publish restarted the
    /// keep, so a crashed Gateway stayed Programmed across edits.
    #[test]
    fn a_silent_data_plane_keeps_nothing_across_versions() {
        let out = edited_after_programmed(&[]).run();
        let programmed = programmed_of(&out.desired);
        assert_eq!(programmed["reason"], "Pending", "{}", out.desired);
        assert_eq!(programmed["observedGeneration"], 2);
        assert_eq!(out.recheck, None);
    }

    /// What does keep the written Programmed for a hold: an instance live
    /// at an older version (from the publish), a goodbye (its replacement
    /// is due), this controller's first publish (data planes reconnect).
    #[test]
    fn a_live_older_instance_a_goodbye_or_a_new_leader_keep_programmed() {
        let mut live_older = edited_after_programmed(&[]);
        live_older.reports = live(vec![report(V1, &["10.0.0.5"], true)]);
        let mut goodbye = edited_after_programmed(&[]);
        goodbye.last_leaving = Some(goodbye.now - Duration::from_secs(10));
        let mut new_leader = edited_after_programmed(&[]);
        new_leader.first_published = Some(new_leader.now - Duration::from_secs(5));
        for (case, left) in [
            (live_older, KEEP),
            (goodbye, KEEP - Duration::from_secs(10)),
        ] {
            let out = case.run();
            assert_eq!(programmed_of(&out.desired)["status"], "True");
            assert_eq!(out.recheck, Some(left));
        }
        let out = new_leader.run();
        assert_eq!(out.recheck, Some(KEEP - Duration::from_secs(5)));
    }

    /// The lapse to Pending, debounce and one refused write included,
    /// lands inside the 60s conformance waits for the latest
    /// observedGeneration (LatestObservedGenerationSet).
    #[test]
    fn the_keep_lapses_inside_the_conformance_wait() {
        let debounce = Duration::from_secs(1);
        let failed_write_retry = Duration::from_secs(10);
        assert!(KEEP + debounce + failed_write_retry < Duration::from_secs(60));
    }

    /// A LoadBalancer Service with no ingress knows the Gateway has no
    /// address: the written one is cleared, with AddressNotAssigned.
    #[test]
    fn a_load_balancer_without_ingress_clears_written_addresses() {
        let first = gateway(1, &[]);
        let mut case = Case::new(
            first.clone(),
            snapshot(&first, &[]),
            vec![report(V1, &["10.0.0.5"], true)],
        );
        case.service = Some(load_balancer(json!([{"ip": "203.0.113.7"}])));
        let before = case.run();
        case.gw = written(&first, &before.desired);
        case.service = Some(load_balancer(json!([])));
        let out = case.run();
        assert_eq!(programmed_of(&out.desired)["reason"], "AddressNotAssigned");
        assert_eq!(out.desired["addresses"], json!([]), "{}", out.desired);
    }

    /// Without a Service, no live instance and no address in the spec, the
    /// Gateway has none: the written ones are cleared. With a spec
    /// address, the pod comes back with it: unknown, kept.
    #[test]
    fn no_pod_and_no_spec_address_clears_written_addresses() {
        let out = edited_after_programmed(&[]).run();
        assert_eq!(programmed_of(&out.desired)["reason"], "Pending");
        assert_eq!(out.desired["addresses"], json!([]), "{}", out.desired);

        let out = edited_after_programmed(&[("IPAddress", "10.0.0.5")]).run();
        assert_eq!(addresses(&out.desired), [ip("10.0.0.5")]);
    }

    /// An HTTPS listener whose certificate went away is Invalid on its own
    /// (ResolvedRefs False from translation); the Gateway stays Programmed
    /// while the rest bind. Its unbound port held the Gateway back.
    #[test]
    fn a_listener_without_its_certificate_does_not_hold_the_gateway() {
        let mut gw = gateway(1, &[]);
        let mut https = gw.spec.listeners[0].clone();
        https.name = "https".into();
        https.protocol = "HTTPS".into();
        gw.spec.listeners.push(https);
        let mut served = snapshot(&gw, &[]);
        let mut config = served.to_config();
        let mut listener = config.gateway.listeners[0].clone();
        listener.name = "https".into();
        listener.port = 8443;
        config.gateway.listeners.push(listener);
        served = Snapshot::from_config(served.gateway.clone(), config, vec![]);
        let mut unbound = report(&served.version, &["10.0.0.5"], true);
        unbound.listeners.push(ListenerReport {
            name: "https".into(),
            endpoints: vec![EndpointReport {
                address: None,
                port: 8443,
                transport: Transport::Tcp,
                bound: false,
                error: Some("no certificate".into()),
            }],
        });
        let mut case = Case::new(gw, served, vec![unbound]);
        case.listeners.insert(
            "https".into(),
            ListenerStatus {
                accepted: true,
                resolved_refs: false,
                resolved_refs_reason: "InvalidCertificateRef".into(),
                programmed: false,
                programmed_reason: "Invalid".into(),
                programmed_message: "Listener has unresolved references".into(),
                ..Default::default()
            },
        );
        let out = case.run();
        assert!(out.programmed, "{}", out.desired);
        let https = out.desired["listeners"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["name"] == "https")
            .unwrap();
        assert_eq!(
            condition_of(&https["conditions"], "Programmed")["reason"],
            "Invalid"
        );
    }

    /// A Network address with no IP on the pod binds nothing: the Gateway
    /// says AddressNotUsable, and reports no address of that pod.
    #[test]
    fn an_unresolved_network_is_not_usable() {
        let gw = gateway(1, &[(NETWORK_ADDRESS_TYPE, "udn-a")]);
        let served = snapshot(&gw, &["udn-a"]);
        let mut unbound = report(V1, &["10.0.0.5"], false);
        unbound.listeners[0].endpoints.clear();
        let out = Case::new(gw, served, vec![unbound]).run();
        let programmed = programmed_of(&out.desired);
        assert_eq!(programmed["reason"], "AddressNotUsable");
        assert!(programmed["message"].as_str().unwrap().contains("udn-a"));
        assert_eq!(
            listener_programmed(&out.desired)["message"],
            "Not bound: no address of this Gateway is on the pod"
        );
        assert!(out.desired.get("addresses").is_none());
    }

    /// Without a Service, each instance at the version gives the spec's
    /// IPAddress values it holds and its IPs on the Networks named, else
    /// its first interface IP; the union, sorted. An instance at an older
    /// version gives nothing.
    #[test]
    fn instances_give_their_addresses_without_a_service() {
        let gw = gateway(1, &[("IPAddress", "172.16.87.21")]);
        let served = snapshot(&gw, &["172.16.87.21"]);
        let vip = report(V1, &["10.0.0.5", "172.16.87.21"], true);
        let wildcard = report(V1, &["10.0.0.9", "10.0.1.9"], true);
        let older = report("v0", &["10.0.0.99"], true);
        let out = Case::new(gw, served, vec![vip, wildcard, older]).run();
        assert_eq!(
            addresses(&out.desired),
            [ip("10.0.0.9"), ip("172.16.87.21")]
        );

        let gw = gateway(1, &[(NETWORK_ADDRESS_TYPE, "udn-a")]);
        let served = snapshot(&gw, &["udn-a"]);
        let mut attached = report(V1, &["10.0.0.5", "10.100.0.7"], true);
        attached
            .networks
            .insert("udn-a".into(), "10.100.0.7".into());
        let mut detached = report(V1, &["10.0.0.6"], false);
        detached.listeners[0].endpoints.clear();
        let out = Case::new(gw, served, vec![attached, detached]).run();
        assert_eq!(
            addresses(&out.desired),
            [ip("10.100.0.7")],
            "an instance that binds nothing gives nothing"
        );
        assert!(out.programmed, "{}", out.desired);
    }

    fn service_of(service_type: &str, status: Value) -> Service {
        serde_json::from_value(json!({
            "metadata": {
                "name": "portail-gw",
                "namespace": "default",
                "labels": {"app.kubernetes.io/managed-by": "portail-controller"},
                "ownerReferences": [{
                    "apiVersion": "gateway.networking.k8s.io/v1",
                    "kind": "Gateway",
                    "name": "gw",
                    "uid": "gw-uid",
                    "controller": true,
                }],
            },
            "spec": {"type": service_type, "clusterIP": "10.96.0.10", "ports": [{"port": 8080}]},
            "status": status,
        }))
        .unwrap()
    }

    fn load_balancer(ingress: Value) -> Service {
        service_of(
            "LoadBalancer",
            json!({"loadBalancer": {"ingress": ingress}}),
        )
    }

    fn with_service(gw: Gateway, service: Service) -> GatewayStatus {
        let served = snapshot(&gw, &[]);
        let mut case = Case::new(gw, served, vec![report(V1, &["10.0.0.5"], true)]);
        case.service = Some(service);
        case.run()
    }

    /// A LoadBalancer Service's ingress IPs and hostnames are the
    /// addresses, sorted; pod IPs are not.
    #[test]
    fn a_load_balancer_gives_its_ingress() {
        let out = with_service(
            gateway(1, &[]),
            load_balancer(json!([{"ip": "203.0.113.7"}, {"hostname": "lb.example.com"}])),
        );
        assert!(out.programmed, "{}", out.desired);
        assert_eq!(
            addresses(&out.desired),
            [
                ("Hostname".to_string(), "lb.example.com".to_string()),
                ip("203.0.113.7")
            ]
        );
    }

    /// No ingress yet: AddressNotAssigned, and no address written.
    #[test]
    fn a_load_balancer_without_ingress_is_not_assigned() {
        let out = with_service(gateway(1, &[]), load_balancer(json!([])));
        let programmed = programmed_of(&out.desired);
        assert_eq!(programmed["reason"], "AddressNotAssigned");
        assert!(programmed["message"]
            .as_str()
            .unwrap()
            .contains("portail-gw"));
        assert!(out.desired.get("addresses").is_none());
        assert!(!out.programmed);
    }

    /// An IPAddress the spec names that the load balancer did not give:
    /// AddressNotAssigned, since static addresses are not requested.
    #[test]
    fn a_spec_address_absent_from_the_ingress_is_not_assigned() {
        let out = with_service(
            gateway(1, &[("IPAddress", "203.0.113.9")]),
            load_balancer(json!([{"ip": "203.0.113.7"}])),
        );
        let programmed = programmed_of(&out.desired);
        assert_eq!(programmed["reason"], "AddressNotAssigned");
        assert!(programmed["message"]
            .as_str()
            .unwrap()
            .contains("203.0.113.9"));
        assert_eq!(addresses(&out.desired), [ip("203.0.113.7")]);

        let held = with_service(
            gateway(1, &[("IPAddress", "203.0.113.7")]),
            load_balancer(json!([{"ip": "203.0.113.7"}])),
        );
        assert!(held.programmed, "{}", held.desired);
    }

    /// A ClusterIP or NodePort Service: its cluster IP.
    #[test]
    fn a_cluster_ip_service_gives_its_cluster_ip() {
        for service_type in ["ClusterIP", "NodePort"] {
            let out = with_service(gateway(1, &[]), service_of(service_type, json!({})));
            assert!(out.programmed, "{service_type}");
            assert_eq!(
                addresses(&out.desired),
                [ip("10.96.0.10")],
                "{service_type}"
            );
        }
    }

    /// A Gateway not accepted, or whose data plane cannot be provisioned,
    /// is not Programmed whatever its data planes bound.
    #[test]
    fn not_accepted_or_not_provisioned_is_invalid() {
        let gw = gateway(1, &[]);
        let served = snapshot(&gw, &[]);
        let mut case = Case::new(gw, served, vec![report(V1, &["10.0.0.5"], true)]);
        case.accepted = GatewayCondition {
            ok: false,
            reason: "UnsupportedAddress".into(),
            message: "Unsupported address type".into(),
        };
        let out = case.run();
        assert_eq!(programmed_of(&out.desired)["reason"], "Invalid");
        assert!(!out.programmed);

        case.accepted = GatewayCondition::ok("Accepted", "ok");
        case.problem =
            Some("name conflict: Service/portail-gw is not owned by this Gateway".into());
        let out = case.run();
        let programmed = programmed_of(&out.desired);
        assert_eq!(programmed["reason"], "Invalid");
        assert!(programmed["message"]
            .as_str()
            .unwrap()
            .contains("name conflict"));
    }

    /// Parameters that cannot be used published nothing: Accepted says so,
    /// and the listeners and addresses stay as written for the translation
    /// that was published; no route status is due.
    #[test]
    fn invalid_parameters_keep_the_published_listeners() {
        let first = gateway(1, &[]);
        let before = Case::new(
            first.clone(),
            snapshot(&first, &[]),
            vec![report(V1, &["10.0.0.5"], true)],
        )
        .run();
        let mut edited = written(&first, &before.desired);
        edited.metadata.generation = Some(2);
        edited.spec.listeners[0].port = 9090;
        let mut case = Case::new(
            edited.clone(),
            snapshot(&first, &[]),
            vec![report(V1, &["10.0.0.5"], true)],
        );
        case.unpublished = Some(Unpublished::InvalidParameters(
            "GatewayParameters default/missing not found".into(),
        ));
        let out = case.run();
        assert!(
            !out.routes,
            "no route status from an unpublished translation"
        );
        assert!(!out.programmed);
        let accepted = condition_of(&out.desired["conditions"], "Accepted");
        assert_eq!(accepted["reason"], "InvalidParameters");
        assert_eq!(accepted["observedGeneration"], 2);
        assert_eq!(programmed_of(&out.desired)["status"], "False");
        assert_eq!(out.desired["listeners"], before.desired["listeners"]);
        assert_eq!(addresses(&out.desired), [ip("10.0.0.5")]);
    }

    /// The pass that publishes runs before any data plane applied: Pending
    /// is written, the report arrives, and the next pass writes Programmed.
    /// After an edit the written Programmed stands until the report of the
    /// new version, and lapses to Pending two holds after the publish.
    #[tokio::test]
    async fn production_order_reaches_programmed_on_the_second_pass() {
        let mut p = Pipeline::new();
        let gw = testkit::gateway("gw", 1);
        let key = testkit::key(&gw);
        apply(&mut p.cluster.gateways, gw.clone());

        let pass = |p: &Pipeline, gw: Gateway| {
            let ctx = p.ctx.clone();
            async move {
                match publish(&gw, &ctx).await {
                    Step::Done(action) => action,
                    Step::Continue(pass) => finish(&gw, &ctx, *pass).await.unwrap(),
                }
            }
        };
        let written = |p: &Pipeline| p.api.status_patch("gateways", "gw").unwrap();

        pass(&p, gw.clone()).await;
        let first = written(&p);
        assert_eq!(programmed_of(&first["status"])["reason"], "Pending");
        assert!(first["status"].get("addresses").is_none(), "{first}");
        let gw = testkit::with_gateway_status(gw, &first);
        apply(&mut p.cluster.gateways, gw.clone());

        report_applied(&mut p, &key).await;
        p.api.clear();
        pass(&p, gw.clone()).await;
        let second = written(&p);
        assert_eq!(
            programmed_of(&second["status"])["status"],
            "True",
            "{second}"
        );
        let ips = &p.ctx.reports.live(&key)[0].1.interface_ips;
        if let Some(first_ip) = ips.first() {
            assert_eq!(addresses(&second["status"]), [ip(first_ip)]);
        }
        let mut gw = testkit::with_gateway_status(gw, &second);
        gw.metadata.generation = Some(2);
        apply(&mut p.cluster.gateways, gw.clone());

        tokio::time::pause();
        p.api.clear();
        let action = pass(&p, gw.clone()).await;
        let edited = written(&p);
        let programmed = programmed_of(&edited["status"]);
        assert_eq!(programmed["status"], "True", "{edited}");
        assert_eq!(programmed["observedGeneration"], 1);
        assert_eq!(action, kube::runtime::controller::Action::requeue(KEEP));

        tokio::time::advance(KEEP).await;
        let gw = testkit::with_gateway_status(gw, &edited);
        apply(&mut p.cluster.gateways, gw.clone());
        p.api.clear();
        pass(&p, gw.clone()).await;
        let lapsed = written(&p);
        let programmed = programmed_of(&lapsed["status"]);
        assert_eq!(programmed["reason"], "Pending");
        assert_eq!(programmed["observedGeneration"], 2);
    }

    /// The data plane applies what the hub serves for `key` and reports.
    async fn report_applied(p: &mut Pipeline, key: &GatewayKey) {
        let snapshot = p.ctx.hub.current(key).unwrap();
        let report = p.applier.apply(&snapshot).await;
        p.ctx
            .reports
            .record(key, InstanceId("pod-a".into()), report);
    }

    /// A leaving instance (Recreate, a drain) leaves the written Programmed
    /// standing two holds for its replacement, then Pending.
    #[tokio::test]
    async fn a_leaving_instance_keeps_programmed_for_its_replacement() {
        let mut p = Pipeline::new();
        let gw = testkit::gateway("gw", 1);
        let key = testkit::key(&gw);
        apply(&mut p.cluster.gateways, gw.clone());
        p.pass(&gw).await.unwrap();
        let gw = testkit::with_gateway_status(gw, &p.api.status_patch("gateways", "gw").unwrap());
        apply(&mut p.cluster.gateways, gw.clone());

        let mut leaving = p.ctx.reports.live(&key)[0].1.clone();
        leaving.leaving = true;
        // Long after the publish: the window runs from the goodbye.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(40)).await;
        p.ctx
            .reports
            .record(&key, InstanceId("pipeline-pod".into()), leaving);
        let served = p.served(&gw).unwrap();
        let ctx = p.ctx.clone();
        let pass = || async {
            match publish(&gw, &ctx).await {
                Step::Done(action) => action,
                Step::Continue(pass) => finish(&gw, &ctx, *pass).await.unwrap(),
            }
        };
        p.api.clear();
        let action = pass().await;
        assert!(Arc::ptr_eq(&served, &p.served(&gw).unwrap()), "nothing new");
        assert!(
            p.api.status_patch("gateways", "gw").is_none(),
            "kept as written"
        );
        assert_eq!(action, kube::runtime::controller::Action::requeue(KEEP));

        tokio::time::advance(KEEP).await;
        pass().await;
        let lapsed = p.api.status_patch("gateways", "gw").unwrap();
        assert_eq!(programmed_of(&lapsed["status"])["reason"], "Pending");
    }

    /// The Gateway status is applied by this controller's field manager,
    /// conditional on the object the pass read.
    #[tokio::test]
    async fn the_gateway_status_write_is_ours_and_bound_to_its_read() {
        let mut p = Pipeline::new();
        let gw = testkit::gateway("gw", 1);
        apply(&mut p.cluster.gateways, gw.clone());
        p.pass(&gw).await.unwrap();
        let write = p
            .api
            .requests()
            .into_iter()
            .find(|r| r.method == "PATCH" && r.path.contains("/gateways/gw/status"))
            .unwrap();
        assert!(
            write.path.contains("fieldManager=portail-controller"),
            "{}",
            write.path
        );
        assert_eq!(write.body["metadata"]["uid"], "gw-uid");
        assert_eq!(write.body["metadata"]["resourceVersion"], "1");
    }

    /// A Service of the Gateway's name not made for it is not its address.
    #[tokio::test]
    async fn only_an_owned_service_fronts_the_gateway() {
        let mut cluster = testkit::TestCluster::default();
        let gw = gateway(1, &[]);
        let mut foreign = load_balancer(json!([{"ip": "203.0.113.7"}]));
        foreign.metadata.owner_references = None;
        apply(&mut cluster.services, foreign);
        let ctx = cluster.ctx(testkit::MockApi::default().client());
        assert!(owned_service(&ctx, &gw).is_none());

        apply(
            &mut cluster.services,
            load_balancer(json!([{"ip": "203.0.113.7"}])),
        );
        assert!(owned_service(&ctx, &gw).is_some());
        let mut recreated = gw.clone();
        recreated.metadata.uid = Some("gw-uid-2".into());
        assert!(
            owned_service(&ctx, &recreated).is_none(),
            "the old Gateway's"
        );
    }
}
