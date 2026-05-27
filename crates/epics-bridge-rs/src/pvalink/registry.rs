//! Process-wide registry of open [`PvaLink`]s, keyed by PV name + direction.
//!
//! Used by record handlers so multiple records pointing at the same PV
//! share a single underlying client connection.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::Notify;

use super::config::{LinkDirection, ProcMode, PvaLinkConfig};
use super::link::{PvaLink, PvaLinkResult};

/// Registry cache key.
///
/// The first four members — `(pv_name, pipeline, queue_size,
/// direction)` — mirror pvxs's `channels_key_t = (channelName,
/// pvRequest)` where pvRequest encodes `pipeline` and `queueSize`
/// (`pvxs/ioc/pvalink_lset.cpp:49-65`, `pvxs/ioc/pvalink.h:116`).
///
/// `out_opts` carries the per-link OUT behavior options
/// (`field`, `process`, `defer`, `retry`). The earlier key excluded
/// these, so two OUT links to the same remote PV with different
/// query options collapsed onto one cached [`PvaLink`] and the later
/// link silently inherited the first link's `field` / `proc` /
/// `defer` / `retry`. pvxs keeps a per-link `pvaLinkConfig`
/// (`pvxs/ioc/pvalink.h:65`); including the OUT options in the key
/// restores that per-link isolation — two OUT links with different
/// behavior options now resolve to distinct cache entries.
///
/// For INP links `out_opts` is always `None`, so INP keying is
/// unchanged (INP per-link `field` / `sevr` / `time` divergence is
/// handled at the getter level, see `integration.rs`).
type RegistryKey = (String, bool, usize, LinkDirection, Option<OutOpts>);

/// Behavior-changing OUT-link options that must not be shared across
/// links — see [`RegistryKey`]. `field` selects the remote sub-field,
/// `process` requests a remote `process()`, `defer` queues the Put,
/// `retry` replays a Put across a disconnect.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OutOpts {
    field: String,
    proc: ProcMode,
    defer: bool,
    retry: bool,
}

impl OutOpts {
    /// Extract the OUT behavior options from a config, or `None` for
    /// an INP link (INP links never key on these).
    fn from_config(config: &PvaLinkConfig) -> Option<Self> {
        match config.direction {
            LinkDirection::Out => Some(Self {
                field: config.field.clone(),
                proc: config.proc,
                defer: config.defer,
                retry: config.retry,
            }),
            LinkDirection::Inp => None,
        }
    }
}

/// Cached PvaLink. Returns the same `Arc<PvaLink>` for repeated
/// `(pv_name, pipeline, queue_size, direction)` tuples.
#[derive(Default)]
pub struct PvaLinkRegistry {
    map: RwLock<HashMap<RegistryKey, Arc<PvaLink>>>,
    /// In-flight open dedup. The original
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

    /// Exact lookup for the full config's [`RegistryKey`]. Returns
    /// `None` when no link with that exact key has been opened.
    pub fn try_get(&self, config: &PvaLinkConfig) -> Option<Arc<PvaLink>> {
        let key: RegistryKey = (
            config.pv_name.clone(),
            config.pipeline,
            config.queue_size,
            config.direction,
            OutOpts::from_config(config),
        );
        self.map.read().get(&key).cloned()
    }

    /// Return the first cached link for `(pv_name, direction)` regardless
    /// of `pipeline` / `queue_size`. Used by hot paths that only want
    /// "any cached value" (fast-path read, connected check, alarm
    /// severity) and don't care about which monitor variant they land on.
    pub fn try_get_any(&self, pv_name: &str, direction: LinkDirection) -> Option<Arc<PvaLink>> {
        self.map
            .read()
            .iter()
            .find(|((name, _, _, dir, _), _)| name == pv_name && *dir == direction)
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
            OutOpts::from_config(&config),
        );

        // Fast path: already cached.
        if let Some(existing) = self.map.read().get(&key).cloned() {
            return Ok(existing);
        }

        // In-flight dedup. Either we claim the slot and open, or we
        // grab a Notify and await another task's completion.
        //
        // re-check the cache UNDER the `pending.write()`
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
            // must register as a waiter BEFORE re-checking
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

    /// Test-only: insert a pre-built [`PvaLink`] under its config's
    /// [`RegistryKey`]. Lets a test seed a cached link (e.g. an INP
    /// link with a pre-populated value) without standing up a PVA
    /// server, so the resolver getter paths can be exercised against
    /// a known cache state.
    #[cfg(test)]
    pub(crate) fn insert_for_test(&self, config: &PvaLinkConfig, link: Arc<PvaLink>) {
        let key: RegistryKey = (
            config.pv_name.clone(),
            config.pipeline,
            config.queue_size,
            config.direction,
            OutOpts::from_config(config),
        );
        self.map.write().insert(key, link);
    }

