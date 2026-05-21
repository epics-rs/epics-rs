//! [`CompositeSource`] — multi-source registry mirroring pvxs's
//! `Server::addSource(name, src, order)` model. Sources are kept in a
//! priority-sorted list and dispatched in order on each PV-name lookup.
//!
//! Lower `order` values are tried first (`order=0` is the default). Ties
//! are broken by insertion order. Source names beginning with "__" are
//! reserved for internal use (pvxs convention) — `__builtin` is a
//! [`crate::server_native::SharedSource`] for [`Self::add_pv`] /
//! [`Self::remove_pv`] convenience.
//!
//! For each request the first source whose `has_pv()` returns `true`
//! wins all subsequent calls (`get_value`, `subscribe`, `put_value`,
//! `rpc`, `is_writable`, `get_introspection`). `list_pvs()` is the
//! union of every source's PV list.

use std::sync::Arc;
use tokio::sync::mpsc;

use crate::pvdata::{FieldDesc, PvField};

use super::source::{AccessChecked, ChannelSource, DynSource, RawMonitorEvent};

/// One entry in the registry.
#[derive(Clone)]
pub struct SourceEntry {
    pub name: String,
    pub order: i32,
    pub source: DynSource,
}

/// Multi-source registry. Wrap with `Arc` and feed to
/// [`crate::server_native::PvaServer::start`].
pub struct CompositeSource {
    entries: Arc<parking_lot::RwLock<Vec<SourceEntry>>>,
    /// Round 50 (R50-G1, audit-followups): the composite's gate is
    /// an aggregator whose `acl_version()` is the `wrapping_sum`
    /// of every inner gate's version (NOT `max(...)` — see the
    /// round-50 audit; a max-based aggregate produced false
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
}

impl Default for CompositeSource {
    fn default() -> Self {
        use epics_base_rs::server::access_security::AccessGate;
        let entries: Arc<parking_lot::RwLock<Vec<SourceEntry>>> =
            Arc::new(parking_lot::RwLock::new(Vec::new()));
        let entries_for_version = entries.clone();
        // Round 50 follow-up (audit): the composite's aggregate
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
            for entry in entries_for_version.read().iter() {
                sum = sum.wrapping_add(entry.source.access_gate().acl_version());
            }
            sum
        }));
        Self {
            entries,
            access_gate,
        }
    }
}

impl CompositeSource {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Add a source. Errors when (`name`, `order`) is already present —
    /// pvxs convention so callers notice double-registration. Higher
    /// priority = lower `order`. Default `order=0`.
    pub fn add_source(&self, name: &str, source: DynSource, order: i32) -> Result<(), String> {
        let mut e = self.entries.write();
        if e.iter().any(|x| x.name == name && x.order == order) {
            return Err(format!("source ({name}, {order}) already registered"));
        }
        e.push(SourceEntry {
            name: name.into(),
            order,
            source,
        });
        e.sort_by_key(|x| x.order);
        Ok(())
    }

    /// Remove and return the source previously added with the given
    /// (`name`, `order`) tuple. Returns `None` when not found.
    pub fn remove_source(&self, name: &str, order: i32) -> Option<DynSource> {
        let mut e = self.entries.write();
        let idx = e.iter().position(|x| x.name == name && x.order == order)?;
        Some(e.remove(idx).source)
    }

    /// Look up a previously added source by (name, order).
    pub fn get_source(&self, name: &str, order: i32) -> Option<DynSource> {
        self.entries
            .read()
            .iter()
            .find(|x| x.name == name && x.order == order)
            .map(|x| x.source.clone())
    }

    /// (name, order) for every registered source — debug helper.
    pub fn list_source(&self) -> Vec<(String, i32)> {
        self.entries
            .read()
            .iter()
            .map(|x| (x.name.clone(), x.order))
            .collect()
    }

    fn snapshot(&self) -> Vec<DynSource> {
        self.entries
            .read()
            .iter()
            .map(|x| x.source.clone())
            .collect()
    }

    /// BRIDGE-FR-8: single owner of credentialed inner-source selection
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
                    .check(name, &ctx.host, &ctx.account, &ctx.method, &ctx.authority)
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

    /// Round 50 follow-up (audit, R50-G3): monitor-reload READ
    /// revalidation owner for composite sources.
    ///
    /// The composite's `access()` gate is an `open_with_aggregator`
    /// — its `acl_version()` correctly reports "some inner moved"
    /// (R50-G1 wrapping-sum aggregate), but its `check()` is the
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
                // BRIDGE-FR-8: select under the downstream peer's identity
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

