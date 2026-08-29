//! [`CompositeSource`] — multi-source registry mirroring pvxs's
//! `Server::addSource(name, src, order)` model. Sources are kept in a
//! priority-sorted list and dispatched in order on each PV-name lookup.
//!
//! Sources are keyed by `(order, name)` and consulted in ascending key
//! order — the same key pvxs uses for its registry
//! (`std::map<std::pair<int, std::string>, ...>`, `serverconn.h:267`,
//! inserted at `src/server.cpp:91`, iterated at `src/server.cpp:696` and
//! `serverchan.cpp:304`). Lower `order` is tried first (`order=0` is the
//! default) and an equal-`order` tie is broken by byte-wise source NAME,
//! never by registration order. Source names beginning with "__" are
//! reserved for internal use (pvxs convention) — `__builtin` is a
//! [`crate::server_native::SharedSource`] for
//! [`add`](crate::server_native::SharedSource::add) /
//! [`remove`](crate::server_native::SharedSource::remove) convenience.
//!
//! For each request the first source whose `has_pv()` returns `true`
//! wins all subsequent calls (`get_value`, `subscribe`, `put_value`,
//! `rpc`, `is_writable`, `get_introspection`). `list_pvs()` is the
//! union of every source's PV list.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::pvdata::{FieldDesc, PvField, RpcReply};

use super::source::{
    AccessChecked, ChannelInvalidator, ChannelSource, DynSource, MonitorStream, OpError,
    RawMonitorEvent,
};

/// Multi-source registry. Wrap with `Arc` and feed to
/// `PvaServer::start`.
pub struct CompositeSource {
    /// `(order, name) -> source`, ordered by construction: a
    /// `BTreeMap` iterates its keys ascending, so the consultation
    /// order pvxs gets from `std::map<std::pair<int, std::string>, ...>`
    /// cannot be lost by a caller forgetting to re-sort after an insert.
    entries: Arc<parking_lot::RwLock<BTreeMap<(i32, String), DynSource>>>,
    /// The composite's gate is
    /// an aggregator whose `acl_version()` is the `wrapping_sum`
    /// of every inner gate's version (NOT `max(...)`: a
    /// max-based aggregate produced false
    /// negatives when a smaller inner bumped under the existing
    /// peak). The aggregate is a **change signal only**: a tcp.rs
    /// monitor task compares the captured-at-subscribe version
    /// against the live aggregate on every event; on mismatch it
    /// re-checks READ access through the matched inner source's
    /// gate via `ChannelSource::revalidate_read` (the
    /// authoritative owner — the composite's own `access()` gate
    /// is permissive and is NOT consulted for allow/deny on the
    /// reload path).
    access_gate: epics_base_rs::server::access_security::AccessGate,
    /// Monotonic registry-topology counter, mirroring pvxs
    /// `Server::pvt->beaconChange` (src/server.cpp:90-115). Bumped on every
    /// [`Self::add_source`] / [`Self::remove_source`] and surfaced
    /// through [`ChannelSource::beacon_change`] so the UDP beacon task
    /// advances the beacon `change_count` on a registry mutation even
    /// when the enumerated PV-name set is unchanged.
    beacon_change: Arc<AtomicU64>,
}

