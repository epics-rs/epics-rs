//! Process-wide registry of open [`PvaLink`]s, keyed by PV name + direction.
//!
//! Used by record handlers so multiple records pointing at the same PV
//! share a single underlying client connection.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::Notify;

use super::config::{LinkDirection, ProcMode, PvaLinkConfig, SevrMode};
use super::link::{PvaLink, PvaLinkResult};

/// Registry cache key.
///
/// All four members — `(pv_name, pipeline, queue_size, direction)` —
/// mirror pvxs's `channels_key_t = (channelName, pvRequest)` where
/// pvRequest encodes `pipeline` and `queueSize`
/// (`pvxs/ioc/pvalink_lset.cpp:49-65`, `pvxs/ioc/pvalink.h:116`). The
/// key deliberately does NOT include the per-link OUT options
/// (`field`, `process`, `defer`, `retry`): pvxs caches one shared
/// `pvaLinkChannel` per `(channelName, pvRequest)` and keeps the
/// per-link `field`/`proc`/`defer`/`retry` on the child `pvaLink`
/// objects, not in the channel key (`pvxs/ioc/pvalink_lset.cpp:99-122`,
/// `pvxs/ioc/pvalink.h:65,116`).
///
/// Folding sibling OUT links to one upstream PV onto one shared
/// [`PvaLink`] is what lets several field writes coalesce into a
/// single upstream PUT: the shared owner gathers each link's staged
/// value (keyed by its `field`) and emits one combined PUT with the
/// process option resolved across all participating links
/// (`pvxs/ioc/pvalink_channel.cpp:220-263`). The per-link options
/// travel with each write via [`PvaLink::put_out_str`] /
/// [`PvaLink::put_out_field`] rather than being baked into the channel
/// identity (see `link.rs`'s `out_scratch`).
///
/// INP per-link `field` / `sevr` / `time` divergence is likewise
/// handled at the getter level (see `integration.rs`), so INP links
/// that differ only in those collapse onto one monitor subscription.
type RegistryKey = (String, bool, usize, LinkDirection);

/// One row of pvalink channel diagnostics — the per-channel state the
/// `dbpvar` IOC shell command prints. Mirrors the fields pvxs `dbpvxr`
/// dumps per channel (`pvxs/ioc/pvalink.cpp:243-303`): the connection
/// state plus the link's `proc`/`sevr` mode and its
/// queue/pipeline/defer/time/retry/monorder configuration.
///
/// NOTE the pvxs `num_disconnect` / `num_type_change` counters and the
/// active-`Put` marker are not represented: the Rust [`PvaLink`] does
/// not instrument those transitions, so they are intentionally absent
/// rather than reported as a fabricated zero.
#[derive(Debug, Clone)]
pub struct ChannelDiag {
    pub pv_name: String,
    pub direction: LinkDirection,
    pub connected: bool,
    pub pipeline: bool,
    pub queue_size: usize,
    pub proc: ProcMode,
    pub sevr: SevrMode,
    pub field: String,
    pub defer: bool,
    pub time: bool,
    pub retry: bool,
    pub monorder: i32,
    /// Sorted set of record names whose links attached to this channel.
    /// pvxs's `pvaLinkChannel` keeps a `links` list of `pvaLink*`, each
    /// with `plink->precord->name`; the registry folds field/sevr-only
    /// variants onto one channel entry, so this is the de-duplicated set
    /// of owning records. Backs `dbpvar`'s record-name glob filter
    /// (`pvxs/ioc/pvalink.cpp:213-243`). Empty for channels opened
    /// manually (e.g. `pvxr`) with no owning record.
    pub records: Vec<String>,
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
    /// Channel → owning record-name back-index. Mirrors pvxs's
    /// `pvaLinkChannel::links` (a list of `pvaLink*` each with
    /// `plink->precord->name`): the set of records whose links attached
    /// to each cached channel. Populated at open time via
    /// [`Self::attach_record`]; backs the `dbpvar` record-name glob
    /// filter (`pvxs/ioc/pvalink.cpp:213-243`).
    channel_records: RwLock<HashMap<RegistryKey, BTreeSet<String>>>,
}

/// Build the [`RegistryKey`] for a config — the shared
/// `(channelName, pvRequest)` identity pvxs caches by.
fn key_of(config: &PvaLinkConfig) -> RegistryKey {
    (
        config.pv_name.clone(),
        config.pipeline,
        config.queue_size,
        config.direction,
    )
}

