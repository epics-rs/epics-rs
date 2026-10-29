//! Process-wide registry of open [`PvaLink`]s, keyed by PV name + pvRequest.
//!
//! Used by record handlers so multiple records pointing at the same PV
//! share a single underlying client connection.

// RTEMS-EXEC-MODEL-ALLOW(5): checked - these run and pass in the exec-backend
// suite. (1 live-upstream monitor test gated out on the exec backend below;
// §4.2, stage 3.)

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use crate::client::PvaClient;
use epics_base_rs::types::RecordName;
use parking_lot::RwLock;
use tokio::sync::Notify;

use super::config::PvaLinkConfig;
use super::link::{PvaLink, PvaLinkResult};

mod opener;
pub(crate) use opener::ChannelOpener;

/// Operation timeout of the IOC-wide pvalink client.
///
/// The value every link used to build its own client with, kept explicit
/// so it does not silently follow `PvaClientBuilder`'s default. pvalink
/// exposes **no** per-link timeout option (neither does pvxs's
/// `pvaLinkConfig`), so one client-level timeout is the whole surface; a
/// per-link timeout, if one is ever added, must be passed to the
/// per-operation call instead of forking the shared client. A link's put
/// is not bounded by it: it ends at its result, the put replacing it or the
/// channel's disconnect, as pvxs's.
const PVALINK_CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

/// Registry cache key.
///
/// `(pv_name, pipeline, queue_size)` is pvxs's `channels_key_t =
/// (channelName, pvRequest)`, the pvRequest encoding `pipeline` and
/// `queueSize` (`pvxs/ioc/pvalink_lset.cpp:99-100`, `pvxs/ioc/pvalink.h:116`).
/// No direction: pvxs opens the same channel for an input and an output
/// link, so they share its monitor, its put and its counters. The key
/// deliberately does NOT include the per-link options (`field`,
/// `process`, `defer`, `retry`): pvxs caches one shared
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
/// travel with each write via [`PvaLink::put_out_field`] rather than
/// being baked into the channel
/// identity (see `link.rs`'s `Outbox`).
///
/// INP per-link `field` / `sevr` / `time` divergence is likewise
/// handled at the getter level (see `integration.rs`), so INP links
/// that differ only in those collapse onto one monitor subscription.
type RegistryKey = (String, bool, usize);

/// A `RegistryKey` a lookup can borrow, so the lset's per-cycle lookups
/// build no `String`. Hashes as the owned tuple does: a `String` hashes
/// as its `str`, and a tuple hashes field by field.
trait KeyView {
    fn view(&self) -> (&str, bool, usize);
}

impl KeyView for RegistryKey {
    fn view(&self) -> (&str, bool, usize) {
        (&self.0, self.1, self.2)
    }
}

impl KeyView for (&str, bool, usize) {
    fn view(&self) -> (&str, bool, usize) {
        *self
    }
}

impl<'a> Borrow<dyn KeyView + 'a> for RegistryKey {
    fn borrow(&self) -> &(dyn KeyView + 'a) {
        self
    }
}

impl PartialEq for dyn KeyView + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.view() == other.view()
    }
}

impl Eq for dyn KeyView + '_ {}

impl Hash for dyn KeyView + '_ {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.view().hash(state)
    }
}

/// One link on a channel: a record field's `pvaLink` on
/// `pvaLinkChannel::links` (`pvxs/ioc/pvalink.h:175-179`), with the
/// options it was opened with.
#[derive(Debug, Clone)]
pub struct ChannelLink {
    pub record: RecordName,
    pub field: Box<str>,
    pub config: Arc<PvaLinkConfig>,
}

/// One channel as pvxs `dbpvxr` prints it (`pvxs/ioc/pvalink.cpp:243-303`).
#[derive(Debug, Clone)]
pub struct ChannelDiag {
    pub pv_name: String,
    pub connected: bool,
    pub pipeline: bool,
    pub queue_size: usize,
    pub num_disconnect: usize,
    pub num_type_change: usize,
    /// pvxs `op_put` is set.
    pub put: bool,
    /// In pvxs `LinkSort` order: `monorder`, then attach order. Empty for
    /// a channel opened with no record (`pvxr`).
    pub links: Vec<ChannelLink>,
}

