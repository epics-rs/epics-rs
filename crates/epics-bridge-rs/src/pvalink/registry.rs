//! Process-wide registry of open [`PvaLink`]s, keyed by PV name + direction.
//!
//! Used by record handlers so multiple records pointing at the same PV
//! share a single underlying client connection.

// RTEMS-EXEC-MODEL-ALLOW(5): checked - these run and pass in the exec-backend
// suite. (1 live-upstream monitor test gated out on the exec backend below;
// §4.2, stage 3.)

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use epics_pva_rs::client::PvaClient;
use parking_lot::RwLock;
use tokio::sync::Notify;

use super::config::{LinkDirection, ProcMode, PvaLinkConfig, SevrMode};
use super::link::{PvaLink, PvaLinkResult};

/// Operation timeout of the IOC-wide pvalink client.
///
/// The value every link used to build its own client with, kept explicit
/// so it does not silently follow `PvaClientBuilder`'s default. pvalink
/// exposes **no** per-link timeout option (neither does pvxs's
/// `pvaLinkConfig`), so one client-level timeout is the whole surface; a
/// per-link timeout, if one is ever added, must be passed to the
/// per-operation call instead of forking the shared client.
const PVALINK_CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

/// Registry cache key.
///
/// All four members — `(pv_name, pipeline, queue_size, direction)` —
/// mirror pvxs's `channels_key_t = (channelName, pvRequest)` where
/// pvRequest encodes `pipeline` and `queueSize`
/// (`pvxs/ioc/pvalink_lset.cpp:99-100`, `pvxs/ioc/pvalink.h:116`). The
/// key deliberately does NOT include the per-link OUT options
/// (`field`, `process`, `defer`, `retry`): pvxs caches one shared
/// `pvaLinkChannel` per `(channelName, pvRequest)` and keeps the
/// per-link `field`/`proc`/`defer`/`retry` on the child `pvaLink`
/// objects, not in the channel key (`pvxs/ioc/pvalink_lset.cpp:99-131`,
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
    /// (`pvxs/ioc/pvalink.cpp:216-231`). Empty for channels opened
    /// manually (e.g. `pvxr`) with no owning record.
    pub records: Vec<String>,
}

/// Cached PvaLink. Returns the same `Arc<PvaLink>` for repeated
/// `(pv_name, pipeline, queue_size, direction)` tuples.
pub struct PvaLinkRegistry {
    /// The ONE [`PvaClient`] every link opened through this registry
    /// runs on — the single owner pvxs calls
    /// `linkGlobal->provider_remote`, built once in `linkGlobal_t::alloc()`
    /// (`pvxs/ioc/pvalink.h:107`, `pvxs/ioc/pvalink.cpp:51-64`).
    ///
    /// The registry, not the link, owns it because the registry is what
    /// constructs links: [`PvaLink::open`] takes a client and never builds
    /// one, so "a link owns a client" is not a representable state. All
    /// links therefore share one `ConnectionPool` (keyed on `SocketAddr`)
    /// and one search engine — two links to the same upstream IOC cost one
    /// TCP connection, not two, whatever their `pvRequest` (`Q`, pipeline)
    /// or direction.
    client: PvaClient,
    map: RwLock<HashMap<RegistryKey, Arc<PvaLink>>>,
    /// In-flight open dedup. The original
    /// `get_or_open` used a textbook DCL — read-lock → open →
    /// write-lock → DCL drop loser. Two concurrent first-callers
    /// both reached `PvaLink::open` (which spawns a monitor task);
    /// the loser's resources cleaned up via Drop, but two upstream
    /// search/connect round-trips and two monitor-task spawns were
    /// spent for one user-visible result.
    /// This map carries an `Arc<Notify>` per in-flight open; the
    /// second caller awaits and then reads the cached entry.
    pending: RwLock<HashMap<RegistryKey, Arc<Notify>>>,
    /// Channel → owning record-name back-index. Mirrors pvxs's
    /// `pvaLinkChannel::links` (a list of `pvaLink*` each with
    /// `plink->precord->name`): the set of records whose links attached
    /// to each cached channel. Populated at open time via
    /// [`Self::attach_record`]; backs the `dbpvar` record-name glob
    /// filter (`pvxs/ioc/pvalink.cpp:216-231`).
    channel_records: RwLock<HashMap<RegistryKey, BTreeSet<String>>>,
}

/// Build the `RegistryKey` for a config — the shared
/// `(channelName, pvRequest)` identity pvxs caches by.
fn key_of(config: &PvaLinkConfig) -> RegistryKey {
    (
        config.pv_name.clone(),
        config.pipeline,
        config.queue_size,
        config.direction,
    )
}

