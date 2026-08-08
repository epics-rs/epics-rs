//! Tower-style middleware for the PVA gateway.
//!
//! Wraps the underlying [`epics_pva_rs::server_native::ChannelSource`]
//! ([`super::source::GatewayChannelSource`] in practice) with
//! composable [`Layer`]s that add cross-cutting concerns:
//!
//! - [`AclLayer`] — refuse `has_pv` / `put_value` for PV names that
//!   match the deny list. Patterns may be glob (`*` wildcard) or,
//!   via [`AclConfig::deny_regex`] / [`AclConfig::allow_regex`],
//!   full anchored regular expressions (B7).
//! - [`ReadOnlyLayer`] — fail every PUT, even if upstream allows
//!   it. Mirrors the existing `read_only` flag but as a composable
//!   layer so operators can stack it with audit / ACL.
//! - [`AuditLayer`] — emit a structured event for every PUT,
//!   reusing the existing audit pipeline shape
//!
//! Design constraint: the inner [`ChannelSource`] is the gateway's
//! own [`super::source::GatewayChannelSource`], and we want the
//! `Layer` chain to short-circuit BEFORE the call reaches it (so an
//! ACL deny doesn't trigger an upstream search). Each [`Layer`]
//! implementation forwards calls verbatim by default; override the
//! method to insert pre/post hooks.

// RTEMS-EXEC-MODEL-ALLOW(18): checked to pass feature-ON under
// --features rtems-exec-model,pva-gateway (the gateway's spawns/timers ride the
// runtime::task seam). The default feature-ON gate omits `pva-gateway`, so re-run
// that combo when touching this module.

use std::net::SocketAddr;
use std::sync::Arc;

use epics_pva_rs::pvdata::{FieldDesc, PvField, RpcReply};
use epics_pva_rs::server_native::ChannelContext;
use epics_pva_rs::server_native::source::{
    ChannelSource, DynSource, MonitorStream, OpError, OpErrorKind, RawMonitorEvent, SourceRead,
    WatermarkEvent,
};

/// Wrap a [`ChannelSource`] and produce a new one with extra
/// behaviour. Implementations override only the methods they need;
/// the default forwards every call unchanged.
pub trait Layer<S: ChannelSource>: Send + Sync + 'static {
    type Wrapped: ChannelSource;
    fn layer(self, inner: S) -> Self::Wrapped;
}

// ── ReadOnlyLayer ────────────────────────────────────────────────

/// Reject every PUT regardless of upstream policy. Composable
/// with audit / ACL — stacking `ReadOnlyLayer` last in the chain
/// guarantees no PUT can reach the underlying source even if a
/// later layer would have allowed it.
pub struct ReadOnlyLayer;

pub struct ReadOnly<S> {
    inner: Arc<S>,
}

impl<S: ChannelSource> Layer<S> for ReadOnlyLayer {
    type Wrapped = ReadOnly<S>;
    fn layer(self, inner: S) -> ReadOnly<S> {
        ReadOnly {
            inner: Arc::new(inner),
        }
    }
}

