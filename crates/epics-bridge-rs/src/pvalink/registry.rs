//! Process-wide registry of open [`PvaLink`]s, keyed by PV name + direction.
//!
//! Used by record handlers so multiple records pointing at the same PV
//! share a single underlying client connection.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::Notify;

use super::config::{LinkDirection, PvaLinkConfig};
use super::link::{PvaLink, PvaLinkResult};

/// `(pv_name, pipeline, queue_size, direction)` — mirrors pvxs's
/// `channels_key_t = (channelName, pvRequest)` where pvRequest encodes
/// `pipeline` and `queueSize` (`pvxs/ioc/pvalink_lset.cpp:49-65`,
/// `pvxs/ioc/pvalink.h:116`). `field` is per-link, not in the channel
/// key — see `PvaLink::read_with_field` / `try_read_cached_with_field`.
type RegistryKey = (String, bool, usize, LinkDirection);

/// Cached PvaLink. Returns the same `Arc<PvaLink>` for repeated
/// `(pv_name, pipeline, queue_size, direction)` tuples.
#[derive(Default)]
pub struct PvaLinkRegistry {
    map: RwLock<HashMap<RegistryKey, Arc<PvaLink>>>,
    /// Round-36 (R36-G1): in-flight open dedup. The original
    /// `get_or_open` used a textbook DCL — read-lock → open →
    /// write-lock → DCL drop loser. Two concurrent first-callers
    /// both reached `PvaLink::open` (which spawns a monitor task
    /// and a `PvaClient`); the loser's resources cleaned up via
    /// Drop, but two upstream search/connect round-trips and two
    /// monitor-task spawns were spent for one user-visible result.
    /// This map carries an `Arc<Notify>` per in-flight open; the
    /// second caller awaits and then reads the cached entry.
    pending: RwLock<HashMap<RegistryKey, Arc<Notify>>>,
}

impl PvaLinkRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Exact lookup: `(pv_name, pipeline, queue_size, direction)`. Returns
    /// `None` when no link with that exact key has been opened.
    pub fn try_get(
        &self,
        pv_name: &str,
        pipeline: bool,
        queue_size: usize,
        direction: LinkDirection,
    ) -> Option<Arc<PvaLink>> {
        self.map
            .read()
            .get(&(pv_name.to_string(), pipeline, queue_size, direction))
            .cloned()
    }

    /// Return the first cached link for `(pv_name, direction)` regardless
    /// of `pipeline` / `queue_size`. Used by hot paths that only want
    /// "any cached value" (fast-path read, connected check, alarm
    /// severity) and don't care about which monitor variant they land on.
    pub fn try_get_any(&self, pv_name: &str, direction: LinkDirection) -> Option<Arc<PvaLink>> {
        self.map
            .read()
            .iter()
            .find(|((name, _, _, dir), _)| name == pv_name && *dir == direction)
            .map(|(_, link)| link.clone())
    }

    /// Get an existing link or open a new one. Concurrent calls
    /// for the same key share one [`PvaLink::open`] invocation;
    /// the second caller awaits via `pending` and reads the
    /// winner's cached entry.
    pub async fn get_or_open(&self, config: PvaLinkConfig) -> PvaLinkResult<Arc<PvaLink>> {
        let key: RegistryKey = (
            config.pv_name.clone(),
            config.pipeline,
            config.queue_size,
            config.direction,
        );

        // Fast path: already cached.
        if let Some(existing) = self.map.read().get(&key).cloned() {
            return Ok(existing);
        }

        // In-flight dedup. Either we claim the slot and open, or we
        // grab a Notify and await another task's completion.
        //
        // R49-G3: re-check the cache UNDER the `pending.write()`
        // lock. The fast path (line 56-58) reads `self.map` without
        // any synchronization vs. the winner's publish-then-clear
        // sequence: a late caller could see a fast-path miss, then
        // by the time it reaches `pending.write()` the winner has
        // already published to the map AND removed the pending slot.
        // Without this re-check the late caller would find pending
        // empty, claim the slot itself, and run `PvaLink::open` a
        // second time — defeating the singleflight invariant. The
        // map read is held inside the same critical section as the
        // pending lookup, so the only path that can publish to the
        // map between the two checks (winner publish at line ~155
        // BEFORE clearing pending) is now observable.
        let (claim, notify) = {
            let mut pending = self.pending.write();
            if let Some(existing) = self.map.read().get(&key).cloned() {
                return Ok(existing);
            }
            if let Some(existing) = pending.get(&key).cloned() {
                (false, existing)
            } else {
                let n = Arc::new(Notify::new());
                pending.insert(key.clone(), n.clone());
                (true, n)
            }
        };

        if !claim {
            // Loser path: wait for the winner to finish, then read
            // the cached entry.
            //
            // R48-G1: must register as a waiter BEFORE re-checking
            // the map. Tokio `Notify::notify_waiters()` does not
            // buffer permits, so the winner's guard drop can wake
            // every currently-registered waiter and then leave a
            // late `.notified().await` first-poll permanently
            // pending. The fix mirrors the
            // `enable()`-then-recheck-then-await pattern in
            // `epics-pva-rs/src/server_native/tcp.rs` (and the
            // tokio Notify docs): pin the future, call
            // `enable()` to register the waker synchronously, then
            // re-read the map. A wake that fires between the pin
            // and the await is now safely observed.
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(existing) = self.map.read().get(&key).cloned() {
                return Ok(existing);
            }
            // Winner may have already finished + cleared the
            // pending slot; in that case the map should hold the
            // entry (just re-checked above). If we get here the
            // winner errored and the entry is absent — recurse.
            if !self.pending.read().contains_key(&key) {
                return Box::pin(self.get_or_open(config)).await;
            }
            notified.await;
            if let Some(existing) = self.map.read().get(&key).cloned() {
                return Ok(existing);
            }
            // Winner errored — recurse so this caller now becomes
            // the claimant.
            return Box::pin(self.get_or_open(config)).await;
        }

        // Winner path: run the open with a drop-guard that always
        // clears the pending slot and wakes waiters — even on
        // panic / cancellation / error.
        struct CompletionGuard<'a> {
            owner: &'a PvaLinkRegistry,
            key: RegistryKey,
            notify: Arc<Notify>,
            armed: bool,
        }
        impl<'a> CompletionGuard<'a> {
            fn disarm(&mut self) {
                self.armed = false;
            }
        }
        impl<'a> Drop for CompletionGuard<'a> {
            fn drop(&mut self) {
                self.owner.pending.write().remove(&self.key);
                self.notify.notify_waiters();
                let _ = self.armed; // suppress unused warning
            }
        }
        let mut guard = CompletionGuard {
            owner: self,
            key: key.clone(),
            notify,
            armed: true,
        };

        let result = PvaLink::open(config).await;
        let link = Arc::new(result?);
        // Publish to the cache before releasing the pending slot so
        // waiters that wake up see the cached entry immediately.
        self.map.write().insert(key.clone(), link.clone());
        guard.disarm();
        // Manually run cleanup now (guard's Drop also clears, but
        // we want it deterministic on the success path).
        Ok(link)
    }

    pub fn close_all(&self) {
        self.map.write().clear();
        self.pending.write().clear();
    }

    pub fn len(&self) -> usize {
        self.map.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn close_all_empties_registry() {
        let reg = PvaLinkRegistry::new();
        // Don't actually open links (would require a running PVA server);
        // just exercise the empty-state APIs.
        assert!(reg.is_empty());
        reg.close_all();
        assert_eq!(reg.len(), 0);
    }
}
