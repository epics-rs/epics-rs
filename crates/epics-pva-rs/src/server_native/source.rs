//! [`ChannelSource`] — the trait every native PVA server is generic over.
//!
//! Replaces the spvirit `PvStore` trait. Uses our own [`crate::pvdata`]
//! types, so no `spvirit_*` types appear in the public surface.

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::pvdata::{FieldDesc, PvField};
pub use epics_base_rs::server::access_security::{AccessChecked, AccessGate};

/// Per-operation context surfaced to [`ChannelSource`] implementors
/// that need the downstream peer's identity (audit, ACL, gateway
/// credential pass-through). Fields mirror what `CONNECTION_VALIDATION`
/// established at handshake time, plus the peer's TCP socket address.
///
/// PG-G10: gateways use this to pick the correct upstream client
/// when the gateway maintains a per-credential connection pool to
/// the upstream IOC. Default trait methods that don't take a context
/// remain available so existing implementations are unaffected.
#[derive(Clone, Debug)]
pub struct ChannelContext {
    /// Downstream client TCP socket address.
    pub peer: SocketAddr,
    /// Account name from CONNECTION_VALIDATION (`anonymous` when the
    /// client didn't authenticate).
    pub account: String,
    /// Auth method (`"anonymous"`, `"ca"`, `"x509"`).
    pub method: String,
    /// Reverse-resolved host name. Empty when DNS lookup failed.
    pub host: String,
}