impl Default for PvaLinkRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PvaLinkRegistry {
    /// A registry owning a freshly built IOC-wide client.
    pub fn new() -> Self {
        Self::with_client(PvaClient::builder().timeout(PVALINK_CLIENT_TIMEOUT).build())
    }

    /// A registry running every link on `client`.
    ///
    /// The injection seam for the IOC-wide client, matching pvxs building
    /// `linkGlobal->provider_remote` from the IOC server's client config
    /// (`pvxs/ioc/pvalink.cpp:60`) rather than from bare defaults. Tests
    /// use it to pin the whole registry at one `PvaServer` address.
    pub fn with_client(client: PvaClient) -> Self {
        Self {
            client,
            map: RwLock::new(HashMap::new()),
            pending: RwLock::new(HashMap::new()),
            channel_records: RwLock::new(HashMap::new()),
        }
    }

    /// STAGE-5 PROBE: the IOC-wide client, so a target image with no shell
    /// can print `PvaClient::report()` on its console — the only way to
    /// observe "one upstream connection regardless of link count" from the
    /// guest side.
    pub fn client(&self) -> &PvaClient {
        &self.client
    }

    /// Record that `record`'s link attached to the channel identified by
    /// `config`. The channel may not be open yet (or may be shared by
    /// several records); the back-index keys by the same
    /// `(channelName, pvRequest)` identity the registry caches by, so a
    /// fold of field/sevr-only variants still accumulates every owning
    /// record. Mirrors pvxs appending a `pvaLink` to
    /// `pvaLinkChannel::links` (`pvxs/ioc/pvalink_channel.cpp` link attach).
    pub fn attach_record(&self, config: &PvaLinkConfig, record: &str) {
        self.channel_records
            .write()
            .entry(key_of(config))
            .or_default()
            .insert(record.to_string());
    }

    /// Exact lookup for the full config's `RegistryKey`. Returns
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
    /// (`pvxs/ioc/pvalink.h:116-120`). Resolver hot paths use this
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

