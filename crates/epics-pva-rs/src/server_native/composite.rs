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
    /// Round 50 (R50-G1): the composite's gate is an aggregator
    /// whose `acl_version()` returns `max(inner.access_gate().acl_version())`.
    /// Pre-fix the composite inherited the default `Open` gate
    /// (version=0 forever), so tcp.rs's monitor loop captured `0`
    /// at subscribe and compared against `0` on every event —
    /// missing every reload bump on the matched inner source. The
    /// aggregator surfaces inner bumps at the top-level gate the
    /// monitor task tracks.
    access_gate: epics_base_rs::server::access_security::AccessGate,
}

impl Default for CompositeSource {
    fn default() -> Self {
        use epics_base_rs::server::access_security::AccessGate;
        let entries: Arc<parking_lot::RwLock<Vec<SourceEntry>>> =
            Arc::new(parking_lot::RwLock::new(Vec::new()));
        let entries_for_version = entries.clone();
        let access_gate = AccessGate::open_with_aggregator(Arc::new(move || {
            entries_for_version
                .read()
                .iter()
                .map(|e| e.source.access_gate().acl_version())
                .max()
                .unwrap_or(0)
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
}

impl ChannelSource for CompositeSource {
    fn access(&self) -> &epics_base_rs::server::access_security::AccessGate {
        &self.access_gate
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
            for src in this {
                if src.has_pv(&name).await {
                    let inner_checked = src
                        .access_gate()
                        .check(&name, &ctx.host, &ctx.account, &ctx.method, "")
                        .await;
                    return src.get_value_checked(inner_checked, ctx).await;
                }
            }
            None
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
            for src in this {
                if src.has_pv(&name).await {
                    let inner_checked = src
                        .access_gate()
                        .check(&name, &ctx.host, &ctx.account, &ctx.method, "")
                        .await;
                    return src.put_value_checked(inner_checked, value, ctx).await;
                }
            }
            Err(format!("no source serves '{name}'"))
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
            for src in this {
                if src.has_pv(&name).await {
                    let inner_checked = src
                        .access_gate()
                        .check(&name, &ctx.host, &ctx.account, &ctx.method, "")
                        .await;
                    return src.subscribe_checked(inner_checked, ctx).await;
                }
            }
            None
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
            for src in this {
                if src.has_pv(&name).await {
                    let inner_checked = src
                        .access_gate()
                        .check(&name, &ctx.host, &ctx.account, &ctx.method, "")
                        .await;
                    return src.subscribe_raw_checked(inner_checked, ctx).await;
                }
            }
            None
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
            for src in this {
                if src.has_pv(&name).await {
                    let inner_checked = src
                        .access_gate()
                        .check(&name, &ctx.host, &ctx.account, &ctx.method, "")
                        .await;
                    return src
                        .rpc_checked(inner_checked, request_desc, request_value, ctx)
                        .await;
                }
            }
            Err(format!("no source serves '{name}'"))
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

    fn notify_watermark_high(&self, name: &str) {
        for src in self.snapshot() {
            // No has_pv check — fire on every source that registered.
            // The per-source override decides whether the name matches.
            src.notify_watermark_high(name);
        }
    }

    fn notify_watermark_low(&self, name: &str) {
        for src in self.snapshot() {
            src.notify_watermark_low(name);
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

    /// Round 50 (R50-G1): composite's top-level gate must reflect
    /// inner sub-gate version bumps. Pre-fix the composite inherited
    /// the default `Open` gate (always version=0); tcp.rs's monitor
    /// loop compared against that stale value and missed every
    /// `GatewayChannelSource::set_acf` reload on a child. The
    /// aggregator gate returns `max(inner.access_gate().acl_version())`
    /// so a bump on ANY inner is observed at the composite.
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
        comp.add_source("a", inner1.clone() as DynSource, 0).unwrap();
        comp.add_source("b", inner2.clone() as DynSource, 1).unwrap();

        let v0 = comp.access().acl_version();
        // Bump inner1 — composite must observe the new max.
        inner1.gate.bump_acl_version();
        let v1 = comp.access().acl_version();
        assert!(v1 > v0, "composite gate must surface inner1 bump: {v0} -> {v1}");

        // Bump inner2 separately — composite must reflect the
        // higher of the two.
        inner2.gate.bump_acl_version();
        inner2.gate.bump_acl_version();
        let v2 = comp.access().acl_version();
        assert!(v2 > v1, "composite gate must track inner2 bumps too: {v1} -> {v2}");
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
}