/// Cached PvaLink. Returns the same `Arc<PvaLink>` for repeated
/// `(pv_name, pipeline, queue_size)` tuples.
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
    /// pvxs `pvaLinkChannel::links` of each channel, in attach order.
    /// [`Self::attach_link`] adds, [`Self::detach_link`] removes.
    channel_links: RwLock<HashMap<RegistryKey, Vec<ChannelLink>>>,
}

/// Build the `RegistryKey` for a config — the shared
/// `(channelName, pvRequest)` identity pvxs caches by.
fn key_of(config: &PvaLinkConfig) -> RegistryKey {
    (config.pv_name.clone(), config.pipeline, config.queue_size)
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
            channel_links: RwLock::new(HashMap::new()),
        }
    }

    /// STAGE-5 PROBE: the IOC-wide client, so a target image with no shell
    /// can print `PvaClient::report()` on its console — the only way to
    /// observe "one upstream connection regardless of link count" from the
    /// guest side.
    pub fn client(&self) -> &PvaClient {
        &self.client
    }

    /// `record.field`'s link joins the channel `config` names, which may not
    /// be open yet: pvxs's `pvaLink` entering `pvaLinkChannel::links`.
    pub fn attach_link(&self, config: &Arc<PvaLinkConfig>, record: &RecordName, field: &str) {
        self.channel_links
            .write()
            .entry(key_of(config))
            .or_default()
            .push(ChannelLink {
                record: record.clone(),
                field: field.into(),
                config: config.clone(),
            });
    }

    /// Undo [`Self::attach_link`]: pvxs `~pvaLink` erasing itself from
    /// `pvaLinkChannel::links` (`pvxs/ioc/pvalink_link.cpp:27-35`). The
    /// last link leaving closes the channel, as `~pvaLinkChannel` erases
    /// it from `linkGlobal->channels` (`pvxs/ioc/pvalink_channel.cpp:95-99`);
    /// `true` then.
    pub fn detach_link(&self, config: &PvaLinkConfig, record: &RecordName, field: &str) -> bool {
        let mut channels = self.channel_links.write();
        let key = key_of(config);
        let Some(links) = channels.get_mut(&key) else {
            return false;
        };
        if let Some(at) = links
            .iter()
            .position(|l| l.record == *record && *l.field == *field)
        {
            links.remove(at);
        }
        if !links.is_empty() {
            return false;
        }
        channels.remove(&key);
        self.map.write().remove(&key);
        true
    }

    /// Exact lookup for the full config's `RegistryKey`. Returns
    /// `None` when no link with that exact key has been opened.
    pub fn try_get(&self, config: &PvaLinkConfig) -> Option<Arc<PvaLink>> {
        self.try_get_channel(&config.pv_name, config.pipeline, config.queue_size)
    }

    /// Exact lookup by channel identity `(pv_name, pipeline,
    /// queue_size)`, pvxs's `channels_key_t = (channelName, pvRequest)`
    /// (`pvxs/ioc/pvalink.h:116-120`). Resolver hot paths use this
    /// instead of a bare-PV-name match so a `Q=1` record reads from its
    /// own monitor, never from a `Q=64` sibling that merely shares the
    /// upstream PV name.
    pub fn try_get_channel(
        &self,
        pv_name: &str,
        pipeline: bool,
        queue_size: usize,
    ) -> Option<Arc<PvaLink>> {
        self.lookup((pv_name, pipeline, queue_size))
    }

    fn lookup(&self, key: (&str, bool, usize)) -> Option<Arc<PvaLink>> {
        self.map.read().get(&key as &dyn KeyView).cloned()
    }

    /// Get an existing link or open a new one; private so every open goes
    /// through [`ChannelOpener::open`]'s `local` gate. Concurrent calls
    /// for the same key share one [`PvaLink::open`] invocation;
    /// the second caller awaits via `pending` and reads the
    /// winner's cached entry.
    async fn get_or_open(&self, config: PvaLinkConfig) -> PvaLinkResult<Arc<PvaLink>> {
        let key = key_of(&config);

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
        self.channel_links.write().clear();
    }

    /// Test-only: insert a pre-built [`PvaLink`] under its config's
    /// `RegistryKey`. Lets a test seed a cached link (e.g. an INP
    /// link with a pre-populated value) without standing up a PVA
    /// server, so the resolver getter paths can be exercised against
    /// a known cache state.
    #[cfg(test)]
    pub(crate) fn insert_for_test(&self, config: &PvaLinkConfig, link: Arc<PvaLink>) {
        self.map.write().insert(key_of(config), link);
    }

    pub fn len(&self) -> usize {
        self.map.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The identities `(pv_name, pipeline, queue_size)` of every open
    /// channel, sorted for a stable order.
    ///
    /// Backs [`epics_base_rs::server::database::LinkSet::link_names`] (the `LinkSet` enumeration
    /// contract): each identity round-trips to an `is_connected(name)`
    /// query that lands on that channel's own monitor. (This enumeration is
    /// not consumed by the iocInit wait: that is CA-facility only and
    /// pvalink never blocks init — pvxs parity.)
    ///
    /// Two links to the same upstream PV with different `pipeline` /
    /// `queue_size` are DISTINCT channels (distinct subscriptions in pvxs,
    /// `pvxs/ioc/pvalink.h:116-120`), so each is reported separately.
    pub fn identities(&self) -> Vec<(String, bool, usize)> {
        let mut ids: Vec<(String, bool, usize)> = self.map.read().keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Per-channel diagnostics snapshot for every open channel. Backs the
    /// `dbpvar` IOC shell command, the Rust counterpart of pvxs `dbpvxr`
    /// (`pvxs/ioc/pvalink.cpp:184-316`), which iterates
    /// `linkGlobal->channels` in its `std::map` order: by name, then by
    /// the formatted pvRequest, whose `pipeline` precedes its decimal
    /// `queueSize` (`pvxs/ioc/pvalink_link.cpp:49-66`).
    pub fn channel_diagnostics(&self) -> Vec<ChannelDiag> {
        let links = self.channel_links.read();
        let mut rows: Vec<(String, ChannelDiag)> = self
            .map
            .read()
            .iter()
            .map(|(key, link)| {
                let (pv, pipeline, qsize) = key;
                let mut links = links.get(key).cloned().unwrap_or_default();
                links.sort_by_key(|l| l.config.monorder);
                let diag = ChannelDiag {
                    pv_name: pv.clone(),
                    connected: link.is_connected(),
                    pipeline: *pipeline,
                    queue_size: *qsize,
                    num_disconnect: link.num_disconnect(),
                    num_type_change: link.num_type_change(),
                    put: link.put_in_flight(),
                    links,
                };
                (qsize.to_string(), diag)
            })
            .collect();
        rows.sort_by(|(qa, a), (qb, b)| {
            (&a.pv_name, a.pipeline, qa).cmp(&(&b.pv_name, b.pipeline, qb))
        });
        rows.into_iter().map(|(_, diag)| diag).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::config::{LinkDirection, ProcMode};
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

    /// `identities` backs `link_names` (the lset enumeration contract):
    /// one identity per channel, so two links to one PV with different
    /// `queue_size` each surface, while links folded onto one channel —
    /// a `field`-only variant, or an output link with the same options —
    /// surface once.
    #[tokio::test]
    async fn identities_are_one_per_channel() {
        let reg = PvaLinkRegistry::new();
        let a1 = PvaLinkConfig {
            queue_size: 1,
            ..PvaLinkConfig::defaults_for("A:PV", LinkDirection::Inp)
        };
        let a8 = PvaLinkConfig {
            queue_size: 8,
            ..PvaLinkConfig::defaults_for("A:PV", LinkDirection::Inp)
        };
        let a1_field = PvaLinkConfig {
            field: "alarm.severity".to_string(),
            ..a1.clone()
        };
        let a1_out = PvaLinkConfig {
            direction: LinkDirection::Out,
            ..a1.clone()
        };
        let b = PvaLinkConfig::defaults_for("B:PV", LinkDirection::Out);
        for cfg in [&a1, &a8, &a1_field, &a1_out, &b] {
            reg.insert_for_test(cfg, Arc::new(PvaLink::for_test(cfg.clone(), None)));
        }
        let default_q = b.queue_size;
        assert_eq!(
            reg.identities(),
            vec![
                ("A:PV".to_string(), false, 1),
                ("A:PV".to_string(), false, 8),
                ("B:PV".to_string(), false, default_q),
            ]
        );
    }

    /// `channel_diagnostics` lists channels in pvxs's `channels` map
    /// order: by name, then by the formatted pvRequest, where `pipeline`
    /// comes first and `queueSize` compares as decimal text, so `Q=10`
    /// sorts before `Q=4`.
    #[tokio::test]
    async fn channel_diagnostics_follow_the_pvxs_channel_order() {
        let reg = PvaLinkRegistry::new();
        let at = |pv: &str, pipeline: bool, queue_size: usize| PvaLinkConfig {
            pipeline,
            queue_size,
            ..PvaLinkConfig::defaults_for(pv, LinkDirection::Inp)
        };
        for cfg in [
            at("B", false, 4),
            at("A", true, 2),
            at("A", false, 4),
            at("A", false, 10),
            at("A", false, 5),
        ] {
            reg.insert_for_test(&cfg, Arc::new(PvaLink::for_test(cfg.clone(), None)));
        }
        let order: Vec<(String, bool, usize)> = reg
            .channel_diagnostics()
            .into_iter()
            .map(|d| (d.pv_name, d.pipeline, d.queue_size))
            .collect();
        assert_eq!(
            order,
            [
                ("A", false, 10),
                ("A", false, 4),
                ("A", false, 5),
                ("A", true, 2),
                ("B", false, 4),
            ]
            .map(|(pv, pipe, q)| (pv.to_string(), pipe, q))
        );
    }

    /// A channel's links as pvxs `pvaLinkChannel::links` holds them: one per
    /// record field, whatever options fold onto the channel, in `LinkSort`
    /// order (`monorder`, then attach order; `pvxs/ioc/pvalink_channel.cpp:82-86`).
    #[tokio::test]
    async fn channel_diagnostics_lists_each_link() {
        let reg = PvaLinkRegistry::new();
        let inp = Arc::new(PvaLinkConfig::defaults_for("SRC:PV", LinkDirection::Inp));
        let inp_field = Arc::new(PvaLinkConfig {
            field: "alarm.severity".to_string(),
            ..(*inp).clone()
        });
        let early = Arc::new(PvaLinkConfig {
            monorder: -1,
            ..(*inp).clone()
        });
        reg.insert_for_test(&inp, Arc::new(PvaLink::for_test((*inp).clone(), None)));

        let rec = |name: &[u8]| RecordName::from(name);
        reg.attach_link(&inp, &rec(b"REC:B"), "INPA");
        reg.attach_link(&inp_field, &rec(b"REC:A"), "INP");
        reg.attach_link(&inp, &rec(b"REC:B"), "INPB");
        reg.attach_link(&inp, &rec(b"caf\xe9"), "INP");
        reg.attach_link(&early, &rec(b"REC:Z"), "INP");
        assert!(!reg.detach_link(&inp, &rec(b"REC:B"), "INPB"));

        let links = |reg: &PvaLinkRegistry| -> Vec<(RecordName, String, String)> {
            reg.channel_diagnostics()
                .into_iter()
                .flat_map(|d| d.links)
                .map(|l| (l.record, l.field.to_string(), l.config.field.clone()))
                .collect()
        };
        assert_eq!(
            links(&reg),
            [
                (rec(b"REC:Z"), "INP", ""),
                (rec(b"REC:B"), "INPA", ""),
                (rec(b"REC:A"), "INP", "alarm.severity"),
                (rec(b"caf\xe9"), "INP", ""),
            ]
            .map(|(r, f, c)| (r, f.to_string(), c.to_string()))
        );

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
        // An input link with the same pvRequest is the same channel too
        // (`pvxs/ioc/pvalink_lset.cpp:99-131` keys no direction).
        let cfg_inp = PvaLinkConfig::defaults_for("MR_R14:PV", LinkDirection::Inp);
        let link_inp = reg.get_or_open(cfg_inp).await.expect("open INP link");
        assert!(Arc::ptr_eq(&link_a, &link_inp));
        assert_eq!(reg.len(), 1);
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
        use crate::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
        use crate::server_native::{PvaServer, SharedPV, SharedSource};

        let desc = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))].into(),
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