    /// Exact OUT lookup — the write-direction twin of [`Self::try_get_inp`].
    ///
    /// The registry keys on direction, so an OUT link's shared channel owner
    /// is a *different* entry from the INP one for the same PV. A caller
    /// asking "is the write path up?" must therefore look here: the INP
    /// lookup answers `None` for a perfectly healthy OUT-only channel.
    /// An OUT channel tracks its connection through its own liveness monitor
    /// (`PvaLink::open`'s OUT arm), which is what `is_connected()` reads.
    pub fn try_get_out(
        &self,
        pv_name: &str,
        pipeline: bool,
        queue_size: usize,
    ) -> Option<Arc<PvaLink>> {
        let key: RegistryKey = (
            pv_name.to_string(),
            pipeline,
            queue_size,
            LinkDirection::Out,
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

        let result = PvaLink::open(config, self.client.clone()).await;
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
    /// `RegistryKey`. Lets a test seed a cached link (e.g. an INP
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
    /// Backs [`epics_base_rs::server::database::LinkSet::link_names`] (the `LinkSet` enumeration
    /// contract): each identity round-trips to an `is_connected(name)`
    /// query that lands on that variant's own monitor. Only INP links
    /// carry a monitor connection signal — OUT links install no monitor —
    /// so OUT keys are excluded here. (This enumeration is not consumed by
    /// the iocInit wait: that is CA-facility only and pvalink never blocks
    /// init — pvxs parity.)
    ///
    /// Two INP links to the same upstream PV with different `pipeline` /
    /// `queue_size` are DISTINCT monitor variants (distinct subscriptions
    /// in pvxs, `pvxs/ioc/pvalink.h:116-120`), so each is reported
    /// separately: a per-variant `is_connected` must land on that
    /// variant's own monitor, not just the first to share the PV name.
    /// Links that share the exact `(pv_name, pipeline, queue_size)`
    /// variant (differing only in `field` / `sevr`, which the registry
    /// folds onto one entry) collapse to a single identity, matching the
    /// one shared subscription that actually backs them.
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

    /// `inp_identities` backs `link_names` (the lset enumeration
    /// contract). It must (a) report each DISTINCT INP monitor variant —
    /// two links to the same upstream PV with different `queue_size` /
    /// `pipeline` are distinct subscriptions and each must surface its own
    /// identity so a per-variant `is_connected` lands correctly;
    /// (b) collapse links that share the exact `(pv, pipeline, Q)` variant
    /// (the registry already folds `field`/`sevr` differences onto one
    /// entry); and (c) exclude OUT links (no monitor signal).
    /// `insert_for_test` seeds links without a PVA server.
    ///
    /// Pre-fix this deduped by bare PV name, collapsing the `Q=1` and
    /// `Q=8` variants of `A:PV` into one identity — so an `is_connected`
    /// query could land on the wrong variant's monitor.
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
    /// `epicsStrGlobMatch(precord->name, ...)` (`pvxs/ioc/pvalink.cpp:216-231`).
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
    /// (`pvxs/ioc/pvalink_lset.cpp:99-131`, `pvxs/ioc/pvalink_channel.cpp:127-184`).
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

    /// Stage 0 gate: the registry owns ONE [`PvaClient`] for the whole IOC,
    /// so links that are deliberately *distinct* still cost the upstream IOC
    /// exactly **one** TCP connection.
    ///
    /// Two INP links to the same `pv_name` with different `queue_size` are
    /// different `pvRequest`s — pvxs `channels_key_t = (channelName,
    /// pvRequest)` (`pvxs/ioc/pvalink.h:116-120`) — hence distinct
    /// `RegistryKey`s and distinct `PvaLink`s, each with its own monitor
    /// subscription. What they must NOT have is distinct transports: both
    /// resolve through the one client's `ConnectionPool`, which is keyed on
    /// `SocketAddr`, so the server sees a single peer.
    ///
    /// Pre-fix, `PvaLink::open` built a `PvaClient` per link
    /// (`link.rs:297`), giving each link its own pool — the server saw
    /// TWO peers. On the RTEMS target that doubling is the resource
    /// ceiling, not a cosmetic cost (design §2.4, §4.3).
    ///
    /// The client is pinned at the test server's address, so this
    /// exercises the connection pool without any UDP search.
    // Drives two live pvalink monitor subscriptions to a real upstream server;
    // the link's monitor task now spawns onto the reactor-less callback pool
    // under `exec_backend`, where the client's `tokio::net` transport cannot
    // run. Reactor-dependent — gated out on the exec backend (stage 3).
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stage0_distinct_link_variants_share_one_upstream_connection() {
        use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
        use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};

        let desc = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        };
        let initial = PvField::Structure(PvStructure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), PvField::Scalar(ScalarValue::Double(1.0)))],
        });
        let pv = SharedPV::build_mailbox();
        pv.open(desc, initial).unwrap();
        let source = SharedSource::new();
        source.add("STAGE0:SHARED:PV", pv.clone());
        let server = PvaServer::isolated(Arc::new(source)).expect("test PVA server must start");
        let addr = server.tcp_addr();

        // The single IOC-wide client — pvxs `linkGlobal->provider_remote`.
        let reg = PvaLinkRegistry::with_client(
            PvaClient::builder()
                .server_addr(addr)
                .timeout(Duration::from_secs(3))
                .build(),
        );

        let cfg_q1 = PvaLinkConfig {
            monitor: true,
            queue_size: 1,
            ..PvaLinkConfig::defaults_for("STAGE0:SHARED:PV", LinkDirection::Inp)
        };
        let cfg_q8 = PvaLinkConfig {
            monitor: true,
            queue_size: 8,
            ..PvaLinkConfig::defaults_for("STAGE0:SHARED:PV", LinkDirection::Inp)
        };

        let link_q1 = reg.get_or_open(cfg_q1).await.expect("open the Q=1 link");
        let link_q8 = reg.get_or_open(cfg_q8).await.expect("open the Q=8 link");

        assert!(
            !Arc::ptr_eq(&link_q1, &link_q8),
            "a different queueSize is a different pvRequest — the two links \
             must stay distinct registry entries, or this gate would be \
             asserting nothing"
        );
        assert_eq!(reg.len(), 2, "two distinct channel owners cached");

        // The peer count only means something once both monitors have
        // actually reached the server; each proves liveness by delivering
        // its first event.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline
            && !(link_q1.is_connected() && link_q8.is_connected())
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            link_q1.is_connected() && link_q8.is_connected(),
            "both monitor subscriptions must connect before the connection \
             count is meaningful (Q=1 connected: {}, Q=8 connected: {})",
            link_q1.is_connected(),
            link_q8.is_connected()
        );

        assert_eq!(
            server.report().peer_count,
            1,
            "two distinct pvalink channels sharing the IOC-wide client must \
             cost the upstream IOC exactly ONE TCP connection"
        );
    }
}