impl<S: ChannelSource> ChannelSource for ReadOnly<S> {
    // forward the inner source's AccessGate so the wire
    // layer's `gate.check` flows to the actual policy holder, not
    // a permissive Open singleton.
    fn access(&self) -> &epics_pva_rs::server_native::source::AccessGate {
        self.inner.access()
    }
    async fn list_pvs(&self) -> Vec<String> {
        self.inner.list_pvs().await
    }
    async fn has_pv(&self, name: &str) -> bool {
        self.inner.has_pv(name).await
    }
    async fn get_introspection(&self, name: &str) -> Option<FieldDesc> {
        self.inner.get_introspection(name).await
    }
    // forward the credential-aware existence/introspection
    // variants to the inner. Without these, the trait default would
    // delegate to THIS layer's ctx-less `has_pv`/`get_introspection`,
    // dropping the downstream peer's identity before it reaches a
    // gateway inner source — the same ctx-severing bug the typed-op
    // forwarders below already guard against.
    async fn has_pv_checked(&self, name: &str, ctx: ChannelContext) -> bool {
        self.inner.has_pv_checked(name, ctx).await
    }
    async fn get_introspection_checked(
        &self,
        name: &str,
        ctx: ChannelContext,
    ) -> Option<FieldDesc> {
        self.inner.get_introspection_checked(name, ctx).await
    }
    async fn get_value(&self, name: &str) -> Option<PvField> {
        self.inner.get_value(name).await
    }
    async fn put_value(&self, _name: &str, _value: PvField) -> Result<(), OpError> {
        Err(OpError::denied("read-only mode: PUT rejected"))
    }
    async fn put_value_checked(
        &self,
        _checked: epics_pva_rs::server_native::source::AccessChecked,
        _value: PvField,
        _ctx: ChannelContext,
    ) -> Result<(), OpError> {
        Err(OpError::denied("read-only mode: PUT rejected"))
    }
    // A BitSet-delta PUT is still a PUT. Reject it here exactly the
    // way `put_value_checked` does — without this override the trait
    // default would run get_value + merge + put_value_checked, which
    // happens to still reject (put_value_checked above), but only
    // after a wasted read-merge and after bypassing the inner
    // source's atomic `put_delta_checked`. Short-circuit instead.
    async fn put_delta_checked(
        &self,
        _checked: epics_pva_rs::server_native::source::AccessChecked,
        _desc: std::sync::Arc<FieldDesc>,
        _changed: epics_pva_rs::proto::BitSet,
        _delta: &PvField,
        _ctx: ChannelContext,
    ) -> Result<(), OpError> {
        Err(OpError::denied("read-only mode: PUT rejected"))
    }
    // A PUT_GET writes — reject it exactly as `put_delta_checked` does, so
    // a PUT_GET never reaches the inner source under read-only mode. The
    // trait default would call `put_delta_checked` (which rejects), but make
    // the rejection explicit and skip the wasted readback path.
    async fn put_get_checked(
        &self,
        _checked: epics_pva_rs::server_native::source::AccessChecked,
        _desc: std::sync::Arc<FieldDesc>,
        _changed: epics_pva_rs::proto::BitSet,
        _delta: &PvField,
        _ctx: ChannelContext,
    ) -> Result<Option<SourceRead>, OpError> {
        Err(OpError::denied("read-only mode: PUT rejected"))
    }
    // PROCESS mutates upstream record state — it is a WRITE-class op,
    // so reject it here exactly the way `put_value` does. Without this
    // override the trait-default `process` (`Ok(())`) would falsely
    // report success and let a PROCESS slip past read-only mode.
    async fn process(&self, _name: &str) -> Result<(), OpError> {
        Err(OpError::denied("read-only mode: PROCESS rejected"))
    }
    // Typed PROCESS — same WRITE-class rejection as `process` above.
    // Without this override the trait-default `process_checked` would
    // fall through to `process` (rejected) but only after the gate
    // check; short-circuit here so a PROCESS never reaches the inner
    // source under read-only mode.
    async fn process_checked(
        &self,
        _checked: epics_pva_rs::server_native::source::AccessChecked,
        _ctx: ChannelContext,
    ) -> Result<(), OpError> {
        Err(OpError::denied("read-only mode: PROCESS rejected"))
    }
    async fn is_writable(&self, _name: &str) -> bool {
        false
    }
    async fn subscribe(&self, name: &str) -> Option<MonitorStream<PvField>> {
        self.inner.subscribe(name).await
    }
    // Forward type-state op variants to the inner. Without these,
    // the trait defaults would route through this layer's ctx-less
    // (`get_value`/`subscribe`/`subscribe_raw`/`rpc`) — silently
    // skipping any ctx-aware behaviour the inner installs (gateway
    // per-credential routing, audit ctx fields).
    async fn get_value_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
    ) -> Option<PvField> {
        self.inner.get_value_checked(checked, ctx).await
    }
    // Every layer that forwards `get_value_checked` MUST forward
    // `read_checked` the same way: the read-FRAMING path (GET reply,
    // PUT_GET readback, monitor seed) goes through `read_checked`, and its
    // trait default re-derives the value from THIS layer's
    // `get_value_checked` as `marked: None` — discarding the marked leaves
    // the inner source declared and framing a full mask over them. For the
    // gateway that means shipping leaves the upstream never assigned.
    // Reads are permitted under read-only mode, so this is a plain forward.
    async fn read_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
    ) -> Option<SourceRead> {
        self.inner.read_checked(checked, ctx).await
    }
    async fn subscribe_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
    ) -> Option<MonitorStream<PvField>> {
        self.inner.subscribe_checked(checked, ctx).await
    }
    async fn subscribe_raw(&self, name: &str) -> Option<MonitorStream<RawMonitorEvent>> {
        self.inner.subscribe_raw(name).await
    }
    async fn subscribe_raw_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
    ) -> Option<MonitorStream<RawMonitorEvent>> {
        self.inner.subscribe_raw_checked(checked, ctx).await
    }
    // forward the cooked (marked) event-affecting-options
    // MONITOR — the entry point the PVA server dispatches on — to the
    // inner. Without this the wrapper's `_marked` falls to the trait
    // default, which routes through this layer's own
    // `subscribe_checked_opts` and would hide the inner gateway's options
    // (which it must reject) and its marked set. A read-only gateway
    // places no extra constraint on a (read) MONITOR — pure pass-through.
    async fn subscribe_checked_opts_marked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
        opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> Option<MonitorStream<epics_pva_rs::server_native::MonitorUpdate>> {
        self.inner
            .subscribe_checked_opts_marked(checked, ctx, opts)
            .await
    }
    async fn subscribe_raw_checked_opts(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
        opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> Option<MonitorStream<RawMonitorEvent>> {
        self.inner
            .subscribe_raw_checked_opts(checked, ctx, opts)
            .await
    }
    // Forward the single-seed MONITOR to the inner source so a
    // self-seeding inner (a gateway's atomic cached snapshot) supplies
    // the connect-time seed; the default would seed via inner.get_value
    // and bypass that atomic seed.
    async fn subscribe_seeded(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
        opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> Option<
        epics_pva_rs::server_native::source::SubscriptionSeed<
            epics_pva_rs::server_native::source::MonitorUpdate,
        >,
    > {
        self.inner.subscribe_seeded(checked, ctx, opts).await
    }
    async fn subscribe_raw_seeded(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
        opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> Option<epics_pva_rs::server_native::source::SubscriptionSeed<RawMonitorEvent>> {
        self.inner.subscribe_raw_seeded(checked, ctx, opts).await
    }
    // RPC is commonly state-mutating and pva2pva blocks
    // `createChannelRPC` under `p2pReadOnly` exactly the way it blocks
    // Put/Process/PutGet (`channel.cpp:140-150`, the `if(!p2pReadOnly)`
    // guard; only Get/Monitor are unconditional). Reject it here as
    // PUT/PROCESS are, rather than forwarding to the inner source —
    // without this override a read-only gateway forwarded downstream
    // RPCs straight to the upstream server.
    async fn rpc(
        &self,
        _name: &str,
        _request_desc: FieldDesc,
        _request_value: PvField,
    ) -> Result<RpcReply, OpError> {
        Err(OpError::denied("read-only mode: RPC rejected"))
    }
    async fn rpc_checked(
        &self,
        _checked: epics_pva_rs::server_native::source::AccessChecked,
        _request_desc: FieldDesc,
        _request_value: PvField,
        _ctx: ChannelContext,
    ) -> Result<RpcReply, OpError> {
        Err(OpError::denied("read-only mode: RPC rejected"))
    }
    // ChannelArray: forward the read-class sub-ops (INIT descriptor probe,
    // getArray, getLength) to the inner so a wrapped gateway source still
    // serves them; reject the write-class sub-ops (putArray, setLength)
    // exactly as `put_*_checked` reject PUT. Without these overrides the
    // trait default ("not supported") would mask a wrapped inner's array
    // support entirely (the wrapper-severs-override defect family). This is
    // stricter than pva2pva, whose `createChannelArray` lacks the
    // `p2pReadOnly` guard its Put/PutGet/Process/RPC creates carry
    // (`channel.cpp:227-232` vs `:118-148`) and so leaks array writes under
    // read-only mode; putArray/setLength mutate upstream array state and are
    // WRITE-class here, so a read-only gateway must refuse them.
    async fn channel_array_init(
        &self,
        name: &str,
        ctx: ChannelContext,
    ) -> Result<FieldDesc, OpError> {
        self.inner.channel_array_init(name, ctx).await
    }
    async fn channel_array_get(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        offset: u32,
        count: u32,
        stride: u32,
        ctx: ChannelContext,
    ) -> Result<PvField, OpError> {
        self.inner
            .channel_array_get(checked, offset, count, stride, ctx)
            .await
    }
    async fn channel_array_get_length(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
    ) -> Result<u32, OpError> {
        self.inner.channel_array_get_length(checked, ctx).await
    }
    async fn channel_array_put(
        &self,
        _checked: epics_pva_rs::server_native::source::AccessChecked,
        _offset: u32,
        _stride: u32,
        _value: PvField,
        _ctx: ChannelContext,
    ) -> Result<(), OpError> {
        Err(OpError::denied("read-only mode: ARRAY putArray rejected"))
    }
    async fn channel_array_set_length(
        &self,
        _checked: epics_pva_rs::server_native::source::AccessChecked,
        _length: u32,
        _ctx: ChannelContext,
    ) -> Result<(), OpError> {
        Err(OpError::denied("read-only mode: ARRAY setLength rejected"))
    }
    // forward the per-PV watermark levels so the inner
    // gateway source's `monitor_watermarks` override is reachable
    // through the wrapper stack. Without this the server's monitor loop
    // sees the trait default `None` and never fires the pause/resume
    // callbacks — the same wrapper-severs-override defect family as
    // FR-8's `has_pv_checked`/`get_introspection_checked`.
    async fn monitor_watermarks(&self, name: &str) -> Option<(usize, usize)> {
        self.inner.monitor_watermarks(name).await
    }
    fn notify_watermark(&self, name: &str, ctx: &ChannelContext, ev: WatermarkEvent) {
        self.inner.notify_watermark(name, ctx, ev);
    }
    // same wrapper-severs-override defect family as
    // `notify_watermark`/`monitor_watermarks` above — a transparent
    // middleware layer that forwards one notify_* sibling but not the
    // other would sever the inner source's monitor-start (onStart)
    // callback (e.g. a wrapped `SharedPV::set_on_start`). Forward the
    // Idle↔Executing edge unchanged.
    fn notify_monitor_start(&self, name: &str, ctx: &ChannelContext, start: bool) {
        self.inner.notify_monitor_start(name, ctx, start);
    }
    // Same wrapper-severs-override defect family as the `notify_*` and
    // `*_checked` forwards above: a transparent middleware layer must
    // delegate every descriptive / advertisement / channel-lifecycle
    // method whose inner override the trait default would otherwise mask.
    // All of these are queried or fired on the BOUND OWNER, which for a
    // middleware wrapper is the wrapper itself (`resolve_owner` keeps the
    // default `None` so every op keeps routing through this layer).
    // Forwarding them preserves a wrapped source's SEARCH advertisement
    // (`searchable`/`searchable_from`, e.g. a ServerInfoSource that hides
    // from UDP discovery), its registry beacon-change signal, its
    // per-channel report info (captured once at admission and surfaced in
    // the server report), and its channel open/close lifecycle callbacks
    // (e.g. a SharedPV acquiring/releasing a per-channel lease).
    // `resolve_owner` is deliberately NOT forwarded — doing so would bind
    // the inner as the channel owner and bypass this layer entirely.
    fn beacon_change(&self) -> u64 {
        self.inner.beacon_change()
    }
    async fn searchable(&self, name: &str) -> bool {
        self.inner.searchable(name).await
    }
    async fn searchable_from(&self, name: &str, requester: SocketAddr) -> bool {
        self.inner.searchable_from(name, requester).await
    }
    async fn channel_report_info(&self, name: &str, ctx: ChannelContext) -> Option<String> {
        self.inner.channel_report_info(name, ctx).await
    }
    fn notify_channel_open(&self, name: &str, ctx: &ChannelContext) {
        self.inner.notify_channel_open(name, ctx);
    }
    fn notify_channel_close(&self, name: &str, ctx: &ChannelContext) {
        self.inner.notify_channel_close(name, ctx);
    }
    // R17-33: the delegation contract of a transparent layer — EVERY
    // trait method whose behaviour an inner source can override must be
    // forwarded, or the trait default silently replaces the inner's
    // implementation. The server hands its ChannelInvalidator to the BOUND
    // source (`runtime.rs`), which in a real gateway is this wrapper stack;
    // the trait default drops it, so the gateway's cache never got a handle
    // and an operator `<prefix>:drop` / `:flush` could not send
    // DESTROY_CHANNEL to the live downstream channels (pva2pva
    // `p2pApp/server.cpp:130-135`). Same class: `revalidate_read` (the
    // per-event ACL re-check on policy reload), `check_monitor_request`
    // (the INIT-time `record._options` validation a source may reject the
    // circuit on), and `subscribe_checked_opts` (the negotiated
    // MonitorOptions — the trait default re-routes to `subscribe_checked`
    // and DROPS them).
    fn set_channel_invalidator(
        &self,
        invalidator: epics_pva_rs::server_native::source::ChannelInvalidator,
    ) {
        self.inner.set_channel_invalidator(invalidator);
    }
    async fn revalidate_read(
        &self,
        pv_name: &str,
        ctx: ChannelContext,
    ) -> Option<epics_pva_rs::server_native::source::AccessChecked> {
        // Read-only adds no READ policy — the inner's gate decides.
        self.inner.revalidate_read(pv_name, ctx).await
    }
    async fn check_monitor_request(
        &self,
        checked: &epics_pva_rs::server_native::source::AccessChecked,
        ctx: &ChannelContext,
    ) -> Result<(), epics_pva_rs::server_native::source::OpError> {
        self.inner.check_monitor_request(checked, ctx).await
    }
    async fn subscribe_checked_opts(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
        opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> Option<MonitorStream<PvField>> {
        self.inner.subscribe_checked_opts(checked, ctx, opts).await
    }
}

// ── AclLayer ─────────────────────────────────────────────────────

/// Pattern-matched access control. PV names matching any `deny`
/// pattern are rejected at the layer before reaching the upstream
/// proxy. `allow_only` (when non-empty) flips the policy — only
/// names matching one of these patterns get through.
///
/// B7: two pattern syntaxes are supported and may be mixed freely.
///
/// * **Glob** — the `deny` / `allow_only` `Vec<String>` fields.
///   `*` matches any run of characters; a pattern with no `*` is an
///   exact match. Backward-compatible with the original AclConfig.
/// * **Regex** — added via [`AclConfig::deny_regex`] /
///   [`AclConfig::allow_regex`]. The pattern is a full
///   [`regex`]-crate regular expression, **anchored** at both ends
///   (the matcher wraps it as `^(?:pattern)$`) so `BL10C:.*`
///   matches the same names a `BL10C:*` glob would, and a bare
///   `MOTOR:VAL` regex matches only that exact name. Regexes are
///   compiled once at config-build time, not per PV check.
///
/// A name is allowed iff it matches **no** deny pattern (glob or
/// regex) AND, when any allow pattern is configured, matches **at
/// least one** allow pattern (glob or regex). Deny always wins.
#[derive(Clone, Default)]
pub struct AclConfig {
    /// Glob deny patterns (`*` wildcard / exact match).
    pub deny: Vec<String>,
    /// Glob allow-only patterns (`*` wildcard / exact match).
    pub allow_only: Vec<String>,
    /// B7: compiled regex deny patterns. Built via
    /// [`AclConfig::deny_regex`]; anchored at both ends.
    deny_re: Vec<regex::Regex>,
    /// B7: compiled regex allow-only patterns. Built via
    /// [`AclConfig::allow_regex`]; anchored at both ends.
    allow_re: Vec<regex::Regex>,
}

impl AclConfig {
    /// B7: add a regex pattern to the deny list. The pattern is
    /// anchored (`^(?:pattern)$`) and compiled immediately; an
    /// invalid pattern returns the [`regex::Error`] so the operator
    /// learns about the typo at config time, not on the first PV
    /// check.
    pub fn deny_regex(mut self, pattern: &str) -> Result<Self, regex::Error> {
        self.deny_re.push(compile_anchored(pattern)?);
        Ok(self)
    }

    /// B7: add a regex pattern to the allow-only list. Anchored and
    /// compiled like [`Self::deny_regex`]. As with glob
    /// `allow_only`, a non-empty allow list flips the policy to
    /// default-deny.
    pub fn allow_regex(mut self, pattern: &str) -> Result<Self, regex::Error> {
        self.allow_re.push(compile_anchored(pattern)?);
        Ok(self)
    }

    /// True iff any allow pattern (glob or regex) is configured —
    /// i.e. the policy is default-deny.
    fn has_allow_list(&self) -> bool {
        !self.allow_only.is_empty() || !self.allow_re.is_empty()
    }

    pub fn allowed(&self, name: &str) -> bool {
        // Deny always wins — glob or regex.
        if self.deny.iter().any(|p| matches_pattern(p, name))
            || self.deny_re.iter().any(|re| re.is_match(name))
        {
            return false;
        }
        // Allow-only: when configured, the name must match at least
        // one allow pattern (glob or regex).
        if self.has_allow_list() {
            let allowed = self.allow_only.iter().any(|p| matches_pattern(p, name))
                || self.allow_re.iter().any(|re| re.is_match(name));
            if !allowed {
                return false;
            }
        }
        true
    }
}

/// B7: per-pattern compiled-program size limit for operator-supplied
/// ACL regexes. 256 KiB is far more than any realistic PV-name
/// pattern needs (a generous allow/deny list compiles to a few KiB)
/// while still rejecting a pathological pattern up front instead of
/// letting it consume unbounded memory at compile time.
const ACL_REGEX_SIZE_LIMIT: usize = 256 * 1024;

/// B7: compile `pattern` as a both-ends-anchored regex so a regex
/// ACL entry matches whole PV names, mirroring glob semantics
/// (`BL10C:*` ≡ `BL10C:.*`, bare token ≡ exact match). Wrapping in a
/// non-capturing group keeps top-level alternation (`a|b`) anchored
/// as `^(?:a|b)$` rather than `^a|b$`.
///
/// Compiled via [`regex::RegexBuilder`] with explicit `size_limit`
/// and `dfa_size_limit` so an operator-supplied pathological pattern
/// fails fast with a bounded `regex::Error` rather than compiling
/// with the crate defaults (which allow far larger programs).
fn compile_anchored(pattern: &str) -> Result<regex::Regex, regex::Error> {
    regex::RegexBuilder::new(&format!("^(?:{pattern})$"))
        .size_limit(ACL_REGEX_SIZE_LIMIT)
        .dfa_size_limit(ACL_REGEX_SIZE_LIMIT)
        .build()
}

/// Match `name` against a glob `pattern` where each `*` matches any
/// (possibly empty) run of characters and every other character is a
/// literal. Supports leading, trailing, interior, and multiple `*`
/// (e.g. `MOTOR:*:JOG`, `A*B*C`), matching ca-gateway's regex-backed
/// pvlist semantics for `*` (`gateAs.cc` compiles each pvlist pattern
/// to an anchored regex). Only `*` is special — for full regular
/// expressions use the `allow_regex` / `deny_regex` lists.
fn matches_pattern(pattern: &str, name: &str) -> bool {
    // No wildcard → exact match.
    if !pattern.contains('*') {
        return name == pattern;
    }

    // Split on `*`; N stars yield N+1 literal segments. An empty
    // segment comes from a leading/trailing/consecutive `*` and imposes
    // no constraint. Non-empty segments must appear in order; a
    // non-empty first segment is anchored to the start, a non-empty
    // last segment to the end.
    let segments: Vec<&str> = pattern.split('*').collect();
    let last = segments.len() - 1;
    let mut pos = 0; // byte offset into `name` already consumed

    for (i, seg) in segments.iter().enumerate() {
        if seg.is_empty() {
            continue;
        }
        if i == 0 {
            // Leading literal (pattern does not start with `*`).
            if !name.starts_with(seg) {
                return false;
            }
            pos = seg.len();
        } else if i == last {
            // Trailing literal (pattern does not end with `*`): pin to
            // the end, ensuring it does not overlap the consumed prefix.
            if name.len() < pos + seg.len() || !name[pos..].ends_with(seg) {
                return false;
            }
        } else {
            // Interior literal: must occur at or after `pos`.
            match name[pos..].find(seg) {
                Some(off) => pos += off + seg.len(),
                None => return false,
            }
        }
    }
    true
}

pub struct AclLayer {
    config: AclConfig,
}

impl AclLayer {
    pub fn new(config: AclConfig) -> Self {
        Self { config }
    }
}

pub struct Acl<S> {
    inner: Arc<S>,
    config: AclConfig,
}

impl<S: ChannelSource> Layer<S> for AclLayer {
    type Wrapped = Acl<S>;
    fn layer(self, inner: S) -> Acl<S> {
        Acl {
            inner: Arc::new(inner),
            config: self.config,
        }
    }
}

impl<S: ChannelSource> ChannelSource for Acl<S> {
    async fn list_pvs(&self) -> Vec<String> {
        // Filter the underlying list so introspection sweeps don't
        // leak the names of denied PVs.
        let mut names = self.inner.list_pvs().await;
        names.retain(|n| self.config.allowed(n));
        names
    }
    async fn has_pv(&self, name: &str) -> bool {
        if !self.config.allowed(name) {
            return false;
        }
        self.inner.has_pv(name).await
    }
    async fn get_introspection(&self, name: &str) -> Option<FieldDesc> {
        if !self.config.allowed(name) {
            return None;
        }
        self.inner.get_introspection(name).await
    }
    // gate the credential-aware existence/introspection
    // variants by the same static allowlist as `has_pv`/
    // `get_introspection`, then forward to the inner's `*_checked` so a
    // gateway inner source resolves under THIS peer's identity. Without
    // these the trait default would delegate to this layer's ctx-less
    // `has_pv`/`get_introspection`, dropping the downstream credentials.
    async fn has_pv_checked(&self, name: &str, ctx: ChannelContext) -> bool {
        if !self.config.allowed(name) {
            return false;
        }
        self.inner.has_pv_checked(name, ctx).await
    }
    async fn get_introspection_checked(
        &self,
        name: &str,
        ctx: ChannelContext,
    ) -> Option<FieldDesc> {
        if !self.config.allowed(name) {
            return None;
        }
        self.inner.get_introspection_checked(name, ctx).await
    }
    async fn get_value(&self, name: &str) -> Option<PvField> {
        if !self.config.allowed(name) {
            return None;
        }
        self.inner.get_value(name).await
    }
    async fn put_value(&self, name: &str, value: PvField) -> Result<(), OpError> {
        if !self.config.allowed(name) {
            return Err(OpError::denied(format!("ACL: PV '{name}' denied")));
        }
        self.inner.put_value(name, value).await
    }
    // PROCESS mutates upstream record state — a WRITE-class op. Gate
    // it by the layer's static allowlist BEFORE forwarding, exactly
    // the way `put_value` is gated. Without this override the trait
    // default `process` (`Ok(())`) bypasses the ACL entirely.
    async fn process(&self, name: &str) -> Result<(), OpError> {
        if !self.config.allowed(name) {
            return Err(OpError::denied(format!("ACL: PV '{name}' denied")));
        }
        self.inner.process(name).await
    }
    // type-state op variants gate by the layer's static
    // allowlist BEFORE delegating. The inner source still gets the
    // full AccessChecked + ctx and may apply its own ACF / per-
    // credential routing on top.
    fn access(&self) -> &epics_pva_rs::server_native::source::AccessGate {
        self.inner.access()
    }
    async fn get_value_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
    ) -> Option<PvField> {
        if !self.config.allowed(checked.pv_name()) {
            return None;
        }
        self.inner.get_value_checked(checked, ctx).await
    }
    /// Same allowlist gate as `get_value_checked`, on the read-FRAMING path
    /// (GET reply / PUT_GET readback / monitor seed). Without this override
    /// the trait default would re-read through this layer's
    /// `get_value_checked` and drop the inner's marked leaves — see the note
    /// on `ReadOnlyLayer::read_checked`.
    async fn read_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
    ) -> Option<SourceRead> {
        if !self.config.allowed(checked.pv_name()) {
            return None;
        }
        self.inner.read_checked(checked, ctx).await
    }
    async fn put_value_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        value: PvField,
        ctx: ChannelContext,
    ) -> Result<(), OpError> {
        if !self.config.allowed(checked.pv_name()) {
            return Err(OpError::denied(format!(
                "ACL: PV '{}' denied",
                checked.pv_name()
            )));
        }
        self.inner.put_value_checked(checked, value, ctx).await
    }
    // A BitSet-delta PUT is a PUT — gate it by the same static
    // allowlist as `put_value_checked`, then forward to the inner's
    // `put_delta_checked`. Without this override the trait default
    // would run get_value + merge + put_value_checked on THIS layer,
    // bypassing the inner source's atomic `put_delta_checked`
    // (`SharedSource` / `CompositeSource` merge under one lock) and
    // re-opening the concurrent-partial-PUT lost-update window.
    async fn put_delta_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        desc: std::sync::Arc<FieldDesc>,
        changed: epics_pva_rs::proto::BitSet,
        delta: &PvField,
        ctx: ChannelContext,
    ) -> Result<(), OpError> {
        if !self.config.allowed(checked.pv_name()) {
            return Err(OpError::denied(format!(
                "ACL: PV '{}' denied",
                checked.pv_name()
            )));
        }
        self.inner
            .put_delta_checked(checked, desc, changed, delta, ctx)
            .await
    }
    // A PUT_GET writes — gate it by the same static allowlist as
    // `put_value_checked`/`put_delta_checked`, then forward to the inner's
    // atomic `put_get_checked`. Without this override the trait default
    // decomposes into `put_delta_checked` + `get_value_checked` on THIS
    // layer, bypassing the inner source's single-op PUT_GET (the gateway's
    // one-upstream-PUT_GET forward).
    async fn put_get_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        desc: std::sync::Arc<FieldDesc>,
        changed: epics_pva_rs::proto::BitSet,
        delta: &PvField,
        ctx: ChannelContext,
    ) -> Result<Option<SourceRead>, OpError> {
        if !self.config.allowed(checked.pv_name()) {
            return Err(OpError::denied(format!(
                "ACL: PV '{}' denied",
                checked.pv_name()
            )));
        }
        self.inner
            .put_get_checked(checked, desc, changed, delta, ctx)
            .await
    }
    async fn is_writable(&self, name: &str) -> bool {
        self.config.allowed(name) && self.inner.is_writable(name).await
    }
    async fn subscribe(&self, name: &str) -> Option<MonitorStream<PvField>> {
        if !self.config.allowed(name) {
            return None;
        }
        self.inner.subscribe(name).await
    }
    async fn subscribe_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
    ) -> Option<MonitorStream<PvField>> {
        if !self.config.allowed(checked.pv_name()) {
            return None;
        }
        self.inner.subscribe_checked(checked, ctx).await
    }
    async fn subscribe_raw(&self, name: &str) -> Option<MonitorStream<RawMonitorEvent>> {
        if !self.config.allowed(name) {
            return None;
        }
        self.inner.subscribe_raw(name).await
    }
    async fn subscribe_raw_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
    ) -> Option<MonitorStream<RawMonitorEvent>> {
        if !self.config.allowed(checked.pv_name()) {
            return None;
        }
        self.inner.subscribe_raw_checked(checked, ctx).await
    }
    // forward the cooked (marked) event-affecting-options
    // MONITOR — the server's dispatch entry point — applying the same ACL
    // deny-list gate as `subscribe_*_checked`.
    async fn subscribe_checked_opts_marked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
        opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> Option<MonitorStream<epics_pva_rs::server_native::MonitorUpdate>> {
        if !self.config.allowed(checked.pv_name()) {
            return None;
        }
        self.inner
            .subscribe_checked_opts_marked(checked, ctx, opts)
            .await
    }
    async fn subscribe_raw_checked_opts(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
        opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> Option<MonitorStream<RawMonitorEvent>> {
        if !self.config.allowed(checked.pv_name()) {
            return None;
        }
        self.inner
            .subscribe_raw_checked_opts(checked, ctx, opts)
            .await
    }
    // Gate the single-seed MONITOR by the same allowlist, then forward
    // to the inner so a self-seeding inner (gateway atomic snapshot)
    // supplies the seed.
    async fn subscribe_seeded(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
        opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> Option<
        epics_pva_rs::server_native::source::SubscriptionSeed<
            epics_pva_rs::server_native::source::MonitorUpdate,
        >,
    > {
        if !self.config.allowed(checked.pv_name()) {
            return None;
        }
        self.inner.subscribe_seeded(checked, ctx, opts).await
    }
    async fn subscribe_raw_seeded(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
        opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> Option<epics_pva_rs::server_native::source::SubscriptionSeed<RawMonitorEvent>> {
        if !self.config.allowed(checked.pv_name()) {
            return None;
        }
        self.inner.subscribe_raw_seeded(checked, ctx, opts).await
    }
    async fn rpc(
        &self,
        name: &str,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> Result<RpcReply, OpError> {
        if !self.config.allowed(name) {
            return Err(OpError::denied(format!("ACL: PV '{name}' denied")));
        }
        self.inner.rpc(name, request_desc, request_value).await
    }
    async fn rpc_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        request_desc: FieldDesc,
        request_value: PvField,
        ctx: ChannelContext,
    ) -> Result<RpcReply, OpError> {
        if !self.config.allowed(checked.pv_name()) {
            return Err(OpError::denied(format!(
                "ACL: PV '{}' denied",
                checked.pv_name()
            )));
        }
        self.inner
            .rpc_checked(checked, request_desc, request_value, ctx)
            .await
    }
    // Typed PROCESS — gate by the static allowlist BEFORE forwarding,
    // mirroring `rpc_checked` / `put_value_checked`. The inner source
    // still gets the full AccessChecked + ctx and may apply its own
    // ACF gate (PROCESS is WRITE-class) on top.
    async fn process_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
    ) -> Result<(), OpError> {
        if !self.config.allowed(checked.pv_name()) {
            return Err(OpError::denied(format!(
                "ACL: PV '{}' denied",
                checked.pv_name()
            )));
        }
        self.inner.process_checked(checked, ctx).await
    }
    // ChannelArray: gate each sub-op by the static allowlist BEFORE
    // forwarding to the inner, mirroring `put_*_checked` / `rpc_checked` /
    // `process_checked`. Without these the trait default ("not supported")
    // would mask a wrapped inner's array support (the wrapper-severs-override
    // defect family). The inner source still gets the full AccessChecked +
    // ctx and applies its own ACF read/write gate on top.
    async fn channel_array_init(
        &self,
        name: &str,
        ctx: ChannelContext,
    ) -> Result<FieldDesc, OpError> {
        if !self.config.allowed(name) {
            return Err(OpError::denied(format!("ACL: PV '{name}' denied")));
        }
        self.inner.channel_array_init(name, ctx).await
    }
    async fn channel_array_get(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        offset: u32,
        count: u32,
        stride: u32,
        ctx: ChannelContext,
    ) -> Result<PvField, OpError> {
        if !self.config.allowed(checked.pv_name()) {
            return Err(OpError::denied(format!(
                "ACL: PV '{}' denied",
                checked.pv_name()
            )));
        }
        self.inner
            .channel_array_get(checked, offset, count, stride, ctx)
            .await
    }
    async fn channel_array_put(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        offset: u32,
        stride: u32,
        value: PvField,
        ctx: ChannelContext,
    ) -> Result<(), OpError> {
        if !self.config.allowed(checked.pv_name()) {
            return Err(OpError::denied(format!(
                "ACL: PV '{}' denied",
                checked.pv_name()
            )));
        }
        self.inner
            .channel_array_put(checked, offset, stride, value, ctx)
            .await
    }
    async fn channel_array_set_length(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        length: u32,
        ctx: ChannelContext,
    ) -> Result<(), OpError> {
        if !self.config.allowed(checked.pv_name()) {
            return Err(OpError::denied(format!(
                "ACL: PV '{}' denied",
                checked.pv_name()
            )));
        }
        self.inner
            .channel_array_set_length(checked, length, ctx)
            .await
    }
    async fn channel_array_get_length(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
    ) -> Result<u32, OpError> {
        if !self.config.allowed(checked.pv_name()) {
            return Err(OpError::denied(format!(
                "ACL: PV '{}' denied",
                checked.pv_name()
            )));
        }
        self.inner.channel_array_get_length(checked, ctx).await
    }
    /// SEARCH advertisement is gated by the same static allowlist as
    /// [`Self::has_pv`] and [`Self::list_pvs`] — a denied name must not be
    /// answered on UDP discovery.
    ///
    /// The gate lives HERE rather than in the caller. `searchable` is the sole
    /// SEARCH predicate: `CompositeSource` asks it alone, because pvxs's
    /// `onSearch`/`onCreate` are independent callbacks and a source may
    /// legitimately advertise a name it later refuses at create (a
    /// `DBF_NOACCESS` field). While the composite still ANDed `has_pv` into
    /// the search reply, this layer's deny reached SEARCH only as a side
    /// effect of that conjunction, so forwarding `searchable` bare was safe by
    /// accident, not by policy. It is not defensible on its own terms: an ACL
    /// that filters `list_pvs` so an introspection sweep cannot "leak the
    /// names of denied PVs" must not then answer a SEARCH for one.
    async fn searchable(&self, name: &str) -> bool {
        if !self.config.allowed(name) {
            return false;
        }
        self.inner.searchable(name).await
    }
    /// Endpoint-scoped [`Self::searchable`] — same allowlist gate, same reason.
    async fn searchable_from(&self, name: &str, requester: SocketAddr) -> bool {
        if !self.config.allowed(name) {
            return false;
        }
        self.inner.searchable_from(name, requester).await
    }
    // forward the per-PV watermark levels so the inner
    // gateway source's `monitor_watermarks` override is reachable
    // through the wrapper stack. Without this the server's monitor loop
    // sees the trait default `None` and never fires the pause/resume
    // callbacks — the same wrapper-severs-override defect family as
    // FR-8's `has_pv_checked`/`get_introspection_checked`.
    async fn monitor_watermarks(&self, name: &str) -> Option<(usize, usize)> {
        self.inner.monitor_watermarks(name).await
    }
    fn notify_watermark(&self, name: &str, ctx: &ChannelContext, ev: WatermarkEvent) {
        self.inner.notify_watermark(name, ctx, ev);
    }
    // same wrapper-severs-override defect family as
    // `notify_watermark`/`monitor_watermarks` above — a transparent
    // middleware layer that forwards one notify_* sibling but not the
    // other would sever the inner source's monitor-start (onStart)
    // callback (e.g. a wrapped `SharedPV::set_on_start`). Forward the
    // Idle↔Executing edge unchanged.
    fn notify_monitor_start(&self, name: &str, ctx: &ChannelContext, start: bool) {
        self.inner.notify_monitor_start(name, ctx, start);
    }
    // Same wrapper-severs-override defect family as the `notify_*` and
    // `*_checked` forwards above: a transparent middleware layer must
    // delegate every descriptive / advertisement / channel-lifecycle
    // method whose inner override the trait default would otherwise mask.
    // All of these are queried or fired on the BOUND OWNER, which for a
    // middleware wrapper is the wrapper itself (`resolve_owner` keeps the
    // default `None` so every op keeps routing through this layer).
    // Forwarding them preserves a wrapped source's registry beacon-change
    // signal, its per-channel report info (captured once at admission and
    // surfaced in the server report), and its channel open/close lifecycle
    // callbacks (e.g. a SharedPV acquiring/releasing a per-channel lease).
    // The SEARCH advertisement pair (`searchable`/`searchable_from`) is
    // forwarded too, but only after this layer's allowlist gate — see those
    // methods above.
    // `resolve_owner` is deliberately NOT forwarded — doing so would bind
    // the inner as the channel owner and bypass this layer entirely.
    fn beacon_change(&self) -> u64 {
        self.inner.beacon_change()
    }
    async fn channel_report_info(&self, name: &str, ctx: ChannelContext) -> Option<String> {
        self.inner.channel_report_info(name, ctx).await
    }
    fn notify_channel_open(&self, name: &str, ctx: &ChannelContext) {
        self.inner.notify_channel_open(name, ctx);
    }
    fn notify_channel_close(&self, name: &str, ctx: &ChannelContext) {
        self.inner.notify_channel_close(name, ctx);
    }
    // R17-33 (see `ReadOnly`): forward every method whose inner override
    // the trait default would mask — applying this layer's deny-list where
    // the method is a READ gate, so forwarding never widens access.
    fn set_channel_invalidator(
        &self,
        invalidator: epics_pva_rs::server_native::source::ChannelInvalidator,
    ) {
        self.inner.set_channel_invalidator(invalidator);
    }
    async fn revalidate_read(
        &self,
        pv_name: &str,
        ctx: ChannelContext,
    ) -> Option<epics_pva_rs::server_native::source::AccessChecked> {
        if !self.config.allowed(pv_name) {
            return None;
        }
        self.inner.revalidate_read(pv_name, ctx).await
    }
    async fn check_monitor_request(
        &self,
        checked: &epics_pva_rs::server_native::source::AccessChecked,
        ctx: &ChannelContext,
    ) -> Result<(), epics_pva_rs::server_native::source::OpError> {
        self.inner.check_monitor_request(checked, ctx).await
    }
    async fn subscribe_checked_opts(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
        opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> Option<MonitorStream<PvField>> {
        if !self.config.allowed(checked.pv_name()) {
            return None;
        }
        self.inner.subscribe_checked_opts(checked, ctx, opts).await
    }
}

// ── AuditLayer ───────────────────────────────────────────────────

/// Hook fired on every PUT. Ops typically wire this into a file
/// sink (line-based JSON, libca-asLib text, etc.) — the layer
/// stays format-agnostic.
///
/// **Implementation contract**: `record` is called synchronously
/// from inside the `put_value` async path. Any blocking I/O the
/// implementation does (file write, network send) blocks the
/// calling tokio worker thread for the duration of the PUT, so
/// real-world implementations should either:
/// - keep `record` purely in-memory (counter increment, mpsc
///   try_send into a background drain task), or
/// - use the bundled mpsc-buffered wrapper (see
///   `epics_ca_rs::audit::AuditLogger` for the same pattern).
///
/// The default `ClosureAudit` is fine for tests and counters.
pub trait AuditSink: Send + Sync + 'static {
    fn record(&self, event: AuditEvent);
}

/// Default no-op sink — useful when wiring a layer chain in tests.
pub struct NoopAudit;

impl AuditSink for NoopAudit {
    fn record(&self, _event: AuditEvent) {}
}

/// Forward through a shared / type-erased sink. Lets a config carry an
/// `Arc<dyn AuditSink>` (the gateway's audit-sink plumbing) and still
/// satisfy `AuditLayer::new`'s concrete `A: AuditSink` bound. Covers
/// both `Arc<ConcreteSink>` and `Arc<dyn AuditSink>`.
impl<A: AuditSink + ?Sized> AuditSink for Arc<A> {
    fn record(&self, event: AuditEvent) {
        (**self).record(event);
    }
}

/// Boxed-closure audit sink — convenient for inline tests +
/// custom integrations without a dedicated trait impl.
pub struct ClosureAudit<F: Fn(AuditEvent) + Send + Sync + 'static>(pub F);

impl<F: Fn(AuditEvent) + Send + Sync + 'static> AuditSink for ClosureAudit<F> {
    fn record(&self, event: AuditEvent) {
        (self.0)(event);
    }
}

