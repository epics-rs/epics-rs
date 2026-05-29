//! Beacon tracker.
//!
//! Tracks which `(server, guid)` incarnations have been seen so the search
//! engine can de-duplicate `Discovered` events and pace reconnect pokes.
//! Bounded by [`BEACON_TRACK_LIMIT`] and aged out by [`prune_stale`].
//!
//! Note: there is intentionally **no** per-server GUID-change suppression.
//! pvxs treats a GUID change as a `Change` and pokes pending searches
//! immediately, subject only to the engine's global 30-second
//! `pokeHoldoff` and one-active-revolution guard (client.cpp:805-847,
//! 736-759). The only overload protection at this layer is the
//! size cap, mirroring pvxs `beaconTrackLimit` (client.cpp:791-797).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::RwLock;

/// Hard cap on tracked (server, guid) entries. Mirrors pvxs
/// `beaconTrackLimit` (client.cpp commit 3f3e394 "Limit beaconTrack by
/// size as well as time"). Without it, an attacker spoofing beacons
/// with arbitrary GUIDs can grow the map unbounded; with it, the new
/// entry is dropped once the cap is reached. Stale entries are still
/// reaped by `prune_stale`.
const BEACON_TRACK_LIMIT: usize = 20_000;

#[derive(Debug, Clone)]
struct ServerEntry {
    guid: [u8; 12],
    last_seen: Instant,
}

#[derive(Default)]
pub struct BeaconTracker {
    inner: RwLock<HashMap<SocketAddr, ServerEntry>>,
    /// One-shot latch: warn loudly the first time the cap-and-drop
    /// path rejects a brand-new server. Repeated cap hits would
    /// otherwise spam the log without adding info — the operator
    /// only needs to learn the cap was reached once.
    warned_at_cap: AtomicBool,
}