    /// Route to the first source that *hosts* the name and ask THAT
    /// source whether it is UDP-search-advertised. A name hosted by a
    /// non-searchable source (the built-in `ServerInfoSource`) must
    /// stay unanswered on broadcast SEARCH even though `has_pv` is
    /// true — so we cannot just OR `searchable` across sources.
    fn searchable(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
        let sources = self.snapshot();
        let name = name.to_string();
        async move {
            for src in sources {
                if src.has_pv(&name).await {
                    return src.searchable(&name).await;
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

    /// BRIDGE-FR-8: the find-loop itself routes through the inner
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

    /// BRIDGE-FR-8: credential-aware introspection routed to the matched
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
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        let name = name.to_string();
        let this = self.snapshot();
        async move {
            for src in this {
                if src.has_pv(&name).await {
                    return src.put_value(&name, value).await;
                }
            }
            Err(format!("no source serves '{name}'"))
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
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send {
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
    ) -> impl std::future::Future<Output = Result<(FieldDesc, PvField), String>> + Send {
        let name = name.to_string();
        let this = self.snapshot();
        async move {
            for src in this {
                if src.has_pv(&name).await {
                    return src.rpc(&name, request_desc, request_value).await;
                }
            }
            Err(format!("no source serves '{name}'"))
        }
    }

    // Round 43: composite has no single gate of its own — each
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

    fn put_value_checked(
        &self,
        checked: AccessChecked,
        value: PvField,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            match Self::resolve_checked(this, &name, &ctx).await {
                Some((src, inner_checked)) => {
                    src.put_value_checked(inner_checked, value, ctx).await
                }
                None => Err(format!("no source serves '{name}'")),
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
        desc: FieldDesc,
        changed: crate::proto::BitSet,
        delta: PvField,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            match Self::resolve_checked(this, &name, &ctx).await {
                Some((src, inner_checked)) => {
                    src.put_delta_checked(inner_checked, desc, changed, delta, ctx)
                        .await
                }
                None => Err(format!("no source serves '{name}'")),
            }
        }
    }

    fn subscribe_checked(
        &self,
        checked: AccessChecked,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send {
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
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<RawMonitorEvent>>> + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            let (src, inner_checked) = Self::resolve_checked(this, &name, &ctx).await?;
            src.subscribe_raw_checked(inner_checked, ctx).await
        }
    }

    /// BR-R14: forward the downstream monitor options to the matched
    /// inner source. A composite over a gateway source must carry
    /// `opts` through so the gateway can reject options it cannot
    /// honor across a fanout monitor.
    fn subscribe_checked_opts(
        &self,
        checked: AccessChecked,
        ctx: crate::server_native::source::ChannelContext,
        opts: crate::server_native::source::MonitorOptions,
    ) -> impl std::future::Future<
        Output = Option<mpsc::Receiver<crate::server_native::source::MonitorUpdate>>,
    > + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            let (src, inner_checked) = Self::resolve_checked(this, &name, &ctx).await?;
            src.subscribe_checked_opts(inner_checked, ctx, opts).await
        }
    }

    /// BR-R14 raw-path counterpart of [`Self::subscribe_checked_opts`].
    fn subscribe_raw_checked_opts(
        &self,
        checked: AccessChecked,
        ctx: crate::server_native::source::ChannelContext,
        opts: crate::server_native::source::MonitorOptions,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<RawMonitorEvent>>> + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            let (src, inner_checked) = Self::resolve_checked(this, &name, &ctx).await?;
            src.subscribe_raw_checked_opts(inner_checked, ctx, opts)
                .await
        }
    }

    fn rpc_checked(
        &self,
        checked: AccessChecked,
        request_desc: FieldDesc,
        request_value: PvField,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Result<(FieldDesc, PvField), String>> + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            match Self::resolve_checked(this, &name, &ctx).await {
                Some((src, inner_checked)) => {
                    src.rpc_checked(inner_checked, request_desc, request_value, ctx)
                        .await
                }
                None => Err(format!("no source serves '{name}'")),
            }
        }
    }

    fn subscribe_raw(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<RawMonitorEvent>>> + Send {
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

    fn process(&self, name: &str) -> impl std::future::Future<Output = Result<(), String>> + Send {
        let name = name.to_string();
        let this = self.snapshot();
        async move {
            for src in this {
                if src.has_pv(&name).await {
                    return src.process(&name).await;
                }
            }
            Err(format!("no source serves '{name}'"))
        }
    }

    fn process_checked(
        &self,
        checked: AccessChecked,
        ctx: crate::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        let name = checked.pv_name().to_string();
        let this = self.snapshot();
        async move {
            match Self::resolve_checked(this, &name, &ctx).await {
                Some((src, inner_checked)) => src.process_checked(inner_checked, ctx).await,
                None => Err(format!("no source serves '{name}'")),
            }
        }
    }

    fn monitor_watermarks(&self, name: &str) -> Option<(usize, usize)> {
        // BRIDGE-FR-11: surface the watermark levels of whichever inner
        // source exposes them, so the server monitor loop drives the
        // pause/resume hysteresis and reaches the per-source
        // notify_watermark callback below. Consistent with that
        // callback (which fans out to every source and lets each decide),
        // this returns the first source that reports levels — only the
        // gateway source overrides this; the control source keeps the
        // default `None`. Without this forwarder the server sees the
        // CompositeSource trait default `None` and never fires the
        // callbacks (the wrapper-severs-override defect family).
        self.snapshot()
            .into_iter()
            .find_map(|src| src.monitor_watermarks(name))
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
        ) -> impl std::future::Future<Output = Result<(), String>> + Send {
            async { Ok(()) }
        }
        fn is_writable(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
            async { true }
        }
        fn subscribe(
            &self,
            _: &str,
        ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send {
            async { None }
        }
    }

    /// Round 50 (R50-G1, audit-followups): composite's top-level
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
    #[tokio::test]
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
            ) -> impl std::future::Future<Output = Result<(), String>> + Send {
                async { Err("n/a".into()) }
            }
            fn is_writable(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send
            {
                async { None }
            }
        }

        let acf = Arc::new(tokio::sync::RwLock::new(None));
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

    /// BRIDGE-FR-11: the composite must forward `monitor_watermarks`
    /// to whichever inner source exposes levels, mirroring the
    /// `notify_watermark` fan-out. Pre-fix the composite inherited
    /// the `ChannelSource` trait default (`None`), so tcp.rs's monitor
    /// loop saw no watermark levels and never armed the pause/resume
    /// hysteresis — the gateway override on the inner source was
    /// severed by the composite (the wrapper-severs-override family).
    /// This registers a plain source (default `None`) ahead of a
    /// watermark source and asserts `find_map` skips the `None` and
    /// surfaces the inner override.
    #[tokio::test]
    async fn fr11_composite_forwards_inner_monitor_watermarks() {
        struct WatermarkSrc;
        impl ChannelSource for WatermarkSrc {
            fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
                async { Vec::new() }
            }
            fn has_pv(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
                async { true }
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
            ) -> impl std::future::Future<Output = Result<(), String>> + Send {
                async { Err("n/a".into()) }
            }
            fn is_writable(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send
            {
                async { None }
            }
            fn monitor_watermarks(&self, _name: &str) -> Option<(usize, usize)> {
                Some((2, 5))
            }
        }

        let comp = CompositeSource::new();
        // Plain source (order 0) reports the trait default `None`;
        // watermark source (order 1) reports levels. find_map must
        // skip the None and surface the override.
        comp.add_source(
            "plain",
            Arc::new(PvSrc {
                name: "X",
                value: 0,
            }) as DynSource,
            0,
        )
        .unwrap();
        comp.add_source("wm", Arc::new(WatermarkSrc) as DynSource, 1)
            .unwrap();

        assert_eq!(
            comp.monitor_watermarks("X"),
            Some((2, 5)),
            "composite must forward the inner source's watermark levels, not the default None"
        );
    }

    /// Round 50 follow-up (audit): pre-fix the composite used
    /// `max(inner.acl_version)` to aggregate. That's wrong — an
    /// inner that bumps but stays below the existing max produces
    /// the SAME aggregate, so the monitor's `live != stored`
    /// compare never fires. This regression test sets up exactly
    /// that pathological shape: inner A pre-bumped to a higher
    /// version than inner B, then bumps B (which the old `max`
    /// aggregator would miss) and asserts the composite version
    /// changes.
    #[tokio::test]
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
            ) -> impl std::future::Future<Output = Result<(), String>> + Send {
                async { Err("n/a".into()) }
            }
            fn is_writable(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send
            {
                async { None }
            }
        }

        let acf = Arc::new(tokio::sync::RwLock::new(None));
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

    #[tokio::test]
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

    #[tokio::test]
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

    /// BRIDGE-FR-8: every composite `*_checked` op must select the
    /// inner source through `has_pv_checked` — carrying the downstream
    /// peer's credentials — never the credential-free `has_pv`. For a
    /// gateway inner source the plain `has_pv` opens the shared-identity
    /// upstream cache; the cited CREATE_CHANNEL/GET_FIELD leak had the
    /// same shape in each `*_checked` find-loop, now centralised in
    /// `resolve_checked`. The recording mock flips `plain_seen` if its
    /// `has_pv` is reached and records the account seen by
    /// `has_pv_checked`; routing `get_value_checked` through the
    /// composite must hit the credentialed path only.
    #[tokio::test]
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
                    *acct.lock() = Some(ctx.account);
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
            ) -> impl std::future::Future<Output = Result<(), String>> + Send {
                async { Ok(()) }
            }
            fn is_writable(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
                async { true }
            }
            fn subscribe(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send
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
            account: "alice".into(),
            method: "ca".into(),
            host: "h".into(),
            authority: String::new(),
            roles: Vec::new(),
            pv_request: None,
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
            "composite must select via has_pv_checked carrying ctx.account"
        );
        assert!(
            !plain_seen.load(Ordering::SeqCst),
            "composite *_checked find-loop must NOT call credential-free has_pv"
        );
    }
}
