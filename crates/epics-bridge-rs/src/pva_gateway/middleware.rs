//! Tower-style middleware for the PVA gateway.
//!
//! Wraps the underlying [`epics_pva_rs::server_native::ChannelSource`]
//! ([`super::source::GatewayChannelSource`] in practice) with
//! composable [`Layer`]s that add cross-cutting concerns:
//!
//! - [`AclLayer`] — refuse `has_pv` / `put_value` for PV names that
//!   match the deny list
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

use std::sync::Arc;

use epics_pva_rs::pvdata::{FieldDesc, PvField};
use epics_pva_rs::server_native::ChannelContext;
use epics_pva_rs::server_native::source::{ChannelSource, RawMonitorEvent};
use tokio::sync::mpsc;

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
    // Round 43: forward the inner source's AccessGate so the wire
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
    async fn get_value(&self, name: &str) -> Option<PvField> {
        self.inner.get_value(name).await
    }
    async fn put_value(&self, _name: &str, _value: PvField) -> Result<(), String> {
        Err("read-only mode: PUT rejected".into())
    }
    async fn put_value_checked(
        &self,
        _checked: epics_pva_rs::server_native::source::AccessChecked,
        _value: PvField,
        _ctx: ChannelContext,
    ) -> Result<(), String> {
        Err("read-only mode: PUT rejected".into())
    }
    async fn is_writable(&self, _name: &str) -> bool {
        false
    }
    async fn subscribe(&self, name: &str) -> Option<mpsc::Receiver<PvField>> {
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
    async fn subscribe_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
    ) -> Option<mpsc::Receiver<PvField>> {
        self.inner.subscribe_checked(checked, ctx).await
    }
    async fn subscribe_raw(&self, name: &str) -> Option<mpsc::Receiver<RawMonitorEvent>> {
        self.inner.subscribe_raw(name).await
    }
    async fn subscribe_raw_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
    ) -> Option<mpsc::Receiver<RawMonitorEvent>> {
        self.inner.subscribe_raw_checked(checked, ctx).await
    }
    async fn rpc(
        &self,
        name: &str,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> Result<(FieldDesc, PvField), String> {
        self.inner.rpc(name, request_desc, request_value).await
    }
    async fn rpc_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        request_desc: FieldDesc,
        request_value: PvField,
        ctx: ChannelContext,
    ) -> Result<(FieldDesc, PvField), String> {
        self.inner.rpc_checked(checked, request_desc, request_value, ctx).await
    }
    fn notify_watermark_high(&self, name: &str) {
        self.inner.notify_watermark_high(name);
    }
    fn notify_watermark_low(&self, name: &str) {
        self.inner.notify_watermark_low(name);
    }
}

// ── AclLayer ─────────────────────────────────────────────────────

/// Pattern-matched access control. PV names matching any entry in
/// `deny` are rejected at the layer before reaching the upstream
/// proxy. `allow_only` (when non-empty) flips the policy — only
/// names matching one of these patterns get through.
///
/// Patterns are simple glob-style — `*` matches any chars, exact
/// match otherwise. For full regex see the per-rule `pvlist` system
/// in the CA gateway; here we keep the surface minimal.
#[derive(Clone, Default)]
pub struct AclConfig {
    pub deny: Vec<String>,
    pub allow_only: Vec<String>,
}

impl AclConfig {
    pub fn allowed(&self, name: &str) -> bool {
        if self.deny.iter().any(|p| matches_pattern(p, name)) {
            return false;
        }
        if !self.allow_only.is_empty() && !self.allow_only.iter().any(|p| matches_pattern(p, name))
        {
            return false;
        }
        true
    }
}