impl Default for CompositeSource {
    fn default() -> Self {
        use epics_base_rs::server::access_security::AccessGate;
        let entries: Arc<parking_lot::RwLock<BTreeMap<(i32, String), DynSource>>> =
            Arc::new(parking_lot::RwLock::new(BTreeMap::new()));
        let entries_for_version = entries.clone();
        // the composite's aggregate
        // version is the `wrapping_sum` of every inner gate's
        // `acl_version()`. The earlier `max(...)` shape produced
        // false negatives — a monitor whose subscribe-time snapshot
        // captured the existing max would miss a bump on a
        // *different* inner whose pre-bump version was below the
        // max (e.g. A=5, B=0 yields max=5; B's set_acf bumps B to
        // 1 but max stays 5, so the monitor's `live == stored`
        // compare on the next event never fires). Per-inner
        // versions only ever monotonically `fetch_add`, so summing
        // them gives strict change-detection: any inner bump shifts
        // the sum; only "no inner moved" keeps it constant.
        // `wrapping_add` covers the astronomical-overflow case
        // (≥ 2^64 cumulative bumps) and the monitor uses
        // `live != stored` rather than `>`, so a wrap-around still
        // triggers a re-check on the next event.
        let access_gate = AccessGate::open_with_aggregator(Arc::new(move || {
            let mut sum: u64 = 0;
            for source in entries_for_version.read().values() {
                sum = sum.wrapping_add(source.access_gate().acl_version());
            }
            sum
        }));
        Self {
            entries,
            access_gate,
            beacon_change: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl CompositeSource {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Add a source under the key `(order, name)`. Errors when that key
    /// is already present — pvxs convention so callers notice
    /// double-registration (`src/server.cpp:91-93`). Higher priority = lower
    /// `order`; two sources sharing an `order` are consulted in byte-wise
    /// name order, exactly as pvxs's `std::map<std::pair<int,
    /// std::string>, ...>` does (`serverconn.h:268`). pvxs keeps its own
    /// internals at `order = -1` (`src/server.cpp:542-546`) and application
    /// sources default to `order = 0` (`pvxs/server.h:116-118`).
    pub fn add_source(&self, name: &str, source: DynSource, order: i32) -> Result<(), String> {
        let mut e = self.entries.write();
        match e.entry((order, name.to_string())) {
            Entry::Occupied(_) => {
                return Err(format!("source ({name}, {order}) already registered"));
            }
            Entry::Vacant(slot) => {
                slot.insert(source);
            }
        }
        // pvxs `Server::addSource` bumps `beaconChange` unconditionally
        // (src/server.cpp:90-96) so the next BEACON signals the topology
        // change. Bump after the successful insert only.
        self.beacon_change.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Remove and return the source previously added with the given
    /// (`name`, `order`) tuple. Returns `None` when not found.
    pub fn remove_source(&self, name: &str, order: i32) -> Option<DynSource> {
        let mut e = self.entries.write();
        let removed = e.remove(&(order, name.to_string()));
        // pvxs `Server::removeSource` bumps `beaconChange` OUTSIDE the
        // `if(it!=pvt->sources.end())` that performs the erase
        // (src/server.cpp:109-113), so a remove naming an absent
        // (`name`, `order`) still advances the counter. Unconditional
        // here for the same reason: the caller asked for a topology
        // change, and pvxs lets the next BEACON carry a new change count
        // either way.
        self.beacon_change.fetch_add(1, Ordering::Relaxed);
        removed
    }

    /// Current registry beacon-change counter. Mirrors pvxs
    /// `Server::pvt->beaconChange` (src/server.cpp:90-115).
    ///
    /// pvxs keeps a SINGLE counter that both `addSource` / `removeSource`
    /// AND the built-in `StaticSource` registry (`addPV` / `removePV`)
    /// bump (`src/server.cpp:95,113,180,189`). To reproduce that single-counter
    /// view across the Rust source tree, fold every inner source's own
    /// [`ChannelSource::beacon_change`] into this composite's local
    /// add/remove counter. Otherwise a built-in
    /// [`SharedSource`](crate::server_native::SharedSource) `add` / `remove`
    /// (the Rust analog of `addPV` / `removePV`) would leave the beacon
    /// `change_count` unchanged because the composite's own add/remove
    /// counter did not move.
    ///
    /// `wrapping_add` over inner counters (not `max`) for the same reason
    /// the access-gate aggregator uses it (see [`Self::default`]): any
    /// single inner bump shifts the sum, so the beacon task's
    /// `live != stored` comparison reliably detects the change; a `max`
    /// fold would miss a bump on an inner whose value stayed below the
    /// peak.
    pub fn beacon_change(&self) -> u64 {
        let mut sum = self.beacon_change.load(Ordering::Relaxed);
        for source in self.entries.read().values() {
            sum = sum.wrapping_add(source.beacon_change());
        }
        sum
    }

    /// Look up a previously added source by (name, order).
    pub fn get_source(&self, name: &str, order: i32) -> Option<DynSource> {
        self.entries.read().get(&(order, name.to_string())).cloned()
    }

    /// (name, order) for every registered source, in consultation
    /// order — debug helper.
    pub fn list_source(&self) -> Vec<(String, i32)> {
        self.entries
            .read()
            .keys()
            .map(|(order, name)| (name.clone(), *order))
            .collect()
    }

    fn snapshot(&self) -> Vec<DynSource> {
        self.entries.read().values().cloned().collect()
    }

    /// single owner of credentialed inner-source selection
    /// for every `*_checked` operation. Resolves the matched source via
    /// `has_pv_checked` — never the credential-free `has_pv` — so a
    /// gateway inner source opens/refreshes upstream state under THIS
    /// peer's identity, then mints the inner `AccessChecked` through that
    /// source's own gate.
    ///
    /// Centralising the find-loop here is the structural closure for the
    /// FR-8 family: the cited leak was CREATE_CHANNEL/GET_FIELD calling
    /// the credential-free `has_pv`, but the same leak existed in each
    /// `*_checked` method's hand-rolled selection loop. Routing all of
    /// them through one helper makes "credentialed selection always uses
    /// `has_pv_checked`" hold by construction — a future `*_checked`
    /// method cannot reintroduce the shared-identity leak by selecting
    /// with plain `has_pv`.
    async fn resolve_checked(
        sources: Vec<DynSource>,
        name: &str,
        ctx: &crate::server_native::source::ChannelContext,
    ) -> Option<(DynSource, AccessChecked)> {
        for src in sources {
            if src.has_pv_checked(name, ctx.clone()).await {
                let inner = src
                    .access_gate()
                    .check(
                        name,
                        &ctx.creds.host,
                        &ctx.creds.account,
                        &ctx.creds.method,
                        &ctx.creds.authority,
                    )
                    .await;
                return Some((src, inner));
            }
        }
        None
    }
}

impl ChannelSource for CompositeSource {
    fn access(&self) -> &epics_base_rs::server::access_security::AccessGate {
        &self.access_gate
    }

    /// Surface the aggregated registry beacon-change counter so the UDP
    /// beacon task advances its `change_count` on a source add/remove AND
    /// on a built-in `SharedSource` PV add/remove (pvxs `beaconChange`,
    /// src/server.cpp:90-115). Delegates to the inherent
    /// [`CompositeSource::beacon_change`], which folds inner sources.
    fn beacon_change(&self) -> u64 {
        CompositeSource::beacon_change(self)
    }

    /// Monitor-reload READ
    /// revalidation owner for composite sources.
    ///
    /// The composite's `access()` gate is an `open_with_aggregator`
    /// — its `acl_version()` correctly reports "some inner moved"
    /// (wrapping-sum aggregate), but its `check()` is the
    /// permissive `Open` variant and is NOT authoritative for
    /// allow/deny. Pre-fix tcp.rs's monitor reload loop called
    /// `src.access_gate().check(...)` on the composite after a
    /// version mismatch and always got `ReadWrite`, so a child's
    /// `set_acf` flipping to deny was detected (version) but not
    /// honoured (still served events).
    ///
    /// Closure: re-resolve the matched inner source by name (same
    /// shape as `get_value_checked` / `subscribe_checked`) and
    /// route the check through its gate — that's the gate that
    /// served the subscription and is the single authoritative
    /// owner of the allow/deny transition.
    fn revalidate_read(
        &self,
        pv_name: &str,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<
        Output = Option<epics_base_rs::server::access_security::AccessChecked>,
    > + Send {
        let name = pv_name.to_string();
        let snapshot = self.snapshot();
        async move {
            for src in snapshot {
                // select under the downstream peer's identity
                // (`has_pv_checked`), not credential-free `has_pv` — a
                // gateway inner source must resolve the revalidation target
                // against THIS peer's upstream state, matching the
                // credentialed `subscribe_checked` that opened the monitor.
                if src.has_pv_checked(&name, ctx.clone()).await {
                    // Delegate to the matched source's OWN
                    // `revalidate_read`, not `access_gate().check()`
                    // directly. For a leaf source the default
                    // `revalidate_read` is exactly `gate.check(...)`,
                    // so behaviour is unchanged. But when the matched
                    // source is itself a nested `CompositeSource`
                    // (e.g. `PvaServer::start` wraps the user
                    // composite together with the built-in
                    // `__server` source), its `access_gate()` is a
                    // permissive `open_with_aggregator` — calling
                    // `check()` on it would always return
                    // `ReadWrite` and silently re-allow a denied
                    // monitor. Recursing through `revalidate_read`
                    // makes the nested composite resolve down to the
                    // authoritative leaf gate.
                    return src.revalidate_read(&name, ctx.clone()).await;
                }
            }
            None
        }
    }

    fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
        let sources = self.snapshot();
        async move {
            let mut all: Vec<String> = Vec::new();
            for src in sources {
                all.extend(src.list_pvs().await);
            }
            all.sort();
            all.dedup();
            all
        }
    }

    fn has_pv(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
        let sources = self.snapshot();
        let name = name.to_string();
        async move {
            for src in sources {
                if src.has_pv(&name).await {
                    return true;
                }
            }
            false
        }
    }

    /// A name is SEARCH-advertised iff *any* source claims it — the OR
    /// of `searchable` across every source.
    ///
    /// SEARCH aggregation is separate from CREATE/GET/PUT/RPC ownership.
    /// pvxs runs `Source::onSearch` on every registered source and lets
    /// each independently mark its claim in the shared `Search` object
    /// (`src/server.cpp:694-712`, `serverchan.cpp:212-221`); a name is
    /// answered when at least one source claims it. The built-in
    /// `ServerSource::onSearch` is empty (`serversource.cpp:25-28`), so
    /// the diagnostic `server` PV claims nothing and never *suppresses*
    /// another source — it simply does not contribute a claim of its own.
    ///
    /// Routing SEARCH to the first hosting source instead would let the
    /// front-ordered, non-searchable `ServerInfoSource` veto a later
    /// user PV that happens to be named `server`, which pvxs advertises.
    /// CREATE/GET/PUT/RPC keep the first-owner rule (lowest order wins,
    /// so `__server` owns the literal `server` channel); only SEARCH ORs.
    ///
    /// `searchable` is asked ALONE — not `has_pv(name) && searchable(name)`.
    /// pvxs's `onSearch` and `onCreate` are independent callbacks and a
    /// source may legitimately claim more at search than it will serve at
    /// create: `SingleSource` claims every name `dbChannelTest` resolves, then
    /// refuses the ones whose field has no NT (`ORACLE:AI.MLOK`, a
    /// `DBF_NOACCESS` field — see
    /// [`PvDatabaseSource::has_pv`](crate::server::PvDatabaseSource::has_pv)).
    /// ANDing `has_pv` in here made that shape unrepresentable and forced the
    /// two questions to share one answer. Sources that do not distinguish them
    /// are unaffected: [`ChannelSource::searchable`]'s default IS `has_pv`.
    fn searchable(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
        let sources = self.snapshot();
        let name = name.to_string();
        async move {
            for src in sources {
                if src.searchable(&name).await {
                    return true;
                }
            }
            false
        }
    }

    /// Endpoint-scoped counterpart of [`Self::searchable`]: the OR of
    /// `searchable_from` across every source, so a
    /// source can claim a PV only for some requesters (pvxs
    /// `Search::source()`, filled from the UDP reply dest /
    /// `src/server.cpp:674-704` or the TCP peer / `serverchan.cpp:197-222`).
    /// Same "any source may claim" rule as [`Self::searchable`] — a
    /// non-searchable hosting source must not veto a later source's
    /// claim, matching pvxs's per-source independent `onSearch`, and
    /// `searchable_from` is likewise asked alone (see [`Self::searchable`] for
    /// why `has_pv` is not ANDed in).
    fn searchable_from(
        &self,
        name: &str,
        requester: std::net::SocketAddr,
    ) -> impl std::future::Future<Output = bool> + Send {
        let sources = self.snapshot();
        let name = name.to_string();
        async move {
            for src in sources {
                if src.searchable_from(&name, requester).await {
                    return true;
                }
            }
            false
        }
    }

    fn get_introspection(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
        let name = name.to_string();
        let this = self.snapshot();
        async move {
            for src in this {
                if src.has_pv(&name).await {
                    return src.get_introspection(&name).await;
                }
            }
            None
        }
    }

    /// the find-loop itself routes through the inner
    /// `has_pv_checked` so a gateway source resolves existence under the
    /// downstream peer's identity — calling the credential-free `has_pv`
    /// here would re-open the shared-identity upstream cache that this
    /// fix exists to avoid.
    fn has_pv_checked(
        &self,
        name: &str,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = bool> + Send {
        let name = name.to_string();
        let this = self.snapshot();
        async move {
            for src in this {
                if src.has_pv_checked(&name, ctx.clone()).await {
                    return true;
                }
            }
            false
        }
    }

    /// credential-aware introspection routed to the matched
    /// inner source via `has_pv_checked`/`get_introspection_checked`, so
    /// descriptor discovery for a credentialed downstream peer never
    /// opens upstream state under the shared gateway identity.
    fn get_introspection_checked(
        &self,
        name: &str,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
        let name = name.to_string();
        let this = self.snapshot();
        async move {
            for src in this {
                if src.has_pv_checked(&name, ctx.clone()).await {
                    return src.get_introspection_checked(&name, ctx).await;
                }
            }
            None
        }
    }

    /// Forward the parking wait to the SAME inner source
    /// [`Self::get_introspection_checked`] would have asked, so a
    /// composite in front of a `SharedSource` parks where the leaf parks
    /// instead of collapsing "not open yet" into "no such PV".
    fn await_introspection(
        &self,
        name: &str,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
        let name = name.to_string();
        let this = self.snapshot();
        async move {
            for src in this {
                if src.has_pv_checked(&name, ctx.clone()).await {
                    return src.await_introspection(&name, ctx).await;
                }
            }
            None
        }
    }

    /// CREATE_CHANNEL owner binding: return the matched inner source so
    /// the channel dispatches every later operation to the source that
    /// accepted it, instead of re-resolving the registry per operation.
    /// Selection uses `has_pv_checked` — the same credentialed find as
    /// `Self::resolve_checked` — so the owner is chosen under the
    /// downstream peer's identity. Descends through a nested composite
    /// to its leaf owner so the bound source is always terminal.
    ///
    /// pvxs binds the accepting source's callbacks into the `ServerChan`
    /// at CREATE_CHANNEL (`serverchan.cpp:295-322`, `serverchan.cpp:70-112`)
    /// and a later `removeSource` does not rewrite them
    /// (`src/server.cpp:100-112`).
    fn resolve_owner(
        &self,
        name: &str,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Option<DynSource>> + Send {
        let name = name.to_string();
        let this = self.snapshot();
        async move {
            for src in this {
                if src.has_pv_checked(&name, ctx.clone()).await {
                    return Some(match src.resolve_owner(&name, ctx).await {
                        Some(leaf) => leaf,
                        None => src,
                    });
                }
            }
            None
        }
    }

    fn get_value(&self, name: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
        let name = name.to_string();
        let this = self.snapshot();
        async move {
            for src in this {
                if src.has_pv(&name).await {
                    return src.get_value(&name).await;
                }
            }
            None
        }
    }

    fn put_value(
        &self,
        name: &str,
        value: PvField,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let name = name.to_string();
        let this = self.snapshot();
        async move {
            for src in this {
                if src.has_pv(&name).await {
                    return src.put_value(&name, value).await;
                }
            }
            Err(OpError::failed(format!("no source serves '{name}'")))
        }
    }

    fn is_writable(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
        let name = name.to_string();
        let this = self.snapshot();
        async move {
            for src in this {
                if src.has_pv(&name).await {
                    return src.is_writable(&name).await;
                }
            }
            false
        }
    }

    fn subscribe(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send {
        let name = name.to_string();
        let this = self.snapshot();
        async move {
            for src in this {
                if src.has_pv(&name).await {
                    return src.subscribe(&name).await;
                }
            }
            None
        }
    }

    fn rpc(
        &self,
        name: &str,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> impl std::future::Future<Output = Result<RpcReply, OpError>> + Send {
        let name = name.to_string();
        let this = self.snapshot();
        async move {
            for src in this {
                if src.has_pv(&name).await {
                    return src.rpc(&name, request_desc, request_value).await;
                }
            }
            Err(OpError::failed(format!("no source serves '{name}'")))
        }
    }

    // composite has no single gate of its own — each
    // inner source carries its own. The `*_checked` overrides
    // resolve the matched source by name, then mint a fresh
    // AccessChecked via THAT source's gate before invoking its
    // typed op. The outer token passed into the composite is
    // effectively a permissive seal from the composite's Open
    // gate (which `access()` returns by default); only the inner
    // re-check is load-bearing for ACF.

    fn get_value_checked(
        &self,
        checked: AccessChecked,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Option<PvField>> + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            let (src, inner_checked) = Self::resolve_checked(this, &name, &ctx).await?;
            src.get_value_checked(inner_checked, ctx).await
        }
    }

    /// Resolve the OWNING source and return ITS read — value AND assigned
    /// leaves. Without this forward the trait default would call the
    /// composite's own `get_value_checked` (which does resolve the owner)
    /// but report `marked: None`, silently widening every GET / seed the
    /// top-level composite fronts — and `PvaServer::start` always wraps the
    /// user source in one.
    fn read_checked(
        &self,
        checked: AccessChecked,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Option<crate::server_native::source::SourceRead>> + Send
    {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            let (src, inner_checked) = Self::resolve_checked(this, &name, &ctx).await?;
            src.read_checked(inner_checked, ctx).await
        }
    }

    fn put_value_checked(
        &self,
        checked: AccessChecked,
        value: PvField,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            match Self::resolve_checked(this, &name, &ctx).await {
                Some((src, inner_checked)) => {
                    src.put_value_checked(inner_checked, value, ctx).await
                }
                None => Err(OpError::failed(format!("no source serves '{name}'"))),
            }
        }
    }

    /// Resolve the matched inner source, re-check WRITE through its
    /// gate, then forward to its `put_delta_checked` — so a composite
    /// over a `SharedSource` still gets the atomic read-merge-write
    /// (no TOCTOU lost update). Without this override the default
    /// trait impl would merge against `CompositeSource::get_value`
    /// and forward through `CompositeSource::put_value_checked` as
    /// two un-serialized ops, defeating the inner source's atomic
    /// `put_delta` primitive.
    fn put_delta_checked(
        &self,
        checked: AccessChecked,
        desc: std::sync::Arc<FieldDesc>,
        changed: crate::proto::BitSet,
        delta: &PvField,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            match Self::resolve_checked(this, &name, &ctx).await {
                Some((src, inner_checked)) => {
                    src.put_delta_checked(inner_checked, desc, changed, delta, ctx)
                        .await
                }
                None => Err(OpError::failed(format!("no source serves '{name}'"))),
            }
        }
    }

    /// Atomic PUT_GET routed to the resolving owner's `put_get_checked`.
    /// Without this override the trait default would decompose the op into
    /// `put_delta_checked` + `read_checked` on the composite itself,
    /// bypassing an owner source's single-op PUT_GET (e.g. the pva-gateway's
    /// one-upstream-PUT_GET forward). Re-mints the inner token through the
    /// owner's gate exactly as the other `_checked` forwarders do.
    fn put_get_checked(
        &self,
        checked: AccessChecked,
        desc: std::sync::Arc<FieldDesc>,
        changed: crate::proto::BitSet,
        delta: &PvField,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<
        Output = Result<Option<crate::server_native::source::SourceRead>, OpError>,
    > + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            match Self::resolve_checked(this, &name, &ctx).await {
                Some((src, inner_checked)) => {
                    src.put_get_checked(inner_checked, desc, changed, delta, ctx)
                        .await
                }
                None => Err(OpError::failed(format!("no source serves '{name}'"))),
            }
        }
    }

    fn subscribe_checked(
        &self,
        checked: AccessChecked,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            let (src, inner_checked) = Self::resolve_checked(this, &name, &ctx).await?;
            src.subscribe_checked(inner_checked, ctx).await
        }
    }

    fn subscribe_raw_checked(
        &self,
        checked: AccessChecked,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Option<MonitorStream<RawMonitorEvent>>> + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            let (src, inner_checked) = Self::resolve_checked(this, &name, &ctx).await?;
            src.subscribe_raw_checked(inner_checked, ctx).await
        }
    }

    /// forward the downstream monitor options to the matched
    /// inner source. A composite over a gateway source must carry
    /// `opts` through so the gateway can reject options it cannot
    /// honor across a fanout monitor. Decoded-`PvField` form, retained
    /// for API compatibility.
    fn subscribe_checked_opts(
        &self,
        checked: AccessChecked,
        ctx: crate::server_native::source::ChannelContext,
        opts: crate::server_native::source::MonitorOptions,
    ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            let (src, inner_checked) = Self::resolve_checked(this, &name, &ctx).await?;
            src.subscribe_checked_opts(inner_checked, ctx, opts).await
        }
    }

    /// cooked (`MonitorUpdate`) counterpart of
    /// [`Self::subscribe_checked_opts`] — the server's monitor dispatch
    /// path. Resolves the owning inner source and forwards `opts` so a
    /// gateway can reject options it cannot honor across a fanout monitor.
    fn subscribe_checked_opts_marked(
        &self,
        checked: AccessChecked,
        ctx: crate::server_native::source::ChannelContext,
        opts: crate::server_native::source::MonitorOptions,
    ) -> impl std::future::Future<
        Output = Option<MonitorStream<crate::server_native::source::MonitorUpdate>>,
    > + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            let (src, inner_checked) = Self::resolve_checked(this, &name, &ctx).await?;
            src.subscribe_checked_opts_marked(inner_checked, ctx, opts)
                .await
        }
    }

    /// Raw-path counterpart of [`Self::subscribe_checked_opts`].
    fn subscribe_raw_checked_opts(
        &self,
        checked: AccessChecked,
        ctx: crate::server_native::source::ChannelContext,
        opts: crate::server_native::source::MonitorOptions,
    ) -> impl std::future::Future<Output = Option<MonitorStream<RawMonitorEvent>>> + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            let (src, inner_checked) = Self::resolve_checked(this, &name, &ctx).await?;
            src.subscribe_raw_checked_opts(inner_checked, ctx, opts)
                .await
        }
    }

    /// Single-seed MONITOR (the server's monitor START dispatch path):
    /// resolve the owning inner source and forward to ITS
    /// `subscribe_seeded`, so a self-seeding inner (the PVA gateway's
    /// cached snapshot, a `SharedPV`'s atomic seed) supplies the
    /// connect-time seed rather than the composite's generic
    /// get_value-seeding default — which would bypass the inner's atomic
    /// seed and re-open the double-seed / gap-duplicate.
    fn subscribe_seeded(
        &self,
        checked: AccessChecked,
        ctx: crate::server_native::source::ChannelContext,
        opts: crate::server_native::source::MonitorOptions,
    ) -> impl std::future::Future<
        Output = Option<
            crate::server_native::source::SubscriptionSeed<
                crate::server_native::source::MonitorUpdate,
            >,
        >,
    > + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            let (src, inner_checked) = Self::resolve_checked(this, &name, &ctx).await?;
            src.subscribe_seeded(inner_checked, ctx, opts).await
        }
    }

    /// Raw-path counterpart of [`Self::subscribe_seeded`].
    fn subscribe_raw_seeded(
        &self,
        checked: AccessChecked,
        ctx: crate::server_native::source::ChannelContext,
        opts: crate::server_native::source::MonitorOptions,
    ) -> impl std::future::Future<
        Output = Option<
            crate::server_native::source::SubscriptionSeed<
                crate::server_native::source::RawMonitorEvent,
            >,
        >,
    > + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            let (src, inner_checked) = Self::resolve_checked(this, &name, &ctx).await?;
            src.subscribe_raw_seeded(inner_checked, ctx, opts).await
        }
    }

    fn rpc_checked(
        &self,
        checked: AccessChecked,
        request_desc: FieldDesc,
        request_value: PvField,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Result<RpcReply, OpError>> + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            match Self::resolve_checked(this, &name, &ctx).await {
                Some((src, inner_checked)) => {
                    src.rpc_checked(inner_checked, request_desc, request_value, ctx)
                        .await
                }
                None => Err(OpError::failed(format!("no source serves '{name}'"))),
            }
        }
    }

    fn subscribe_raw(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<MonitorStream<RawMonitorEvent>>> + Send {
        let name = name.to_string();
        let this = self.snapshot();
        async move {
            for src in this {
                if src.has_pv(&name).await {
                    return src.subscribe_raw(&name).await;
                }
            }
            None
        }
    }

    fn process(&self, name: &str) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let name = name.to_string();
        let this = self.snapshot();
        async move {
            for src in this {
                if src.has_pv(&name).await {
                    return src.process(&name).await;
                }
            }
            Err(OpError::failed(format!("no source serves '{name}'")))
        }
    }

    fn process_checked(
        &self,
        checked: AccessChecked,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            match Self::resolve_checked(this, &name, &ctx).await {
                Some((src, inner_checked)) => src.process_checked(inner_checked, ctx).await,
                None => Err(OpError::failed(format!("no source serves '{name}'"))),
            }
        }
    }

    // ChannelArray routed to the resolving owner. Without these the trait
    // default ("not supported") would mask a wrapped inner's array support
    // whenever a composite wraps it (e.g. the PVA gateway under a
    // `control_prefix`, where `CompositeSource` wraps the layered gateway
    // source) — the wrapper-severs-override defect family. INIT picks the
    // owner by credentialed existence (matching `resolve_owner`); the
    // sub-ops re-mint the access token through the owner's gate via
    // `resolve_checked`, exactly as `put_get_checked` / `process_checked` do.
    fn channel_array_init(
        &self,
        name: &str,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Result<FieldDesc, OpError>> + Send {
        let name = name.to_string();
        let this = self.snapshot();
        async move {
            for src in this {
                if src.has_pv_checked(&name, ctx.clone()).await {
                    return src.channel_array_init(&name, ctx).await;
                }
            }
            Err(OpError::failed(format!("no source serves '{name}'")))
        }
    }

    fn channel_array_get(
        &self,
        checked: AccessChecked,
        offset: u32,
        count: u32,
        stride: u32,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Result<PvField, OpError>> + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            match Self::resolve_checked(this, &name, &ctx).await {
                Some((src, inner_checked)) => {
                    src.channel_array_get(inner_checked, offset, count, stride, ctx)
                        .await
                }
                None => Err(OpError::failed(format!("no source serves '{name}'"))),
            }
        }
    }

    fn channel_array_put(
        &self,
        checked: AccessChecked,
        offset: u32,
        stride: u32,
        value: PvField,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            match Self::resolve_checked(this, &name, &ctx).await {
                Some((src, inner_checked)) => {
                    src.channel_array_put(inner_checked, offset, stride, value, ctx)
                        .await
                }
                None => Err(OpError::failed(format!("no source serves '{name}'"))),
            }
        }
    }

    fn channel_array_set_length(
        &self,
        checked: AccessChecked,
        length: u32,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            match Self::resolve_checked(this, &name, &ctx).await {
                Some((src, inner_checked)) => {
                    src.channel_array_set_length(inner_checked, length, ctx)
                        .await
                }
                None => Err(OpError::failed(format!("no source serves '{name}'"))),
            }
        }
    }

    fn channel_array_get_length(
        &self,
        checked: AccessChecked,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Result<u32, OpError>> + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            match Self::resolve_checked(this, &name, &ctx).await {
                Some((src, inner_checked)) => {
                    src.channel_array_get_length(inner_checked, ctx).await
                }
                None => Err(OpError::failed(format!("no source serves '{name}'"))),
            }
        }
    }

    fn monitor_watermarks(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<(usize, usize)>> + Send {
        // surface the watermark levels of the inner source
        // that serves `name`, so the server monitor loop drives the
        // pause/resume hysteresis and reaches the per-source
        // notify_watermark callback below.
        //
        // Resolve the OWNING source the same way every other op does —
        // first source whose `has_pv` claims `name`, in registration
        // order (the single-owner resolution `resolve_checked` enforces)
        // — and read ITS levels. The old `find_map(monitor_watermarks)`
        // returned the first source that reported ANY levels, which let a
        // catch-all source (the PVA gateway returns `Some((0,0))` for
        // every name, served or not) preempt a later name-scoped owner
        // (e.g. a SharedPV with real per-PV levels). Asking only the
        // owner keeps the levels consistent with the source whose stream
        // the monitor actually rides. The `notify_watermark` fan-out
        // below is unchanged: it lets each source decide per name, and a
        // non-owning source no-ops.
        let name = name.to_string();
        let sources = self.snapshot();
        async move {
            for src in sources {
                if src.has_pv(&name).await {
                    return src.monitor_watermarks(&name).await;
                }
            }
            None
        }
    }

    fn notify_watermark(
        &self,
        name: &str,
        ctx: &crate::server_native::source::ChannelContext,
        ev: crate::server_native::source::WatermarkEvent,
    ) {
        for src in self.snapshot() {
            // No has_pv check — fire on every source that registered.
            // The per-source override decides whether the name matches.
            // `ctx`/`ev` forward unchanged so the gateway source can scope
            // per-credential and reference-count the per-op pause vote.
            src.notify_watermark(name, ctx, ev);
        }
    }

    fn notify_monitor_start(
        &self,
        name: &str,
        ctx: &crate::server_native::source::ChannelContext,
        start: bool,
    ) {
        // same wrapper-severs-override defect family as
        // `notify_watermark`/`monitor_watermarks` above. The server's
        // MonitorStartControl fires the Idle↔Executing edge against the
        // *bound* source, which `PvaServer::start` makes a CompositeSource.
        // Without this forwarder the edge hits the trait-default no-op and
        // never reaches the leaf (e.g. a `SharedPV`'s `set_on_start`
        // callback). Fan out to every source with `ctx`/`start` unchanged;
        // the per-source override decides whether the name matches.
        for src in self.snapshot() {
            src.notify_monitor_start(name, ctx, start);
        }
    }

    fn set_channel_invalidator(&self, invalidator: ChannelInvalidator) {
        // The server's bound source is a CompositeSource (and the PVA
        // gateway nests another composite under it), so the trait-default
        // no-op would strand the handle at the wrapper and never reach the
        // leaf source(s) that publish invalidations. Fan the SAME handle
        // out to every child — including nested composites, which forward
        // again — so multi-tenant gateways (N `GatewayChannelSource`s under
        // one composite, `multi_gateway.rs`) all publish onto the one
        // server-wide invalidator the per-connection tasks subscribe to.
        for src in self.snapshot() {
            src.set_channel_invalidator(invalidator.clone());
        }
    }
}