    pub fn len(&self) -> usize {
        self.map.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// distinct upstream PV names of every opened INP
    /// link, sorted for a stable order.
    ///
    /// The base iocInit external-link wait
    /// (`PvDatabase::wait_for_external_links`) drives off
    /// `LinkSet::link_names()` and then queries `is_connected(name)`
    /// for each. Only INP links carry a monitor connection signal —
    /// `PvaLinkResolver::is_connected` resolves through
    /// `try_get_any(.., Inp)`, and OUT links install no monitor so
    /// they would report perpetually disconnected. Returning INP names
    /// only keeps the wait set inside the resolver's own key family.
    ///
    /// Distinct per-option INP links to the same upstream PV (differing
    /// `pipeline` / `queue_size`) collapse to a single name here:
    /// `is_connected` answers for any of them via `try_get_any`, so the
    /// wait needs each upstream PV exactly once.
    pub fn inp_link_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .map
            .read()
            .keys()
            .filter(|(_, _, _, dir, _)| *dir == LinkDirection::Inp)
            .map(|(name, _, _, _, _)| name.clone())
            .collect();
        names.sort();
        names.dedup();
        names
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

    /// `inp_link_names` feeds the base iocInit
    /// external-link wait. It must (a) report each opened INP upstream
    /// PV exactly once even when two option-variants of the same PV are
    /// cached under distinct registry keys, and (b) exclude OUT links
    /// (which carry no monitor connection signal `is_connected` could
    /// answer for). `insert_for_test` seeds links without a PVA server.
    #[tokio::test]
    async fn fr15_inp_link_names_dedups_by_pv_and_excludes_out() {
        let reg = PvaLinkRegistry::new();

        // Two INP option-variants of the same upstream PV.
        let a1 = PvaLinkConfig {
            queue_size: 1,
            ..PvaLinkConfig::defaults_for("A:PV", LinkDirection::Inp)
        };
        let a2 = PvaLinkConfig {
            queue_size: 8,
            ..PvaLinkConfig::defaults_for("A:PV", LinkDirection::Inp)
        };
        let b = PvaLinkConfig::defaults_for("B:PV", LinkDirection::Inp);
        let c_out = PvaLinkConfig::defaults_for("C:OUT", LinkDirection::Out);

        reg.insert_for_test(&a1, Arc::new(PvaLink::for_test(a1.clone(), None)));
        reg.insert_for_test(&a2, Arc::new(PvaLink::for_test(a2.clone(), None)));
        reg.insert_for_test(&b, Arc::new(PvaLink::for_test(b.clone(), None)));
        reg.insert_for_test(&c_out, Arc::new(PvaLink::for_test(c_out.clone(), None)));

        assert_eq!(
            reg.inp_link_names(),
            vec!["A:PV".to_string(), "B:PV".to_string()],
            "INP names deduped by PV (A:PV once), OUT (C:OUT) excluded"
        );
    }

    /// two OUT links to the same remote PV with different
    /// behavior-changing query options (`field`, `proc`, `defer`,
    /// `retry`) must resolve to *distinct* cached [`PvaLink`]s, each
    /// owning its own config. Pre-fix the registry key was
    /// `(pv_name, pipeline, queue_size, direction)` — it excluded the
    /// OUT options, so the second `get_or_open` returned the first
    /// link and silently inherited its `field` / `proc` / `defer` /
    /// `retry`.
    ///
    /// OUT-link `PvaLink::open` does no network I/O (it only builds a
    /// `PvaClient`), so this exercises the real registry path without
    /// a PVA server. pvxs `pvaLinkConfig` is per-link
    /// (`pvxs/ioc/pvalink.h:65`).
    #[tokio::test]
    async fn mr_r14_out_links_keep_own_options() {
        let reg = PvaLinkRegistry::new();

        let cfg_a = PvaLinkConfig {
            field: "fieldA".to_string(),
            proc: ProcMode::Default,
            defer: false,
            retry: false,
            ..PvaLinkConfig::defaults_for("MR_R14:PV", LinkDirection::Out)
        };
        let cfg_b = PvaLinkConfig {
            field: "fieldB".to_string(),
            proc: ProcMode::Pp,
            defer: true,
            retry: true,
            ..PvaLinkConfig::defaults_for("MR_R14:PV", LinkDirection::Out)
        };

        let link_a = reg.get_or_open(cfg_a).await.expect("open OUT link A");
        let link_b = reg.get_or_open(cfg_b).await.expect("open OUT link B");

        assert!(
            !Arc::ptr_eq(&link_a, &link_b),
            "OUT links with different options must not share one PvaLink"
        );
        assert_eq!(
            link_a.config().field,
            "fieldA",
            "link A keeps its own field"
        );
        assert_eq!(
            link_b.config().field,
            "fieldB",
            "link B must not inherit link A's field"
        );
        assert!(
            link_a.config().proc == ProcMode::Default
                && !link_a.config().defer
                && !link_a.config().retry,
            "link A keeps its own proc/defer/retry"
        );
        assert!(
            link_b.config().proc == ProcMode::Pp && link_b.config().defer && link_b.config().retry,
            "link B must not inherit link A's proc/defer/retry"
        );

        // A second open of link A's exact config returns the same
        // cached Arc — connection sharing for identical options is
        // preserved.
        let cfg_a2 = PvaLinkConfig {
            field: "fieldA".to_string(),
            proc: ProcMode::Default,
            defer: false,
            retry: false,
            ..PvaLinkConfig::defaults_for("MR_R14:PV", LinkDirection::Out)
        };
        let link_a2 = reg.get_or_open(cfg_a2).await.expect("re-open OUT link A");
        assert!(
            Arc::ptr_eq(&link_a, &link_a2),
            "identical OUT options must share one cached PvaLink"
        );
        assert_eq!(reg.len(), 2, "two distinct OUT links cached");
    }
}
