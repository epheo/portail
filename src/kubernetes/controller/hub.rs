//! The snapshots the controller serves, one per Gateway it owns. A
//! reconcile publishes; the sync server answers a poll from the current
//! snapshot, or holds it on the Gateway's channel until one with another
//! version is published.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::watch;
use tokio::time::Instant;

use crate::sync::{GatewayKey, Snapshot};

/// A Gateway's published snapshots.
struct Slot {
    current: watch::Sender<Option<Arc<Snapshot>>>,
    /// What `current` replaced: data planes that have not applied the
    /// current version yet serve this one.
    previous: Option<Arc<Snapshot>>,
    /// When `current` was published.
    published: Option<Instant>,
    /// When this controller first published the Gateway: its leadership
    /// began, or the Gateway became its.
    first_published: Option<Instant>,
}

impl Slot {
    fn new() -> Self {
        Self {
            current: watch::Sender::new(None),
            previous: None,
            published: None,
            first_published: None,
        }
    }
}

#[derive(Default)]
pub(crate) struct Hub {
    slots: Mutex<HashMap<GatewayKey, Slot>>,
}

impl Hub {
    fn slots(&self) -> MutexGuard<'_, HashMap<GatewayKey, Slot>> {
        self.slots.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Serve `snapshot` for its Gateway. Returns whether its version is new:
    /// an unchanged translation wakes no poll and keeps the current `Arc`.
    pub(crate) fn publish(&self, snapshot: Snapshot) -> bool {
        let mut slots = self.slots();
        let slot = slots
            .entry(snapshot.gateway.key())
            .or_insert_with(Slot::new);
        if slot
            .current
            .borrow()
            .as_ref()
            .is_some_and(|current| current.version == snapshot.version)
        {
            return false;
        }
        slot.previous = slot.current.send_replace(Some(Arc::new(snapshot)));
        let now = Instant::now();
        slot.published = Some(now);
        slot.first_published.get_or_insert(now);
        true
    }

    /// Stop serving `key`: its held polls end (their channel closes) and
    /// later ones are the server's 404. Returns whether it was served.
    pub(crate) fn remove(&self, key: &GatewayKey) -> bool {
        self.slots().remove(key).is_some()
    }

    /// Stop serving every Gateway `live` rejects.
    pub(crate) fn retain(&self, live: impl Fn(&GatewayKey) -> bool) {
        self.slots().retain(|key, _| live(key));
    }

    /// The Gateway's channel, `None` until its first publish. Creates the
    /// slot: subscribe only to a Gateway known to be ours, or a poll for one
    /// that never comes holds to expiry instead of answering 404.
    pub(crate) fn subscribe(&self, key: &GatewayKey) -> watch::Receiver<Option<Arc<Snapshot>>> {
        self.slots()
            .entry(key.clone())
            .or_insert_with(Slot::new)
            .current
            .subscribe()
    }

    pub(crate) fn current(&self, key: &GatewayKey) -> Option<Arc<Snapshot>> {
        self.slots().get(key)?.current.borrow().clone()
    }

    /// When the current snapshot of `key` was published.
    pub(crate) fn published_at(&self, key: &GatewayKey) -> Option<Instant> {
        self.slots().get(key)?.published
    }

    /// When `key` was first published, by this controller.
    pub(crate) fn first_published_at(&self, key: &GatewayKey) -> Option<Instant> {
        self.slots().get(key)?.first_published
    }

    /// The Gateway generation `version` was translated from, while it is
    /// the current or the previous snapshot.
    pub(crate) fn generation_of(&self, key: &GatewayKey, version: &str) -> Option<i64> {
        let slots = self.slots();
        let slot = slots.get(key)?;
        let current = slot.current.borrow();
        current
            .iter()
            .chain(slot.previous.iter())
            .find(|s| s.version == version)
            .map(|s| s.gateway.generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PortailConfig;
    use crate::sync::GatewayRef;

    fn snapshot(name: &str, generation: i64) -> Snapshot {
        let gateway = GatewayRef {
            namespace: "default".into(),
            name: name.into(),
            uid: format!("{name}-uid"),
            generation,
        };
        Snapshot::from_config(gateway, PortailConfig::default(), vec![])
    }

    #[tokio::test]
    async fn a_publish_wakes_held_polls_only_when_the_version_moves() {
        let hub = Hub::default();
        let key = GatewayKey::new("default", "gw");
        let mut poll = hub.subscribe(&key);
        assert!(poll.borrow_and_update().is_none(), "nothing published yet");

        assert!(hub.publish(snapshot("gw", 1)));
        assert!(poll.has_changed().unwrap());
        let first = poll.borrow_and_update().clone().unwrap();

        assert!(!hub.publish(snapshot("gw", 1)), "same version");
        assert!(!poll.has_changed().unwrap());
        assert!(Arc::ptr_eq(&first, &hub.current(&key).unwrap()));

        assert!(hub.publish(snapshot("gw", 2)));
        assert!(poll.has_changed().unwrap());
    }

    #[tokio::test]
    async fn a_removed_gateway_ends_its_polls() {
        let hub = Hub::default();
        let key = GatewayKey::new("default", "gw");
        hub.publish(snapshot("gw", 1));
        let mut poll = hub.subscribe(&key);

        assert!(hub.remove(&key));
        assert!(poll.changed().await.is_err(), "the channel closed");
        assert!(hub.current(&key).is_none());
        assert!(!hub.remove(&key));
    }

    #[test]
    fn the_previous_version_keeps_its_generation() {
        let hub = Hub::default();
        let key = GatewayKey::new("default", "gw");
        let (one, two, three) = (snapshot("gw", 1), snapshot("gw", 2), snapshot("gw", 3));
        let (v1, v2, v3) = (
            one.version.clone(),
            two.version.clone(),
            three.version.clone(),
        );
        hub.publish(one);
        hub.publish(two);
        assert_eq!(hub.generation_of(&key, &v2), Some(2));
        assert_eq!(hub.generation_of(&key, &v1), Some(1));
        hub.publish(three);
        assert_eq!(hub.generation_of(&key, &v3), Some(3));
        assert_eq!(hub.generation_of(&key, &v1), None, "two versions back");
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_version_moves_the_publish_time() {
        let hub = Hub::default();
        let key = GatewayKey::new("default", "gw");
        assert_eq!(hub.published_at(&key), None);
        hub.publish(snapshot("gw", 1));
        let first = hub.published_at(&key).unwrap();
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        hub.publish(snapshot("gw", 1));
        assert_eq!(hub.published_at(&key), Some(first), "same version");
        hub.publish(snapshot("gw", 2));
        assert!(hub.published_at(&key).unwrap() > first);
        assert_eq!(hub.first_published_at(&key), Some(first));
    }

    #[test]
    fn retain_drops_the_rest() {
        let hub = Hub::default();
        hub.publish(snapshot("a", 1));
        hub.publish(snapshot("b", 1));
        hub.retain(|key| key.name == "a");
        assert!(hub.current(&GatewayKey::new("default", "a")).is_some());
        assert!(hub.current(&GatewayKey::new("default", "b")).is_none());
    }
}
