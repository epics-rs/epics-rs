//! [`ChannelSource`] — the trait every native PVA server is generic over.
//!
//! Replaces the spvirit `PvStore` trait. Uses our own [`crate::pvdata`]
//! types, so no `spvirit_*` types appear in the public surface.

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::pvdata::{FieldDesc, PvField};

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

    /// Fetch the current value of a PV with the downstream peer's
    /// [`ChannelContext`]. Default impl ignores the context and
    /// forwards to [`Self::get_value`]; ACF-aware sources override
    /// this so READ may be denied per epics-base PR #563/#618.
    fn get_value_ctx(
        &self,
        name: &str,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Option<PvField>> + Send {
        let _ = ctx;
        self.get_value(name)
    }

    /// Apply a PUT.
    fn put_value(
        &self,
        name: &str,
        value: PvField,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;

    /// Apply a PUT with the downstream peer's [`ChannelContext`].
    /// Default impl forwards to [`Self::put_value`] (ignores ctx);
    /// gateways override this to route the put through a
    /// per-credential upstream client pool. Mirrors pvxs's
    /// `Source::onCreate` plumbing of the connecting peer's
    /// credentials.
    fn put_value_ctx(
        &self,
        name: &str,
        value: PvField,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        let _ = ctx;
        self.put_value(name, value)
    }

    /// True iff PUT is allowed against this PV (for ACL gating).
    fn is_writable(&self, name: &str) -> impl std::future::Future<Output = bool> + Send;

    /// Subscribe to value-change notifications. Returns `None` if unknown.
    fn subscribe(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send;

    /// Context-aware subscribe. Default impl delegates to
    /// [`Self::subscribe`]; ACF-aware sources override to deny
    /// MONITOR when the peer has no READ access.
    fn subscribe_ctx(
        &self,
        name: &str,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send {
        let _ = ctx;
        self.subscribe(name)
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

    /// Context-aware raw-frame subscribe. Round-30A audit (R31-G6)
    /// found that the round-29 ACF gate covered the decoded
    /// `subscribe_ctx` path but NOT the raw-frame fast path: a
    /// gateway peer with `NoAccess` on a PV's ASG could still mount
    /// the raw `subscribe_raw` subscription and receive every
    /// upstream MONITOR event verbatim. Default impl delegates to
    /// the legacy `subscribe_raw`; ACF-aware sources MUST override
    /// to deny when the peer lacks READ.
    fn subscribe_raw_ctx(
        &self,
        name: &str,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<RawMonitorEvent>>> + Send {
        let _ = ctx;
        self.subscribe_raw(name)
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

    /// Context-aware RPC dispatch. Round-37 (R37-G1) audit found
    /// that PVA RPC bypassed every gateway ACF gate: the round-29
    /// ACL covered GET / PUT / MONITOR (and round-32A covered the
    /// raw-frame fast path), but tcp.rs's `OpKind::Rpc` branch
    /// called the legacy `rpc()` — letting a `NoAccess` peer
    /// invoke action RPCs (archiver controls, custom records,
    /// gateway-proxied RPCs) without authentication-aware gating.
    /// Default impl delegates; ACF-aware sources MUST override.
    fn rpc_ctx(
        &self,
        name: &str,
        request_desc: FieldDesc,
        request_value: PvField,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Result<(FieldDesc, PvField), String>> + Send {
        let _ = ctx;
        self.rpc(name, request_desc, request_value)
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
    fn get_value_ctx<'a>(
        &'a self,
        name: &'a str,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<PvField>> + Send + 'a>>;
    fn put_value<'a>(
        &'a self,
        name: &'a str,
        value: PvField,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>;
    fn put_value_ctx<'a>(
        &'a self,
        name: &'a str,
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
    fn subscribe_ctx<'a>(
        &'a self,
        name: &'a str,
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
    fn subscribe_raw_ctx<'a>(
        &'a self,
        name: &'a str,
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
    fn rpc_ctx<'a>(
        &'a self,
        name: &'a str,
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
    fn get_value_ctx<'a>(
        &'a self,
        name: &'a str,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<PvField>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::get_value_ctx(self, name, ctx))
    }
    fn put_value<'a>(
        &'a self,
        name: &'a str,
        value: PvField,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::put_value(self, name, value))
    }
    fn put_value_ctx<'a>(
        &'a self,
        name: &'a str,
        value: PvField,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::put_value_ctx(
            self, name, value, ctx,
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
    fn subscribe_ctx<'a>(
        &'a self,
        name: &'a str,
        ctx: ChannelContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::subscribe_ctx(self, name, ctx))
    }
    fn subscribe_raw<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<RawMonitorEvent>>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::subscribe_raw(self, name))
    }
    fn subscribe_raw_ctx<'a>(
        &'a self,
        name: &'a str,
        ctx: ChannelContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<RawMonitorEvent>>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::subscribe_raw_ctx(self, name, ctx))
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
    fn rpc_ctx<'a>(
        &'a self,
        name: &'a str,
        request_desc: FieldDesc,
        request_value: PvField,
        ctx: ChannelContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(FieldDesc, PvField), String>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::rpc_ctx(
            self,
            name,
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
