//! Service externalIPs for Managed Gateways: which of a Gateway's IPAddress
//! values its Service carries, and why the others are not.
//!
//! An address is assigned only inside its GatewayClass's allowlist, and to
//! one Gateway: the oldest of those that ask for it and are allowed it
//! (creationTimestamp, then namespace and name, the Gateway API's conflict
//! order). Two Services with one externalIP and port are undefined in
//! kube-proxy, and a tenant could take another's traffic.
//!
//! A Gateway's pass reads the other Gateways' specs, which wake only their
//! own Gateway: [`Claims`] records what each one asked for and wakes the
//! others asking for an address it took up or let go.

use std::collections::{BTreeSet, HashMap};
use std::net::IpAddr;
use std::sync::Mutex;

use futures::channel::mpsc;
use gateway_api::gateways::Gateway;
use kube::ResourceExt;

use super::params::{self, Params};
use super::provision;
use super::reconcile::{class_owner, ClassOwner};
use super::ControllerCtx;
use crate::kubernetes::crds::{Provisioning, ServiceType};
use crate::sync::GatewayKey;

const IP_ADDRESS: &str = "IPAddress";

/// The pass's verdict on each IPAddress of a Gateway's spec.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Assignment {
    /// On its Service's externalIPs, sorted.
    pub assigned: Vec<IpAddr>,
    /// The others, each with why: AddressNotUsable.
    pub refused: Vec<(String, String)>,
}

impl Assignment {
    /// Why the Gateway's refused addresses are not usable, if any are.
    pub(crate) fn not_usable(&self) -> Option<String> {
        (!self.refused.is_empty()).then(|| {
            self.refused
                .iter()
                .map(|(ip, why)| format!("IPAddress {ip} {why}"))
                .collect::<Vec<_>>()
                .join("; ")
        })
    }
}