impl PvaLinkRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `record`'s link attached to the channel identified by
    /// `config`. The channel may not be open yet (or may be shared by
    /// several records); the back-index keys by the same
    /// `(channelName, pvRequest)` identity the registry caches by, so a
    /// fold of field/sevr-only variants still accumulates every owning
    /// record. Mirrors pvxs appending a `pvaLink` to
    /// `pvaLinkChannel::links` (`pvalink_channel.cpp` link attach).
    pub fn attach_record(&self, config: &PvaLinkConfig, record: &str) {
        self.channel_records
            .write()
            .entry(key_of(config))
            .or_default()
            .insert(record.to_string());
    }

    /// Exact lookup for the full config's [`RegistryKey`]. Returns
    /// `None` when no link with that exact key has been opened.
    pub fn try_get(&self, config: &PvaLinkConfig) -> Option<Arc<PvaLink>> {
        let key: RegistryKey = (
            config.pv_name.clone(),
            config.pipeline,
            config.queue_size,
            config.direction,
        );
        self.map.read().get(&key).cloned()
    }

    /// Exact INP lookup by monitor-variant identity `(pv_name,
    /// pipeline, queue_size)`. Returns the cached INP link for that
    /// exact variant, or `None`.
    ///
    /// The monitor variant is fully described by these three fields —
    /// matching pvxs's `channels_key_t = (channelName, pvRequest)` where
    /// the pvRequest encodes `pipeline` / `queueSize`
    /// (`pvxs/ioc/pvalink.h:115-120`). Resolver hot paths use this
    /// instead of a bare-PV-name match so a `Q=1` record reads from its
    /// own monitor, never from a `Q=64` sibling that merely shares the
    /// upstream PV name.
    pub fn try_get_inp(
        &self,
        pv_name: &str,
        pipeline: bool,
        queue_size: usize,
    ) -> Option<Arc<PvaLink>> {
        let key: RegistryKey = (
            pv_name.to_string(),
            pipeline,
            queue_size,
            LinkDirection::Inp,
        );
        self.map.read().get(&key).cloned()
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
        self.channel_records.write().clear();
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
        );
        self.map.write().insert(key, link);
    }

    pub fn len(&self) -> usize {
        self.map.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Distinct monitor-variant identities `(pv_name, pipeline,
    /// queue_size)` of every opened INP link, sorted for a stable order.
    ///
    /// The base iocInit external-link wait
    /// (`PvDatabase::wait_for_external_links`) drives off
    /// `LinkSet::link_names()` and then queries `is_connected(name)` for
    /// each. Only INP links carry a monitor connection signal — OUT
    /// links install no monitor — so OUT keys are excluded here.
    ///
    /// Two INP links to the same upstream PV with different `pipeline` /
    /// `queue_size` are DISTINCT monitor variants (distinct
    /// subscriptions in pvxs, `pvxs/ioc/pvalink.h:115-120`), so each is
    /// reported separately: the wait must confirm every variant's own
    /// monitor connected, not just the first to share the PV name. Links
    /// that share the exact `(pv_name, pipeline, queue_size)` variant
    /// (differing only in `field` / `sevr`, which the registry folds
    /// onto one entry) collapse to a single identity, matching the one
    /// shared subscription that actually backs them.
    pub fn inp_identities(&self) -> Vec<(String, bool, usize)> {
        let mut ids: Vec<(String, bool, usize)> = self
            .map
            .read()
            .keys()
            .filter(|(_, _, _, dir)| *dir == LinkDirection::Inp)
            .map(|(name, pipeline, qsize, _)| (name.clone(), *pipeline, *qsize))
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// Every cached OUT-direction [`PvaLink`] — the shared channel
    /// owners that may hold staged or retry-queued writes. Backs the
    /// `LinkSet::flush_puts` production drain: the resolver re-attempts
    /// any channel holding a retry-queued write after a reconnect.
    pub fn out_links(&self) -> Vec<Arc<PvaLink>> {
        self.map
            .read()
            .iter()
            .filter(|((_, _, _, dir), _)| *dir == LinkDirection::Out)
            .map(|(_, link)| link.clone())
            .collect()
    }

    /// Per-channel diagnostics snapshot for every cached link, sorted by
    /// `(pv_name, direction)` for stable output. Backs the `dbpvar` IOC
    /// shell command, the Rust counterpart of pvxs `dbpvxr`
    /// (`pvxs/ioc/pvalink.cpp:184-316`), which iterates
    /// `linkGlobal->channels`. Each cached registry entry is one channel
    /// (the registry already folds links that differ only in
    /// `field`/`sevr` onto one entry, matching pvxs's one-subscription
    /// fold).
    pub fn channel_diagnostics(&self) -> Vec<ChannelDiag> {
        fn dir_rank(d: LinkDirection) -> u8 {
            match d {
                LinkDirection::Inp => 0,
                LinkDirection::Out => 1,
            }
        }
        let records = self.channel_records.read();
        let mut rows: Vec<ChannelDiag> = self
            .map
            .read()
            .iter()
            .map(|(key, link)| {
                let (pv, pipeline, qsize, dir) = key;
                let c = link.config();
                ChannelDiag {
                    pv_name: pv.clone(),
                    direction: *dir,
                    connected: link.is_connected(),
                    pipeline: *pipeline,
                    queue_size: *qsize,
                    proc: c.proc,
                    sevr: c.sevr,
                    field: c.field.clone(),
                    defer: c.defer,
                    time: c.time,
                    retry: c.retry,
                    monorder: c.monorder,
                    records: records
                        .get(key)
                        .map(|s| s.iter().cloned().collect())
                        .unwrap_or_default(),
                }
            })
            .collect();
        rows.sort_by(|a, b| {
            a.pv_name
                .cmp(&b.pv_name)
                .then(dir_rank(a.direction).cmp(&dir_rank(b.direction)))
        });
        rows
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

    /// `inp_identities` feeds the base iocInit external-link wait. It
    /// must (a) report each DISTINCT INP monitor variant — two links to
    /// the same upstream PV with different `queue_size` / `pipeline` are
    /// distinct subscriptions and each must be waited on independently;
    /// (b) collapse links that share the exact `(pv, pipeline, Q)`
    /// variant (the registry already folds `field`/`sevr` differences
    /// onto one entry); and (c) exclude OUT links (no monitor signal).
    /// `insert_for_test` seeds links without a PVA server.
    ///
    /// BRIDGE-RS-2026-05-28-128: pre-fix this deduped by bare PV name,
    /// collapsing the `Q=1` and `Q=8` variants of `A:PV` into one wait
    /// entry — so a record could begin processing before its own
    /// monitor variant connected.
    #[tokio::test]
    async fn fr15_inp_identities_separate_variants_and_exclude_out() {
        let reg = PvaLinkRegistry::new();

        // Two INP monitor-variants of the same upstream PV.
        let a1 = PvaLinkConfig {
            queue_size: 1,
            ..PvaLinkConfig::defaults_for("A:PV", LinkDirection::Inp)
        };
        let a2 = PvaLinkConfig {
            queue_size: 8,
            ..PvaLinkConfig::defaults_for("A:PV", LinkDirection::Inp)
        };
        // Same variant as a1 but a different sub-field — registry folds
        // it onto a1's entry, so it must NOT add a second identity.
        let a1_field = PvaLinkConfig {
            queue_size: 1,
            field: "alarm.severity".to_string(),
            ..PvaLinkConfig::defaults_for("A:PV", LinkDirection::Inp)
        };
        let b = PvaLinkConfig::defaults_for("B:PV", LinkDirection::Inp);
        let c_out = PvaLinkConfig::defaults_for("C:OUT", LinkDirection::Out);

        reg.insert_for_test(&a1, Arc::new(PvaLink::for_test(a1.clone(), None)));
        reg.insert_for_test(&a2, Arc::new(PvaLink::for_test(a2.clone(), None)));
        reg.insert_for_test(
            &a1_field,
            Arc::new(PvaLink::for_test(a1_field.clone(), None)),
        );
        reg.insert_for_test(&b, Arc::new(PvaLink::for_test(b.clone(), None)));
        reg.insert_for_test(&c_out, Arc::new(PvaLink::for_test(c_out.clone(), None)));

        let default_q = PvaLinkConfig::defaults_for("B:PV", LinkDirection::Inp).queue_size;
        assert_eq!(
            reg.inp_identities(),
            vec![
                ("A:PV".to_string(), false, 1),
                ("A:PV".to_string(), false, 8),
                ("B:PV".to_string(), false, default_q),
            ],
            "distinct Q variants of A:PV reported separately; field-only \
             variant folded; OUT (C:OUT) excluded"
        );
    }

    /// `channel_diagnostics` backs the `dbpvar` IOC shell command: it
    /// snapshots every cached channel (INP and OUT) with its connection
    /// state and per-link config, sorted by `(pv_name, direction)`.
    #[tokio::test]
    async fn channel_diagnostics_reports_all_channels_with_config() {
        let reg = PvaLinkRegistry::new();

        let inp = PvaLinkConfig {
            queue_size: 8,
            time: true,
            sevr: SevrMode::Ms,
            ..PvaLinkConfig::defaults_for("A:PV", LinkDirection::Inp)
        };
        let out = PvaLinkConfig {
            field: "VAL".to_string(),
            proc: ProcMode::Pp,
            defer: true,
            ..PvaLinkConfig::defaults_for("A:PV", LinkDirection::Out)
        };
        reg.insert_for_test(&inp, Arc::new(PvaLink::for_test(inp.clone(), None)));
        reg.insert_for_test(&out, Arc::new(PvaLink::for_test(out.clone(), None)));

        let diags = reg.channel_diagnostics();
        assert_eq!(diags.len(), 2, "both INP and OUT channels reported");

        // Sorted: same pv_name, INP before OUT.
        assert_eq!(diags[0].direction, LinkDirection::Inp);
        assert_eq!(diags[0].queue_size, 8);
        assert_eq!(diags[0].sevr, SevrMode::Ms);
        assert!(diags[0].time, "INP link config (time=true) surfaced");

        assert_eq!(diags[1].direction, LinkDirection::Out);
        assert_eq!(diags[1].proc, ProcMode::Pp);
        assert!(diags[1].defer, "OUT link config (defer=true) surfaced");
        assert_eq!(diags[1].field, "VAL");

        // `for_test` links are not connected (no live monitor).
        assert!(diags.iter().all(|d| !d.connected));
    }

    /// The channel → record back-index that backs `dbpvar`'s record-name
    /// glob filter: `attach_record` accumulates every owning record on a
    /// channel (folding field/sevr-only variants onto one entry), and
    /// `channel_diagnostics` surfaces the de-duplicated, sorted set.
    /// pvxs equivalent: `pvaLinkChannel::links` filtered by
    /// `epicsStrGlobMatch(precord->name, ...)` (`pvalink.cpp:213-243`).
    #[tokio::test]
    async fn channel_diagnostics_carries_attached_record_names() {
        let reg = PvaLinkRegistry::new();

        let inp = PvaLinkConfig::defaults_for("SRC:PV", LinkDirection::Inp);
        // A field-only variant folds onto the same channel key.
        let inp_field = PvaLinkConfig {
            field: "alarm.severity".to_string(),
            ..PvaLinkConfig::defaults_for("SRC:PV", LinkDirection::Inp)
        };
        reg.insert_for_test(&inp, Arc::new(PvaLink::for_test(inp.clone(), None)));
        reg.insert_for_test(
            &inp_field,
            Arc::new(PvaLink::for_test(inp_field.clone(), None)),
        );

        // Two records share the folded channel; one of them registers
        // twice (idempotent set insert).
        reg.attach_record(&inp, "REC:A");
        reg.attach_record(&inp_field, "REC:B");
        reg.attach_record(&inp, "REC:A");

        let diags = reg.channel_diagnostics();
        let row = diags
            .iter()
            .find(|d| d.pv_name == "SRC:PV" && d.direction == LinkDirection::Inp)
            .expect("INP channel present");
        assert_eq!(
            row.records,
            vec!["REC:A".to_string(), "REC:B".to_string()],
            "both owning records surface, de-duplicated and sorted"
        );

        // close_all clears the back-index too.
        reg.close_all();
        assert!(reg.channel_diagnostics().is_empty());
    }

    /// Two OUT links to the same remote PV that differ only in their
    /// behavior options (`field`, `proc`, `defer`, `retry`) must
    /// resolve to ONE shared [`PvaLink`] channel owner — the
    /// precondition for coalescing sibling field writes into a single
    /// upstream PUT. pvxs caches one `pvaLinkChannel` per
    /// `(channelName, pvRequest)` and keeps the per-link options on the
    /// child `pvaLink` objects, so two links to the same channel share
    /// the owner and each contributes its staged value with its own
    /// `field`/`proc` at PUT-build time
    /// (`pvxs/ioc/pvalink_lset.cpp:99-122`, `pvalink_channel.cpp:127-156`).
    ///
    /// The per-link `field`/`proc`/`defer`/`retry` no longer live in
    /// the registry key (they travel with each write via
    /// `PvaLink::put_out_str` / `put_out_field`), so they cannot cause
    /// distinct cache entries. OUT-link `PvaLink::open` does no network
    /// I/O, so this exercises the real registry path without a PVA
    /// server.
    #[tokio::test]
    async fn mr_r14_out_links_share_one_channel_owner() {
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
            Arc::ptr_eq(&link_a, &link_b),
            "sibling OUT links to one PV must share a single channel owner \
             so their writes can coalesce into one upstream PUT"
        );
        assert_eq!(
            reg.len(),
            1,
            "differing OUT options must NOT spawn distinct cache entries"
        );
        // A differing `pipeline` / `queue_size` IS a distinct channel
        // (distinct upstream pvRequest, pvxs `channels_key_t`), so
        // those still key separately.
        let cfg_pipe = PvaLinkConfig {
            pipeline: true,
            ..PvaLinkConfig::defaults_for("MR_R14:PV", LinkDirection::Out)
        };
        let link_pipe = reg.get_or_open(cfg_pipe).await.expect("open piped owner");
        assert!(
            !Arc::ptr_eq(&link_a, &link_pipe),
            "a different pvRequest (pipeline) is a different channel owner"
        );
        assert_eq!(reg.len(), 2, "two distinct channel owners cached");
    }
}
