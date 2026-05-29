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
    /// pvxs `BeaconInfo::peerVersion` (client.cpp:807): the PVA message
    /// header version of the last beacon. A change in *either* GUID or
    /// peerVersion is a `Change`.
    peer_version: u8,
    last_seen: Instant,
}

/// Beacon identity key. pvxs keys `beaconTrack` by `(server, proto)`
/// (client.cpp:780-782), so a server advertising both `tcp` and `tls` for
/// the same endpoint is two discovery identities, not one collapsed entry.
type BeaconKey = (SocketAddr, String);

/// Classification of an observed beacon, mirroring pvxs onBeacon's
/// `New` / `Change` / `Update` decision (client.cpp:784-847). The engine
/// uses this to drive `Discovered` emission and reconnect pokes; it is the
/// single owner of beacon-identity de-duplication (there is no separate
/// "already announced" set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeaconAction {
    /// First beacon for this `(server, proto)` — emit `Online`, poke.
    New,
    /// Known `(server, proto)` reported a different GUID or peerVersion —
    /// emit `Timeout` for `old_guid` then `Online` for the new identity,
    /// and poke (client.cpp:807-821).
    Changed { old_guid: [u8; 12] },
    /// Same identity (same GUID and peerVersion) — no event, no poke.
    Update,
    /// New entry refused because the tracker is at its size cap.
    CapDropped,
}

#[derive(Default)]
pub struct BeaconTracker {
    inner: RwLock<HashMap<BeaconKey, ServerEntry>>,
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

    /// Record an observed beacon and classify it. pvxs keys by
    /// `(server, proto)` and treats a change in GUID *or* peerVersion as a
    /// `Change` (client.cpp:780-808). A GUID/version change is reported
    /// immediately — there is no per-server suppression window; pacing is
    /// the engine's global `pokeHoldoff`.
    pub fn observe(
        &self,
        server: SocketAddr,
        proto: &str,
        guid: [u8; 12],
        peer_version: u8,
    ) -> BeaconAction {
        let mut map = self.inner.write();
        let now = Instant::now();
        let key: BeaconKey = (server, proto.to_owned());
        match map.get_mut(&key) {
            None => {
                // Cap-and-drop: if we'd exceed the limit, refuse the new
                // entry rather than evict an existing one. The next
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
                    return BeaconAction::CapDropped;
                }
                map.insert(
                    key,
                    ServerEntry {
                        guid,
                        peer_version,
                        last_seen: now,
                    },
                );
                BeaconAction::New
            }
            Some(entry) => {
                entry.last_seen = now;
                if entry.guid != guid || entry.peer_version != peer_version {
                    let old_guid = entry.guid;
                    entry.guid = guid;
                    entry.peer_version = peer_version;
                    BeaconAction::Changed { old_guid }
                } else {
                    BeaconAction::Update
                }
            }
        }
    }

    /// Most recent GUID observed for `server` on any protocol, or `None`
    /// if we haven't seen a beacon from it yet. Used by Channel reconnect
    /// to detect server replacement at the same address; the GUID is the
    /// same across protocols for one server incarnation.
    pub fn guid_for(&self, server: SocketAddr) -> Option<[u8; 12]> {
        self.inner
            .read()
            .iter()
            .find(|((sa, _proto), _)| *sa == server)
            .map(|(_, e)| e.guid)
    }

    /// Forget a server on every protocol (called when we explicitly
    /// disconnect & don't intend to reconnect).
    pub fn forget(&self, server: SocketAddr) {
        self.inner.write().retain(|(sa, _proto), _| *sa != server);
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
        map.retain(|(server, _proto), entry| {
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

    // Default peer version used by tests where the version is irrelevant.
    const V: u8 = 2;

    #[test]
    fn first_observation_is_new() {
        let t = BeaconTracker::new();
        assert_eq!(t.observe(addr(), "tcp", [1u8; 12], V), BeaconAction::New);
    }

    #[test]
    fn same_identity_repeats_are_update() {
        let t = BeaconTracker::new();
        assert_eq!(t.observe(addr(), "tcp", [1u8; 12], V), BeaconAction::New);
        assert_eq!(t.observe(addr(), "tcp", [1u8; 12], V), BeaconAction::Update);
        assert_eq!(t.observe(addr(), "tcp", [1u8; 12], V), BeaconAction::Update);
    }

    /// pvxs keys beacon tracking by `(server, proto)` (client.cpp:780-782):
    /// the same endpoint/GUID advertised on `tcp` and `tls` is two
    /// discovery identities, each a fresh `New`, not one collapsed entry.
    #[test]
    fn distinct_protocols_are_distinct_identities() {
        let t = BeaconTracker::new();
        assert_eq!(t.observe(addr(), "tcp", [1u8; 12], V), BeaconAction::New);
        assert_eq!(
            t.observe(addr(), "tls", [1u8; 12], V),
            BeaconAction::New,
            "tls is a separate identity from tcp for the same server/GUID"
        );
        assert_eq!(t.observe(addr(), "tcp", [1u8; 12], V), BeaconAction::Update);
    }

    /// A GUID change (server restart) is a `Change` reporting the old GUID
    /// so the caller can emit Timeout(old)+Online(new); pvxs pokes at once
    /// (client.cpp:805-847), with pacing left to the global pokeHoldoff.
    #[test]
    fn guid_change_is_a_change() {
        let t = BeaconTracker::new();
        assert_eq!(t.observe(addr(), "tcp", [1u8; 12], V), BeaconAction::New);
        assert_eq!(
            t.observe(addr(), "tcp", [2u8; 12], V),
            BeaconAction::Changed {
                old_guid: [1u8; 12]
            }
        );
        assert_eq!(t.guid_for(addr()), Some([2u8; 12]));
    }

    /// pvxs classifies a peerVersion change as a `Change` even when the
    /// GUID is unchanged (client.cpp:807): the version field participates
    /// in identity.
    #[test]
    fn peer_version_change_is_a_change() {
        let t = BeaconTracker::new();
        assert_eq!(t.observe(addr(), "tcp", [1u8; 12], 2), BeaconAction::New);
        assert_eq!(
            t.observe(addr(), "tcp", [1u8; 12], 3),
            BeaconAction::Changed {
                old_guid: [1u8; 12]
            },
            "same GUID + new peerVersion must be a Change, not an Update"
        );
    }

    #[test]
    fn forget_clears_state() {
        let t = BeaconTracker::new();
        t.observe(addr(), "tcp", [1u8; 12], V);
        t.forget(addr());
        assert!(t.guid_for(addr()).is_none());
    }

    /// Stale entries — last_seen older than `max_age` — are pruned and
    /// returned so the caller can fire `Discovered::Timeout`. Mirrors
    /// pvxs `tickBeaconClean` (client.cpp:1254).
    #[test]
    fn prune_stale_returns_aged_out_entries() {
        let t = BeaconTracker::new();
        t.observe(addr(), "tcp", [9u8; 12], V);
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
            assert_eq!(t.observe(sa, "tcp", [0u8; 12], V), BeaconAction::New);
        }
        // Next insertion is refused — reported as CapDropped and the map
        // size stays at the cap.
        let extra: SocketAddr =
            SocketAddr::new(std::net::Ipv4Addr::new(255, 255, 255, 254).into(), 5075);
        assert_eq!(
            t.observe(extra, "tcp", [1u8; 12], V),
            BeaconAction::CapDropped
        );
        assert_eq!(t.inner.read().len(), BEACON_TRACK_LIMIT);
    }
}