/// Bounded-mpsc adapter that drains audit events on a background
/// task. Use this when the underlying sink does blocking I/O
/// (file write, network send, syslog) — the AuditLayer's
/// `record()` becomes a non-blocking `try_send` that drops on
/// queue overflow rather than stalling the PUT path.
///
/// The drainer task keeps running until both: (a) every clone of
/// this sink has been dropped, and (b) the receiver has drained.
/// Drop order matters in shutdown — drop the gateway / `Arc<Audited>`
/// chain BEFORE waiting on `.flush()` to avoid leaving events
/// in flight.
///
/// Mirrors the pattern in `epics_ca_rs::audit::AuditLogger` but
/// generalised to the gateway's `AuditEvent` shape.
pub struct MpscAuditSink {
    tx: tokio::sync::mpsc::Sender<AuditEvent>,
    /// Counter of events dropped due to a full queue. Read via
    /// [`Self::drops`] for diagnostics. Drops happen when the
    /// blocking sink can't keep up — losing audit events under
    /// sustained overload is strictly better than pinning a
    /// downstream PUT.
    drops: Arc<std::sync::atomic::AtomicU64>,
}

impl MpscAuditSink {
    /// Wrap a blocking sink (anything that impls AuditSink) in a
    /// bounded queue. `capacity` is the max in-flight events; past
    /// that the layer's `record()` becomes a no-op + drop counter
    /// increment. `inner` runs on the spawned drainer task — its
    /// `record()` is allowed to block.
    pub fn wrap<A: AuditSink>(capacity: usize, inner: A) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AuditEvent>(capacity.max(1));
        epics_base_rs::runtime::task::spawn(async move {
            while let Some(ev) = rx.recv().await {
                inner.record(ev);
            }
        });
        Self {
            tx,
            drops: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Number of audit events dropped due to a full queue. Stays
    /// at 0 in normal operation; growing values mean the sink is
    /// slower than the PUT rate and the operator should look at
    /// the underlying I/O stack.
    pub fn drops(&self) -> u64 {
        self.drops.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl AuditSink for MpscAuditSink {
    fn record(&self, event: AuditEvent) {
        if self.tx.try_send(event).is_err() {
            self.drops
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

#[derive(Debug, Clone)]
pub struct AuditEvent {
    /// PV name the operation targeted.
    pub pv: String,
    /// Operation type. Currently only PUT triggers the layer; new
    /// variants will be additive when other op types start
    /// auditing.
    pub event: AuditEventKind,
    /// Authenticated user from the downstream peer's
    /// `ChannelContext`. Empty for `put_value` (non-credentialed)
    /// path.
    pub user: String,
    /// Authenticated host. Same caveat as `user`.
    pub host: String,
    /// Outcome — see [`AuditResult`].
    pub result: AuditResult,
    /// Wall-clock at the moment `record()` was called. Useful for
    /// log shippers that need their own canonical timestamp rather
    /// than the time-of-write.
    pub timestamp: std::time::SystemTime,
    /// Error message body when `result` is `Failed` / `Denied`.
    /// Empty otherwise.
    pub error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditEventKind {
    /// PUT operation — the layer always audits these.
    Put,
    /// GET operation. Audited only when [`AuditLayer::with_get`]
    /// is enabled; defaults off because GET frequency is
    /// typically much higher than PUT.
    Get,
    /// Subscribe / monitor INIT. Logged when an audit-enabled
    /// layer wraps a source whose `subscribe` returns a fresh
    /// receiver. Distinct from individual update events.
    Subscribe,
    /// RPC dispatch.
    Rpc,
    /// PROCESS operation (PVA wire cmd 16) — record-state mutation
    /// without a value payload. WRITE-class for ACF, always audited
    /// alongside PUT.
    Process,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditResult {
    Ok,
    Denied,
    Failed,
}

/// Build an [`AuditEvent`] from the inner-call outcome. The
/// `Denied`-vs-`Failed` bucket is read directly from the typed
/// [`OpErrorKind`] the inner layer set — an access-control refusal
/// ([`AclLayer`] / [`ReadOnlyLayer`] / source-level gateway ACF) is
/// constructed as [`OpErrorKind::Denied`], everything else as
/// [`OpErrorKind::Failed`]. No substring matching of the message:
/// an upstream failure whose text happens to contain "denied" is
/// classified `Failed`, because only a gateway access decision sets
/// the `Denied` kind.
fn make_audit_event(
    name: &str,
    user: &str,
    host: &str,
    result: &Result<(), OpError>,
) -> AuditEvent {
    let (kind, error) = match result {
        Ok(_) => (AuditResult::Ok, String::new()),
        Err(e) => {
            let bucket = match e.kind {
                OpErrorKind::Denied => AuditResult::Denied,
                OpErrorKind::Failed => AuditResult::Failed,
            };
            (bucket, e.message.clone())
        }
    };
    AuditEvent {
        pv: name.to_string(),
        event: AuditEventKind::Put,
        user: user.to_string(),
        host: host.to_string(),
        result: kind,
        timestamp: std::time::SystemTime::now(),
        error,
    }
}

pub struct AuditLayer<A: AuditSink> {
    sink: Arc<A>,
    audit_get: bool,
    audit_subscribe: bool,
    audit_rpc: bool,
}

impl<A: AuditSink> AuditLayer<A> {
    /// New layer that audits PUT only (high-signal events).
    ///
    /// **Note**: `sink.record()` runs synchronously inside the PUT
    /// path. If your sink does blocking I/O (file write, syslog,
    /// HTTP), wrap it with [`MpscAuditSink::wrap`] first or use the
    /// [`AuditLayer::with_blocking_sink`] convenience constructor —
    /// otherwise gateway worker threads stall under sustained load.
    pub fn new(sink: A) -> Self {
        Self {
            sink: Arc::new(sink),
            audit_get: false,
            audit_subscribe: false,
            audit_rpc: false,
        }
    }
}

impl AuditLayer<MpscAuditSink> {
    /// Construct an audit layer where the user-supplied sink may
    /// block (file write, syslog, etc.). Wraps `inner` in an
    /// [`MpscAuditSink`] of `capacity`; the layer's `record()`
    /// becomes a non-blocking try_send and a background drainer task
    /// services the blocking I/O. Audit events past `capacity` are
    /// dropped (counted via [`MpscAuditSink::drops`]).
    pub fn with_blocking_sink<I: AuditSink>(capacity: usize, inner: I) -> Self {
        Self {
            sink: Arc::new(MpscAuditSink::wrap(capacity, inner)),
            audit_get: false,
            audit_subscribe: false,
            audit_rpc: false,
        }
    }
}

impl<A: AuditSink> AuditLayer<A> {
    /// Also emit an audit event on every GET. Off by default
    /// because GET frequency dominates real workloads (a Phoebus
    /// dashboard polls dozens per second).
    pub fn with_get(mut self) -> Self {
        self.audit_get = true;
        self
    }

    /// Also audit subscribe (monitor INIT). One event per
    /// subscriber connect — distinct from per-update events.
    pub fn with_subscribe(mut self) -> Self {
        self.audit_subscribe = true;
        self
    }

    /// Also audit RPC dispatch.
    pub fn with_rpc(mut self) -> Self {
        self.audit_rpc = true;
        self
    }
}

pub struct Audited<S, A> {
    inner: Arc<S>,
    sink: Arc<A>,
    audit_get: bool,
    audit_subscribe: bool,
    audit_rpc: bool,
}

impl<S: ChannelSource, A: AuditSink> Layer<S> for AuditLayer<A> {
    type Wrapped = Audited<S, A>;
    fn layer(self, inner: S) -> Audited<S, A> {
        Audited {
            inner: Arc::new(inner),
            sink: self.sink,
            audit_get: self.audit_get,
            audit_subscribe: self.audit_subscribe,
            audit_rpc: self.audit_rpc,
        }
    }
}

impl<S: ChannelSource, A: AuditSink> ChannelSource for Audited<S, A> {
    fn access(&self) -> &epics_pva_rs::server_native::source::AccessGate {
        self.inner.access()
    }
    async fn list_pvs(&self) -> Vec<String> {
        self.inner.list_pvs().await
    }
    async fn has_pv(&self, name: &str) -> bool {
        self.inner.has_pv(name).await
    }
    async fn get_introspection(&self, name: &str) -> Option<FieldDesc> {
        self.inner.get_introspection(name).await
    }
    // pure pass-through of the credential-aware variants,
    // matching the unaudited `has_pv`/`get_introspection` above
    // (existence/descriptor probes carry no audit row). Without these
    // the trait default would route through this layer's ctx-less
    // `has_pv`/`get_introspection`, dropping the downstream peer's
    // identity before it reaches a gateway inner source.
    async fn has_pv_checked(&self, name: &str, ctx: ChannelContext) -> bool {
        self.inner.has_pv_checked(name, ctx).await
    }
    async fn get_introspection_checked(
        &self,
        name: &str,
        ctx: ChannelContext,
    ) -> Option<FieldDesc> {
        self.inner.get_introspection_checked(name, ctx).await
    }
    async fn get_value(&self, name: &str) -> Option<PvField> {
        let result = self.inner.get_value(name).await;
        if self.audit_get {
            // GET has no error path here — None means "missing" not
            // "denied" — so we model it as Ok with empty error.
            let outcome: Result<(), OpError> = if result.is_some() {
                Ok(())
            } else {
                Err(OpError::failed(format!("PV '{name}' not found")))
            };
            let mut ev = make_audit_event(name, "", "", &outcome);
            ev.event = AuditEventKind::Get;
            self.sink.record(ev);
        }
        result
    }
    async fn put_value(&self, name: &str, value: PvField) -> Result<(), OpError> {
        let result = self.inner.put_value(name, value).await;
        self.sink.record(make_audit_event(name, "", "", &result));
        result
    }
    // typed PUT — emits a credential-aware audit row and
    // forwards through the inner's gate-enforced path.
    async fn put_value_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        value: PvField,
        ctx: ChannelContext,
    ) -> Result<(), OpError> {
        let pv = checked.pv_name().to_string();
        let user = ctx.creds.account.clone();
        let host = ctx.creds.host.clone();
        let result = self.inner.put_value_checked(checked, value, ctx).await;
        self.sink
            .record(make_audit_event(&pv, &user, &host, &result));
        result
    }
    // A BitSet-delta PUT records the same audit row shape as
    // `put_value_checked` (event kind Put, peer credentials) and
    // forwards through the inner's `put_delta_checked`. Without this
    // override the trait default would run get_value + merge +
    // put_value_checked on THIS layer: it would still audit (via the
    // put_value_checked above) but bypass the inner source's atomic
    // `put_delta_checked`, re-opening the concurrent-partial-PUT
    // lost-update window.
    async fn put_delta_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        desc: std::sync::Arc<FieldDesc>,
        changed: epics_pva_rs::proto::BitSet,
        delta: &PvField,
        ctx: ChannelContext,
    ) -> Result<(), OpError> {
        let pv = checked.pv_name().to_string();
        let user = ctx.creds.account.clone();
        let host = ctx.creds.host.clone();
        let result = self
            .inner
            .put_delta_checked(checked, desc, changed, delta, ctx)
            .await;
        self.sink
            .record(make_audit_event(&pv, &user, &host, &result));
        result
    }
    // Atomic PUT_GET writes the record — audit it with the same Put row
    // shape as `put_delta_checked` and forward through the inner's
    // `put_get_checked` so the inner's single-op PUT_GET (the pva-gateway's
    // one-upstream-PUT_GET forward) is preserved. Without this override the
    // trait default decomposes into `put_delta_checked` + `get_value_checked`
    // on THIS layer, bypassing that atomic forward (and recording a Put plus
    // a Get audit row instead of one Put).
    async fn put_get_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        desc: std::sync::Arc<FieldDesc>,
        changed: epics_pva_rs::proto::BitSet,
        delta: &PvField,
        ctx: ChannelContext,
    ) -> Result<Option<SourceRead>, OpError> {
        let pv = checked.pv_name().to_string();
        let user = ctx.creds.account.clone();
        let host = ctx.creds.host.clone();
        let result = self
            .inner
            .put_get_checked(checked, desc, changed, delta, ctx)
            .await;
        // Audit the write outcome (the readback payload is not logged),
        // preserving the Denied/Failed bucket the inner reported.
        let outcome: Result<(), OpError> = result.as_ref().map(|_| ()).map_err(|e| e.clone());
        self.sink
            .record(make_audit_event(&pv, &user, &host, &outcome));
        result
    }
    // PROCESS is a record-state mutation — audit it with the same
    // row shape as PUT (`AuditEventKind::Process`), then forward.
    // Without this override the trait-default `process` would never
    // reach the inner source and no audit row would be emitted.
    async fn process(&self, name: &str) -> Result<(), OpError> {
        let result = self.inner.process(name).await;
        let mut ev = make_audit_event(name, "", "", &result);
        ev.event = AuditEventKind::Process;
        self.sink.record(ev);
        result
    }
    // Typed PROCESS — emits a credential-aware audit row (mirroring
    // `put_value_checked`) and forwards through the inner's
    // gate-enforced `process_checked`.
    async fn process_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
    ) -> Result<(), OpError> {
        let pv = checked.pv_name().to_string();
        let user = ctx.creds.account.clone();
        let host = ctx.creds.host.clone();
        let result = self.inner.process_checked(checked, ctx).await;
        let mut ev = make_audit_event(&pv, &user, &host, &result);
        ev.event = AuditEventKind::Process;
        self.sink.record(ev);
        result
    }
    async fn get_value_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
    ) -> Option<PvField> {
        let pv = checked.pv_name().to_string();
        let user = ctx.creds.account.clone();
        let host = ctx.creds.host.clone();
        let result = self.inner.get_value_checked(checked, ctx).await;
        if self.audit_get {
            // GET returns Option, which cannot distinguish "not found"
            // from "read-denied" — the read gate enforces denial upstream
            // by refusing to mint a readable token, so a None reaching
            // here (after a permitted read) is operationally a miss
            // (Failed), not an access denial.
            let outcome: Result<(), OpError> = if result.is_some() {
                Ok(())
            } else {
                Err(OpError::failed(format!("PV '{pv}' not found")))
            };
            let mut ev = make_audit_event(&pv, &user, &host, &outcome);
            ev.event = AuditEventKind::Get;
            self.sink.record(ev);
        }
        result
    }
    /// The read-FRAMING path (GET reply / PUT_GET readback / monitor seed)
    /// carries the same Get audit row as `get_value_checked` and forwards the
    /// inner's marked leaves. Without this override the trait default would
    /// re-read through this layer's `get_value_checked` — one extra upstream
    /// GET, and the marks dropped (see `ReadOnlyLayer::read_checked`).
    async fn read_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
    ) -> Option<SourceRead> {
        let pv = checked.pv_name().to_string();
        let user = ctx.creds.account.clone();
        let host = ctx.creds.host.clone();
        let result = self.inner.read_checked(checked, ctx).await;
        if self.audit_get {
            let outcome: Result<(), OpError> = if result.is_some() {
                Ok(())
            } else {
                Err(OpError::failed(format!("PV '{pv}' not found")))
            };
            let mut ev = make_audit_event(&pv, &user, &host, &outcome);
            ev.event = AuditEventKind::Get;
            self.sink.record(ev);
        }
        result
    }
    async fn subscribe_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
    ) -> Option<MonitorStream<PvField>> {
        let pv = checked.pv_name().to_string();
        let user = ctx.creds.account.clone();
        let host = ctx.creds.host.clone();
        let result = self.inner.subscribe_checked(checked, ctx).await;
        if self.audit_subscribe {
            let outcome: Result<(), OpError> = if result.is_some() {
                Ok(())
            } else {
                Err(OpError::failed(format!("PV '{pv}' not subscribable")))
            };
            let mut ev = make_audit_event(&pv, &user, &host, &outcome);
            ev.event = AuditEventKind::Subscribe;
            self.sink.record(ev);
        }
        result
    }
    async fn is_writable(&self, name: &str) -> bool {
        self.inner.is_writable(name).await
    }
    async fn subscribe(&self, name: &str) -> Option<MonitorStream<PvField>> {
        let result = self.inner.subscribe(name).await;
        if self.audit_subscribe {
            let outcome: Result<(), OpError> = if result.is_some() {
                Ok(())
            } else {
                Err(OpError::failed(format!("PV '{name}' not subscribable")))
            };
            let mut ev = make_audit_event(name, "", "", &outcome);
            ev.event = AuditEventKind::Subscribe;
            self.sink.record(ev);
        }
        result
    }
    async fn subscribe_raw(&self, name: &str) -> Option<MonitorStream<RawMonitorEvent>> {
        // Audit on subscribe_raw too, since the zero-copy
        // path bypasses the typed `subscribe` and would otherwise
        // miss the audit event entirely.
        let result = self.inner.subscribe_raw(name).await;
        if self.audit_subscribe {
            let outcome: Result<(), OpError> = if result.is_some() {
                Ok(())
            } else {
                Err(OpError::failed(format!(
                    "PV '{name}' not subscribable (raw)"
                )))
            };
            let mut ev = make_audit_event(name, "", "", &outcome);
            ev.event = AuditEventKind::Subscribe;
            self.sink.record(ev);
        }
        result
    }
    // typed raw MONITOR — populate audit row with peer
    // credentials and forward through the inner's gate-enforced
    // path.
    async fn subscribe_raw_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: epics_pva_rs::server_native::source::ChannelContext,
    ) -> Option<MonitorStream<RawMonitorEvent>> {
        let pv = checked.pv_name().to_string();
        let user = ctx.creds.account.clone();
        let host = ctx.creds.host.clone();
        let result = self.inner.subscribe_raw_checked(checked, ctx).await;
        if self.audit_subscribe {
            let outcome: Result<(), OpError> = if result.is_some() {
                Ok(())
            } else {
                Err(OpError::failed(format!("PV '{pv}' not subscribable (raw)")))
            };
            let mut ev = make_audit_event(&pv, &user, &host, &outcome);
            ev.event = AuditEventKind::Subscribe;
            self.sink.record(ev);
        }
        result
    }
    // forward the cooked (marked) event-affecting-options
    // MONITOR — the server's dispatch entry point — recording the same
    // Subscribe audit row as `subscribe_*_checked`. The audit layer is
    // outermost, so it records the attempt even when the inner gateway
    // source rejects an unsupported option.
    async fn subscribe_checked_opts_marked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
        opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> Option<MonitorStream<epics_pva_rs::server_native::MonitorUpdate>> {
        let pv = checked.pv_name().to_string();
        let user = ctx.creds.account.clone();
        let host = ctx.creds.host.clone();
        let result = self
            .inner
            .subscribe_checked_opts_marked(checked, ctx, opts)
            .await;
        if self.audit_subscribe {
            let outcome: Result<(), OpError> = if result.is_some() {
                Ok(())
            } else {
                Err(OpError::failed(format!("PV '{pv}' not subscribable")))
            };
            let mut ev = make_audit_event(&pv, &user, &host, &outcome);
            ev.event = AuditEventKind::Subscribe;
            self.sink.record(ev);
        }
        result
    }
    async fn subscribe_raw_checked_opts(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
        opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> Option<MonitorStream<RawMonitorEvent>> {
        let pv = checked.pv_name().to_string();
        let user = ctx.creds.account.clone();
        let host = ctx.creds.host.clone();
        let result = self
            .inner
            .subscribe_raw_checked_opts(checked, ctx, opts)
            .await;
        if self.audit_subscribe {
            let outcome: Result<(), OpError> = if result.is_some() {
                Ok(())
            } else {
                Err(OpError::failed(format!("PV '{pv}' not subscribable (raw)")))
            };
            let mut ev = make_audit_event(&pv, &user, &host, &outcome);
            ev.event = AuditEventKind::Subscribe;
            self.sink.record(ev);
        }
        result
    }
    // Single-seed MONITOR — the server's dispatch entry point. Record
    // the same Subscribe audit row and forward to the inner so a
    // self-seeding inner (gateway atomic snapshot) supplies the seed.
    // Without this override the server's seeded dispatch would bypass
    // the audit layer entirely.
    async fn subscribe_seeded(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
        opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> Option<
        epics_pva_rs::server_native::source::SubscriptionSeed<
            epics_pva_rs::server_native::source::MonitorUpdate,
        >,
    > {
        let pv = checked.pv_name().to_string();
        let user = ctx.creds.account.clone();
        let host = ctx.creds.host.clone();
        let result = self.inner.subscribe_seeded(checked, ctx, opts).await;
        if self.audit_subscribe {
            let outcome: Result<(), OpError> = if result.is_some() {
                Ok(())
            } else {
                Err(OpError::failed(format!("PV '{pv}' not subscribable")))
            };
            let mut ev = make_audit_event(&pv, &user, &host, &outcome);
            ev.event = AuditEventKind::Subscribe;
            self.sink.record(ev);
        }
        result
    }
    async fn subscribe_raw_seeded(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
        opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> Option<epics_pva_rs::server_native::source::SubscriptionSeed<RawMonitorEvent>> {
        let pv = checked.pv_name().to_string();
        let user = ctx.creds.account.clone();
        let host = ctx.creds.host.clone();
        let result = self.inner.subscribe_raw_seeded(checked, ctx, opts).await;
        if self.audit_subscribe {
            let outcome: Result<(), OpError> = if result.is_some() {
                Ok(())
            } else {
                Err(OpError::failed(format!("PV '{pv}' not subscribable (raw)")))
            };
            let mut ev = make_audit_event(&pv, &user, &host, &outcome);
            ev.event = AuditEventKind::Subscribe;
            self.sink.record(ev);
        }
        result
    }
    async fn rpc(
        &self,
        name: &str,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> Result<RpcReply, OpError> {
        let result = self.inner.rpc(name, request_desc, request_value).await;
        if self.audit_rpc {
            let outcome: Result<(), OpError> = match &result {
                Ok(_) => Ok(()),
                Err(e) => Err(e.clone()),
            };
            let mut ev = make_audit_event(name, "", "", &outcome);
            ev.event = AuditEventKind::Rpc;
            self.sink.record(ev);
        }
        result
    }
    // typed RPC — populate audit row with peer
    // credentials and forward through the inner's gate-enforced
    // path.
    async fn rpc_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        request_desc: FieldDesc,
        request_value: PvField,
        ctx: epics_pva_rs::server_native::source::ChannelContext,
    ) -> Result<RpcReply, OpError> {
        let pv = checked.pv_name().to_string();
        let user = ctx.creds.account.clone();
        let host = ctx.creds.host.clone();
        let result = self
            .inner
            .rpc_checked(checked, request_desc, request_value, ctx)
            .await;
        if self.audit_rpc {
            let outcome: Result<(), OpError> = match &result {
                Ok(_) => Ok(()),
                Err(e) => Err(e.clone()),
            };
            let mut ev = make_audit_event(&pv, &user, &host, &outcome);
            ev.event = AuditEventKind::Rpc;
            self.sink.record(ev);
        }
        result
    }
    // ChannelArray: forward every sub-op through the inner (so a wrapped
    // gateway source's array support survives the audit layer — the
    // wrapper-severs-override defect family) and emit an audit row matching
    // the sub-op's class. The INIT descriptor probe carries no audit row
    // (like `has_pv`/`get_introspection`). putArray/setLength are WRITE-class
    // → `AuditEventKind::Put` (always audited, alongside PUT/PROCESS).
    // getArray/getLength are READ-class → `AuditEventKind::Get` (gated by
    // `audit_get`, like GET/MONITOR). There is no dedicated Array audit kind;
    // the Put/Get buckets capture the operation's mutate-vs-read intent,
    // which is what an audit log records.
    async fn channel_array_init(
        &self,
        name: &str,
        ctx: ChannelContext,
    ) -> Result<FieldDesc, OpError> {
        self.inner.channel_array_init(name, ctx).await
    }
    async fn channel_array_get(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        offset: u32,
        count: u32,
        stride: u32,
        ctx: ChannelContext,
    ) -> Result<PvField, OpError> {
        let pv = checked.pv_name().to_string();
        let user = ctx.creds.account.clone();
        let host = ctx.creds.host.clone();
        let result = self
            .inner
            .channel_array_get(checked, offset, count, stride, ctx)
            .await;
        if self.audit_get {
            let outcome: Result<(), OpError> = result.as_ref().map(|_| ()).map_err(|e| e.clone());
            let mut ev = make_audit_event(&pv, &user, &host, &outcome);
            ev.event = AuditEventKind::Get;
            self.sink.record(ev);
        }
        result
    }
    async fn channel_array_get_length(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
    ) -> Result<u32, OpError> {
        let pv = checked.pv_name().to_string();
        let user = ctx.creds.account.clone();
        let host = ctx.creds.host.clone();
        let result = self.inner.channel_array_get_length(checked, ctx).await;
        if self.audit_get {
            let outcome: Result<(), OpError> = result.as_ref().map(|_| ()).map_err(|e| e.clone());
            let mut ev = make_audit_event(&pv, &user, &host, &outcome);
            ev.event = AuditEventKind::Get;
            self.sink.record(ev);
        }
        result
    }
    async fn channel_array_put(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        offset: u32,
        stride: u32,
        value: PvField,
        ctx: ChannelContext,
    ) -> Result<(), OpError> {
        let pv = checked.pv_name().to_string();
        let user = ctx.creds.account.clone();
        let host = ctx.creds.host.clone();
        let result = self
            .inner
            .channel_array_put(checked, offset, stride, value, ctx)
            .await;
        let mut ev = make_audit_event(&pv, &user, &host, &result);
        ev.event = AuditEventKind::Put;
        self.sink.record(ev);
        result
    }
    async fn channel_array_set_length(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        length: u32,
        ctx: ChannelContext,
    ) -> Result<(), OpError> {
        let pv = checked.pv_name().to_string();
        let user = ctx.creds.account.clone();
        let host = ctx.creds.host.clone();
        let result = self
            .inner
            .channel_array_set_length(checked, length, ctx)
            .await;
        let mut ev = make_audit_event(&pv, &user, &host, &result);
        ev.event = AuditEventKind::Put;
        self.sink.record(ev);
        result
    }
    // forward the per-PV watermark levels so the inner
    // gateway source's `monitor_watermarks` override is reachable
    // through the wrapper stack. Without this the server's monitor loop
    // sees the trait default `None` and never fires the pause/resume
    // callbacks — the same wrapper-severs-override defect family as
    // FR-8's `has_pv_checked`/`get_introspection_checked`.
    async fn monitor_watermarks(&self, name: &str) -> Option<(usize, usize)> {
        self.inner.monitor_watermarks(name).await
    }
    fn notify_watermark(&self, name: &str, ctx: &ChannelContext, ev: WatermarkEvent) {
        self.inner.notify_watermark(name, ctx, ev);
    }
    // same wrapper-severs-override defect family as
    // `notify_watermark`/`monitor_watermarks` above — a transparent
    // middleware layer that forwards one notify_* sibling but not the
    // other would sever the inner source's monitor-start (onStart)
    // callback (e.g. a wrapped `SharedPV::set_on_start`). Forward the
    // Idle↔Executing edge unchanged.
    fn notify_monitor_start(&self, name: &str, ctx: &ChannelContext, start: bool) {
        self.inner.notify_monitor_start(name, ctx, start);
    }
    // Same wrapper-severs-override defect family as the `notify_*` and
    // `*_checked` forwards above: a transparent middleware layer must
    // delegate every descriptive / advertisement / channel-lifecycle
    // method whose inner override the trait default would otherwise mask.
    // All of these are queried or fired on the BOUND OWNER, which for a
    // middleware wrapper is the wrapper itself (`resolve_owner` keeps the
    // default `None` so every op keeps routing through this layer).
    // Forwarding them preserves a wrapped source's SEARCH advertisement
    // (`searchable`/`searchable_from`, e.g. a ServerInfoSource that hides
    // from UDP discovery), its registry beacon-change signal, its
    // per-channel report info (captured once at admission and surfaced in
    // the server report), and its channel open/close lifecycle callbacks
    // (e.g. a SharedPV acquiring/releasing a per-channel lease). These are
    // transparent pass-throughs, not audit events, so they are forwarded
    // verbatim like the `notify_*` siblings. `resolve_owner` is
    // deliberately NOT forwarded — doing so would bind the inner as the
    // channel owner and bypass this layer entirely.
    fn beacon_change(&self) -> u64 {
        self.inner.beacon_change()
    }
    async fn searchable(&self, name: &str) -> bool {
        self.inner.searchable(name).await
    }
    async fn searchable_from(&self, name: &str, requester: SocketAddr) -> bool {
        self.inner.searchable_from(name, requester).await
    }
    async fn channel_report_info(&self, name: &str, ctx: ChannelContext) -> Option<String> {
        self.inner.channel_report_info(name, ctx).await
    }
    fn notify_channel_open(&self, name: &str, ctx: &ChannelContext) {
        self.inner.notify_channel_open(name, ctx);
    }
    fn notify_channel_close(&self, name: &str, ctx: &ChannelContext) {
        self.inner.notify_channel_close(name, ctx);
    }
    // R17-33 (see `ReadOnly`): forward every method whose inner override
    // the trait default would mask; the subscribe path keeps recording its
    // audit row, so forwarding never loses an event.
    fn set_channel_invalidator(
        &self,
        invalidator: epics_pva_rs::server_native::source::ChannelInvalidator,
    ) {
        self.inner.set_channel_invalidator(invalidator);
    }
    async fn revalidate_read(
        &self,
        pv_name: &str,
        ctx: ChannelContext,
    ) -> Option<epics_pva_rs::server_native::source::AccessChecked> {
        self.inner.revalidate_read(pv_name, ctx).await
    }
    async fn check_monitor_request(
        &self,
        checked: &epics_pva_rs::server_native::source::AccessChecked,
        ctx: &ChannelContext,
    ) -> Result<(), epics_pva_rs::server_native::source::OpError> {
        self.inner.check_monitor_request(checked, ctx).await
    }
    async fn subscribe_checked_opts(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
        opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> Option<MonitorStream<PvField>> {
        let pv = checked.pv_name().to_string();
        let user = ctx.creds.account.clone();
        let host = ctx.creds.host.clone();
        let result = self.inner.subscribe_checked_opts(checked, ctx, opts).await;
        if self.audit_subscribe {
            let outcome: Result<(), OpError> = if result.is_some() {
                Ok(())
            } else {
                Err(OpError::failed(format!("PV '{pv}' not subscribable")))
            };
            let mut ev = make_audit_event(&pv, &user, &host, &outcome);
            ev.event = AuditEventKind::Subscribe;
            self.sink.record(ev);
        }
        result
    }
}

/// Wrap a gateway proxy source in the canonical access-control chain
/// `Audit( ReadOnly?( Acl( source ) ) )` and type-erase it to a
/// [`DynSource`].
///
/// This is the single owner of the chain *shape*, so every gateway
/// topology that registers a proxy source through a `CompositeSource`
/// enforces ACL / read-only / audit identically:
///
/// - `Acl` is innermost so a denied PV name short-circuits before the
///   call reaches the proxy (no upstream search for a denied PV).
/// - `ReadOnly` (only when `read_only`) sits above `Acl` so it rejects
///   every PUT regardless of upstream policy.
/// - `Audit` is outermost so it records the *final* outcome, including
///   ACL / read-only denials, not just PUTs that reached the upstream.
///
/// `Acl` and `Audit` are always present (a permissive [`AclConfig`] /
/// [`NoopAudit`] when not configured) so the chain shape is uniform;
/// only `read_only` is a genuine branch. The single-tenant
/// [`super::PvaGateway`] builds the same chain inline because its
/// non-composite path threads the concrete layered type into
/// `PvaServer::start::<S>` (which requires `S: ChannelSource`, a bound
/// the type-erased `DynSource` does not satisfy); this helper serves
/// the composite-only topologies such as the multi-tenant gateway.
pub(crate) fn layer_access_control<S>(
    source: S,
    acl: AclConfig,
    read_only: bool,
    audit: Arc<dyn AuditSink>,
) -> DynSource
where
    S: ChannelSource + 'static,
{
    let acl_layer = AclLayer::new(acl).layer(source);
    if read_only {
        Arc::new(AuditLayer::new(audit).layer(ReadOnlyLayer.layer(acl_layer))) as DynSource
    } else {
        Arc::new(AuditLayer::new(audit).layer(acl_layer)) as DynSource
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_matching() {
        assert!(matches_pattern("MOTOR:*", "MOTOR:VAL"));
        assert!(matches_pattern("*VAL", "MOTOR:VAL"));
        assert!(matches_pattern("EXACT", "EXACT"));
        assert!(!matches_pattern("EXACT", "EXACT2"));
        assert!(!matches_pattern("MOTOR:*", "OTHER:VAL"));
    }

    #[test]
    fn pattern_matching_interior_wildcard() {
        // an interior `*` must behave as a wildcard, not a
        // literal. Previously `MOTOR:*:JOG` matched only the literal
        // string `MOTOR:*:JOG`.
        assert!(matches_pattern("MOTOR:*:JOG", "MOTOR:X:JOG"));
        assert!(matches_pattern("MOTOR:*:JOG", "MOTOR:AXIS1:JOG"));
        // `*` matches the empty run too.
        assert!(matches_pattern("MOTOR:*:JOG", "MOTOR::JOG"));
        // Trailing literal must still be present and pinned to the end.
        assert!(!matches_pattern("MOTOR:*:JOG", "MOTOR:X:VAL"));
        assert!(!matches_pattern("MOTOR:*:JOG", "MOTOR:X:JOGGER"));
        // The leading literal stays anchored to the start.
        assert!(!matches_pattern("MOTOR:*:JOG", "PUMP:X:JOG"));
        // Multiple `*`.
        assert!(matches_pattern("A*B*C", "AxxByyC"));
        assert!(matches_pattern("A*B*C", "ABC"));
        assert!(!matches_pattern("A*B*C", "AxxByy"));
        // The literal `*`-as-text meaning is gone: a pattern with `*`
        // no longer matches a name that literally contains `*` unless
        // the glob does.
        assert!(!matches_pattern("MOTOR:*:JOG", "MOTOR:*:JOGX"));
    }

    /// an interior-wildcard glob now works directly in the
    /// allow/deny lists, without needing the `deny_regex` workaround.
    #[test]
    fn acl_interior_glob_in_allow_and_deny() {
        let cfg = AclConfig {
            allow_only: vec!["MOTOR:*".into()],
            deny: vec!["MOTOR:*:JOG".into()],
            ..Default::default()
        };
        assert!(cfg.allowed("MOTOR:X:VAL"));
        // Interior-glob deny now actually fires (was a literal before).
        assert!(!cfg.allowed("MOTOR:X:JOG"));
        assert!(!cfg.allowed("MOTOR:AXIS1:JOG"));
    }

    #[test]
    fn acl_allow_only() {
        let cfg = AclConfig {
            allow_only: vec!["BL10C:*".into()],
            ..Default::default()
        };
        assert!(cfg.allowed("BL10C:VG-01:PRESSURE"));
        assert!(!cfg.allowed("RFP:HV"));
    }

    #[test]
    fn acl_deny_overrides_allow() {
        let cfg = AclConfig {
            allow_only: vec!["MOTOR:*".into()],
            deny: vec!["MOTOR:JOG:*".into()],
            ..Default::default()
        };
        assert!(cfg.allowed("MOTOR:VAL"));
        assert!(!cfg.allowed("MOTOR:JOG:UP"));
        assert!(!cfg.allowed("OTHER:PV"));
    }

    // ── B7: regex ACL ────────────────────────────────────────────

    /// A regex deny entry is anchored at both ends, so `BL10C:.*`
    /// matches the same names a `BL10C:*` glob would.
    #[test]
    fn acl_regex_deny_anchored() {
        let cfg = AclConfig::default().deny_regex(r"BL10C:.*:HV").unwrap();
        assert!(!cfg.allowed("BL10C:RFP:HV"));
        assert!(!cfg.allowed("BL10C::HV"));
        // Anchored: the regex must match the WHOLE name.
        assert!(cfg.allowed("X:BL10C:RFP:HV"));
        assert!(cfg.allowed("BL10C:RFP:HV:SETPOINT"));
        assert!(cfg.allowed("BL10D:RFP:HV"));
    }

    /// A regex allow-only entry flips the policy to default-deny,
    /// exactly like a glob allow-only entry.
    #[test]
    fn acl_regex_allow_only_default_denies() {
        let cfg = AclConfig::default().allow_regex(r"(SR|BL)\d+:.*").unwrap();
        assert!(cfg.allowed("SR01:CURRENT"));
        assert!(cfg.allowed("BL10:SHUTTER"));
        assert!(!cfg.allowed("RFP:HV"));
        // Alternation stays anchored as ^(?:(SR|BL)\d+:.*)$ — a name
        // that merely contains a branch must not slip through.
        assert!(!cfg.allowed("X-SR01:CURRENT"));
    }

    /// Glob and regex patterns may be mixed in the same config;
    /// deny (glob or regex) always wins over allow (glob or regex).
    #[test]
    fn acl_glob_and_regex_mixed() {
        let cfg = AclConfig {
            allow_only: vec!["MOTOR:*".into()],
            ..Default::default()
        }
        .allow_regex(r"TEMP:\d+")
        .unwrap()
        .deny_regex(r"MOTOR:.*:JOG")
        .unwrap();
        // Glob allow.
        assert!(cfg.allowed("MOTOR:X:VAL"));
        // Regex allow.
        assert!(cfg.allowed("TEMP:42"));
        assert!(!cfg.allowed("TEMP:hot")); // \d+ requires digits
        // Regex deny beats glob allow.
        assert!(!cfg.allowed("MOTOR:X:JOG"));
        // Not in any allow list → default-deny.
        assert!(!cfg.allowed("RFP:HV"));
    }

    /// An invalid regex is reported at config-build time, not on the
    /// first PV check.
    #[test]
    fn acl_invalid_regex_rejected_at_build() {
        assert!(AclConfig::default().deny_regex(r"BL10C:[").is_err());
        assert!(AclConfig::default().allow_regex(r"(unclosed").is_err());
    }

    /// B7: a pathological operator-supplied ACL regex whose compiled
    /// program exceeds `ACL_REGEX_SIZE_LIMIT` must fail fast at build
    /// time with a bounded `regex::Error` rather than compiling with
    /// the crate's larger defaults. A bounded counted repetition of a
    /// large bounded inner repetition blows the program size well
    /// past 256 KiB while staying a syntactically valid pattern.
    #[test]
    fn acl_oversized_regex_rejected_by_size_limit() {
        // `(?:a{1000}){1000}` — syntactically valid, but the compiled
        // program is far larger than the 256 KiB ACL limit.
        let pathological = r"(?:a{1000}){1000}";

        // `compile_anchored` is the single funnel both `deny_regex`
        // and `allow_regex` route through — test it directly so the
        // failing error variant is observable (`AclConfig` is not
        // `Debug`, so `expect_err` on the builder is unavailable).
        match compile_anchored(pathological) {
            Err(regex::Error::CompiledTooBig(_)) => {}
            other => panic!("expected CompiledTooBig, got {other:?}"),
        }
        // Both ACL builder entry points reject it.
        assert!(
            AclConfig::default().deny_regex(pathological).is_err(),
            "oversized deny regex must be rejected"
        );
        assert!(
            AclConfig::default().allow_regex(pathological).is_err(),
            "oversized allow regex must be rejected"
        );

        // A realistic ACL pattern still compiles fine under the limit.
        assert!(compile_anchored(r"BL\d+C:.*:HV").is_ok());
        assert!(AclConfig::default().deny_regex(r"BL\d+C:.*:HV").is_ok());
    }

    /// The regex ACL still gates through the `AclLayer` wrapper on a
    /// real `ChannelSource`, not just the bare `AclConfig::allowed`.
    #[tokio::test]
    async fn acl_layer_applies_regex_via_channel_source() {
        use super::super::channel_cache::{ChannelCache, DEFAULT_CLEANUP_INTERVAL};
        use super::super::source::GatewayChannelSource;
        use epics_pva_rs::client::PvaClient;

        let client = Arc::new(PvaClient::builder().build());
        let cache = ChannelCache::new(client, DEFAULT_CLEANUP_INTERVAL);
        let inner = GatewayChannelSource::new(cache);

        let cfg = AclConfig::default().deny_regex(r"SECRET:.*").unwrap();
        let acl = AclLayer::new(cfg).layer(inner);

        // Denied name short-circuits at the layer — has_pv is false
        // and no upstream search is triggered.
        assert!(!acl.has_pv("SECRET:KEY").await);
        // put_value on a denied name returns the ACL error without
        // reaching the upstream.
        let err = acl
            .put_value(
                "SECRET:KEY",
                PvField::Scalar(epics_pva_rs::pvdata::ScalarValue::Double(0.0)),
            )
            .await
            .expect_err("regex-denied PUT must fail at the ACL layer");
        assert_eq!(
            err.kind,
            OpErrorKind::Denied,
            "ACL denial must classify as Denied: {err:?}"
        );
        assert!(err.message.contains("denied"));
    }

    /// A denied PV must not be SEARCH-advertised, and the ACL layer must
    /// enforce that ITSELF rather than inherit it from a caller.
    ///
    /// `CompositeSource` asks `searchable` alone — it no longer ANDs
    /// `has_pv(name) && searchable(name)`, because pvxs's `onSearch` and
    /// `onCreate` are independent and a source may legitimately advertise a
    /// name it refuses at create (a `DBF_NOACCESS` field). Under the old
    /// conjunction this layer's deny reached SEARCH only as a side effect of
    /// `has_pv` being consulted, so forwarding `searchable` bare to the inner
    /// looked correct. It was safe by accident: an inner that advertises the
    /// name (below) would have leaked it the moment the caller stopped ANDing.
    /// `list_pvs` is already filtered here so introspection cannot "leak the
    /// names of denied PVs"; SEARCH is the same disclosure, so it is gated at
    /// the same place.
    #[tokio::test]
    async fn acl_denied_pv_is_not_search_advertised() {
        /// Advertises every name — so only the ACL's own gate can deny.
        struct AdvertisesEverything;
        impl ChannelSource for AdvertisesEverything {
            async fn list_pvs(&self) -> Vec<String> {
                vec!["SECRET:KEY".into(), "PUBLIC:PV".into()]
            }
            async fn has_pv(&self, _name: &str) -> bool {
                true
            }
            async fn searchable(&self, _name: &str) -> bool {
                true
            }
            async fn searchable_from(&self, _name: &str, _requester: SocketAddr) -> bool {
                true
            }
            async fn get_introspection(&self, _name: &str) -> Option<FieldDesc> {
                Some(FieldDesc::Scalar(ScalarType::Double))
            }
            async fn get_value(&self, _name: &str) -> Option<PvField> {
                Some(PvField::Scalar(ScalarValue::Double(0.0)))
            }
            async fn put_value(&self, _name: &str, _value: PvField) -> Result<(), OpError> {
                Ok(())
            }
            async fn is_writable(&self, _name: &str) -> bool {
                true
            }
            async fn subscribe(&self, _name: &str) -> Option<MonitorStream<PvField>> {
                None
            }
        }

        let requester: SocketAddr = "10.0.0.5:5076".parse().unwrap();
        let cfg = AclConfig::default().deny_regex(r"SECRET:.*").unwrap();
        let acl = AclLayer::new(cfg).layer(AdvertisesEverything);

        assert!(
            !acl.searchable("SECRET:KEY").await,
            "a denied PV must not be UDP-search-advertised, even when the \
             inner source advertises it"
        );
        assert!(
            !acl.searchable_from("SECRET:KEY", requester).await,
            "a denied PV must not be TCP-circuit search-advertised either"
        );
        assert!(
            !acl.list_pvs().await.contains(&"SECRET:KEY".to_string()),
            "the sibling advertisement surface must stay filtered"
        );

        // An allowed name still reaches the inner's own answer — the gate
        // must deny, not blanket-suppress.
        assert!(acl.searchable("PUBLIC:PV").await);
        assert!(acl.searchable_from("PUBLIC:PV", requester).await);
    }

    /// Audit-event classifier tags ACL/read-only error messages
    /// as Denied; other errors as Failed; Ok results as Ok.
    #[test]
    fn audit_event_classifies_results() {
        let denied = make_audit_event(
            "MOTOR:VAL",
            "alice",
            "host1",
            &Err(OpError::denied("ACL: PV 'MOTOR:VAL' denied")),
        );
        assert_eq!(denied.result, AuditResult::Denied);
        assert!(!denied.error.is_empty());

        let read_only = make_audit_event(
            "MOTOR:VAL",
            "",
            "",
            &Err(OpError::denied("read-only mode: PUT rejected")),
        );
        assert_eq!(read_only.result, AuditResult::Denied);

        let failed = make_audit_event(
            "MOTOR:VAL",
            "alice",
            "host1",
            &Err("upstream timeout".into()),
        );
        assert_eq!(failed.result, AuditResult::Failed);

        let ok = make_audit_event("MOTOR:VAL", "alice", "host1", &Ok(()));
        assert_eq!(ok.result, AuditResult::Ok);
        assert!(ok.error.is_empty());
    }

    // ── put_delta_checked forwarding through the wrappers ────────────

    use epics_pva_rs::pvdata::{ScalarType, ScalarValue};
    use epics_pva_rs::server_native::source::{AccessChecked, AccessGate};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Minimal `ChannelSource` stub that records which PUT method
    /// the wrapper routed a delta PUT to. `put_delta_checked` is
    /// overridden (atomic merge stand-in); the default
    /// `put_value_checked` chains through it. If a wrapper bypasses
    /// `put_delta_checked` and runs the non-atomic default merge, it
    /// lands in `put_value_checked` and `delta_reached` stays false.
    struct RecordingSource {
        delta_reached: Arc<AtomicBool>,
        value_reached: Arc<AtomicBool>,
        process_reached: Arc<AtomicBool>,
    }

    impl ChannelSource for RecordingSource {
        async fn list_pvs(&self) -> Vec<String> {
            vec!["X".into()]
        }
        async fn has_pv(&self, _name: &str) -> bool {
            true
        }
        async fn get_introspection(&self, _name: &str) -> Option<FieldDesc> {
            Some(FieldDesc::Scalar(ScalarType::Double))
        }
        async fn get_value(&self, _name: &str) -> Option<PvField> {
            Some(PvField::Scalar(ScalarValue::Double(0.0)))
        }
        async fn put_value(&self, _name: &str, _value: PvField) -> Result<(), OpError> {
            Ok(())
        }
        async fn put_value_checked(
            &self,
            _checked: AccessChecked,
            _value: PvField,
            _ctx: ChannelContext,
        ) -> Result<(), OpError> {
            self.value_reached.store(true, Ordering::SeqCst);
            Ok(())
        }
        async fn put_delta_checked(
            &self,
            _checked: AccessChecked,
            _desc: std::sync::Arc<FieldDesc>,
            _changed: epics_pva_rs::proto::BitSet,
            _delta: &PvField,
            _ctx: ChannelContext,
        ) -> Result<(), OpError> {
            self.delta_reached.store(true, Ordering::SeqCst);
            Ok(())
        }
        // Inner PROCESS sink. The trait-default `process_checked`
        // applies the `allows_write()` gate then delegates here, so a
        // wrapper that correctly forwards `process_checked` lands in
        // this method and flips `process_reached`.
        async fn process(&self, _name: &str) -> Result<(), OpError> {
            self.process_reached.store(true, Ordering::SeqCst);
            Ok(())
        }
        async fn is_writable(&self, _name: &str) -> bool {
            true
        }
        async fn subscribe(&self, _name: &str) -> Option<MonitorStream<PvField>> {
            None
        }
    }

    fn test_ctx() -> ChannelContext {
        ChannelContext {
            peer: "127.0.0.1:5075".parse().unwrap(),
            creds: std::sync::Arc::new(epics_pva_rs::server_native::config::ClientCredentials {
                account: "alice".into(),
                method: "anonymous".into(),
                host: "host1".into(),
                authority: String::new(),
                roles: Vec::new(),
            }),
            pv_request: None,
            log: Default::default(),
        }
    }

    async fn checked_for(name: &str) -> AccessChecked {
        AccessGate::open()
            .check(name, "host1", "alice", "anonymous", "")
            .await
    }

    /// A delta PUT through the `Acl` wrapper must reach the inner
    /// source's `put_delta_checked` — not the non-atomic
    /// get+put_value_checked default merge.
    #[tokio::test]
    async fn acl_forwards_put_delta_checked_to_inner() {
        let delta_reached = Arc::new(AtomicBool::new(false));
        let value_reached = Arc::new(AtomicBool::new(false));
        let inner = RecordingSource {
            delta_reached: delta_reached.clone(),
            value_reached: value_reached.clone(),
            process_reached: Arc::new(AtomicBool::new(false)),
        };
        let acl = AclLayer::new(AclConfig::default()).layer(inner);

        let mut changed = epics_pva_rs::proto::BitSet::new();
        changed.set(0);
        let res = acl
            .put_delta_checked(
                checked_for("X").await,
                std::sync::Arc::new(FieldDesc::Scalar(ScalarType::Double)),
                changed,
                &PvField::Scalar(ScalarValue::Double(1.0)),
                test_ctx(),
            )
            .await;
        assert!(res.is_ok());
        assert!(
            delta_reached.load(Ordering::SeqCst),
            "Acl must route delta PUT to inner put_delta_checked"
        );
        assert!(
            !value_reached.load(Ordering::SeqCst),
            "Acl must NOT fall back to the non-atomic put_value_checked merge"
        );
    }

    /// An `Acl`-denied delta PUT must be rejected at the layer and
    /// never reach the inner source at all.
    #[tokio::test]
    async fn acl_denied_put_delta_checked_short_circuits() {
        let delta_reached = Arc::new(AtomicBool::new(false));
        let value_reached = Arc::new(AtomicBool::new(false));
        let inner = RecordingSource {
            delta_reached: delta_reached.clone(),
            value_reached: value_reached.clone(),
            process_reached: Arc::new(AtomicBool::new(false)),
        };
        let cfg = AclConfig::default().deny_regex(r"SECRET:.*").unwrap();
        let acl = AclLayer::new(cfg).layer(inner);

        let mut changed = epics_pva_rs::proto::BitSet::new();
        changed.set(0);
        let err = acl
            .put_delta_checked(
                checked_for("SECRET:KEY").await,
                std::sync::Arc::new(FieldDesc::Scalar(ScalarType::Double)),
                changed,
                &PvField::Scalar(ScalarValue::Double(1.0)),
                test_ctx(),
            )
            .await
            .expect_err("ACL-denied delta PUT must fail at the layer");
        assert_eq!(
            err.kind,
            OpErrorKind::Denied,
            "ACL denial must classify as Denied: {err:?}"
        );
        assert!(err.message.contains("denied"));
        assert!(!delta_reached.load(Ordering::SeqCst));
        assert!(!value_reached.load(Ordering::SeqCst));
    }

    /// A delta PUT through the `Audited` wrapper must reach the
    /// inner's `put_delta_checked` and emit one `Put` audit row
    /// carrying the peer credentials.
    #[tokio::test]
    async fn audited_forwards_put_delta_checked_and_records() {
        let delta_reached = Arc::new(AtomicBool::new(false));
        let value_reached = Arc::new(AtomicBool::new(false));
        let inner = RecordingSource {
            delta_reached: delta_reached.clone(),
            value_reached: value_reached.clone(),
            process_reached: Arc::new(AtomicBool::new(false)),
        };
        let events: Arc<std::sync::Mutex<Vec<AuditEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_sink = events.clone();
        let audited = AuditLayer::new(ClosureAudit(move |ev| {
            events_sink.lock().unwrap().push(ev);
        }))
        .layer(inner);

        let mut changed = epics_pva_rs::proto::BitSet::new();
        changed.set(0);
        let res = audited
            .put_delta_checked(
                checked_for("X").await,
                std::sync::Arc::new(FieldDesc::Scalar(ScalarType::Double)),
                changed,
                &PvField::Scalar(ScalarValue::Double(1.0)),
                test_ctx(),
            )
            .await;
        assert!(res.is_ok());
        assert!(
            delta_reached.load(Ordering::SeqCst),
            "Audited must route delta PUT to inner put_delta_checked"
        );
        assert!(
            !value_reached.load(Ordering::SeqCst),
            "Audited must NOT fall back to the non-atomic put_value_checked merge"
        );
        let recorded = events.lock().unwrap();
        assert_eq!(recorded.len(), 1, "exactly one audit row per delta PUT");
        assert_eq!(recorded[0].event, AuditEventKind::Put);
        assert_eq!(recorded[0].result, AuditResult::Ok);
        assert_eq!(recorded[0].pv, "X");
        assert_eq!(recorded[0].user, "alice");
        assert_eq!(recorded[0].host, "host1");
    }

    /// A delta PUT through the `ReadOnly` wrapper must be rejected
    /// at the layer and never reach the inner source.
    #[tokio::test]
    async fn read_only_rejects_put_delta_checked() {
        let delta_reached = Arc::new(AtomicBool::new(false));
        let value_reached = Arc::new(AtomicBool::new(false));
        let inner = RecordingSource {
            delta_reached: delta_reached.clone(),
            value_reached: value_reached.clone(),
            process_reached: Arc::new(AtomicBool::new(false)),
        };
        let ro = ReadOnlyLayer.layer(inner);

        let mut changed = epics_pva_rs::proto::BitSet::new();
        changed.set(0);
        let err = ro
            .put_delta_checked(
                checked_for("X").await,
                std::sync::Arc::new(FieldDesc::Scalar(ScalarType::Double)),
                changed,
                &PvField::Scalar(ScalarValue::Double(1.0)),
                test_ctx(),
            )
            .await
            .expect_err("read-only delta PUT must be rejected");
        assert_eq!(
            err.kind,
            OpErrorKind::Denied,
            "read-only refusal must classify as Denied: {err:?}"
        );
        assert!(err.message.contains("read-only"));
        assert!(!delta_reached.load(Ordering::SeqCst));
        assert!(!value_reached.load(Ordering::SeqCst));
    }

    // ── credentialed existence / introspection forwarding ──

    /// Records whether the credential-free (`has_pv`/`get_introspection`)
    /// or the credentialed (`*_checked`) path was reached, plus the
    /// account the checked path observed. A wrapper that omits the
    /// `_checked` forwarders lets the trait default delegate to its own
    /// ctx-less `has_pv`/`get_introspection`, so `plain_*` flips and the
    /// recorded account stays `None`.
    struct ExistenceRecordingSource {
        plain_has_pv: Arc<AtomicBool>,
        plain_intro: Arc<AtomicBool>,
        checked_has_pv_account: Arc<std::sync::Mutex<Option<String>>>,
        checked_intro_account: Arc<std::sync::Mutex<Option<String>>>,
    }
    impl ChannelSource for ExistenceRecordingSource {
        async fn list_pvs(&self) -> Vec<String> {
            vec!["X".into()]
        }
        async fn has_pv(&self, _name: &str) -> bool {
            self.plain_has_pv.store(true, Ordering::SeqCst);
            true
        }
        async fn has_pv_checked(&self, _name: &str, ctx: ChannelContext) -> bool {
            *self.checked_has_pv_account.lock().unwrap() = Some(ctx.creds.account.clone());
            true
        }
        async fn get_introspection(&self, _name: &str) -> Option<FieldDesc> {
            self.plain_intro.store(true, Ordering::SeqCst);
            Some(FieldDesc::Scalar(ScalarType::Double))
        }
        async fn get_introspection_checked(
            &self,
            _name: &str,
            ctx: ChannelContext,
        ) -> Option<FieldDesc> {
            *self.checked_intro_account.lock().unwrap() = Some(ctx.creds.account.clone());
            Some(FieldDesc::Scalar(ScalarType::Double))
        }
        async fn get_value(&self, _name: &str) -> Option<PvField> {
            Some(PvField::Scalar(ScalarValue::Double(0.0)))
        }
        async fn put_value(&self, _name: &str, _value: PvField) -> Result<(), OpError> {
            Ok(())
        }
        async fn is_writable(&self, _name: &str) -> bool {
            true
        }
        async fn subscribe(&self, _name: &str) -> Option<MonitorStream<PvField>> {
            None
        }
    }

    /// The full production wrapper stack `Audited<ReadOnly<Acl<S>>>`
    /// (gateway.rs `Audit( ReadOnly?( Acl( source ) ) )`) must forward
    /// BOTH credentialed existence/introspection variants down to the
    /// inner source carrying the downstream account — never collapse to
    /// the ctx-less `has_pv`/`get_introspection` via the trait default.
    /// Without the per-layer `*_checked` forwarders the credentials die
    /// at the first wrapper boundary, re-opening the FR-8 leak.
    #[tokio::test]
    async fn fr8_wrapper_stack_threads_checked_existence_and_introspection() {
        let plain_has_pv = Arc::new(AtomicBool::new(false));
        let plain_intro = Arc::new(AtomicBool::new(false));
        let checked_has_pv_account = Arc::new(std::sync::Mutex::new(None));
        let checked_intro_account = Arc::new(std::sync::Mutex::new(None));
        let inner = ExistenceRecordingSource {
            plain_has_pv: plain_has_pv.clone(),
            plain_intro: plain_intro.clone(),
            checked_has_pv_account: checked_has_pv_account.clone(),
            checked_intro_account: checked_intro_account.clone(),
        };
        let stack = AuditLayer::new(NoopAudit)
            .layer(ReadOnlyLayer.layer(AclLayer::new(AclConfig::default()).layer(inner)));

        let found = stack.has_pv_checked("X", test_ctx()).await;
        let intro = stack.get_introspection_checked("X", test_ctx()).await;

        assert!(
            found,
            "credentialed existence must resolve through the stack"
        );
        assert!(
            intro.is_some(),
            "credentialed introspection must resolve through the stack"
        );
        assert_eq!(
            checked_has_pv_account.lock().unwrap().as_deref(),
            Some("alice"),
            "has_pv_checked must reach the inner carrying ctx.creds.account through every wrapper"
        );
        assert_eq!(
            checked_intro_account.lock().unwrap().as_deref(),
            Some("alice"),
            "get_introspection_checked must reach the inner carrying ctx.creds.account"
        );
        assert!(
            !plain_has_pv.load(Ordering::SeqCst),
            "no wrapper may collapse has_pv_checked to credential-free has_pv"
        );
        assert!(
            !plain_intro.load(Ordering::SeqCst),
            "no wrapper may collapse get_introspection_checked to credential-free get_introspection"
        );
    }

    /// a source's `monitor_watermarks` levels must survive
    /// the full `Audited<ReadOnly<Acl<S>>>` wrapper stack. Without the
    /// per-layer forwarder each wrapper returns the trait default `None`,
    /// so the server's monitor loop never fires the pause/resume
    /// callbacks — the same wrapper-severs-override defect family as FR-8.
    struct WatermarkSource;
    impl ChannelSource for WatermarkSource {
        async fn list_pvs(&self) -> Vec<String> {
            vec!["X".into()]
        }
        async fn has_pv(&self, _name: &str) -> bool {
            true
        }
        async fn get_introspection(&self, _name: &str) -> Option<FieldDesc> {
            Some(FieldDesc::Scalar(ScalarType::Double))
        }
        async fn get_value(&self, _name: &str) -> Option<PvField> {
            Some(PvField::Scalar(ScalarValue::Double(0.0)))
        }
        async fn put_value(&self, _name: &str, _value: PvField) -> Result<(), OpError> {
            Ok(())
        }
        async fn is_writable(&self, _name: &str) -> bool {
            true
        }
        async fn subscribe(&self, _name: &str) -> Option<MonitorStream<PvField>> {
            None
        }
        async fn monitor_watermarks(&self, _name: &str) -> Option<(usize, usize)> {
            Some((2, 5))
        }
    }

    #[tokio::test]
    async fn fr11_wrapper_stack_forwards_monitor_watermarks() {
        let stack = AuditLayer::new(NoopAudit)
            .layer(ReadOnlyLayer.layer(AclLayer::new(AclConfig::default()).layer(WatermarkSource)));
        assert_eq!(
            stack.monitor_watermarks("X").await,
            Some((2, 5)),
            "monitor_watermarks must forward through every wrapper, not collapse to the default None"
        );
    }

    /// A source's descriptive / advertisement / channel-lifecycle methods
    /// must survive the full `Audited<ReadOnly<Acl<S>>>` wrapper stack.
    /// `channel_report_info`, `searchable`/`searchable_from`,
    /// `beacon_change`, and `notify_channel_open`/`notify_channel_close`
    /// are all queried or fired on the BOUND OWNER — the wrapper itself,
    /// since `resolve_owner` stays the default `None`. Without the per-layer
    /// forwarders each wrapper returns the trait default (None / has_pv / 0
    /// / no-op) and the inner override is severed — the same
    /// wrapper-severs-override defect family as FR-8 and FR-11.
    struct ReportInfoSource {
        opened: Arc<AtomicBool>,
        closed: Arc<AtomicBool>,
    }
    impl ChannelSource for ReportInfoSource {
        async fn list_pvs(&self) -> Vec<String> {
            vec!["X".into()]
        }
        async fn has_pv(&self, _name: &str) -> bool {
            true
        }
        async fn get_introspection(&self, _name: &str) -> Option<FieldDesc> {
            Some(FieldDesc::Scalar(ScalarType::Double))
        }
        async fn get_value(&self, _name: &str) -> Option<PvField> {
            Some(PvField::Scalar(ScalarValue::Double(0.0)))
        }
        async fn put_value(&self, _name: &str, _value: PvField) -> Result<(), OpError> {
            Ok(())
        }
        async fn is_writable(&self, _name: &str) -> bool {
            true
        }
        async fn subscribe(&self, _name: &str) -> Option<MonitorStream<PvField>> {
            None
        }
        async fn channel_report_info(&self, _name: &str, _ctx: ChannelContext) -> Option<String> {
            Some("gw-upstream".into())
        }
        // Deliberately distinct from `has_pv` (true) so a wrapper that
        // collapses to the `searchable` default (→ has_pv) is caught.
        async fn searchable(&self, _name: &str) -> bool {
            false
        }
        async fn searchable_from(&self, _name: &str, _requester: SocketAddr) -> bool {
            false
        }
        fn beacon_change(&self) -> u64 {
            7
        }
        fn notify_channel_open(&self, _name: &str, _ctx: &ChannelContext) {
            self.opened.store(true, Ordering::SeqCst);
        }
        fn notify_channel_close(&self, _name: &str, _ctx: &ChannelContext) {
            self.closed.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn wrapper_stack_forwards_channel_report_info_and_lifecycle_family() {
        let opened = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let inner = ReportInfoSource {
            opened: opened.clone(),
            closed: closed.clone(),
        };
        let stack = AuditLayer::new(NoopAudit)
            .layer(ReadOnlyLayer.layer(AclLayer::new(AclConfig::default()).layer(inner)));

        // The named finding: per-channel report info captured at admission
        // must reach the inner through every wrapper, not collapse to None.
        assert_eq!(
            stack.channel_report_info("X", test_ctx()).await,
            Some("gw-upstream".to_string()),
            "channel_report_info must forward through every wrapper, not collapse to None"
        );

        // Same family: SEARCH advertisement, beacon signal, and channel
        // open/close lifecycle callbacks must also forward to the inner.
        assert!(
            !stack.searchable("X").await,
            "searchable must forward the inner override, not collapse to the has_pv default"
        );
        let requester: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        assert!(
            !stack.searchable_from("X", requester).await,
            "searchable_from must forward the inner override, not collapse to searchable/has_pv"
        );
        assert_eq!(
            stack.beacon_change(),
            7,
            "beacon_change must forward through every wrapper, not collapse to 0"
        );
        stack.notify_channel_open("X", &test_ctx());
        stack.notify_channel_close("X", &test_ctx());
        assert!(
            opened.load(Ordering::SeqCst),
            "notify_channel_open must reach the inner through every wrapper"
        );
        assert!(
            closed.load(Ordering::SeqCst),
            "notify_channel_close must reach the inner through every wrapper"
        );
    }

    // ── process / process_checked forwarding through the wrappers ────

    /// A typed PROCESS through the `Acl` wrapper (allowed name) must
    /// reach the inner source's PROCESS path. Pre-fix the trait
    /// default `process_checked` → `process` (`Ok(())`) ran on the
    /// `Acl` layer itself and never forwarded.
    #[tokio::test]
    async fn acl_forwards_process_checked_to_inner() {
        let process_reached = Arc::new(AtomicBool::new(false));
        let inner = RecordingSource {
            delta_reached: Arc::new(AtomicBool::new(false)),
            value_reached: Arc::new(AtomicBool::new(false)),
            process_reached: process_reached.clone(),
        };
        let acl = AclLayer::new(AclConfig::default()).layer(inner);

        let res = acl
            .process_checked(checked_for("X").await, test_ctx())
            .await;
        assert!(res.is_ok());
        assert!(
            process_reached.load(Ordering::SeqCst),
            "Acl must route PROCESS to the inner source's process_checked"
        );
    }

    /// An `Acl`-denied PROCESS must be rejected at the layer and never
    /// reach the inner source.
    #[tokio::test]
    async fn acl_denied_process_checked_short_circuits() {
        let process_reached = Arc::new(AtomicBool::new(false));
        let inner = RecordingSource {
            delta_reached: Arc::new(AtomicBool::new(false)),
            value_reached: Arc::new(AtomicBool::new(false)),
            process_reached: process_reached.clone(),
        };
        let cfg = AclConfig::default().deny_regex(r"SECRET:.*").unwrap();
        let acl = AclLayer::new(cfg).layer(inner);

        let err = acl
            .process_checked(checked_for("SECRET:KEY").await, test_ctx())
            .await
            .expect_err("ACL-denied PROCESS must fail at the layer");
        assert_eq!(
            err.kind,
            OpErrorKind::Denied,
            "ACL denial must classify as Denied: {err:?}"
        );
        assert!(err.message.contains("denied"));
        assert!(
            !process_reached.load(Ordering::SeqCst),
            "ACL-denied PROCESS must not reach the inner source"
        );
    }

    /// A PROCESS through the `ReadOnly` wrapper must be rejected at
    /// the layer (WRITE-class op) and never reach the inner source.
    #[tokio::test]
    async fn read_only_rejects_process_checked() {
        let process_reached = Arc::new(AtomicBool::new(false));
        let inner = RecordingSource {
            delta_reached: Arc::new(AtomicBool::new(false)),
            value_reached: Arc::new(AtomicBool::new(false)),
            process_reached: process_reached.clone(),
        };
        let ro = ReadOnlyLayer.layer(inner);

        let err = ro
            .process_checked(checked_for("X").await, test_ctx())
            .await
            .expect_err("read-only PROCESS must be rejected");
        assert_eq!(
            err.kind,
            OpErrorKind::Denied,
            "read-only refusal must classify as Denied: {err:?}"
        );
        assert!(err.message.contains("read-only"));
        assert!(
            !process_reached.load(Ordering::SeqCst),
            "read-only PROCESS must not reach the inner source"
        );
        // The ctx-less `process` is rejected the same way.
        let err = ro
            .process("X")
            .await
            .expect_err("read-only ctx-less PROCESS must be rejected");
        assert_eq!(
            err.kind,
            OpErrorKind::Denied,
            "read-only refusal must classify as Denied: {err:?}"
        );
        assert!(err.message.contains("read-only"));
        assert!(!process_reached.load(Ordering::SeqCst));
    }

    /// RPC through the `ReadOnly` wrapper must be rejected at the layer.
    /// pva2pva blocks `createChannelRPC` under `p2pReadOnly` the same way
    /// it blocks Put/Process (`channel.cpp:140-150`). The override
    /// returns the read-only error without touching the inner source, so
    /// the "read-only" message distinguishes rejection from forwarding
    /// (a forwarded RPC would surface the inner source's own error).
    #[tokio::test]
    async fn read_only_rejects_rpc() {
        let inner = RecordingSource {
            delta_reached: Arc::new(AtomicBool::new(false)),
            value_reached: Arc::new(AtomicBool::new(false)),
            process_reached: Arc::new(AtomicBool::new(false)),
        };
        let ro = ReadOnlyLayer.layer(inner);
        let desc = FieldDesc::Scalar(ScalarType::Double);
        let val = PvField::Scalar(ScalarValue::Double(1.0));

        let err = ro
            .rpc_checked(
                checked_for("X").await,
                desc.clone(),
                val.clone(),
                test_ctx(),
            )
            .await
            .expect_err("read-only RPC must be rejected");
        assert_eq!(
            err.kind,
            OpErrorKind::Denied,
            "read-only refusal must classify as Denied: {err:?}"
        );
        assert!(err.message.contains("read-only"), "err: {err}");

        let err = ro
            .rpc("X", desc, val)
            .await
            .expect_err("read-only ctx-less RPC must be rejected");
        assert_eq!(
            err.kind,
            OpErrorKind::Denied,
            "read-only refusal must classify as Denied: {err:?}"
        );
        assert!(err.message.contains("read-only"), "err: {err}");
    }

    /// A PROCESS through the `Audited` wrapper must reach the inner
    /// source and emit exactly one `Process` audit row carrying the
    /// peer credentials.
    #[tokio::test]
    async fn audited_forwards_process_checked_and_records() {
        let process_reached = Arc::new(AtomicBool::new(false));
        let inner = RecordingSource {
            delta_reached: Arc::new(AtomicBool::new(false)),
            value_reached: Arc::new(AtomicBool::new(false)),
            process_reached: process_reached.clone(),
        };
        let events: Arc<std::sync::Mutex<Vec<AuditEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_sink = events.clone();
        let audited = AuditLayer::new(ClosureAudit(move |ev| {
            events_sink.lock().unwrap().push(ev);
        }))
        .layer(inner);

        let res = audited
            .process_checked(checked_for("X").await, test_ctx())
            .await;
        assert!(res.is_ok());
        assert!(
            process_reached.load(Ordering::SeqCst),
            "Audited must route PROCESS to the inner source's process_checked"
        );
        let recorded = events.lock().unwrap();
        assert_eq!(recorded.len(), 1, "exactly one audit row per PROCESS");
        assert_eq!(recorded[0].event, AuditEventKind::Process);
        assert_eq!(recorded[0].result, AuditResult::Ok);
        assert_eq!(recorded[0].pv, "X");
        assert_eq!(recorded[0].user, "alice");
        assert_eq!(recorded[0].host, "host1");
    }

    /// The full production layer stack `Audited(Acl(inner))` (what
    /// `PvaGateway::start` builds) must forward a PROCESS all the way
    /// to the inner source — proving the fix is effective in the real
    /// layered deployment, not just for a single wrapper. Pre-fix the
    /// PROCESS hit `Audited`'s trait-default `process_checked` →
    /// `process` (`Ok(())`) and never reached `Acl`, let alone the
    /// inner source.
    #[tokio::test]
    async fn layered_audited_acl_forwards_process_to_inner() {
        let process_reached = Arc::new(AtomicBool::new(false));
        let inner = RecordingSource {
            delta_reached: Arc::new(AtomicBool::new(false)),
            value_reached: Arc::new(AtomicBool::new(false)),
            process_reached: process_reached.clone(),
        };
        let events: Arc<std::sync::Mutex<Vec<AuditEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_sink = events.clone();
        let acl = AclLayer::new(AclConfig::default()).layer(inner);
        let layered = AuditLayer::new(ClosureAudit(move |ev| {
            events_sink.lock().unwrap().push(ev);
        }))
        .layer(acl);

        let res = layered
            .process_checked(checked_for("X").await, test_ctx())
            .await;
        assert!(res.is_ok());
        assert!(
            process_reached.load(Ordering::SeqCst),
            "PROCESS through Audited(Acl(inner)) must reach the inner source"
        );
        assert_eq!(
            events.lock().unwrap()[0].event,
            AuditEventKind::Process,
            "the layered PROCESS must still be audited"
        );
    }

    /// In the layered stack, an ACL deny still short-circuits a
    /// PROCESS at the `Acl` layer (the audit row records the denial).
    #[tokio::test]
    async fn layered_audited_acl_denies_process() {
        let process_reached = Arc::new(AtomicBool::new(false));
        let inner = RecordingSource {
            delta_reached: Arc::new(AtomicBool::new(false)),
            value_reached: Arc::new(AtomicBool::new(false)),
            process_reached: process_reached.clone(),
        };
        let events: Arc<std::sync::Mutex<Vec<AuditEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_sink = events.clone();
        let cfg = AclConfig::default().deny_regex(r"SECRET:.*").unwrap();
        let acl = AclLayer::new(cfg).layer(inner);
        let layered = AuditLayer::new(ClosureAudit(move |ev| {
            events_sink.lock().unwrap().push(ev);
        }))
        .layer(acl);

        let err = layered
            .process_checked(checked_for("SECRET:KEY").await, test_ctx())
            .await
            .expect_err("ACL-denied PROCESS must fail through the layered stack");
        assert!(err.message.contains("denied"));
        assert!(
            !process_reached.load(Ordering::SeqCst),
            "ACL-denied PROCESS must not reach the inner source"
        );
        let recorded = events.lock().unwrap();
        assert_eq!(recorded[0].event, AuditEventKind::Process);
        assert_eq!(recorded[0].result, AuditResult::Denied);
    }

    /// Regression: minimal source that records whether its
    /// event-affecting-options MONITOR variant was reached.
    struct OptsRecordingSource {
        opts_reached: Arc<AtomicBool>,
    }

    impl ChannelSource for OptsRecordingSource {
        async fn list_pvs(&self) -> Vec<String> {
            vec!["X".into()]
        }
        async fn has_pv(&self, _name: &str) -> bool {
            true
        }
        async fn get_introspection(&self, _name: &str) -> Option<FieldDesc> {
            Some(FieldDesc::Scalar(ScalarType::Double))
        }
        async fn get_value(&self, _name: &str) -> Option<PvField> {
            Some(PvField::Scalar(ScalarValue::Double(0.0)))
        }
        async fn put_value(&self, _name: &str, _value: PvField) -> Result<(), OpError> {
            Ok(())
        }
        async fn is_writable(&self, _name: &str) -> bool {
            false
        }
        async fn subscribe(&self, _name: &str) -> Option<MonitorStream<PvField>> {
            None
        }
        async fn subscribe_checked_opts_marked(
            &self,
            _checked: AccessChecked,
            _ctx: ChannelContext,
            _opts: epics_pva_rs::server_native::MonitorOptions,
        ) -> Option<MonitorStream<epics_pva_rs::server_native::MonitorUpdate>> {
            self.opts_reached.store(true, Ordering::SeqCst);
            None
        }
        async fn subscribe_raw_checked_opts(
            &self,
            _checked: AccessChecked,
            _ctx: ChannelContext,
            _opts: epics_pva_rs::server_native::MonitorOptions,
        ) -> Option<MonitorStream<RawMonitorEvent>> {
            self.opts_reached.store(true, Ordering::SeqCst);
            None
        }
    }

    /// the `Audit` / `ReadOnly` / `Acl` middleware wrappers
    /// must forward `subscribe_checked_opts_marked` (the server's cooked
    /// dispatch entry point) / `subscribe_raw_checked_opts` to the inner
    /// source. Without the override the wrapper inherits the trait
    /// default, which drops the inner source's own marked path — so the
    /// gateway source (the innermost layer) never sees the downstream's
    /// event-affecting monitor options and cannot reject an unsupported
    /// set.
    #[tokio::test]
    async fn middleware_layers_forward_subscribe_opts_to_inner() {
        use epics_pva_rs::server_native::MonitorOptions;

        for raw in [false, true] {
            let opts_reached = Arc::new(AtomicBool::new(false));
            let inner = OptsRecordingSource {
                opts_reached: opts_reached.clone(),
            };
            // Production gateway stack shape: Audit( ReadOnly( Acl( inner ) ) ).
            let acl = AclLayer::new(AclConfig::default()).layer(inner);
            let read_only = ReadOnlyLayer.layer(acl);
            let layered = AuditLayer::new(NoopAudit).layer(read_only);

            let opts = MonitorOptions {
                server_filter: true,
                ..MonitorOptions::default()
            };
            if raw {
                let _ = layered
                    .subscribe_raw_checked_opts(checked_for("X").await, test_ctx(), opts)
                    .await;
            } else {
                let _ = layered
                    .subscribe_checked_opts_marked(checked_for("X").await, test_ctx(), opts)
                    .await;
            }
            assert!(
                opts_reached.load(Ordering::SeqCst),
                "middleware stack must forward subscribe{}_checked_opts to the inner source",
                if raw { "_raw" } else { "" },
            );
        }
    }

    /// R17-33 regression: the REAL downstream stack is
    /// `Audit(ReadOnly(Acl(GatewayChannelSource)))`, and the server hands
    /// its `ChannelInvalidator` to that stack — not to the bare gateway
    /// source. Every layer must forward `set_channel_invalidator`, or the
    /// handle stops at the outermost wrapper (trait default = drop it) and
    /// the gateway's caches publish nothing: an operator `<prefix>:drop` /
    /// `:flush` then ends the upstream entry while the live downstream
    /// channels stay bound, never receiving the server-initiated
    /// DESTROY_CHANNEL pva2pva sends (`p2pApp/server.cpp:130-135`).
    ///
    /// Pre-fix this fails at the first `try_recv` (no invalidator ever
    /// reached the cache); the existing coverage passed only because it
    /// wired the UNLAYERED source.
    #[tokio::test]
    async fn r17_33_layered_stack_forwards_set_channel_invalidator() {
        use crate::pva_gateway::{ChannelCache, GatewayChannelSource};
        use epics_pva_rs::client::PvaClient;
        use epics_pva_rs::server_native::source::ChannelInvalidator;
        use std::time::Duration;

        // Both production chain shapes: read_only adds the ReadOnly layer,
        // so the handle has to survive three wrappers in the longest one.
        for read_only in [false, true] {
            let client = Arc::new(PvaClient::builder().build());
            let cache = ChannelCache::new(client, Duration::from_secs(60));
            let gw = GatewayChannelSource::new(cache.clone());
            let layered: DynSource = layer_access_control(
                gw.clone(),
                AclConfig::default(),
                read_only,
                Arc::new(NoopAudit) as Arc<dyn AuditSink>,
            );

            let inv = ChannelInvalidator::new();
            let mut rx = inv.subscribe();
            // The server calls this on the BOUND source — the layered stack.
            layered.set_channel_invalidator(inv.clone());

            cache.insert_test_entry("GW:LAYERED:PV").await;
            assert!(
                gw.drop_entry_all_caches("GW:LAYERED:PV").await,
                "the entry must be dropped (read_only={read_only})"
            );
            assert_eq!(
                rx.try_recv()
                    .expect("the layered stack must forward the invalidator to the gateway cache")
                    .to_vec(),
                vec!["GW:LAYERED:PV".to_string()],
                "an operator drop through a LAYERED gateway must publish the \
                 invalidation (read_only={read_only})"
            );
        }
    }
}