/// A backend that can answer pvAccess GET / PUT / MONITOR requests for a
/// set of named PVs.
pub trait ChannelSource: Send + Sync + 'static {
    /// Per-source access policy. Returns the [`AccessGate`] used by
    /// the wire layer to mint [`AccessChecked`] tokens for the typed
    /// op methods (`*_checked` family). Default impl returns a
    /// process-wide singleton `Open` gate — sources that need ACF
    /// enforcement override to install a `Required` gate wrapping
    /// their `AcfCell`.
    ///
    /// Round 40 (type-state ACF gate): the typed op methods take
    /// `AccessChecked` instead of `name: &str + ctx`. Because
    /// `AccessChecked` is unforgeable outside this gate's `check`,
    /// every wire op MUST flow through it — closing the missed-path
    /// pattern that surfaced across rounds 32-39.
    fn access(&self) -> &AccessGate {
        static OPEN_GATE: std::sync::OnceLock<AccessGate> = std::sync::OnceLock::new();
        OPEN_GATE.get_or_init(AccessGate::open)
    }

    /// Enumerate every PV name this source can serve.
    fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send;

    /// True iff `name` resolves to a known PV.
    fn has_pv(&self, name: &str) -> impl std::future::Future<Output = bool> + Send;

    /// Fetch the type descriptor for a PV (used by GET-INIT and GET_FIELD).
    fn get_introspection(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send;

    /// Fetch the current value of a PV.
    fn get_value(&self, name: &str) -> impl std::future::Future<Output = Option<PvField>> + Send;

    /// Round 50 follow-up (audit, R50-G3): re-check READ access for
    /// `(pv_name, ctx)` through the SOURCE-SPECIFIC ACL gate that
    /// served the original subscription. Returns `Some(token)` on
    /// allow, `None` on deny.
    ///
    /// Invariant (closed by this method): every monitor event
    /// after an ACL version mismatch MUST re-check READ through
    /// the same gate that originally produced its `AccessChecked`.
    /// For terminal sources (`PvDatabaseSource`,
    /// `GatewayChannelSource`) `self.access()` IS that gate, so the
    /// default impl below is correct. For `CompositeSource` the
    /// top-level `access()` is an `open_with_aggregator` —
    /// permissive on every call — and the override MUST resolve
    /// the matched inner source and route the check through THAT
    /// source's gate. Without the override, a monitor whose
    /// subscribe-time inner-gate said allow would re-check against
    /// the composite's Open gate on reload and keep streaming
    /// after the inner flipped to deny.
    fn revalidate_read(
        &self,
        pv_name: &str,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Option<AccessChecked>> + Send {
        let gate = self.access();
        let host = ctx.host.clone();
        let account = ctx.account.clone();
        let method = ctx.method.clone();
        let name = pv_name.to_string();
        async move {
            let checked = gate.check(&name, &host, &account, &method, "").await;
            if checked.allows_read() {
                Some(checked)
            } else {
                None
            }
        }
    }

    /// Type-state-enforced GET. The wire layer mints `checked` via
    /// `self.access().check(...)` once per op; the source then
    /// inspects `checked.allows_read()` and dispatches. Round 43
    /// deleted the `get_value_ctx` legacy path — every credential-
    /// aware GET now flows through this method and the AccessGate.
    /// The ctx is still passed so gateway-style sources can route
    /// to per-credential upstream connection pools.
    fn get_value_checked(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Option<PvField>> + Send {
        let _ = ctx;
        async move {
            if !checked.allows_read() {
                return None;
            }
            self.get_value(checked.pv_name()).await
        }
    }

    /// Apply a PUT.
    fn put_value(
        &self,
        name: &str,
        value: PvField,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;

    /// Type-state-enforced PUT. Refuses non-`ReadWrite` tokens; on
    /// `ReadWrite` it delegates to the legacy ctx-less `put_value`.
    /// Sources that need credential-aware PUT routing (e.g. gateway
    /// per-credential upstream client pool) override this directly
    /// and consume `ctx` themselves.
    fn put_value_checked(
        &self,
        checked: AccessChecked,
        value: PvField,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        async move {
            if !checked.allows_write() {
                return Err(format!(
                    "PUT denied by access security: '{}' from {}/{}/{}",
                    checked.pv_name(),
                    ctx.host,
                    ctx.account,
                    ctx.method,
                ));
            }
            self.put_value(checked.pv_name(), value).await
        }
    }

    /// True iff PUT is allowed against this PV (for ACL gating).
    fn is_writable(&self, name: &str) -> impl std::future::Future<Output = bool> + Send;

    /// Subscribe to value-change notifications. Returns `None` if unknown.
    fn subscribe(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send;

    /// Type-state-enforced MONITOR. Refuses `NoAccess` tokens; on
    /// any READ-class level delegates to the legacy ctx-less
    /// `subscribe`. Sources that need credential-aware MONITOR
    /// routing override this directly.
    fn subscribe_checked(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send {
        let _ = ctx;
        async move {
            if !checked.allows_read() {
                return None;
            }
            self.subscribe(checked.pv_name()).await
        }
    }

    /// Optional **raw-frame subscribe** (F-G12). When the source can
    /// hand the server pre-encoded MONITOR DATA payloads (e.g. the
    /// pva_gateway upstream-monitor task already received them on the
    /// wire and never decoded them), the server skips its own
    /// `encode_pv_field` step and writes the cached bytes straight
    /// onto the downstream socket — pvxs / pva2pva style raw frame
    /// forwarding. Default returns `None`, which keeps the regular
    /// `subscribe` decoded-PvField path active.
    ///
    /// Each [`RawMonitorEvent`] holds the **changed bitset + value
    /// bytes + overrun bitset** verbatim from upstream. The dispatch
    /// layer prepends the per-subscription PVA header (with the
    /// downstream IOID + subcmd 0) and emits.
    fn subscribe_raw(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<RawMonitorEvent>>> + Send {
        let _ = name;
        async { None }
    }

    /// Type-state-enforced raw MONITOR fast path. NoAccess → None;
    /// otherwise delegates to ctx-less `subscribe_raw`.
    fn subscribe_raw_checked(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<RawMonitorEvent>>> + Send {
        let _ = ctx;
        async move {
            if !checked.allows_read() {
                return None;
            }
            self.subscribe_raw(checked.pv_name()).await
        }
    }

    /// Dispatch an RPC. The default impl returns "RPC not supported";
    /// implementors can override to provide actual RPC behaviour.
    ///
    /// Returns the response (FieldDesc, PvField) on success.
    fn rpc(
        &self,
        name: &str,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> impl std::future::Future<Output = Result<(FieldDesc, PvField), String>> + Send {
        let _ = (name, request_desc, request_value);
        async move { Err("RPC not supported by this source".to_string()) }
    }

    /// Type-state-enforced RPC. pvxs treats RPC as READ-class for
    /// ACF; refuse `NoAccess` tokens with an error and otherwise
    /// delegate to the legacy ctx-less `rpc`.
    fn rpc_checked(
        &self,
        checked: AccessChecked,
        request_desc: FieldDesc,
        request_value: PvField,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Result<(FieldDesc, PvField), String>> + Send {
        async move {
            if !checked.allows_read() {
                return Err(format!(
                    "RPC denied by access security: '{}' from {}/{}/{}",
                    checked.pv_name(),
                    ctx.host,
                    ctx.account,
                    ctx.method,
                ));
            }
            self.rpc(checked.pv_name(), request_desc, request_value).await
        }
    }

    /// Notify the source that the per-connection monitor outbox for
    /// `name` just crossed UP through its high watermark. Producers
    /// can throttle their post() rate in response. Default impl is
    /// a no-op; [`crate::server_native::shared_pv::SharedSource`]
    /// overrides to fire the per-PV `on_high_mark` callback.
    /// Mirrors pvxs `MonitorControlOp::onHighMark`.
    fn notify_watermark_high(&self, name: &str) {
        let _ = name;
    }

    /// Companion to [`Self::notify_watermark_high`]: fired when the
    /// outbox drained back to empty. Producers should un-throttle.
    /// Default no-op.
    fn notify_watermark_low(&self, name: &str) {
        let _ = name;
    }
}

/// One MONITOR DATA event in **raw wire form** — the bytes the
/// upstream server emitted, ready to be re-emitted downstream after
/// the per-subscription PVA header has been prepended. Used by
/// [`ChannelSource::subscribe_raw`] to skip the server-side
/// `encode_pv_field` round-trip (F-G12).
///
/// `body_bytes` is the **`changed bitset | value bytes | overrun
/// bitset`** triplet exactly as it sat on the upstream wire (after
/// the upstream server's IOID + subcmd, which we discard and
/// replace with the downstream IOID + subcmd 0).
///
/// `byte_order` records what byte order the producer encoded with,
/// so the dispatch layer can refuse the fast path when the
/// downstream connection negotiated the opposite endian (rare, but
/// matters for cross-host gateways). On mismatch, dispatch falls
/// back to the decoded `subscribe` path.
#[derive(Debug, Clone)]
pub struct RawMonitorEvent {
    /// Body of the MONITOR DATA frame: `changed | value | overrun`.
    /// Refcounted via `bytes::Bytes` so fan-out across N
    /// downstream subscribers is N atomic increments, no copies.
    pub body_bytes: bytes::Bytes,
    /// Byte order the producer encoded with. `Little` is the
    /// default for both pva-rs and pvxs; only relevant when the
    /// server's downstream connection negotiated `Big`.
    pub byte_order: crate::proto::ByteOrder,
}

/// Type-erased handle so the server runtime can hold heterogeneous sources
/// without monomorphising every async path. Most callers pass an
/// `Arc<MySource>` directly; this is mainly for the runtime internals.
pub type DynSource = Arc<dyn ChannelSourceObj>;

/// Object-safe variant of [`ChannelSource`]. Auto-implemented via blanket
/// for any `T: ChannelSource`.
pub trait ChannelSourceObj: Send + Sync {
    fn list_pvs<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<String>> + Send + 'a>>;
    fn has_pv<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;
    fn get_introspection<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<FieldDesc>> + Send + 'a>>;
    fn get_value<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<PvField>> + Send + 'a>>;
    /// Round 41: type-state-gated GET. Dyn forwarder.
    fn get_value_checked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<PvField>> + Send + 'a>>;
    /// Round 41: per-source access gate. Dyn forwarder.
    fn access_gate<'a>(&'a self) -> &'a AccessGate;
    /// Round 50 follow-up: monitor reload revalidation owner.
    fn revalidate_read<'a>(
        &'a self,
        pv_name: &'a str,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<AccessChecked>> + Send + 'a>>;
    fn put_value<'a>(
        &'a self,
        name: &'a str,
        value: PvField,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>;
    /// Round 42: dyn forwarder for type-state PUT.
    fn put_value_checked<'a>(
        &'a self,
        checked: AccessChecked,
        value: PvField,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>;
    fn is_writable<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;
    fn subscribe<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send + 'a>,
    >;
    /// Round 42: dyn forwarder for type-state MONITOR.
    fn subscribe_checked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send + 'a>,
    >;
    fn subscribe_raw<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<RawMonitorEvent>>> + Send + 'a>,
    >;
    /// Round 42: dyn forwarder for type-state raw MONITOR.
    fn subscribe_raw_checked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<RawMonitorEvent>>> + Send + 'a>,
    >;
    fn rpc<'a>(
        &'a self,
        name: &'a str,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(FieldDesc, PvField), String>> + Send + 'a>,
    >;
    /// Round 42: dyn forwarder for type-state RPC.
    fn rpc_checked<'a>(
        &'a self,
        checked: AccessChecked,
        request_desc: FieldDesc,
        request_value: PvField,
        ctx: ChannelContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(FieldDesc, PvField), String>> + Send + 'a>,
    >;
    fn notify_watermark_high(&self, name: &str);
    fn notify_watermark_low(&self, name: &str);
}

impl<T: ChannelSource + 'static> ChannelSourceObj for T {
    fn list_pvs<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<String>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::list_pvs(self))
    }
    fn has_pv<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::has_pv(self, name))
    }
    fn get_introspection<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<FieldDesc>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::get_introspection(self, name))
    }
    fn get_value<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<PvField>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::get_value(self, name))
    }
    fn get_value_checked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<PvField>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::get_value_checked(self, checked, ctx))
    }
    fn access_gate<'a>(&'a self) -> &'a AccessGate {
        <Self as ChannelSource>::access(self)
    }
    fn revalidate_read<'a>(
        &'a self,
        pv_name: &'a str,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<AccessChecked>> + Send + 'a>>
    {
        Box::pin(<Self as ChannelSource>::revalidate_read(self, pv_name, ctx))
    }
    fn put_value<'a>(
        &'a self,
        name: &'a str,
        value: PvField,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::put_value(self, name, value))
    }
    fn put_value_checked<'a>(
        &'a self,
        checked: AccessChecked,
        value: PvField,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::put_value_checked(
            self, checked, value, ctx,
        ))
    }
    fn is_writable<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::is_writable(self, name))
    }
    fn subscribe<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::subscribe(self, name))
    }
    fn subscribe_checked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::subscribe_checked(
            self, checked, ctx,
        ))
    }
    fn subscribe_raw<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<RawMonitorEvent>>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::subscribe_raw(self, name))
    }
    fn subscribe_raw_checked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<RawMonitorEvent>>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::subscribe_raw_checked(
            self, checked, ctx,
        ))
    }
    fn rpc<'a>(
        &'a self,
        name: &'a str,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(FieldDesc, PvField), String>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::rpc(
            self,
            name,
            request_desc,
            request_value,
        ))
    }
    fn rpc_checked<'a>(
        &'a self,
        checked: AccessChecked,
        request_desc: FieldDesc,
        request_value: PvField,
        ctx: ChannelContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(FieldDesc, PvField), String>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::rpc_checked(
            self,
            checked,
            request_desc,
            request_value,
            ctx,
        ))
    }
    fn notify_watermark_high(&self, name: &str) {
        <Self as ChannelSource>::notify_watermark_high(self, name);
    }
    fn notify_watermark_low(&self, name: &str) {
        <Self as ChannelSource>::notify_watermark_low(self, name);
    }
}