/// The IPAddress values of `gateway`'s spec, as written.
fn requested(gateway: &Gateway) -> Vec<&str> {
    gateway
        .spec
        .addresses
        .iter()
        .flatten()
        .filter(|a| a.r#type.as_deref().unwrap_or(IP_ADDRESS) == IP_ADDRESS)
        .filter_map(|a| a.value.as_deref())
        .collect()
}

/// Whether `params` put `ip` on a Service of this controller's.
fn allows(params: &Params, ip: IpAddr) -> bool {
    params.provisioning == Provisioning::Managed
        && params.service_type != ServiceType::None
        && params.external_ip_cidrs.iter().any(|c| c.contains(ip))
}

/// The conflict order: the smallest holds.
fn order(gateway: &Gateway) -> impl Ord {
    (
        gateway.metadata.creation_timestamp.clone(),
        gateway.namespace().unwrap_or_default(),
        gateway.name_any(),
    )
}

/// The verdict on `gateway`'s IPAddress values under `params`, and the
/// addresses it asks for and is allowed, for [`Claims::record`]. A Gateway
/// with no Service carries none: its addresses are its pods' to hold.
pub(crate) fn assign(
    ctx: &ControllerCtx,
    gateway: &Gateway,
    params: &Params,
) -> (Assignment, BTreeSet<IpAddr>) {
    let mut assignment = Assignment::default();
    let mut asked = BTreeSet::new();
    if params.provisioning != Provisioning::Managed || params.service_type == ServiceType::None {
        return (assignment, asked);
    }
    let me = GatewayKey::new(gateway.namespace().unwrap_or_default(), gateway.name_any());
    let gateways = ctx.cache.gateways.state();
    let classes = ctx.cache.gateway_classes.state();
    // A load balancer that assigns a requested address anyway holds it:
    // nothing to ask, nothing to refuse.
    let ingress: Vec<String> = provision::owned_service(ctx, gateway)
        .iter()
        .flat_map(|s| s.status.iter())
        .flat_map(|s| s.load_balancer.iter())
        .flat_map(|lb| lb.ingress.iter().flatten())
        .filter_map(|i| i.ip.clone())
        .collect();
    for value in requested(gateway) {
        let Ok(ip) = value.parse::<IpAddr>() else {
            // validate_gateway refuses it as UnsupportedAddress.
            continue;
        };
        // The list matches ::ffff:a.b.c.d as IPv4 and kube-proxy reads it so,
        // where a holder written as IPv4 would not be found: two Gateways
        // would carry one address.
        if ip != ip.to_canonical() {
            assignment.refused.push((
                value.to_string(),
                "is an IPv4-mapped IPv6 address: write it as IPv4".to_string(),
            ));
            continue;
        }
        if ingress.iter().any(|held| held.parse() == Ok(ip)) {
            continue;
        }
        if !allows(params, ip) {
            assignment.refused.push((
                value.to_string(),
                "is not in the GatewayClass's externalIPs allowlist".to_string(),
            ));
            continue;
        }
        asked.insert(ip);
        let holder = gateways
            .iter()
            .filter(|other| {
                let key = GatewayKey::new(other.namespace().unwrap_or_default(), other.name_any());
                key != me
                    && requested(other).iter().any(|v| v.parse() == Ok(ip))
                    && matches!(
                        class_owner(
                            &classes,
                            &other.spec.gateway_class_name,
                            &ctx.controller_name
                        ),
                        ClassOwner::Ours
                    )
                    && match params::resolve(ctx, other) {
                        Ok(p) => allows(&p, ip),
                        // Invalid parameters leave its objects as they are.
                        Err(_) => provision::owned_service(ctx, other).is_some_and(|s| {
                            s.spec
                                .iter()
                                .flat_map(|spec| spec.external_ips.iter().flatten())
                                .any(|v| v.parse() == Ok(ip))
                        }),
                    }
            })
            .min_by_key(|other| order(other))
            .filter(|other| order(other) < order(gateway));
        match holder {
            None => assignment.assigned.push(ip),
            Some(holder) if holder.namespace() == gateway.namespace() => assignment.refused.push((
                value.to_string(),
                format!("is in use by Gateway {}", holder.name_any()),
            )),
            Some(_) => assignment.refused.push((
                value.to_string(),
                "is in use by another Gateway".to_string(),
            )),
        }
    }
    assignment.assigned.sort();
    assignment.assigned.dedup();
    (assignment, asked)
}

/// What each Gateway's last pass asked for and was allowed.
pub(crate) struct Claims {
    asked: Mutex<HashMap<GatewayKey, BTreeSet<IpAddr>>>,
    wakes: mpsc::UnboundedSender<GatewayKey>,
    receiver: Mutex<Option<mpsc::UnboundedReceiver<GatewayKey>>>,
}

impl Default for Claims {
    fn default() -> Self {
        let (wakes, receiver) = mpsc::unbounded();
        Self {
            asked: Mutex::default(),
            wakes,
            receiver: Mutex::new(Some(receiver)),
        }
    }
}

impl Claims {
    fn asked(&self) -> std::sync::MutexGuard<'_, HashMap<GatewayKey, BTreeSet<IpAddr>>> {
        self.asked.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// `gateway` asks for `now`: the others asking for an address it took
    /// up or let go may hold it or lose it.
    pub(crate) fn record(&self, gateway: &GatewayKey, now: BTreeSet<IpAddr>) {
        let mut asked = self.asked();
        let before = asked.get(gateway).cloned().unwrap_or_default();
        let moved: BTreeSet<IpAddr> = before.symmetric_difference(&now).copied().collect();
        if now.is_empty() {
            asked.remove(gateway);
        } else {
            asked.insert(gateway.clone(), now);
        }
        self.wake(&asked, gateway, &moved);
    }

    /// Forget the Gateways `live` rejects, waking those that asked for
    /// what they held.
    pub(crate) fn retain(&self, live: impl Fn(&GatewayKey) -> bool) {
        let mut asked = self.asked();
        let gone: Vec<(GatewayKey, BTreeSet<IpAddr>)> = asked
            .iter()
            .filter(|(key, _)| !live(key))
            .map(|(key, ips)| (key.clone(), ips.clone()))
            .collect();
        for (key, ips) in gone {
            asked.remove(&key);
            self.wake(&asked, &key, &ips);
        }
    }

    fn wake(
        &self,
        asked: &HashMap<GatewayKey, BTreeSet<IpAddr>>,
        gateway: &GatewayKey,
        moved: &BTreeSet<IpAddr>,
    ) {
        if moved.is_empty() {
            return;
        }
        for (other, ips) in asked {
            if other != gateway && !ips.is_disjoint(moved) {
                let _ = self.wakes.unbounded_send(other.clone());
            }
        }
    }

    /// Every Gateway to reconcile again for its addresses. Taken once.
    pub(crate) fn wakes(&self) -> Option<mpsc::UnboundedReceiver<GatewayKey>> {
        self.receiver
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::{self, apply, MockApi, TestCluster, CONTROLLER};
    use super::*;
    use crate::kubernetes::crds::GatewayParameters;
    use gateway_api::gatewayclasses::GatewayClassParametersRef;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;

    /// A Managed class whose Gateways may ask for 192.0.2.0/28.
    fn cluster() -> TestCluster {
        let mut cluster = TestCluster::default();
        let mut params = GatewayParameters::new(
            "defaults",
            serde_yaml::from_str(
                "service: {type: ClusterIP, externalIPs: {allowedCIDRs: [192.0.2.0/28]}}",
            )
            .unwrap(),
        );
        params.metadata.namespace = Some("portail-system".into());
        apply(&mut cluster.parameters, params);
        let mut class = testkit::gateway_class("portail", CONTROLLER);
        class.spec.parameters_ref = Some(GatewayClassParametersRef {
            group: "portail.epheo.eu".into(),
            kind: "GatewayParameters".into(),
            name: "defaults".into(),
            namespace: Some("portail-system".into()),
        });
        apply(&mut cluster.gateway_classes, class);
        cluster
    }

    /// Gateway `name` of `ns`, created at `second`, asking for `ips`.
    fn asking(ns: &str, name: &str, second: i64, ips: &[&str]) -> Gateway {
        let mut gw = testkit::gateway(name, 1);
        gw.metadata.namespace = Some(ns.into());
        gw.metadata.creation_timestamp = Some(Time(
            k8s_openapi::jiff::Timestamp::from_second(second).unwrap(),
        ));
        gw.spec.addresses = Some(
            ips.iter()
                .map(|ip| {
                    serde_json::from_value(serde_json::json!({"type": "IPAddress", "value": ip}))
                        .unwrap()
                })
                .collect(),
        );
        gw
    }

    /// One address, one Gateway: the oldest asking holds it, a newer one
    /// learns its holder's name only in its own namespace, and an address
    /// outside the list is refused whoever asks.
    #[tokio::test]
    async fn the_oldest_gateway_asking_holds_an_address() {
        let mut cluster = cluster();
        let older = asking("a", "first", 100, &["192.0.2.5"]);
        let newer = asking("b", "second", 200, &["192.0.2.5", "198.51.100.1"]);
        let neighbour = asking("a", "third", 300, &["192.0.2.5"]);
        for gw in [&older, &newer, &neighbour] {
            apply(&mut cluster.gateways, gw.clone());
        }
        let ctx = cluster.ctx(MockApi::default().client());
        let assigned = |gw: &Gateway| {
            let params = params::resolve(&ctx, gw).unwrap();
            assign(&ctx, gw, &params)
        };

        let (first, asked) = assigned(&older);
        assert_eq!(first.assigned, ["192.0.2.5".parse::<IpAddr>().unwrap()]);
        assert!(first.refused.is_empty());
        assert_eq!(asked, BTreeSet::from(["192.0.2.5".parse().unwrap()]));

        let (second, asked) = assigned(&newer);
        assert!(second.assigned.is_empty());
        assert_eq!(
            second.refused,
            [
                (
                    "192.0.2.5".to_string(),
                    "is in use by another Gateway".to_string()
                ),
                (
                    "198.51.100.1".to_string(),
                    "is not in the GatewayClass's externalIPs allowlist".to_string()
                ),
            ]
        );
        assert_eq!(asked, BTreeSet::from(["192.0.2.5".parse().unwrap()]));

        let (third, _) = assigned(&neighbour);
        assert_eq!(
            third.refused,
            [(
                "192.0.2.5".to_string(),
                "is in use by Gateway first".to_string()
            )]
        );
    }

    /// An address written as IPv4-mapped IPv6 is refused before the list,
    /// which would match it as the IPv4 an older Gateway holds.
    #[tokio::test]
    async fn a_mapped_address_cannot_take_a_held_one() {
        let mut cluster = cluster();
        let holder = asking("a", "first", 100, &["192.0.2.5"]);
        let mapped = asking("b", "second", 200, &["::ffff:192.0.2.5"]);
        for gw in [&holder, &mapped] {
            apply(&mut cluster.gateways, gw.clone());
        }
        let ctx = cluster.ctx(MockApi::default().client());
        let assigned = |gw: &Gateway| {
            let params = params::resolve(&ctx, gw).unwrap();
            assign(&ctx, gw, &params)
        };
        let (second, asked) = assigned(&mapped);
        assert!(second.assigned.is_empty());
        assert_eq!(
            second.refused,
            [(
                "::ffff:192.0.2.5".to_string(),
                "is an IPv4-mapped IPv6 address: write it as IPv4".to_string()
            )]
        );
        assert!(asked.is_empty(), "nothing claimed");
        let (first, _) = assigned(&holder);
        assert_eq!(first.assigned, ["192.0.2.5".parse::<IpAddr>().unwrap()]);
    }

    fn ips(list: &[&str]) -> BTreeSet<IpAddr> {
        list.iter().map(|s| s.parse().unwrap()).collect()
    }

    /// A Gateway taking up or letting go of an address wakes the others
    /// asking for it, and only them.
    #[test]
    fn a_moved_claim_wakes_the_others_asking() {
        let claims = Claims::default();
        let mut wakes = claims.wakes().unwrap();
        let (a, b, c) = (
            GatewayKey::new("ns", "a"),
            GatewayKey::new("other", "b"),
            GatewayKey::new("ns", "c"),
        );
        claims.record(&b, ips(&["192.0.2.10"]));
        claims.record(&c, ips(&["192.0.2.20"]));
        assert!(wakes.try_recv().is_err(), "nobody else asked yet");

        claims.record(&a, ips(&["192.0.2.10"]));
        assert_eq!(wakes.try_recv().unwrap(), b.clone());
        assert!(wakes.try_recv().is_err(), "c asks for another address");

        claims.record(&a, ips(&["192.0.2.10"]));
        assert!(wakes.try_recv().is_err(), "nothing moved");

        claims.retain(|key| key != &a);
        assert_eq!(wakes.try_recv().unwrap(), b);
        assert!(wakes.try_recv().is_err());
    }
}