impl BeaconTracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Record an observed beacon.
    ///
    /// Returns `true` if this beacon may trigger a reconnect poke, `false`
    /// only when the new entry was cap-dropped. A GUID change (server
    /// restart) is allowed immediately — pvxs treats it as a `Change` and
    /// pokes at once, with pacing handled by the engine's global
    /// `pokeHoldoff`, not by a per-server suppression window
    /// (client.cpp:805-847).
    pub fn observe(&self, server: SocketAddr, guid: [u8; 12]) -> bool {
        let mut map = self.inner.write();
        let now = Instant::now();
        match map.get_mut(&server) {
            None => {
                // Cap-and-drop: if we'd exceed the limit, refuse the new
                // entry rather than evict an existing one. Returning
                // `false` suppresses the would-be reconnect; the next
                // `prune_stale` cycle frees space as old beacons age out.
                if map.len() >= BEACON_TRACK_LIMIT {
                    if !self.warned_at_cap.swap(true, Ordering::Relaxed) {
                        tracing::warn!(
                            cap = BEACON_TRACK_LIMIT,
                            "beacon tracker cap reached — new servers temporarily \
                             ignored until existing entries age out (180s). Further \
                             cap hits will log at debug only."
                        );
                    } else {
                        tracing::debug!(
                            server = %server,
                            "beacon tracker cap-drop"
                        );
                    }
                    return false;
                }
                map.insert(server, ServerEntry { guid, last_seen: now });
                true
            }
            Some(entry) => {
                entry.last_seen = now;
                // Same incarnation or a restart (new GUID) both pass
                // through; the engine's `first_announce` gate + global
                // pokeHoldoff decide whether a poke actually fires.
                entry.guid = guid;
                true
            }
        }
    }

    /// Most recent GUID observed for `server`, or `None` if we
    /// haven't seen a beacon from it yet. Used by Channel reconnect
    /// to detect server replacement at the same address.
    pub fn guid_for(&self, server: SocketAddr) -> Option<[u8; 12]> {
        self.inner.read().get(&server).map(|e| e.guid)
    }

    /// Forget a server (called when we explicitly disconnect & don't intend
    /// to reconnect).
    pub fn forget(&self, server: SocketAddr) {
        self.inner.write().remove(&server);
    }

    /// Drop entries whose last beacon is older than `max_age`. Returns the
    /// list of (server, guid) tuples that were pruned so the caller can
    /// raise a `Discovered::Timeout` for each. Mirrors pvxs
    /// `tickBeaconClean` (client.cpp:1254) which prunes after 2× the
    /// beacon-clean interval (default 360s).
    pub fn prune_stale(&self, max_age: Duration) -> Vec<(SocketAddr, [u8; 12])> {
        let now = Instant::now();
        let mut map = self.inner.write();
        let mut pruned = Vec::new();
        map.retain(|server, entry| {
            if now.duration_since(entry.last_seen) > max_age {
                pruned.push((*server, entry.guid));
                false
            } else {
                true
            }
        });
        pruned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn addr() -> SocketAddr {
        SocketAddr::new(Ipv4Addr::new(127, 0, 0, 1).into(), 5075)
    }

    #[test]
    fn first_observation_passes_through() {
        let t = BeaconTracker::new();
        assert!(t.observe(addr(), [1u8; 12]));
    }

    #[test]
    fn same_guid_repeats_pass_through() {
        let t = BeaconTracker::new();
        assert!(t.observe(addr(), [1u8; 12]));
        assert!(t.observe(addr(), [1u8; 12]));
        assert!(t.observe(addr(), [1u8; 12]));
    }

    /// A GUID change (server restart) is NOT suppressed: pvxs pokes
    /// immediately on a `Change` (client.cpp:805-847), so `observe` keeps
    /// returning `true` and updates the tracked GUID. Pacing is the
    /// engine's global pokeHoldoff, not a per-server window.
    #[test]
    fn guid_change_allows_reconnect() {
        let t = BeaconTracker::new();
        assert!(t.observe(addr(), [1u8; 12]));
        assert!(
            t.observe(addr(), [2u8; 12]),
            "a GUID change must not be throttled — pvxs pokes at once"
        );
        assert_eq!(
            t.guid_for(addr()),
            Some([2u8; 12]),
            "the tracker adopts the new incarnation's GUID"
        );
    }

    #[test]
    fn forget_clears_state() {
        let t = BeaconTracker::new();
        t.observe(addr(), [1u8; 12]);
        t.forget(addr());
        assert!(t.guid_for(addr()).is_none());
    }

    /// Stale entries — last_seen older than `max_age` — are pruned and
    /// returned so the caller can fire `Discovered::Timeout`. Mirrors
    /// pvxs `tickBeaconClean` (client.cpp:1254).
    #[test]
    fn prune_stale_returns_aged_out_entries() {
        let t = BeaconTracker::new();
        t.observe(addr(), [9u8; 12]);
        // Immediate prune with a far-future age cutoff drops nothing.
        let pruned = t.prune_stale(Duration::from_secs(3600));
        assert!(pruned.is_empty());
        // Negative-ish (zero) cutoff drops everything currently tracked.
        let pruned = t.prune_stale(Duration::from_secs(0));
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].0, addr());
        assert_eq!(pruned[0].1, [9u8; 12]);
        // Idempotent: a second call with no entries left returns empty.
        assert!(t.prune_stale(Duration::from_secs(0)).is_empty());
    }

    #[test]
    fn cap_drops_new_entries_after_limit() {
        let t = BeaconTracker::new();
        // Fill the tracker up to the cap with distinct (server, guid) pairs.
        for i in 0..BEACON_TRACK_LIMIT as u32 {
            let octets = i.to_be_bytes();
            let sa: SocketAddr = SocketAddr::new(
                std::net::Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3]).into(),
                5075,
            );
            assert!(t.observe(sa, [0u8; 12]));
        }
        // Next insertion is refused — function returns false and the
        // map size stays at the cap.
        let extra: SocketAddr =
            SocketAddr::new(std::net::Ipv4Addr::new(255, 255, 255, 254).into(), 5075);
        assert!(!t.observe(extra, [1u8; 12]));
        assert_eq!(t.inner.read().len(), BEACON_TRACK_LIMIT);
    }
}