fn matches_pattern(pattern: &str, name: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        return name.starts_with(prefix);
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return name.ends_with(suffix);
    }
    name == pattern
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
    async fn get_value(&self, name: &str) -> Option<PvField> {
        if !self.config.allowed(name) {
            return None;
        }
        self.inner.get_value(name).await
    }
    async fn put_value(&self, name: &str, value: PvField) -> Result<(), String> {
        if !self.config.allowed(name) {
            return Err(format!("ACL: PV '{name}' denied"));
        }
        self.inner.put_value(name, value).await
    }
    // Round 43: type-state op variants gate by the layer's static
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
    async fn put_value_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        value: PvField,
        ctx: ChannelContext,
    ) -> Result<(), String> {
        if !self.config.allowed(checked.pv_name()) {
            return Err(format!("ACL: PV '{}' denied", checked.pv_name()));
        }
        self.inner.put_value_checked(checked, value, ctx).await
    }
    async fn is_writable(&self, name: &str) -> bool {
        self.config.allowed(name) && self.inner.is_writable(name).await
    }
    async fn subscribe(&self, name: &str) -> Option<mpsc::Receiver<PvField>> {
        if !self.config.allowed(name) {
            return None;
        }
        self.inner.subscribe(name).await
    }
    async fn subscribe_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
    ) -> Option<mpsc::Receiver<PvField>> {
        if !self.config.allowed(checked.pv_name()) {
            return None;
        }
        self.inner.subscribe_checked(checked, ctx).await
    }
    async fn subscribe_raw(&self, name: &str) -> Option<mpsc::Receiver<RawMonitorEvent>> {
        if !self.config.allowed(name) {
            return None;
        }
        self.inner.subscribe_raw(name).await
    }
    async fn subscribe_raw_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
    ) -> Option<mpsc::Receiver<RawMonitorEvent>> {
        if !self.config.allowed(checked.pv_name()) {
            return None;
        }
        self.inner.subscribe_raw_checked(checked, ctx).await
    }
    async fn rpc(
        &self,
        name: &str,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> Result<(FieldDesc, PvField), String> {
        if !self.config.allowed(name) {
            return Err(format!("ACL: PV '{name}' denied"));
        }
        self.inner.rpc(name, request_desc, request_value).await
    }
    async fn rpc_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        request_desc: FieldDesc,
        request_value: PvField,
        ctx: ChannelContext,
    ) -> Result<(FieldDesc, PvField), String> {
        if !self.config.allowed(checked.pv_name()) {
            return Err(format!("ACL: PV '{}' denied", checked.pv_name()));
        }
        self.inner
            .rpc_checked(checked, request_desc, request_value, ctx)
            .await
    }
    fn notify_watermark_high(&self, name: &str) {
        self.inner.notify_watermark_high(name);
    }
    fn notify_watermark_low(&self, name: &str) {
        self.inner.notify_watermark_low(name);
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
/// Drop order matters in shutdown — drop the gateway / Arc<Audited>
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
        tokio::spawn(async move {
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditResult {
    Ok,
    Denied,
    Failed,
}

/// Build an [`AuditEvent`] from the inner-call outcome. Detects
/// `Denied` vs `Failed` heuristically: error messages produced by
/// [`AclLayer`] / [`ReadOnlyLayer`] / source-level ACF checks
/// usually contain "denied" / "deny" / "ACL" / "read-only", so the
/// downstream operator gets the right bucket without each layer
/// having to invent a structured-error type.
fn make_audit_event(name: &str, user: &str, host: &str, result: &Result<(), String>) -> AuditEvent {
    let (kind, error) = match result {
        Ok(_) => (AuditResult::Ok, String::new()),
        Err(msg) => {
            let lower = msg.to_lowercase();
            if lower.contains("deny")
                || lower.contains("denied")
                || lower.contains("acl:")
                || lower.contains("read-only")
            {
                (AuditResult::Denied, msg.clone())
            } else {
                (AuditResult::Failed, msg.clone())
            }
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
    async fn get_value(&self, name: &str) -> Option<PvField> {
        let result = self.inner.get_value(name).await;
        if self.audit_get {
            // GET has no error path here — None means "missing" not
            // "denied" — so we model it as Ok with empty error.
            let outcome: Result<(), String> = if result.is_some() {
                Ok(())
            } else {
                Err(format!("PV '{name}' not found"))
            };
            let mut ev = make_audit_event(name, "", "", &outcome);
            ev.event = AuditEventKind::Get;
            self.sink.record(ev);
        }
        result
    }
    async fn put_value(&self, name: &str, value: PvField) -> Result<(), String> {
        let result = self.inner.put_value(name, value).await;
        self.sink.record(make_audit_event(name, "", "", &result));
        result
    }
    // Round 43: typed PUT — emits a credential-aware audit row and
    // forwards through the inner's gate-enforced path.
    async fn put_value_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        value: PvField,
        ctx: ChannelContext,
    ) -> Result<(), String> {
        let pv = checked.pv_name().to_string();
        let user = ctx.account.clone();
        let host = ctx.host.clone();
        let result = self.inner.put_value_checked(checked, value, ctx).await;
        self.sink
            .record(make_audit_event(&pv, &user, &host, &result));
        result
    }
    async fn get_value_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: ChannelContext,
    ) -> Option<PvField> {
        let pv = checked.pv_name().to_string();
        let user = ctx.account.clone();
        let host = ctx.host.clone();
        let result = self.inner.get_value_checked(checked, ctx).await;
        if self.audit_get {
            let outcome: Result<(), String> = if result.is_some() {
                Ok(())
            } else {
                Err(format!("PV '{pv}' not found / denied"))
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
    ) -> Option<mpsc::Receiver<PvField>> {
        let pv = checked.pv_name().to_string();
        let user = ctx.account.clone();
        let host = ctx.host.clone();
        let result = self.inner.subscribe_checked(checked, ctx).await;
        if self.audit_subscribe {
            let outcome: Result<(), String> = if result.is_some() {
                Ok(())
            } else {
                Err(format!("PV '{pv}' not subscribable"))
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
    async fn subscribe(&self, name: &str) -> Option<mpsc::Receiver<PvField>> {
        let result = self.inner.subscribe(name).await;
        if self.audit_subscribe {
            let outcome: Result<(), String> = if result.is_some() {
                Ok(())
            } else {
                Err(format!("PV '{name}' not subscribable"))
            };
            let mut ev = make_audit_event(name, "", "", &outcome);
            ev.event = AuditEventKind::Subscribe;
            self.sink.record(ev);
        }
        result
    }
    async fn subscribe_raw(&self, name: &str) -> Option<mpsc::Receiver<RawMonitorEvent>> {
        // Audit on subscribe_raw too, since the F-G12 zero-copy
        // path bypasses the typed `subscribe` and would otherwise
        // miss the audit event entirely.
        let result = self.inner.subscribe_raw(name).await;
        if self.audit_subscribe {
            let outcome: Result<(), String> = if result.is_some() {
                Ok(())
            } else {
                Err(format!("PV '{name}' not subscribable (raw)"))
            };
            let mut ev = make_audit_event(name, "", "", &outcome);
            ev.event = AuditEventKind::Subscribe;
            self.sink.record(ev);
        }
        result
    }
    // Round 43: typed raw MONITOR — populate audit row with peer
    // credentials and forward through the inner's gate-enforced
    // path.
    async fn subscribe_raw_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: epics_pva_rs::server_native::source::ChannelContext,
    ) -> Option<mpsc::Receiver<RawMonitorEvent>> {
        let pv = checked.pv_name().to_string();
        let user = ctx.account.clone();
        let host = ctx.host.clone();
        let result = self.inner.subscribe_raw_checked(checked, ctx).await;
        if self.audit_subscribe {
            let outcome: Result<(), String> = if result.is_some() {
                Ok(())
            } else {
                Err(format!("PV '{pv}' not subscribable (raw)"))
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
    ) -> Result<(FieldDesc, PvField), String> {
        let result = self.inner.rpc(name, request_desc, request_value).await;
        if self.audit_rpc {
            let outcome: Result<(), String> = match &result {
                Ok(_) => Ok(()),
                Err(e) => Err(e.clone()),
            };
            let mut ev = make_audit_event(name, "", "", &outcome);
            ev.event = AuditEventKind::Rpc;
            self.sink.record(ev);
        }
        result
    }
    // Round 43: typed RPC — populate audit row with peer
    // credentials and forward through the inner's gate-enforced
    // path.
    async fn rpc_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        request_desc: FieldDesc,
        request_value: PvField,
        ctx: epics_pva_rs::server_native::source::ChannelContext,
    ) -> Result<(FieldDesc, PvField), String> {
        let pv = checked.pv_name().to_string();
        let user = ctx.account.clone();
        let host = ctx.host.clone();
        let result = self
            .inner
            .rpc_checked(checked, request_desc, request_value, ctx)
            .await;
        if self.audit_rpc {
            let outcome: Result<(), String> = match &result {
                Ok(_) => Ok(()),
                Err(e) => Err(e.clone()),
            };
            let mut ev = make_audit_event(&pv, &user, &host, &outcome);
            ev.event = AuditEventKind::Rpc;
            self.sink.record(ev);
        }
        result
    }
    fn notify_watermark_high(&self, name: &str) {
        self.inner.notify_watermark_high(name);
    }
    fn notify_watermark_low(&self, name: &str) {
        self.inner.notify_watermark_low(name);
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
        };
        assert!(cfg.allowed("MOTOR:VAL"));
        assert!(!cfg.allowed("MOTOR:JOG:UP"));
        assert!(!cfg.allowed("OTHER:PV"));
    }

    /// Audit-event classifier tags ACL/read-only error messages
    /// as Denied; other errors as Failed; Ok results as Ok.
    #[test]
    fn audit_event_classifies_results() {
        let denied = make_audit_event(
            "MOTOR:VAL",
            "alice",
            "host1",
            &Err("ACL: PV 'MOTOR:VAL' denied".into()),
        );
        assert_eq!(denied.result, AuditResult::Denied);
        assert!(!denied.error.is_empty());

        let read_only = make_audit_event(
            "MOTOR:VAL",
            "",
            "",
            &Err("read-only mode: PUT rejected".into()),
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
}