#[cfg(test)]
#[allow(clippy::manual_async_fn)]
mod tests {
    use super::*;
    use crate::pvdata::{PvStructure, ScalarType, ScalarValue};
    use std::sync::Arc;

    struct PvSrc {
        name: &'static str,
        value: i32,
    }

    impl ChannelSource for PvSrc {
        fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
            let n = self.name.to_string();
            async move { vec![n] }
        }
        fn has_pv(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
            let want = self.name;
            let got = name.to_string();
            async move { got == want }
        }
        fn get_introspection(
            &self,
            _: &str,
        ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
            async {
                Some(FieldDesc::Structure {
                    struct_id: "epics:nt/NTScalar:1.0".into(),
                    fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Int))],
                })
            }
        }
        fn get_value(&self, _: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
            let v = self.value;
            async move {
                let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
                s.fields
                    .push(("value".into(), PvField::Scalar(ScalarValue::Int(v))));
                Some(PvField::Structure(s))
            }
        }
        fn put_value(
            &self,
            _: &str,
            _: PvField,
        ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
            async { Ok(()) }
        }
        fn is_writable(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
            async { true }
        }
        fn subscribe(
            &self,
            _: &str,
        ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send {
            async { None }
        }
    }

    /// Composite's top-level
    /// gate must reflect inner sub-gate version bumps. Pre-fix the
    /// composite inherited the default `Open` gate (always
    /// version=0); tcp.rs's monitor loop compared against that
    /// stale value and missed every `GatewayChannelSource::set_acf`
    /// reload on a child. The aggregator gate uses `wrapping_sum`
    /// of every inner's version (a later audit replaced the original
    /// `max(...)` shape — see `aggregator_detects_inner_bump_below_max`).
    /// The aggregate is **change-signal only**: re-check of allow/deny
    /// must go through the matched inner source's gate via
    /// `ChannelSource::revalidate_read`, not the composite's own
    /// permissive `access()` gate.
    #[epics_macros_rs::epics_test]
    async fn aggregator_gate_observes_inner_bumps() {
        use epics_base_rs::server::access_security::AccessGate;
        struct VersionedSrc {
            gate: AccessGate,
        }
        impl ChannelSource for VersionedSrc {
            fn access(&self) -> &AccessGate {
                &self.gate
            }
            fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
                async { Vec::new() }
            }
            fn has_pv(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn get_introspection(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
                async { None }
            }
            fn get_value(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<PvField>> + Send {
                async { None }
            }
            fn put_value(
                &self,
                _: &str,
                _: PvField,
            ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
                async { Err("n/a".into()) }
            }
            fn is_writable(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send
            {
                async { None }
            }
        }

        let acf = epics_base_rs::server::access_security::new_acf_cell(None);
        let resolver: epics_base_rs::server::access_security::AsgAslResolver =
            Arc::new(|_| Box::pin(async { ("DEFAULT".to_string(), 0u8) }));
        let inner1 = Arc::new(VersionedSrc {
            gate: AccessGate::required(acf.clone(), resolver.clone()),
        });
        let inner2 = Arc::new(VersionedSrc {
            gate: AccessGate::required(acf, resolver),
        });

        let comp = CompositeSource::new();
        comp.add_source("a", inner1.clone() as DynSource, 0)
            .unwrap();
        comp.add_source("b", inner2.clone() as DynSource, 1)
            .unwrap();

        let v0 = comp.access().acl_version();
        // Bump inner1 — composite aggregate must change.
        inner1.gate.bump_acl_version();
        let v1 = comp.access().acl_version();
        assert!(
            v1 != v0,
            "composite gate must surface inner1 bump: {v0} -> {v1}"
        );

        // Bump inner2 separately — composite aggregate must change again.
        inner2.gate.bump_acl_version();
        inner2.gate.bump_acl_version();
        let v2 = comp.access().acl_version();
        assert!(
            v2 != v1,
            "composite gate must track inner2 bumps too: {v1} -> {v2}"
        );
    }

    /// Watermark-routing fix: the composite must report
    /// the `monitor_watermarks` levels of the source that **owns** the
    /// name (resolved by `has_pv`, registration order), not the first
    /// source that reports any levels. Pre-fix the composite used
    /// `find_map(monitor_watermarks)`, so a catch-all source that returns
    /// levels for every name (the PVA gateway returns `Some((0,0))`)
    /// preempted a later name-scoped owner with real per-PV levels.
    ///
    /// `WmSrc` models both shapes: `serves` is the single name it owns
    /// (its `has_pv` is name-scoped, like the real gateway whose upstream
    /// lookup fails for names no upstream serves); `blanket` makes it
    /// report `levels` for EVERY name (the gateway's name-independent
    /// `monitor_watermarks`) versus only for the name it owns.
    #[epics_macros_rs::epics_test]
    async fn composite_watermarks_come_from_the_owning_source() {
        struct WmSrc {
            serves: &'static str,
            levels: Option<(usize, usize)>,
            blanket: bool,
        }
        impl ChannelSource for WmSrc {
            fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
                async { Vec::new() }
            }
            fn has_pv(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
                let owned = name == self.serves;
                async move { owned }
            }
            fn get_introspection(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
                async { None }
            }
            fn get_value(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<PvField>> + Send {
                async { None }
            }
            fn put_value(
                &self,
                _: &str,
                _: PvField,
            ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
                async { Err("n/a".into()) }
            }
            fn is_writable(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send
            {
                async { None }
            }
            async fn monitor_watermarks(&self, name: &str) -> Option<(usize, usize)> {
                if self.blanket || name == self.serves {
                    self.levels
                } else {
                    None
                }
            }
        }

        let comp = CompositeSource::new();
        // Gateway-like (order 0, registered first): owns only "GW:PV" but
        // reports (0,0) for EVERY name (blanket).
        comp.add_source(
            "gw",
            Arc::new(WmSrc {
                serves: "GW:PV",
                levels: Some((0, 0)),
                blanket: true,
            }) as DynSource,
            0,
        )
        .unwrap();
        // SharedPV-like owner (order 1, after the gateway): owns
        // "LOCAL:PV" with real per-PV levels, reported only for its name.
        comp.add_source(
            "local",
            Arc::new(WmSrc {
                serves: "LOCAL:PV",
                levels: Some((3, 9)),
                blanket: false,
            }) as DynSource,
            1,
        )
        .unwrap();

        // The owner of "LOCAL:PV" is the SharedPV-like source; the
        // gateway-like source is ordered first and reports (0,0) for every
        // name but does NOT own "LOCAL:PV", so it must not shadow the
        // owner's (3,9). The old find_map(first-Some) returned (0,0).
        assert_eq!(
            comp.monitor_watermarks("LOCAL:PV").await,
            Some((3, 9)),
            "a catch-all source ordered before the owner must not shadow the owner's levels"
        );
        // A name the gateway-like source owns still gets its (0,0).
        assert_eq!(comp.monitor_watermarks("GW:PV").await, Some((0, 0)));
        // No source owns this name — no levels.
        assert_eq!(comp.monitor_watermarks("UNKNOWN:PV").await, None);
    }

    /// Pre-fix the composite used
    /// `max(inner.acl_version)` to aggregate. That's wrong — an
    /// inner that bumps but stays below the existing max produces
    /// the SAME aggregate, so the monitor's `live != stored`
    /// compare never fires. This regression test sets up exactly
    /// that pathological shape: inner A pre-bumped to a higher
    /// version than inner B, then bumps B (which the old `max`
    /// aggregator would miss) and asserts the composite version
    /// changes.
    #[epics_macros_rs::epics_test]
    async fn aggregator_detects_inner_bump_below_max() {
        use epics_base_rs::server::access_security::AccessGate;
        struct VersionedSrc {
            gate: AccessGate,
        }
        impl ChannelSource for VersionedSrc {
            fn access(&self) -> &AccessGate {
                &self.gate
            }
            fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
                async { Vec::new() }
            }
            fn has_pv(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn get_introspection(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
                async { None }
            }
            fn get_value(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<PvField>> + Send {
                async { None }
            }
            fn put_value(
                &self,
                _: &str,
                _: PvField,
            ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
                async { Err("n/a".into()) }
            }
            fn is_writable(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send
            {
                async { None }
            }
        }

        let acf = epics_base_rs::server::access_security::new_acf_cell(None);
        let resolver: epics_base_rs::server::access_security::AsgAslResolver =
            Arc::new(|_| Box::pin(async { ("DEFAULT".to_string(), 0u8) }));
        let inner_a = Arc::new(VersionedSrc {
            gate: AccessGate::required(acf.clone(), resolver.clone()),
        });
        let inner_b = Arc::new(VersionedSrc {
            gate: AccessGate::required(acf, resolver),
        });

        // Pre-bump A several times so its version is well above B's
        // initial 0. The "max" aggregator would lock the composite
        // at A's version regardless of subsequent activity on B.
        inner_a.gate.bump_acl_version();
        inner_a.gate.bump_acl_version();
        inner_a.gate.bump_acl_version();
        inner_a.gate.bump_acl_version();
        inner_a.gate.bump_acl_version();
        assert_eq!(inner_a.gate.acl_version(), 5);
        assert_eq!(inner_b.gate.acl_version(), 0);

        let comp = CompositeSource::new();
        comp.add_source("a", inner_a.clone() as DynSource, 0)
            .unwrap();
        comp.add_source("b", inner_b.clone() as DynSource, 1)
            .unwrap();

        let v0 = comp.access().acl_version();
        // Bump only B by one — B(0)→B(1). With `max` aggregator
        // composite stays at 5; with `wrapping_sum` it moves from
        // 5 to 6, which is what the monitor needs to detect.
        inner_b.gate.bump_acl_version();
        let v1 = comp.access().acl_version();
        assert_ne!(
            v0, v1,
            "composite gate must detect a sub-max inner bump: \
             v0={v0} v1={v1} (max-style aggregation would have left it equal)"
        );
    }

    /// The registry beacon-change counter must advance on every source
    /// add/remove — including the two cases the PV-set hash cannot
    /// detect: replacing a source with another serving the SAME
    /// `list_pvs()` output, and changing a source's priority for the
    /// same PV name. Mirrors pvxs `beaconChange` (src/server.cpp:90-115).
    #[epics_macros_rs::epics_test]
    async fn beacon_change_advances_on_registry_mutations() {
        let comp = CompositeSource::new();
        let v0 = comp.beacon_change();

        // Add a source serving "pv" → counter advances.
        comp.add_source(
            "a",
            Arc::new(PvSrc {
                name: "pv",
                value: 1,
            }) as DynSource,
            0,
        )
        .unwrap();
        let v1 = comp.beacon_change();
        assert!(v1 > v0, "add_source must bump beacon_change: {v0} -> {v1}");

        // Replace it with a DIFFERENT source serving the SAME name
        // (identical list_pvs output) — the PV-set hash would not move,
        // but the registry counter must.
        comp.remove_source("a", 0).expect("source a removed");
        let v2 = comp.beacon_change();
        assert!(
            v2 > v1,
            "remove_source must bump beacon_change: {v1} -> {v2}"
        );
        comp.add_source(
            "b",
            Arc::new(PvSrc {
                name: "pv",
                value: 2,
            }) as DynSource,
            0,
        )
        .unwrap();
        let v3 = comp.beacon_change();
        assert!(
            v3 > v2,
            "re-add with the same PV set must still bump beacon_change: {v2} -> {v3}"
        );

        // Change the source's priority for the same PV name (remove at
        // order 0, re-add at order 5) — again no PV-set change, but two
        // registry mutations.
        comp.remove_source("b", 0).expect("source b removed");
        comp.add_source(
            "b",
            Arc::new(PvSrc {
                name: "pv",
                value: 2,
            }) as DynSource,
            5,
        )
        .unwrap();
        let v4 = comp.beacon_change();
        assert!(
            v4 > v3,
            "a priority change for the same PV must bump beacon_change: {v3} -> {v4}"
        );

        // A remove naming no entry STILL bumps: pvxs puts
        // `pvt->beaconChange++` outside the `if(it!=end())` that erases
        // (src/server.cpp:109-113), so the miss path reaches it too.
        assert!(comp.remove_source("nope", 0).is_none());
        let v5 = comp.beacon_change();
        assert!(
            v5 > v4,
            "a remove of an absent source must still advance beacon_change: {v4} -> {v5}"
        );

        // The trait-level accessor reports the same value.
        assert_eq!(<CompositeSource as ChannelSource>::beacon_change(&comp), v5);
    }

    /// The composite must fold an
    /// inner source's own `beacon_change()` into its returned counter, so
    /// a built-in `SharedSource` PV add/remove advances the beacon even
    /// when the composite's OWN add/remove counter is unchanged. pvxs
    /// keeps a single `beaconChange` bumped by both
    /// `addSource`/`removeSource` AND `addPV`/`removePV`
    /// (src/server.cpp:95,113,180,189).
    #[epics_macros_rs::epics_test]
    async fn beacon_change_aggregates_inner_shared_source_mutations() {
        use crate::server_native::{SharedPV, SharedSource};

        let comp = CompositeSource::new();
        let shared = Arc::new(SharedSource::new());
        comp.add_source("__user", shared.clone() as DynSource, 0)
            .unwrap();

        // Snapshot AFTER registration: from here the composite's own
        // add/remove counter does not move; only the inner SharedSource
        // mutates. Pre-fix the composite returned only its own counter, so
        // these inner mutations left the aggregate unchanged.
        let base = comp.beacon_change();

        // Inner add → composite aggregate advances.
        shared.add("X", SharedPV::new());
        let after_add = comp.beacon_change();
        assert!(
            after_add > base,
            "inner SharedSource add must advance composite beacon_change: \
             {base} -> {after_add}"
        );

        // Inner remove → composite aggregate advances again.
        assert!(shared.remove("X").is_some());
        let after_remove = comp.beacon_change();
        assert!(
            after_remove > after_add,
            "inner SharedSource remove must advance composite beacon_change: \
             {after_add} -> {after_remove}"
        );

        // The trait accessor reports the same aggregate.
        assert_eq!(
            <CompositeSource as ChannelSource>::beacon_change(&comp),
            after_remove
        );
    }

    #[epics_macros_rs::epics_test]
    async fn priority_order_dispatch() {
        let comp = CompositeSource::new();
        let lo: DynSource = Arc::new(PvSrc {
            name: "shared",
            value: 1,
        });
        let hi: DynSource = Arc::new(PvSrc {
            name: "shared",
            value: 2,
        });
        comp.add_source("lo", lo, 10).unwrap();
        comp.add_source("hi", hi, 0).unwrap();

        // Lower order wins → value=2.
        let v = comp.get_value("shared").await.unwrap();
        let PvField::Structure(s) = v else { panic!() };
        let PvField::Scalar(ScalarValue::Int(n)) = &s.fields[0].1 else {
            panic!()
        };
        assert_eq!(*n, 2);
    }

    /// pvxs keys its registry `std::map<std::pair<int, std::string>, ...>`
    /// (`serverconn.h:267`), inserts at `std::make_pair(order, name)`
    /// (`src/server.cpp:91`) and iterates it ascending (`src/server.cpp:696`,
    /// `serverchan.cpp:304`), so two sources sharing an `order` are
    /// consulted in byte-wise NAME order — that is what puts `__builtin`
    /// ahead of `__server` at pvxs's internal order -1
    /// (`src/server.cpp:542-546`). Registering the later-sorting name FIRST
    /// must not give it priority.
    #[epics_macros_rs::epics_test]
    async fn equal_order_ties_break_by_source_name_not_by_insertion() {
        let comp = CompositeSource::new();
        let first: DynSource = Arc::new(PvSrc {
            name: "shared",
            value: 1,
        });
        let second: DynSource = Arc::new(PvSrc {
            name: "shared",
            value: 2,
        });
        comp.add_source("zzz", first, 0).unwrap();
        comp.add_source("aaa", second, 0).unwrap();

        assert_eq!(
            comp.list_source(),
            vec![("aaa".to_string(), 0), ("zzz".to_string(), 0)],
            "consultation order must be (order, name) ascending"
        );

        // "aaa" < "zzz" → value=2, even though "zzz" was registered first.
        let v = comp.get_value("shared").await.unwrap();
        let PvField::Structure(s) = v else { panic!() };
        let PvField::Scalar(ScalarValue::Int(n)) = &s.fields[0].1 else {
            panic!()
        };
        assert_eq!(*n, 2);
    }

    #[epics_macros_rs::epics_test]
    async fn list_pvs_unions_sources() {
        let comp = CompositeSource::new();
        comp.add_source(
            "a",
            Arc::new(PvSrc {
                name: "alpha",
                value: 0,
            }),
            0,
        )
        .unwrap();
        comp.add_source(
            "b",
            Arc::new(PvSrc {
                name: "beta",
                value: 0,
            }),
            10,
        )
        .unwrap();
        let mut pvs = comp.list_pvs().await;
        pvs.sort();
        assert_eq!(pvs, vec!["alpha".to_string(), "beta".to_string()]);
    }

    /// every composite `*_checked` op must select the
    /// inner source through `has_pv_checked` — carrying the downstream
    /// peer's credentials — never the credential-free `has_pv`. For a
    /// gateway inner source the plain `has_pv` opens the shared-identity
    /// upstream cache; the cited CREATE_CHANNEL/GET_FIELD leak had the
    /// same shape in each `*_checked` find-loop, now centralised in
    /// `resolve_checked`. The recording mock flips `plain_seen` if its
    /// `has_pv` is reached and records the account seen by
    /// `has_pv_checked`; routing `get_value_checked` through the
    /// composite must hit the credentialed path only.
    #[epics_macros_rs::epics_test]
    async fn fr8_checked_ops_select_via_has_pv_checked() {
        use crate::server_native::source::ChannelContext;
        use epics_base_rs::server::access_security::AccessGate;
        use parking_lot::Mutex;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct RecordingSrc {
            gate: AccessGate,
            plain_seen: Arc<AtomicBool>,
            checked_account: Arc<Mutex<Option<String>>>,
        }
        impl ChannelSource for RecordingSrc {
            fn access(&self) -> &AccessGate {
                &self.gate
            }
            fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
                async { Vec::new() }
            }
            fn has_pv(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
                let seen = self.plain_seen.clone();
                async move {
                    seen.store(true, Ordering::SeqCst);
                    true
                }
            }
            fn has_pv_checked(
                &self,
                _: &str,
                ctx: ChannelContext,
            ) -> impl std::future::Future<Output = bool> + Send {
                let acct = self.checked_account.clone();
                async move {
                    *acct.lock() = Some(ctx.creds.account.clone());
                    true
                }
            }
            fn get_introspection(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
                async { None }
            }
            fn get_value(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<PvField>> + Send {
                async { Some(PvField::Scalar(ScalarValue::Int(7))) }
            }
            fn put_value(
                &self,
                _: &str,
                _: PvField,
            ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
                async { Ok(()) }
            }
            fn is_writable(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
                async { true }
            }
            fn subscribe(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send
            {
                async { None }
            }
        }

        let plain_seen = Arc::new(AtomicBool::new(false));
        let checked_account = Arc::new(Mutex::new(None));
        let mock = Arc::new(RecordingSrc {
            gate: AccessGate::open(),
            plain_seen: plain_seen.clone(),
            checked_account: checked_account.clone(),
        });
        let comp = CompositeSource::new();
        comp.add_source("rec", mock as DynSource, 0).unwrap();

        let ctx = ChannelContext {
            peer: "127.0.0.1:5075".parse().unwrap(),
            creds: std::sync::Arc::new(crate::server_native::config::ClientCredentials {
                account: "alice".into(),
                method: "ca".into(),
                host: "h".into(),
                authority: String::new(),
                roles: Vec::new(),
            }),
            pv_request: None,
            log: Default::default(),
        };
        let token = AccessGate::open()
            .check("PVX", "h", "alice", "ca", "")
            .await;
        let got = comp.get_value_checked(token, ctx).await;

        assert!(
            matches!(got, Some(PvField::Scalar(ScalarValue::Int(7)))),
            "credentialed GET must resolve through the inner source"
        );
        assert_eq!(
            checked_account.lock().as_deref(),
            Some("alice"),
            "composite must select via has_pv_checked carrying ctx.creds.account"
        );
        assert!(
            !plain_seen.load(Ordering::SeqCst),
            "composite *_checked find-loop must NOT call credential-free has_pv"
        );
    }

    /// CREATE_CHANNEL binds the OWNING source so later operations cannot
    /// silently re-route to a different source when the registry changes.
    /// pvxs installs the accepting source's callbacks into the
    /// `ServerChan` (`serverchan.cpp:70-112`) and a later `removeSource`
    /// does not rewrite them (`src/server.cpp:100-112`).
    ///
    /// `resolve_owner` returns the matched inner source; a held clone of
    /// that owner keeps serving the original source even after the same
    /// name is removed and re-registered to a different source. A fresh
    /// resolve sees the new registry — proving the binding, not the
    /// registry, is what a live channel dispatches through.
    #[epics_macros_rs::epics_test]
    async fn resolve_owner_binds_accepting_source_across_registry_change() {
        use crate::server_native::source::ChannelContext;
        let ctx = ChannelContext {
            peer: "127.0.0.1:5075".parse().unwrap(),
            creds: std::sync::Arc::new(crate::server_native::config::ClientCredentials {
                account: "alice".into(),
                method: "ca".into(),
                host: "h".into(),
                authority: String::new(),
                roles: Vec::new(),
            }),
            pv_request: None,
            log: Default::default(),
        };

        let comp = CompositeSource::new();
        let src_a: DynSource = Arc::new(PvSrc {
            name: "pv",
            value: 1,
        });
        comp.add_source("pv", src_a, 0).unwrap();

        // CREATE_CHANNEL binds the owner that accepted the channel.
        let owner = comp
            .resolve_owner("pv", ctx.clone())
            .await
            .expect("composite resolves an owner for a known PV");
        assert!(
            matches!(
                owner.get_value("pv").await,
                Some(PvField::Structure(ref s))
                    if matches!(s.fields.first(),
                        Some((_, PvField::Scalar(ScalarValue::Int(1)))))
            ),
            "bound owner must be source A (value=1)"
        );

        // Registry mutates: A is removed and the SAME name is re-registered
        // to a different source B.
        comp.remove_source("pv", 0).expect("source A removed");
        let src_b: DynSource = Arc::new(PvSrc {
            name: "pv",
            value: 2,
        });
        comp.add_source("pv", src_b, 0).unwrap();

        // The already-bound owner still dispatches to A — a live channel
        // keeps its CREATE_CHANNEL owner.
        assert!(
            matches!(
                owner.get_value("pv").await,
                Some(PvField::Structure(ref s))
                    if matches!(s.fields.first(),
                        Some((_, PvField::Scalar(ScalarValue::Int(1)))))
            ),
            "bound owner must remain source A after registry change"
        );

        // A fresh CREATE_CHANNEL sees the new registry and binds B.
        let fresh = comp
            .resolve_owner("pv", ctx)
            .await
            .expect("composite resolves the new owner");
        assert!(
            matches!(
                fresh.get_value("pv").await,
                Some(PvField::Structure(ref s))
                    if matches!(s.fields.first(),
                        Some((_, PvField::Scalar(ScalarValue::Int(2)))))
            ),
            "a new channel must bind the replacement source B (value=2)"
        );
    }

    /// A front-ordered, non-searchable source that *hosts* a name must
    /// not veto a later source's SEARCH claim for that same name. pvxs
    /// runs `onSearch` on every source and ORs the claims
    /// (`src/server.cpp:694-712`); its built-in `ServerSource::onSearch` is
    /// empty (`serversource.cpp:25-28`), so the diagnostic `server` PV
    /// never suppresses a user PV named `server`. Mirrors the built-in
    /// `__server` (order -1, non-searchable) sitting in front of a
    /// default-order user `server` PV.
    #[epics_macros_rs::epics_test]
    async fn search_ors_claims_across_sources_non_searchable_host_does_not_veto() {
        /// Hosts `name` but is never search-advertised — the
        /// `ServerInfoSource`/pvxs `ServerSource` shape.
        struct NonSearchableSrc {
            name: &'static str,
        }
        impl ChannelSource for NonSearchableSrc {
            fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
                async { Vec::new() }
            }
            fn has_pv(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
                let m = name == self.name;
                async move { m }
            }
            fn searchable(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn get_introspection(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
                async { None }
            }
            fn get_value(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<PvField>> + Send {
                async { None }
            }
            fn put_value(
                &self,
                _: &str,
                _: PvField,
            ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
                async { Err("n/a".into()) }
            }
            fn is_writable(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send
            {
                async { None }
            }
        }

        let requester: std::net::SocketAddr = "10.0.0.5:5076".parse().unwrap();

        // Bare built-in `server`: only the non-searchable host exists, so
        // SEARCH stays unanswered (pvxs serves `server` by direct connect
        // only).
        let bare = CompositeSource::new();
        bare.add_source(
            "__server",
            Arc::new(NonSearchableSrc { name: "server" }),
            -1,
        )
        .unwrap();
        assert!(
            !bare.searchable("server").await,
            "a bare built-in `server` must not be UDP-search-advertised"
        );
        assert!(
            !bare.searchable_from("server", requester).await,
            "a bare built-in `server` must not be TCP-circuit search-advertised"
        );

        // Front non-searchable `__server` + default-order user `server`
        // PV: the user PV's claim survives the front host's non-claim.
        let comp = CompositeSource::new();
        comp.add_source(
            "__server",
            Arc::new(NonSearchableSrc { name: "server" }),
            -1,
        )
        .unwrap();
        comp.add_source(
            "user",
            Arc::new(PvSrc {
                name: "server",
                value: 7,
            }),
            0,
        )
        .unwrap();
        assert!(
            comp.searchable("server").await,
            "a user PV named `server` must be UDP-search-advertised even \
             behind the front non-searchable `__server` source"
        );
        assert!(
            comp.searchable_from("server", requester).await,
            "a user PV named `server` must be TCP-circuit search-advertised \
             even behind the front non-searchable `__server` source"
        );

        // CREATE/GET ownership still goes to the front source (lowest
        // order wins): the non-searchable `__server` keeps owning the
        // literal `server` channel, so a GET returns no value (pvxs
        // `server` has no GET surface), not the user PV's value=7.
        assert!(
            comp.get_value("server").await.is_none(),
            "first-owner CREATE/GET rule must keep `server` owned by the \
             front `__server` source; SEARCH OR must not change ownership"
        );
    }

    /// The other direction of the same independence: a source may advertise a
    /// name at SEARCH that it will REFUSE at CREATE_CHANNEL.
    ///
    /// This is pvxs's `SingleSource` shape — `onSearch` claims every name
    /// `dbChannelTest` resolves (`singlesource.cpp:467-472`), and `onCreate`
    /// then refuses a field with no NT (`DBF_NOACCESS`), which the server
    /// reports as `Refused to create Channel` (`serverchan.cpp:328-351`).
    /// Measured on `softIocPVX`: `pvxget ORACLE:AI.MLOK` gets exactly that
    /// refusal, which proves the search was answered first.
    ///
    /// The composite previously asked `has_pv(name) && searchable(name)`,
    /// which made `searchable`-wider-than-`has_pv` unrepresentable: the
    /// refusal would have degraded to a search timeout. Sources that do not
    /// distinguish the two are unaffected — `searchable` defaults to `has_pv`,
    /// which the sibling test above still pins.
    #[epics_macros_rs::epics_test]
    async fn search_advertises_a_name_the_source_refuses_at_create() {
        /// Advertised at SEARCH, refused at CREATE — the `SingleSource`
        /// `DBF_NOACCESS` shape.
        struct RefusesAtCreateSrc {
            name: &'static str,
        }
        impl ChannelSource for RefusesAtCreateSrc {
            fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
                async { Vec::new() }
            }
            /// The CREATE gate: this source serves nothing.
            fn has_pv(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            /// The SEARCH gate: the name resolves, so it is advertised.
            fn searchable(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
                let m = name == self.name;
                async move { m }
            }
            fn get_introspection(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
                async { None }
            }
            fn get_value(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<PvField>> + Send {
                async { None }
            }
            fn put_value(
                &self,
                _: &str,
                _: PvField,
            ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
                async { Err("n/a".into()) }
            }
            fn is_writable(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send
            {
                async { None }
            }
        }

        let requester: std::net::SocketAddr = "10.0.0.5:5076".parse().unwrap();
        let comp = CompositeSource::new();
        comp.add_source(
            "db",
            Arc::new(RefusesAtCreateSrc {
                name: "REC.NOACCESS",
            }),
            0,
        )
        .unwrap();

        assert!(
            comp.searchable("REC.NOACCESS").await,
            "SEARCH must be answered for a name the source advertises, even \
             though CREATE will refuse it — else the client never sends \
             CREATE_CHANNEL and the refusal becomes a timeout"
        );
        assert!(
            comp.searchable_from("REC.NOACCESS", requester).await,
            "the TCP-circuit search gate must agree with the UDP one"
        );
        assert!(
            !comp.has_pv("REC.NOACCESS").await,
            "the CREATE gate must still refuse the advertised name"
        );
    }
}
